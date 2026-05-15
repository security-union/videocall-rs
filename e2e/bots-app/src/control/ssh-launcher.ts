import { spawn, type ChildProcess } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";

import {
  buildRemoteLaunchCommand,
  buildSshArgsForLaunch,
  shellEscape,
  type SshHost,
} from "./ssh-hosts";

/**
 * Sentinel value the launcher injects as `--sso-state-file <token>` on
 * the remote npm command line when SSO forwarding is active. The OUTER
 * remote shell exports `T=$(mktemp)`; the inner `<shell> -lc …` sees
 * `$T` as an environment variable and expands it to the mktemp path.
 *
 * We use the literal `"$T"` (with double quotes) so the path is
 * expanded in the inner shell — single-quoting would suppress the
 * expansion and the bot would try to open the four-byte literal
 * filename `"$T"`, which obviously does not exist.
 */
export const SSO_FORWARD_TEMP_TOKEN = '"$T"';

/**
 * Maximum number of stdout/stderr lines we keep per remote bot. Lets
 * the dashboard's log viewer scroll back without growing the
 * orchestrator's heap unboundedly during long-lived debug sessions.
 */
export const REMOTE_LOG_CAP = 200;

/**
 * Wire shape exposed to the orchestrator. The control server stashes
 * these on each SSH-hosted registry entry so action handlers can find
 * the underlying ChildProcess (for SIGTERM / SIGKILL) and the log
 * buffer (for the GET /bots/:id/log endpoint).
 */
export interface SshBotHandle {
  /** The host registry row used to launch this bot. */
  host: SshHost;
  /** Underlying `ssh` ChildProcess. Survives until the remote bot exits. */
  child: ChildProcess;
  /** Rolling stdout/stderr buffer, newest-last, capped at REMOTE_LOG_CAP. */
  recentLog: string[];
  /** Total number of log lines pushed since the bot was launched. */
  totalLines: number;
  /** Resolved exit code (null while still running). */
  exitCode: number | null;
  /** Set when the SSH process exits — `done` (0) or `failed` (≠0). */
  finished: boolean;
  /** Set when `finished === true`; describes the exit reason. */
  finishReason?: "remote-exit-ok" | "remote-exit-error" | "ssh-error";
  /** Resolves the first time the SSH process exits. */
  exit: Promise<number | null>;
}

/**
 * Spec accepted by {@link spawnRemoteBot}. Pure data — no orchestrator
 * coupling — so the launch path is easy to unit-test by stubbing
 * `deps.spawn`.
 */
export interface SshLaunchSpec {
  host: SshHost;
  ttl: string;
  meetingURL: string;
  participant: string;
  network?: string | null;
  authBackend?: string | null;
  displayName?: string | null;
  /**
   * `headless: false` lets the bot run with a visible Chrome window on
   * the remote host — only useful when the operator has X forwarding
   * set up. Defaults to `true` (the common case).
   */
  headless?: boolean;
  /**
   * Absolute path to the LOCAL captured HCL SSO state JSON
   * (`<runDir>/auth/hcl-sso.json`). When set, the host has
   * `forwardSsoState !== false`, the file exists, and `authBackend ===
   * "jwt"`, the launcher wraps the remote command so the file contents
   * are piped over SSH stdin into a mode-0600 temp file the remote bot
   * consumes via `--sso-state-file "$T"`. The temp file is removed by
   * a `trap … EXIT` line on the outer remote shell when the SSH
   * session ends (clean exit, SIGTERM, SIGKILL).
   *
   * Pass `null` / omit to fall back to the un-wrapped command shape
   * (byte-for-byte identical to today's behaviour).
   */
  ssoStateFile?: string | null;
  /**
   * Optional `botId` used when emitting the warning log line that
   * fires when SSO forwarding is requested but the local state file
   * does not exist. Surfaced in `[<botId>] ssh: …` so operators
   * correlate the warning with the matching registry entry.
   */
  botId?: string;
}

/**
 * Build options for {@link buildSshCommand}: ssoWrap flips the
 * launcher into the wrapped form (outer mktemp + cat to temp file +
 * trap EXIT + `--sso-state-file "$T"` flag). The dashboard's preview
 * surface passes `true` when the host has forwardSsoState ON AND a
 * local state file exists, so operators see exactly what gets run.
 */
export interface BuildSshCommandOpts {
  /**
   * When `true`, emit the SSO-forward wrapper instead of the bare form.
   * Caller is responsible for deciding whether forwarding is enabled
   * (forwardSsoState + local-file-exists + authBackend===jwt).
   */
  ssoWrap?: boolean;
}

export interface SshLaunchDeps {
  spawn?: typeof spawn;
}

/**
 * Output of {@link buildSshCommand}: the argv array we hand to
 * `child_process.spawn("ssh", argv)` plus a human-readable single-line
 * rendering of the same. The display string is what the dashboard's
 * "SSH command preview" surface shows and what the SSH launcher writes
 * as the first line of `recentLog` so the operator can scroll to the
 * top of the per-bot log dialog and see exactly what `ssh` invocation
 * actually ran.
 *
 * `remoteCommand` is the single-line bash command embedded as the last
 * argv slot — exposed separately so the preview UI can show it on its
 * own (it tends to be the most interesting part to copy-paste into a
 * terminal for ad-hoc debugging).
 */
export interface SshCommandRender {
  argv: string[];
  display: string;
  remoteCommand: string;
}

/**
 * Pure helper: build the SSH command we would run for the given host +
 * launch spec, both as the argv slot list and as a single-line
 * human-readable rendering. No side effects, no file IO — safe to call
 * from the `/api/hosts/:label/preview-launch` endpoint (which is
 * specifically supposed to NOT execute anything).
 *
 * The rendering quotes each argv slot via {@link renderArgvForDisplay}
 * so a copy-paste of the result reproduces what `child_process.spawn`
 * would actually pass.
 */
export function buildSshCommand(
  host: SshHost,
  spec: SshLaunchSpec,
  opts: BuildSshCommandOpts = {},
): SshCommandRender {
  const ssoWrap = opts.ssoWrap === true;
  const remoteCommand = buildRemoteLaunchCommand({
    reposPath: host.reposPath,
    ttl: spec.ttl,
    meetingURL: spec.meetingURL,
    participant: spec.participant,
    network: spec.network ?? null,
    authBackend: spec.authBackend ?? null,
    displayName: spec.displayName ?? null,
    headless: spec.headless !== false,
    // When SSO wrap is on, the bot reads its state from the mktemp
    // path exported by the outer remote shell. The raw token is
    // `"$T"` (double-quoted so the inner shell expands it). When the
    // wrap is off this field stays null and the `--sso-state-file`
    // flag is omitted entirely — preserving byte-for-byte the un-
    // wrapped launch command shape operators see today.
    ssoStateFileRaw: ssoWrap ? SSO_FORWARD_TEMP_TOKEN : null,
  });
  const argv = ["ssh", ...buildSshArgsForLaunch(host, remoteCommand, { ssoWrap })];
  return {
    argv,
    display: renderArgvForDisplay(argv),
    remoteCommand,
  };
}

/**
 * Quote argv slots for human-readable rendering. We single-quote any
 * slot that contains whitespace, a shell metacharacter, or the embedded
 * remote-command (which always has spaces and the `cd … && npm run …`
 * pattern); slots that are already a safe identifier are left bare to
 * keep the output scannable. The function never returns something the
 * shell would parse differently from the original argv — values with
 * an embedded `'` use the standard `'\''` dance, same as
 * {@link shellEscape}.
 */
function renderArgvForDisplay(argv: string[]): string {
  return argv.map((slot) => (NEEDS_QUOTING_RE.test(slot) ? shellEscape(slot) : slot)).join(" ");
}

/**
 * Argv slots that match this regex are passed through bare. Anything
 * else gets single-quoted. Mirrors what an operator would naturally
 * type on a shell prompt — `-o`, `ConnectTimeout=10`, `alice@host:2222`,
 * `/path/to/key` stay bare; the embedded remote command does not.
 */
const NEEDS_QUOTING_RE = /[^A-Za-z0-9_@%+=:,./-]/;

/**
 * Spawn `ssh user@host '<remote-cmd>'` and wrap the resulting
 * ChildProcess in a {@link SshBotHandle}. Lines from stdout/stderr are
 * accumulated in `recentLog` (capped at {@link REMOTE_LOG_CAP}); the
 * `exit` promise resolves once `ssh` exits, with the resolved code
 * being either the remote bot's exit code (when SSH itself succeeded)
 * or one of SSH's own error codes (255 = connect failure).
 *
 * The function is fully synchronous up to the first `spawn`; the
 * returned handle's `exit` promise is what callers await.
 *
 * The first line pushed onto `recentLog` is the displayable rendering
 * of the SSH command we just spawned, prefixed with `$ ` to make it
 * visually distinct from real remote stdout. This lets the operator
 * read the actual command back from the per-bot log dialog (and
 * counts against {@link REMOTE_LOG_CAP} like any other line).
 */
export function spawnRemoteBot(spec: SshLaunchSpec, deps: SshLaunchDeps = {}): SshBotHandle {
  const spawnImpl = deps.spawn ?? spawn;
  // Decide whether to enable the SSO-forward wrapper. All three gates
  // must be on:
  //   1. The host has `forwardSsoState !== false`.
  //   2. The launch spec carries an `ssoStateFile` path AND that path
  //      exists on the local FS (we can't pipe a non-existent file).
  //   3. The launch spec's `authBackend === "jwt"`. Storage-state and
  //      Guest auth don't consume SSO state, so wrapping would just
  //      pipe an unused file and clutter the trace.
  //
  // When any gate is off we fall through to the legacy un-wrapped
  // command shape — the existing local-bot SSH flow is preserved
  // byte-for-byte for hosts that don't opt in.
  const forwardEnabled = spec.host.forwardSsoState !== false;
  const haveLocalFile =
    typeof spec.ssoStateFile === "string" &&
    spec.ssoStateFile !== "" &&
    existsSync(spec.ssoStateFile);
  const isJwt = spec.authBackend === "jwt";
  const ssoWrap = forwardEnabled && haveLocalFile && isJwt;

  // Emit a one-line warning (early, before spawn) when the host wants
  // SSO forwarding AND uses jwt auth AND a local state file is missing.
  // Operators can run `bots-app sso-login` or recapture via the
  // dashboard to fix this; we don't want to fail the launch — the bot
  // may still get through if the remote happens to have a cached
  // state, and the warning surfaces in the bot's recentLog for context.
  const ssoMissingWarning =
    forwardEnabled && isJwt && !haveLocalFile && typeof spec.ssoStateFile === "string"
      ? `[${spec.botId ?? "ssh"}] ssh: no local SSO state file at ${spec.ssoStateFile} — bot will hit HCL SSO portal if the target sits behind one`
      : null;

  const render = buildSshCommand(spec.host, spec, { ssoWrap });
  // Slice off the leading "ssh" token — `spawn(cmd, args)` expects the
  // args list NOT to include the program name. buildSshCommand returns
  // the full argv including "ssh" so the display rendering matches what
  // a human would type.
  const args = render.argv.slice(1);
  // When SSO wrap is on we MUST keep stdin open and writable so we can
  // pipe the locally-captured state into the remote `cat > "$T"` loop.
  // Otherwise stdin stays "ignore" exactly as before — a closed stdin
  // is the safer default for non-interactive remote bots.
  const stdio: ["ignore" | "pipe", "pipe", "pipe"] = ssoWrap
    ? ["pipe", "pipe", "pipe"]
    : ["ignore", "pipe", "pipe"];
  const child = spawnImpl("ssh", args, { stdio });

  // Pipe the SSO state. Read as a Buffer (binary) — not a UTF-8 string
  // — so cookie values with non-printable or multi-byte content round-
  // trip unchanged. The remote `cat > "$T"` blocks on stdin EOF, so we
  // close immediately after writing the full payload.
  if (ssoWrap && spec.ssoStateFile !== null && spec.ssoStateFile !== undefined) {
    try {
      const bytes = readFileSync(spec.ssoStateFile);
      child.stdin?.write(bytes);
    } catch (e) {
      // We re-check existsSync above, but the file could vanish between
      // the check and the read on a busy operator workstation. Log the
      // failure and let the bot continue — the remote will still spin
      // up but will hit the SSO portal at runtime.
      console.warn(
        `[${spec.botId ?? "ssh"}] ssh: failed to pipe SSO state to remote: ${(e as Error).message}`,
      );
    } finally {
      child.stdin?.end();
    }
  }

  const handle: SshBotHandle = {
    host: spec.host,
    child,
    recentLog: [],
    totalLines: 0,
    exitCode: null,
    finished: false,
    exit: Promise.resolve(null),
  };
  // Record the exact command as line 1 of recentLog so the log dialog
  // shows what was actually executed. `$ ` prefix matches a typical
  // shell-prompt convention and helps the operator distinguish the
  // header line from real remote stdout.
  handle.recentLog.push(`$ ${render.display}`);
  handle.totalLines += 1;
  // Surface the SSO-missing warning right after the command header so
  // operators reading the per-bot log dialog see it before any remote
  // stdout. Bookkeeping (totalLines + recentLog) matches pushLine so
  // the dashboard's incremental-log fetch stays consistent.
  if (ssoMissingWarning !== null) {
    console.warn(ssoMissingWarning);
    handle.recentLog.push(ssoMissingWarning);
    handle.totalLines += 1;
  }

  const pushLine = (raw: string): void => {
    // Split on newlines; drop trailing empty chunk from a final \n.
    const lines = raw.split(/\r?\n/);
    for (const line of lines) {
      if (line === "") continue;
      handle.recentLog.push(line);
      handle.totalLines += 1;
      if (handle.recentLog.length > REMOTE_LOG_CAP) {
        handle.recentLog.splice(0, handle.recentLog.length - REMOTE_LOG_CAP);
      }
    }
  };
  child.stdout?.on("data", (b: Buffer) => pushLine(b.toString("utf8")));
  child.stderr?.on("data", (b: Buffer) => pushLine(b.toString("utf8")));

  handle.exit = new Promise<number | null>((resolveFn) => {
    let resolved = false;
    const settle = (code: number | null, reason: SshBotHandle["finishReason"]): void => {
      if (resolved) return;
      resolved = true;
      handle.exitCode = code;
      handle.finished = true;
      handle.finishReason = reason;
      resolveFn(code);
    };
    child.on("error", (err: Error) => {
      pushLine(`[ssh-launcher] spawn error: ${err.message}`);
      settle(null, "ssh-error");
    });
    child.on("exit", (code: number | null) => {
      const reason: SshBotHandle["finishReason"] =
        code === 0 ? "remote-exit-ok" : "remote-exit-error";
      settle(code, reason);
    });
  });

  return handle;
}

/**
 * Read a window of recentLog lines starting from `since` (zero-based
 * absolute line number, matching `handle.totalLines`). Used by the
 * GET /bots/:id/log endpoint to support cheap incremental polling.
 *
 * Semantics:
 *   - `since < firstLineNumber` (the buffer has rolled over):
 *      callers get the entire current buffer; the lost lines are
 *      irrecoverable.
 *   - `since >= totalLines`: returns an empty `lines` array and the
 *      caller's `since` cursor stays put.
 */
export function readLogWindow(
  handle: SshBotHandle,
  since: number,
): { lines: string[]; totalLines: number } {
  const firstLineNumber = handle.totalLines - handle.recentLog.length;
  const startIndex = Math.max(0, since - firstLineNumber);
  return {
    lines: handle.recentLog.slice(startIndex),
    totalLines: handle.totalLines,
  };
}

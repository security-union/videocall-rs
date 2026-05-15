import { spawn, type ChildProcess } from "node:child_process";

import {
  buildRemoteLaunchCommand,
  buildSshArgsForLaunch,
  shellEscape,
  type SshHost,
} from "./ssh-hosts";

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
export function buildSshCommand(host: SshHost, spec: SshLaunchSpec): SshCommandRender {
  const remoteCommand = buildRemoteLaunchCommand({
    reposPath: host.reposPath,
    ttl: spec.ttl,
    meetingURL: spec.meetingURL,
    participant: spec.participant,
    network: spec.network ?? null,
    authBackend: spec.authBackend ?? null,
    displayName: spec.displayName ?? null,
    headless: spec.headless !== false,
  });
  const argv = ["ssh", ...buildSshArgsForLaunch(host, remoteCommand)];
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
  const render = buildSshCommand(spec.host, spec);
  // Slice off the leading "ssh" token — `spawn(cmd, args)` expects the
  // args list NOT to include the program name. buildSshCommand returns
  // the full argv including "ssh" so the display rendering matches what
  // a human would type.
  const args = render.argv.slice(1);
  const child = spawnImpl("ssh", args, { stdio: ["ignore", "pipe", "pipe"] });

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

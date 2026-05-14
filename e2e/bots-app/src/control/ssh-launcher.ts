import { spawn, type ChildProcess } from "node:child_process";

import { buildRemoteLaunchCommand, buildSshArgsForLaunch, type SshHost } from "./ssh-hosts";

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
 * Spawn `ssh user@host '<remote-cmd>'` and wrap the resulting
 * ChildProcess in a {@link SshBotHandle}. Lines from stdout/stderr are
 * accumulated in `recentLog` (capped at {@link REMOTE_LOG_CAP}); the
 * `exit` promise resolves once `ssh` exits, with the resolved code
 * being either the remote bot's exit code (when SSH itself succeeded)
 * or one of SSH's own error codes (255 = connect failure).
 *
 * The function is fully synchronous up to the first `spawn`; the
 * returned handle's `exit` promise is what callers await.
 */
export function spawnRemoteBot(spec: SshLaunchSpec, deps: SshLaunchDeps = {}): SshBotHandle {
  const spawnImpl = deps.spawn ?? spawn;
  const remoteCmd = buildRemoteLaunchCommand({
    reposPath: spec.host.reposPath,
    ttl: spec.ttl,
    meetingURL: spec.meetingURL,
    participant: spec.participant,
    network: spec.network ?? null,
    authBackend: spec.authBackend ?? null,
    displayName: spec.displayName ?? null,
    headless: spec.headless !== false,
  });
  const args = buildSshArgsForLaunch(spec.host, remoteCmd);
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

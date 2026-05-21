import { randomUUID } from "node:crypto";

import type { BotHandle } from "../bot";
import { formatDuration, type Ttl } from "../ttl";

// `BotTask` is defined in `../orchestrator`. We pull it in as a
// type-only import to avoid a runtime circular dependency between the
// registry helpers and the orchestrator that imports them.
import type { BotTask } from "../orchestrator";
import type { SshBotHandle } from "./ssh-launcher";

/**
 * Where a bot is physically running. Local bots use Playwright in the
 * orchestrator's own Node process; SSH-hosted bots are launched on a
 * remote machine via the operator's system `ssh` and their lifecycle
 * is tracked through the SSH ChildProcess.
 */
export type BotHostKind = { kind: "local" } | { kind: "ssh"; hostLabel: string };

/**
 * Thrown by SSH-hosted-bot action handlers (mute, camera, share, etc.)
 * that are not proxied through to the remote host in v1. The control
 * server maps this to HTTP 501 with a clear message so the dashboard
 * can pre-emptively disable the corresponding buttons.
 */
export class NotSupportedRemoteError extends Error {
  constructor(action: string) {
    super(
      `Action '${action}' is not available for SSH-hosted bots (v1). Use the CLI directly on the host for now.`,
    );
    this.name = "NotSupportedRemoteError";
  }
}

/**
 * Lifecycle states a bot transitions through in the orchestrator's
 * in-process registry. Used by the control API to render `GET /bots`
 * and `GET /bots/:id` responses, and by the `ctl list` table.
 *
 * Ordering of the variants is the canonical forward progression for a
 * healthy bot:
 *
 *   `priming` → `launching` → `joining` → `in-meeting` → `leaving` → `done`
 *
 * `priming` only fires for local bots that have a manifest + runDir
 * attached; when the auto-prime is skipped (SSH bots, no manifest,
 * everything already cached) the entry transitions straight into
 * `launching`.
 *
 * `failed` is terminal and only reached when `launchBot` rejects with a
 * non-user-hangup error. Entries in `done` / `failed` are kept around
 * for one hour (`REGISTRY_RETENTION_MS`) so the dashboard's Terminated
 * Bots section can surface them for post-mortem log inspection — after
 * that they're swept. The total terminated-entry count is also capped
 * by `TERMINATED_REGISTRY_CAP` (oldest-finished evicted first) to
 * prevent unbounded memory growth on long-running daemons.
 */
export type BotStatus =
  | "priming"
  | "launching"
  | "joining"
  | "in-meeting"
  | "leaving"
  | "done"
  | "failed";

/**
 * Number of milliseconds a `done` / `failed` registry entry is kept
 * around. Bumped to one hour so the dashboard's Terminated Bots section
 * has a useful post-mortem window — operators routinely click a row,
 * see "done", then want to read the logs a few minutes later.
 *
 * The corresponding rolling log buffer (`recentLog`) lives on the same
 * entry, so logs stay available for the full retention window.
 */
export const REGISTRY_RETENTION_MS = 3_600_000;

/**
 * Hard cap on the number of terminated (`done` / `failed`) registry
 * entries kept in memory. Above this cap, `sweepStaleEntries` evicts
 * the oldest-finished terminated entries until the cap is satisfied.
 * Prevents unbounded growth on long-running daemons where the operator
 * launches hundreds of bots over the course of a day.
 *
 * Running entries are NEVER evicted by this cap — only terminated ones.
 */
export const TERMINATED_REGISTRY_CAP = 100;

/**
 * One bot's entry in the orchestrator's in-process registry.
 *
 * Fields prefixed by lifecycle stage:
 *   - `botId`, `task`, `startedAt` — set at construction; immutable.
 *   - `handle` — populated once `launchBot` resolves; null while the
 *      bot is in `launching` and remains null on `failed`.
 *   - `status`, `lastError`, `finishReason` — mutated by the
 *      orchestrator + the control server as the bot progresses.
 *   - `ttl`, `ttlDeadline` — `ttl` is the most recently-configured
 *      lifetime (mutable via the `/bots/:id/ttl` endpoint). For finite
 *      TTLs the orchestrator also writes `ttlDeadline` (absolute
 *      `Date.now() + remaining ms`) so the control surface can render
 *      a true remaining-time without coordinating with the in-flight
 *      timer.
 *   - `network` — the most recently-applied netsim profile (mutable
 *      via `/bots/:id/network`).
 *   - `finishedAt` — set when the entry transitions to `done` /
 *      `failed`. Used by the sweeper.
 */
export interface BotRegistryEntry {
  readonly botId: string;
  task: BotTask;
  handle: BotHandle | null;
  /**
   * Where this bot is running. `{ kind: "local" }` (the default) means
   * it's a Playwright bot in the orchestrator's own Node process; the
   * `handle` field above tracks its lifecycle. `{ kind: "ssh", … }`
   * means it was launched via SSH and the lifecycle is tracked via
   * {@link sshHandle} instead.
   */
  host: BotHostKind;
  /**
   * Set only when `host.kind === "ssh"`. Owns the live SSH
   * ChildProcess + the rolling log buffer the dashboard's log viewer
   * paginates over.
   */
  sshHandle: SshBotHandle | null;
  status: BotStatus;
  readonly startedAt: number;
  ttl: Ttl;
  /**
   * Absolute timestamp (ms since epoch) when this bot's TTL fires.
   * `null` when `ttl === "infinite"`. Updated by `POST /bots/:id/ttl`.
   */
  ttlDeadline: number | null;
  /** Most recently-applied netsim profile (e.g. `"lossy_mobile"`). */
  network: string | null;
  /** Set when status === "failed". */
  lastError?: string;
  /**
   * Set on transition to `done` / `failed`. Mirrors `BotExitReason.kind`
   * plus the control-API-initiated variants. Known values today:
   *   - `"ttl-expired"`              (done)
   *   - `"shutdown-signal"`          (done)
   *   - `"user-hangup"`              (done)
   *   - `"waiting-room:waiting-room"`     (done — host's Waiting Room is on)
   *   - `"waiting-room:waiting-for-host"` (done — host hasn't started yet)
   *   - `"ctl-leave"`                (done)
   *   - `"ctl-kill"`                 (done)
   *   - `"meeting-rejected:rejected"`(failed — host denied)
   *   - `"meeting-rejected:error"`   (failed — server-reported error)
   *   - `"launch-error"`             (failed)
   */
  finishReason?: string;
  /**
   * Wall-clock timestamp (ms since epoch) when this entry transitioned
   * to `done` / `failed`. Set by the orchestrator at each terminal
   * transition site. Used by:
   *   - the sweeper, to decide when to drop the entry,
   *   - the over-cap evictor, to pick the oldest-finished terminated
   *     entries when the registry exceeds {@link TERMINATED_REGISTRY_CAP},
   *   - the dashboard's Terminated Bots table, to sort newest-first
   *     and render a "X minutes ago" relative timestamp.
   */
  finishedAt?: number;
  /**
   * Rolling log buffer for **local** bots, mirrored over
   * `GET /bots/:id/log`. Populated by the orchestrator's
   * `appendLocalLog` helper — currently just auto-prime progress
   * events, but the buffer is general-purpose so future
   * orchestrator-side log lines (e.g. launch errors) can land here
   * too. SSH-hosted bots use `sshHandle.recentLog` instead — see
   * `botLogRoute` in `control/server.ts` for the unified read path.
   */
  recentLog: string[];
  /** Total log lines pushed since the bot was registered (local bots). */
  totalLines: number;
}

/**
 * Snapshot view of a registry entry safe to serialize over the
 * control API. Strips the live `BotHandle` (browser, page) and
 * derives `ttlRemainingMs` so clients don't have to do clock math.
 */
export interface BotSnapshot {
  botId: string;
  participant: string;
  status: BotStatus;
  startedAt: number;
  meetingURL: string;
  network: string | null;
  ttl: string;
  ttlRemainingMs: number | null;
  finishReason?: string;
  lastError?: string;
  /**
   * Wall-clock timestamp (ms since epoch) when the bot transitioned to
   * `done` / `failed`. `null` while the bot is still running. Used by
   * the dashboard's Terminated Bots table to sort newest-first and to
   * render a "X minutes ago" relative timestamp.
   */
  finishedAt: number | null;
  /**
   * Where the bot is running. Mirrors `BotRegistryEntry.host` 1:1 —
   * the dashboard's bots-table renders a small chip ("local" or
   * "ssh:<label>") based on this. `null`/undefined would be
   * back-compat-friendly, but we always emit a value to keep the
   * client logic simple.
   */
  host: BotHostKind;
}

/**
 * Generate a fresh bot id. Always a v4 UUID — short enough to fit in
 * log prefixes when truncated to the first 8 chars, unique enough to
 * never collide within a single orchestrator process.
 */
export function generateBotId(): string {
  return randomUUID();
}

/**
 * Short log-prefix form of a bot id. The full UUID is unwieldy in
 * logs; the first 8 hex chars are unique enough across a fleet of
 * <100 bots and let the operator correlate ctl output (which shows
 * the full id) with stdout (which shows the short form).
 */
export function shortBotId(botId: string): string {
  return botId.slice(0, 8);
}

/**
 * Construct a fresh registry entry for `task`. Computes
 * `ttlDeadline` from `task.ttl` at construction time — the
 * orchestrator does not actually start the timer here, but anchoring
 * the deadline to "now" matches the semantics of "the TTL clock
 * starts the moment the orchestrator picks up the task." If
 * `task.ttl === "infinite"`, `ttlDeadline === null`.
 *
 * The fresh entry is in `launching` state with `handle: null`. The
 * orchestrator is responsible for transitioning it forward.
 */
export function newRegistryEntry(
  task: BotTask,
  host: BotHostKind = { kind: "local" },
): BotRegistryEntry {
  const now = Date.now();
  const ttlDeadline = task.ttl === "infinite" ? null : now + task.ttl;
  return {
    botId: task.botId,
    task,
    handle: null,
    host,
    sshHandle: null,
    status: "launching",
    startedAt: now,
    ttl: task.ttl,
    ttlDeadline,
    network: task.network ?? null,
    recentLog: [],
    totalLines: 0,
  };
}

/**
 * Cap on the per-bot {@link BotRegistryEntry.recentLog} buffer. Mirrors
 * the SSH path's {@link ../control/ssh-launcher.REMOTE_LOG_CAP} so the
 * dashboard's log dialog renders the same volume of lines for either
 * local or SSH bots.
 */
export const LOCAL_LOG_CAP = 200;

/**
 * Append a line to a local bot's rolling log buffer, dropping the
 * oldest entry when the cap is exceeded. Idempotent on empty input.
 *
 * Mirrors the SSH `pushLine` helper in `ssh-launcher.ts` so the
 * server-side log buffer semantics are identical for both bot kinds.
 */
export function appendLocalLog(entry: BotRegistryEntry, line: string): void {
  if (line === "") return;
  entry.recentLog.push(line);
  entry.totalLines += 1;
  if (entry.recentLog.length > LOCAL_LOG_CAP) {
    entry.recentLog.splice(0, entry.recentLog.length - LOCAL_LOG_CAP);
  }
}

/**
 * Read a window of recentLog lines starting from `since` (zero-based
 * absolute line number, matching `entry.totalLines`). Mirrors
 * `ssh-launcher::readLogWindow` so the server's log route can use
 * one shape regardless of the underlying bot kind.
 */
export function readLocalLogWindow(
  entry: BotRegistryEntry,
  since: number,
): { lines: string[]; totalLines: number } {
  const firstLineNumber = entry.totalLines - entry.recentLog.length;
  const startIndex = Math.max(0, since - firstLineNumber);
  return {
    lines: entry.recentLog.slice(startIndex),
    totalLines: entry.totalLines,
  };
}

/**
 * Convert a registry entry to the JSON shape exposed by the control
 * API. The control server uses this on every `GET /bots` and
 * `GET /bots/:id` response — keeping the conversion centralized
 * means the snapshot can never accidentally leak the live
 * `BotHandle`.
 */
export function snapshotEntry(entry: BotRegistryEntry, now: number = Date.now()): BotSnapshot {
  let ttlRemainingMs: number | null = null;
  if (entry.ttlDeadline !== null) {
    ttlRemainingMs = Math.max(0, entry.ttlDeadline - now);
  }
  const snap: BotSnapshot = {
    botId: entry.botId,
    participant: entry.task.participant,
    status: entry.status,
    startedAt: entry.startedAt,
    meetingURL: entry.task.meetingURL,
    network: entry.network,
    ttl: formatDuration(entry.ttl),
    ttlRemainingMs,
    host: entry.host,
    finishedAt: entry.finishedAt ?? null,
  };
  if (entry.finishReason !== undefined) snap.finishReason = entry.finishReason;
  if (entry.lastError !== undefined) snap.lastError = entry.lastError;
  return snap;
}

/**
 * Drop `done` / `failed` entries older than `REGISTRY_RETENTION_MS`
 * AND apply the `TERMINATED_REGISTRY_CAP` soft cap (drop oldest-finished
 * terminated entries until the cap is satisfied). Idempotent;
 * cheap (O(N) over registry size, plus an O(N log N) sort if the cap
 * is exceeded — but the cap is tiny so this is fine).
 *
 * Running entries (`priming` / `launching` / `joining` / `in-meeting`
 * / `leaving`) are NEVER dropped — only terminated ones.
 */
export function sweepStaleEntries(
  registry: Map<string, BotRegistryEntry>,
  now: number = Date.now(),
): void {
  // Pass 1: age-based eviction.
  for (const [id, entry] of registry) {
    if (
      (entry.status === "done" || entry.status === "failed") &&
      entry.finishedAt !== undefined &&
      now - entry.finishedAt > REGISTRY_RETENTION_MS
    ) {
      registry.delete(id);
    }
  }

  // Pass 2: enforce the cap. Collect remaining terminated entries and
  // sort by `finishedAt` ascending — the head of the list is the oldest.
  // Entries with no `finishedAt` set (shouldn't happen for terminated
  // entries, but defensive) are treated as "very old" so they're
  // evicted first.
  const terminated: BotRegistryEntry[] = [];
  for (const entry of registry.values()) {
    if (entry.status === "done" || entry.status === "failed") {
      terminated.push(entry);
    }
  }
  if (terminated.length > TERMINATED_REGISTRY_CAP) {
    terminated.sort((a, b) => (a.finishedAt ?? 0) - (b.finishedAt ?? 0));
    const evictCount = terminated.length - TERMINATED_REGISTRY_CAP;
    for (let i = 0; i < evictCount; i++) {
      registry.delete(terminated[i].botId);
    }
  }
}

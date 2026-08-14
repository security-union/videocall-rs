import { readFileSync } from "node:fs";

import { Command } from "commander";
import { parse as parseYaml } from "yaml";

import { type CtlClientConfig, ctlRequest } from "./client";
import { NetemValidationError, resolveNetemRequest } from "./netem";
import { type BotSnapshot } from "./registry";

/**
 * Increment 4 (#2072): the CONDUCTOR.
 *
 * A conductor runs a scripted scenario timeline against the bot fleet's
 * per-pod control servers (Increment 3). Each `bot: <N>` in the timeline
 * is resolved to a StatefulSet pod's stable DNS name
 * (`videocall-bots-<N>.videocall-bots.bot-load.svc.cluster.local`) and the
 * action is issued to that pod's control API at its scheduled offset from
 * t0. This lets a load test choreograph a repeatable scenario — "bot 1
 * shares its screen at t+10s, bot 2's link goes lossy at t+20s, bot 0
 * talks for 15s at t+40s" — instead of hand-driving `ctl` calls.
 *
 * DESIGN — the module is split into pure, independently-testable pieces:
 *   1. {@link parseScenario}       YAML text  → validated `ParsedScenario`
 *   2. {@link buildSchedule}       entries    → a flat, sorted `ScheduledCall[]`
 *      (this is where `talk` expands into an unmute + a follow-up mute and
 *      overlapping talk windows are coalesced — see below)
 *   3. {@link runSchedule}         schedule   → issues each call at its offset
 *      using an INJECTABLE clock + sleep so tests never real-sleep.
 * The HTTP surface ({@link httpConductorClientFactory}) is behind the
 * {@link ConductorClient} seam so tests mock the client and assert the
 * exact method + args each action maps to, with no live server.
 *
 * SECURITY: the bearer token is passed to the client factory but is NEVER
 * written to a log line — only the target host, bot ordinal, action, and
 * offset are logged.
 */

// ── Action vocabulary ────────────────────────────────────────────────────

/**
 * Every action a timeline entry may name. MUST stay in sync with the
 * deploy task's `scenario.example.yaml`. `talk` is a convenience macro
 * (unmute now + mute after `durationMs`); every other action maps 1:1 to a
 * single control call.
 */
export const CONDUCT_ACTIONS = [
  "mute",
  "unmute",
  "camera-on",
  "camera-off",
  "screenshare-on",
  "screenshare-off",
  "netem",
  "netem-clear",
  "talk",
  "leave",
] as const;

export type ActionKind = (typeof CONDUCT_ACTIONS)[number];

/**
 * Thrown for any malformed scenario (bad YAML, unknown action, bad `at`,
 * missing params, …). The CLI maps this to a usage-error exit code (2) so
 * a broken timeline is distinguishable from a mid-run transport failure.
 */
export class ScenarioValidationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ScenarioValidationError";
  }
}

// ── `at` duration parsing ────────────────────────────────────────────────

/**
 * Accepts an integer or decimal magnitude with an `s` (seconds) or `ms`
 * (milliseconds) suffix. `ms` is listed first in the alternation so it
 * wins over the `s` that is its own final character.
 */
const AT_PATTERN = /^(\d+(?:\.\d+)?)(ms|s)$/;

/**
 * Parse an `at` offset (e.g. `"0s"`, `"10s"`, `"500ms"`) into whole
 * milliseconds. Rounds fractional milliseconds to the nearest integer so a
 * value like `"0.5ms"` never produces a sub-millisecond sleep. Throws a
 * {@link ScenarioValidationError} on anything that is not a
 * non-negative `<number>(s|ms)`.
 */
export function parseAtDuration(raw: string): number {
  const s = raw.trim();
  const m = AT_PATTERN.exec(s);
  if (m === null) {
    throw new ScenarioValidationError(
      `expected a duration like "0s", "10s", or "500ms" (got "${raw}")`,
    );
  }
  const value = Number(m[1]);
  return m[2] === "s" ? Math.round(value * 1000) : Math.round(value);
}

// ── Parsed scenario shapes ───────────────────────────────────────────────

/**
 * One validated timeline entry. Action-specific fields are populated only
 * for the action that uses them: `netemBody`/`netemLabel` for `netem`,
 * `durationMs` for `talk`.
 */
export interface ParsedEntry {
  /** Offset from t0 in whole milliseconds. */
  atMs: number;
  /** StatefulSet pod ordinal (the `bot: <N>` field). */
  bot: number;
  action: ActionKind;
  /** Validated request body for a `netem` action's `POST /netem`. */
  netemBody?: Record<string, unknown>;
  /** Human label for a `netem` action (profile name or `"custom"`). */
  netemLabel?: string;
  /** Talk-window length in ms (the follow-up mute lands at `atMs + durationMs`). */
  durationMs?: number;
}

export interface ParsedScenario {
  /** Informational only — never used for addressing. */
  room?: string;
  entries: ParsedEntry[];
}

const NETEM_PARAM_KEYS = ["profile", "delayMs", "jitterMs", "lossPct", "rateKbit"] as const;

/**
 * Collect the netem-relevant keys present on a raw entry into a request
 * body. Values are passed through un-coerced — {@link resolveNetemRequest}
 * (the SAME validator the control server runs) is the single source of
 * truth for range/grammar checks, so an out-of-range value produces one
 * consistent message on both the conductor and the pod.
 */
function collectNetemBody(o: Record<string, unknown>): Record<string, unknown> {
  const body: Record<string, unknown> = {};
  for (const k of NETEM_PARAM_KEYS) {
    if (o[k] !== undefined) body[k] = o[k];
  }
  return body;
}

/**
 * Parse + validate a scenario YAML document. Returns a
 * {@link ParsedScenario} or throws {@link ScenarioValidationError} with a
 * message that names the offending `timeline[i]` entry.
 */
export function parseScenario(text: string): ParsedScenario {
  let doc: unknown;
  try {
    doc = parseYaml(text);
  } catch (e) {
    throw new ScenarioValidationError(`scenario is not valid YAML: ${(e as Error).message}`);
  }
  if (doc === null || typeof doc !== "object" || Array.isArray(doc)) {
    throw new ScenarioValidationError("scenario must be a YAML mapping with a `timeline` array");
  }
  const o = doc as Record<string, unknown>;

  let room: string | undefined;
  if (o.room !== undefined) {
    if (typeof o.room !== "string") {
      throw new ScenarioValidationError("`room` must be a string when present");
    }
    room = o.room;
  }

  if (!Array.isArray(o.timeline)) {
    throw new ScenarioValidationError("`timeline` must be an array");
  }
  if (o.timeline.length === 0) {
    throw new ScenarioValidationError("`timeline` must contain at least one entry");
  }
  const entries = o.timeline.map((raw, i) => parseEntry(raw, i));
  return { room, entries };
}

function parseEntry(raw: unknown, index: number): ParsedEntry {
  const where = `timeline[${index}]`;
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) {
    throw new ScenarioValidationError(`${where} must be a mapping`);
  }
  const o = raw as Record<string, unknown>;

  if (typeof o.at !== "string") {
    throw new ScenarioValidationError(`${where}.at must be a string duration (e.g. "10s")`);
  }
  let atMs: number;
  try {
    atMs = parseAtDuration(o.at);
  } catch (e) {
    throw new ScenarioValidationError(`${where}.at: ${(e as Error).message}`);
  }

  if (typeof o.bot !== "number" || !Number.isInteger(o.bot) || o.bot < 0) {
    throw new ScenarioValidationError(`${where}.bot must be a non-negative integer pod ordinal`);
  }
  const bot = o.bot;

  if (typeof o.action !== "string") {
    throw new ScenarioValidationError(`${where}.action must be a string`);
  }
  if (!(CONDUCT_ACTIONS as readonly string[]).includes(o.action)) {
    throw new ScenarioValidationError(
      `${where}.action "${o.action}" is unknown (known: ${CONDUCT_ACTIONS.join(", ")})`,
    );
  }
  const action = o.action as ActionKind;

  const entry: ParsedEntry = { atMs, bot, action };

  if (action === "netem") {
    const body = collectNetemBody(o);
    // Validate through the production netem resolver so a missing/invalid
    // param fails HERE with the same wording the pod would use.
    try {
      resolveNetemRequest(body);
    } catch (e) {
      if (e instanceof NetemValidationError) {
        throw new ScenarioValidationError(`${where}: ${e.message}`);
      }
      throw e;
    }
    entry.netemBody = body;
    entry.netemLabel = typeof body.profile === "string" ? body.profile : "custom";
  } else if (action === "talk") {
    const d = o.durationMs;
    if (typeof d !== "number" || !Number.isInteger(d) || d <= 0) {
      throw new ScenarioValidationError(
        `${where}.durationMs must be a positive integer (ms) for a "talk" action`,
      );
    }
    entry.durationMs = d;
  }

  return entry;
}

// ── Bot → pod DNS resolution ─────────────────────────────────────────────

export interface HostResolveOptions {
  /** StatefulSet + headless Service name. */
  service: string;
  /** Namespace the fleet runs in. */
  namespace: string;
  /** Cluster DNS suffix appended after `.<namespace>.`. */
  dnsSuffix: string;
}

/**
 * Resolve a `bot: <N>` ordinal to its StatefulSet pod's stable FQDN. The
 * headless Service (`clusterIP: None`) publishes per-pod records of the
 * form `<service>-<N>.<service>.<namespace>.<dns-suffix>`; a request to
 * that name connects straight to pod N's IP (no VIP to load-balance
 * across). See `k8s/service.yaml`.
 */
export function resolveBotHost(bot: number, opts: HostResolveOptions): string {
  return `${opts.service}-${bot}.${opts.service}.${opts.namespace}.${opts.dnsSuffix}`;
}

// ── Control-call model + the client seam ─────────────────────────────────

/**
 * A single resolved control operation. `talk` and the `*-on/*-off` action
 * pairs have all collapsed into these six shapes by the time a call
 * reaches the client, so {@link applyAction} is an exhaustive switch.
 */
export type ControlCall =
  | { kind: "mute"; muted: boolean }
  | { kind: "camera"; off: boolean }
  | { kind: "share"; on: boolean }
  | { kind: "netem"; body: Record<string, unknown>; label: string }
  | { kind: "netem-clear" }
  | { kind: "leave" };

/**
 * The seam the conductor drives. One instance is created per target host.
 * The HTTP implementation lazily resolves the pod's single bot id for the
 * mute/camera/share/leave routes; `applyNetem`/`clearNetem` are top-level
 * (`/netem`) and never need a bot id.
 */
export interface ConductorClient {
  /** `POST /bots/:id/mute` — `muted === true` mutes the mic. */
  mute(muted: boolean): Promise<void>;
  /** `POST /bots/:id/video` — `off === true` turns the camera off. */
  setCameraOff(off: boolean): Promise<void>;
  /** `POST /bots/:id/share` — `on === true` starts sharing. */
  setScreenShare(on: boolean): Promise<void>;
  /** `POST /bots/:id/leave` — graceful leave. */
  leave(): Promise<void>;
  /** `POST /netem` — apply a profile or raw shaping params. */
  applyNetem(body: Record<string, unknown>): Promise<void>;
  /** `DELETE /netem` — remove all shaping. */
  clearNetem(): Promise<void>;
}

/**
 * Builds a {@link ConductorClient} for one host. `host` is required (a
 * conductor always targets a specific pod, never loopback-by-default).
 */
export type ConductorClientFactory = (
  config: CtlClientConfig & { host: string },
) => ConductorClient;

/**
 * Dispatch a resolved {@link ControlCall} to the client. Exhaustive over
 * every `ControlCall` variant — adding a variant without a case here is a
 * compile error.
 */
export async function applyAction(client: ConductorClient, call: ControlCall): Promise<void> {
  switch (call.kind) {
    case "mute":
      return client.mute(call.muted);
    case "camera":
      return client.setCameraOff(call.off);
    case "share":
      return client.setScreenShare(call.on);
    case "netem":
      return client.applyNetem(call.body);
    case "netem-clear":
      return client.clearNetem();
    case "leave":
      return client.leave();
  }
}

/** Human-readable one-liner for a call. NEVER includes any token. */
export function describeCall(call: ControlCall): string {
  switch (call.kind) {
    case "mute":
      return call.muted ? "mute (mic off)" : "unmute (mic on)";
    case "camera":
      return call.off ? "camera-off" : "camera-on";
    case "share":
      return call.on ? "screenshare-on" : "screenshare-off";
    case "netem":
      return `netem (${call.label})`;
    case "netem-clear":
      return "netem-clear";
    case "leave":
      return "leave";
  }
}

// ── Schedule building ────────────────────────────────────────────────────

export interface ScheduledCall {
  /** Offset from t0 in ms at which this call fires. */
  atMs: number;
  bot: number;
  host: string;
  call: ControlCall;
  /**
   * Deterministic tiebreak for equal `atMs`. Assigned in the order calls
   * are materialized so the final ordering is stable regardless of the
   * platform sort's stability guarantees.
   */
  seq: number;
}

interface Interval {
  start: number;
  end: number;
}

/**
 * Merge a set of [start, end] intervals, coalescing any that overlap OR
 * touch (`next.start <= cur.end`). Coalescing touching windows avoids an
 * instantaneous mute-then-unmute flap at a shared boundary.
 */
function mergeIntervals(intervals: Interval[]): Interval[] {
  if (intervals.length === 0) return [];
  const sorted = [...intervals].sort((a, b) => a.start - b.start || a.end - b.end);
  const merged: Interval[] = [{ ...sorted[0] }];
  for (let i = 1; i < sorted.length; i++) {
    const cur = sorted[i];
    const last = merged[merged.length - 1];
    if (cur.start <= last.end) {
      last.end = Math.max(last.end, cur.end);
    } else {
      merged.push({ ...cur });
    }
  }
  return merged;
}

/** Map a non-`talk` entry to its single {@link ControlCall}. */
function callForEntry(entry: ParsedEntry): ControlCall {
  switch (entry.action) {
    case "mute":
      return { kind: "mute", muted: true };
    case "unmute":
      return { kind: "mute", muted: false };
    case "camera-on":
      return { kind: "camera", off: false };
    case "camera-off":
      return { kind: "camera", off: true };
    case "screenshare-on":
      return { kind: "share", on: true };
    case "screenshare-off":
      return { kind: "share", on: false };
    case "netem":
      // netemBody/netemLabel are always populated for a validated netem entry.
      return { kind: "netem", body: entry.netemBody ?? {}, label: entry.netemLabel ?? "custom" };
    case "netem-clear":
      return { kind: "netem-clear" };
    case "leave":
      return { kind: "leave" };
    case "talk":
      // Talk is expanded in buildSchedule, never routed through here.
      throw new Error("callForEntry: talk must be expanded by buildSchedule");
  }
}

/**
 * Flatten a validated timeline into a sorted list of concrete calls.
 *
 * `talk` entries are collected per-bot and their [start, start+duration]
 * windows are MERGED (see {@link mergeIntervals}) before emitting an
 * unmute at each merged window's start and a mute at its end. This is what
 * "handle overlapping talk windows" means: two talk windows that overlap
 * on the same bot yield a single unmute…mute pair spanning both, so the
 * mic is never muted while another talk window is still open. Because
 * every follow-up mute is materialized here (not scheduled dynamically at
 * run time), it is guaranteed to be in the schedule the runner walks — it
 * cannot be lost behind a later action.
 *
 * Non-talk entries map 1:1 to a call at their own `atMs`.
 */
export function buildSchedule(
  entries: ParsedEntry[],
  hostOpts: HostResolveOptions,
): ScheduledCall[] {
  const out: ScheduledCall[] = [];
  let seq = 0;
  const push = (atMs: number, bot: number, call: ControlCall): void => {
    out.push({ atMs, bot, host: resolveBotHost(bot, hostOpts), call, seq: seq++ });
  };

  const talkWindowsByBot = new Map<number, Interval[]>();
  for (const e of entries) {
    if (e.action === "talk") {
      const windows = talkWindowsByBot.get(e.bot) ?? [];
      // durationMs is guaranteed present + positive for a validated talk entry.
      windows.push({ start: e.atMs, end: e.atMs + (e.durationMs ?? 0) });
      talkWindowsByBot.set(e.bot, windows);
      continue;
    }
    push(e.atMs, e.bot, callForEntry(e));
  }

  for (const [bot, windows] of talkWindowsByBot) {
    for (const iv of mergeIntervals(windows)) {
      push(iv.start, bot, { kind: "mute", muted: false });
      push(iv.end, bot, { kind: "mute", muted: true });
    }
  }

  out.sort((a, b) => a.atMs - b.atMs || a.seq - b.seq);
  return out;
}

// ── The runner ───────────────────────────────────────────────────────────

/** Injectable monotonic clock (ms). Production uses `Date.now`. */
export interface Clock {
  now(): number;
}

/** Injectable sleep. Production uses `setTimeout`; tests advance a fake clock. */
export type SleepFn = (ms: number) => Promise<void>;

export interface ConductDeps {
  clientFactory: ConductorClientFactory;
  clock: Clock;
  sleep: SleepFn;
  log: (line: string) => void;
}

export interface ConductSummary {
  /** Total calls in the schedule. */
  planned: number;
  /** Calls that returned successfully. */
  fired: number;
  /** Calls that threw (logged, non-fatal — the run continues). */
  failed: number;
  dryRun: boolean;
}

/**
 * Walk the schedule, sleeping until each call's offset from t0 and issuing
 * it. Timing is anchored to `deps.clock` sampled ONCE at t0 and the wait
 * is recomputed against the clock every iteration, so a slow/overshooting
 * prior action self-corrects (the next wait shrinks) rather than drifting.
 * A per-call failure is logged and counted but does NOT abort the run — a
 * single unreachable pod must not sink the whole scenario.
 */
async function runSchedule(
  schedule: ScheduledCall[],
  deps: ConductDeps & { port: number; token: string },
): Promise<ConductSummary> {
  const t0 = deps.clock.now();
  // One client per host so a pod's bot-id lookup is done once and reused.
  const clients = new Map<string, ConductorClient>();
  let fired = 0;
  let failed = 0;

  for (const sc of schedule) {
    const due = t0 + sc.atMs;
    const waitMs = due - deps.clock.now();
    if (waitMs > 0) await deps.sleep(waitMs);

    let client = clients.get(sc.host);
    if (client === undefined) {
      // The token flows into the client here but is never logged below.
      client = deps.clientFactory({ host: sc.host, port: deps.port, token: deps.token });
      clients.set(sc.host, client);
    }

    deps.log(`[t+${sc.atMs}ms] bot ${sc.bot} (${sc.host}) -> ${describeCall(sc.call)}`);
    try {
      await applyAction(client, sc.call);
      fired += 1;
    } catch (e) {
      failed += 1;
      deps.log(
        `  ! bot ${sc.bot} (${sc.host}) ${describeCall(sc.call)} failed: ${(e as Error).message}`,
      );
    }
  }

  deps.log(`conduct: done - ${fired} action(s) fired, ${failed} failed`);
  return { planned: schedule.length, fired, failed, dryRun: false };
}

/** Print the resolved schedule (host + action + offset). No calls issued. */
export function printSchedule(schedule: ScheduledCall[], log: (line: string) => void): void {
  for (const sc of schedule) {
    log(`[t+${sc.atMs}ms] bot ${sc.bot} (${sc.host}) -> ${describeCall(sc.call)}`);
  }
}

function countBots(schedule: ScheduledCall[]): number {
  return new Set(schedule.map((s) => s.bot)).size;
}

/**
 * Top-level entry point used by the CLI action AND the tests. Parses,
 * builds the schedule, then either prints it (`dryRun`) or runs it.
 *
 * A `token` is required for a live run (not for `--dry-run`). The missing-
 * token case throws a {@link ScenarioValidationError} so the CLI exits 2.
 */
export async function conductScenario(params: {
  scenarioText: string;
  hostOpts: HostResolveOptions;
  port: number;
  dryRun: boolean;
  token?: string;
  deps: ConductDeps;
}): Promise<ConductSummary> {
  const { deps } = params;
  const scenario = parseScenario(params.scenarioText);
  const schedule = buildSchedule(scenario.entries, params.hostOpts);

  deps.log(
    `conduct: room=${scenario.room ?? "(unspecified)"} - ${schedule.length} scheduled action(s) across ${countBots(schedule)} bot(s)`,
  );

  if (params.dryRun) {
    printSchedule(schedule, deps.log);
    deps.log("conduct: dry-run - no control calls issued");
    return { planned: schedule.length, fired: 0, failed: 0, dryRun: true };
  }

  if (params.token === undefined || params.token.length === 0) {
    throw new ScenarioValidationError(
      "a control token is required to run a scenario (pass --token-file or set BOT_CTL_TOKEN); use --dry-run to preview without a token",
    );
  }

  return runSchedule(schedule, { ...deps, port: params.port, token: params.token });
}

// ── HTTP client implementation ───────────────────────────────────────────

/**
 * The live {@link ConductorClient}. mute/camera/share/leave target the
 * pod's SINGLE bot, whose (random UUID) id is not known ahead of time, so
 * it is resolved once via `GET /bots` and cached. netem is a top-level
 * route and needs no bot id — a netem-only scenario never triggers a
 * bot-id lookup.
 */
class HttpConductorClient implements ConductorClient {
  private botIdPromise: Promise<string> | null = null;

  constructor(private readonly config: CtlClientConfig) {}

  private botId(): Promise<string> {
    // Memoize the in-flight promise so concurrent first calls share one
    // GET /bots rather than racing several.
    if (this.botIdPromise === null) {
      this.botIdPromise = this.resolveBotId().catch((e) => {
        // Do NOT cache a REJECTED lookup. A first action at t=0 can hit the pod
        // mid-boot (Service publishNotReadyAddresses + no readiness probe → DNS
        // resolves before the ctl server binds) → GET /bots refuses. Caching that
        // rejection would poison every later mute/camera/share/leave for this pod
        // for the whole scenario. Reset so a later scheduled action retries.
        this.botIdPromise = null;
        throw e;
      });
    }
    return this.botIdPromise;
  }

  private async resolveBotId(): Promise<string> {
    const res = await ctlRequest<{ bots: BotSnapshot[] }>(this.config, "GET", "/bots");
    const bots = Array.isArray(res.bots) ? res.bots : [];
    // Prefer a live bot; a terminated (done/failed) entry may linger in the
    // registry's retention window and must not be targeted.
    const pick = bots.find((b) => b.status !== "done" && b.status !== "failed") ?? bots[0];
    if (pick === undefined) {
      throw new Error(
        `no bot registered on ${this.config.host ?? "127.0.0.1"} - cannot target its meeting controls (netem actions do not need a bot)`,
      );
    }
    return pick.botId;
  }

  async mute(muted: boolean): Promise<void> {
    const id = await this.botId();
    await ctlRequest(this.config, "POST", `/bots/${encodeURIComponent(id)}/mute`, { mic: muted });
  }

  async setCameraOff(off: boolean): Promise<void> {
    const id = await this.botId();
    await ctlRequest(this.config, "POST", `/bots/${encodeURIComponent(id)}/video`, { camera: off });
  }

  async setScreenShare(on: boolean): Promise<void> {
    const id = await this.botId();
    await ctlRequest(this.config, "POST", `/bots/${encodeURIComponent(id)}/share`, { share: on });
  }

  async leave(): Promise<void> {
    const id = await this.botId();
    await ctlRequest(this.config, "POST", `/bots/${encodeURIComponent(id)}/leave`);
  }

  async applyNetem(body: Record<string, unknown>): Promise<void> {
    await ctlRequest(this.config, "POST", "/netem", body);
  }

  async clearNetem(): Promise<void> {
    await ctlRequest(this.config, "DELETE", "/netem");
  }
}

/** Production factory: real HTTP calls via {@link ctlRequest}. */
export function httpConductorClientFactory(): ConductorClientFactory {
  return (config) => new HttpConductorClient(config);
}

// ── CLI wiring ───────────────────────────────────────────────────────────

interface ConductCommandOptions {
  scenario: string;
  service: string;
  namespace: string;
  port: string;
  tokenFile?: string;
  dnsSuffix: string;
  dryRun: boolean;
}

/**
 * Register the `bots-app conduct` subcommand onto the supplied program.
 */
export function registerConductCommand(program: Command): void {
  program
    .command("conduct")
    .description(
      "Increment 4 (#2072): run a scripted scenario timeline against the fleet's per-pod control servers. Resolves each `bot: <N>` to its StatefulSet pod DNS name and drives that pod's control API (mute/camera/screenshare/netem/leave) at the scheduled offset from t0. Use --dry-run to preview the resolved schedule without connecting.",
    )
    .requiredOption(
      "--scenario <file>",
      "Path to the scenario timeline YAML (see scenario.example.yaml).",
    )
    .option(
      "--service <name>",
      "StatefulSet + headless Service name. Pod FQDN is <service>-<N>.<service>.<namespace>.<dns-suffix>.",
      "videocall-bots",
    )
    .option("--namespace <ns>", "Namespace the bot fleet runs in.", "bot-load")
    .option("--port <port>", "Control-API port on each pod.", "8080")
    .option(
      "--token-file <path>",
      "File whose contents are the shared control-API bearer token (the bot-ctl-token Secret value; trailing newline trimmed). Falls back to the BOT_CTL_TOKEN env var. Not required with --dry-run.",
    )
    .option(
      "--dns-suffix <suffix>",
      "Cluster DNS suffix appended after .<namespace>.",
      "svc.cluster.local",
    )
    .option(
      "--dry-run",
      "Print the resolved schedule (bot host + action + time) without connecting to any pod.",
      false,
    )
    .action(async (opts: ConductCommandOptions) => {
      const port = Number.parseInt(opts.port, 10);
      if (!Number.isFinite(port) || port <= 0 || port > 65535) {
        console.error(`conduct: --port must be a positive integer (got "${opts.port}")`);
        process.exit(2);
      }

      let scenarioText: string;
      try {
        scenarioText = readFileSync(opts.scenario, "utf8");
      } catch (e) {
        console.error(
          `conduct: cannot read scenario file "${opts.scenario}": ${(e as Error).message}`,
        );
        process.exit(2);
      }

      let token: string | undefined;
      if (opts.tokenFile !== undefined) {
        try {
          token = readFileSync(opts.tokenFile, "utf8").trim();
        } catch (e) {
          console.error(
            `conduct: cannot read --token-file "${opts.tokenFile}": ${(e as Error).message}`,
          );
          process.exit(2);
        }
      } else if (process.env.BOT_CTL_TOKEN) {
        token = process.env.BOT_CTL_TOKEN;
      }

      const deps: ConductDeps = {
        clientFactory: httpConductorClientFactory(),
        clock: { now: () => Date.now() },
        sleep: (ms) => new Promise<void>((resolve) => setTimeout(resolve, ms)),
        log: (line) => console.log(line),
      };

      try {
        const summary = await conductScenario({
          scenarioText,
          hostOpts: {
            service: opts.service,
            namespace: opts.namespace,
            dnsSuffix: opts.dnsSuffix,
          },
          port,
          dryRun: opts.dryRun,
          token,
          deps,
        });
        // A live run with any failed call exits non-zero so CI / a wrapper
        // script sees the scenario did not fully apply.
        if (!summary.dryRun && summary.failed > 0) {
          process.exit(1);
        }
      } catch (e) {
        if (e instanceof ScenarioValidationError) {
          console.error(`conduct: ${e.message}`);
          process.exit(2);
        }
        console.error(`conduct: ${(e as Error).message}`);
        process.exit(1);
      }
    });
}

import { readFileSync } from "node:fs";

import { parse as parseYaml, stringify as stringifyYaml } from "yaml";

import { type Manifest } from "./manifest";

/**
 * Canonical list of netsim preset names accepted on the `network`
 * field (both per-bot and meeting-level). The single source of truth
 * is the Rust crate at `videocall-netsim/src/profiles.rs` —
 * specifically the `PRESET_NAMES` constant. This TS array must mirror
 * that list exactly; any drift will surface as a parse-time
 * `network must be one of: <list>` error before the browser is ever
 * launched.
 *
 * See discussion #793 phase 3 for the design.
 */
export const NETSIM_PRESETS: readonly string[] = [
  "none",
  "good_wifi",
  "good_4g",
  "congested_wifi",
  "lossy_mobile",
  "satellite",
  "dialup",
] as const;

/**
 * Names accepted on the optional `auth:` field in a meeting config.
 * Mirrors the runtime `AuthBackend` type in `src/auth/storage-state.ts`
 * — kept as a TS-side string list so this module stays free of
 * runtime imports from `bot.ts`.
 */
export const AUTH_BACKEND_NAMES: readonly string[] = ["jwt", "storage-state", "none"] as const;

/**
 * One bot's entry within a meeting config. Currently only carries the
 * participant handle; `ttl` and (future) network-condition overrides
 * are placeholders for phase 3+ work but don't ship per-bot yet so
 * `bot.ts` doesn't need to know about them.
 */
export interface BotEntry {
  participant: string;
  /**
   * Optional per-bot TTL override. When unset the bot inherits the
   * meeting-level `ttl`. Per-bot TTL is mostly useful with
   * `bots-app gen --ttl-jitter` (not yet implemented — `gen` always
   * emits homogeneous TTLs today) but the field is preserved through
   * parse/emit so generators can populate it without a schema change.
   */
  ttl?: string;
  /**
   * Optional per-bot netsim profile (one of `NETSIM_PRESETS`).
   * Overrides the meeting-level default. When set, the bot's meeting
   * URL is rewritten to include `?netsim=<profile>` before navigation
   * — the in-tab `videocall-client` (when built with `--features
   * netsim`) installs the matching shim on the WT + WS send paths.
   * See discussion #793 phase 3.
   */
  network?: string;
  /**
   * Optional per-bot auth backend override (one of
   * `AUTH_BACKEND_NAMES`). Overrides the meeting-level default. When
   * `"none"` the bot launches without any session cookie injected —
   * useful for testing guest-join flows on meetings that accept them.
   */
  auth?: string;
}

/**
 * Shape of a meeting config YAML — the input format `bots-app run
 * --config <path>` consumes and the output format `bots-app gen` emits.
 *
 * Only `bots[]` and `meeting_url` are required at the meeting level. The
 * rest are inherited from CLI flags when missing in the file.
 */
export interface MeetingConfig {
  meetingUrl: string;
  ttl?: string;
  /**
   * Optional meeting-level netsim profile (one of `NETSIM_PRESETS`).
   * Bots that do not specify their own `network` inherit this value.
   * See `BotEntry.network` for the per-bot override.
   */
  network?: string;
  /**
   * Optional meeting-level auth backend (one of `AUTH_BACKEND_NAMES`).
   * Bots that do not specify their own `auth` inherit this value.
   * Most useful as `auth: none` to mark a guest-friendly meeting where
   * no bot needs a real session.
   */
  auth?: string;
  bots: BotEntry[];
  /** Provenance metadata `gen` writes so a generated file can be re-rolled. */
  meta?: {
    seed?: number;
    generatedAt?: string;
  };
}

/**
 * Throws `network must be one of: <list>` when `value` is not in
 * `NETSIM_PRESETS`. `where` is a human-readable prefix injected into
 * the error message (e.g. `"meeting"` or `"bots[2]"`).
 */
function validateNetsimProfile(value: unknown, where: string): string {
  if (typeof value !== "string") {
    throw new Error(`${where}.network, when present, must be a string`);
  }
  if (!NETSIM_PRESETS.includes(value)) {
    throw new Error(
      `${where}.network must be one of: ${NETSIM_PRESETS.join(", ")} (got "${value}")`,
    );
  }
  return value;
}

/**
 * Throws `auth must be one of: <list>` when `value` is not in
 * `AUTH_BACKEND_NAMES`. `where` is a human-readable prefix injected
 * into the error message (e.g. `"meeting"` or `"bots[2]"`).
 */
function validateAuthBackend(value: unknown, where: string): string {
  if (typeof value !== "string") {
    throw new Error(`${where}.auth, when present, must be a string`);
  }
  if (!AUTH_BACKEND_NAMES.includes(value)) {
    throw new Error(
      `${where}.auth must be one of: ${AUTH_BACKEND_NAMES.join(", ")} (got "${value}")`,
    );
  }
  return value;
}

/**
 * Parse a meeting config from YAML text. Throws with a human-readable
 * message on malformed input.
 */
export function parseMeetingConfigText(text: string): MeetingConfig {
  const raw = parseYaml(text);
  if (raw == null || typeof raw !== "object" || Array.isArray(raw)) {
    throw new Error("meeting config is not a YAML mapping");
  }
  const obj = raw as Record<string, unknown>;

  const meetingUrl = obj.meeting_url;
  if (typeof meetingUrl !== "string" || meetingUrl === "") {
    throw new Error("meeting_url must be a non-empty string");
  }
  const ttl = obj.ttl;
  if (ttl !== undefined && typeof ttl !== "string") {
    throw new Error("ttl, when present, must be a string");
  }
  const network =
    obj.network !== undefined ? validateNetsimProfile(obj.network, "meeting") : undefined;
  const auth = obj.auth !== undefined ? validateAuthBackend(obj.auth, "meeting") : undefined;
  if (!Array.isArray(obj.bots)) {
    throw new Error("bots must be an array");
  }
  if (obj.bots.length === 0) {
    throw new Error("bots must not be empty — at least one bot entry is required");
  }
  const bots: BotEntry[] = obj.bots.map((entry: unknown, idx: number) => {
    if (entry == null || typeof entry !== "object" || Array.isArray(entry)) {
      throw new Error(`bots[${idx}] must be a mapping`);
    }
    const row = entry as Record<string, unknown>;
    const participant = row.participant;
    if (typeof participant !== "string" || participant === "") {
      throw new Error(`bots[${idx}].participant must be a non-empty string`);
    }
    const botTtl = row.ttl;
    if (botTtl !== undefined && typeof botTtl !== "string") {
      throw new Error(`bots[${idx}].ttl, when present, must be a string`);
    }
    const botNetwork =
      row.network !== undefined ? validateNetsimProfile(row.network, `bots[${idx}]`) : undefined;
    const botAuth =
      row.auth !== undefined ? validateAuthBackend(row.auth, `bots[${idx}]`) : undefined;
    return { participant, ttl: botTtl, network: botNetwork, auth: botAuth };
  });
  const meta = obj.meta;
  const result: MeetingConfig = { meetingUrl, ttl, network, auth, bots };
  if (meta != null && typeof meta === "object" && !Array.isArray(meta)) {
    const m = meta as Record<string, unknown>;
    result.meta = {
      seed: typeof m.seed === "number" ? m.seed : undefined,
      generatedAt: typeof m.generated_at === "string" ? m.generated_at : undefined,
    };
  }
  return result;
}

/**
 * Load a meeting config from disk.
 */
export function loadMeetingConfig(path: string): MeetingConfig {
  return parseMeetingConfigText(readFileSync(path, "utf8"));
}

/**
 * Render a meeting config back to YAML. Field names use snake_case to
 * match the manifest convention and to read naturally as a config file.
 */
export function emitMeetingConfigYaml(config: MeetingConfig): string {
  const out: Record<string, unknown> = {
    meeting_url: config.meetingUrl,
  };
  if (config.ttl !== undefined) {
    out.ttl = config.ttl;
  }
  if (config.network !== undefined) {
    out.network = config.network;
  }
  if (config.auth !== undefined) {
    out.auth = config.auth;
  }
  out.bots = config.bots.map((b) => {
    const entry: Record<string, unknown> = { participant: b.participant };
    if (b.ttl !== undefined) entry.ttl = b.ttl;
    if (b.network !== undefined) entry.network = b.network;
    if (b.auth !== undefined) entry.auth = b.auth;
    return entry;
  });
  if (config.meta) {
    const meta: Record<string, unknown> = {};
    if (config.meta.seed !== undefined) meta.seed = config.meta.seed;
    if (config.meta.generatedAt !== undefined) meta.generated_at = config.meta.generatedAt;
    if (Object.keys(meta).length > 0) out.meta = meta;
  }
  return stringifyYaml(out);
}

/**
 * Seeded RNG (mulberry32) — deterministic, 32-bit, fine for shuffling a
 * handful of participants. The whole point of the seed is so any bug
 * surfaced by a random-N matrix run can be reproduced via
 * `bots-app gen --seed <S>`; pulling in a heavier RNG (e.g. seedrandom)
 * would not add accuracy at this scale.
 */
export function seededRng(seed: number): () => number {
  let state = seed | 0;
  return () => {
    state = (state + 0x6d2b79f5) | 0;
    let t = Math.imul(state ^ (state >>> 15), 1 | state);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/**
 * Fisher-Yates shuffle backed by the supplied seeded RNG. Returns a new
 * array; does not mutate the input.
 */
export function shuffleSeeded<T>(items: readonly T[], rng: () => number): T[] {
  const arr = [...items];
  for (let i = arr.length - 1; i > 0; i--) {
    const j = Math.floor(rng() * (i + 1));
    [arr[i], arr[j]] = [arr[j], arr[i]];
  }
  return arr;
}

/**
 * Generate a `MeetingConfig` with `count` randomly-shuffled participants
 * picked from the manifest. Deterministic when called with the same
 * `seed`.
 *
 * By default the shuffle pool is **only participants with a costume_dir**
 * — i.e. the 19 named characters that have prep-able y4m + WAV. The
 * unnamed `observer-NN` slots (which exist on the manifest as
 * receive-only seats for the Rust bot) are excluded by default because
 * they have no costume + no lines, so a browser bot rolled into one of
 * those seats would surface as Chrome's default fake pattern with no
 * audio — useless for the passive-mimic case. Pass
 * `includeObservers: true` to include them anyway (useful when you
 * specifically want a meeting with mostly receive-only seats).
 *
 * The parameter space today is just "which participants ride this
 * meeting." Future revisions will randomize per-bot TTL (with the
 * `--ttl-jitter` flag) and, after phase 3, per-bot network profile.
 */
export function generateMeetingConfig(args: {
  manifest: Manifest;
  count: number;
  seed: number;
  meetingUrl: string;
  ttl?: string;
  /**
   * Optional meeting-level netsim profile written to the generated
   * config. Validated against `NETSIM_PRESETS`; throws on an unknown
   * name. Per-bot networks are NOT randomized today — a future
   * `--network-jitter` flag can layer that on without a schema
   * change.
   */
  network?: string;
  includeObservers?: boolean;
}): MeetingConfig {
  if (args.count <= 0) {
    throw new Error("count must be a positive integer");
  }
  if (args.network !== undefined) {
    validateNetsimProfile(args.network, "meeting");
  }
  const eligible = args.includeObservers
    ? args.manifest.participants
    : args.manifest.participants.filter((p) => p.costumeDir);
  if (args.count > eligible.length) {
    const label = args.includeObservers ? "participants" : "costumed participants";
    throw new Error(`count ${args.count} exceeds the manifest's ${eligible.length} ${label}`);
  }
  const rng = seededRng(args.seed);
  const shuffled = shuffleSeeded(
    eligible.map((p) => p.name),
    rng,
  );
  const picked = shuffled.slice(0, args.count);
  return {
    meetingUrl: args.meetingUrl,
    ttl: args.ttl,
    network: args.network,
    bots: picked.map((participant) => ({ participant })),
    meta: {
      seed: args.seed,
      generatedAt: new Date().toISOString(),
    },
  };
}

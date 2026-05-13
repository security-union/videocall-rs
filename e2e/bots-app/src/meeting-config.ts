import { readFileSync } from "node:fs";

import { parse as parseYaml, stringify as stringifyYaml } from "yaml";

import { type Manifest } from "./manifest";

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
  bots: BotEntry[];
  /** Provenance metadata `gen` writes so a generated file can be re-rolled. */
  meta?: {
    seed?: number;
    generatedAt?: string;
  };
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
    return { participant, ttl: botTtl };
  });
  const meta = obj.meta;
  const result: MeetingConfig = { meetingUrl, ttl, bots };
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
  out.bots = config.bots.map((b) => {
    const entry: Record<string, unknown> = { participant: b.participant };
    if (b.ttl !== undefined) entry.ttl = b.ttl;
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
  includeObservers?: boolean;
}): MeetingConfig {
  if (args.count <= 0) {
    throw new Error("count must be a positive integer");
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
    bots: picked.map((participant) => ({ participant })),
    meta: {
      seed: args.seed,
      generatedAt: new Date().toISOString(),
    },
  };
}

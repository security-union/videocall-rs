import type { Resolved } from "./run-record";

export const SPEECH_ENV = {
  model: "BOT_SPEECH_MODEL",
  seed: "BOT_SPEECH_SEED",
  dominant: "BOT_SPEECH_DOMINANT",
  observers: "BOT_SPEECH_OBSERVERS",
  keep: "BOT_SPEECH_KEEP",
  overlap: "BOT_SPEECH_OVERLAP",
} as const;

export const SPEECH_ENV_NAMES = Object.values(SPEECH_ENV);

export interface WeightedSpeechPlan {
  readonly model: "weighted";
  readonly seed: number;
  /** Talking shares of the 1-2 dominant talkers; the other talkers split the remainder. */
  readonly dominant: readonly number[];
  readonly observers: number;
  /** Probability the speaker keeps the floor at a block boundary. */
  readonly keep: number;
  /** Target fraction of speaker changes whose new speaker starts early (clock-speech.js `MAX_LEAD_MS`). */
  readonly overlap: number;
}

export type SpeechPlan = { readonly model: "rotation" } | WeightedSpeechPlan;

export const ROTATION_PLAN: SpeechPlan = { model: "rotation" };

export const SPEECH_PLAN_DEFAULT: WeightedSpeechPlan = {
  model: "weighted",
  seed: 0,
  dominant: [0.35, 0.2],
  observers: 0,
  keep: 0.75,
  overlap: 0.3,
};

const DECIMAL = /^(?:\d(?:\.\d{1,4})?|\.\d{1,4})$/;

function fraction(name: string, raw: string, min: number, max: number, maxIncl: boolean) {
  const n = DECIMAL.test(raw) ? Number(raw) : Number.NaN;
  const ok = n >= min && (maxIncl ? n <= max : n < max);
  const range = `${min}..${max}${maxIncl ? "" : " (exclusive)"}`;
  return ok ? n : `${name} must be a decimal in ${range} with at most 4 places, got "${raw}"`;
}

export function resolveSpeechPlan(env: Record<string, string | undefined>): Resolved<SpeechPlan> {
  const raw = Object.fromEntries(
    Object.entries(SPEECH_ENV).map(([k, name]) => [k, (env[name] ?? "").trim()]),
  ) as Record<keyof typeof SPEECH_ENV, string>;
  const invalid = (message: string): Resolved<SpeechPlan> => ({ kind: "invalid", message });
  const model = raw.model.toLowerCase() || SPEECH_PLAN_DEFAULT.model;
  if (model === "rotation") {
    const set = (Object.keys(raw) as (keyof typeof SPEECH_ENV)[])
      .filter((k) => k !== "model" && raw[k] !== "")
      .map((k) => SPEECH_ENV[k]);
    if (set.length > 0)
      return invalid(`${set.join(", ")} has no effect when ${SPEECH_ENV.model}=rotation; unset it`);
    return { kind: "ok", value: ROTATION_PLAN };
  }
  if (model !== "weighted") {
    return invalid(`${SPEECH_ENV.model} must be "weighted" or "rotation", got "${raw.model}"`);
  }
  const d = SPEECH_PLAN_DEFAULT;
  let seed = d.seed;
  if (raw.seed !== "") {
    seed = /^\d{1,10}$/.test(raw.seed) ? Number(raw.seed) : Number.NaN;
    if (!(seed <= 0xffffffff)) {
      return invalid(`${SPEECH_ENV.seed} must be an integer in 0..4294967295, got "${raw.seed}"`);
    }
  }
  let dominant: number[] = [...d.dominant];
  if (raw.dominant !== "") {
    const tokens = raw.dominant.split(",").map((t) => t.trim());
    if (tokens.length > 2)
      return invalid(`${SPEECH_ENV.dominant} takes 1 or 2 shares, got "${raw.dominant}"`);
    dominant = [];
    for (const t of tokens) {
      const share = fraction(SPEECH_ENV.dominant, t, 0, 1, false);
      if (typeof share === "string") return invalid(share);
      if (share === 0) return invalid(`${SPEECH_ENV.dominant} shares must be above 0, got "${t}"`);
      dominant.push(share);
    }
    if (dominant.reduce((a, b) => a + b, 0) >= 1) {
      return invalid(`${SPEECH_ENV.dominant} shares must sum below 1, got "${raw.dominant}"`);
    }
  }
  let observers = d.observers;
  if (raw.observers !== "") {
    if (!/^\d{1,4}$/.test(raw.observers)) {
      return invalid(
        `${SPEECH_ENV.observers} must be an integer in 0..9999, got "${raw.observers}"`,
      );
    }
    observers = Number(raw.observers);
  }
  const keep = raw.keep === "" ? d.keep : fraction(SPEECH_ENV.keep, raw.keep, 0, 1, false);
  if (typeof keep === "string") return invalid(keep);
  const overlap =
    raw.overlap === "" ? d.overlap : fraction(SPEECH_ENV.overlap, raw.overlap, 0, 1, true);
  if (typeof overlap === "string") return invalid(overlap);
  return { kind: "ok", value: { model: "weighted", seed, dominant, observers, keep, overlap } };
}

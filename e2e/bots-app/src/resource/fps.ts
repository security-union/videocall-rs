/**
 * Per-bot capture/encode FPS capture for the bots-app output (issue 2032,
 * rider b).
 *
 * bot.ts polls the client's `window.__videocall_encoder_fps` global, coerces
 * each raw value here, and feeds readings or explicit no-data signals into the
 * per-bot tracker.
 */

/**
 * Coerce a raw value read from the client's `window.__videocall_encoder_fps`
 * global (issue #2062 / #2057) into a usable fps reading.
 *
 * Once the client-side publisher ships (#2057, not yet merged), videocall-client
 * `health_reporter` sets a POSITIVE number when the camera encoder is active and
 * has produced a real sample, and clears the property (`undefined`) when the
 * camera is off / warming up / on teardown. Until #2057 is deployed the global
 * is absent, so every read coerces to `null` (no data) and the fps rule stays
 * dormant (CPU-only verdict) — no false readings in the interim. We treat:
 *   - a finite number `> 0`  → a real reading (recorded; a low value like 1–4
 *                              is the partial-starvation signal the verdict
 *                              targets — see resource/verdict.ts).
 *   - `0`                    → "no data", NOT a starvation reading. This mirrors
 *                              the client's OWN convention: health_reporter.rs
 *                              gates the metric on `encoder_output_fps > 0`
 *                              because 0 means "the encoder hasn't started yet,
 *                              which isn't diagnostic". The window value alone
 *                              cannot distinguish not-started from active-stall,
 *                              so recording 0 would false-flag a cold-start bot
 *                              as `RESOURCE_STARVED` (min 0 < base rung). A
 *                              genuine total stall is backstopped by the CPU
 *                              rule; since #2060 the client emits 0 on a stall,
 *                              but this gate still maps 0 -> no-data (see the
 *                              #2060 COORDINATION note below).
 *   - anything else (`undefined`/absent, `null`, `NaN`, `Infinity`, negative,
 *                    non-number) → `null` = "no data".
 * Returning `null` (not `0`) for absent/zero data is what stops a cold-start /
 * idle bot from being mis-recorded as starved while still allowing the tracker
 * to reset a sustained-low run.
 *
 * #2060 COORDINATION: #2060 HAS LANDED — the camera_encoder now resets
 * current_fps to 0 on stop/start and decays it to 0 after a sustained layer-0
 * output gap, so the client (#2057) now DOES publish a literal `0` on a total
 * stall (health_reporter `encoder_fps_publish_value(true, 0, true) == Some(0)`).
 * This gate STILL maps that `0` to no-data on purpose: it cannot yet distinguish
 * "encoder total-stall (0)" from "no sample this poll", and treating every 0 as
 * starvation would false-flag the sub-1s re-enable/warmup window. Flagging a
 * total stall AS `RESOURCE_STARVED` — accepting `0` as a stall reading, e.g. via
 * an explicit stall signal distinct from no-data — is the remaining follow-up
 * (revisit this `> 0` gate). Until then, `0` stays not-diagnostic here.
 */
export function coerceEncoderFps(raw: unknown): number | null {
  if (typeof raw !== "number" || !Number.isFinite(raw) || raw <= 0) return null;
  return raw;
}

/**
 * Max wall-clock gap (ms) between two sub-rung readings for them to count as the
 * SAME sustained run (#2064). ~3x the bots' ~2s encoder-fps poll
 * (bot.ts ENCODER_FPS_POLL_MS): tolerates one or two jittery polls or a stalled
 * poll loop. Explicit no-data readings reset the run immediately; this elapsed
 * gap remains the backstop when `record` is not called at all.
 */
export const FPS_RUN_MAX_GAP_MS = 6000;

/** Aggregated FPS statistics for one bot. */
export interface FpsStats {
  /** Most recent reading. */
  latest: number;
  /** Lowest reading seen (the worst). */
  min: number;
  /** Arithmetic mean of all readings. */
  mean: number;
  /** Number of readings recorded. */
  count: number;
  /**
   * Longest SUSTAINED sub-rung stretch, in milliseconds of wall-clock (#2064).
   * TIME-based (not a poll-read count) so it is independent of the poll rate:
   * the bots poll the persisted global faster than the client republishes it,
   * so counting reads would let one transient publication (read 2-3x) look
   * "sustained". A duration cannot be inflated by re-reading a held value.
   */
  maxSustainedBelowRungMs: number;
}

/**
 * Tracks per-bot FPS readings over a run. `record` is fed a parsed reading or
 * an explicit `null` no-data signal for a bot; `snapshot` returns the per-bot
 * aggregates the summary + verdict consume. Pure in-memory accumulation — no
 * I/O, so it is trivially testable and cheap to update from the polling path.
 */
export class FpsTracker {
  private readonly byBot = new Map<
    string,
    {
      latest: number;
      min: number;
      sum: number;
      count: number;
      runStartMs: number | null;
      runLastMs: number;
      maxRunMs: number;
    }
  >();

  /**
   * @param belowRungThreshold readings STRICTLY below this extend the sustained
   *   sub-rung run (#2064). Callers pass the verdict's `RESOURCE_FPS_BASE_RUNG`
   *   so the tracker and verdict agree on the floor.
   * @param now injectable monotonic-ish clock in ms. Defaults to `Date.now`;
   *   tests inject a fake so the duration logic is deterministic.
   */
  constructor(
    private readonly belowRungThreshold: number,
    private readonly now: () => number = () => Date.now(),
  ) {}

  record(botId: string, fps: number | null): void {
    if (fps === null) {
      const cur = this.byBot.get(botId);
      if (cur !== undefined) cur.runStartMs = null;
      return;
    }
    if (!Number.isFinite(fps) || fps < 0) return;
    const t = this.now();
    const below = fps < this.belowRungThreshold;
    const cur = this.byBot.get(botId);
    if (cur === undefined) {
      this.byBot.set(botId, {
        latest: fps,
        min: fps,
        sum: fps,
        count: 1,
        runStartMs: below ? t : null,
        runLastMs: t,
        maxRunMs: 0,
      });
      return;
    }
    cur.latest = fps;
    cur.min = Math.min(cur.min, fps);
    cur.sum += fps;
    cur.count += 1;
    if (below) {
      // Continue the active run only if this reading is within MAX_GAP of the
      // previous one; otherwise readings were dropped (a no-data gap) so restart.
      const continues = cur.runStartMs !== null && t - cur.runLastMs <= FPS_RUN_MAX_GAP_MS;
      if (!continues) cur.runStartMs = t;
      const runStartMs = cur.runStartMs ?? t;
      cur.maxRunMs = Math.max(cur.maxRunMs, t - runStartMs);
    } else {
      cur.runStartMs = null;
    }
    cur.runLastMs = t;
  }

  /** Whether any reading has been recorded for any bot. */
  get hasData(): boolean {
    return this.byBot.size > 0;
  }

  snapshot(): Map<string, FpsStats> {
    const out = new Map<string, FpsStats>();
    for (const [botId, s] of this.byBot) {
      out.set(botId, {
        latest: s.latest,
        min: s.min,
        mean: s.count > 0 ? s.sum / s.count : 0,
        count: s.count,
        maxSustainedBelowRungMs: s.maxRunMs,
      });
    }
    return out;
  }
}

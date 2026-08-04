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
 * videocall-client `health_reporter` (#2057, SHIPPED) publishes the camera's
 * layer-0 output fps once the encoder is active AND has produced at least one
 * real sample, and CLEARS the property (`undefined`) when the camera is off /
 * warming up / on teardown. We treat:
 *   - a finite number `> 0`  → a real reading (recorded; a low value like 1–4
 *                              is the partial-starvation signal the verdict
 *                              targets — see resource/verdict.ts).
 *   - `0`                    → "no data" here, NOT a starvation reading.
 *   - anything else (`undefined`/absent, `null`, `NaN`, `Infinity`, negative,
 *                    non-number) → `null` = "no data".
 *
 * Why `0` is DISCARDED even though it is now meaningful (#2079, OPEN).
 * Do not read this gate as "the client publishes 0 only when it has no data" —
 * that was true before #2060 and is FALSE now. Since #2060 the producer resets
 * `current_fps` to 0 on stop/start and decays it to 0 after a sustained layer-0
 * output gap, so `Some(0)` IS published on a genuine total stall
 * (`encoder_fps_publish_value(true, 0, true) == Some(0)`). A `0` arriving here
 * therefore carries real information that this function throws away, and the
 * verdict's CPU rule is the only backstop for a total stall.
 *
 * It is not simply flipped to accept `0` because the reachable failure modes of
 * doing so are worse than the gap. `0` conflates a starved BOX (the verdict's
 * subject) with a WEDGED ENCODER — a product bug that must not be relabelled as a
 * confounded harness run. Attempts to separate them from fps VALUES alone each
 * failed on a reachable path (all four verified against this code, see #2079):
 *   1. accept `0` outright               → a wedged encoder (pure zeros, healthy
 *                                          CPU) flags RESOURCE_STARVED
 *   2. require a nonzero low in the run  → a LATER, longer pure-zero run erased an
 *                                          earlier genuinely-starved run's verdict
 *   3. track the eligible run separately → one low then N zeros reports N poll
 *                                          intervals of "sustained" starvation
 *                                          from a single poll of real evidence
 *   4. tighten the run gap               → trades the false positive for a false
 *                                          negative under genuine load
 * The conclusion recorded on #2079 is that the ambiguity belongs at the SOURCE: a
 * stall signal distinct from no-data, published by the client, rather than a
 * heuristic in this consumer. Until that lands, `0` stays discarded HERE — which
 * is a known gap, not a claim that `0` is meaningless.
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

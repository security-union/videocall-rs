import { describe, expect, it } from "vitest";

import { FpsTracker, FPS_RUN_MAX_GAP_MS, coerceEncoderFps } from "./fps";
import { RESOURCE_FPS_BASE_RUNG } from "./verdict";

describe("coerceEncoderFps", () => {
  it("accepts a finite positive number (incl. the 1-4 partial-starvation range)", () => {
    expect(coerceEncoderFps(4)).toBe(4);
    expect(coerceEncoderFps(3.2)).toBe(3.2);
    expect(coerceEncoderFps(1)).toBe(1);
    expect(coerceEncoderFps(30)).toBe(30);
  });

  it("treats 0 as no-data (null), NOT a starvation reading", () => {
    // Mirrors the client's own convention (health_reporter.rs: encoder_output_fps
    // > 0), where 0 means "encoder not started, not diagnostic". Recording 0
    // would false-flag a cold-start bot as starved (min 0 < base rung).
    expect(coerceEncoderFps(0)).toBeNull();
  });

  it("treats absent/undefined/null as no-data (null)", () => {
    expect(coerceEncoderFps(undefined)).toBeNull();
    expect(coerceEncoderFps(null)).toBeNull();
  });

  it("rejects non-finite, negative, and non-number values", () => {
    expect(coerceEncoderFps(NaN)).toBeNull();
    expect(coerceEncoderFps(Infinity)).toBeNull();
    expect(coerceEncoderFps(-4)).toBeNull();
    expect(coerceEncoderFps("8")).toBeNull();
    expect(coerceEncoderFps({})).toBeNull();
  });
});

describe("FpsTracker", () => {
  it("tracks min / mean / latest / count and the sustained sub-rung DURATION", () => {
    let t = 0;
    const tr = new FpsTracker(RESOURCE_FPS_BASE_RUNG, () => t);
    t = 0;
    tr.record("a", 8);
    t = 2000;
    tr.record("a", 2);
    t = 4000;
    tr.record("a", 2);
    t = 6000;
    tr.record("a", 5);
    tr.record("b", 30);
    const a = tr.snapshot().get("a");
    expect(a).toEqual({
      latest: 5,
      min: 2,
      mean: 4.25,
      count: 4,
      maxSustainedBelowRungMs: 2000,
    });
    expect(tr.snapshot().get("b")).toEqual({
      latest: 30,
      min: 30,
      mean: 30,
      count: 1,
      maxSustainedBelowRungMs: 0,
    });
  });

  it("does NOT reach sustain from one held publication read several times (poll oversampling)", () => {
    // Client holds one sub-rung value for a ~5s publication; the 2s poll reads
    // it 3x. That is ~4s of run, NOT a sustained stretch.
    let t = 0;
    const tr = new FpsTracker(RESOURCE_FPS_BASE_RUNG, () => t);
    for (const at of [0, 2000, 4000]) {
      t = at;
      tr.record("a", 2);
    }
    expect(tr.snapshot().get("a")?.maxSustainedBelowRungMs).toBe(4000);
  });

  it("breaks the run across a no-data gap (does not stitch isolated lows)", () => {
    let t = 0;
    const tr = new FpsTracker(RESOURCE_FPS_BASE_RUNG, () => t);
    t = 0;
    tr.record("a", 2);
    t = 2000;
    tr.record("a", 2);
    // ...camera off: the poll skips record for a long time...
    t = 2000 + FPS_RUN_MAX_GAP_MS + 4000;
    tr.record("a", 2);
    t = t + 2000;
    tr.record("a", 2);
    // Max is 2000, NOT the ~14s span from the first low to the last.
    expect(tr.snapshot().get("a")?.maxSustainedBelowRungMs).toBe(2000);
  });

  it("resets the run on an explicit null no-data signal", () => {
    let t = 0;
    const tr = new FpsTracker(RESOURCE_FPS_BASE_RUNG, () => t);
    tr.record("a", 2);
    t = 2000;
    tr.record("a", 2);
    t = 3000;
    tr.record("a", null);
    t = 4000;
    tr.record("a", 2);
    t = 6000;
    tr.record("a", 2);

    expect(tr.snapshot().get("a")).toEqual({
      latest: 2,
      min: 2,
      mean: 2,
      count: 4,
      maxSustainedBelowRungMs: 2000,
    });
  });

  it("ignores non-finite / negative readings", () => {
    const tr = new FpsTracker(RESOURCE_FPS_BASE_RUNG, () => 0);
    tr.record("a", Number.NaN);
    tr.record("a", -1);
    expect(tr.hasData).toBe(false);
  });
});

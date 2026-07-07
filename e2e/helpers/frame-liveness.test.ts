import { describe, it, expect } from "vitest";

import { longestFrozenRunMs, type ChecksumSample } from "./frame-liveness";

/**
 * Build a checksum series at a fixed cadence from a list of values. A value of
 * `null` becomes an unsampleable instant; a string is the concrete checksum.
 * The first sample is at `atMs = 0`.
 */
function seriesAt(intervalMs: number, values: (string | null)[]): ChecksumSample[] {
  return values.map((checksum, i) => ({ atMs: i * intervalMs, checksum }));
}

describe("longestFrozenRunMs", () => {
  it("bridges a single null in the middle of a sustained freeze (issue #1712)", () => {
    // X at 0..4000 (every 400ms), one null at 4400, then X again at 4800..9600.
    // Span exceeds MAX_FROZEN_RUN_MS (8000ms). With a hard-reset on null this
    // would report only max(4000, 4800) = 4800ms — a false-green vector. The
    // hardened helper must report the FULL first-match→last-match span (9600ms).
    const series: ChecksumSample[] = [];
    for (let t = 0; t <= 4000; t += 400) {
      series.push({ atMs: t, checksum: "X" });
    }
    series.push({ atMs: 4400, checksum: null });
    for (let t = 4800; t <= 9600; t += 400) {
      series.push({ atMs: t, checksum: "X" });
    }

    expect(longestFrozenRunMs(series)).toBe(9600);
    // And it must clear the 8000ms ceiling it would have under-reported below.
    expect(longestFrozenRunMs(series)).toBeGreaterThanOrEqual(8000);
  });

  it("does NOT fabricate a freeze across a null gap longer than the bridge budget", () => {
    // X at 0,400, then four consecutive nulls (800,1200,1600,2000 — a 1600ms
    // gap from the last match at 400, exceeding the 1000ms default), then X at
    // 2400. The post-gap X must start a FRESH run, so only the 400ms pre-gap
    // fragment is reported — a long absence is not evidence of a held frame.
    const series = seriesAt(400, ["X", "X", null, null, null, null, "X"]);

    expect(longestFrozenRunMs(series)).toBe(400);
  });

  it("returns ~0 for a healthy tile painting all-distinct frames", () => {
    // Every sample a different checksum: a live tile repaints each frame, so
    // there is never an identical back-to-back pair — no run accrues.
    const series = seriesAt(400, ["A", "B", "C", "D", "E"]);

    expect(longestFrozenRunMs(series)).toBe(0);
  });

  it("returns the full span for a fully-frozen run with no nulls (base case)", () => {
    // X repeated every 400ms from 0 to 4000 — first-match→last-match = 4000ms.
    const series = seriesAt(400, ["X", "X", "X", "X", "X", "X", "X", "X", "X", "X", "X"]);

    expect(longestFrozenRunMs(series)).toBe(4000);
  });

  it("honours a custom maxBridgeNullMs that is wide enough to bridge a long gap", () => {
    // Same series as the long-gap case (1600ms gap), but with an explicit
    // 2000ms budget the gap IS bridged, so the run continues across it and the
    // full first-match→last-match span (2400ms) is reported.
    const series = seriesAt(400, ["X", "X", null, null, null, null, "X"]);

    expect(longestFrozenRunMs(series, 2000)).toBe(2400);
    // Sanity: the default budget (1000ms) still rejects this gap.
    expect(longestFrozenRunMs(series)).toBe(400);
  });

  it("starts a fresh run when a different checksum appears after a bridged null", () => {
    // X@0,X@400, null@800 (bridged, gap 400 ≤ 1000), then Y@1200 — a real
    // repaint. The X run ends at 400ms (the last X match), and Y anchors a new
    // run; with no further Y the longest remains 400ms.
    const series = seriesAt(400, ["X", "X", null, "Y", "Y", "Y"]);

    // X run = 400ms (0→400); Y run = 800ms (1200→2000). Longest is the Y run.
    expect(longestFrozenRunMs(series)).toBe(800);
  });

  it("does NOT fabricate a freeze when a matching sample arrives over-budget after a bridged null", () => {
    // X@0, a single null@400 (gap 400 ≤ 1000, so the null itself is bridged),
    // then the next matching X arrives at 2000 — a 2000ms gap since the last
    // concrete match at 0, which EXCEEDS the 1000ms default. We have no evidence
    // the frame was held across that unsampled stretch, so X@2000 must re-anchor
    // a FRESH run rather than extend; with no later match the longest stays 0.
    // A naive "extend on any match" impl would report the full 2000ms — this is
    // the realistic CI-sampling-stall over-report the budget guard must reject.
    const series: ChecksumSample[] = [
      { atMs: 0, checksum: "X" },
      { atMs: 400, checksum: null },
      { atMs: 2000, checksum: "X" },
    ];

    expect(longestFrozenRunMs(series)).toBe(0);
  });

  it("bridges a delayed matching sample that is within the budget", () => {
    // X@0, null@400, X@800 — the matching X arrives 800ms after the last concrete
    // match (≤ 1000ms budget), so the run continues across the gap and the full
    // first-match→last-match span (800ms) is reported.
    const series: ChecksumSample[] = [
      { atMs: 0, checksum: "X" },
      { atMs: 400, checksum: null },
      { atMs: 800, checksum: "X" },
    ];

    expect(longestFrozenRunMs(series)).toBe(800);
  });

  it("treats a gap exactly equal to the budget as still bridging (inclusive boundary)", () => {
    // X@0, X@1000 — gap 1000 since last match is EQUAL to the default budget, so
    // the inclusive (`<=`) boundary bridges it and the run extends to 1000ms.
    const series: ChecksumSample[] = [
      { atMs: 0, checksum: "X" },
      { atMs: 1000, checksum: "X" },
    ];

    expect(longestFrozenRunMs(series)).toBe(1000);
  });

  it("ignores leading nulls before any run is anchored", () => {
    // Nulls before the first concrete checksum contribute nothing; the run
    // starts only once X first appears.
    const series = seriesAt(400, [null, null, "X", "X", "X"]);

    // X first at 800, last at 1600 → 800ms.
    expect(longestFrozenRunMs(series)).toBe(800);
  });
});

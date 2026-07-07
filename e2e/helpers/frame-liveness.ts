/**
 * E2E helper: per-tile frame-liveness checksum.
 *
 * ## Provenance (NOT a new primitive — this is the existing one, factored out)
 *
 * The `getImageData` pixel-checksum pattern this file exposes ALREADY exists in
 * `e2e/tests/wt-persistent-streams-freeze-regression.spec.ts`
 * (`samplePeerVideoChecksum`, ~line 308-340 of that spec, the `ctx.getImageData`
 * read at its line 325). That spec samples a 32x32 centre patch of the first
 * `#grid-container .canvas-container canvas`, sums every 17th byte, and asserts
 * the checksum CHANGES between two samples 40s apart — identical buffers prove a
 * frozen tile. This module lifts that exact logic into a reusable helper so a
 * second freeze spec (issue #1702, the #1695 pin/enlarge ≤5s decode-guard
 * freeze) can REUSE it rather than duplicate it.
 *
 * Two additions over the original (both backward-compatible refinements, not a
 * new mechanism):
 *   1. `nth` — sample a SPECIFIC tile by index (the original always took the
 *      first). The #1702 spec pins/enlarges a peer tile, so it must sample THAT
 *      tile, which after pinning becomes `.grid-item-pinned` (full-screen) — but
 *      its `<canvas>` node is REUSED in place (Dioxus diffs the tile by template
 *      identity; see canvas_generator.rs issue #508 note), so the first
 *      `.canvas-container canvas` remains the pinned peer's canvas in a 2-peer
 *      call where the only remote tile is the publisher. We keep `nth=0` as the
 *      default to match the original behaviour.
 *   2. `sampleChecksumSeries` — sample the SAME tile repeatedly at a fine cadence
 *      and return the ordered series, so a caller can detect a SHORT (≤5s)
 *      transient freeze (a run of identical samples) rather than only a sustained
 *      one. The original spec's two-point t=20s/t=60s sample is too coarse for a
 *      sub-5s self-healing freeze.
 */

import { Page } from "@playwright/test";

/**
 * Sample a peer-video `<canvas>` inside `#grid-container` and return a short
 * pixel-checksum string, or `null` if no usable canvas tile is present yet.
 *
 * Reads a 32x32 centre patch and sums every 17th byte — the SAME cheap checksum
 * as `wt-persistent-streams-freeze-regression.spec.ts:325`. Identical strings
 * across samples ⇒ the tile painted no new frame ⇒ frozen.
 *
 * @param page The page whose grid is sampled.
 * @param nth  Zero-based index of the canvas-container tile to sample (default 0,
 *             matching the original first-tile behaviour). Containers with a
 *             zero-sized canvas are skipped, so `nth` counts only renderable
 *             tiles.
 */
export async function samplePeerVideoChecksum(page: Page, nth = 0): Promise<string | null> {
  return page.evaluate((index) => {
    const containers = document.querySelectorAll("#grid-container .canvas-container");
    let seen = 0;
    for (const container of Array.from(containers)) {
      const canvas = container.querySelector("canvas") as HTMLCanvasElement | null;
      if (!canvas || canvas.width === 0 || canvas.height === 0) {
        continue;
      }
      if (seen < index) {
        seen += 1;
        continue;
      }
      const ctx = canvas.getContext("2d");
      if (!ctx) {
        return null;
      }
      const w = Math.min(32, canvas.width);
      const h = Math.min(32, canvas.height);
      const x = Math.max(0, Math.floor((canvas.width - w) / 2));
      const y = Math.max(0, Math.floor((canvas.height - h) / 2));
      try {
        const data = ctx.getImageData(x, y, w, h).data;
        // Cheap checksum: sum every 17th byte. Sufficient to detect a changing
        // image vs a frozen tile without doing a full hash.
        let sum = 0;
        for (let i = 0; i < data.length; i += 17) {
          sum = (sum + data[i]) >>> 0;
        }
        return `${canvas.width}x${canvas.height}:${sum}`;
      } catch {
        // SecurityError on a tainted canvas — should not happen for our own
        // decoded media; treat as unsampleable.
        return null;
      }
    }
    return null;
  }, nth);
}

/** One sample in a liveness series: the wall-clock offset (ms from series start) and the checksum. */
export interface ChecksumSample {
  /** ms since the first sample in the series. */
  atMs: number;
  /** The pixel checksum string, or `null` if the tile was unsampleable at that instant. */
  checksum: string | null;
}

/**
 * Sample one tile's checksum repeatedly at `intervalMs` cadence for `durationMs`,
 * returning the ordered series. Used to catch a SHORT (≤5s) transient freeze: a
 * run of identical (non-null) checksums spanning a freeze window means the tile
 * stopped painting new frames for that long.
 *
 * @param page       The page whose grid is sampled.
 * @param durationMs Total sampling window (e.g. ~5500ms to cover a ≤5s freeze
 *                   plus headroom).
 * @param intervalMs Cadence between samples (e.g. 300-500ms — fine enough that a
 *                   ≤5s freeze yields ≥10 identical samples).
 * @param nth        Tile index (see {@link samplePeerVideoChecksum}).
 */
export async function sampleChecksumSeries(
  page: Page,
  durationMs: number,
  intervalMs: number,
  nth = 0,
): Promise<ChecksumSample[]> {
  const series: ChecksumSample[] = [];
  const start = Date.now();
  // Inclusive of t=0 and bounded by the wall-clock window so CI jitter in
  // `waitForTimeout` cannot run the loop long.
  while (Date.now() - start <= durationMs) {
    const atMs = Date.now() - start;
    const checksum = await samplePeerVideoChecksum(page, nth);
    series.push({ atMs, checksum });
    await page.waitForTimeout(intervalMs);
  }
  return series;
}

/**
 * Given a checksum series, return the longest run (in ms) of IDENTICAL, NON-NULL
 * checksums — i.e. the longest window the tile painted no new frame — tolerating
 * SHORT `null` gaps inside an otherwise-sustained run.
 *
 * A run is anchored by a concrete (non-null) checksum and measured from its
 * FIRST matching sample to its LAST matching sample (so a single isolated sample
 * contributes 0ms). A `null` sample neither extends nor immediately breaks an
 * open run: it is BRIDGED as long as the elapsed time since the last matching
 * non-null sample stays within `maxBridgeNullMs`. A later non-null sample with
 * the SAME checksum then CONTINUES the run across the gap, so a single dropped
 * frame in the middle of a freeze no longer under-reports its length. A
 * genuinely DIFFERENT non-null checksum still starts a fresh run (a real
 * repaint), and a `null` before any run is anchored contributes nothing.
 *
 * The `maxBridgeNullMs` guard prevents fabricating a freeze from a genuine long
 * absence: a continuation is only credited when the gap since the last concrete
 * match is within budget. Past budget — whether the gap manifests as `null`
 * samples, as no samples at all (a sampling stall), or both — the next matching
 * sample re-anchors a FRESH run instead of extending, because a long
 * unsampled/absent interval is NOT evidence the tile was still holding the same
 * frame. Existing callers pass a single argument and keep the 1000ms default.
 *
 * This is the freeze metric: a healthy synthetic-camera tile repaints every
 * frame, so identical-checksum runs are at most one cadence long; a frozen tile
 * yields a long run, even if one or two samples in the middle were unsampleable.
 *
 * @param series          The ordered checksum series.
 * @param maxBridgeNullMs Max elapsed time (ms) since the last matching non-null
 *                        sample within which a later matching sample still
 *                        CONTINUES the run. Past this budget the next match
 *                        re-anchors a fresh run instead. Default 1000ms.
 */
export function longestFrozenRunMs(series: ChecksumSample[], maxBridgeNullMs = 1000): number {
  let longest = 0;
  let runStartMs: number | null = null;
  let runValue: string | null = null;
  // atMs of the last non-null sample equal to `runValue` — the anchor we measure
  // the bridge gap from.
  let lastMatchMs: number | null = null;
  for (const s of series) {
    if (s.checksum === null) {
      // A null neither extends nor breaks the run on its own — it is just an
      // unsampleable instant. Whether the resulting gap is tolerable is decided
      // when (and if) the next concrete sample arrives, by the bridge-budget
      // check in the matching branch below. This keeps the budget the single
      // source of truth and correctly handles a gap that manifests as no
      // samples at all (a sampling stall) rather than as interspersed nulls.
      continue;
    }
    if (s.checksum === runValue) {
      // Same frame. Treat it as a CONTINUATION of the open run only if the gap
      // since the last concrete match is within the bridge budget — otherwise
      // an over-budget unsampled/absent interval (a CI sampling stall or a long
      // null gap) would fabricate a freeze across time we have no evidence the
      // frame was held. Past budget, this matching sample re-anchors a FRESH
      // run instead of extending. The boundary is inclusive: a gap exactly
      // equal to the budget still bridges.
      if (runStartMs !== null && lastMatchMs !== null && s.atMs - lastMatchMs <= maxBridgeNullMs) {
        longest = Math.max(longest, s.atMs - runStartMs);
        lastMatchMs = s.atMs;
      } else {
        runStartMs = s.atMs;
        lastMatchMs = s.atMs;
      }
    } else {
      // A different concrete checksum — a real repaint starts a fresh run here.
      runValue = s.checksum;
      runStartMs = s.atMs;
      lastMatchMs = s.atMs;
    }
  }
  return longest;
}

/**
 * Count the DISTINCT non-null checksums among the samples whose `atMs` falls in
 * `[fromMs, toMs)`. A value > 1 means the tile painted CHANGING frames inside
 * that window — it is live in that interval. Used to prove RECOVERY: that after
 * a pin-driven layer up-switch (which legitimately holds the last frame for up
 * to one publisher GOP — ~5s on camera — while the newly-requested layer's
 * keyframe arrives), the tile resumes painting by the END of a longer window. A
 * tile still frozen at the end (a sustained regression, not the benign keyframe
 * wait) yields ≤1 distinct checksum in the tail window.
 */
export function distinctChecksumsInWindow(
  series: ChecksumSample[],
  fromMs: number,
  toMs: number,
): number {
  return new Set(
    series
      .filter((s) => s.atMs >= fromMs && s.atMs < toMs && s.checksum !== null)
      .map((s) => s.checksum),
  ).size;
}

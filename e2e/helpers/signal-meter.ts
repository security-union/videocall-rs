// Signal-disc selectors (2661). Never select on the accessible name. PEER selectors
// need `enableDiagnosticsTileIndicators` seeded; SELF ones resolve unseeded (exempt).

import type { Locator } from "@playwright/test";

/** Hand-copied from `SPARK_MIN_POINTS` (`dioxus-ui/src/components/signal_quality.rs`), and
 *  nothing catches drift: it is absent from `RUST_MIRRORS`, and `pub(crate)` misses that
 *  lock's `^(?:pub )?const` regex — so locking it means widening it in the same change. */
export const SPARK_MIN_POINTS = 4;

export const SELF_SIGNAL_DISC = '[data-testid="self-signal-indicator"]';

/** ANY peer disc, camera and screen alike. Tile-scoped queries only. */
export const PEER_SIGNAL_DISC = '[data-testid="peer-signal-indicator"]';

/**
 * USE THIS FOR ANY PAGE-LEVEL QUERY: the screen-share disc shares the testid but
 * opens a different popup, so a bare `.first()` drives the wrong surface under
 * an active share. Excludes on the `:screen` suffix `refresh_peer_disc` writes.
 */
export const CAMERA_PEER_SIGNAL_DISC = `${PEER_SIGNAL_DISC}:not(:has([data-signal-spark$=":screen"]))`;

export const SCREEN_SIGNAL_DISC = `${PEER_SIGNAL_DISC}:has([data-signal-spark$=":screen"])`;

/** The sparkline mount inside a disc. Disc- or tile-scoped queries only. */
export const SIGNAL_SPARK = "[data-signal-spark]";

export const SELF_SIGNAL_SPARK = '[data-signal-spark="self"]';

/** A bare `polyline` also matches the keyline: doubles counts, `.first()` is black. */
export const SPARK_TREND = "polyline:not(.spark-halo)";

export const SPARK_KEYLINE = "polyline.spark-halo";

/** DISC-scoped, count 0. Generic: covers a re-added dot or ring either way. */
export const SPARK_REMOVED_CIRCLES = "circle";

/** `spark_y(0.5)`. NOTHING is drawn here since #2661; y grows DOWNWARD. */
export const SPARK_WARN_Y = 8;

/** SPARK-scoped, count 0. Generic: a re-added area fill trips it whatever class it wears. */
export const SPARK_REMOVED_AREA = "polygon";

/** `SPARK_X_LAST` as `build_spark_points` writes it: the newest sample's x. */
export const SPARK_X_NEWEST = "22.60";

/** Every plotted y, oldest first — runs are oldest-first, so the LAST is newest. */
export async function plottedTrendYs(spark: Locator): Promise<number[]> {
  const runs = await spark
    .locator(SPARK_TREND)
    .evaluateAll((nodes) => nodes.map((n) => n.getAttribute("points") ?? ""));
  return runs
    .flatMap((points) => points.trim().split(/\s+/))
    .map((pair) => Number(pair.split(",")[1]))
    .filter((y) => Number.isFinite(y));
}

/** `NaN`, not `null`: `null <= SPARK_WARN_Y` coerces TRUE and polls vacuously. */
export async function newestTrendY(spark: Locator): Promise<number> {
  const ys = await plottedTrendYs(spark);
  return ys.at(-1) ?? Number.NaN;
}

/** Lowest point in the plotted window (largest y), or `NaN` — see above. */
export async function deepestTrendY(spark: Locator): Promise<number> {
  const ys = await plottedTrendYs(spark);
  return ys.length > 0 ? Math.max(...ys) : Number.NaN;
}

/** ONE turn: separate `count()`s straddle the 1 Hz repaint and never agree. */
export async function keylinesMatchTrends(spark: Locator): Promise<boolean> {
  return await spark.evaluate(
    (mount, sel) => {
      const trends = mount.querySelectorAll(sel.trend).length;
      return trends > 0 && mount.querySelectorAll(sel.keyline).length === trends;
    },
    { keyline: SPARK_KEYLINE, trend: SPARK_TREND },
  );
}

export async function keylinesPrecedeTrends(spark: Locator): Promise<boolean> {
  return await spark.evaluate(
    (mount, sel) => {
      const keylines = mount.querySelectorAll(sel.keyline);
      const trends = mount.querySelectorAll(sel.trend);
      if (keylines.length === 0 || trends.length === 0) return false;
      const lastKeyline = keylines[keylines.length - 1];
      return Boolean(
        lastKeyline.compareDocumentPosition(trends[0]) & Node.DOCUMENT_POSITION_FOLLOWING,
      );
    },
    { keyline: SPARK_KEYLINE, trend: SPARK_TREND },
  );
}

/** `"no-area-fill"` ONLY once a trend is plotted, so an unpainted mount reads
 *  `"no-trend"` and its caller polls on rather than passing vacuously. ONE turn:
 *  separate `count()`s straddle the 1 Hz repaint and never agree. */
export async function sparkAreaFillState(spark: Locator): Promise<string> {
  return await spark.evaluate(
    (mount, sel) => {
      if (mount.querySelectorAll(sel.trend).length === 0) return "no-trend";
      const areas = mount.querySelectorAll(sel.area).length;
      return areas === 0 ? "no-area-fill" : `area-fill:${areas}`;
    },
    { area: SPARK_REMOVED_AREA, trend: SPARK_TREND },
  );
}

/** Authored `stroke-width`s in paint order. EMPTY on no match — count first. */
export async function authoredStrokeWidths(marks: Locator): Promise<number[]> {
  return await marks.evaluateAll((nodes) =>
    nodes.map((n) => Number(n.getAttribute("stroke-width"))),
  );
}

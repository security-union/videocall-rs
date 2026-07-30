/**
 * Regression spec: per-frame text-width memoisation in the recording compositor.
 *
 * ## What this guards
 *
 * `drawFrame()` in `dioxus-ui/scripts/recording.js` measures text for TWO
 * elements on EVERY participant tile, EVERY frame — the floating-name chip
 * (`drawNameChip`) and the WT/WS transport badge (`drawTile`). `measureText` is
 * a known canvas hazard (it shapes the string and allocates a fresh TextMetrics
 * object per call). The (font, text) key space is tiny and fixed per session, so
 * both call sites now route through the production `measureTextWidthCached`
 * helper, which memoises the width keyed by "font\ntext".
 *
 * ## How it guards it (mutation sensitivity)
 *
 * The test drives the EXACT production function — exposed as
 * `window.__vcRecording._measureTextWidthCached` — against a canvas context whose
 * `measureText` has been wrapped with a call counter, using a random (guaranteed
 * cold-miss) text key. It asserts:
 *
 *   - the first lookup measures exactly once (cold miss), and
 *   - the second lookup for the SAME (font, text) adds NO further measureText
 *     call (served from cache), while returning the identical width.
 *
 * Revert the memoisation (make the helper measure every call) and the second
 * lookup measures again → `callsAfterSecond` becomes 2 → this test fails. It
 * therefore pins the caching behaviour, not a re-implementation of it.
 *
 * ## Harness
 *
 * `recording.js` is loaded on every app page via `<script defer src="/recording.js">`
 * (see `dioxus-ui/index.html`), so navigating to the app root is enough to expose
 * `window.__vcRecording` — no meeting, media, or auth is required for this
 * pure-logic path.
 */

import { test, expect } from "@playwright/test";

// Local shape for the diagnostic accessor exposed by recording.js. Declared as
// a cast rather than a global `Window` augmentation so it does not collide with
// the `__vcRecording` augmentation in recording.spec.ts (TS merges global
// augmentations and rejects a differently-typed re-declaration of the same
// property).
type RecordingTestApi = {
  _measureTextWidthCached(ctx: CanvasRenderingContext2D, font: string, text: string): number;
};

test("recording compositor memoises per-frame text-width measurements", async ({
  page,
  baseURL,
}) => {
  const uiURL = baseURL || "http://localhost:3001";
  await page.goto(uiURL, { waitUntil: "domcontentloaded" });

  // recording.js is deferred; wait until its IIFE has exposed the accessor.
  await page.waitForFunction(
    () =>
      typeof (window as unknown as { __vcRecording?: RecordingTestApi }).__vcRecording
        ?._measureTextWidthCached === "function",
    undefined,
    { timeout: 15_000 },
  );

  const result = await page.evaluate(() => {
    const fn = (window as unknown as { __vcRecording: RecordingTestApi }).__vcRecording
      ._measureTextWidthCached;

    // Fresh context whose measureText is wrapped with a call counter. The
    // production helper calls THIS ctx.measureText only on a cache MISS.
    const cv = document.createElement("canvas");
    const ctx = cv.getContext("2d") as CanvasRenderingContext2D;
    let calls = 0;
    const orig = ctx.measureText.bind(ctx);
    ctx.measureText = (t: string) => {
      calls++;
      return orig(t);
    };

    // A random text guarantees a cold miss regardless of any widths the running
    // app may have already cached in the shared module-level map.
    const font = "600 11px -apple-system, BlinkMacSystemFont, sans-serif";
    const text = "REGRESSION-" + Math.random();

    const w1 = fn(ctx, font, text);
    const callsAfterFirst = calls;
    const w2 = fn(ctx, font, text);
    const callsAfterSecond = calls;

    return { w1, w2, callsAfterFirst, callsAfterSecond };
  });

  // Cold miss measures exactly once.
  expect(result.callsAfterFirst).toBe(1);
  // Second lookup for the same (font, text) is served from cache — no extra
  // measureText. This is the assertion that fails if the cache is reverted.
  expect(result.callsAfterSecond).toBe(1);
  // The cached width matches the freshly-measured one (correctness of the hit).
  expect(result.w2).toBe(result.w1);
  expect(result.w1).toBeGreaterThan(0);
});

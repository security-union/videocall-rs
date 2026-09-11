/**
 * Override `vadThreshold` for one BrowserContext. One value feeds BOTH VADs —
 * the mic encoder's (whose `is_speaking` rides the heartbeat) and every peer
 * decoder's — so patching one context splits the two ends of a call.
 *
 * Routes rather than `addInitScript` (`config.js` reassigns `__APP_CONFIG`), and
 * patches `config.local.js` too — a gitignored shim local serves ship and CI
 * does not, which would otherwise re-set the key and make a spec pass in one and
 * fail in the other (#1883). Must run before the first navigation: memoized.
 */

import { BrowserContext } from "@playwright/test";

export const VAD_THRESHOLD_FLAG_KEY = "vadThreshold";

/** Latches the ENCODER's `is_speaking` true — `set_speaking` fires only on a change. */
export const ALWAYS_SPEAKING_VAD_THRESHOLD = -1.0;

/**
 * Makes the DECODER's VAD unconditionally negative: PCM is normalised to
 * [-1.0, 1.0], so RMS cannot exceed 1.0 and 1.5 sits above every reachable value
 * regardless of source level or capture gain. No `peer_speaking` is emitted, so
 * the verdict stays `Unheard` — but audio still arrives and plays.
 */
export const NEVER_SPEAKING_VAD_THRESHOLD = 1.5;

/** A bare NUMBER: `RuntimeConfig::vad_threshold` is an `f32`; a string fails the parse. */
export async function setVadThreshold(context: BrowserContext, value: number): Promise<void> {
  if (!Number.isFinite(value)) {
    throw new Error(
      `vadThreshold must be finite (got ${value}); NaN/Infinity serialises as \`null\` and ` +
        `fails the serde f32 parse, taking the whole RuntimeConfig with it`,
    );
  }
  const entry = `${JSON.stringify(VAD_THRESHOLD_FLAG_KEY)}:${value}`;
  const injection = `;window.__APP_CONFIG=Object.assign(window.__APP_CONFIG||{},{${entry}});`;

  await context.route("**/config.js", async (route) => {
    const response = await route.fetch();
    const original = await response.text();
    const patched = original.trimStart().startsWith("window.__APP_CONFIG")
      ? original + injection
      : `window.__APP_CONFIG=window.__APP_CONFIG||{};` + injection;
    await route.fulfill({ status: 200, contentType: "application/javascript", body: patched });
  });

  await context.route("**/config.local.js", async (route) => {
    let original = "";
    try {
      const response = await route.fetch();
      if (response.status() === 200) {
        original = await response.text();
      }
    } catch {
      /* shim absent on this serve (404 in CI) — serve just the patch */
    }
    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: original + injection,
    });
  });
}

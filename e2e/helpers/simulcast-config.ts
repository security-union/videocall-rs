/**
 * E2E helper: enable the experimental per-receiver simulcast feature for a
 * single BrowserContext WITHOUT touching any committed/local config file.
 *
 * ## Why a route interception (and not addInitScript)
 *
 * The Dioxus UI reads its runtime config from `window.__APP_CONFIG`, which is
 * populated by `dioxus-ui/scripts/config.js` via a *full reassignment*:
 *
 *   window.__APP_CONFIG = ({ ...keys... });
 *
 * Because it reassigns the whole object, a pre-navigation `page.addInitScript`
 * that sets `window.__APP_CONFIG.experimentalSimulcastMaxLayers` is clobbered
 * the instant `config.js` runs. The committed `config.js` shipped by the e2e
 * docker stack ALSO omits `experimentalSimulcastMaxLayers` entirely (see
 * `docker/start-dioxus.sh`, which has no SIMULCAST env line), so the Rust
 * `#[serde(default = ...)]` falls back to `1` — feature OFF.
 *
 * The robust, source-file-free way to flip the flag for just the test browser
 * is to intercept the `GET /config.js` response, append the simulcast key to
 * the served object literal, and let the patched script run normally. This:
 *   - never mutates `dioxus-ui/scripts/config.js` (the committed production
 *     default stays 1 / OFF),
 *   - never touches the developer's gitignored `config.local.js` override,
 *   - is scoped to the intercepting context only (other tabs/tests are
 *     unaffected),
 *   - survives the `config.local.js` `Object.assign` shim, which only sets the
 *     keys it explicitly lists (it does not list this one).
 *
 * ## IMPORTANT: capability ceiling can still force 1 layer (see report)
 *
 * The effective publisher layer count is
 *   `min(experimentalSimulcastMaxLayers, capability_max_simulcast_layers())`
 * (see `dioxus-ui/src/components/host.rs`). The capability ceiling is derived
 * at runtime from `navigator.hardwareConcurrency`, the UA platform, AND a live
 * ~100ms CPU benchmark (`videocall_capability_score()`), with thresholds:
 *   - `< 4` cores            → Block  → 1 layer
 *   - `< 6` cores            → StrongWarn → 1 layer
 *   - older Intel Mac        → StrongWarn → 1 layer
 *   - score `< 5000`         → 1 layer
 *   - score `5000..30000`    → 2 layers
 *   - score `>= 30000`       → 3 layers
 *
 * There is NO test override hook for the benchmark score today, so on a weak /
 * containerized CI runner the publisher may still emit a single layer even with
 * this flag set to 3. Tests that assert multi-layer SEND behaviour therefore
 * treat ">= 2 received layers reported in the ladder" as the success signal and
 * are written to be skipped (not failed) when the runner's capability ceiling
 * clamps to 1 — that branch is documented inline in the spec.
 */

import { BrowserContext } from "@playwright/test";

/**
 * The runtime flag key consumed by
 * `RuntimeConfig::experimental_simulcast_max_layers`
 * (`dioxus-ui/src/constants.rs`, serde rename `experimentalSimulcastMaxLayers`).
 */
export const SIMULCAST_FLAG_KEY = "experimentalSimulcastMaxLayers";

/**
 * Patch the `config.js` served to every page in `context` so the experimental
 * simulcast flag is set to `maxLayers` (default 3). Idempotent per context.
 *
 * Must be called BEFORE the first navigation in the context so the very first
 * `/config.js` request is intercepted.
 *
 * @param context   The BrowserContext to patch (route is context-scoped).
 * @param maxLayers The value to inject for `experimentalSimulcastMaxLayers`.
 */
export async function enableSimulcastFlag(
  context: BrowserContext,
  maxLayers: number = 3,
): Promise<void> {
  await context.route("**/config.js", async (route) => {
    // Fetch the real config.js the server would have served, then append our
    // override key so production defaults (and any docker-generated values)
    // are preserved.
    const response = await route.fetch();
    const original = await response.text();

    // Defensive: if the body is not the expected object-literal assignment
    // (e.g. an SPA HTML fallback), fall back to writing a minimal config so
    // the flag is still applied rather than silently lost.
    const injection = `;window.__APP_CONFIG=Object.assign(window.__APP_CONFIG||{},{${JSON.stringify(
      SIMULCAST_FLAG_KEY,
    )}:${Number(maxLayers)}});`;

    const patched = original.trimStart().startsWith("window.__APP_CONFIG")
      ? original + injection
      : `window.__APP_CONFIG=window.__APP_CONFIG||{};` + injection;

    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: patched,
    });
  });
}

/**
 * Pin `experimentalSimulcastMaxLayers` to an EXPLICIT value for a context,
 * regardless of the production default. Thin wrapper over
 * {@link enableSimulcastFlag} so the OFF-path / single-layer no-regression test
 * reads correctly (`pinSimulcastMaxLayers(ctx, 1)`) instead of the misleading
 * `enableSimulcastFlag(ctx, 1)`.
 *
 * ## Why this is now REQUIRED for the OFF path (not just stylistic)
 *
 * The runtime default of `experimentalSimulcastMaxLayers`
 * (`dioxus-ui/src/constants.rs`) has been flipped from `1` → `3` (multicast ON
 * by default). A test that sets NO flag therefore now gets `3`, not `1`. To
 * genuinely exercise the single-layer / feature-OFF path, the OFF test must pin
 * the value to `1` explicitly via this helper rather than relying on the
 * (no-longer-1) default.
 *
 * Must be called BEFORE the first navigation in the context (same constraint as
 * {@link enableSimulcastFlag}).
 *
 * @param context   The BrowserContext to patch (route is context-scoped).
 * @param maxLayers The exact value to inject (e.g. `1` for the OFF path).
 */
export async function pinSimulcastMaxLayers(
  context: BrowserContext,
  maxLayers: number,
): Promise<void> {
  await enableSimulcastFlag(context, maxLayers);
}

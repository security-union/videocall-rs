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
 * ## Capability ceiling can force 1 layer — and the #1093 override that lifts it
 *
 * The effective publisher layer count is
 *   `min(experimentalSimulcastMaxLayers, capability_max_simulcast_layers())`
 * (see `dioxus-ui/src/components/host.rs`). Post-#1140/#1141 the capability
 * ceiling is derived from cheap, stable device facts ONLY — CPU core count
 * (`navigator.hardwareConcurrency`) and the UA platform — with NO CPU benchmark:
 *   - `< 6` cores OR unknown → 1 layer
 *   - older Intel Mac        → 1 layer
 *   - `6..10` cores          → 2 layers
 *   - `>= 10` cores          → 3 layers
 * (The runtime `videocall-aq` loop then earns layers up to that ceiling.)
 *
 * On a containerized CI runner `navigator.hardwareConcurrency` is typically 1–2,
 * so the sniffed ceiling clamps to 1 and the publisher emits a single layer even
 * with `experimentalSimulcastMaxLayers` set to 3 — which is why the multi-party
 * SEND tests were `test.fixme`'d (issue #1093).
 *
 * Issue #1093 adds a TEST-ONLY config key, `testCapabilityMaxLayersOverride`,
 * that REPLACES the sniffed capability ceiling (clamped to `[1, ladder depth]`;
 * `0` → 1). Inject it via the `capabilityMaxLayersOverride` option on
 * {@link enableSimulcastFlag} to force the publisher to a known layer count
 * regardless of the runner's core count. The key affects ONLY the capability
 * ceiling, never the `experimentalSimulcastMaxLayers` flag, and the UI emits a
 * `warn!` whenever it is honoured so it can't silently leak into production. It is
 * absent from every production / default-docker `config.js`.
 *
 * It is injected by the SAME `/config.js` route interception as the flag (NOT
 * `addInitScript`, for the reassignment reason above), appended as an extra key in
 * the same `Object.assign` patch.
 */

import { BrowserContext } from "@playwright/test";

/**
 * The runtime flag key consumed by
 * `RuntimeConfig::experimental_simulcast_max_layers`
 * (`dioxus-ui/src/constants.rs`, serde rename `experimentalSimulcastMaxLayers`).
 */
export const SIMULCAST_FLAG_KEY = "experimentalSimulcastMaxLayers";

/**
 * The TEST-ONLY capability-ceiling override key consumed by
 * `RuntimeConfig::test_capability_max_layers_override`
 * (`dioxus-ui/src/constants.rs`, serde rename `testCapabilityMaxLayersOverride`).
 * When present it REPLACES the device-sniffed
 * `capability_max_simulcast_layers()` ceiling (issue #1093). Absent in production.
 */
export const CAPABILITY_OVERRIDE_KEY = "testCapabilityMaxLayersOverride";

/** Options for {@link enableSimulcastFlag}. */
export interface SimulcastConfigOptions {
  /**
   * TEST-ONLY override for the device-capability simulcast ceiling (issue #1093).
   * When provided, `testCapabilityMaxLayersOverride` is injected alongside the
   * flag so the publisher's effective layer count is
   * `min(experimentalSimulcastMaxLayers, override)` rather than
   * `min(flag, sniffed-ceiling)`. This lets the multi-party SEND tests force a
   * known layer count on a low-core CI runner whose sniffed ceiling would
   * otherwise clamp to 1.
   *
   * The UI clamps the value to `[1, ladder depth]` (`0` → 1), so a `3` here forces
   * the full ladder. Omit it (the default) to leave the real device-sniffed
   * ceiling in effect — matching production behaviour.
   */
  capabilityMaxLayersOverride?: number;
}

/**
 * Patch the `config.js` served to every page in `context` so the experimental
 * simulcast flag is set to `maxLayers` (default 3), and — when
 * `options.capabilityMaxLayersOverride` is given — ALSO inject the TEST-ONLY
 * `testCapabilityMaxLayersOverride` capability-ceiling override (issue #1093).
 * Idempotent per context.
 *
 * Must be called BEFORE the first navigation in the context so the very first
 * `/config.js` request is intercepted.
 *
 * @param context   The BrowserContext to patch (route is context-scoped).
 * @param maxLayers The value to inject for `experimentalSimulcastMaxLayers`.
 * @param options   Optional extra config injection (e.g. the #1093 capability
 *                  override). Defaults to none, so existing callers are unchanged.
 */
export async function enableSimulcastFlag(
  context: BrowserContext,
  maxLayers: number = 3,
  options: SimulcastConfigOptions = {},
): Promise<void> {
  // Build the key/value pairs to inject. The flag is always set; the capability
  // override is only added when explicitly requested, so omitting it leaves the
  // real device-sniffed ceiling in effect (production-equivalent behaviour) and
  // never writes the test-only key into `config.js`.
  const entries: string[] = [`${JSON.stringify(SIMULCAST_FLAG_KEY)}:${Number(maxLayers)}`];
  if (options.capabilityMaxLayersOverride !== undefined) {
    entries.push(
      `${JSON.stringify(CAPABILITY_OVERRIDE_KEY)}:${Number(options.capabilityMaxLayersOverride)}`,
    );
  }

  await context.route("**/config.js", async (route) => {
    // Fetch the real config.js the server would have served, then append our
    // override key(s) so production defaults (and any docker-generated values)
    // are preserved.
    const response = await route.fetch();
    const original = await response.text();

    // Defensive: if the body is not the expected object-literal assignment
    // (e.g. an SPA HTML fallback), fall back to writing a minimal config so
    // the key(s) are still applied rather than silently lost.
    const injection = `;window.__APP_CONFIG=Object.assign(window.__APP_CONFIG||{},{${entries.join(
      ",",
    )}});`;

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

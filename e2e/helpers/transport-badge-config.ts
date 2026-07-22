/**
 * E2E helper: flip the server-side `transportBadgeEnabled` runtime flag for a
 * single BrowserContext WITHOUT mutating any committed/local config file.
 *
 * ## What the flag does (issue #1483)
 *
 * When `transportBadgeEnabled` is truthy, every REMOTE peer tile whose
 * transport is known renders a small "WT"/"WS" badge
 * (`span.transport-badge.transport-badge--wt|--ws`) inside `.tile-top-icons`,
 * adjacent to the `.signal-indicator` button. The badge is gated in
 * `dioxus-ui/src/components/peer_tile.rs`
 * (`transport_badge_enabled().unwrap_or(false)`), which re-parses
 * `window.__APP_CONFIG.transportBadgeEnabled` and runs it through
 * `videocall_types::truthy` — so ONLY the strings `"true"`/`"1"`
 * (case-insensitive) enable it. The committed default
 * (`dioxus-ui/scripts/config.js`) is `transportBadgeEnabled: "false"` → OFF.
 *
 * ## Why a `/config.js` route interception (and not addInitScript)
 *
 * Identical reasoning to `helpers/simulcast-config.ts`: the Dioxus UI reads its
 * runtime config from `window.__APP_CONFIG`, which `config.js` populates via a
 * FULL reassignment (`window.__APP_CONFIG = ({ ... })`). A pre-navigation
 * `page.addInitScript` that sets the key on `window.__APP_CONFIG` is therefore
 * clobbered the instant `config.js` runs. Intercepting the `GET /config.js`
 * response and appending an `Object.assign` patch survives that reassignment,
 * is scoped to the intercepting context only, and never touches the committed
 * `config.js` (so the production default stays OFF and the OFF-path test below
 * keeps exercising the real default).
 *
 * ## Why the value is a STRING, not a JS boolean
 *
 * `RuntimeConfig::transport_badge_enabled` is a `String` field
 * (`dioxus-ui/src/constants.rs`, serde rename `transportBadgeEnabled`), and
 * `truthy` lowercases the value and matches it against `"true"`/`"1"`. A JS
 * boolean (`true`) would serialise as the wrong runtime type for the serde
 * `String`. We therefore inject the literal string `"true"` — matching how the
 * committed `config.js` ships `"false"`.
 */

import { BrowserContext } from "@playwright/test";

/**
 * The runtime flag key consumed by `RuntimeConfig::transport_badge_enabled`
 * (`dioxus-ui/src/constants.rs`, serde rename `transportBadgeEnabled`).
 */
export const TRANSPORT_BADGE_FLAG_KEY = "transportBadgeEnabled";

/**
 * Patch the `config.js` served to every page in `context` so
 * `transportBadgeEnabled` is set to the string `value` (default `"true"`,
 * i.e. ON). Production defaults / docker-generated keys in the served
 * `config.js` are preserved; only this one key is appended.
 *
 * Must be called BEFORE the first navigation in the context so the very first
 * `/config.js` request is intercepted.
 *
 * @param context The BrowserContext to patch (route is context-scoped).
 * @param value   The string to inject for `transportBadgeEnabled`. Use `"true"`
 *                (the default) to turn the badge ON; `"false"` to force it OFF
 *                explicitly. Passed through `videocall_types::truthy` on the
 *                Rust side, so only `"true"`/`"1"` (case-insensitive) enable it.
 */
export async function setTransportBadgeFlag(
  context: BrowserContext,
  value: string = "true",
): Promise<void> {
  const entry = `${JSON.stringify(TRANSPORT_BADGE_FLAG_KEY)}:${JSON.stringify(value)}`;
  const injection = `;window.__APP_CONFIG=Object.assign(window.__APP_CONFIG||{},{${entry}});`;

  await context.route("**/config.js", async (route) => {
    // Fetch the real config.js the server would have served, then append our
    // override key so production defaults (and any docker-generated values)
    // are preserved.
    const response = await route.fetch();
    const original = await response.text();

    // Defensive: if the body is not the expected object-literal assignment
    // (e.g. an SPA HTML fallback), still apply the key rather than lose it.
    const patched = original.trimStart().startsWith("window.__APP_CONFIG")
      ? original + injection
      : `window.__APP_CONFIG=window.__APP_CONFIG||{};` + injection;

    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: patched,
    });
  });

  // ALSO patch `config.local.js` (issue #1883): index.html loads a gitignored
  // developer-override shim AFTER `config.js` (a synchronous XHR eval), and when
  // a serve ships one (e.g. a local `trunk serve` clone — the e2e docker stack
  // that mirrors `clone-*/dist`) it `Object.assign`s its own values onto
  // `__APP_CONFIG`, RE-SETTING `transportBadgeEnabled` and clobbering the
  // `config.js` patch above. Without re-applying the flag after the shim, the
  // stack's local value wins and NO badge renders — which is exactly why the
  // flag-ON tests failed against the docker stack while passing in CI (where no
  // `config.local.js` is served). Re-apply the flag AFTER the shim's body so ON
  // wins in BOTH serve shapes: present → append after its overrides; absent
  // (404 in CI) → serve the patch alone (the index.html XHR evals it and sets
  // the key). Only `transportBadgeEnabled` is touched; all other shim keys pass
  // through unchanged.
  await context.route("**/config.local.js", async (route) => {
    let original = "";
    try {
      const response = await route.fetch();
      if (response.status() === 200) {
        original = await response.text();
      }
    } catch {
      /* shim absent on this serve — serve just the patch */
    }
    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: original + injection,
    });
  });
}

/**
 * E2E helper: flip the `showBuildGitInfo` runtime flag for a single
 * BrowserContext WITHOUT mutating any committed/local config file.
 *
 * ## What the flag does (issue #1480)
 *
 * When `showBuildGitInfo` is truthy, the git COMMIT SHA + BRANCH are surfaced in
 * two places, both gated in the Rust UI via `crate::constants::show_build_git_info()`
 * (which re-parses `window.__APP_CONFIG.showBuildGitInfo` through
 * `videocall_types::truthy`, so ONLY `"true"`/`"1"` case-insensitive enable it):
 *
 *   1. The About modal (`dioxus-ui/src/components/about_modal.rs`):
 *      - Client section gains `Commit` + `Branch` rows (each an `.about-modal-row`
 *        whose `.about-modal-label` is "Commit"/"Branch"). They are rsx-`if`-gated,
 *        so when the flag is falsey the rows are REMOVED from the DOM (not hidden).
 *      - Server table header gains a `Commit` span and each data row gains a Commit
 *        span. When hidden, the header + data rows carry the extra class
 *        `about-modal-row--server-nogit` (a 3-col grid) instead of the base 4-col
 *        `.about-modal-row`.
 *   2. The diagnostics build-info table (`dioxus-ui/src/components/diagnostics.rs`):
 *      - Truthy -> `div.build-info-table.build-info-table--git` (Component/Commit/
 *        Branch/Built, 4 cols). Falsey -> `build-info-table--nogit` (Component/Built,
 *        2 cols). Commit/Branch header cells + per-row spans are rsx-`if`-gated.
 *
 * VERSION and the BUILD TIMESTAMP (`Built`) are ALWAYS shown regardless of this
 * flag. FAIL-CLOSED on the Rust side: absent / empty / falsey -> hidden, so a
 * production `config.js` that omits the key never leaks commit/branch.
 *
 * ## Why this patches `config.local.js`, not just `config.js`
 *
 * The dioxus UI layers its runtime config in `index.html` in this exact order,
 * and the wasm reads `window.__APP_CONFIG` only AFTER all of them have run:
 *
 *   1. `<script src="/config.js">`  — the committed default; *wholesale-reassigns*
 *      `window.__APP_CONFIG = ({ ... })`. The committed `dioxus-ui/scripts/config.js`
 *      ships `showBuildGitInfo: "true"` (the non-production dev/e2e default).
 *   2. a sync-XHR loader for `/config.local.js` (gitignored dev/e2e override) —
 *      `Object.assign(window.__APP_CONFIG, { ... })`, runs LAST, BEFORE wasm boot.
 *   3. the wasm module reads `window.__APP_CONFIG.showBuildGitInfo`
 *      (constants.rs::show_build_git_info via app_config()).
 *
 * The e2e stack's `docker/start-dioxus.sh` GENERATES a `config.local.js` that sets
 * `showBuildGitInfo` to `"${SHOW_BUILD_GIT_INFO:-false}"`, and Trunk.toml's
 * post_build hook copies it into `dist/`, so it IS served and is the AUTHORITATIVE
 * last layer the app reads. A patch that only rewrites `/config.js` (the old
 * implementation of this helper, and the latent bug `enableSimulcastFlag` /
 * `setTransportBadgeFlag` still carry) is silently UNDONE by `config.local.js`
 * running afterward — the exact failure mode documented for the `wsUrl` override
 * in `helpers/downlink-impair.ts` (issue #1355). The override must therefore land
 * on the LAST layer the app reads, so we intercept `/config.local.js` and append
 * the `showBuildGitInfo` override to it. We ALSO patch `/config.js` (defensively,
 * see below) so the override still wins if the e2e stack ever stops generating
 * `config.local.js`.
 *
 * The committed `config.js` / generated `config.local.js` files are never touched
 * on disk; the patch is a per-context Playwright route fulfillment only.
 *
 * ## Why the value is a STRING, not a JS boolean
 *
 * `RuntimeConfig::show_build_git_info` is a `String` field
 * (`dioxus-ui/src/constants.rs`, serde rename `showBuildGitInfo`), and `truthy`
 * lowercases the value and matches it against `"true"`/`"1"`. A JS boolean
 * (`true`) would serialise as the wrong runtime type for the serde `String`. We
 * therefore inject the literal string — matching how the committed `config.js`
 * ships `"true"`.
 */

import { BrowserContext } from "@playwright/test";

/**
 * The runtime flag key consumed by `RuntimeConfig::show_build_git_info`
 * (`dioxus-ui/src/constants.rs`, serde rename `showBuildGitInfo`).
 */
export const SHOW_BUILD_GIT_INFO_FLAG_KEY = "showBuildGitInfo";

/**
 * Patch BOTH config layers served to every page in `context` so
 * `showBuildGitInfo` is set to the string `value`, with the override landing on
 * the AUTHORITATIVE `config.local.js` last layer so it wins over the
 * docker-generated default. Production defaults / docker-generated keys in the
 * served config are preserved; only this one key is appended.
 *
 * Must be called BEFORE the first navigation in the context so the very first
 * `/config.js` + `/config.local.js` requests are intercepted.
 *
 * @param context The BrowserContext to patch (routes are context-scoped).
 * @param value   The string to inject for `showBuildGitInfo`. Use `"true"` to
 *                show commit/branch; `"false"` to force the production-style
 *                HIDDEN path. Passed through `videocall_types::truthy` on the Rust
 *                side, so only `"true"`/`"1"` (case-insensitive) enable it.
 */
export async function setShowBuildGitInfoFlag(
  context: BrowserContext,
  value: string,
): Promise<void> {
  const entry = `${JSON.stringify(SHOW_BUILD_GIT_INFO_FLAG_KEY)}:${JSON.stringify(value)}`;

  // The override appended to whichever config layer we patch. `Object.assign`
  // onto the live `window.__APP_CONFIG` (creating it if absent) so the single
  // `showBuildGitInfo` key is rewritten while every other key set by the prior
  // layer is preserved.
  const injection = `;window.__APP_CONFIG=Object.assign(window.__APP_CONFIG||{},{${entry}});`;

  // AUTHORITATIVE layer: `/config.local.js` runs last, after `config.js`, and
  // before the wasm reads `__APP_CONFIG` (see the doc comment above). Appending
  // the override here makes the forced value the FINAL value the wasm sees, even
  // though the e2e-generated `config.local.js` sets `showBuildGitInfo` from the
  // `SHOW_BUILD_GIT_INFO` env var.
  await context.route("**/config.local.js", async (route) => {
    let original = "";
    try {
      const response = await route.fetch();
      // Mirror index.html's sync-XHR loader: only a JS-shaped body is real.
      // A 200 SPA HTML fallback (charAt(0) === "<") or an empty body is not a
      // config layer, so we discard it and emit a standalone override instead.
      if (response.ok()) {
        const body = (await response.text()).trim();
        if (body && body.charAt(0) !== "<") {
          original = body;
        }
      }
    } catch {
      /* config.local.js may be absent; emit a standalone override below */
    }
    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: `${original}window.__APP_CONFIG=window.__APP_CONFIG||{};${injection}`,
    });
  });

  // DEFENSIVE layer: also rewrite `/config.js` so the override still applies if
  // the e2e stack ever stops generating a `config.local.js`. The committed
  // `config.js` starts with COMMENT lines before the `window.__APP_CONFIG = (...)`
  // assignment, so a `startsWith("window.__APP_CONFIG")` sniff would return false
  // and drop the entire real config body (the bug `enableSimulcastFlag` still
  // carries). We instead ALWAYS keep the original body and append the override
  // after it — `config.js` is unconditional-assignment JS regardless of leading
  // comments, so concatenation is safe.
  await context.route("**/config.js", async (route) => {
    const response = await route.fetch();
    const original = await response.text();
    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: original + injection,
    });
  });
}

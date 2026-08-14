/**
 * E2E helper: flip the `experimentalReducedLadder` runtime flag for a single
 * BrowserContext WITHOUT mutating any committed/local config file.
 *
 * ## What the flag does (issue #1768)
 *
 * When truthy, every publisher encoding >=2 simulcast layers encodes its CAMERA
 * simulcast ladder with a **540p top rung instead of 720p** (`180p/360p/540p` rather
 * than `180p/360p/720p`). Single-stream publishers (devices clamped to 1 layer by the
 * capability sniff) are unaffected — the variant is read on the simulcast branch only.
 * Read once per `Host` mount by
 * `crate::constants::camera_ladder_variant()` — which routes the value through
 * `videocall_types::truthy`, so ONLY `"true"`/`"1"` (case-insensitive) enable it —
 * and threaded into BOTH publisher halves (the encoder's per-layer geometry and the
 * AQ controller's per-layer bitrate targets).
 *
 * The one USER-VISIBLE consequence, and the reason this helper exists: the
 * performance drawer's SEND rung-strip pip LABEL is derived from the deployed
 * ladder (`performance_settings.rs::send_layer_labels` →
 * `active_camera_top_rung_label`), so the top video pip reads **"540p"** with the
 * flag on and **"720p"** with it off. `send_layer_res_span` was already
 * ladder-derived, so a hardcoded label would make the panel contradict itself.
 *
 * Default OFF: the committed `config.js` omits the key, `RuntimeConfig` defaults it
 * to `""`, and `truthy("")` is false — so the shipped 720p ladder is unchanged
 * unless an operator opts in via the Helm `runtimeConfig`.
 *
 * ## Why this patches `config.local.js` as well as `config.js`
 *
 * Copied deliberately from `show-build-git-info-config.ts`, which documents the
 * layering: `index.html` runs `<script src="/config.js">` (wholesale assignment),
 * then a sync-XHR loader for the gitignored `/config.local.js`
 * (`Object.assign`, runs LAST), and only then does the wasm read
 * `window.__APP_CONFIG`. A patch that rewrites only `/config.js` is silently UNDONE
 * by `config.local.js` running afterwards (issue #1355). The override must land on
 * the last layer the app reads.
 *
 * NOTE: `docker/start-dioxus.sh` does NOT currently emit `experimentalReducedLadder`
 * into `config.local.js`, so its `Object.assign` cannot clobber this override — but
 * we patch both layers anyway so the helper keeps working if that changes.
 *
 * Do NOT model this on `enableSimulcastFlag`: that helper sniffs
 * `original.trimStart().startsWith("window.__APP_CONFIG")`, which is FALSE against
 * the committed `config.js` (it opens with comment lines) and so discards the entire
 * real config on the fallback branch. We always retain `original`.
 *
 * ## ⚠️ ROUTE-ORDER HAZARD — call this BEFORE any other config-patching helper
 *
 * `BrowserContext.route()` registers with `_routes.unshift(...)`
 * (`playwright-core/lib/client/browserContext.js`), so the **last** handler
 * registered for a URL is tried **first**, and `_onRoute` stops at the first handler
 * that reports `handled` — which `fulfill()` does. `route.fetch()` does NOT re-enter
 * the handler list.
 *
 * The `config.js` route below therefore **SHADOWS any earlier `**\/config.js`
 * handler entirely**, silently discarding its injected keys. `enableSimulcastFlag`
 * (`simulcast-config.ts`) and `setShowBuildGitInfoFlag` both register one.
 *
 * So when combining helpers, register THIS one FIRST:
 *
 * ```ts
 * await setReducedLadderFlag(ctx, "true");            // first
 * await enableSimulcastFlag(ctx, 3, { … });           // wins on config.js
 * ```
 *
 * The ladder key still applies, because it also lands on the separate — and
 * unshadowed — `config.local.js` handler, which is the AUTHORITATIVE last layer.
 *
 * Getting this backwards is not a loud failure: it silently reverts
 * `experimentalSimulcastMaxLayers` to the committed `config.js` value of `1`, so the
 * publisher emits ONE layer, the upper rung pips never render, and a
 * capability-guarded test SKIPS — which reads GREEN in the summary. (The same latent
 * hazard pre-dates this helper: `downlink-impair.ts` stacks a `config.js` handler
 * after `enableSimulcastFlag` in `simulcast-per-receiver.spec.ts`.)
 *
 * ## Why the value is a STRING, not a JS boolean
 *
 * `RuntimeConfig::experimental_reduced_ladder` is a `String` field (serde rename
 * `experimentalReducedLadder`) — a defaulted `String` rather than a bare `bool`
 * specifically so a stale bind-mounted `config.js` predating the key still parses.
 * A JS boolean would be the wrong runtime type for serde.
 */

import { BrowserContext } from "@playwright/test";

/**
 * The runtime flag key consumed by `RuntimeConfig::experimental_reduced_ladder`
 * (`dioxus-ui/src/constants.rs`, serde rename `experimentalReducedLadder`).
 */
export const REDUCED_LADDER_FLAG_KEY = "experimentalReducedLadder";

/**
 * Patch BOTH config layers served to every page in `context` so
 * `experimentalReducedLadder` is set to the string `value`, with the override
 * landing on the AUTHORITATIVE `config.local.js` last layer. Every other key in the
 * served config is preserved; only this one is appended.
 *
 * Must be called BEFORE the first navigation in the context so the very first
 * `/config.js` + `/config.local.js` requests are intercepted.
 *
 * @param context The BrowserContext to patch (routes are context-scoped).
 * @param value   The string to inject. `"true"` selects the reduced
 *                180p/360p/540p ladder; `"false"` (or omitting the call) keeps the
 *                shipped 180p/360p/720p one. Passed through
 *                `videocall_types::truthy`, so only `"true"`/`"1"` enable it.
 */
export async function setReducedLadderFlag(context: BrowserContext, value: string): Promise<void> {
  const entry = `${JSON.stringify(REDUCED_LADDER_FLAG_KEY)}:${JSON.stringify(value)}`;

  // `Object.assign` onto the live `window.__APP_CONFIG` (creating it if absent) so
  // this single key is rewritten while every other key from the prior layer stands.
  const injection = `;window.__APP_CONFIG=Object.assign(window.__APP_CONFIG||{},{${entry}});`;

  // AUTHORITATIVE layer: `/config.local.js` runs last, before the wasm reads
  // `__APP_CONFIG`, so appending here makes the forced value final.
  await context.route("**/config.local.js", async (route) => {
    let original = "";
    try {
      const response = await route.fetch();
      // Mirror index.html's sync-XHR loader: only a JS-shaped body is a real config
      // layer. A 200 SPA HTML fallback (charAt(0) === "<") or an empty body is not.
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

  // DEFENSIVE layer: also rewrite `/config.js` so the override still applies if the
  // e2e stack ever stops generating a `config.local.js`. ALWAYS keep the original
  // body — see the `enableSimulcastFlag` warning in the doc comment above.
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

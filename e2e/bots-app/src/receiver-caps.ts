// receiver-caps.ts — resolve + inject the optional per-bot receiver low-power
// knobs (issues #2068 / #2069): cap the RECEIVED simulcast layer and/or skip
// the per-tile canvas paint, to cut a bot's decode + paint CPU so a room can
// hold more bots before the box saturates (capacity = TOTAL room decode load,
// not bot count).
//
// These are LAUNCH-TIME `window.__APP_CONFIG` overrides, NOT runtime-toggleable
// control actions. The served videocall-client parses `window.__APP_CONFIG`
// exactly ONCE and memoizes it (dioxus-ui `constants.rs` `app_config`, #1492),
// and production `config.js` assigns it via `Object.freeze({...})`
// (helm/videocall-ui configmap-configjs). So the override must be in place
// BEFORE the client's first read and cannot change afterward — hence a
// pre-navigation Playwright `addInitScript`, not a `/control` endpoint.
//
// INJECTION MECHANISM (validated against the client config-load path): a bot
// cannot edit the deployment's server-side `config.js`, so the init script
// installs an ACCESSOR (`Object.defineProperty` get/set) on
// `window.__APP_CONFIG`. When `config.js` runs `window.__APP_CONFIG =
// Object.freeze({...})`, that assignment invokes our setter, which merges the
// bot overrides OVER the deployment config into a fresh (unfrozen) object the
// wasm read then sees. This is DISTINCT from the mutation variant the e2e
// helpers avoid (`Object.assign(window.__APP_CONFIG, {...})`, which config.js
// clobbers): the setter INTERCEPTS the reassignment, so it survives the freeze.
//
// OPERATOR NOTES:
//   - Effect requires the DEPLOYED client to carry the #2068/#2069 knobs
//     (videocall-client PR #2078). Against an older deployment the fields are
//     present in `__APP_CONFIG` but ignored — harmless, but the intended CPU
//     saving will NOT materialize. bot.ts logs a post-join assertion so this is
//     visible per run.
//   - `maxReceivedLayer: 0` = base rung only (regression-locked in
//     videocall-client `video_call_client.rs`); ABSENT = no receive cap. It is
//     freeze-safe: relay forwards layer 0 unconditionally.
//   - `skipCanvasPaint` still DECODES each frame (then drops it before
//     `drawImage`) — it saves paint/GPU cost only. To cut DECODE CPU, use
//     `maxReceivedLayer`. The two are independent and composable.

/**
 * Result of resolving a raw `--max-received-layer` / `BOT_MAX_RECEIVED_LAYER`.
 * - `ok` + `value: number`    → a non-negative integer receive cap (`0` = base
 *   rung only). Injected as `window.__APP_CONFIG.maxReceivedLayer`.
 * - `ok` + `value: undefined` → unset/empty; NO cap key injected (the client's
 *   `Option<u32>` stays `None` ⇒ receiver ceiling fully open).
 * - `invalid`                 → malformed / negative; the caller should fail fast.
 */
export type MaxReceivedLayerResult =
  | { kind: "ok"; value: number | undefined }
  | { kind: "invalid"; message: string };

/**
 * Defensive upper bound on `--max-received-layer`. A simulcast ladder is at most
 * a handful of rungs (0..2 today) and the CLIENT clamps this to the real ladder
 * depth regardless, so any small value is safe. The ceiling exists only to
 * fail-fast on a fat-fingered huge value: the client deserializes it into a
 * `u32`, and a value past `u32::MAX` errors the deserialization of the WHOLE
 * `__APP_CONFIG` (not just this key) — so an absurd cap would silently break
 * every runtime-config getter (apiBaseUrl, wsUrl, …) and strand the bot. 16 is
 * far above any real ladder yet forces an obvious CLI error instead.
 */
export const MAX_RECEIVED_LAYER_CEILING = 16;

/**
 * Result of resolving a raw `--skip-canvas-paint` / `BOT_SKIP_CANVAS_PAINT`.
 * - `ok` + `value: boolean`   → override `skipCanvasPaint` to that value.
 * - `ok` + `value: undefined` → unset/empty; NO key injected (inherit the
 *   deployment's `config.js` value, historically `"false"`).
 * - `invalid`                 → not a recognized boolean; caller should fail fast.
 */
export type SkipCanvasPaintResult =
  | { kind: "ok"; value: boolean | undefined }
  | { kind: "invalid"; message: string };

/** Resolved per-bot receiver caps, as consumed by {@link buildReceiverConfigOverrides}. */
export interface ReceiverCaps {
  /** `window.__APP_CONFIG.maxReceivedLayer` — `0` = base only; `undefined` = no cap. */
  maxReceivedLayer?: number;
  /** `window.__APP_CONFIG.skipCanvasPaint` — `undefined` = inherit deployment. */
  skipCanvasPaint?: boolean;
}

/**
 * Resolve `--max-received-layer` (or `BOT_MAX_RECEIVED_LAYER`).
 * Contract (kept in lockstep with the CLI help + the `BotRunOptions` doc):
 *   - unset / empty / whitespace   → `undefined` (no receive cap)
 *   - a non-negative integer (`0`+) → that integer (`0` = base rung only)
 *   - negative / non-integer / junk → invalid (must be rejected)
 *
 * The WHOLE token is integer-validated before `parseInt`, because
 * `Number.parseInt("0junk", 10)` silently returns `0` — accepting a numeric
 * prefix would apply an unintended cap instead of failing fast. `0` is a
 * DELIBERATELY valid value here (unlike hardware-concurrency, where `0` is the
 * disable sentinel): a `0` cap means "base rung only", the primary #2068 use.
 */
export function resolveMaxReceivedLayer(raw: string | undefined): MaxReceivedLayerResult {
  if (raw === undefined || raw.trim() === "") {
    return { kind: "ok", value: undefined };
  }
  const token = raw.trim();
  if (!/^-?\d+$/.test(token)) {
    return {
      kind: "invalid",
      message: `--max-received-layer (or BOT_MAX_RECEIVED_LAYER) must be a non-negative integer, got "${raw}"`,
    };
  }
  const parsed = Number.parseInt(token, 10);
  if (parsed < 0) {
    return {
      kind: "invalid",
      message: `--max-received-layer (or BOT_MAX_RECEIVED_LAYER) must be >= 0 (0 = base rung only), got "${raw}"`,
    };
  }
  if (parsed > MAX_RECEIVED_LAYER_CEILING) {
    return {
      kind: "invalid",
      message: `--max-received-layer (or BOT_MAX_RECEIVED_LAYER) must be <= ${MAX_RECEIVED_LAYER_CEILING} (a simulcast ladder has only a few rungs; a larger value would break the client's whole __APP_CONFIG parse), got "${raw}"`,
    };
  }
  return { kind: "ok", value: parsed };
}

const TRUE_TOKENS = new Set(["true", "1", "yes", "on"]);
const FALSE_TOKENS = new Set(["false", "0", "no", "off"]);

/**
 * Resolve `--skip-canvas-paint` (or `BOT_SKIP_CANVAS_PAINT`).
 *   - unset / empty / whitespace         → `undefined` (inherit deployment)
 *   - `true`/`1`/`yes`/`on`  (any case)   → `true`
 *   - `false`/`0`/`no`/`off` (any case)   → `false`
 *   - anything else                       → invalid (must be rejected)
 *
 * Both an explicit `true` and an explicit `false` are honored (symmetric) so an
 * operator can force paint OFF for CPU saving OR force it back ON against a
 * deployment whose `config.js` enabled it.
 */
export function resolveSkipCanvasPaint(raw: string | undefined): SkipCanvasPaintResult {
  if (raw === undefined || raw.trim() === "") {
    return { kind: "ok", value: undefined };
  }
  const token = raw.trim().toLowerCase();
  if (TRUE_TOKENS.has(token)) {
    return { kind: "ok", value: true };
  }
  if (FALSE_TOKENS.has(token)) {
    return { kind: "ok", value: false };
  }
  return {
    kind: "invalid",
    message: `--skip-canvas-paint (or BOT_SKIP_CANVAS_PAINT) must be a boolean (true/false/1/0/yes/no/on/off), got "${raw}"`,
  };
}

/**
 * Build the `window.__APP_CONFIG` override object from resolved caps, or `null`
 * if nothing is set (⇒ bot.ts injects nothing and the deployment config is used
 * verbatim, preserving default behavior). Includes ONLY the keys the operator
 * set, and — critically — serializes `skipCanvasPaint` as a STRING because the
 * client's `RuntimeConfig.skip_canvas_paint` field is a `String` read through a
 * `truthy()` helper (a JS boolean would fail serde deserialization).
 */
export function buildReceiverConfigOverrides(caps: ReceiverCaps): Record<string, unknown> | null {
  const overrides: Record<string, unknown> = {};
  if (caps.maxReceivedLayer !== undefined) {
    overrides.maxReceivedLayer = caps.maxReceivedLayer;
  }
  if (caps.skipCanvasPaint !== undefined) {
    // String, not boolean — the client's RuntimeConfig field is `String`.
    overrides.skipCanvasPaint = caps.skipCanvasPaint ? "true" : "false";
  }
  return Object.keys(overrides).length > 0 ? overrides : null;
}

/**
 * Build the Playwright `addInitScript` source that installs the
 * `window.__APP_CONFIG` setter-merge for the given overrides. Pure string
 * builder (the mutation-testable core): the returned IIFE defines an accessor
 * on `window.__APP_CONFIG` whose setter merges `overrides` over whatever
 * `config.js` assigns (including a frozen literal), so the wasm client's
 * one-time read observes the merged config.
 *
 * `current` is seeded with the overrides so a read BEFORE `config.js` runs (or
 * a deployment that mutates in place instead of reassigning) still returns a
 * valid object rather than `undefined`.
 */
export function buildReceiverConfigInitScript(overrides: Record<string, unknown>): string {
  const json = JSON.stringify(overrides);
  return `(() => {
  const __botReceiverOverrides = ${json};
  let __botAppConfig = Object.assign({}, __botReceiverOverrides);
  try {
    Object.defineProperty(window, '__APP_CONFIG', {
      configurable: true,
      get() { return __botAppConfig; },
      set(v) {
        __botAppConfig = (v && typeof v === 'object')
          ? Object.assign({}, v, __botReceiverOverrides)
          : Object.assign({}, __botReceiverOverrides);
      },
    });
  } catch (e) {
    console.error('[bot] receiver-caps __APP_CONFIG override failed to install:', e);
  }
})();`;
}

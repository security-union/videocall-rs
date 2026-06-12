/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Simulcast layer-capability *ceiling* (issue #989 / #1082 / #1140 / #1141).
//!
//! This module derives the maximum number of simulcast layers a device is
//! **allowed** to encode, from cheap, stable device facts only: CPU core count
//! and UA platform. Encoding N layers is ~N× the encode CPU, so a weak publisher
//! that overcommits to 3 layers would stall its own main thread (the failure
//! mode discussion #890 / #562 documents on 2-core Intel Macs).
//!
//! ## #1140 / #1141: the cold CPU benchmark no longer gates layer count
//!
//! Historically [`max_simulcast_layers`] also consumed
//! `videocall_capability_score()` — a 100 ms `f64` multiply-add microbenchmark —
//! and thresholded it against fixed cross-device constants (`SCORE_FOR_2_LAYERS`,
//! `SCORE_FOR_3_LAYERS`) to pick 1/2/3 layers. That was a category error: the
//! score measures scalar FP throughput in the wasm main thread, but running 2–3
//! WebCodecs `VideoEncoder`s is bound by the hardware encode block, GPU/VRAM, and
//! memory bandwidth — none of which the loop touches. Worse, the score's run-to-
//! run noise band (~±1500 on the same machine) was *wider* than the decision
//! boundary at 5000, so 87% of the fleet latched to a single layer on a coin
//! flip (issue #1140).
//!
//! The score is now **observability-only** (a log-once preamble breadcrumb +
//! the `client_capability_score` health field). It MUST NOT be read by any layer
//! decision. The ceiling is `f(cores, platform)`; the *operating point* (how many
//! layers are actually encoded) is earned at runtime by the `videocall-aq`
//! control loop, which observes real encoder-queue backpressure + uplink budget
//! and ramps up one layer at a time from a base of 1 (issue #1141). A generous
//! ceiling is therefore safe — the runtime loop, not this function, realizes
//! layers, and it sheds fast under backpressure.
//!
//! It is **not** a join gate. Per product decision (issue #1054) every client
//! joins regardless of capabilities — there is no pre-join block or warning.
//! The old `assess_from_inputs` / `assess_capability` verdict path that gated
//! join has been removed.
//!
//! The pure-logic core lives in [`max_simulcast_layers`] / [`is_older_intel_mac`]
//! / [`parse_platform_from_ua`] so it can be unit-tested on host without a
//! browser. [`capability_max_simulcast_layers`] is the wasm32 wrapper that
//! sources real navigator data.

/// At or below this core count the device is "marginal" for simulcast: encode
/// CPU is too tight to safely run more than a single layer.
///
/// Anchored in the cc7tp post-mortem: 2-core / low-core Intel MacBooks hit
/// catastrophic main-thread stalls under multi-layer encode. This preserves the
/// old StrongWarn floor (cores < 6); the old Block floor (cores < 4 / unknown)
/// collapses into it since both pin the ceiling to 1 layer.
const MIN_CORES_FOR_MULTILAYER: u32 = 6;

/// Cap for the `macOS 15*` older-Intel rule. Above this, modern Intel /
/// Apple-Silicon Macs are typically fine.
const OLDER_INTEL_MAC_15_CORE_CEILING: u32 = 8;

/// At or above this core count the device is allowed the full 3-layer ladder
/// (issue #1140 / #1141). Below it (but `>= MIN_CORES_FOR_MULTILAYER`) the
/// ceiling is 2.
///
/// Chosen from stable, cheap device facts rather than the (now-removed) CPU
/// microbenchmark: 10+ *logical* cores reliably distinguishes a genuinely
/// capable publisher (modern Apple-Silicon M-series report 8–12, high-end
/// desktops 12–16+) from the 6–8-core older-Intel laptops that the
/// [`is_older_intel_mac`] rule already pins to a single layer. The bar is a
/// *ceiling*, not an operating point: the `videocall-aq` runtime ramp earns the
/// third layer only when observed backpressure headroom + uplink budget allow,
/// and sheds it fast otherwise — so erring slightly generous here is safe (a
/// false-high core-count bet is caught by the backpressure shed, never latched).
const CORES_FOR_3_LAYERS: u32 = 10;

/// Heuristic: does this look like an older Intel Mac that we should
/// warn about for large meetings?
///
/// Rules (per the Phase 9 spec):
///
/// * Any `macOS 14*` host → `true` (these are the Big Sur / Monterey-era
///   Intel MacBooks, deep into thermal-throttling territory).
/// * `macOS 15*` host **with** `cores <= 8` → `true` (older Intel MBPs
///   that report 4-8 cores; modern Apple-Silicon machines exceed this
///   ceiling).
/// * Anything else → `false`.
///
/// The platform string should already be the canonical token produced by
/// [`parse_platform_from_ua`].
pub fn is_older_intel_mac(platform: &str, cores: u32) -> bool {
    if platform.starts_with("macOS 14") {
        return true;
    }
    if platform.starts_with("macOS 15") && cores <= OLDER_INTEL_MAC_15_CORE_CEILING {
        return true;
    }
    false
}

/// Best-effort platform extraction from a User-Agent string.
///
/// Returns one of:
///
/// * `"macOS <X>"` where `<X>` is the major version inferred from the
///   embedded `Mac OS X N_M_P` token (the Safari/Chromium UA convention).
///   Note that `Mac OS X 10_15_7` is the historical "frozen" string Safari
///   serves on every modern macOS host since Catalina, so it does **not**
///   tell us much; we map it to `"macOS 10"`. Real per-version detection
///   on Apple-Silicon requires `navigator.userAgentData.getHighEntropyValues`
///   which we may layer in later.
/// * `"Windows 10"` / `"Windows 11"` for `Windows NT 10.0`+ hosts.
/// * `"Linux"` for any X11/Linux UA.
/// * `""` if nothing matched (unknown).
///
/// Like [`max_simulcast_layers`], the only non-test caller is the wasm32-gated
/// [`capability_max_simulcast_layers`], so a native non-test build (e.g.
/// `cargo clippy --all`) sees this as dead code; the `allow` keeps that build
/// warning-free without hiding genuine dead code on wasm or in the host tests.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn parse_platform_from_ua(user_agent: &str) -> String {
    // Mac UA tokens look like: "(Macintosh; Intel Mac OS X 10_15_7)".
    if let Some(rest) = user_agent.split("Mac OS X ").nth(1) {
        // `rest` now starts with e.g. "10_15_7) AppleWebKit/...".
        let major: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !major.is_empty() {
            return format!("macOS {major}");
        }
    }

    if user_agent.contains("Windows NT 10.0") {
        // Chromium shipped a UA-reduction change that maps Windows 11 back
        // to "Windows NT 10.0", so we can't reliably split 10 vs 11 from
        // the legacy UA string. Treat both as "Windows 10" — neither is a
        // capability concern for our purposes.
        return "Windows 10".to_string();
    }

    if user_agent.contains("Linux") || user_agent.contains("X11") {
        return "Linux".to_string();
    }

    String::new()
}

/// Simulcast capability *ceiling* (issue #989 / #1140 / #1141), derived from
/// cheap, stable device facts only: CPU `cores` and the UA `platform` token.
///
/// **The CPU microbenchmark (`videocall_capability_score()`) is deliberately
/// NOT a parameter** (issue #1140). It measured the wrong subsystem (scalar wasm
/// FP throughput, not the HW encode block that actually runs the layers) and its
/// run-to-run noise was wider than its old decision boundary, so it gated the
/// fleet on a coin flip. The score remains a log-once observability breadcrumb
/// (see `videocall-client/src/capability.rs`) but never reaches this decision.
///
/// Rules (a `ceiling`, not an operating point — see the module docs):
///
/// * **1 layer** (single stream, byte-identical to pre-simulcast) when the
///   device is marginal: `cores < 6` (this absorbs the old StrongWarn floor, and
///   the old Block floor of `cores < 4` / unknown cores `== 0` collapses into
///   it, since `cores == 0` < 6), OR an older Intel Mac (see
///   [`is_older_intel_mac`]).
/// * **3 layers** for a non-marginal device with `cores >= 10`
///   ([`CORES_FOR_3_LAYERS`]).
/// * **2 layers** for any other non-marginal device (`6 <= cores < 10`).
///
/// This is only the ceiling; the *operating point* starts at 1 layer for
/// everyone and is ramped up at runtime by the `videocall-aq` control loop
/// (issue #1141) based on observed encoder-queue backpressure headroom + uplink
/// budget. The effective ceiling the encoder is configured with is
/// `min(this, experimentalSimulcastMaxLayers runtime flag)` (flag defaults to 3,
/// #1082). Because the runtime loop realizes layers and sheds fast under
/// backpressure, a generous ceiling here is safe — a false-high core-count bet
/// is caught by the shed, never latched at load.
///
/// The only non-test caller is the wasm32-gated `capability_max_simulcast_layers`
/// below, so a native non-test build (e.g. `cargo clippy --all`) sees this as
/// dead code; the `allow` keeps that build warning-free without hiding genuine
/// dead code on wasm or in the host test build.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn max_simulcast_layers(cores: u32, platform: &str) -> u32 {
    // `cores == 0` means navigator.hardwareConcurrency was unavailable /
    // spoofed; treat unknown as marginal (`0 < 6`). `cores < 6` then also covers
    // the old hard-block floor of `cores < 4`.
    let marginal = cores < MIN_CORES_FOR_MULTILAYER || is_older_intel_mac(platform, cores);

    if marginal {
        return 1;
    }

    if cores >= CORES_FOR_3_LAYERS {
        3
    } else {
        // Non-marginal, 6 <= cores < 10.
        2
    }
}

/// Apply the **TEST-ONLY** capability-ceiling override (issue #1093) to a sniffed
/// ceiling.
///
/// `sniffed` is the device-derived ceiling from [`max_simulcast_layers`] (cores +
/// platform). `override_layers` is `Some(n)` only when `config.js` explicitly set
/// `testCapabilityMaxLayersOverride` (see
/// [`crate::constants::test_capability_max_layers_override`]); it is `None` in
/// every production / default-docker deployment. `ladder_depth` is the real
/// simulcast ladder depth (`SIMULCAST_MAX_LAYERS`, currently 3) — passed in rather
/// than referenced directly so this helper stays a pure function with no crate
/// dependency, host-testable without a browser.
///
/// Semantics:
///
/// * `override_layers == None` → return `sniffed` **unchanged**. This is the only
///   path taken when the key is absent, which is the only path that can run in
///   production (the key is never shipped). The sniffed ceiling is the source of
///   truth here.
/// * `override_layers == Some(n)` → the override **REPLACES** `sniffed`, clamped
///   into `[1, ladder_depth]`. A `0` (or any value below 1) becomes `1` — we never
///   return a `0` ceiling, since `min(flag, 0)` would silently disable all video
///   layers including the base stream. A value above `ladder_depth` is clamped
///   down so a bogus config can't request more layers than the ladder defines.
///
/// This is deliberately the ONLY place the override is interpreted, so the clamp
/// and 0-handling have a single host-tested definition. The caller
/// ([`capability_max_simulcast_layers`]) is responsible for emitting the `warn!`
/// when an override is active (it has the live context to log cores/platform); the
/// `Some` arm here is what makes that warning fire.
///
/// Like [`max_simulcast_layers`], the only non-test caller is the wasm32-gated
/// [`capability_max_simulcast_layers`], so the native non-test build sees this as
/// dead code; the `allow` keeps that build warning-free.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn apply_capability_override(
    sniffed: u32,
    override_layers: Option<u32>,
    ladder_depth: u32,
) -> u32 {
    match override_layers {
        // No override configured (the only production-reachable path): the sniffed
        // ceiling is authoritative.
        None => sniffed,
        // Override active: REPLACE the sniffed ceiling, clamped to a real layer
        // count. `clamp(1, ladder_depth)` maps 0 → 1 (never disable the base
        // stream) and any over-large value → ladder_depth.
        Some(n) => n.clamp(1, ladder_depth.max(1)),
    }
}

/// Whether [`capability_max_simulcast_layers`] can skip the ~100 ms CPU
/// microbenchmark **and** the navigator sniff entirely and just return `1`
/// (issue #1065).
///
/// This is a pure perf gate: it does NOT change the layer decision. It returns
/// `true` only in the cases where the wasm wrapper would have returned `1`
/// anyway, so the early return is behaviour-identical:
///
/// * `flag <= 1` — every call site combines the capability ceiling with the
///   runtime flag as `min(flag, capability)` (`host.rs` encoder setup + the
///   `send_layer_max` hook), so `min(flag<=1, _) == flag` and the effective
///   layer count floors at 1 no matter what the sniff would have produced. The
///   diagnostics summary stores `video_capability` standalone, but both
///   formatters (`performance_settings::format_simulcast_summary` /
///   `_compact`) take their `effective_video <= 1` branch — which never
///   references the capability number — whenever `flag <= 1`, so the unsniffed
///   `1` is never surfaced. `flag <= 1` is the production-OFF default's old
///   behaviour and the `pinSimulcastMaxLayers(ctx, 1)` e2e OFF path.
///
/// The guard additionally requires `override_layers.is_none()` so a TEST-ONLY
/// `testCapabilityMaxLayersOverride` (issue #1093) always takes effect. In
/// practice every e2e caller pairs the override with `experimentalSimulcastMaxLayers
/// = 3` (see `e2e/helpers/simulcast-config.ts` + `simulcast-per-receiver.spec.ts`:
/// the override is only ever passed through `enableSimulcastFlag(ctx, 3, …)`,
/// never alongside `pinSimulcastMaxLayers(ctx, 1)`), so `flag <= 1` already
/// implies no override today. Gating on the override too makes the skip robust
/// to a hypothetical future test that pairs an override with `flag <= 1`: the
/// override would still be honoured rather than silently swallowed by the skip.
///
/// The CPU score is log-only / health-field-only since issue #1140 — it has not
/// gated the layer count since then — so skipping its computation cannot change
/// any layer decision; at most it omits the observability breadcrumb in the
/// already-single-layer case.
///
/// Like [`max_simulcast_layers`] / [`apply_capability_override`], the only
/// non-test caller is the wasm32-gated [`capability_max_simulcast_layers`], so
/// the native non-test build sees this as dead code (`cargo clippy --all` does
/// not lint test targets); the `allow` keeps that build warning-free without
/// hiding genuine dead code on wasm or in the host tests.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn should_skip_capability_sniff(flag: u32, override_layers: Option<u32>) -> bool {
    flag <= 1 && override_layers.is_none()
}

// ---------------------------------------------------------------------------
// Browser-side wrapper. The simulcast ceiling needs the live navigator + the
// wasm-gated capability benchmark, so the real implementation is wasm32-only;
// the host build (used by `cargo test --lib`) gets a conservative stub.
// ---------------------------------------------------------------------------

/// The JS global the console-log collector (`console_log_collector.rs`) and the
/// health reporter (`videocall-client/src/health_reporter.rs`) both use for the
/// per-page CPU capability score. Keeping the name in one place keeps the
/// reader/writer here in lockstep with those two consumers.
#[cfg(target_arch = "wasm32")]
const CAPABILITY_SCORE_GLOBAL: &str = "__videocall_capability_score";

/// Read the cached per-page capability score from `window.__videocall_capability_score`.
///
/// Returns `Some(score)` only for a finite, strictly-positive value (mirroring
/// the validation in `health_reporter::read_client_metadata`, which treats
/// `<= 0` / non-finite as "absent" and omits `client_capability_score`). Returns
/// `None` on a cache miss — the global is unset, `undefined`, or not a usable
/// number — so the caller knows to fall back to a one-shot benchmark.
#[cfg(target_arch = "wasm32")]
fn read_cached_capability_score(window: &web_sys::Window) -> Option<u32> {
    use wasm_bindgen::JsValue;
    let val = js_sys::Reflect::get(window, &JsValue::from_str(CAPABILITY_SCORE_GLOBAL)).ok()?;
    let score = val.as_f64()?;
    if score.is_finite() && score > 0.0 {
        Some(score.min(u32::MAX as f64) as u32)
    } else {
        None
    }
}

/// Memoize a freshly-computed capability score into
/// `window.__videocall_capability_score` so subsequent readers (this function on
/// a later mount, plus `health_reporter::read_client_metadata`) reuse it instead
/// of re-benchmarking. A failed write is non-fatal — it only costs a future
/// re-benchmark — so the `Result` is intentionally ignored.
#[cfg(target_arch = "wasm32")]
fn write_cached_capability_score(window: &web_sys::Window, score: u32) {
    use wasm_bindgen::JsValue;
    let _ = js_sys::Reflect::set(
        window,
        &JsValue::from_str(CAPABILITY_SCORE_GLOBAL),
        &JsValue::from_f64(score as f64),
    );
}

/// Capability score for the observability breadcrumb + health field, reusing the
/// cached global when present (issue #1065).
///
/// On a cache HIT returns the cached value with NO benchmark — so a Host
/// re-mount reuses the score rather than re-benchmarking. On a cache MISS runs
/// the ~100 ms benchmark exactly ONCE (within this function) and writes it back,
/// so the `client_capability_score` health field gets a real (non-zero) value
/// even when console-log upload is disabled (the only other writer never runs).
///
/// Note: when console-log upload IS enabled, the collector's
/// `set_console_log_context` (`console_log_collector.rs`) ALSO runs its own
/// unconditional one-shot benchmark on connect — it does not consult this cache
/// first — so on the mount-before-connect race the benchmark can run twice per
/// page (once here on miss, once in the collector). That is unchanged by this
/// fix; the win is eliminating the PER-MOUNT re-benchmark, which is the #1065
/// hot path.
#[cfg(target_arch = "wasm32")]
fn cached_or_benchmark_capability_score(window: &web_sys::Window) -> u32 {
    if let Some(score) = read_cached_capability_score(window) {
        return score;
    }
    let score = videocall_client::capability::videocall_capability_score();
    write_cached_capability_score(window, score);
    score
}

/// Device-capability ceiling on simulcast layers (issue #989 / #1140), sniffed
/// from the live browser environment.
///
/// Sniffs `navigator.hardwareConcurrency` + UA platform exactly once, then
/// applies the pure [`max_simulcast_layers`] (cores + platform only). Returns a
/// conservative **1** if the browser globals are unreachable.
///
/// The CPU benchmark score is still *logged* here as an observability
/// breadcrumb (issue #1140 keeps it alive as a log-once signal + the
/// `client_capability_score` health field), but it is **NOT** passed to
/// [`max_simulcast_layers`] — it no longer gates the layer count. Do not
/// reintroduce a score threshold here or in any dashboard.
///
/// ## #1065: don't re-run the benchmark on every Host mount
///
/// Host mounts/remounts call this once each, and the benchmark is a synchronous
/// ~100 ms main-thread busy loop. Two changes keep that cost off the hot path
/// without altering the layer decision:
///
/// 1. When [`should_skip_capability_sniff`] is true (feature OFF — the
///    production default `experimentalSimulcastMaxLayers <= 1` — with no test
///    override), return `1` immediately, doing ZERO benchmark and ZERO navigator
///    work. This is behaviour-identical because every caller floors at
///    `min(flag<=1, _) == 1` (see the helper's doc for the per-call-site proof).
/// 2. Otherwise, READ the cached `window.__videocall_capability_score` (set by
///    the console-log collector's preamble path) instead of re-benchmarking, so
///    a Host re-mount reuses the score. On a cache MISS (Host's `use_hook`
///    commonly runs at mount *before* `set_console_log_context` fires from
///    `on_connection_established`, and the collector never sets it at all when
///    console-log upload is disabled), fall back to running the benchmark ONCE
///    and writing the result back into the global — so `capability_check` itself
///    benchmarks at most once per page, and the `client_capability_score` health
///    field still gets a real (non-zero) value in the upload-disabled case.
///    (The collector's own on-connect benchmark is unconditional, so the
///    upload-enabled path may still benchmark once there too; the #1065 win is
///    removing the per-mount re-benchmark, not deduping against the collector.)
///
/// This is only the capability ceiling; the *operating point* starts at 1 layer
/// and is ramped up at runtime by `videocall-aq` (issue #1141). The encoder is
/// configured with `min(this, experimentalSimulcastMaxLayers runtime flag)`, and
/// the flag now defaults to 3 (feature ON, #1082).
///
/// wasm32-only: it sniffs `web_sys` navigator and reads/refreshes the cached
/// CPU score (for the log + health field only, via
/// `videocall_client::capability::videocall_capability_score()` on a cache
/// miss), both of which are `#[cfg(target_arch = "wasm32")]`. The pure-logic
/// [`max_simulcast_layers`] / [`should_skip_capability_sniff`] above are
/// available on host for unit testing.
#[cfg(target_arch = "wasm32")]
pub fn capability_max_simulcast_layers() -> u32 {
    // #1065 short-circuit: when the feature is OFF (runtime flag <= 1) and no
    // test override is configured, the effective layer count is pinned to 1 at
    // every call site (`min(flag<=1, _) == 1`; the diagnostics summary's
    // `video_capability` is hidden by the formatters' `effective_video <= 1`
    // branch). Return 1 with ZERO benchmark + ZERO navigator work. The override
    // is read first (it is `None` in every production/default-docker config) so
    // an active e2e override always takes effect rather than being swallowed.
    let override_layers = crate::constants::test_capability_max_layers_override();
    let flag = crate::constants::experimental_simulcast_max_layers();
    if should_skip_capability_sniff(flag, override_layers) {
        return 1;
    }

    let Some(window) = web_sys::window() else {
        return 1;
    };
    let navigator = window.navigator();

    let cores_f64 = navigator.hardware_concurrency();
    let cores: u32 = if cores_f64.is_finite() && cores_f64 >= 1.0 {
        cores_f64.min(u32::MAX as f64) as u32
    } else {
        0
    };

    let user_agent = navigator.user_agent().unwrap_or_default();
    let platform = parse_platform_from_ua(&user_agent);

    // Observability breadcrumb ONLY (issue #1140): obtain + log the score, but do
    // not feed it to the layer decision. Keeping the read here documents at the
    // call site that the score is intentionally NOT a gate.
    //
    // #1065: reuse the cached `window.__videocall_capability_score` instead of
    // re-running the ~100 ms benchmark on every Host mount. On a cache miss
    // (Host raced ahead of `set_console_log_context`, or console-log upload is
    // disabled so the collector never set it) run the benchmark ONCE and memoize
    // it back into the global, so `capability_check` benchmarks at most once per
    // page (a re-mount reuses it) and the `client_capability_score` health field
    // still sees a real value.
    let score = cached_or_benchmark_capability_score(&window);
    let older_intel = is_older_intel_mac(&platform, cores);

    let sniffed = max_simulcast_layers(cores, &platform);

    // TEST-ONLY override (issue #1093): if `config.js` set
    // `testCapabilityMaxLayersOverride`, REPLACE the sniffed ceiling with the
    // clamped override so the containerized e2e runner (low core count → sniffed
    // == 1) can still exercise multi-layer SEND paths. The key is absent in every
    // production / default-docker `config.js`, so `test_capability_max_layers_override()`
    // returns `None` there and `apply_capability_override` is a pass-through.
    //
    // `SIMULCAST_MAX_LAYERS` is the real video ladder depth (the override is
    // clamped to it so a bogus config can't request more layers than exist).
    // `override_layers` was read once at the top of the function (before the
    // #1065 skip guard) so the guard could honour an active override; reuse it.
    let ladder_depth = videocall_client::adaptive_quality_constants::SIMULCAST_MAX_LAYERS as u32;
    let layers = apply_capability_override(sniffed, override_layers, ladder_depth);

    if let Some(requested) = override_layers {
        // WARN (never info): an active capability override is a test-only affordance
        // that must NEVER silently take effect in a real deployment. Surfacing it at
        // warn! means that if this key ever leaks into a production `config.js` it is
        // visible in the console / log pipeline rather than masquerading as a normal
        // capability decision (issue #1093).
        log::warn!(
            "simulcast capability ceiling is TEST-OVERRIDDEN to {layers} layer(s) \
             (requested testCapabilityMaxLayersOverride={requested}, clamped to \
             [1, {ladder_depth}]); the device-sniffed ceiling was {sniffed} \
             (cores={cores} platform={platform:?} older_intel={older_intel}). This is an \
             e2e-only hook (issue 1093) and MUST NOT be set in production config.js."
        );
    } else {
        log::info!(
            "simulcast capability ceiling: {layers} layer(s) (cores={cores} platform={platform:?} \
             older_intel={older_intel}); capability_score={score} (observability only — NOT a layer gate, issue 1140)"
        );
    }
    layers
}

/// Host-build stub of [`capability_max_simulcast_layers`]. The real
/// implementation is wasm32-only (it needs `web_sys` navigator + the
/// wasm-gated capability benchmark). On host — where `cargo test --lib`
/// compiles `host.rs` — there is no browser to sniff, so we return the
/// conservative single-layer ceiling. `host.rs` is browser-only at runtime, so
/// this stub is never actually exercised; it exists purely so the native test
/// build links.
#[cfg(not(target_arch = "wasm32"))]
pub fn capability_max_simulcast_layers() -> u32 {
    1
}

// ---------------------------------------------------------------------------
// Tests. Pure-logic only, runnable with `cargo test -p videocall-ui --lib`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- is_older_intel_mac ---------------------------------------------

    #[test]
    fn older_intel_mac_jason_profile() {
        // macOS 14, 2 cores (below block threshold but the helper itself
        // doesn't know about the block rule — only the platform/cores match).
        assert!(is_older_intel_mac("macOS 14", 2));
    }

    #[test]
    fn older_intel_mac_kent_profile() {
        // macOS 15, 6 cores → covered by the macOS 15 + cores <= 8 rule.
        assert!(is_older_intel_mac("macOS 15", 6));
    }

    #[test]
    fn not_older_intel_mac_tony_profile() {
        // macOS 26, 12 cores → no match.
        assert!(!is_older_intel_mac("macOS 26", 12));
    }

    #[test]
    fn macos_15_high_core_count_is_not_older_intel() {
        // 12 cores on macOS 15 is above the older-Intel ceiling.
        assert!(!is_older_intel_mac("macOS 15", 12));
    }

    #[test]
    fn macos_15_at_ceiling_is_older_intel() {
        // Exactly 8 cores hits the rule (cores <= 8).
        assert!(is_older_intel_mac("macOS 15", 8));
    }

    #[test]
    fn macos_14_high_core_count_still_older_intel() {
        // macOS 14 always trips the rule regardless of core count.
        assert!(is_older_intel_mac("macOS 14", 32));
    }

    #[test]
    fn windows_is_never_older_intel() {
        assert!(!is_older_intel_mac("Windows 10", 2));
        assert!(!is_older_intel_mac("Windows 10", 12));
    }

    #[test]
    fn linux_is_never_older_intel() {
        assert!(!is_older_intel_mac("Linux", 4));
    }

    #[test]
    fn unknown_platform_is_never_older_intel() {
        assert!(!is_older_intel_mac("", 4));
    }

    // --- parse_platform_from_ua -----------------------------------------

    #[test]
    fn parses_macos_safari_ua() {
        // Modern Safari freezes "Mac OS X 10_15_7"; we map to "macOS 10".
        let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
                  (KHTML, like Gecko) Version/18.5 Safari/605.1.15";
        assert_eq!(parse_platform_from_ua(ua), "macOS 10");
    }

    #[test]
    fn parses_macos_14_ua() {
        let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_2_1) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
        assert_eq!(parse_platform_from_ua(ua), "macOS 14");
    }

    #[test]
    fn parses_macos_15_ua() {
        let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_0_0) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
        assert_eq!(parse_platform_from_ua(ua), "macOS 15");
    }

    #[test]
    fn parses_macos_26_ua() {
        let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 26_0_0) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
        assert_eq!(parse_platform_from_ua(ua), "macOS 26");
    }

    #[test]
    fn parses_windows_ua() {
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
        assert_eq!(parse_platform_from_ua(ua), "Windows 10");
    }

    #[test]
    fn parses_linux_ua() {
        let ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
        assert_eq!(parse_platform_from_ua(ua), "Linux");
    }

    #[test]
    fn unknown_ua_returns_empty_platform() {
        assert_eq!(parse_platform_from_ua("Mozilla/5.0 (Unknown OS)"), "");
        assert_eq!(parse_platform_from_ua(""), "");
    }

    // --- max_simulcast_layers (issue #989 / #1140 / #1141) --------------
    //
    // The ceiling is a pure function of (cores, platform). The CPU benchmark
    // score was REMOVED as a parameter in #1140 (it measured the wrong
    // subsystem and gated on noise), so these boundary cases span only the
    // cores/platform branches. There is intentionally no score argument: a
    // reviewer mutating the function to re-add one would break compilation
    // here and at every call site (compile-level pin).

    #[test]
    fn simulcast_low_cores_is_one_layer() {
        // The old hard-block floor (cores < 4 / unknown) collapses into the
        // marginal `cores < 6` rule — both pin to 1.
        assert_eq!(max_simulcast_layers(0, "Windows 10"), 1);
        assert_eq!(max_simulcast_layers(2, "Windows 10"), 1);
        assert_eq!(max_simulcast_layers(3, "Windows 10"), 1);
    }

    #[test]
    fn simulcast_marginal_cores_is_one_layer() {
        // The old StrongWarn floor (4 <= cores < 6) also pins to 1.
        for cores in 4..MIN_CORES_FOR_MULTILAYER {
            assert_eq!(
                max_simulcast_layers(cores, "Windows 10"),
                1,
                "cores={cores} should pin to 1 layer"
            );
        }
    }

    #[test]
    fn simulcast_older_intel_is_one_layer() {
        // Older Intel Mac pins to 1 even with plenty of cores: macOS 14 always,
        // macOS 15 at/under the 8-core ceiling. (8 cores would otherwise be a
        // 2-layer device, so this proves the older-Intel rule overrides the
        // core bar.)
        assert_eq!(max_simulcast_layers(8, "macOS 14"), 1);
        assert_eq!(max_simulcast_layers(8, "macOS 15"), 1);
        // Even a high core count on macOS 14 is pinned (macOS 14 always trips
        // the rule), so the older-Intel guard beats the 3-layer core bar too.
        assert_eq!(max_simulcast_layers(12, "macOS 14"), 1);
    }

    #[test]
    fn simulcast_min_multilayer_cores_is_two_layers() {
        // Exactly MIN_CORES_FOR_MULTILAYER (6) on a non-marginal platform is the
        // lower boundary of the 2-layer band.
        assert_eq!(
            max_simulcast_layers(MIN_CORES_FOR_MULTILAYER, "Windows 10"),
            2
        );
        // Just below the 3-layer core bar stays at 2.
        assert_eq!(
            max_simulcast_layers(CORES_FOR_3_LAYERS - 1, "Windows 10"),
            2
        );
    }

    #[test]
    fn simulcast_high_core_count_is_three_layers() {
        // Lower boundary of the 3-layer band (inclusive) and above.
        assert_eq!(max_simulcast_layers(CORES_FOR_3_LAYERS, "Windows 10"), 3);
        assert_eq!(max_simulcast_layers(16, "Windows 10"), 3);
        // A modern (non-older-Intel) high-core Mac also gets 3.
        assert_eq!(max_simulcast_layers(12, "macOS 26"), 3);
    }

    #[test]
    fn simulcast_ceiling_is_stable_for_same_inputs() {
        // #1140: the same (cores, platform) must always yield the same ceiling
        // — the old benchmark made this flip run-to-run on noise. The ceiling
        // is now a pure function with no hidden input, so repeated calls are
        // identical by construction; this pins that contract.
        for _ in 0..100 {
            assert_eq!(max_simulcast_layers(8, "Windows 10"), 2);
            assert_eq!(max_simulcast_layers(12, "Windows 10"), 3);
            assert_eq!(max_simulcast_layers(4, "Windows 10"), 1);
        }
    }

    /// Issue #1082 / #1140: the device capability ceiling is the safety bound;
    /// the effective layer count the encoder is configured with is
    /// `min(flag, capability_ceiling)`. A weak device must end up at a ceiling
    /// of 1 even though the flag default is 3. (The runtime ramp then governs
    /// how many of the allowed layers are actually encoded — issue #1141.)
    #[test]
    fn default_on_still_gates_weak_device_to_one_layer() {
        const DEFAULT_FLAG: u32 = 3; // experimentalSimulcastMaxLayers default (issue 1082)

        // Unknown / very low cores (old Block floor) → ceiling 1 → effective 1.
        assert_eq!(
            DEFAULT_FLAG.min(max_simulcast_layers(0, "Windows 10")),
            1,
            "no-core device gated to 1"
        );

        // Marginal cores (old StrongWarn floor) → ceiling 1 → effective 1.
        assert_eq!(
            DEFAULT_FLAG.min(max_simulcast_layers(4, "Windows 10")),
            1,
            "marginal-core device gated to 1"
        );

        // Older Intel Mac (plenty of cores) → ceiling 1 → effective 1.
        assert_eq!(
            DEFAULT_FLAG.min(max_simulcast_layers(8, "macOS 14")),
            1,
            "older Intel Mac gated to 1"
        );

        // Mid device → ceiling 2 → effective 2 (default flag does not force 3).
        assert_eq!(
            DEFAULT_FLAG.min(max_simulcast_layers(8, "Windows 10")),
            2,
            "mid device runs 2 layers"
        );

        // Capable device → ceiling 3 → effective 3 (default-ON delivers full ladder).
        assert_eq!(
            DEFAULT_FLAG.min(max_simulcast_layers(12, "Windows 10")),
            3,
            "capable device runs 3 layers"
        );
    }

    // --- apply_capability_override (issue #1093, TEST-ONLY hook) ---------
    //
    // These pin the four behaviours the e2e override hook promises:
    //   1. absent override  → sniffed ceiling unchanged (the production path),
    //   2. present override → REPLACES the sniffed ceiling,
    //   3. over-ladder      → clamped DOWN to the ladder depth,
    //   4. zero             → clamped UP to 1 (never disable the base stream).
    //
    // The ladder depth used in the real call site is
    // `videocall_client::adaptive_quality_constants::SIMULCAST_MAX_LAYERS`. The
    // tests reference that SAME constant (not a literal `3`) so they track the real
    // source of truth — if the ladder ever grows to 4, the clamp tests follow it
    // automatically and a mutation that hardcodes the clamp bound would be caught.

    /// The real ladder depth the production call site clamps to. Referencing the
    /// crate constant (not a literal) keeps the override tests honest if the ladder
    /// size changes.
    const LADDER_DEPTH: u32 =
        videocall_client::adaptive_quality_constants::SIMULCAST_MAX_LAYERS as u32;

    #[test]
    fn override_absent_passes_through_sniffed_ceiling() {
        // The ONLY path reachable in production: no override key → return the
        // sniffed ceiling verbatim, for every possible sniffed value.
        for sniffed in 0..=LADDER_DEPTH + 2 {
            assert_eq!(
                apply_capability_override(sniffed, None, LADDER_DEPTH),
                sniffed,
                "absent override must not alter the sniffed ceiling {sniffed}"
            );
        }
    }

    #[test]
    fn override_present_replaces_sniffed_ceiling() {
        // The override REPLACES the sniffed value — it does not min/max/add to it.
        // Sniffed 1 (the containerized-runner reality) + override 3 must yield 3,
        // which is the whole point of the hook. Prove replacement (not min) by also
        // checking the override can RAISE above the sniffed value.
        assert_eq!(
            apply_capability_override(1, Some(3), LADDER_DEPTH),
            3,
            "override 3 must raise a sniffed-1 ceiling to 3 (replace, not min)"
        );
        assert_eq!(
            apply_capability_override(1, Some(2), LADDER_DEPTH),
            2,
            "override 2 replaces sniffed 1"
        );
        // And it can LOWER below the sniffed value too (pure replacement).
        assert_eq!(
            apply_capability_override(3, Some(1), LADDER_DEPTH),
            1,
            "override 1 replaces sniffed 3 (replace, not max)"
        );
    }

    #[test]
    fn override_above_ladder_depth_is_clamped_down() {
        // A bogus config can't request more layers than the ladder defines.
        assert_eq!(
            apply_capability_override(1, Some(LADDER_DEPTH + 1), LADDER_DEPTH),
            LADDER_DEPTH,
            "override above ladder depth clamps to ladder depth"
        );
        assert_eq!(
            apply_capability_override(1, Some(99), LADDER_DEPTH),
            LADDER_DEPTH,
            "absurd override clamps to ladder depth"
        );
        // Exactly at the ladder depth is allowed unchanged.
        assert_eq!(
            apply_capability_override(1, Some(LADDER_DEPTH), LADDER_DEPTH),
            LADDER_DEPTH,
            "override exactly at ladder depth is the max allowed"
        );
    }

    #[test]
    fn override_zero_is_clamped_to_one() {
        // A `0` override must NEVER produce a 0 ceiling: `min(flag, 0)` would
        // silently disable every video layer including the base stream. Treat 0 as
        // "force single layer" (decided/documented in the helper + config doc).
        assert_eq!(
            apply_capability_override(2, Some(0), LADDER_DEPTH),
            1,
            "override 0 clamps UP to 1 (never disable the base stream)"
        );
    }

    #[test]
    fn override_one_forces_single_layer_even_on_capable_sniff() {
        // Symmetric to the e2e force-multi case: an override of 1 must pin a capable
        // device (sniffed 3) down to a single layer — useful for an OFF-path test
        // that wants the single-stream behaviour on a beefy CI runner.
        assert_eq!(
            apply_capability_override(3, Some(1), LADDER_DEPTH),
            1,
            "override 1 forces single layer regardless of sniffed capability"
        );
    }

    // --- should_skip_capability_sniff (issue #1065) ----------------------

    #[test]
    fn skip_sniff_when_flag_off_and_no_override() {
        // Feature OFF (flag 0 or 1) with no test override → the effective layer
        // count is pinned to 1 at every call site, so the wasm wrapper can skip
        // the ~100 ms benchmark + navigator sniff and just return 1.
        assert!(
            should_skip_capability_sniff(0, None),
            "flag 0, no override must skip"
        );
        assert!(
            should_skip_capability_sniff(1, None),
            "flag 1, no override must skip"
        );
    }

    #[test]
    fn do_not_skip_sniff_when_feature_on() {
        // Feature ON (flag >= 2) → the sniffed ceiling can matter (it is combined
        // via min(flag, capability)), so the sniff must run. This is the
        // mutation-killer for flipping `<=` to `<`: with `flag < 1` the boundary
        // flag value 1 would WRONGLY stop skipping and run the benchmark again.
        assert!(
            !should_skip_capability_sniff(2, None),
            "flag 2 must NOT skip — capability ceiling is live"
        );
        assert!(
            !should_skip_capability_sniff(3, None),
            "flag 3 (production default) must NOT skip"
        );
    }

    #[test]
    fn flag_one_is_the_skip_boundary() {
        // Pin the `<=` boundary explicitly. If the predicate is mutated to `<`,
        // flag == 1 would return false here and this assertion fails — proving the
        // test is wired to the real boundary, not a tautology.
        assert!(
            should_skip_capability_sniff(1, None),
            "flag == 1 is the inclusive OFF boundary and MUST skip"
        );
        assert!(
            !should_skip_capability_sniff(2, None),
            "flag == 2 is the first ON value and MUST NOT skip"
        );
    }

    #[test]
    fn do_not_skip_sniff_when_override_present_even_if_flag_off() {
        // An active #1093 test override must ALWAYS reach
        // `apply_capability_override`, so the skip guard must NOT fire while an
        // override is configured — even on the flag <= 1 boundary. This is the
        // mutation-killer for dropping the `&& override_layers.is_none()` clause:
        // without it these would WRONGLY skip and swallow the override.
        assert!(
            !should_skip_capability_sniff(0, Some(3)),
            "flag 0 with override must NOT skip — override must take effect"
        );
        assert!(
            !should_skip_capability_sniff(1, Some(3)),
            "flag 1 with override must NOT skip — override must take effect"
        );
        // And of course an override with the feature on also runs the sniff path.
        assert!(
            !should_skip_capability_sniff(3, Some(3)),
            "flag 3 with override must NOT skip"
        );
    }
}

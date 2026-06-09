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

// ---------------------------------------------------------------------------
// Browser-side wrapper. The simulcast ceiling needs the live navigator + the
// wasm-gated capability benchmark, so the real implementation is wasm32-only;
// the host build (used by `cargo test --lib`) gets a conservative stub.
// ---------------------------------------------------------------------------

/// Device-capability ceiling on simulcast layers (issue #989 / #1140), sniffed
/// from the live browser environment.
///
/// Sniffs `navigator.hardwareConcurrency` + UA platform exactly once, then
/// applies the pure [`max_simulcast_layers`] (cores + platform only). Returns a
/// conservative **1** if the browser globals are unreachable.
///
/// The CPU benchmark `videocall_capability_score()` is still *read and logged*
/// here as an observability breadcrumb (issue #1140 keeps it alive as a
/// log-once signal + the `client_capability_score` health field), but it is
/// **NOT** passed to [`max_simulcast_layers`] — it no longer gates the layer
/// count. Do not reintroduce a score threshold here or in any dashboard.
///
/// This is only the capability ceiling; the *operating point* starts at 1 layer
/// and is ramped up at runtime by `videocall-aq` (issue #1141). The encoder is
/// configured with `min(this, experimentalSimulcastMaxLayers runtime flag)`, and
/// the flag now defaults to 3 (feature ON, #1082).
///
/// wasm32-only: it sniffs `web_sys` navigator and calls
/// `videocall_client::capability::videocall_capability_score()` (for the log
/// only), which is itself `#[cfg(target_arch = "wasm32")]`. The pure-logic
/// [`max_simulcast_layers`] above is available on host for unit testing.
#[cfg(target_arch = "wasm32")]
pub fn capability_max_simulcast_layers() -> u32 {
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

    // Observability breadcrumb ONLY (issue #1140): read + log the score, but do
    // not feed it to the layer decision. Keeping the read here documents at the
    // call site that the score is intentionally NOT a gate.
    let score = videocall_client::capability::videocall_capability_score();
    let older_intel = is_older_intel_mac(&platform, cores);

    let layers = max_simulcast_layers(cores, &platform);
    log::info!(
        "simulcast capability ceiling: {layers} layer(s) (cores={cores} platform={platform:?} \
         older_intel={older_intel}); capability_score={score} (observability only — NOT a layer gate, issue #1140)"
    );
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
}

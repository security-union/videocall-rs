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

//! Simulcast layer-capability ceiling (issue #989 / #1082).
//!
//! This module is **only** a publisher-side encode-CPU budget: it derives the
//! maximum number of simulcast layers a device should encode, given its CPU
//! core count, UA platform, and the synthetic capability benchmark. Encoding N
//! layers is ~N× the encode CPU, so a weak publisher that overcommits to 3
//! layers would stall its own main thread (the failure mode discussion #890 /
//! #562 documents on 2-core Intel Macs).
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

/// Simulcast capability ceiling (issue #989), derived from raw device inputs.
///
/// Maps a device's CPU `cores`, UA `platform` token, its
/// `videocall_capability_score()` (a synthetic per-device benchmark; higher =
/// faster — see `videocall-client/src/capability.rs`), to the maximum number of
/// simulcast layers the publisher should encode.
///
/// Encoding N layers is ~N× the encode CPU, so this is deliberately
/// conservative — a weak publisher that overcommits to 3 layers would stall its
/// own main thread (the exact failure mode discussion #890 / #562 documents on
/// 2-core Intel Macs). Rules:
///
/// * **1 layer** (single stream, byte-identical to pre-simulcast) when the
///   device is at all marginal: `cores < 6` (this absorbs the old StrongWarn
///   floor, and the old Block floor of `cores < 4` / unknown cores `== 0`
///   collapses into it), OR an older Intel Mac (see [`is_older_intel_mac`]), OR
///   a benchmark `score < 5000`.
/// * **2 layers** for a non-marginal device with `5000 <= score < 30000`.
/// * **3 layers** for a non-marginal device with `score >= 30000`.
///
/// Note this is only the *capability ceiling*. The effective layer count the
/// encoder uses is `min(this, experimentalSimulcastMaxLayers runtime flag)`,
/// and the flag now defaults to 3 (feature ON, #1082) — so a high-end device
/// emits up to 3 layers by default while a weak device is still gated down to 1
/// (or 2) here regardless of the flag.
///
/// The only non-test caller is the wasm32-gated `capability_max_simulcast_layers`
/// below, so a native non-test build (e.g. `cargo clippy --all`) sees this as
/// dead code; the `allow` keeps that build warning-free without hiding genuine
/// dead code on wasm or in the host test build.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn max_simulcast_layers(cores: u32, platform: &str, score: u32) -> u32 {
    const SCORE_FOR_2_LAYERS: u32 = 5000;
    const SCORE_FOR_3_LAYERS: u32 = 30000;

    // `cores == 0` means navigator.hardwareConcurrency was unavailable /
    // spoofed; treat unknown as marginal. `cores < 6` then also covers the old
    // hard-block floor of `cores < 4`.
    let marginal = cores < MIN_CORES_FOR_MULTILAYER
        || is_older_intel_mac(platform, cores)
        || score < SCORE_FOR_2_LAYERS;

    if marginal {
        return 1;
    }

    if score >= SCORE_FOR_3_LAYERS {
        3
    } else {
        // Not marginal, 5000 <= score < 30000.
        2
    }
}

// ---------------------------------------------------------------------------
// Browser-side wrapper. The simulcast ceiling needs the live navigator + the
// wasm-gated capability benchmark, so the real implementation is wasm32-only;
// the host build (used by `cargo test --lib`) gets a conservative stub.
// ---------------------------------------------------------------------------

/// Device-capability ceiling on simulcast layers (issue #989), sniffed from
/// the live browser environment.
///
/// Sniffs `navigator.hardwareConcurrency` + UA platform exactly once, reads the
/// publisher CPU benchmark via
/// `videocall_client::capability::videocall_capability_score()`, then applies
/// [`max_simulcast_layers`]. Returns a conservative **1** if the browser
/// globals are unreachable.
///
/// This is only the capability ceiling; the encoder uses
/// `min(this, experimentalSimulcastMaxLayers runtime flag)`, and the flag now
/// defaults to 3 (feature ON, #1082) — this ceiling is the weak-device safety floor.
///
/// wasm32-only: it sniffs `web_sys` navigator and calls
/// `videocall_client::capability::videocall_capability_score()`, which is
/// itself `#[cfg(target_arch = "wasm32")]`. The pure-logic
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

    let score = videocall_client::capability::videocall_capability_score();
    let older_intel = is_older_intel_mac(&platform, cores);

    let layers = max_simulcast_layers(cores, &platform, score);
    log::info!(
        "simulcast capability ceiling: {layers} layer(s) (cores={cores} platform={platform:?} score={score} older_intel={older_intel})"
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

    // --- max_simulcast_layers (issue #989) ------------------------------
    //
    // Boundary cases spanning every branch and both score thresholds. Inputs
    // are now raw (cores, platform, score) rather than a precomputed verdict.

    #[test]
    fn simulcast_low_cores_is_one_layer() {
        // The old hard-block floor (cores < 4 / unknown) collapses into the
        // marginal `cores < 6` rule — both pin to 1 even with a huge score.
        assert_eq!(max_simulcast_layers(0, "Windows 10", 99_999), 1);
        assert_eq!(max_simulcast_layers(2, "Windows 10", 99_999), 1);
        assert_eq!(max_simulcast_layers(3, "Windows 10", 99_999), 1);
    }

    #[test]
    fn simulcast_marginal_cores_is_one_layer() {
        // The old StrongWarn floor (4 <= cores < 6) also pins to 1.
        for cores in 4..MIN_CORES_FOR_MULTILAYER {
            assert_eq!(
                max_simulcast_layers(cores, "Windows 10", 99_999),
                1,
                "cores={cores} should pin to 1 layer"
            );
        }
    }

    #[test]
    fn simulcast_older_intel_is_one_layer() {
        // Older Intel Mac pins to 1 even with plenty of cores + a high score.
        // macOS 14 always, macOS 15 at/under the 8-core ceiling.
        assert_eq!(max_simulcast_layers(8, "macOS 14", 99_999), 1);
        assert_eq!(max_simulcast_layers(8, "macOS 15", 99_999), 1);
    }

    #[test]
    fn simulcast_low_score_is_one_layer() {
        // Capable cores but score just below the 2-layer threshold → 1.
        assert_eq!(max_simulcast_layers(8, "Windows 10", 4999), 1);
    }

    #[test]
    fn simulcast_mid_score_is_two_layers() {
        // Lower boundary of the 2-layer band (inclusive).
        assert_eq!(max_simulcast_layers(8, "Windows 10", 5000), 2);
        // Just below the 3-layer threshold stays at 2.
        assert_eq!(max_simulcast_layers(8, "Windows 10", 29_999), 2);
    }

    #[test]
    fn simulcast_high_score_is_three_layers() {
        // Lower boundary of the 3-layer band (inclusive).
        assert_eq!(max_simulcast_layers(8, "Windows 10", 30_000), 3);
    }

    /// Issue #1082: with simulcast now ON BY DEFAULT (flag = 3), the device
    /// capability ceiling is the safety floor for weak devices. The effective
    /// layer count the encoder uses is `min(flag, capability_ceiling)`, so a weak
    /// device must still end up at 1 layer even though the flag default is 3.
    /// This models that `min` at the host call site without a browser.
    #[test]
    fn default_on_still_gates_weak_device_to_one_layer() {
        const DEFAULT_FLAG: u32 = 3; // experimentalSimulcastMaxLayers default (issue 1082)

        // Unknown / very low cores (old Block floor) → ceiling 1 → effective 1.
        let weak_block = max_simulcast_layers(0, "Windows 10", 0);
        assert_eq!(DEFAULT_FLAG.min(weak_block), 1, "no-core device gated to 1");

        // Marginal cores (old StrongWarn floor) → ceiling 1 → effective 1.
        let weak_warn = max_simulcast_layers(4, "Windows 10", 0);
        assert_eq!(
            DEFAULT_FLAG.min(weak_warn),
            1,
            "marginal-core device gated to 1"
        );

        // Older Intel Mac (plenty of cores + high score) → ceiling 1 → effective 1.
        let older_intel = max_simulcast_layers(8, "macOS 14", 99_999);
        assert_eq!(
            DEFAULT_FLAG.min(older_intel),
            1,
            "older Intel Mac gated to 1"
        );

        // Low-benchmark capable device → ceiling 1 → effective 1.
        let low_score = max_simulcast_layers(8, "Windows 10", 4999);
        assert_eq!(
            DEFAULT_FLAG.min(low_score),
            1,
            "low-score device gated to 1"
        );

        // Mid device → ceiling 2 → effective 2 (default flag does not force 3).
        let mid = max_simulcast_layers(8, "Windows 10", 5000);
        assert_eq!(DEFAULT_FLAG.min(mid), 2, "mid device runs 2 layers");

        // Capable device → ceiling 3 → effective 3 (default-ON delivers full ladder).
        let strong = max_simulcast_layers(8, "Windows 10", 30_000);
        assert_eq!(DEFAULT_FLAG.min(strong), 3, "capable device runs 3 layers");
    }
}

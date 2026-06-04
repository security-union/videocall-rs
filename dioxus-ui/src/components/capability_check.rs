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

//! Pre-join capability check — Phase 9 (Jay's UX-1 + UX-2 action items).
//!
//! Before the user attempts to attach media and connect transports, we sniff
//! `navigator.hardwareConcurrency` and the User-Agent platform string to
//! decide whether the device is likely to survive a group meeting at all.
//! Underpowered hosts (notably 2-core / older Intel MacBooks) historically
//! hit catastrophic CPU stalls in cc7tp; warning ahead of join is the
//! cheapest mitigation.
//!
//! The pure-logic core lives in [`assess_from_inputs`] / [`is_older_intel_mac`]
//! / [`parse_platform_from_ua`] so it can be unit-tested on host without a
//! browser. [`assess_capability`] is the wasm32 wrapper that sources real
//! navigator data.
//!
//! See discussion 562 (Phase 9).
//!
//! ## Verdict semantics
//!
//! - [`CapabilityVerdict::Block`] — fewer than 4 logical cores. Almost
//!   certainly going to be unusable in any group meeting; the join button
//!   is disabled and an explanation is rendered in place of the lobby card.
//! - [`CapabilityVerdict::StrongWarn`] — fewer than 6 cores, OR an older
//!   Intel Mac (`macOS 14*` always; `macOS 15*` only when cores <= 8).
//!   The user can still join, but a prominent modal must be acknowledged
//!   first.
//! - [`CapabilityVerdict::SoftWarn`] — reserved for future use; at present
//!   we never construct this variant. Kept in the public enum so call sites
//!   can match exhaustively without requiring a future breaking change.
//! - [`CapabilityVerdict::Ok`] — no concerns. Logged at info level.

/// Outcome of a pre-join capability assessment.
///
/// The associated [`String`] on [`Block`](Self::Block),
/// [`StrongWarn`](Self::StrongWarn), and [`SoftWarn`](Self::SoftWarn)
/// is the user-facing copy to render alongside the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityVerdict {
    /// Hard-block: render the explanation, leave the join button disabled.
    Block(String),
    /// Soft-block: render a prominent warning, but allow the user to
    /// proceed once they have explicitly acknowledged it.
    StrongWarn(String),
    /// Reserved for future telemetry-only notices. Never constructed by
    /// [`assess_from_inputs`] today; the `#[allow(dead_code)]` is deliberate
    /// — keeping the variant in the public enum lets call sites keep an
    /// exhaustive `match` without us shipping a future breaking change.
    #[allow(dead_code)]
    SoftWarn(String),
    /// No concerns.
    Ok,
}

/// Below this core count we hard-block joining a group meeting.
///
/// Anchored in the cc7tp post-mortem: Jason's 2-core Intel MacBook
/// hit a catastrophic main-thread stall well before re-election even fired.
const MIN_CORES_BLOCK: u32 = 4;

/// At or below this core count we strong-warn but still allow join.
const MIN_CORES_STRONG_WARN: u32 = 6;

/// Cap for the `macOS 15*` strong-warn rule. Above this, modern Intel /
/// Apple-Silicon Macs are typically fine for group calls.
const OLDER_INTEL_MAC_15_CORE_CEILING: u32 = 8;

/// Pure-logic core: assess capability from already-extracted inputs.
///
/// `cores` should be `navigator.hardwareConcurrency` cast to `u32`
/// (with `0` treated as "unknown" — we conservatively treat unknown
/// as a block).
///
/// `platform` should be the platform token produced by
/// [`parse_platform_from_ua`] (e.g. `"macOS 14"`, `"Windows 10"`,
/// `"Linux"`, or `""` if unknown).
pub fn assess_from_inputs(cores: u32, platform: &str) -> CapabilityVerdict {
    if cores == 0 {
        // navigator.hardwareConcurrency unavailable / spoofed to 0.
        // Be conservative — block — rather than silently letting a
        // potentially toaster-grade device into a group meeting.
        return CapabilityVerdict::Block(
            "We couldn't detect your device's CPU capability. Group video meetings require at \
             least 4 CPU cores to run smoothly. Please join from a different device."
                .to_string(),
        );
    }

    if cores < MIN_CORES_BLOCK {
        return CapabilityVerdict::Block(format!(
            "Your device has only {cores} CPU core{plural}. Group video meetings need at least \
             4 cores to run smoothly. Please join from a different device.",
            plural = if cores == 1 { "" } else { "s" }
        ));
    }

    if cores < MIN_CORES_STRONG_WARN {
        return CapabilityVerdict::StrongWarn(format!(
            "Your device has limited CPU resources ({cores} cores). Meeting performance may \
             degrade. Consider joining audio-only or from a more powerful device."
        ));
    }

    if is_older_intel_mac(platform, cores) {
        return CapabilityVerdict::StrongWarn(
            "Older Intel Macs may struggle with large meetings. Consider audio-only mode."
                .to_string(),
        );
    }

    CapabilityVerdict::Ok
}

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

/// Simulcast capability gate (issue #989).
///
/// Maps a device's [`CapabilityVerdict`], its `videocall_capability_score()`
/// (a synthetic per-device benchmark; higher = faster — see
/// `videocall-client/src/capability.rs`), and the `older_intel` heuristic to
/// the maximum number of simulcast layers the publisher should encode.
///
/// Encoding N layers is ~N× the encode CPU, so this is deliberately
/// conservative — a weak publisher that overcommits to 3 layers would stall its
/// own main thread (the exact failure mode discussion #890 / #562 documents on
/// 2-core Intel Macs). Rules:
///
/// * **1 layer** (single stream, byte-identical to pre-simulcast) when the
///   device is at all marginal: a [`Block`](CapabilityVerdict::Block) or
///   [`StrongWarn`](CapabilityVerdict::StrongWarn) verdict, OR an older Intel
///   Mac, OR a benchmark `score < 5000`.
/// * **2 layers** for an [`Ok`](CapabilityVerdict::Ok) device with
///   `5000 <= score < 30000`.
/// * **3 layers** for an [`Ok`](CapabilityVerdict::Ok) device with
///   `score >= 30000`.
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
pub fn max_simulcast_layers(verdict: &CapabilityVerdict, score: u32, older_intel: bool) -> u32 {
    const SCORE_FOR_2_LAYERS: u32 = 5000;
    const SCORE_FOR_3_LAYERS: u32 = 30000;

    let marginal = matches!(
        verdict,
        CapabilityVerdict::Block(_) | CapabilityVerdict::StrongWarn(_)
    ) || older_intel
        || score < SCORE_FOR_2_LAYERS;

    if marginal {
        return 1;
    }

    if score >= SCORE_FOR_3_LAYERS {
        3
    } else {
        // Ok verdict, not older-intel, 5000 <= score < 30000.
        2
    }
}

// ---------------------------------------------------------------------------
// Browser-side wrapper. Compiled for both host and wasm32 — the lib itself
// targets host for `cargo test --lib`, and `web_sys::window()` returning
// `None` on host is the safe path that exits with a Block verdict.
// ---------------------------------------------------------------------------

/// Sniff `navigator.hardwareConcurrency` and the UA platform token, then
/// run [`assess_from_inputs`].
///
/// Falls back to [`CapabilityVerdict::Block`] if the browser globals are
/// unreachable (which would be deeply unusual at this point in the lifecycle —
/// we're already inside a Dioxus render).
pub fn assess_capability() -> CapabilityVerdict {
    let Some(window) = web_sys::window() else {
        return CapabilityVerdict::Block(
            "We couldn't access your browser environment. Please refresh and try again."
                .to_string(),
        );
    };
    let navigator = window.navigator();

    // hardware_concurrency() returns f64; clamp to u32 and treat negatives /
    // NaN as 0 (== "unknown" in assess_from_inputs).
    let cores_f64 = navigator.hardware_concurrency();
    let cores: u32 = if cores_f64.is_finite() && cores_f64 >= 1.0 {
        cores_f64.min(u32::MAX as f64) as u32
    } else {
        0
    };

    let user_agent = navigator.user_agent().unwrap_or_default();
    let platform = parse_platform_from_ua(&user_agent);

    let verdict = assess_from_inputs(cores, &platform);

    // Always log the assessment: even on Ok we want a single info line in
    // the console log collector so we can correlate later quality reports.
    match &verdict {
        CapabilityVerdict::Block(msg) => {
            log::warn!("capability-check: BLOCK cores={cores} platform={platform:?} reason={msg}");
        }
        CapabilityVerdict::StrongWarn(msg) => {
            log::warn!(
                "capability-check: STRONG_WARN cores={cores} platform={platform:?} reason={msg}"
            );
        }
        CapabilityVerdict::SoftWarn(msg) => {
            log::info!(
                "capability-check: SOFT_WARN cores={cores} platform={platform:?} reason={msg}"
            );
        }
        CapabilityVerdict::Ok => {
            log::info!("capability-check: OK cores={cores} platform={platform:?}");
        }
    }

    verdict
}

/// Device-capability ceiling on simulcast layers (issue #989), sniffed from
/// the live browser environment.
///
/// Sniffs `navigator.hardwareConcurrency` + UA platform exactly once, derives
/// the [`CapabilityVerdict`] and the `older_intel` heuristic from them, reads
/// the publisher CPU benchmark via
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

    let verdict = assess_from_inputs(cores, &platform);
    let older_intel = is_older_intel_mac(&platform, cores);
    let score = videocall_client::capability::videocall_capability_score();

    let layers = max_simulcast_layers(&verdict, score, older_intel);
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

    // --- assess_from_inputs ---------------------------------------------

    #[test]
    fn cores_zero_is_blocked() {
        match assess_from_inputs(0, "macOS 26") {
            CapabilityVerdict::Block(msg) => {
                assert!(
                    msg.to_lowercase().contains("couldn't detect"),
                    "expected unknown-cores wording, got: {msg}"
                );
            }
            other => panic!("expected Block for cores=0, got {other:?}"),
        }
    }

    #[test]
    fn cores_below_block_threshold_blocks_with_count() {
        for cores in 1..MIN_CORES_BLOCK {
            match assess_from_inputs(cores, "Windows 10") {
                CapabilityVerdict::Block(msg) => {
                    assert!(
                        msg.contains(&cores.to_string()),
                        "block message should mention core count, got: {msg}"
                    );
                }
                other => panic!("expected Block for cores={cores}, got {other:?}"),
            }
        }
    }

    #[test]
    fn cores_at_block_threshold_does_not_block() {
        // 4 cores is the floor; should not block.
        assert!(!matches!(
            assess_from_inputs(MIN_CORES_BLOCK, "Windows 10"),
            CapabilityVerdict::Block(_)
        ));
    }

    #[test]
    fn cores_below_strong_warn_threshold_strong_warns() {
        for cores in MIN_CORES_BLOCK..MIN_CORES_STRONG_WARN {
            match assess_from_inputs(cores, "Windows 10") {
                CapabilityVerdict::StrongWarn(msg) => {
                    assert!(
                        msg.to_lowercase().contains("limited cpu"),
                        "expected limited-cpu wording, got: {msg}"
                    );
                }
                other => panic!("expected StrongWarn for cores={cores}, got {other:?}"),
            }
        }
    }

    #[test]
    fn cores_at_strong_warn_threshold_on_modern_platform_is_ok() {
        // 6 cores on modern platform clears every rule.
        assert_eq!(
            assess_from_inputs(MIN_CORES_STRONG_WARN, "Windows 10"),
            CapabilityVerdict::Ok
        );
    }

    #[test]
    fn jason_macos_14_two_cores_is_blocked() {
        // Below 4 cores wins regardless of the older-Intel-Mac rule.
        match assess_from_inputs(2, "macOS 14") {
            CapabilityVerdict::Block(msg) => {
                assert!(
                    msg.contains('2'),
                    "block message should mention 2 cores: {msg}"
                );
            }
            other => panic!("expected Block for Jason's profile, got {other:?}"),
        }
    }

    #[test]
    fn macos_14_with_enough_cores_strong_warns() {
        // 8 cores avoids both core-based rules but trips the older-Intel-Mac rule.
        match assess_from_inputs(8, "macOS 14") {
            CapabilityVerdict::StrongWarn(msg) => {
                assert!(
                    msg.to_lowercase().contains("intel"),
                    "expected older-Intel-Mac wording, got: {msg}"
                );
            }
            other => panic!("expected StrongWarn for macOS 14 / 8 cores, got {other:?}"),
        }
    }

    #[test]
    fn kent_macos_15_six_cores_strong_warns() {
        // Kent's profile per the Phase 9 spec.
        match assess_from_inputs(6, "macOS 15") {
            CapabilityVerdict::StrongWarn(msg) => {
                assert!(
                    msg.to_lowercase().contains("intel"),
                    "expected older-Intel-Mac wording, got: {msg}"
                );
            }
            other => panic!("expected StrongWarn for Kent's profile, got {other:?}"),
        }
    }

    #[test]
    fn tony_macos_26_twelve_cores_is_ok() {
        // Tony's profile per the Phase 9 spec.
        assert_eq!(assess_from_inputs(12, "macOS 26"), CapabilityVerdict::Ok);
    }

    #[test]
    fn modern_apple_silicon_high_core_macos_15_is_ok() {
        // 12 cores on macOS 15 is above the older-Intel ceiling.
        assert_eq!(assess_from_inputs(12, "macOS 15"), CapabilityVerdict::Ok);
    }

    #[test]
    fn windows_10_with_8_cores_is_ok() {
        assert_eq!(assess_from_inputs(8, "Windows 10"), CapabilityVerdict::Ok);
    }

    #[test]
    fn linux_with_4_cores_strong_warns_but_not_for_intel_mac() {
        match assess_from_inputs(4, "Linux") {
            CapabilityVerdict::StrongWarn(msg) => {
                // Should be the limited-cpu wording, not the older-Intel-Mac wording.
                assert!(msg.to_lowercase().contains("limited cpu"));
                assert!(!msg.to_lowercase().contains("intel"));
            }
            other => panic!("expected StrongWarn for Linux / 4 cores, got {other:?}"),
        }
    }

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
    // Six boundary cases spanning every branch and both score thresholds.

    #[test]
    fn simulcast_block_verdict_is_one_layer() {
        // A Block verdict pins to 1 even with an otherwise-huge score.
        assert_eq!(
            max_simulcast_layers(&CapabilityVerdict::Block("too weak".into()), 99_999, false),
            1
        );
    }

    #[test]
    fn simulcast_strong_warn_verdict_is_one_layer() {
        // StrongWarn (e.g. cores < 6) pins to 1 regardless of score.
        assert_eq!(
            max_simulcast_layers(
                &CapabilityVerdict::StrongWarn("limited cpu".into()),
                99_999,
                false
            ),
            1
        );
    }

    #[test]
    fn simulcast_older_intel_is_one_layer() {
        // Older Intel Mac pins to 1 even on an Ok verdict + high score.
        assert_eq!(
            max_simulcast_layers(&CapabilityVerdict::Ok, 99_999, true),
            1
        );
    }

    #[test]
    fn simulcast_low_score_ok_is_one_layer() {
        // Ok verdict but score just below the 2-layer threshold → 1.
        assert_eq!(max_simulcast_layers(&CapabilityVerdict::Ok, 4999, false), 1);
    }

    #[test]
    fn simulcast_mid_score_ok_is_two_layers() {
        // Lower boundary of the 2-layer band (inclusive).
        assert_eq!(max_simulcast_layers(&CapabilityVerdict::Ok, 5000, false), 2);
        // Just below the 3-layer threshold stays at 2.
        assert_eq!(
            max_simulcast_layers(&CapabilityVerdict::Ok, 29_999, false),
            2
        );
    }

    #[test]
    fn simulcast_high_score_ok_is_three_layers() {
        // Lower boundary of the 3-layer band (inclusive).
        assert_eq!(
            max_simulcast_layers(&CapabilityVerdict::Ok, 30_000, false),
            3
        );
    }

    /// Issue #1082: with simulcast now ON BY DEFAULT (flag = 3), the device
    /// capability ceiling is the safety floor for weak devices. The effective
    /// layer count the encoder uses is `min(flag, capability_ceiling)`, so a weak
    /// device must still end up at 1 layer even though the flag default is 3.
    /// This models that `min` at the host call site without a browser.
    #[test]
    fn default_on_still_gates_weak_device_to_one_layer() {
        const DEFAULT_FLAG: u32 = 3; // experimentalSimulcastMaxLayers default (#1082)

        // Block verdict → ceiling 1 → effective 1.
        let weak_block =
            max_simulcast_layers(&CapabilityVerdict::Block("too weak".into()), 0, false);
        assert_eq!(DEFAULT_FLAG.min(weak_block), 1, "Block device gated to 1");

        // StrongWarn verdict → ceiling 1 → effective 1.
        let weak_warn = max_simulcast_layers(
            &CapabilityVerdict::StrongWarn("limited cpu".into()),
            0,
            false,
        );
        assert_eq!(
            DEFAULT_FLAG.min(weak_warn),
            1,
            "StrongWarn device gated to 1"
        );

        // Older Intel Mac (Ok verdict but flagged) → ceiling 1 → effective 1.
        let older_intel = max_simulcast_layers(&CapabilityVerdict::Ok, 99_999, true);
        assert_eq!(
            DEFAULT_FLAG.min(older_intel),
            1,
            "older Intel Mac gated to 1"
        );

        // Low-benchmark Ok device → ceiling 1 → effective 1.
        let low_score = max_simulcast_layers(&CapabilityVerdict::Ok, 4999, false);
        assert_eq!(
            DEFAULT_FLAG.min(low_score),
            1,
            "low-score device gated to 1"
        );

        // Mid device → ceiling 2 → effective 2 (default flag does not force 3).
        let mid = max_simulcast_layers(&CapabilityVerdict::Ok, 5000, false);
        assert_eq!(DEFAULT_FLAG.min(mid), 2, "mid device runs 2 layers");

        // Capable device → ceiling 3 → effective 3 (default-ON delivers full ladder).
        let strong = max_simulcast_layers(&CapabilityVerdict::Ok, 30_000, false);
        assert_eq!(DEFAULT_FLAG.min(strong), 3, "capable device runs 3 layers");
    }
}

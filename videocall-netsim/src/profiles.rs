//! Named network-impairment presets.
//!
//! These are the human-friendly handles exposed to both the bot's
//! configuration files (`network: <profile>`) and the CLI overrides
//! (`--impair-all` / `--impair-name` in the Rust bot; `network:
//! <profile>` in the bots-app browser bot's meeting-config YAML — see
//! discussion #793). All numeric values are round-number plausibility
//! sketches, not measurements — they are intended to give a load test
//! a repeatable shape, not to reproduce any specific carrier.
//!
//! Convention reminder: total one-way delay is drawn from
//! `[latency_ms, latency_ms + 2 * jitter_ms]`. See [`crate::shim`].

use crate::shim::NetworkProfile;

/// Names of built-in presets, in the order they should appear in CLI
/// help.
pub const PRESET_NAMES: &[&str] = &[
    "none",
    "good_wifi",
    "good_4g",
    "congested_wifi",
    "lossy_mobile",
    "satellite",
    "dialup",
    "crushed_downlink",
];

/// Expose the preset names in a stable order (for `--help`, error
/// messages, etc.).
pub fn list_profiles() -> &'static [&'static str] {
    PRESET_NAMES
}

/// Resolve a preset name to its profile, or `None` if unknown.
pub fn resolve_profile(name: &str) -> Option<NetworkProfile> {
    match name {
        "none" => Some(NetworkProfile::passthrough()),
        "good_wifi" => Some(NetworkProfile {
            latency_ms: 20,
            jitter_ms: 5,
            loss_pct: 0.1,
            uplink_kbps: Some(20_000),
            downlink_kbps: Some(50_000),
            ..Default::default()
        }),
        "good_4g" => Some(NetworkProfile {
            latency_ms: 50,
            jitter_ms: 15,
            loss_pct: 0.5,
            uplink_kbps: Some(10_000),
            downlink_kbps: Some(30_000),
            ..Default::default()
        }),
        "congested_wifi" => Some(NetworkProfile {
            latency_ms: 80,
            jitter_ms: 30,
            loss_pct: 2.0,
            uplink_kbps: Some(2_000),
            downlink_kbps: Some(4_000),
            ..Default::default()
        }),
        "lossy_mobile" => Some(NetworkProfile {
            latency_ms: 150,
            jitter_ms: 50,
            loss_pct: 5.0,
            uplink_kbps: Some(800),
            downlink_kbps: Some(2_000),
            ..Default::default()
        }),
        "satellite" => Some(NetworkProfile {
            latency_ms: 600,
            jitter_ms: 50,
            loss_pct: 1.0,
            uplink_kbps: Some(1_500),
            downlink_kbps: Some(10_000),
            ..Default::default()
        }),
        "dialup" => Some(NetworkProfile {
            latency_ms: 200,
            jitter_ms: 40,
            loss_pct: 3.0,
            uplink_kbps: Some(56),
            downlink_kbps: Some(56),
            ..Default::default()
        }),
        // Dedicated impairment for the issue #1080 e2e per-receiver
        // simulcast-divergence test. The receive-side layer chooser steps
        // DOWN on `loss_per_sec >= 5.0` (sequence gaps), NOT on raw
        // bandwidth — and the shim's token bucket models a bandwidth
        // shortfall as `Admission::Delay`, which the inbound shaping path
        // maps to Pass (see `netsim_hook::shape_inbound`). So a tiny
        // `downlink_kbps` alone would NOT produce the deterministic LOSS
        // the test requires. This preset therefore drives step-down with a
        // high Bernoulli `loss_pct` (40%): at a single video stream's
        // typical packet rate (tens of pps) that clears >5 dropped
        // packets/sec within a couple of seconds, reliably pushing the
        // degraded receiver's chooser below a healthy co-receiver's. The
        // `downlink_kbps` cap is included for realism but is not the
        // step-down driver. Latency/jitter are kept modest so the loss
        // signal — not delay — dominates.
        "crushed_downlink" => Some(NetworkProfile {
            latency_ms: 40,
            jitter_ms: 10,
            loss_pct: 40.0,
            downlink_kbps: Some(500),
            ..Default::default()
        }),
        _ => None,
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn all_presets_resolve() {
        for name in list_profiles() {
            let p =
                resolve_profile(name).unwrap_or_else(|| panic!("preset {} should resolve", name));
            p.validate()
                .unwrap_or_else(|e| panic!("preset {} failed validation: {}", name, e));
        }
    }

    #[test]
    fn none_preset_is_passthrough() {
        assert!(resolve_profile("none").unwrap().is_passthrough());
    }

    #[test]
    fn unknown_preset_is_none() {
        assert!(resolve_profile("not-a-real-profile").is_none());
    }

    /// Issue #1080: `crushed_downlink` must resolve, validate, and carry a
    /// loss rate high enough to drive the receive-side chooser's
    /// `loss_per_sec >= 5.0` step-down within a few seconds at a single
    /// video stream's packet rate. The chooser does NOT react to bandwidth
    /// (the shim models that as Delay, which the inbound path passes), so a
    /// meaningful `loss_pct` — not just `downlink_kbps` — is the contract
    /// this test pins. Asserting `>= 20.0` would still fail loudly if a
    /// future edit dropped the loss to a token value while leaving only the
    /// (insufficient) bandwidth cap.
    #[test]
    fn crushed_downlink_has_step_down_grade_loss() {
        let p = resolve_profile("crushed_downlink").expect("crushed_downlink must resolve");
        p.validate().expect("crushed_downlink must validate");
        assert!(
            p.loss_pct >= 20.0,
            "crushed_downlink loss_pct={} too low to drive chooser step-down via loss",
            p.loss_pct
        );
        assert!(
            !p.is_passthrough(),
            "crushed_downlink must not be passthrough"
        );
    }
}

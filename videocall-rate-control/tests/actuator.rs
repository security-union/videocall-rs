//! Tier choice, encoder bitrate and frame decimation: as pure functions,
//! then in the closed loop.

mod support;

use support::{run, SimConfig, RESTORE_MS, STEP_MS};
use videocall_rate_control::{
    choose_tier, default_tiers, encoder_bitrate_bps, FrameDecision, FrameGate, Tier,
    MIN_DRAIN_FRACTION,
};

const CAP_BPS: f64 = 1_500_000.0;
/// Probe step of the baseline publisher: 33.3 ms over a 600 ms cycle.
const P: f64 = (1000.0 / 30.0) / 600.0;

fn tiers() -> Vec<Tier> {
    default_tiers(CAP_BPS)
}

#[test]
fn default_table_is_the_measured_ladder() {
    let tiers = tiers();
    let table: Vec<(&str, u32, u32, f64, f64, f64)> = tiers
        .iter()
        .map(|t| {
            (
                t.label.as_str(),
                t.width,
                t.height,
                t.fps,
                t.lo_bps,
                t.hi_bps,
            )
        })
        .collect();
    assert_eq!(
        table,
        vec![
            ("720p30", 1280, 720, 30.0, 960_000.0, CAP_BPS),
            ("480p30", 854, 480, 30.0, 600_000.0, CAP_BPS),
            ("360p30", 640, 360, 30.0, 360_000.0, CAP_BPS),
            ("360p15", 640, 360, 15.0, 270_000.0, CAP_BPS),
        ]
    );
    // A cap below a tier's floor drops that tier; the bottom one stays.
    let labels =
        |cap: f64| -> Vec<String> { default_tiers(cap).into_iter().map(|t| t.label).collect() };
    assert_eq!(labels(936_000.0), ["480p30", "360p30", "360p15"]);
    assert_eq!(labels(100_000.0), ["360p15"]);
}

#[test]
fn tier_down_goes_to_the_highest_tier_the_capacity_affords() {
    let tiers = tiers();
    assert_eq!(choose_tier(&tiers, 0, 959_999.0, P), 1);
    assert_eq!(choose_tier(&tiers, 0, 500_000.0, P), 2);
    assert_eq!(choose_tier(&tiers, 0, 300_000.0, P), 3);
    // Below every floor: the bottom tier.
    assert_eq!(choose_tier(&tiers, 0, 100_000.0, P), 3);
    assert_eq!(choose_tier(&tiers, 3, 100_000.0, P), 3);
}

#[test]
fn tier_up_one_step_at_one_probe_step_above_the_upper_floor() {
    let tiers = tiers();
    let edge = 960_000.0 * (1.0 + P);
    assert_eq!(choose_tier(&tiers, 1, edge - 1.0, P), 1);
    assert_eq!(choose_tier(&tiers, 1, edge, P), 0);
    // A big capacity still moves one step per report.
    assert_eq!(choose_tier(&tiers, 3, 10_000_000.0, P), 2);
}

/// The probe ripple: capacity moving within `[lo, lo * (1 + p))` of the
/// tier above keeps the tier where it is, both ways.
#[test]
fn capacity_ripple_inside_the_band_does_not_flap() {
    let tiers = tiers();
    let lo = tiers[0].lo_bps;
    let mut tier = 1;
    for step in 0..200 {
        let ripple = lo * (1.0 + P * (f64::from(step % 10) / 10.0));
        tier = choose_tier(&tiers, tier, ripple, P);
        assert_eq!(tier, 1, "moved at step {step}, capacity {ripple}");
    }
    // Once up, the same ripple keeps it up.
    let mut tier = 0;
    for step in 0..200 {
        let ripple = lo * (1.0 + P * (f64::from(step % 10) / 10.0));
        tier = choose_tier(&tiers, tier, ripple, P);
        assert_eq!(tier, 0, "moved at step {step}, capacity {ripple}");
    }
}

#[test]
fn encoder_bitrate_is_the_target_clamped_to_the_tier() {
    let tier = &tiers()[1];
    assert_eq!(encoder_bitrate_bps(tier, 300_000.0), 600_000.0);
    assert_eq!(encoder_bitrate_bps(tier, 800_000.0), 800_000.0);
    assert_eq!(encoder_bitrate_bps(tier, 9_000_000.0), CAP_BPS);
}

const CAMERA_MS: f64 = 1000.0 / 30.0;

/// A 30 fps camera's decisions, with exact timestamps.
fn handed(gate: &mut FrameGate, tier: &Tier, target_bps: f64, frames: usize) -> Vec<bool> {
    (0..frames)
        .map(|i| gate.admit(tier, i as f64 * CAMERA_MS, target_bps) == FrameDecision::Encode)
        .collect()
}

#[test]
fn no_decimation_at_or_above_the_floor() {
    let tiers = tiers();
    let mut gate = FrameGate::new();
    assert!(handed(&mut gate, &tiers[1], 600_000.0, 90)
        .iter()
        .all(|h| *h));
}

#[test]
fn drain_decimation_hands_target_over_floor_evenly_spaced() {
    let tier = &tiers()[1];
    let mut gate = FrameGate::new();
    // Half the floor: every other frame.
    let half = handed(&mut gate, tier, 300_000.0, 100);
    assert_eq!(half.iter().filter(|h| **h).count(), 50);
    assert!(half.windows(2).all(|w| w[0] != w[1]), "not alternating");
    // 75% of the floor: 75 of 100, never more than 2 apart.
    let mut gate = FrameGate::new();
    let some = handed(&mut gate, tier, 450_000.0, 100);
    assert_eq!(some.iter().filter(|h| **h).count(), 75);
    let positions: Vec<usize> = (0..100).filter(|i| some[*i]).collect();
    assert!(positions
        .windows(2)
        .all(|w| (1..=2).contains(&(w[1] - w[0]))));
}

/// However low the target (a hostile "1 kbps" report, a hopeless viewer),
/// the gate hands at least `MIN_DRAIN_FRACTION` of the frames: the room's
/// camera never freezes.
#[test]
fn drain_decimation_never_drops_below_the_drain_fraction() {
    let tier = &tiers()[3];
    for target in [0.0, 1_000.0, 50_000.0] {
        let mut gate = FrameGate::new();
        let kept = handed(&mut gate, tier, target, 120)
            .into_iter()
            .filter(|h| *h)
            .count();
        // 15 fps tier: 60 frames at its rate, half of them.
        assert_eq!(
            kept,
            (60.0 * MIN_DRAIN_FRACTION) as usize,
            "target {target}"
        );
    }
}

#[test]
fn a_15_fps_tier_takes_every_other_camera_frame_before_any_drain() {
    let bottom = &tiers()[3];
    let mut gate = FrameGate::new();
    let decisions: Vec<FrameDecision> = (0..60)
        .map(|i| gate.admit(bottom, i as f64 * CAMERA_MS, 270_000.0))
        .collect();
    let encoded = decisions
        .iter()
        .filter(|d| **d == FrameDecision::Encode)
        .count();
    assert_eq!(encoded, 30);
    // At half the floor, half of those.
    let mut gate = FrameGate::new();
    let decisions: Vec<FrameDecision> = (0..120)
        .map(|i| gate.admit(bottom, i as f64 * CAMERA_MS, 135_000.0))
        .collect();
    let count = |want: FrameDecision| decisions.iter().filter(|d| **d == want).count();
    assert_eq!(count(FrameDecision::Encode), 30);
    assert_eq!(count(FrameDecision::SkipTierRate), 60);
    assert_eq!(count(FrameDecision::SkipDrain), 30);
}

/// The tier rate follows the frames' timestamps, not a claimed camera rate:
/// a webcam that really delivers 15 fps keeps all of them on a 15 fps tier,
/// and a jittery 30 fps camera still gives 15.
#[test]
fn tier_rate_follows_real_frame_timestamps() {
    let bottom = &tiers()[3];
    let mut gate = FrameGate::new();
    let slow = (0..60)
        .filter(|i| {
            gate.admit(bottom, f64::from(*i) * 2.0 * CAMERA_MS, 270_000.0) == FrameDecision::Encode
        })
        .count();
    assert_eq!(slow, 60, "a real 15 fps camera halved again");
    let mut gate = FrameGate::new();
    let jitter = [0.0, 4.0, -3.0, 6.0, -5.0, 2.0];
    let kept = (0..300)
        .map(|i| i as f64 * CAMERA_MS + jitter[i % jitter.len()])
        .filter(|ts| gate.admit(bottom, *ts, 270_000.0) == FrameDecision::Encode)
        .count();
    assert!(
        (148..=152).contains(&kept),
        "jittery 30 fps on a 15 fps tier kept {kept} of 300"
    );
}

/// A cap below the bottom tier's 270 kbps (a user bitrate setting) lowers
/// that tier's floor to the cap: the capped target is not a drain, so
/// nothing beyond the tier's own frame rate is skipped.
#[test]
fn a_cap_below_the_bottom_floor_never_decimates() {
    let tiers = default_tiers(200_000.0);
    assert_eq!(tiers.len(), 1);
    assert_eq!(tiers[0].lo_bps, 200_000.0);
    let mut gate = FrameGate::new();
    let kept = handed(&mut gate, &tiers[0], 200_000.0, 120);
    // 15 fps tier on a 30 fps camera: every other frame, no drain skips.
    assert!(kept.iter().enumerate().all(|(i, k)| *k == (i % 2 == 0)));
    assert_eq!(encoder_bitrate_bps(&tiers[0], 200_000.0), 200_000.0);
    // A custom table with hi below lo behaves the same.
    let custom = Tier {
        hi_bps: 150_000.0,
        ..tiers[0].clone()
    };
    assert_eq!(custom.floor_bps(), 150_000.0);
    let mut gate = FrameGate::new();
    let kept = handed(&mut gate, &custom, 150_000.0, 120)
        .iter()
        .filter(|k| **k)
        .count();
    assert_eq!(kept, 60);
}

fn tiered_baseline() -> SimConfig {
    SimConfig {
        max_target_bps: CAP_BPS,
        tiers: Some(default_tiers(CAP_BPS)),
        ..SimConfig::baseline()
    }
}

/// The baseline step with the full actuator: tiers follow capacity, the
/// probe ripple never flaps them, and latency and throughput hold.
#[test]
fn closed_loop_tiers_follow_capacity_without_flapping() {
    let config = tiered_baseline();
    let result = run(&config);
    let changes = &result.tier_changes;
    // Settled phases: no tier change from 5 s after start, after the step,
    // or after the restore until the next event.
    let changes_in = |from: f64, to: f64| {
        changes
            .iter()
            .filter(|(t, _)| *t >= from && *t < to)
            .count()
    };
    assert_eq!(changes_in(5_000.0, STEP_MS), 0, "{changes:?}");
    assert_eq!(changes_in(STEP_MS + 3_000.0, RESTORE_MS), 0, "{changes:?}");
    assert_eq!(
        changes_in(RESTORE_MS + 15_000.0, config.duration_ms),
        0,
        "{changes:?}"
    );
    // On the narrow link the tier is the one 500 kbps affords.
    let tier_at = |t_ms: f64| {
        changes
            .iter()
            .take_while(|(t, _)| *t <= t_ms)
            .last()
            .map_or(0, |(_, tier)| *tier)
    };
    assert_eq!(tier_at(RESTORE_MS - 1.0), 2, "{changes:?}");
    // Back on top after the restore.
    assert_eq!(tier_at(config.duration_ms), 0, "{changes:?}");
    let metrics = result.metrics();
    assert!(metrics.b2_peak_lateness_ms < 1_000.0, "{metrics:?}");
    assert!(metrics.b3_delivered_kbps >= 400.0, "{metrics:?}");
}

/// Decimation carries the drains: frames are skipped only while the target
/// is below the tier's floor, and the skipped frames take no sequence
/// number (the viewer never sees a gap).
#[test]
fn closed_loop_decimation_only_during_drains() {
    let result = run(&tiered_baseline());
    let drains: Vec<f64> = result
        .skipped
        .iter()
        .filter(|(_, why)| *why == FrameDecision::SkipDrain)
        .map(|(t, _)| *t)
        .collect();
    assert!(!drains.is_empty(), "the step needs a drain");
    let tiers = default_tiers(CAP_BPS);
    for t in &drains {
        let tier = result
            .tier_changes
            .iter()
            .take_while(|(at, _)| at <= t)
            .last()
            .map_or(0, |(_, tier)| *tier);
        assert!(
            result.target_at(*t) < tiers[tier].lo_bps,
            "skipped at {t} ms with the target at the floor"
        );
    }
    assert!(result.frames.windows(2).all(|w| w[1].seq == w[0].seq + 1));
}

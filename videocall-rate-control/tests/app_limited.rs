//! An encoder that sends less than it is allowed to (easy content: a still
//! scene, a person sitting in front of a webcam). With no queue anywhere,
//! what the viewer measures is only a lower bound on the path; it must not
//! pull the target or the tier down. Only a queue can.

mod support;

use support::{run, SimConfig};
use videocall_rate_control::default_tiers;

const CAP_BPS: f64 = 1_500_000.0;

fn app_limited(link_bps: f64, fill: Vec<(f64, f64)>) -> SimConfig {
    SimConfig {
        duration_ms: 60_000.0,
        max_target_bps: CAP_BPS,
        start_target_bps: CAP_BPS,
        schedule: vec![(0.0, link_bps)],
        tiers: Some(default_tiers(CAP_BPS)),
        encoder_fill: fill,
        ..SimConfig::baseline()
    }
}

fn tier_at(changes: &[(f64, usize)], t_ms: f64) -> usize {
    changes
        .iter()
        .take_while(|(t, _)| *t <= t_ms)
        .last()
        .map_or(0, |(_, tier)| *tier)
}

/// The encoder makes a quarter of its bitrate on a 5 Mbps link. Nothing
/// ever queues, so the target stays at the cap and the tier at the top.
#[test]
fn an_undershooting_encoder_on_a_wide_link_keeps_the_top() {
    let config = app_limited(5_000_000.0, vec![(0.0, 0.25)]);
    let result = run(&config);
    assert_eq!(result.backlog_reports(0, 0.0, f64::INFINITY), 0);
    let min = result.min_target(0.0, config.duration_ms);
    assert!(
        min >= CAP_BPS * 0.999,
        "target fell to {:.0} kbps",
        min / 1000.0
    );
    assert_eq!(result.tier_changes, vec![(0.0, 0)], "tier moved");
}

/// The same source through a 500 kbps link. A quarter of 1.5 Mbps fits on
/// average, but its keyframes queue on a link this narrow (the deltas keep
/// it 75% busy, so a keyframe's queue outlives what its own bytes explain):
/// that is a real queue, and it may set the capacity to the link. What must
/// hold: the target never goes below what the link carries except to drain
/// a measured queue, and after a scene change at 20 s that fills the
/// encoder, the queue is caught and the rate settles near the link.
#[test]
fn an_undershooting_encoder_that_fills_a_narrow_link_is_caught_by_the_queue() {
    let scene_change_ms = 20_000.0;
    let config = app_limited(500_000.0, vec![(0.0, 0.25), (scene_change_ms, 1.0)]);
    let result = run(&config);
    // Anything below the link before the scene change is a drain of a
    // reported backlog, never a lower bound read as a ceiling.
    for sent in result
        .reports
        .iter()
        .filter(|sent| sent.t_ms < scene_change_ms)
    {
        if let Some(bps) = sent.report.capacity_bps {
            assert!(bps >= 450_000, "a ceiling below the link: {sent:?}");
        }
    }
    assert!(result.backlog_reports(0, scene_change_ms, f64::INFINITY) > 0);
    let peak = result.peak_lateness_ms(0, scene_change_ms, f64::INFINITY);
    assert!(
        peak < 1_000.0,
        "peak lateness {peak:.0} ms after the scene change"
    );
    let delivered = result.delivered_kbps(scene_change_ms + 10_000.0, config.duration_ms);
    assert!(delivered >= 400.0, "delivered {delivered:.0} kbps of 500");
    // 500 kbps affords 640x360@30 (from 360 kbps) and not 854x480 (600).
    assert_eq!(tier_at(&result.tier_changes, config.duration_ms), 2);
}

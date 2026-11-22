//! The relay's one-bit CONGESTION signal in the closed loop: viewers that
//! never report (native clients, older web clients) behind a relay that
//! drops when their queue is full and tells the publisher.

mod support;

use support::{run, SimConfig, RESTORE_MS, STEP_MS};

/// Video share of the relay's 256-message outbound channel, frames: the
/// channel also carries about 50 audio packets per second against 30
/// video frames.
const RELAY_QUEUE_FRAMES: usize = 96;

/// A silent viewer behind a 300 kbps link: without the signal the publisher
/// would sit at its 936 kbps cap forever. With it the rate settles near the
/// link.
#[test]
fn a_silent_viewer_behind_a_slow_link_pulls_the_rate_near_the_link() {
    let config = SimConfig {
        duration_ms: 120_000.0,
        schedule: vec![(0.0, 300_000.0)],
        relay_queue_frames: Some(RELAY_QUEUE_FRAMES),
        silent_viewers: vec![0],
        ..SimConfig::baseline()
    };
    let result = run(&config);
    assert!(result.signals.iter().any(|(_, _, accepted)| *accepted));
    let from = 60_000.0;
    let mean_kbps = {
        let mut edges: Vec<f64> = vec![from];
        edges.extend(
            result
                .target_changes
                .iter()
                .map(|(t, _)| *t)
                .filter(|t| *t > from),
        );
        edges.push(config.duration_ms);
        let bits: f64 = edges
            .windows(2)
            .map(|w| result.target_at(w[0]) * (w[1] - w[0]))
            .sum();
        bits / (config.duration_ms - from) / 1000.0
    };
    assert!(
        (150.0..=450.0).contains(&mean_kbps),
        "mean target {mean_kbps:.0} kbps behind a 300 kbps link"
    );
    let min = result.metrics().min_target_kbps;
    assert!(min >= 135.0, "below the drain floor: {min} kbps");
}

/// Signals blamed on a viewer that reports are ignored, so the limit never
/// fights its reports: the baseline step with a small relay queue that
/// drops at the step gives the same targets whether the relay signals or
/// not.
#[test]
fn signals_from_a_reporting_viewer_never_cut_the_rate() {
    let config = SimConfig {
        relay_queue_frames: Some(10),
        ..SimConfig::baseline()
    };
    let result = run(&config);
    assert!(
        !result.signals.is_empty(),
        "the scenario must make the relay signal"
    );
    assert!(
        result.signals.iter().all(|(_, _, accepted)| !accepted),
        "{:?}",
        result.signals
    );
    let metrics = result.metrics();
    assert!(metrics.b2_peak_lateness_ms < 1_000.0, "{metrics:?}");
    assert!(
        metrics.b4_recovery_ms.is_some_and(|ms| ms <= 15_000.0),
        "{metrics:?}"
    );
}

/// A reporting viewer on a fast link and a silent one stepping down: the
/// silent viewer's signals still protect it, and once its link is restored
/// the limit grows away and the target recovers.
#[test]
fn a_silent_slow_viewer_is_protected_next_to_a_reporting_fast_one() {
    let config = SimConfig {
        duration_ms: 70_000.0,
        schedule: vec![(0.0, 2_000_000.0)],
        other_viewers: vec![vec![
            (0.0, 2_000_000.0),
            (STEP_MS, 300_000.0),
            (RESTORE_MS, 2_000_000.0),
        ]],
        relay_queue_frames: Some(RELAY_QUEUE_FRAMES),
        silent_viewers: vec![1],
        ..SimConfig::baseline()
    };
    let result = run(&config);
    assert!(result
        .signals
        .iter()
        .any(|(_, viewer, accepted)| *viewer == 1 && *accepted));
    let narrow = result.min_target(STEP_MS, RESTORE_MS) / 1000.0;
    assert!(
        narrow < 450.0,
        "never came down for the silent viewer: {narrow} kbps"
    );
    let after = result.min_target(RESTORE_MS + 30_000.0, config.duration_ms) / 1000.0;
    assert!(
        after >= support::B4_TARGET_KBPS,
        "no recovery: {after} kbps"
    );
}

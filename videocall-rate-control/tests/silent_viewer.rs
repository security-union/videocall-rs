//! A viewer that has reported and then goes quiet while frames keep going
//! out is itself a congestion signal: its frames are not being released.

mod support;

use support::{run, SimConfig};

/// The link drops to 250 kbps at 10 s and the viewer, stuck behind the
/// queue, sends nothing until 40 s, when the link is back at 2 Mbps. The
/// silence cuts the rate once (one halving stands for one missing report)
/// and the rate recovers afterwards.
#[test]
fn a_viewer_that_goes_quiet_under_overload_brings_the_rate_down() {
    let config = SimConfig {
        duration_ms: 70_000.0,
        schedule: vec![
            (0.0, 2_000_000.0),
            (10_000.0, 250_000.0),
            (40_000.0, 2_000_000.0),
        ],
        muted_reports: vec![(0, 10_000.0, 40_000.0)],
        ..SimConfig::baseline()
    };
    let result = run(&config);
    let before = result.min_target(0.0, 10_000.0) / 1000.0;
    assert!(
        before >= support::B1_TARGET_KBPS,
        "cut before the quiet: {before}"
    );
    // One halving for the episode, not one per silence span.
    assert_eq!(result.silence_cuts.len(), 1, "{:?}", result.silence_cuts);
    let quiet = result.min_target(10_000.0, 40_000.0) / 1000.0;
    assert!(
        quiet <= before * 0.5 * 1.01,
        "still at {quiet:.0} kbps while the viewer is quiet"
    );
    let back = result.min_target(55_000.0, config.duration_ms) / 1000.0;
    assert!(
        back >= support::B4_TARGET_KBPS,
        "not recovered: {back:.0} kbps"
    );
}

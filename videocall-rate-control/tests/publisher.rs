//! The drain/probe/hold policy and the multi-viewer rule, as pure functions.

use videocall_rate_control::{
    combine_reports, drain_target_bps, CongestionLimit, MultiViewerPublisher, Publisher,
    PublisherConfig, Report, MIN_DRAIN_FRACTION,
};

const RTT_MS: f64 = 100.0;
/// The default bottom tier's floor.
const FLOOR_BPS: f64 = 270_000.0;
/// Fresh while `age <= T + I` = 600 + 500 ms.
const FRESH_MS: f64 = 1_100.0;

fn config() -> PublisherConfig {
    PublisherConfig {
        start_bps: 936_000.0,
        max_bps: 1_500_000.0,
        frame_interval_ms: 1000.0 / 30.0,
        report_interval_ms: 500.0,
        floor_bps: FLOOR_BPS,
    }
}

fn report(capacity_bps: u64, backlog_ms: f64) -> Report {
    Report {
        capacity_bps,
        backlog_ms,
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

#[test]
fn cycle_probe_and_freshness_follow_the_inputs() {
    let config = config();
    assert!(close(config.cycle_ms(RTT_MS), 600.0));
    assert!(close(
        config.probe_fraction(RTT_MS),
        (1000.0 / 30.0) / 600.0
    ));
    assert!(close(config.fresh_ms(RTT_MS), 1_100.0));
    // A longer round trip probes more gently.
    assert!(config.probe_fraction(300.0) < config.probe_fraction(RTT_MS));
}

#[test]
fn start_rate_before_any_report() {
    let publisher = Publisher::new(config());
    assert!(close(publisher.target_bps(0.0, RTT_MS), 936_000.0));
}

#[test]
fn probe_on_a_fresh_clean_report_hold_on_a_stale_one() {
    let config = config();
    let clean = report(800_000, 0.0);
    let probe = 800_000.0 * (1.0 + config.probe_fraction(RTT_MS));
    assert!(close(drain_target_bps(&config, clean, 0.0, RTT_MS), probe));
    assert!(close(
        drain_target_bps(&config, clean, 1_100.0, RTT_MS),
        probe
    ));
    assert!(close(
        drain_target_bps(&config, clean, 1_101.0, RTT_MS),
        800_000.0
    ));
}

#[test]
fn drain_depth_follows_the_backlog_then_holds() {
    let config = config();
    // 150 ms in a 600 ms cycle: 75% of capacity for one cycle.
    let small = report(500_000, 150.0);
    assert!(close(
        drain_target_bps(&config, small, 0.0, RTT_MS),
        375_000.0
    ));
    assert!(close(
        drain_target_bps(&config, small, 599.0, RTT_MS),
        375_000.0
    ));
    assert!(close(
        drain_target_bps(&config, small, 600.0, RTT_MS),
        500_000.0
    ));
    // 900 ms: floored at half, for 900 / (1 - 0.5) = 1,800 ms, but only
    // while the report is fresh (1,100 ms); the next report, due every
    // 500 ms, carries on.
    let big = report(500_000, 900.0);
    let floor = 500_000.0 * MIN_DRAIN_FRACTION;
    assert!(close(
        drain_target_bps(&config, big, FRESH_MS, RTT_MS),
        floor
    ));
    assert!(close(
        drain_target_bps(&config, big, FRESH_MS + 1.0, RTT_MS),
        500_000.0
    ));
    assert!(close(
        drain_target_bps(&config, big, 1_800.0, RTT_MS),
        500_000.0
    ));
}

#[test]
fn never_above_the_cap() {
    let config = config();
    let target = drain_target_bps(&config, report(5_000_000, 0.0), 0.0, RTT_MS);
    assert!(close(target, 1_500_000.0));
}

#[test]
fn multi_viewer_rule_takes_lowest_capacity_and_largest_backlog() {
    let combined = combine_reports(
        [
            (100.0, report(2_000_000, 0.0)),
            (300.0, report(500_000, 50.0)),
            (200.0, report(900_000, 200.0)),
        ],
        FRESH_MS,
        FLOOR_BPS,
    )
    .expect("combined");
    assert_eq!(combined.report, report(500_000, 200.0));
    // The age that drives the drain is the backlog's own report's.
    assert!(close(combined.age_ms, 200.0));
}

#[test]
fn multi_viewer_rule_ignores_stale_viewers_while_any_is_fresh() {
    let combined = combine_reports(
        [
            (5_000.0, report(100_000, 900.0)),
            (100.0, report(1_000_000, 0.0)),
        ],
        FRESH_MS,
        FLOOR_BPS,
    )
    .expect("combined");
    assert_eq!(combined.report, report(1_000_000, 0.0));
    // All stale: capacity remembered (the lowest), and the policy holds.
    let stale = combine_reports(
        [
            (5_000.0, report(400_000, 0.0)),
            (4_000.0, report(1_000_000, 0.0)),
        ],
        FRESH_MS,
        FLOOR_BPS,
    )
    .expect("combined");
    assert_eq!(stale.report, report(400_000, 0.0));
    assert!(close(
        drain_target_bps(&config(), stale.report, stale.age_ms, RTT_MS),
        400_000.0
    ));
    assert_eq!(combine_reports([], FRESH_MS, FLOOR_BPS), None);
}

#[test]
fn multi_viewer_publisher_drops_a_viewer_that_left() {
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report("slow", 0.0, report(300_000, 0.0));
    publisher.on_report("fast", 0.0, report(1_200_000, 0.0));
    let probe = 1.0 + config().probe_fraction(RTT_MS);
    assert!(close(publisher.target_bps(10.0, RTT_MS), 300_000.0 * probe));
    publisher.remove_viewer(&"slow");
    assert_eq!(publisher.viewers(), 1);
    assert!(close(
        publisher.target_bps(10.0, RTT_MS),
        1_200_000.0 * probe
    ));
}

/// A report of 1 kbps (hostile, or an honest hopeless viewer) is beyond
/// what one encoding can serve: it counts as the bottom tier's floor, so
/// the target never goes below half of it.
#[test]
fn a_tiny_capacity_counts_as_the_floor() {
    let combined = combine_reports(
        [(100.0, report(1_000, 0.0)), (100.0, report(2_000_000, 0.0))],
        FRESH_MS,
        FLOOR_BPS,
    )
    .expect("combined");
    assert_eq!(combined.report.capacity_bps, 270_000);
    // The worst a backlog can do on top: the drain floor.
    let worst = drain_target_bps(&config(), report(1_000, 1_000_000.0), 0.0, RTT_MS);
    assert!(close(worst, FLOOR_BPS * MIN_DRAIN_FRACTION), "{worst}");
}

/// A backlog is perishable: a stale backlog report, however large, holds at
/// the capacity instead of draining (u32::MAX ms would have drained for
/// 100 days).
#[test]
fn a_stale_backlog_holds_and_never_drains() {
    let config = config();
    let huge = report(500_000, f64::from(u32::MAX));
    assert!(close(
        drain_target_bps(&config, huge, 0.0, RTT_MS),
        250_000.0
    ));
    assert!(close(
        drain_target_bps(&config, huge, FRESH_MS, RTT_MS),
        250_000.0
    ));
    assert!(close(
        drain_target_bps(&config, huge, FRESH_MS + 1.0, RTT_MS),
        500_000.0
    ));
    // And through the multi-viewer rule: no fresh report, no backlog.
    let combined = combine_reports([(5_000.0, huge)], FRESH_MS, FLOOR_BPS).expect("combined");
    assert_eq!(combined.report, report(500_000, 0.0));
}

#[test]
fn reconfigure_keeps_the_reports_and_the_remembered_capacity() {
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report(1u64, 0.0, report(400_000, 0.0));
    // Camera restarted at 15 fps, much later: the target holds the
    // remembered capacity instead of jumping back to the start rate.
    publisher.set_frame_interval_ms(1000.0 / 15.0);
    assert!(close(publisher.config().frame_interval_ms, 1000.0 / 15.0));
    assert!(close(publisher.target_bps(60_000.0, RTT_MS), 400_000.0));
    publisher.reconfigure(PublisherConfig {
        max_bps: 2_000_000.0,
        ..config()
    });
    assert!(close(publisher.target_bps(60_000.0, RTT_MS), 400_000.0));
    assert_eq!(publisher.viewers(), 1);
}

#[test]
fn congestion_limit_halves_then_grows_one_probe_step_per_cycle() {
    let config = config();
    let cycle = config.cycle_ms(RTT_MS);
    let p = config.probe_fraction(RTT_MS);
    let mut limit = CongestionLimit::new();
    assert_eq!(limit.limit_bps(0.0, cycle, p, 1_500_000.0), None);
    assert!(limit.on_signal(1_000.0, 900_000.0, cycle, FLOOR_BPS));
    assert!(close(
        limit
            .limit_bps(1_000.0, cycle, p, 1_500_000.0)
            .expect("cut"),
        450_000.0
    ));
    // Within one cycle the next signal is the same congestion: ignored.
    assert!(!limit.on_signal(1_000.0 + cycle - 1.0, 450_000.0, cycle, FLOOR_BPS));
    let later = limit
        .limit_bps(1_000.0 + 3.0 * cycle, cycle, p, 1_500_000.0)
        .expect("still limited");
    assert!(close(later, 450_000.0 * (1.0 + p).powi(3)));
    // Gone once it reaches the maximum.
    assert_eq!(
        limit.limit_bps(1_000.0 + 100.0 * cycle, cycle, p, 1_500_000.0),
        None
    );
    // Never below the drain floor.
    let mut low = CongestionLimit::new();
    low.on_signal(0.0, 10_000.0, cycle, FLOOR_BPS);
    assert!(close(
        low.limit_bps(0.0, cycle, p, 1e9).expect("cut"),
        FLOOR_BPS * MIN_DRAIN_FRACTION
    ));
}

#[test]
fn congestion_signal_caps_target_and_tier_capacity_without_reports() {
    let mut publisher: MultiViewerPublisher<u64> = MultiViewerPublisher::new(config());
    assert!(close(publisher.target_bps(0.0, RTT_MS), 936_000.0));
    assert_eq!(publisher.tier_capacity_bps(0.0, RTT_MS), None);
    assert!(publisher.on_congestion_signal(100.0, RTT_MS, None));
    assert!(close(publisher.target_bps(100.0, RTT_MS), 468_000.0));
    assert!(close(
        publisher.tier_capacity_bps(100.0, RTT_MS).expect("limit"),
        468_000.0
    ));
}

/// A signal blamed on a viewer that reports fresh numbers is ignored: its
/// reports already say what it takes.
#[test]
fn congestion_signal_from_a_reporting_viewer_is_ignored() {
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report(7u64, 0.0, report(800_000, 0.0));
    assert!(!publisher.on_congestion_signal(100.0, RTT_MS, Some(&7)));
    assert_eq!(publisher.congestion_limit_bps(100.0, RTT_MS), None);
    // A silent viewer's signal counts.
    assert!(publisher.on_congestion_signal(100.0, RTT_MS, Some(&8)));
    // And so does the reporting one's once its report went stale.
    let mut stale = MultiViewerPublisher::new(config());
    stale.on_report(7u64, 0.0, report(800_000, 0.0));
    assert!(stale.on_congestion_signal(FRESH_MS + 1.0, RTT_MS, Some(&7)));
}

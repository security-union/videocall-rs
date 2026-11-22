//! The drain/probe/hold policy and the multi-viewer rule, as pure functions.

use videocall_rate_control::{
    combine_reports, default_tiers, drain_target_bps, LimitSource, MultiViewerPublisher, Publisher,
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
        capacity_bps: Some(capacity_bps),
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
    publisher.on_frame_sent(10.0, RTT_MS);
    assert!(close(publisher.target_bps(10.0, RTT_MS), 300_000.0 * probe));
    publisher.remove_viewer(&"slow");
    assert_eq!(publisher.viewers(), 1);
    // The ceiling it set goes; the target climbs to the fast viewer's one
    // probe step per cycle instead of leaping.
    assert!(close(publisher.target_bps(10.0, RTT_MS), 300_000.0 * probe));
    let mut t = 10.0;
    while t < 30_000.0 {
        publisher.on_report("fast", t, report(1_200_000, 0.0));
        publisher.on_frame_sent(t, RTT_MS);
        t += FRAME_MS;
    }
    assert!(close(publisher.target_bps(t, RTT_MS), 1_200_000.0 * probe));
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
    assert_eq!(combined.report.capacity_bps, Some(270_000));
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

/// Feedback cycle `T = I + RTT`.
const CYCLE_MS: f64 = 600.0;
const FRAME_MS: f64 = 1000.0 / 30.0;

fn open() -> Report {
    Report {
        capacity_bps: None,
        backlog_ms: 0.0,
    }
}

/// Frames from `from_ms` to `to_ms` (moving the paced capacity on); the
/// times a quiet viewer cut.
fn send_frames<K: Ord>(
    publisher: &mut MultiViewerPublisher<K>,
    from_ms: f64,
    to_ms: f64,
) -> Vec<f64> {
    let mut cuts = Vec::new();
    let mut t = from_ms;
    while t < to_ms {
        if publisher.on_frame_sent(t, RTT_MS) {
            cuts.push(t);
        }
        t += FRAME_MS;
    }
    cuts
}

/// The relay signal halves the target, is debounced by one cycle, grows
/// back one probe step per cycle, and never goes below the drain floor.
#[test]
fn relay_signal_halves_then_grows_one_probe_step_per_cycle() {
    let mut publisher: MultiViewerPublisher<u64> = MultiViewerPublisher::new(config());
    let probe = 1.0 + config().probe_fraction(RTT_MS);
    assert!(close(publisher.target_bps(0.0, RTT_MS), 936_000.0));
    assert_eq!(publisher.limit(0.0, RTT_MS), None);
    assert!(publisher.on_congestion_signal(100.0, RTT_MS, None));
    assert!(close(publisher.target_bps(100.0, RTT_MS), 468_000.0));
    assert_eq!(
        publisher.limit(100.0, RTT_MS),
        Some((LimitSource::RelaySignal, 468_000.0))
    );
    // Within one cycle the next signal is the same congestion: ignored.
    assert!(!publisher.on_congestion_signal(100.0 + CYCLE_MS - 1.0, RTT_MS, None));
    send_frames(&mut publisher, 100.0, 100.0 + 3.0 * CYCLE_MS + 1.0);
    let later = publisher.target_bps(100.0 + 3.0 * CYCLE_MS, RTT_MS);
    assert!(close(later, 468_000.0 * probe.powi(3)), "{later}");
    // Back to the start rate, and then the reports (none) govern.
    send_frames(&mut publisher, 2_000.0, 30_000.0);
    assert!(close(publisher.target_bps(30_000.0, RTT_MS), 936_000.0));
    assert_eq!(publisher.limit(30_000.0, RTT_MS), None);
    // Never below the drain floor.
    for k in 0..10 {
        publisher.on_congestion_signal(40_000.0 + f64::from(k) * CYCLE_MS, RTT_MS, None);
    }
    assert!(close(
        publisher.target_bps(40_000.0 + 9.0 * CYCLE_MS, RTT_MS),
        FLOOR_BPS * MIN_DRAIN_FRACTION
    ));
}

/// With no reports the tier capacity is the start rate, and a relay cut
/// lowers it.
#[test]
fn congestion_signal_caps_target_and_tier_capacity_without_reports() {
    let mut publisher: MultiViewerPublisher<u64> = MultiViewerPublisher::new(config());
    assert!(close(publisher.capacity_bps(0.0, RTT_MS), 936_000.0));
    assert!(publisher.on_congestion_signal(100.0, RTT_MS, None));
    assert!(close(publisher.target_bps(100.0, RTT_MS), 468_000.0));
    assert!(close(publisher.capacity_bps(100.0, RTT_MS), 468_000.0));
}

/// A signal blamed on a viewer whose fresh report has a ceiling is ignored:
/// its reports already say what it takes.
#[test]
fn congestion_signal_from_a_reporting_viewer_is_ignored() {
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report(7u64, 0.0, report(800_000, 0.0));
    assert!(!publisher.on_congestion_signal(100.0, RTT_MS, Some(&7)));
    assert_eq!(publisher.limit(100.0, RTT_MS), None);
    // A silent viewer's signal counts.
    assert!(publisher.on_congestion_signal(100.0, RTT_MS, Some(&8)));
    // And so does the reporting one's once its report went stale.
    let mut stale = MultiViewerPublisher::new(config());
    stale.on_report(7u64, 0.0, report(800_000, 0.0));
    assert!(stale.on_congestion_signal(FRESH_MS + 1.0, RTT_MS, Some(&7)));
}

/// A report without a ceiling never lowers the target; the target probes
/// up from the start rate by one step per cycle instead of leaping to max.
#[test]
fn a_report_without_a_ceiling_probes_up_from_the_start_rate() {
    let mut publisher = MultiViewerPublisher::new(config());
    let probe = 1.0 + config().probe_fraction(RTT_MS);
    publisher.on_report("a", 0.0, open());
    send_frames(&mut publisher, 0.0, 3.0 * CYCLE_MS + 1.0);
    // The target is one probe step above the paced capacity.
    assert!(close(
        publisher.capacity_bps(3.0 * CYCLE_MS, RTT_MS),
        936_000.0 * probe.powi(3)
    ));
    assert_eq!(
        publisher.limit(3.0 * CYCLE_MS, RTT_MS).map(|limit| limit.0),
        Some(LimitSource::Rise)
    );
    let mut t = 3.0 * CYCLE_MS;
    while t < 60_000.0 {
        publisher.on_report("a", t, open());
        send_frames(&mut publisher, t, t + 500.0);
        t += 500.0;
    }
    assert!(close(publisher.target_bps(t, RTT_MS), 1_500_000.0));
    assert!(close(publisher.capacity_bps(t, RTT_MS), 1_500_000.0));
}

/// A capacity far above anything that could be sent chooses tiers as the
/// maximum would, no more.
#[test]
fn an_implausible_capacity_counts_as_the_maximum() {
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report("a", 0.0, report(50_000_000, 0.0));
    let mut t = 0.0;
    while t < 30_000.0 {
        publisher.on_report("a", t, report(50_000_000, 0.0));
        send_frames(&mut publisher, t, t + 500.0);
        t += 500.0;
    }
    assert!(close(publisher.capacity_bps(t, RTT_MS), 1_500_000.0));
}

/// A viewer that reported and then goes quiet while frames go out cuts
/// after `fresh + capacity window` = 1100 + 2000 ms, once per episode.
#[test]
fn a_viewer_quiet_while_frames_go_out_cuts_once() {
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report("a", 0.0, report(800_000, 0.0));
    assert_eq!(publisher.silence_ms(RTT_MS), 3_100.0);
    let cuts = send_frames(&mut publisher, 0.0, 10_000.0);
    assert_eq!(cuts.len(), 1, "{cuts:?}");
    assert!(cuts[0] > 3_100.0 && cuts[0] < 3_100.0 + FRAME_MS + 1.0);
    assert_eq!(
        publisher.limit(cuts[0], RTT_MS).map(|limit| limit.0),
        Some(LimitSource::QuietViewer)
    );
    // It reports again: a new episode can start after another silence.
    publisher.on_report("a", 10_000.0, report(800_000, 0.0));
    let cuts = send_frames(&mut publisher, 10_000.0, 20_000.0);
    assert_eq!(cuts.len(), 1, "{cuts:?}");
}

/// No cut with the camera off, for a viewer that left, or for viewers
/// that never reported; a report in time resets the clock.
#[test]
fn silence_needs_frames_and_a_viewer_that_reported() {
    // Camera off from 1 s to 5 s: quiet time starts over at 5 s.
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report("a", 0.0, report(1_200_000, 0.0));
    assert!(send_frames(&mut publisher, 0.0, 1_000.0).is_empty());
    let cuts = send_frames(&mut publisher, 5_000.0, 9_000.0);
    assert_eq!(cuts.len(), 1);
    assert!(cuts[0] > 8_100.0, "{cuts:?}");
    // Left.
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report("a", 0.0, report(1_200_000, 0.0));
    publisher.remove_viewer(&"a");
    assert!(send_frames(&mut publisher, 0.0, 10_000.0).is_empty());
    // Never reported.
    let mut publisher: MultiViewerPublisher<&str> = MultiViewerPublisher::new(config());
    assert!(send_frames(&mut publisher, 0.0, 10_000.0).is_empty());
    // Reporting every interval.
    let mut publisher = MultiViewerPublisher::new(config());
    for i in 0..20 {
        let t = f64::from(i) * 500.0;
        publisher.on_report("a", t, report(1_200_000, 0.0));
        assert!(send_frames(&mut publisher, t, t + 500.0).is_empty());
    }
}

/// After the link is lost the target in force is held and grows back one
/// probe step per cycle, up to the maximum.
#[test]
fn a_lost_link_holds_the_rate_and_grows_it_back() {
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report("a", 0.0, report(500_000, 0.0));
    send_frames(&mut publisher, 0.0, 100.0);
    let held = publisher.target_bps(100.0, RTT_MS);
    publisher.on_link_lost(100.0, RTT_MS);
    assert_eq!(publisher.viewers(), 0);
    let probe = 1.0 + config().probe_fraction(RTT_MS);
    assert!(close(publisher.target_bps(100.0, RTT_MS), held));
    assert_eq!(
        publisher.limit(100.0, RTT_MS),
        Some((LimitSource::LinkLost, held))
    );
    send_frames(&mut publisher, 100.0, 100.0 + 4.0 * CYCLE_MS + 1.0);
    assert!(close(
        publisher.target_bps(100.0 + 4.0 * CYCLE_MS, RTT_MS),
        held * probe.powi(4)
    ));
    send_frames(&mut publisher, 3_000.0, 120_000.0);
    assert!(close(publisher.target_bps(120_000.0, RTT_MS), 1_500_000.0));
    assert_eq!(publisher.limit(120_000.0, RTT_MS), None);
}

/// Called on every frame, the tier goes up at most one rung per cycle and
/// down at once.
#[test]
fn next_tier_climbs_one_rung_per_cycle_and_drops_at_once() {
    let tiers = default_tiers(1_500_000.0);
    let start = PublisherConfig {
        start_bps: 1_500_000.0,
        ..config()
    };
    let mut publisher = MultiViewerPublisher::new(start);
    publisher.on_report("a", 0.0, report(5_000_000, 0.0));
    let mut tier = 3;
    let mut ups = Vec::new();
    let mut t = 0.0;
    while t < 3_000.0 {
        let next = publisher.next_tier(&tiers, tier, t, RTT_MS);
        if next != tier {
            ups.push((t, next));
            tier = next;
        }
        t += FRAME_MS;
    }
    assert_eq!(ups.len(), 3, "{ups:?}");
    assert_eq!(tier, 0);
    for pair in ups.windows(2) {
        assert!(pair[1].0 - pair[0].0 >= CYCLE_MS, "{ups:?}");
    }
    publisher.on_report("a", t, report(300_000, 0.0));
    assert_eq!(publisher.next_tier(&tiers, tier, t, RTT_MS), 3);
}

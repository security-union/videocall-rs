//! A capacity rise a viewer measured is taken at once; only rises nothing
//! measured are paced one probe step per cycle.

mod support;

use support::{run, SimConfig};
use videocall_rate_control::{MultiViewerPublisher, PublisherConfig, Report};

const RTT_MS: f64 = 100.0;
const FRAME_MS: f64 = 1000.0 / 30.0;

/// The round 3 figures (3550 and 3650 ms), with one report period of slack.
#[test]
fn recovery_after_the_link_reopens_follows_the_measurements() {
    for (name, config, limit_ms) in [
        ("baseline", SimConfig::baseline(), 4_050.0),
        ("long haul", SimConfig::long_delay(), 4_150.0),
    ] {
        let recovery = run(&config)
            .metrics()
            .b4_recovery_ms
            .expect("never recovered");
        assert!(recovery <= limit_ms, "{name}: B4 {recovery:.0} ms");
    }
}

fn config() -> PublisherConfig {
    PublisherConfig {
        start_bps: 936_000.0,
        max_bps: 1_500_000.0,
        frame_interval_ms: FRAME_MS,
        report_interval_ms: 500.0,
        floor_bps: 270_000.0,
    }
}

fn step() -> f64 {
    1.0 + config().probe_fraction(RTT_MS)
}

fn ceiling(bps: u64) -> Report {
    Report {
        capacity_bps: Some(bps),
        backlog_ms: 0.0,
    }
}

/// Runs `from_ms..to_ms` with `reports` every 500 ms; returns the highest
/// target seen.
fn drive(
    publisher: &mut MultiViewerPublisher<&'static str>,
    from_ms: f64,
    to_ms: f64,
    reports: &[(&'static str, Report)],
) -> f64 {
    let mut peak: f64 = 0.0;
    let mut t = from_ms;
    let mut next = from_ms;
    while t < to_ms {
        if t >= next {
            for (viewer, report) in reports {
                publisher.on_report(viewer, t, *report);
            }
            next += 500.0;
        }
        publisher.on_frame_sent(t, RTT_MS);
        peak = peak.max(publisher.target_bps(t, RTT_MS));
        t += FRAME_MS;
    }
    peak
}

/// A viewer measuring more is believed at once.
#[test]
fn a_measured_rise_is_taken_at_once() {
    let mut publisher = MultiViewerPublisher::new(config());
    drive(&mut publisher, 0.0, 5_000.0, &[("a", ceiling(400_000))]);
    drive(
        &mut publisher,
        5_000.0,
        5_100.0,
        &[("a", ceiling(1_000_000))],
    );
    let target = publisher.target_bps(5_100.0, RTT_MS);
    assert!(
        (target - 1_000_000.0 * step()).abs() < 1.0,
        "{:.0} kbps",
        target / 1000.0
    );
}

/// A hostile or mistaken viewer reporting a huge measured ceiling cannot
/// lift the target past what the others' reports allow (lowest ceiling).
#[test]
fn a_huge_measured_ceiling_does_not_override_the_others() {
    let mut publisher = MultiViewerPublisher::new(config());
    let reports = [("honest", ceiling(500_000)), ("liar", ceiling(50_000_000))];
    let peak = drive(&mut publisher, 0.0, 20_000.0, &reports);
    assert!(
        peak <= 500_000.0 * step() * 1.000_001,
        "{:.0} kbps",
        peak / 1000.0
    );
}

/// Alone, a viewer's measurement is bounded by the cap, and its own next
/// backlog report brings the target down at once.
#[test]
fn a_lone_viewer_is_bounded_by_the_cap_and_its_own_backlog() {
    let mut publisher = MultiViewerPublisher::new(config());
    let peak = drive(&mut publisher, 0.0, 2_000.0, &[("a", ceiling(50_000_000))]);
    assert_eq!(peak, 1_500_000.0);
    let backlog = Report {
        capacity_bps: Some(400_000),
        backlog_ms: 300.0,
    };
    publisher.on_report("a", 2_000.0, backlog);
    assert!(publisher.target_bps(2_000.0, RTT_MS) < 400_000.0);
}

/// A relay cut speaks for viewers that do not report: a reporting viewer's
/// measurement of its own path does not undo it.
#[test]
fn a_measurement_does_not_undo_a_relay_cut() {
    let mut publisher = MultiViewerPublisher::new(config());
    drive(&mut publisher, 0.0, 3_000.0, &[("a", ceiling(1_000_000))]);
    assert!(publisher.on_congestion_signal(3_000.0, RTT_MS, None));
    let cut = publisher.target_bps(3_000.0, RTT_MS);
    let peak = drive(
        &mut publisher,
        3_000.0,
        4_200.0,
        &[("a", ceiling(1_000_000))],
    );
    assert!(
        peak <= cut * step() * step() * 1.000_001,
        "{:.0} after {:.0}",
        peak / 1000.0,
        cut / 1000.0
    );
}

/// A quiet viewer returning with a new measurement (its ceiling rose) is
/// believed at once; returning with the same ceiling is old news and the
/// target climbs back by steps.
#[test]
fn a_quiet_viewer_returning_with_a_measurement_is_believed() {
    let mut publisher = MultiViewerPublisher::new(config());
    drive(&mut publisher, 0.0, 1_000.0, &[("a", ceiling(1_000_000))]);
    drive(&mut publisher, 1_000.0, 5_000.0, &[]);
    let cut = publisher.target_bps(5_000.0, RTT_MS);
    assert!(cut < 800_000.0);
    let mut same = publisher.clone();
    drive(&mut same, 5_000.0, 5_100.0, &[("a", ceiling(1_000_000))]);
    assert!(same.target_bps(5_100.0, RTT_MS) <= cut * step() * 1.000_001);
    drive(
        &mut publisher,
        5_000.0,
        5_100.0,
        &[("a", ceiling(1_100_000))],
    );
    let target = publisher.target_bps(5_100.0, RTT_MS);
    assert!(
        (target - 1_100_000.0 * step()).abs() < 1.0,
        "{:.0} kbps",
        target / 1000.0
    );
}

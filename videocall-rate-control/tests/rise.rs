//! The publisher's one rule for going up: the target never rises by more
//! than one probe step `(1 + p)` per feedback cycle, whatever lets it rise.
//! Decreases apply at once.

use videocall_rate_control::{default_tiers, MultiViewerPublisher, PublisherConfig, Report};

const RTT_MS: f64 = 100.0;
/// Feedback cycle `T = I + RTT`.
const CYCLE_MS: f64 = 600.0;
const FRAME_MS: f64 = 1000.0 / 30.0;

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

fn open() -> Report {
    Report {
        capacity_bps: None,
        backlog_ms: 0.0,
    }
}

type Reporters<'a> = &'a [(&'static str, Report)];

/// Sends frames from `from_ms` to `to_ms` while `reporters` report every
/// 500 ms; the times a quiet viewer cut.
fn send(
    publisher: &mut MultiViewerPublisher<&'static str>,
    from_ms: f64,
    to_ms: f64,
    reporters: Reporters,
) -> Vec<f64> {
    let mut cuts = Vec::new();
    let mut t = from_ms;
    let mut next_report = from_ms;
    while t < to_ms {
        if t >= next_report {
            for (viewer, report) in reporters {
                publisher.on_report(viewer, t, *report);
            }
            next_report += 500.0;
        }
        if publisher.on_frame_sent(t, RTT_MS) {
            cuts.push(t);
        }
        t += FRAME_MS;
    }
    cuts
}

/// Checks the rule on a run of frames: no rise above one step per cycle.
fn assert_paced(
    publisher: &mut MultiViewerPublisher<&'static str>,
    from_ms: f64,
    to_ms: f64,
    reporters: Reporters,
    before_bps: f64,
) {
    let mut history: Vec<(f64, f64)> = vec![(from_ms, before_bps)];
    let mut t = from_ms;
    let mut next_report = from_ms + 500.0;
    while t < to_ms {
        if t >= next_report {
            for (viewer, report) in reporters {
                publisher.on_report(viewer, t, *report);
            }
            next_report += 500.0;
        }
        publisher.on_frame_sent(t, RTT_MS);
        let target = publisher.target_bps(t, RTT_MS);
        for (at, bps) in history
            .iter()
            .rev()
            .take_while(|(at, _)| t - at <= CYCLE_MS)
        {
            assert!(
                target <= bps * step() * 1.000_001,
                "{:.0} kbps at {t:.0} ms after {:.0} kbps at {at:.0} ms",
                target / 1000.0,
                bps / 1000.0
            );
        }
        history.push((t, target));
        t += FRAME_MS;
    }
}

/// The first no-ceiling report of the call is long gone (its pacing has
/// grown out to the cap) when a 400 kbps viewer leaves the rest open: the
/// target climbs from 400 kbps, it does not jump to the cap.
#[test]
fn a_ceiling_viewer_leaving_late_in_the_call_is_paced() {
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report("open", 0.0, open());
    send(&mut publisher, 0.0, 20_000.0, &[("open", open())]);
    assert_eq!(publisher.target_bps(20_000.0, RTT_MS), 1_500_000.0);
    let both = [("narrow", ceiling(400_000)), ("open", open())];
    send(&mut publisher, 20_000.0, 22_000.0, &both);
    assert!(publisher.target_bps(22_000.0, RTT_MS) <= 400_000.0 * step() * 1.000_001);
    let before = publisher.target_bps(22_000.0, RTT_MS);
    publisher.remove_viewer(&"narrow");
    assert_paced(
        &mut publisher,
        22_000.0,
        60_000.0,
        &[("open", open())],
        before,
    );
    assert_eq!(publisher.target_bps(60_000.0, RTT_MS), 1_500_000.0);
}

/// A quiet viewer coming back with a report that shows no ceiling: the
/// target climbs from where the silence cut it.
#[test]
fn a_quiet_viewer_returning_without_a_ceiling_is_paced() {
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report("a", 0.0, ceiling(1_200_000));
    let cuts = send(&mut publisher, 0.0, 5_000.0, &[]);
    assert_eq!(cuts.len(), 1);
    let cut_bps = publisher.target_bps(5_000.0, RTT_MS);
    assert!(cut_bps < 800_000.0, "not cut: {cut_bps:.0}");
    publisher.on_report("a", 5_000.0, open());
    assert!(publisher.target_bps(5_000.0, RTT_MS) <= cut_bps * step() * 1.000_001);
    assert_paced(&mut publisher, 5_000.0, 40_000.0, &[("a", open())], cut_bps);
}

/// The cap raised mid-call (a settings change) is approached, not leapt to.
#[test]
fn a_raised_cap_is_paced() {
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report("open", 0.0, open());
    send(&mut publisher, 0.0, 30_000.0, &[("open", open())]);
    assert_eq!(publisher.target_bps(30_000.0, RTT_MS), 1_500_000.0);
    publisher.reconfigure(PublisherConfig {
        max_bps: 3_000_000.0,
        ..config()
    });
    assert!(publisher.target_bps(30_000.0, RTT_MS) <= 1_500_000.0 * step() * 1.000_001);
    assert_paced(
        &mut publisher,
        30_000.0,
        60_000.0,
        &[("open", open())],
        1_500_000.0,
    );
}

/// A lost link holds the rate; a relay signal right after it still counts.
#[test]
fn a_relay_signal_right_after_a_lost_link_is_honoured() {
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report("a", 0.0, ceiling(1_000_000));
    send(&mut publisher, 0.0, 1_000.0, &[("a", ceiling(1_000_000))]);
    let held = publisher.target_bps(1_000.0, RTT_MS);
    publisher.on_link_lost(1_000.0, RTT_MS);
    assert!(publisher.on_congestion_signal(1_100.0, RTT_MS, None));
    // Half of the target at the signal, which is at most a sixth of a
    // step above the held rate after 100 ms.
    assert!(publisher.target_bps(1_100.0, RTT_MS) <= held * 0.5 * step());
}

/// One halving stands for one missing report: a 60 s silence cuts once,
/// then the rate grows.
#[test]
fn a_long_silence_cuts_once() {
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report("a", 0.0, ceiling(1_200_000));
    let mut cuts = send(&mut publisher, 0.0, 3_200.0, &[]);
    assert_eq!(cuts.len(), 1, "{cuts:?}");
    let after_cut = publisher.target_bps(3_200.0, RTT_MS);
    cuts.extend(send(&mut publisher, 3_200.0, 60_000.0, &[]));
    assert_eq!(cuts.len(), 1, "{cuts:?}");
    assert!(publisher.target_bps(60_000.0, RTT_MS) > after_cut * step().powi(10));
}

/// A viewer whose fresh report shows no ceiling has said nothing about a
/// limit, so the relay's drop on its path is the only evidence: honoured.
#[test]
fn a_relay_signal_on_a_viewer_without_a_ceiling_is_honoured() {
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report("a", 0.0, open());
    assert!(publisher.on_congestion_signal(100.0, RTT_MS, Some(&"a")));
    // A viewer whose report has a ceiling is still trusted over the bit.
    let mut publisher = MultiViewerPublisher::new(config());
    publisher.on_report("a", 0.0, ceiling(800_000));
    assert!(!publisher.on_congestion_signal(100.0, RTT_MS, Some(&"a")));
}

/// With no reports the tier is the start rate's, also after a relay cut
/// has grown back, even when the start rate is just above a floor (980 kbps
/// over 720p's 960 kbps: inside the step-up margin).
#[test]
fn with_no_reports_the_tier_is_the_start_rates() {
    let tiers = default_tiers(1_500_000.0);
    let mut publisher: MultiViewerPublisher<&str> = MultiViewerPublisher::new(config());
    assert_eq!(publisher.next_tier(&tiers, 0, 0.0, RTT_MS), 1);
    let start = PublisherConfig {
        start_bps: 980_000.0,
        ..config()
    };
    let mut publisher: MultiViewerPublisher<&str> = MultiViewerPublisher::new(start);
    let mut tier = publisher.next_tier(&tiers, 0, 0.0, RTT_MS);
    assert_eq!(tier, 0);
    assert!(publisher.on_congestion_signal(0.0, RTT_MS, None));
    let mut t = 0.0;
    while t < 60_000.0 {
        publisher.on_frame_sent(t, RTT_MS);
        tier = publisher.next_tier(&tiers, tier, t, RTT_MS);
        t += FRAME_MS;
    }
    assert_eq!(tier, 0);
}

//! Viewer estimator on synthetic arrivals: start-up, clock offset, keyframes,
//! sender skips, a lost frame, a real queue, silence, the early report, and
//! frames that finish out of order.

use videocall_rate_control::{FrameInfo, Report, ViewerConfig, ViewerEstimator, NO_CEILING_KBPS};

const FRAME_MS: f64 = 1000.0 / 30.0;

fn estimator() -> ViewerEstimator {
    ViewerEstimator::new(ViewerConfig {
        report_interval_ms: 500.0,
    })
}

/// A FIFO link with 50 ms delay; returns arrival for a frame sent at
/// `send_ms`, updating `link_free_ms`.
fn link(send_ms: f64, bytes: u32, bps: f64, link_free_ms: &mut f64) -> f64 {
    let start = send_ms.max(*link_free_ms);
    *link_free_ms = start + f64::from(bytes) * 8.0 * 1000.0 / bps;
    *link_free_ms + 50.0
}

/// What a `steady` run saw.
struct Steady {
    reports: Vec<Option<Report>>,
    congested_after_2s: bool,
}

/// Feeds `seconds` of 30 fps camera frames (3,900-byte deltas, 6x keyframe
/// every 2 s) through a link at `bps`, sender clock shifted by `offset_ms`,
/// reports every 500 ms. `send(frame)` decides whether the sender hands
/// camera frame `frame` to the transport; only handed frames are numbered.
/// `lost(seq)` drops a numbered frame on the way.
fn steady(
    seconds: f64,
    bps: f64,
    offset_ms: f64,
    send: impl Fn(u64) -> bool,
    lost: impl Fn(u64) -> bool,
) -> Steady {
    let mut estimator = estimator();
    let mut link_free = 0.0;
    let mut reports = Vec::new();
    let mut next_report = 500.0;
    let mut congested_after_2s = false;
    let mut seq = 0u64;
    for frame in 0..(seconds * 30.0) as u64 {
        if !send(frame) {
            continue;
        }
        let this_seq = seq;
        seq += 1;
        let timestamp = frame as f64 * FRAME_MS;
        let bytes = if frame % 60 == 0 { 23_400 } else { 3_900 };
        let arrival = link(timestamp, bytes, bps, &mut link_free);
        if lost(this_seq) {
            continue;
        }
        while next_report <= arrival {
            reports.push(estimator.report(next_report));
            next_report += 500.0;
        }
        let outcome = estimator.on_frame(FrameInfo {
            seq: this_seq,
            timestamp_ms: timestamp + offset_ms,
            arrival_ms: arrival,
            bytes,
        });
        congested_after_2s |= outcome.congested && timestamp >= 2_000.0;
    }
    Steady {
        reports,
        congested_after_2s,
    }
}

#[test]
fn no_report_before_any_frame_and_no_ceiling_after_one() {
    let mut estimator = estimator();
    assert_eq!(estimator.report(500.0), None);
    estimator.on_frame(FrameInfo {
        seq: 0,
        timestamp_ms: 0.0,
        arrival_ms: 60.0,
        bytes: 3_900,
    });
    // Frames flow, nothing has queued: a report, but no ceiling.
    assert_eq!(
        estimator.report(1_000.0),
        Some(Report {
            capacity_bps: None,
            backlog_ms: 0.0
        })
    );
    assert_eq!(estimator.report(1_500.0), None, "nothing new");
}

#[test]
fn unknown_clock_offset_changes_nothing() {
    let a = steady(10.0, 2e6, 0.0, |_| true, |_| false);
    let b = steady(10.0, 2e6, 37_000.0, |_| true, |_| false);
    assert_eq!(a.reports, b.reports);
}

#[test]
fn keyframes_on_a_healthy_link_are_not_congestion() {
    let run = steady(20.0, 2e6, 0.0, |_| true, |_| false);
    assert!(!run.congested_after_2s, "keyframe read as standing queue");
    // The path never queued: what it delivered is only a lower bound, so
    // the report says "flowing, no ceiling seen".
    let last = run.reports.last().copied().flatten().expect("a report");
    assert_eq!(
        last,
        Report {
            capacity_bps: None,
            backlog_ms: 0.0
        }
    );
}

/// A sender that skips camera frames (decimation, a send buffer dropping
/// before the handoff) numbers only what it hands over: the viewer sees
/// timestamp gaps and consecutive sequence numbers, never a sequence gap.
/// That must not look like congestion, and reports keep coming.
#[test]
fn sender_side_skips_are_not_congestion() {
    let run = steady(20.0, 2e6, 0.0, |frame| frame % 10 != 5, |_| false);
    assert!(!run.congested_after_2s);
    let last = run.reports.last().copied().flatten().expect("a report");
    assert_eq!(last.backlog_ms, 0.0, "{last:?}");
}

/// A numbered frame that never arrives (dropped after the handoff by a
/// relay or a shaper) holds the frames behind it until the oldest is one
/// capacity window old, then the gap is given up: estimation resumes, and
/// the loss is not read as a queue.
#[test]
fn a_frame_lost_after_numbering_is_given_up_and_not_congestion() {
    let lost = |seq: u64| seq == 150 || seq == 400;
    let run = steady(20.0, 2e6, 0.0, |_| true, lost);
    assert!(!run.congested_after_2s, "a loss read as congestion");
    // Reports go on after both losses and stay clean.
    let tail: Vec<Report> = run
        .reports
        .iter()
        .rev()
        .take(4)
        .flatten()
        .copied()
        .collect();
    assert_eq!(tail.len(), 4, "{:?}", run.reports);
    assert!(tail.iter().all(|r| r.backlog_ms == 0.0), "{tail:?}");
}

/// The frames behind a lost one are held until the oldest of them is one
/// capacity window old, then released: the loss costs the estimate at most
/// that window.
#[test]
fn frames_behind_a_lost_one_are_released_after_the_capacity_window() {
    let mut estimator = estimator();
    let mut released_at = None;
    for seq in 0..200u64 {
        if seq == 30 {
            continue;
        }
        let arrival = seq as f64 * FRAME_MS + 50.0;
        estimator.on_frame(FrameInfo {
            seq,
            timestamp_ms: arrival - 50.0,
            arrival_ms: arrival,
            bytes: 3_900,
        });
        if seq > 30 && released_at.is_none() && estimator.held_frames() == 0 {
            released_at = Some(arrival);
        }
    }
    let first_held = 31.0 * FRAME_MS + 50.0;
    let released = released_at.expect("frames released after the loss");
    assert!(
        released > first_held + 2_000.0 && released <= first_held + 2_000.0 + FRAME_MS,
        "released {} ms after the first held frame",
        released - first_held
    );
}

#[test]
fn a_queue_every_frame_pays_for_is_congestion() {
    // 30 fps of 3,900 bytes = 936 kbps into a 500 kbps link: the queue grows.
    let mut estimator = estimator();
    let mut link_free = 0.0;
    let mut first_congested = None;
    let mut last_report = None;
    for seq in 0..120u64 {
        let send = seq as f64 * FRAME_MS;
        let bps = if send < 2_000.0 { 2e6 } else { 5e5 };
        let arrival = link(send, 3_900, bps, &mut link_free);
        let outcome = estimator.on_frame(FrameInfo {
            seq,
            timestamp_ms: send,
            arrival_ms: arrival,
            bytes: 3_900,
        });
        if outcome.congested && first_congested.is_none() {
            first_congested = Some(arrival);
        }
        if seq % 15 == 14 {
            last_report = estimator.report(arrival);
        }
    }
    let first = first_congested.expect("never congested");
    assert!(first < 3_000.0, "congestion first seen at {first}");
    let report = last_report.expect("a report");
    // The link within 10%, and a backlog.
    assert!(
        report.backlog_ms > 0.0
            && report
                .capacity_bps
                .is_some_and(|bps| (450_000..=550_000).contains(&bps)),
        "{report:?}"
    );
}

#[test]
fn silence_is_neither_congestion_nor_capacity() {
    let mut estimator = estimator();
    let mut before = None;
    for seq in 0..150u64 {
        let send = seq as f64 * FRAME_MS;
        estimator.on_frame(FrameInfo {
            seq,
            timestamp_ms: send,
            arrival_ms: send + 66.0,
            bytes: 3_900,
        });
        if seq % 15 == 14 {
            before = estimator.report(send + 66.0);
        }
    }
    let before = before.expect("a report before silence");
    // Camera off for 10 s: report ticks keep coming, frames do not, so
    // there is no news to send (the publisher ages what it has and holds).
    let silence_start = 150.0 * FRAME_MS + 66.0;
    for tick in 1..=20 {
        let report = estimator.report(silence_start + f64::from(tick) * 500.0);
        assert_eq!(report, None, "a copy sent at tick {tick}");
    }
    // Frames resume with the next sequence number after a timestamp gap
    // (the sender numbered nothing while paused). The first one, a
    // keyframe, must not be read as a queue.
    let resume = 150.0 * FRAME_MS + 10_000.0;
    let outcome = estimator.on_frame(FrameInfo {
        seq: 150,
        timestamp_ms: resume,
        arrival_ms: resume + 66.0,
        bytes: 23_400,
    });
    assert!(!outcome.congested);
    // Silence changed nothing: the next report has the same capacity.
    let after = estimator.report(resume + 100.0).expect("news after resume");
    assert_eq!(after.capacity_bps, before.capacity_bps);
    assert_eq!(after.backlog_ms, 0.0);
}

/// The early report is sent once, on the transition into congestion, and
/// carries a backlog.
#[test]
fn one_early_report_per_congestion_event() {
    let mut estimator = estimator();
    let mut link_free = 0.0;
    let mut early = Vec::new();
    for seq in 0..150u64 {
        let send = seq as f64 * FRAME_MS;
        let bps = if send < 2_000.0 { 2e6 } else { 5e5 };
        let arrival = link(send, 3_900, bps, &mut link_free);
        let outcome = estimator.on_frame(FrameInfo {
            seq,
            timestamp_ms: send,
            arrival_ms: arrival,
            bytes: 3_900,
        });
        if let Some(report) = outcome.early_report {
            early.push(report);
        }
    }
    assert_eq!(early.len(), 1, "{early:?}");
    assert!(early[0].backlog_ms > 0.0);
}

#[test]
fn report_wire_round_trip() {
    let report = Report {
        capacity_bps: Some(1_234_567),
        backlog_ms: 312.6,
    };
    assert_eq!(report.capacity_kbps(), 1_235);
    assert_eq!(report.backlog_ms_rounded(), 313);
    assert_eq!(
        Report::from_wire(1_235, 313),
        Report {
            capacity_bps: Some(1_235_000),
            backlog_ms: 313.0,
        }
    );
    // No ceiling seen has its own wire value, both ways; 0 stays "no
    // report" for the caller.
    let open = Report {
        capacity_bps: None,
        backlog_ms: 0.0,
    };
    assert_eq!(open.capacity_kbps(), NO_CEILING_KBPS);
    assert_eq!(Report::from_wire(NO_CEILING_KBPS, 0), open);
    let huge = Report {
        capacity_bps: Some(u64::MAX),
        backlog_ms: 0.0,
    };
    assert_eq!(huge.capacity_kbps(), NO_CEILING_KBPS - 1);
    // A known capacity never rounds to 0.
    let tiny = Report {
        capacity_bps: Some(100),
        backlog_ms: 0.0,
    };
    assert_eq!(tiny.capacity_kbps(), 1);
}

#[test]
fn a_new_session_after_reset_starts_from_scratch() {
    let mut estimator = estimator();
    for seq in 0..100u64 {
        let t = seq as f64 * FRAME_MS;
        estimator.on_frame(FrameInfo {
            seq: 1_000 + seq,
            timestamp_ms: t,
            arrival_ms: t + 50.0,
            bytes: 3_900,
        });
    }
    assert!(estimator.report(4_000.0).is_some());
    estimator.reset();
    assert_eq!(estimator.report(4_500.0), None);
    // A restarted sequence is accepted, not ignored as late.
    let outcome = estimator.on_frame(FrameInfo {
        seq: 0,
        timestamp_ms: 5_000.0,
        arrival_ms: 5_050.0,
        bytes: 3_900,
    });
    assert_eq!(outcome.capacity_bps, None);
}

fn frame(seq: u64, timestamp_ms: f64, arrival_ms: f64, bytes: u32) -> FrameInfo {
    FrameInfo {
        seq,
        timestamp_ms,
        arrival_ms,
        bytes,
    }
}

/// While frames are held behind a lost one, nothing new is known: the
/// report tick sends nothing, then resumes once the gap is given up.
#[test]
fn no_report_while_frames_are_held_behind_a_gap() {
    let mut estimator = estimator();
    let mut reports = Vec::new();
    let mut tick = 500.0;
    for seq in 0..300u64 {
        let ts = seq as f64 * FRAME_MS;
        let arrival = ts + 50.0;
        while tick <= arrival {
            reports.push((tick, estimator.report(tick)));
            tick += 500.0;
        }
        if seq != 90 {
            estimator.on_frame(frame(seq, ts, arrival, 3_900));
        }
    }
    let lost_at = 90.0 * FRAME_MS + 50.0;
    let held: Vec<_> = reports
        .iter()
        .filter(|(t, _)| *t > lost_at + 500.0 && *t < lost_at + 2_000.0)
        .collect();
    assert!(!held.is_empty());
    assert!(held.iter().all(|(_, r)| r.is_none()), "{held:?}");
    let after: Vec<_> = reports
        .iter()
        .filter(|(t, _)| *t > lost_at + 2_600.0)
        .collect();
    assert!(after.iter().all(|(_, r)| r.is_some()), "{after:?}");
}

/// A new run of the sender's sequence (camera restarted: numbering from 0,
/// timestamps later) is picked up without any reset by the caller.
#[test]
fn a_restarted_sequence_starts_a_new_run_by_itself() {
    let mut estimator = estimator();
    for seq in 0..150u64 {
        let ts = seq as f64 * FRAME_MS;
        estimator.on_frame(frame(1_000 + seq, ts, ts + 50.0, 3_900));
    }
    assert!(estimator.report(5_100.0).is_some());
    // Camera back 3 s later, numbering from 0.
    let start = 150.0 * FRAME_MS + 3_000.0;
    for seq in 0..30u64 {
        let ts = start + seq as f64 * FRAME_MS;
        estimator.on_frame(frame(seq, ts, ts + 50.0, 3_900));
        assert_eq!(estimator.held_frames(), 0, "held at seq {seq}");
    }
    // A fresh run: no capacity yet until a delivered window (it would have
    // been the old one's if the frames were taken as the old run's).
    let outcome = estimator.on_frame(frame(
        30,
        start + 30.0 * FRAME_MS,
        start + 30.0 * FRAME_MS + 50.0,
        3_900,
    ));
    assert!(!outcome.congested);
}

/// WebTransport: seq 1 overtakes the big seq 0 keyframe. The run starts at
/// 1 without a 2 s hold, and seq 0 is a late frame, not a new run.
#[test]
fn seq_1_before_seq_0_starts_without_a_hold() {
    let mut estimator = estimator();
    estimator.on_frame(frame(1, FRAME_MS, FRAME_MS + 50.0, 3_900));
    estimator.on_frame(frame(0, 0.0, FRAME_MS + 60.0, 23_400));
    for seq in 2..90u64 {
        let ts = seq as f64 * FRAME_MS;
        estimator.on_frame(frame(seq, ts, ts + 50.0, 3_900));
        assert_eq!(estimator.held_frames(), 0, "held at seq {seq}");
    }
    assert!(estimator.report(3_100.0).is_some());
}

/// The first frame is lost: the run starts at seq 1, nothing is held.
#[test]
fn a_lost_seq_0_holds_nothing() {
    let mut estimator = estimator();
    for seq in 1..90u64 {
        let ts = seq as f64 * FRAME_MS;
        estimator.on_frame(frame(seq, ts, ts + 50.0, 3_900));
        assert_eq!(estimator.held_frames(), 0, "held at seq {seq}");
    }
    assert!(estimator.report(3_100.0).is_some());
}

/// After a restart, a straggler from the old run (high sequence number,
/// old timestamp) is ignored: it is neither held nor taken as a frame.
#[test]
fn a_straggler_from_the_old_run_is_ignored() {
    let mut estimator = estimator();
    for seq in 0..150u64 {
        let ts = seq as f64 * FRAME_MS;
        estimator.on_frame(frame(seq, ts, ts + 50.0, 3_900));
    }
    let start = 10_000.0;
    for seq in 0..5u64 {
        let ts = start + seq as f64 * FRAME_MS;
        estimator.on_frame(frame(seq, ts, ts + 50.0, 3_900));
    }
    // Old run's seq 150, sent at 5 s, delivered very late.
    estimator.on_frame(frame(150, 150.0 * FRAME_MS, start + 200.0, 3_900));
    assert_eq!(estimator.held_frames(), 0);
    for seq in 5..90u64 {
        let ts = start + seq as f64 * FRAME_MS;
        estimator.on_frame(frame(seq, ts, ts + 50.0, 3_900));
        assert_eq!(estimator.held_frames(), 0, "held at seq {seq}");
    }
}

/// Normal reordering within a run (a small frame finishing before a big
/// one) holds and releases; it never starts a new run.
#[test]
fn normal_reordering_does_not_restart_the_run() {
    let mut estimator = estimator();
    for seq in 0..120u64 {
        let ts = seq as f64 * FRAME_MS;
        estimator.on_frame(frame(seq, ts, ts + 50.0, 3_900));
    }
    let before = estimator.report(4_100.0).expect("a report");
    // 120 (a keyframe) finishes after 121 and 122.
    let ts = |seq: u64| seq as f64 * FRAME_MS;
    estimator.on_frame(frame(121, ts(121), ts(121) + 50.0, 3_900));
    estimator.on_frame(frame(122, ts(122), ts(122) + 50.0, 3_900));
    assert_eq!(estimator.held_frames(), 2);
    estimator.on_frame(frame(120, ts(120), ts(122) + 55.0, 23_400));
    assert_eq!(estimator.held_frames(), 0);
    let after = estimator.report(ts(122) + 100.0).expect("same run");
    assert_eq!(after.capacity_bps, before.capacity_bps);
}

/// A remote `u64::MAX` must not overflow; what follows is a new run.
#[test]
fn sequence_u64_max_does_not_overflow() {
    let mut estimator = estimator();
    estimator.on_frame(frame(u64::MAX - 1, 0.0, 50.0, 3_900));
    estimator.on_frame(frame(u64::MAX, FRAME_MS, FRAME_MS + 50.0, 3_900));
    for seq in 0..60u64 {
        let ts = (seq + 2) as f64 * FRAME_MS;
        estimator.on_frame(frame(seq, ts, ts + 50.0, 3_900));
        assert_eq!(estimator.held_frames(), 0);
    }
    assert!(estimator.report(2_200.0).is_some());
}

/// A hostile or broken publisher leaving a gap and flooding frames behind
/// it cannot grow the held set past the bound.
#[test]
fn held_frames_are_bounded() {
    use videocall_rate_control::viewer::MAX_HELD_FRAMES;
    let mut estimator = estimator();
    estimator.on_frame(frame(0, 0.0, 50.0, 3_900));
    let mut most = 0;
    for seq in 2..5_000u64 {
        let ts = seq as f64 * 0.2;
        estimator.on_frame(frame(seq, ts, ts + 50.0, 100));
        most = most.max(estimator.held_frames());
    }
    assert!(most <= MAX_HELD_FRAMES + 1, "held {most}");
}

/// Jitter around the threshold can flip the verdict often; the early
/// report still goes out at most once per report interval.
#[test]
fn at_most_one_early_report_per_report_interval() {
    let mut estimator = estimator();
    let mut link_free = 0.0;
    let mut early = Vec::new();
    for seq in 0..600u64 {
        let send = seq as f64 * FRAME_MS;
        // Alternate 100 ms of a narrow link with 100 ms of a wide one after
        // 2 s: the queue keeps forming and clearing.
        let bps = if send < 2_000.0 || (send as u64 / 100).is_multiple_of(2) {
            2e6
        } else {
            3e5
        };
        let arrival = link(send, 3_900, bps, &mut link_free);
        if let Some(report) = estimator
            .on_frame(frame(seq, send, arrival, 3_900))
            .early_report
        {
            early.push((arrival, report));
        }
    }
    assert!(!early.is_empty(), "the scenario must congest");
    assert!(
        early.windows(2).all(|w| w[1].0 - w[0].0 >= 500.0),
        "{:?}",
        early.iter().map(|(t, _)| *t).collect::<Vec<_>>()
    );
}

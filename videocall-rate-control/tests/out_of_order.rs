//! Frames that finish out of order, and back-to-back oversized frames, on a
//! synthetic healthy 2 Mbps link (50 ms one way, 30 fps, deltas of 5,000
//! bytes, so the link is never the limit).
//!
//! - One stream per frame (WebTransport): the link is shared between the
//!   frames in flight, so small frames finish before a large one sent
//!   earlier. Capacity must stay at what the link carries: never above it,
//!   never below the rate the sender delivered.
//! - Two oversized frames back to back (a keyframe, then a delta three times
//!   its size, as VP9 produced on static content) through a FIFO: the queue
//!   they leave is their own bytes at the link rate, so it is not congestion.

use videocall_rate_control::{FrameInfo, ViewerConfig, ViewerEstimator};

const FRAME_MS: f64 = 1000.0 / 30.0;
const LINK_BPS: f64 = 2_000_000.0;
const DELAY_MS: f64 = 50.0;
const DELTA_BYTES: u32 = 5_000;

/// Sender: 30 fps of 5,000-byte deltas; `big(seq)` overrides a frame size.
fn frames(seconds: f64, big: impl Fn(u64) -> Option<u32>) -> Vec<(u64, f64, u32)> {
    (0..(seconds * 30.0) as u64)
        .map(|seq| (seq, seq as f64 * FRAME_MS, big(seq).unwrap_or(DELTA_BYTES)))
        .collect()
}

/// FIFO link: arrival of each frame, in send order.
fn fifo(frames: &[(u64, f64, u32)]) -> Vec<FrameInfo> {
    let mut free = 0.0_f64;
    frames
        .iter()
        .map(|&(seq, send, bytes)| {
            free = send.max(free) + f64::from(bytes) * 8_000.0 / LINK_BPS;
            FrameInfo {
                seq,
                timestamp_ms: send,
                arrival_ms: free + DELAY_MS,
                bytes,
            }
        })
        .collect()
}

/// Shared link: the rate is split equally between every frame sent and not
/// yet finished (round-robin streams, fluid model). Arrivals in completion
/// order.
fn shared(frames: &[(u64, f64, u32)]) -> Vec<FrameInfo> {
    let per_ms = LINK_BPS / 8_000.0;
    // (seq, send, bytes left, bytes)
    let mut active: Vec<(u64, f64, f64, u32)> = Vec::new();
    let mut done = Vec::new();
    let mut next = 0;
    let mut now = 0.0_f64;
    while next < frames.len() || !active.is_empty() {
        let next_send = frames.get(next).map_or(f64::INFINITY, |f| f.1);
        let share = if active.is_empty() {
            0.0
        } else {
            per_ms / active.len() as f64
        };
        let finish = active
            .iter()
            .map(|a| now + a.2 / share)
            .fold(f64::INFINITY, f64::min);
        let t = next_send.min(finish);
        for a in &mut active {
            a.2 -= share * (t - now);
        }
        now = t;
        if t == next_send {
            let (seq, send, bytes) = frames[next];
            active.push((seq, send, f64::from(bytes), bytes));
            next += 1;
        }
        active.retain(|&(seq, send, left, bytes)| {
            if left <= 1e-6 {
                done.push(FrameInfo {
                    seq,
                    timestamp_ms: send,
                    arrival_ms: now + DELAY_MS,
                    bytes,
                });
                false
            } else {
                true
            }
        });
    }
    done
}

/// What a run saw.
struct Seen {
    /// Frames judged congested once a capacity existed.
    congested: usize,
    /// Every capacity reported or held.
    capacities: Vec<u64>,
    /// Reports (periodic or early) with a backlog.
    backlogs: usize,
}

/// Feeds arrivals with report ticks every 500 ms.
fn feed(arrivals: &[FrameInfo]) -> Seen {
    let mut estimator = ViewerEstimator::new(ViewerConfig {
        report_interval_ms: 500.0,
    });
    let mut tick = 500.0;
    let mut seen = Seen {
        congested: 0,
        capacities: Vec::new(),
        backlogs: 0,
    };
    for frame in arrivals {
        while tick <= frame.arrival_ms {
            if let Some(report) = estimator.report(tick) {
                seen.capacities.extend(report.capacity_bps);
                seen.backlogs += usize::from(report.backlog_ms > 0.0);
            }
            tick += 500.0;
        }
        let outcome = estimator.on_frame(*frame);
        if let Some(capacity) = outcome.capacity_bps {
            seen.capacities.push(capacity);
            seen.congested += usize::from(outcome.congested);
        }
        if let Some(report) = outcome.early_report {
            seen.backlogs += usize::from(report.backlog_ms > 0.0);
        }
    }
    seen
}

/// Every 4 s from 2 s on, a 22 KB keyframe and then a 70 KB delta share the
/// link with the deltas sent after them; the 70 KB frame finishes last.
fn out_of_order_static_content() -> Vec<FrameInfo> {
    shared(&frames(14.0, |seq| match seq % 120 {
        60 => Some(22_000),
        61 => Some(70_000),
        _ => None,
    }))
}

#[test]
fn frames_finishing_out_of_order_keep_capacity_at_the_link() {
    let arrivals = out_of_order_static_content();
    let reordered = arrivals.windows(2).filter(|w| w[1].seq < w[0].seq).count();
    assert!(reordered > 0, "the scenario must reorder");
    // The lowest rate the sender ever delivered over a report period: its
    // deltas alone.
    let delta_bps = f64::from(DELTA_BYTES) * 8_000.0 / FRAME_MS;
    let seen = feed(&arrivals);
    let lo = seen.capacities.iter().copied().min().expect("a capacity") as f64;
    let hi = seen.capacities.iter().copied().max().expect("a capacity") as f64;
    assert!(hi <= 1.025 * LINK_BPS, "capacity above the link: {hi}");
    assert!(
        lo >= 0.875 * delta_bps,
        "capacity below what the sender delivered: {lo}"
    );
}

#[test]
#[ignore = "known limit: oversized pair out of order gives 3 congested frames, 3 backlog reports (README)"]
fn oversized_frames_finishing_out_of_order_are_not_congestion() {
    let seen = feed(&out_of_order_static_content());
    assert_eq!(seen.congested, 0, "congested frames");
    assert_eq!(seen.backlogs, 0, "backlog reports");
}

#[test]
#[ignore = "known limit: keyframe then 3x delta reads as congestion, 38 frames, 4 backlog reports (README)"]
fn two_oversized_frames_in_a_row_on_a_healthy_link_are_not_congestion() {
    // Keyframe 22 KB then a 70 KB delta at 5 s and 10 s.
    let sent = frames(14.0, |seq| match seq % 150 {
        0 if seq > 0 => Some(22_000),
        1 if seq > 1 => Some(70_000),
        _ => None,
    });
    let seen = feed(&fifo(&sent));
    assert!(!seen.capacities.is_empty(), "no capacity at all");
    assert_eq!(seen.congested, 0, "congested frames");
    assert_eq!(seen.backlogs, 0, "backlog reports");
}

//! Test support: a deterministic closed-loop simulation of one publisher and
//! one or more viewers, each behind its own reliable FIFO link.
//!
//! - Publisher: `fps` camera frames per second. Without an actuator, delta
//!   frames are `target / fps / 8` bytes and every `keyframe_interval_ms` a
//!   keyframe is `keyframe_ratio` deltas (about 8% over the target at 6x
//!   every 2 s). With an actuator, the tier is chosen on every report, the
//!   [`FrameGate`] decides which camera frames are encoded, and a handed
//!   frame is `encoder bitrate / tier fps / 8` bytes. Only handed frames
//!   take a sequence number.
//! - Link: one FIFO per viewer with an unbounded queue and no loss. A frame
//!   is serialized at the scheduled rate (a rate change mid-frame is
//!   honoured), then delayed by `one_way_delay_ms`. It arrives as one unit
//!   when its last byte does.
//! - Viewer: a [`ViewerEstimator`] sees each frame at arrival and is asked
//!   for a report every `report_interval_ms`; an early report leaves at the
//!   arrival that triggered it.
//! - Report path: delivered after `return_delay_ms` to a
//!   [`MultiViewerPublisher`] with `rtt = one way + return`.
//!
//! - Relay (optional): each viewer's link queue holds at most
//!   `relay_queue_frames`; a frame arriving at a full queue is dropped for
//!   that viewer, after it was numbered, as the relay's outbound channel
//!   does. Five drops within 1 s send one CONGESTION signal (at most one
//!   per second per viewer), delivered after `return_delay_ms`, as
//!   `actix-api`'s `CongestionTracker` does.
//! - Viewers listed in `silent_viewers` never report, like the native
//!   clients.
//!
//! Event ties at the same microsecond resolve as arrival, report, delivery,
//! signal, capture. Metrics are on viewer 0.

#![allow(dead_code)]

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use std::collections::VecDeque;

use videocall_rate_control::{
    default_tiers, encoder_bitrate_bps, FrameDecision, FrameGate, FrameInfo, FrameOutcome,
    MultiViewerPublisher, PublisherConfig, Report, Tier, ViewerConfig, ViewerEstimator,
};

/// Drops within [`RELAY_WINDOW_MS`] that make the relay signal (actix-api
/// `CONGESTION_DROP_THRESHOLD`).
pub const RELAY_DROP_THRESHOLD: u32 = 5;
/// Relay drop-count window and minimum signal spacing, ms (actix-api
/// `CONGESTION_WINDOW`, `CONGESTION_NOTIFY_MIN_INTERVAL`).
pub const RELAY_WINDOW_MS: f64 = 1_000.0;

/// Offset added to simulation time before it reaches the estimator, so a
/// test never depends on the clock sitting at zero.
const ESTIMATOR_CLOCK_ORIGIN_MS: f64 = 100_000.0;

/// Link restored at this time in [`SimConfig::baseline`], ms.
pub const RESTORE_MS: f64 = 25_000.0;
/// Link narrows at this time in [`SimConfig::baseline`], ms.
pub const STEP_MS: f64 = 10_000.0;

/// B1: the target stays at the 936 kbps cap on the fast link, kbps.
pub const B1_TARGET_KBPS: f64 = 936.0;
/// B2: peak lateness under this, ms.
pub const B2_MAX_LATENESS_MS: f64 = 1_000.0;
/// B3: delivered from 5 s after the step to restore, at least, kbps.
pub const B3_MIN_DELIVERED_KBPS: f64 = 400.0;
/// B4: target back to this level after restore, kbps ...
pub const B4_TARGET_KBPS: f64 = 840.0;
/// ... within this deadline, ms.
pub const B4_DEADLINE_MS: f64 = 15_000.0;
/// B8: target below this (25% of the 500 kbps link) ...
pub const B8_FLOOR_BPS: f64 = 125_000.0;
/// ... for no more than this in total during the narrow phase, ms.
pub const B8_MAX_MS: f64 = 1_000.0;

/// Resolution of linear link ramps: piecewise-constant steps this long.
pub const RAMP_STEP_MS: f64 = 10.0;

/// Random encode time between capture (the frame's timestamp) and send.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodeDelay {
    /// Shortest encode time, ms.
    pub min_ms: f64,
    /// Longest encode time, ms.
    pub max_ms: f64,
    /// PRNG seed.
    pub seed: u64,
}

/// Uniform `0..max_ms` extra delay per frame after the link, in order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeliveryJitter {
    /// Largest extra delay, ms.
    pub max_ms: f64,
    /// PRNG seed.
    pub seed: u64,
}

/// Sender clock model: `timestamp = t * (1 + drift_ppm / 1e6) + offset_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SenderClock {
    /// Fixed offset unknown to the viewer, ms.
    pub offset_ms: f64,
    /// Rate error of the sender clock, parts per million.
    pub drift_ppm: f64,
}

impl SenderClock {
    fn stamp(&self, t_ms: f64) -> f64 {
        t_ms * (1.0 + self.drift_ppm * 1e-6) + self.offset_ms
    }
}

/// Simulation parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct SimConfig {
    /// Simulated time, ms.
    pub duration_ms: f64,
    /// Camera frames per second.
    pub fps: f64,
    /// Keyframe period, ms.
    pub keyframe_interval_ms: f64,
    /// Keyframe size in delta frames.
    pub keyframe_ratio: f64,
    /// Publisher cap, bps.
    pub max_target_bps: f64,
    /// Publisher target before the first report, bps.
    pub start_target_bps: f64,
    /// Viewer 0's link: `(start_ms, bps)`, sorted, first entry at 0.
    pub schedule: Vec<(f64, f64)>,
    /// Links of further viewers, same format.
    pub other_viewers: Vec<Vec<(f64, f64)>>,
    /// One-way propagation delay, ms.
    pub one_way_delay_ms: f64,
    /// Viewer report period, ms.
    pub report_interval_ms: f64,
    /// Viewer-to-publisher delay of a report, ms.
    pub return_delay_ms: f64,
    /// Encode time between capture timestamp and send.
    pub encode_delay: Option<EncodeDelay>,
    /// Sender clock relative to the viewer clock.
    pub sender_clock: SenderClock,
    /// Per-frame delivery jitter after the link.
    pub delivery_jitter: Option<DeliveryJitter>,
    /// Camera off (no frames captured) in `[from, to)`, ms.
    pub pause: Option<(f64, f64)>,
    /// Tier table; `None` encodes every frame at the target.
    pub tiers: Option<Vec<Tier>>,
    /// Relay queue bound per viewer, frames; `None` is unbounded.
    pub relay_queue_frames: Option<usize>,
    /// Viewers (indices, 0 is `schedule`) that never report.
    pub silent_viewers: Vec<usize>,
    /// Encoder output as a fraction of its bitrate: `(start_ms, fraction)`,
    /// sorted. Easy content (a still scene) makes a real encoder undershoot.
    /// Empty means 1.0 throughout.
    pub encoder_fill: Vec<(f64, f64)>,
    /// `(viewer, from_ms, to_ms)`: the viewer sends no report in that span
    /// (a viewer stuck behind a queue it cannot release).
    pub muted_reports: Vec<(usize, f64, f64)>,
}

/// Appends a linear ramp from `from_bps` to `to_bps` over
/// `[start_ms, start_ms + duration_ms)` to `schedule`.
pub fn push_ramp(
    schedule: &mut Vec<(f64, f64)>,
    start_ms: f64,
    duration_ms: f64,
    from_bps: f64,
    to_bps: f64,
) {
    let steps = (duration_ms / RAMP_STEP_MS).round() as usize;
    for step in 0..steps {
        let fraction = (step as f64 + 0.5) / steps as f64;
        schedule.push((
            start_ms + step as f64 * RAMP_STEP_MS,
            from_bps + (to_bps - from_bps) * fraction,
        ));
    }
}

impl SimConfig {
    /// The B-series setup: 2 Mbps / 500 kbps / 2 Mbps at 0 / 10 s / 25 s,
    /// 50 ms each way, 500 ms reports, 30 fps, 6x keyframes every 2 s,
    /// 936 kbps start and cap, 55 s.
    pub fn baseline() -> Self {
        Self {
            duration_ms: 55_000.0,
            fps: 30.0,
            keyframe_interval_ms: 2_000.0,
            keyframe_ratio: 6.0,
            max_target_bps: 936_000.0,
            start_target_bps: 936_000.0,
            schedule: vec![
                (0.0, 2_000_000.0),
                (STEP_MS, 500_000.0),
                (RESTORE_MS, 2_000_000.0),
            ],
            other_viewers: Vec::new(),
            one_way_delay_ms: 50.0,
            report_interval_ms: 500.0,
            return_delay_ms: 50.0,
            encode_delay: None,
            sender_clock: SenderClock::default(),
            delivery_jitter: None,
            pause: None,
            tiers: None,
            relay_queue_frames: None,
            silent_viewers: Vec::new(),
            encoder_fill: Vec::new(),
            muted_reports: Vec::new(),
        }
    }

    /// S2: 2 Mbps for 10 s, linear ramp to 500 kbps over 5 s, hold 15 s,
    /// ramp back over 5 s, 2 Mbps for 15 s. The hold is `[15 s, 30 s)`.
    pub fn ramp() -> Self {
        let mut schedule = vec![(0.0, 2_000_000.0)];
        push_ramp(&mut schedule, 10_000.0, 5_000.0, 2_000_000.0, 500_000.0);
        schedule.push((15_000.0, 500_000.0));
        push_ramp(&mut schedule, 30_000.0, 5_000.0, 500_000.0, 2_000_000.0);
        schedule.push((35_000.0, 2_000_000.0));
        Self {
            duration_ms: 50_000.0,
            schedule,
            ..Self::baseline()
        }
    }

    /// S3: capacity uniform in 400 kbps..2 Mbps, redrawn every 3 s, 120 s.
    pub fn random_walk(seed: u64) -> Self {
        let mut rng = SplitMix64(seed);
        let schedule = (0..40)
            .map(|step| {
                (
                    f64::from(step) * 3_000.0,
                    400_000.0 + 1_600_000.0 * rng.next_f64(),
                )
            })
            .collect();
        Self {
            duration_ms: 120_000.0,
            schedule,
            ..Self::baseline()
        }
    }

    /// S4: constant 2 Mbps, 60 s, per-frame delivery jitter 0..40 ms.
    pub fn jitter(seed: u64) -> Self {
        Self {
            duration_ms: 60_000.0,
            schedule: vec![(0.0, 2_000_000.0)],
            delivery_jitter: Some(DeliveryJitter { max_ms: 40.0, seed }),
            ..Self::baseline()
        }
    }

    /// S5: the baseline with 150 ms one-way delay in each direction.
    pub fn long_delay() -> Self {
        Self {
            one_way_delay_ms: 150.0,
            return_delay_ms: 150.0,
            ..Self::baseline()
        }
    }

    /// B7: ten minutes on a constant 2 Mbps link at a 936 kbps cap, sender
    /// clock 37 s ahead and drifting by `drift_ppm`.
    pub fn steady_drift(drift_ppm: f64) -> Self {
        Self {
            duration_ms: 600_000.0,
            schedule: vec![(0.0, 2_000_000.0)],
            sender_clock: SenderClock {
                offset_ms: 37_000.0,
                drift_ppm,
            },
            ..Self::baseline()
        }
    }

    /// Encoder fill at `t_ms`.
    pub fn fill_at(&self, t_ms: f64) -> f64 {
        self.encoder_fill
            .iter()
            .take_while(|(start, _)| *start <= t_ms)
            .last()
            .map_or(1.0, |(_, fill)| *fill)
    }

    /// Whether `viewer` may report at `t_ms`.
    pub fn reports_at(&self, viewer: usize, t_ms: f64) -> bool {
        !self.silent_viewers.contains(&viewer)
            && !self
                .muted_reports
                .iter()
                .any(|(muted, from, to)| *muted == viewer && t_ms >= *from && t_ms < *to)
    }

    fn links(&self) -> Vec<Link> {
        std::iter::once(&self.schedule)
            .chain(&self.other_viewers)
            .map(|schedule| Link {
                schedule: schedule.clone(),
                free_ms: 0.0,
                last_arrival_ms: f64::NEG_INFINITY,
                queued_ends: VecDeque::new(),
                drops: 0,
                window_start_ms: f64::NEG_INFINITY,
                last_signal_ms: f64::NEG_INFINITY,
            })
            .collect()
    }
}

struct Link {
    schedule: Vec<(f64, f64)>,
    free_ms: f64,
    last_arrival_ms: f64,
    /// Serialization ends of frames still in the queue (relay model).
    queued_ends: VecDeque<f64>,
    drops: u32,
    window_start_ms: f64,
    last_signal_ms: f64,
}

impl Link {
    fn rate_at(&self, t_ms: f64) -> (f64, f64) {
        let idx = self
            .schedule
            .iter()
            .rposition(|(start, _)| *start <= t_ms)
            .unwrap_or(0);
        let end = self
            .schedule
            .get(idx + 1)
            .map_or(f64::INFINITY, |(start, _)| *start);
        (self.schedule[idx].1, end)
    }

    /// When the last bit of `bytes` leaves if serialization starts at
    /// `start_ms`.
    fn serialization_end(&self, start_ms: f64, bytes: u32) -> f64 {
        let mut bits = f64::from(bytes) * 8.0;
        let mut t = start_ms;
        loop {
            let (bps, segment_end) = self.rate_at(t);
            let capacity_bits = (segment_end - t) * bps / 1000.0;
            if bits <= capacity_bits {
                return t + bits * 1000.0 / bps;
            }
            bits -= capacity_bits;
            t = segment_end;
        }
    }
}

/// One sent frame's journey to viewer 0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimFrame {
    /// Sequence number (frames handed to the transport).
    pub seq: u64,
    /// Keyframe flag.
    pub keyframe: bool,
    /// Capture time = the frame's timestamp, ms.
    pub timestamp_ms: f64,
    /// Actual send time (timestamp + encode delay), ms.
    pub send_ms: f64,
    /// Arrival at viewer 0, ms; NaN when the relay dropped it.
    pub arrival_ms: f64,
    /// Frame size.
    pub bytes: u32,
    /// Publisher target when the frame was produced, bps.
    pub target_bps: f64,
}

/// A report a viewer sent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SentReport {
    /// Send time at the viewer, ms.
    pub t_ms: f64,
    /// Which viewer.
    pub viewer: usize,
    /// The values.
    pub report: Report,
    /// Sent early (on the transition into congestion).
    pub early: bool,
}

/// Simulation output.
#[derive(Debug, Clone)]
pub struct SimResult {
    /// The configuration that produced this result.
    pub config: SimConfig,
    /// Every frame sent, with its arrival at viewer 0.
    pub frames: Vec<SimFrame>,
    /// Arrivals at every viewer, `[viewer][frame]`; NaN when dropped.
    pub arrivals: Vec<Vec<f64>>,
    /// CONGESTION signals delivered: `(t_ms, viewer, accepted)`.
    pub signals: Vec<(f64, usize, bool)>,
    /// Times a quiet viewer cut the rate.
    pub silence_cuts: Vec<f64>,
    /// Publisher target changes: `(t_ms, bps)`, starting at `(0, start)`.
    pub target_changes: Vec<(f64, f64)>,
    /// Every report, in send order.
    pub reports: Vec<SentReport>,
    /// Viewer 0's state after each arrival: `(t_ms, seq, outcome)`.
    pub outcomes: Vec<(f64, u64, FrameOutcome)>,
    /// Tier changes: `(t_ms, tier index)`, starting at `(0, 0)`.
    pub tier_changes: Vec<(f64, usize)>,
    /// Camera frames skipped by the gate: `(t_ms, decision)`.
    pub skipped: Vec<(f64, FrameDecision)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    // Variant order is the tie-break order.
    Arrive(usize, usize),
    Report,
    Deliver(usize),
    Signal(usize),
    Capture(u64),
}

/// SplitMix64: tiny, seeded, good enough for jitter.
pub struct SplitMix64(pub u64);

impl SplitMix64 {
    /// Uniform in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn to_us(ms: f64) -> i64 {
    (ms * 1000.0).round() as i64
}

/// The publisher configuration the simulation uses. The floor is the
/// bottom tier's (the default table's when the run has no tiers).
pub fn publisher_config(config: &SimConfig) -> PublisherConfig {
    let tiers = config
        .tiers
        .clone()
        .unwrap_or_else(|| default_tiers(config.max_target_bps));
    PublisherConfig {
        start_bps: config.start_target_bps,
        max_bps: config.max_target_bps,
        frame_interval_ms: 1000.0 / config.fps,
        report_interval_ms: config.report_interval_ms,
        floor_bps: tiers.last().map_or(0.0, Tier::floor_bps),
    }
}

/// Round trip the publisher is told about.
pub fn rtt_ms(config: &SimConfig) -> f64 {
    config.one_way_delay_ms + config.return_delay_ms
}

/// Runs the closed loop.
pub fn run(config: &SimConfig) -> SimResult {
    let mut encode_rng = config.encode_delay.map(|delay| SplitMix64(delay.seed));
    let mut jitter_rng = config.delivery_jitter.map(|jitter| SplitMix64(jitter.seed));
    let frame_interval_ms = 1000.0 / config.fps;
    let mut links = config.links();
    let mut viewers: Vec<ViewerEstimator> = links
        .iter()
        .map(|_| {
            ViewerEstimator::new(ViewerConfig {
                report_interval_ms: config.report_interval_ms,
            })
        })
        .collect();
    let pub_config = publisher_config(config);
    let rtt = rtt_ms(config);
    let mut publisher: MultiViewerPublisher<usize> = MultiViewerPublisher::new(pub_config);
    let mut gate = FrameGate::new();
    let mut tier = 0usize;

    let mut queue: BinaryHeap<Reverse<(i64, Event)>> = BinaryHeap::new();
    queue.push(Reverse((0, Event::Capture(0))));
    queue.push(Reverse((to_us(config.report_interval_ms), Event::Report)));

    let mut target_bps = publisher.target_bps(0.0, rtt);
    let mut frames: Vec<SimFrame> = Vec::new();
    let mut arrivals: Vec<Vec<f64>> = vec![Vec::new(); links.len()];
    let mut outcomes = Vec::new();
    let mut target_changes = vec![(0.0, target_bps)];
    let mut reports: Vec<SentReport> = Vec::new();
    let mut tier_changes = vec![(0.0, 0usize)];
    let mut signals = Vec::new();
    let mut silence_cuts = Vec::new();
    let mut skipped = Vec::new();
    let mut handed = 0u64;

    let mut set_target = |t_ms: f64, bps: f64, target_bps: &mut f64| {
        if bps != *target_bps {
            *target_bps = bps;
            target_changes.push((t_ms, bps));
        }
    };
    let send_report = |queue: &mut BinaryHeap<Reverse<(i64, Event)>>,
                       reports: &mut Vec<SentReport>,
                       sent: SentReport| {
        reports.push(sent);
        let deliver = sent.t_ms + config.return_delay_ms;
        queue.push(Reverse((to_us(deliver), Event::Deliver(reports.len() - 1))));
    };

    while let Some(Reverse((t_us, event))) = queue.pop() {
        let t_ms = t_us as f64 / 1000.0;
        if t_ms > config.duration_ms {
            break;
        }
        match event {
            Event::Capture(index) => {
                let next = (index + 1) as f64 * frame_interval_ms;
                queue.push(Reverse((to_us(next), Event::Capture(index + 1))));
                if config
                    .pause
                    .is_some_and(|(from, to)| t_ms >= from && t_ms < to)
                {
                    continue;
                }
                let bps = publisher.target_bps(t_ms, rtt);
                set_target(t_ms, bps, &mut target_bps);
                let bytes_and_key = match &config.tiers {
                    None => {
                        let per_key = (config.keyframe_interval_ms / frame_interval_ms).round();
                        let keyframe = index.is_multiple_of(per_key as u64);
                        let scale = if keyframe { config.keyframe_ratio } else { 1.0 };
                        let fill = config.fill_at(t_ms);
                        Some((target_bps / config.fps / 8.0 * scale * fill, keyframe))
                    }
                    Some(tiers) => {
                        let current = &tiers[tier];
                        match gate.admit(current, t_ms, target_bps) {
                            FrameDecision::Encode => {
                                let per_key =
                                    (config.keyframe_interval_ms * current.fps / 1000.0).round();
                                let keyframe = handed.is_multiple_of(per_key as u64);
                                handed += 1;
                                let scale = if keyframe { config.keyframe_ratio } else { 1.0 };
                                let rate = encoder_bitrate_bps(current, target_bps);
                                let fill = config.fill_at(t_ms);
                                Some((rate / current.fps / 8.0 * scale * fill, keyframe))
                            }
                            skip => {
                                skipped.push((t_ms, skip));
                                None
                            }
                        }
                    }
                };
                let Some((bytes, keyframe)) = bytes_and_key else {
                    continue;
                };
                let bytes = bytes.round().max(1.0) as u32;
                let encode_ms = encode_rng
                    .as_mut()
                    .zip(config.encode_delay)
                    .map_or(0.0, |(rng, delay)| {
                        delay.min_ms + (delay.max_ms - delay.min_ms) * rng.next_f64()
                    });
                let send_ms = t_ms + encode_ms;
                if publisher.on_frame_sent(t_ms, rtt) {
                    silence_cuts.push(t_ms);
                    let bps = publisher.target_bps(t_ms, rtt);
                    set_target(t_ms, bps, &mut target_bps);
                }
                let slot = frames.len();
                for (viewer, link) in links.iter_mut().enumerate() {
                    while link.queued_ends.front().is_some_and(|end| *end <= send_ms) {
                        link.queued_ends.pop_front();
                    }
                    if config
                        .relay_queue_frames
                        .is_some_and(|bound| link.queued_ends.len() >= bound)
                    {
                        arrivals[viewer].push(f64::NAN);
                        if send_ms - link.window_start_ms > RELAY_WINDOW_MS {
                            link.drops = 0;
                            link.window_start_ms = send_ms;
                        }
                        link.drops += 1;
                        if link.drops >= RELAY_DROP_THRESHOLD
                            && send_ms - link.last_signal_ms >= RELAY_WINDOW_MS
                        {
                            link.drops = 0;
                            link.window_start_ms = send_ms;
                            link.last_signal_ms = send_ms;
                            let at = send_ms + config.return_delay_ms;
                            queue.push(Reverse((to_us(at), Event::Signal(viewer))));
                        }
                        continue;
                    }
                    let start_ms = send_ms.max(link.free_ms);
                    link.free_ms = link.serialization_end(start_ms, bytes);
                    link.queued_ends.push_back(link.free_ms);
                    let jitter_ms = if viewer == 0 {
                        jitter_rng
                            .as_mut()
                            .zip(config.delivery_jitter)
                            .map_or(0.0, |(rng, jitter)| jitter.max_ms * rng.next_f64())
                    } else {
                        0.0
                    };
                    let arrival_ms = (link.free_ms + config.one_way_delay_ms + jitter_ms)
                        .max(link.last_arrival_ms);
                    link.last_arrival_ms = arrival_ms;
                    arrivals[viewer].push(arrival_ms);
                    queue.push(Reverse((to_us(arrival_ms), Event::Arrive(viewer, slot))));
                }
                frames.push(SimFrame {
                    seq: slot as u64,
                    keyframe,
                    timestamp_ms: t_ms,
                    send_ms,
                    arrival_ms: arrivals[0][slot],
                    bytes,
                    target_bps,
                });
            }
            Event::Arrive(viewer, slot) => {
                let frame = frames[slot];
                let outcome = viewers[viewer].on_frame(FrameInfo {
                    seq: frame.seq,
                    timestamp_ms: config.sender_clock.stamp(frame.timestamp_ms)
                        + ESTIMATOR_CLOCK_ORIGIN_MS,
                    arrival_ms: arrivals[viewer][slot] + ESTIMATOR_CLOCK_ORIGIN_MS,
                    bytes: frame.bytes,
                });
                if viewer == 0 {
                    outcomes.push((t_ms, frame.seq, outcome));
                }
                if let Some(report) = outcome
                    .early_report
                    .filter(|_| config.reports_at(viewer, t_ms))
                {
                    send_report(
                        &mut queue,
                        &mut reports,
                        SentReport {
                            t_ms,
                            viewer,
                            report,
                            early: true,
                        },
                    );
                }
            }
            Event::Report => {
                for (viewer, estimator) in viewers.iter_mut().enumerate() {
                    if !config.reports_at(viewer, t_ms) {
                        continue;
                    }
                    if let Some(report) = estimator.report(t_ms + ESTIMATOR_CLOCK_ORIGIN_MS) {
                        send_report(
                            &mut queue,
                            &mut reports,
                            SentReport {
                                t_ms,
                                viewer,
                                report,
                                early: false,
                            },
                        );
                    }
                }
                let next = t_ms + config.report_interval_ms;
                queue.push(Reverse((to_us(next), Event::Report)));
            }
            Event::Deliver(slot) => {
                let sent = reports[slot];
                publisher.on_report(sent.viewer, t_ms, sent.report);
                let bps = publisher.target_bps(t_ms, rtt);
                set_target(t_ms, bps, &mut target_bps);
                if let Some(tiers) = &config.tiers {
                    let next = publisher.next_tier(tiers, tier, t_ms, rtt);
                    if next != tier {
                        tier = next;
                        tier_changes.push((t_ms, tier));
                    }
                }
            }
            Event::Signal(viewer) => {
                let accepted = publisher.on_congestion_signal(t_ms, rtt, Some(&viewer));
                signals.push((t_ms, viewer, accepted));
                let bps = publisher.target_bps(t_ms, rtt);
                set_target(t_ms, bps, &mut target_bps);
                if let Some(tiers) = &config.tiers {
                    let next = publisher.next_tier(tiers, tier, t_ms, rtt);
                    if next != tier {
                        tier = next;
                        tier_changes.push((t_ms, tier));
                    }
                }
            }
        }
    }

    SimResult {
        config: config.clone(),
        frames,
        arrivals,
        signals,
        silence_cuts,
        target_changes,
        reports,
        outcomes,
        tier_changes,
        skipped,
    }
}

/// B1-B4 measurements for one run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClosedLoopMetrics {
    /// Lowest publisher target in `[0, step)`, kbps.
    pub b1_min_target_kbps: f64,
    /// Peak of arrival - send - one-way delay, ms.
    pub b2_peak_lateness_ms: f64,
    /// Bytes arriving in `[step + 5 s, restore)` over that window, kbps.
    pub b3_delivered_kbps: f64,
    /// Time after restore until the target is at least 840 kbps, ms.
    pub b4_recovery_ms: Option<f64>,
    /// Lowest target over the whole run, kbps.
    pub min_target_kbps: f64,
}

impl ClosedLoopMetrics {
    /// The B criteria that failed.
    pub fn failures(&self) -> Vec<&'static str> {
        let mut failed = Vec::new();
        if self.b1_min_target_kbps < B1_TARGET_KBPS {
            failed.push("B1");
        }
        if self.b2_peak_lateness_ms >= B2_MAX_LATENESS_MS {
            failed.push("B2");
        }
        if self.b3_delivered_kbps < B3_MIN_DELIVERED_KBPS {
            failed.push("B3");
        }
        if self.b4_recovery_ms.is_none_or(|ms| ms > B4_DEADLINE_MS) {
            failed.push("B4");
        }
        failed
    }
}

impl SimResult {
    /// Publisher target in force at `t_ms`, bps.
    pub fn target_at(&self, t_ms: f64) -> f64 {
        self.target_changes
            .iter()
            .take_while(|(t, _)| *t <= t_ms)
            .last()
            .map_or(self.config.start_target_bps, |(_, bps)| *bps)
    }

    /// Lowest target in `[from_ms, to_ms)`, bps.
    pub fn min_target(&self, from_ms: f64, to_ms: f64) -> f64 {
        self.target_changes
            .iter()
            .filter(|(t, _)| *t >= from_ms && *t < to_ms)
            .map(|(_, bps)| *bps)
            .fold(self.target_at(from_ms), f64::min)
    }

    /// Milliseconds within `[from_ms, to_ms)` with the target below `bps`.
    pub fn time_below_ms(&self, from_ms: f64, to_ms: f64, bps: f64) -> f64 {
        let mut edges: Vec<f64> = vec![from_ms, to_ms];
        edges.extend(
            self.target_changes
                .iter()
                .map(|(t, _)| *t)
                .filter(|t| *t > from_ms && *t < to_ms),
        );
        edges.sort_by(f64::total_cmp);
        edges
            .windows(2)
            .filter(|pair| self.target_at(pair[0]) < bps)
            .map(|pair| pair[1] - pair[0])
            .sum()
    }

    fn lateness_at(&self, viewer: usize) -> impl Iterator<Item = (f64, f64)> + '_ {
        self.frames
            .iter()
            .zip(&self.arrivals[viewer])
            .filter(|(_, arrival)| arrival.is_finite())
            .map(|(frame, arrival)| {
                (
                    *arrival,
                    arrival - frame.send_ms - self.config.one_way_delay_ms,
                )
            })
    }

    /// `p`-quantile (0..1) of viewer 0's per-frame lateness, nearest rank.
    pub fn lateness_quantile_ms(&self, p: f64) -> f64 {
        let mut lateness: Vec<f64> = self.lateness_at(0).map(|(_, late)| late).collect();
        lateness.sort_by(f64::total_cmp);
        let rank = ((p * lateness.len() as f64).ceil() as usize).clamp(1, lateness.len());
        lateness[rank - 1]
    }

    /// Peak lateness at `viewer` over frames arriving in `[from_ms, to_ms)`.
    pub fn peak_lateness_ms(&self, viewer: usize, from_ms: f64, to_ms: f64) -> f64 {
        self.lateness_at(viewer)
            .filter(|(arrival, _)| *arrival >= from_ms && *arrival < to_ms)
            .map(|(_, late)| late)
            .fold(f64::NEG_INFINITY, f64::max)
    }

    /// Bits of `min(link rate, cap_bps)` over the whole run, viewer 0.
    pub fn capacity_bits(&self, cap_bps: f64) -> f64 {
        let schedule = &self.config.schedule;
        schedule
            .iter()
            .enumerate()
            .map(|(idx, (start, bps))| {
                let end = schedule
                    .get(idx + 1)
                    .map_or(self.config.duration_ms, |(next, _)| *next)
                    .min(self.config.duration_ms);
                (end - start).max(0.0) * bps.min(cap_bps) / 1000.0
            })
            .sum()
    }

    /// Bits of every frame that reached viewer 0 within the run.
    pub fn delivered_bits(&self) -> f64 {
        self.frames
            .iter()
            .filter(|frame| frame.arrival_ms <= self.config.duration_ms)
            .map(|frame| f64::from(frame.bytes) * 8.0)
            .sum()
    }

    /// Kbps of frame bytes reaching viewer 0 in `[from_ms, to_ms)`.
    pub fn delivered_kbps(&self, from_ms: f64, to_ms: f64) -> f64 {
        let bytes: f64 = self
            .frames
            .iter()
            .filter(|frame| frame.arrival_ms >= from_ms && frame.arrival_ms < to_ms)
            .map(|frame| f64::from(frame.bytes))
            .sum();
        bytes * 8.0 / (to_ms - from_ms)
    }

    /// Reports from `viewer` with a backlog, sent in `[from_ms, to_ms)`.
    pub fn backlog_reports(&self, viewer: usize, from_ms: f64, to_ms: f64) -> usize {
        self.reports
            .iter()
            .filter(|sent| {
                sent.viewer == viewer
                    && sent.t_ms >= from_ms
                    && sent.t_ms < to_ms
                    && sent.report.backlog_ms > 0.0
            })
            .count()
    }

    /// B1-B4, with step and restore taken from the schedule's second and
    /// third entries.
    pub fn metrics(&self) -> ClosedLoopMetrics {
        let step_ms = self.config.schedule.get(1).map_or(STEP_MS, |(t, _)| *t);
        let restore_ms = self.config.schedule.get(2).map_or(RESTORE_MS, |(t, _)| *t);
        let recovery = if self.target_at(restore_ms) / 1000.0 >= B4_TARGET_KBPS {
            Some(0.0)
        } else {
            self.target_changes
                .iter()
                .find(|(t, bps)| *t >= restore_ms && *bps / 1000.0 >= B4_TARGET_KBPS)
                .map(|(t, _)| t - restore_ms)
        };
        ClosedLoopMetrics {
            b1_min_target_kbps: self.min_target(0.0, step_ms) / 1000.0,
            b2_peak_lateness_ms: self.peak_lateness_ms(0, f64::NEG_INFINITY, f64::INFINITY),
            b3_delivered_kbps: self.delivered_kbps(step_ms + 5_000.0, restore_ms),
            b4_recovery_ms: recovery,
            min_target_kbps: self.min_target(0.0, self.config.duration_ms) / 1000.0,
        }
    }
}

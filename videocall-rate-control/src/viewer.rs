//! Viewer side: one [`ViewerEstimator`] per remote video stream.
//!
//! The viewer sees every frame as (timestamp, sequence, bytes, arrival) and
//! reports two numbers: the link capacity and the standing backlog. It never
//! turns a backlog into a rate; the publisher does
//! ([`crate::publisher::Publisher`]).
//!
//! # Signal: the wait each frame found
//!
//! - `d = arrival - timestamp` carries an unknown clock offset. `base` is the
//!   minimum `d` over [`BASE_BUCKETS`] x [`BASE_BUCKET_MS`];
//!   `lateness = d - base`.
//! - Lindley's recursion for a FIFO bottleneck: frame `i` waited for the part
//!   of its predecessor's lateness the timestamp gap did not cover,
//!   `wait_i = max(0, lateness_{i-1} - (ts_i - ts_{i-1}))`. That excludes
//!   frame `i`'s own transmission time.
//! - `jump_i = wait_i - wait_{i-1}` is the queue frame `i-1` added. A frame
//!   can only add what its own bytes take at the capacity `C` already known,
//!   so the jump is capped at `bytes_{i-1} * 8 / C - (ts_i - ts_{i-1})`. A
//!   single frame's jump can only still be in front of frame `i` if the link
//!   had nothing else to do, so it explains at most `jump_m - (ts_i - ts_m)`
//!   of `wait_i`. `excess_i = wait_i - max_m(that)` is the queue no single
//!   frame explains.
//! - A frame that found the link idle (`wait_i == 0`) opens the busy period.
//!   Its excess is its own service beyond its bytes at `C`:
//!   `lateness_i - bytes_i * 8 / C`. This sees a keyframe sent into a link
//!   that just narrowed, at the keyframe's own arrival.
//! - Congested when `excess > frame interval` (median timestamp gap): a
//!   queue of more than one frame interval that no one frame accounts for.
//!   A keyframe on a link at `C` or faster is fully explained by its bytes.
//!   Before the first capacity nothing is capped.
//!
//! # Capacity, with memory
//!
//! - A busy period is a run of frames with `wait > 0`: the link never idled
//!   between their arrivals, so their bytes over their arrival span is the
//!   link rate. While congested, capacity := that rate over the last
//!   [`CAPACITY_WINDOW_MS`] of the busy period. A large backlog does not
//!   change this number.
//! - Otherwise capacity is held, and raised only by a delivered rate higher
//!   than it: bytes over arrival span over one report interval in which no
//!   frame waited more than one frame interval.
//! - Before the first clean delivered window there is no capacity and no
//!   report: the publisher keeps its start rate.
//! - Silence (no frame released since the last report) sends no report:
//!   the publisher ages the last one it has and holds. New news only.
//!
//! # Sequence order
//!
//! Everything above assumes the link delivers frames in the order they were
//! sent. With one stream per frame (WebTransport) a small frame finishes
//! before a large one sent earlier, so a frame's completion no longer marks
//! when its bytes crossed. Frames are therefore released in sequence order,
//! each at the instant every earlier frame had arrived (on an in-order
//! transport that is its own arrival). A release is clean when no later
//! frame had arrived by then: only there are the bytes delivered exactly the
//! bytes released, so only clean releases carry a verdict or a capacity
//! measurement, and a busy period that opens behind an unclean release is
//! anchored at the last clean one.
//!
//! **Requirement on the sender:** a frame the sender deliberately does not
//! send (paused camera, tier frame rate, drain decimation) takes no sequence
//! number. Every other gap is a frame still in flight or lost after it was
//! numbered (a full send buffer, a shaper, a relay dropping it), and the gap
//! rule below handles those. The frames behind a gap are held until the
//! oldest of them is [`CAPACITY_WINDOW_MS`] old, or until [`MAX_HELD_FRAMES`]
//! are waiting: by then the gap can no longer be inside any measurement, so
//! it is given up. That is liveness only, not a guess about the path. The
//! order is the whole path's (a WebTransport publisher seen by a WebSocket
//! viewer still arrives out of order), so the rule is the same on every
//! transport.
//!
//! Frames behind a given-up gap are released at their own arrivals, as if
//! the lost frame had never been sent, so a loss costs a delay in the
//! estimate, never a false queue. A frame arriving after its gap was given
//! up is ignored.
//!
//! # A new run
//!
//! A publisher numbers from 0 again whenever its camera encoder restarts.
//! The estimator notices on its own, from the sequence and the timestamp: a
//! frame behind the sequence (`seq < next`) is a late frame of this run if
//! its timestamp is no later than the latest one seen, and the first frame
//! of a new run if it is later (the sender clock moved on while the
//! sequence went back). So a reordered start (seq 1 before seq 0) and a lost
//! first frame both just start the run where they land, and a straggler
//! from the old run (a timestamp before the new run's first frame) is
//! ignored. **Requirement on the sender:** timestamps of one stream come
//! from one clock that does not jump back when the camera restarts. If it
//! does, a backstop applies: when every frame for one capacity window was
//! rejected as late, the run has ended and the next frame starts a new one.
//!
//! # Report
//!
//! `(capacity, backlog)` with backlog = the latest frame's wait (for a frame
//! that found the link idle, its excess) if it was congested, else 0. On
//! the transition into congestion [`FrameOutcome::early_report`] carries one
//! report to send at once; the next one is armed only after a frame finds
//! the queue empty, and at most one goes out per report interval.

use std::collections::{BTreeMap, VecDeque};

/// Width of one bucket of the base-delay history, ms.
pub const BASE_BUCKET_MS: f64 = 10_000.0;

/// Base-delay history: 6 buckets = 60 s. A 100 ppm clock drift moves `d` by
/// at most 6 ms over that span, under one frame interval at any rate up to
/// 166 fps.
pub const BASE_BUCKETS: usize = 6;

/// Busy-period capacity window, ms: at least twice the longest arrival stall
/// in the recorded WebTransport runs (750 ms), so a stall and the burst
/// after it average out. Also the longest a sequence gap is held.
pub const CAPACITY_WINDOW_MS: f64 = 2_000.0;

/// Frames kept for the median timestamp gap: one second at 30 fps.
pub const GAP_HISTORY: usize = 31;

/// Highest frame rate the estimator budgets for, fps: twice a 60 fps
/// camera. Only bounds memory and per-frame work.
pub const MAX_FRAME_RATE_FPS: f64 = 120.0;

/// Most frames held behind one gap, and most kept since the last clean
/// release: one capacity window at [`MAX_FRAME_RATE_FPS`] (240). Reaching it
/// gives the gap up, as the window would.
pub const MAX_HELD_FRAMES: usize = (CAPACITY_WINDOW_MS / 1000.0 * MAX_FRAME_RATE_FPS) as usize;

/// What a viewer knows about one received video frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameInfo {
    /// Sender's frame sequence number (`VideoMetadata.sequence`). Must count
    /// the frames handed to the transport; see the module docs.
    pub seq: u64,
    /// Sender timestamp, sender clock, ms (`MediaPacket.timestamp`). Any
    /// fixed offset and slow drift against the viewer clock are fine.
    pub timestamp_ms: f64,
    /// Arrival of the whole frame, viewer clock, ms. Must never decrease
    /// between calls.
    pub arrival_ms: f64,
    /// Bytes the frame took on the wire (the payload size is close enough).
    pub bytes: u32,
}

/// What a viewer sends back to the publisher.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Report {
    /// Bitrate the path carries, bps.
    pub capacity_bps: u64,
    /// Standing backlog at the viewer, ms of queueing every frame pays.
    pub backlog_ms: f64,
}

impl Report {
    /// Capacity rounded to whole kbps, for the wire.
    pub fn capacity_kbps(&self) -> u32 {
        u32::try_from(self.capacity_bps.saturating_add(500) / 1000).unwrap_or(u32::MAX)
    }

    /// Backlog rounded to whole ms, for the wire.
    pub fn backlog_ms_rounded(&self) -> u32 {
        // Saturating float-to-int cast; backlog is never negative.
        self.backlog_ms.max(0.0).round() as u32
    }

    /// The report as it arrives off the wire (kbps, ms).
    pub fn from_wire(capacity_kbps: u32, backlog_ms: u32) -> Self {
        Self {
            capacity_bps: u64::from(capacity_kbps).saturating_mul(1000),
            backlog_ms: f64::from(backlog_ms),
        }
    }
}

/// What the viewer knows about its own report path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewerConfig {
    /// Report period, ms. Also the delivered-rate window: one measurement
    /// per report.
    pub report_interval_ms: f64,
}

/// Estimator state after one call to [`ViewerEstimator::on_frame`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameOutcome {
    /// The estimator currently sees a standing queue.
    pub congested: bool,
    /// The capacity it would report now, bps; `None` before it has one.
    pub capacity_bps: Option<u64>,
    /// The congestion signal (queue no single frame explains), ms. For logs.
    pub signal_ms: f64,
    /// The level `signal_ms` is compared against (one frame interval), ms.
    pub threshold_ms: f64,
    /// A report to send now, outside the periodic tick: the transition into
    /// congestion.
    pub early_report: Option<Report>,
}

impl FrameOutcome {
    const START: Self = Self {
        congested: false,
        capacity_bps: None,
        signal_ms: 0.0,
        threshold_ms: f64::INFINITY,
        early_report: None,
    };
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    index: i64,
    min_delay_ms: f64,
}

#[derive(Debug, Clone, Copy)]
struct Prev {
    timestamp_ms: f64,
    arrival_ms: f64,
    lateness_ms: f64,
    wait_ms: f64,
    /// Its bytes' link time at the capacity known when it arrived (infinite
    /// before any capacity): the most queue it can leave behind.
    own_ms: f64,
    /// No later frame had arrived when it was released.
    clean: bool,
}

/// One frame of the current busy period or the delivered window.
#[derive(Debug, Clone, Copy)]
struct Arrived {
    arrival_ms: f64,
    bytes: u32,
    /// Waited more than one frame interval.
    queued: bool,
}

/// The viewer-side estimator for one remote video stream. Pure: no I/O,
/// time passed in.
#[derive(Debug, Clone)]
pub struct ViewerEstimator {
    config: ViewerConfig,
    buckets: VecDeque<Bucket>,
    prev: Option<Prev>,
    gaps: VecDeque<f64>,
    /// The same gaps, sorted, for the median without a per-frame sort.
    gaps_sorted: Vec<f64>,
    /// `(timestamp of the frame that saw the jump, jump)` in this busy period.
    jumps: VecDeque<(f64, f64)>,
    /// Frames of the current busy period (first one anchors the span).
    busy: VecDeque<Arrived>,
    /// Recent frames for the delivered rate.
    recent: VecDeque<Arrived>,
    capacity_bps: Option<f64>,
    congested: bool,
    early_armed: bool,
    last_wait_ms: f64,
    last_report: Option<Report>,
    frames_since_report: usize,
    /// The next sequence number to release; `None` before any frame.
    next_seq: Option<u64>,
    /// Frames that arrived ahead of a missing one, by sequence number.
    pending: BTreeMap<u64, FrameInfo>,
    /// Release time of the latest released frame.
    last_release_ms: f64,
    /// Arrival of the latest clean release: the last instant at which the
    /// bytes delivered were exactly the bytes released.
    last_clean_ms: Option<f64>,
    /// Frames released since then.
    since_clean: VecDeque<Arrived>,
    /// Returned while every new arrival is held.
    last_outcome: FrameOutcome,
    /// Latest arrival seen: arrivals are clamped to never go back.
    last_arrival_ms: f64,
    /// Timestamp of this run's first frame; earlier ones are the old run's.
    run_start_ts_ms: f64,
    /// Latest timestamp accepted in this run.
    latest_ts_ms: f64,
    /// Arrival of the first frame rejected as late since the last accepted
    /// one (the new-run backstop).
    rejected_since_ms: Option<f64>,
    /// When the last early report went out (release instant).
    last_early_ms: f64,
}

impl ViewerEstimator {
    /// Creates an estimator with no history.
    pub fn new(config: ViewerConfig) -> Self {
        Self {
            config,
            buckets: VecDeque::new(),
            prev: None,
            gaps: VecDeque::new(),
            gaps_sorted: Vec::with_capacity(GAP_HISTORY + 1),
            jumps: VecDeque::new(),
            busy: VecDeque::new(),
            recent: VecDeque::new(),
            capacity_bps: None,
            congested: false,
            early_armed: true,
            last_wait_ms: 0.0,
            last_report: None,
            frames_since_report: 0,
            next_seq: None,
            pending: BTreeMap::new(),
            last_release_ms: f64::NEG_INFINITY,
            last_clean_ms: None,
            since_clean: VecDeque::new(),
            last_outcome: FrameOutcome::START,
            last_arrival_ms: f64::NEG_INFINITY,
            run_start_ts_ms: f64::NEG_INFINITY,
            latest_ts_ms: f64::NEG_INFINITY,
            rejected_since_ms: None,
            last_early_ms: f64::NEG_INFINITY,
        }
    }

    /// Forgets everything, as a new estimator. A restarted sequence within a
    /// stream is detected without it (see the module docs); call it when the
    /// stream itself is replaced.
    pub fn reset(&mut self) {
        *self = Self::new(self.config);
    }

    /// Frames waiting behind a sequence gap. For logs: a count that stays
    /// above zero means frames are lost after numbering.
    pub fn held_frames(&self) -> usize {
        self.pending.len()
    }

    /// The configuration it was built with.
    pub fn config(&self) -> ViewerConfig {
        self.config
    }

    /// Feeds one received video frame. Send `early_report` at once when it
    /// is `Some`. Frames with a non-finite timestamp or arrival are ignored.
    pub fn on_frame(&mut self, mut frame: FrameInfo) -> FrameOutcome {
        let ignored = FrameOutcome {
            early_report: None,
            ..self.last_outcome
        };
        if !frame.timestamp_ms.is_finite() || !frame.arrival_ms.is_finite() {
            return ignored;
        }
        frame.arrival_ms = frame.arrival_ms.max(self.last_arrival_ms);
        self.last_arrival_ms = frame.arrival_ms;
        if !self.accept(&frame) {
            return ignored;
        }
        let now_ms = frame.arrival_ms;
        self.pending.insert(frame.seq, frame);
        let mut early_report = None;
        let mut outcome = None;
        loop {
            self.release_contiguous(&mut early_report, &mut outcome);
            if !self.gap_given_up(now_ms) {
                break;
            }
            // The missing frame is lost: carry on from the first one held.
            self.next_seq = self.pending.first_key_value().map(|(seq, _)| *seq);
        }
        // Frames released together are one instant: an early report carries
        // the capacity after all of them.
        let early_report = early_report.map(|report: Report| Report {
            capacity_bps: self
                .capacity_bps
                .map_or(report.capacity_bps, |bps| bps.round() as u64),
            ..report
        });
        if early_report.is_some() {
            self.last_report = early_report;
        }
        let outcome = outcome.unwrap_or(self.last_outcome);
        self.last_outcome = outcome;
        FrameOutcome {
            early_report,
            ..outcome
        }
    }

    /// Run bookkeeping: whether `frame` belongs to the current run (starting
    /// a new one if it is the first frame of the next run).
    fn accept(&mut self, frame: &FrameInfo) -> bool {
        let Some(next) = self.next_seq else {
            if self.prev.is_none() && self.pending.is_empty() {
                self.start_run(frame);
                return true;
            }
            // Past the end of the sequence space: only a new run follows.
            return self.restart_if_newer(frame);
        };
        if frame.timestamp_ms < self.run_start_ts_ms {
            // Sent before this run began: a straggler from the old run, or
            // a reordered frame overtaken at the start of this one.
            return self.reject(frame);
        }
        if frame.seq < next || self.pending.contains_key(&frame.seq) {
            return self.restart_if_newer(frame);
        }
        self.latest_ts_ms = self.latest_ts_ms.max(frame.timestamp_ms);
        self.rejected_since_ms = None;
        true
    }

    /// A frame behind the sequence: the next run if its timestamp is later
    /// than any in this run, else a late frame.
    fn restart_if_newer(&mut self, frame: &FrameInfo) -> bool {
        if frame.timestamp_ms > self.latest_ts_ms {
            self.restart(frame);
            return true;
        }
        self.reject(frame)
    }

    /// Ignores `frame`, unless frames have been rejected for one capacity
    /// window with none accepted: then this run is over (a sender clock
    /// that went back) and `frame` starts the next one.
    fn reject(&mut self, frame: &FrameInfo) -> bool {
        let since = *self.rejected_since_ms.get_or_insert(frame.arrival_ms);
        if frame.arrival_ms - since >= CAPACITY_WINDOW_MS {
            self.restart(frame);
            return true;
        }
        false
    }

    fn restart(&mut self, frame: &FrameInfo) {
        let last_arrival_ms = self.last_arrival_ms;
        self.reset();
        self.last_arrival_ms = last_arrival_ms;
        self.start_run(frame);
    }

    fn start_run(&mut self, frame: &FrameInfo) {
        self.next_seq = Some(frame.seq);
        self.run_start_ts_ms = frame.timestamp_ms;
        self.latest_ts_ms = frame.timestamp_ms;
        self.rejected_since_ms = None;
    }

    /// The periodic report tick: the values to send, or `None` to send
    /// nothing. `None` before the first capacity (the publisher keeps its
    /// start rate) and whenever no frame was released since the last tick
    /// (a paused camera, frames held behind a gap): the publisher ages the
    /// report it has and holds, instead of taking a copy as fresh news.
    pub fn report(&mut self, _now_ms: f64) -> Option<Report> {
        if self.frames_since_report == 0 {
            return None;
        }
        let report = self.current_report()?;
        self.frames_since_report = 0;
        self.last_report = Some(report);
        self.last_report
    }

    /// Whether the gap at `next_seq` (frames are held behind it) is lost.
    fn gap_given_up(&self, now_ms: f64) -> bool {
        let oldest_ms = self
            .pending
            .values()
            .map(|held| held.arrival_ms)
            .fold(f64::INFINITY, f64::min);
        oldest_ms < now_ms - CAPACITY_WINDOW_MS || self.pending.len() > MAX_HELD_FRAMES
    }

    /// Releases every frame from `next_seq` on that has arrived, each at the
    /// instant all frames before it had arrived.
    fn release_contiguous(
        &mut self,
        early_report: &mut Option<Report>,
        outcome: &mut Option<FrameOutcome>,
    ) {
        while let Some(seq) = self.next_seq {
            let Some(mut released) = self.pending.remove(&seq) else {
                break;
            };
            let release_ms = released.arrival_ms.max(self.last_release_ms);
            released.arrival_ms = release_ms;
            self.last_release_ms = release_ms;
            // `None` after `u64::MAX`: only a new run can follow.
            self.next_seq = seq.checked_add(1);
            let clean = self
                .pending
                .values()
                .all(|held| held.arrival_ms > release_ms);
            let step = self.process(released, clean);
            *early_report = early_report.or(step.early_report);
            *outcome = Some(step);
        }
    }

    fn base_ms(&self) -> f64 {
        self.buckets
            .iter()
            .map(|bucket| bucket.min_delay_ms)
            .fold(f64::INFINITY, f64::min)
    }

    fn frame_interval_ms(&self) -> Option<f64> {
        self.gaps_sorted.get(self.gaps_sorted.len() / 2).copied()
    }

    fn push_gap(&mut self, gap: f64) {
        self.gaps.push_back(gap);
        let at = self
            .gaps_sorted
            .partition_point(|held| held.total_cmp(&gap).is_lt());
        self.gaps_sorted.insert(at, gap);
        if self.gaps.len() > GAP_HISTORY {
            if let Some(old) = self.gaps.pop_front() {
                let at = self
                    .gaps_sorted
                    .partition_point(|held| held.total_cmp(&old).is_lt());
                self.gaps_sorted.remove(at);
            }
        }
    }

    fn update_base(&mut self, arrival_ms: f64, delay_ms: f64) {
        let index = (arrival_ms / BASE_BUCKET_MS).floor() as i64;
        match self.buckets.back_mut() {
            Some(bucket) if bucket.index == index => {
                bucket.min_delay_ms = bucket.min_delay_ms.min(delay_ms);
            }
            _ => self.buckets.push_back(Bucket {
                index,
                min_delay_ms: delay_ms,
            }),
        }
        while self
            .buckets
            .front()
            .is_some_and(|bucket| bucket.index <= index - BASE_BUCKETS as i64)
        {
            self.buckets.pop_front();
        }
    }

    /// Bytes after the first over the arrival span, bps. One frame after
    /// the anchor is enough: a busy period's anchor is the instant the link
    /// started on its first frame.
    fn rate_bps(frames: &VecDeque<Arrived>) -> Option<f64> {
        let first = frames.front()?;
        let last = frames.back()?;
        let span = last.arrival_ms - first.arrival_ms;
        if frames.len() < 2 || span <= 0.0 {
            return None;
        }
        let bytes: f64 = frames.iter().skip(1).map(|f| f64::from(f.bytes)).sum();
        Some(bytes * 8.0 * 1000.0 / span)
    }

    fn current_report(&self) -> Option<Report> {
        let capacity = self.capacity_bps?;
        Some(Report {
            capacity_bps: capacity.round() as u64,
            backlog_ms: if self.congested {
                self.last_wait_ms
            } else {
                0.0
            },
        })
    }

    /// One frame in sequence order, at the instant every frame before it
    /// had arrived. `clean`: no later frame had arrived by then, so the
    /// bytes delivered by this instant are exactly the bytes released.
    fn process(&mut self, frame: FrameInfo, clean: bool) -> FrameOutcome {
        let delay_ms = frame.arrival_ms - frame.timestamp_ms;
        self.update_base(frame.arrival_ms, delay_ms);
        let lateness_ms = delay_ms - self.base_ms();
        self.frames_since_report += 1;

        let (wait_ms, ts_gap_ms) = match self.prev {
            Some(prev) => {
                let ts_gap = frame.timestamp_ms - prev.timestamp_ms;
                ((prev.lateness_ms - ts_gap).max(0.0), Some(ts_gap))
            }
            None => (0.0, None),
        };
        if let Some(gap) = ts_gap_ms.filter(|gap| *gap > 0.0) {
            self.push_gap(gap);
        }
        let interval_ms = self.frame_interval_ms();

        // The link time this frame's bytes take at the capacity already
        // known: all the delay it can account for.
        let own_ms = self
            .capacity_bps
            .map_or(f64::INFINITY, |bps| f64::from(frame.bytes) * 8_000.0 / bps);

        // Busy period bookkeeping and the single-frame explanation.
        let excess_ms;
        if let Some(prev) = self.prev.filter(|_| wait_ms > 0.0) {
            if self.busy.is_empty() {
                // The predecessor opened the busy period: the link was busy
                // from its arrival on. If later frames had already arrived
                // by then, part of their bytes were delivered before it, so
                // the span starts at the last clean release instead and
                // takes every frame released since.
                match self.last_clean_ms.filter(|_| !prev.clean) {
                    Some(clean_ms) => {
                        self.busy.push_back(Arrived {
                            arrival_ms: clean_ms,
                            bytes: 0,
                            queued: false,
                        });
                        self.busy.extend(self.since_clean.iter().copied());
                    }
                    None => self.busy.push_back(Arrived {
                        arrival_ms: prev.arrival_ms,
                        bytes: 0,
                        queued: false,
                    }),
                }
            }
            // The predecessor can leave at most its own bytes' time, less
            // the timestamp gap, in front of this frame.
            let ts_gap = frame.timestamp_ms - prev.timestamp_ms;
            let jump = (wait_ms - prev.wait_ms).min(prev.own_ms - ts_gap);
            self.jumps.push_back((frame.timestamp_ms, jump));
            self.jumps
                .retain(|(ts, jump)| jump - (frame.timestamp_ms - ts) > 0.0);
            let explained = self
                .jumps
                .iter()
                .map(|(ts, jump)| jump - (frame.timestamp_ms - ts))
                .fold(0.0, f64::max);
            excess_ms = wait_ms - explained;
        } else {
            // The frame found the link idle, so it opens any busy period and
            // no earlier wait carries evidence: judge its own service. The
            // part beyond its bytes at the known capacity is queue no frame
            // explains, and the link was busy with it from its first byte.
            self.busy.clear();
            self.jumps.clear();
            let service_ms = lateness_ms;
            excess_ms = (service_ms - own_ms).max(0.0);
            if excess_ms > 0.0 {
                self.busy.push_back(Arrived {
                    arrival_ms: frame.arrival_ms - service_ms,
                    bytes: 0,
                    queued: false,
                });
            }
        }

        let threshold_ms = interval_ms.unwrap_or(f64::INFINITY);
        let was_congested = self.congested;
        // A release that later frames overtook shared the link with bytes
        // not yet counted (theirs, partly delivered): no verdict and no
        // measurement until the next clean release.
        if clean {
            self.congested = excess_ms > threshold_ms;
        }
        let arrived = Arrived {
            arrival_ms: frame.arrival_ms,
            bytes: frame.bytes,
            queued: wait_ms > threshold_ms,
        };
        if !self.busy.is_empty() {
            self.busy.push_back(arrived);
            while self.busy.len() > 2
                && self.busy[1].arrival_ms < frame.arrival_ms - CAPACITY_WINDOW_MS
            {
                self.busy.pop_front();
            }
        }

        // Capacity: measured while congested, otherwise held and raised only
        // by a clean delivered window.
        if clean && self.congested {
            if let Some(bps) = Self::rate_bps(&self.busy) {
                self.capacity_bps = Some(bps);
            }
        }
        self.recent.push_back(arrived);
        let window_ms = self.config.report_interval_ms;
        while self.recent.len() > 2 && self.recent[1].arrival_ms <= frame.arrival_ms - window_ms {
            self.recent.pop_front();
        }
        let full_window = self
            .recent
            .front()
            .is_some_and(|first| frame.arrival_ms - first.arrival_ms >= window_ms);
        if clean && full_window && !self.congested && self.recent.iter().all(|f| !f.queued) {
            if let Some(bps) = Self::rate_bps(&self.recent) {
                if self.capacity_bps.is_none_or(|capacity| bps > capacity) {
                    self.capacity_bps = Some(bps);
                }
            }
        }

        if wait_ms <= 0.0 {
            self.early_armed = true;
        }
        // The queue the latest frame found, or for a frame that found the
        // link idle, the part of its own service its bytes do not explain.
        self.last_wait_ms = wait_ms.max(excess_ms);
        let spaced = frame.arrival_ms - self.last_early_ms >= self.config.report_interval_ms;
        let early_report = if self.congested && !was_congested && self.early_armed && spaced {
            self.early_armed = false;
            self.last_early_ms = frame.arrival_ms;
            self.current_report()
        } else {
            None
        };

        if clean {
            self.last_clean_ms = Some(frame.arrival_ms);
            self.since_clean.clear();
        } else if self.last_clean_ms.is_some() {
            // Without an anchor there is nothing to keep them for.
            self.since_clean.push_back(arrived);
            if self.since_clean.len() > MAX_HELD_FRAMES
                || self
                    .last_clean_ms
                    .is_some_and(|clean_ms| clean_ms < frame.arrival_ms - CAPACITY_WINDOW_MS)
            {
                // Too old to anchor a span inside the capacity window.
                self.last_clean_ms = None;
                self.since_clean.clear();
            }
        }

        self.prev = Some(Prev {
            timestamp_ms: frame.timestamp_ms,
            arrival_ms: frame.arrival_ms,
            lateness_ms,
            wait_ms,
            own_ms,
            clean,
        });

        FrameOutcome {
            congested: self.congested,
            capacity_bps: self.capacity_bps.map(|bps| bps.round() as u64),
            signal_ms: excess_ms,
            threshold_ms,
            early_report,
        }
    }
}

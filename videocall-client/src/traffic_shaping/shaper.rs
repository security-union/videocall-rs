/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Pure, deterministic core of the traffic shaper.
//!
//! The shaper models one narrow uplink: a work-conserving FIFO that drains at
//! `rate_bps`, followed by a fixed delay line of `delay_ms`. A packet of `N`
//! bytes occupies the link for `N * 8 / rate` seconds:
//!
//! ```text
//! link_departure_k = max(arrival_k, link_departure_{k-1}) + N_k * 8 / rate
//! release_k        = max(link_departure_k + delay, release_{k-1})
//! ```
//!
//! The release time is stored per packet when it is scheduled. A delay change
//! applies to packets still on the link; packets already in the delay line
//! keep the release time they had. The `max` keeps the release order FIFO when
//! the delay shrinks.
//!
//! The cap is a memory safety valve, not link behaviour; see
//! [`TrafficShaper::enqueue`].
//!
//! Time is always passed in (milliseconds on any monotonic clock), so the
//! whole thing runs under native `cargo test` with no browser APIs.

use std::collections::VecDeque;

/// The queue holds at most this many seconds of data at the current rate.
/// Packets overdue by more than this are dropped instead of sent.
pub const TRAFFIC_SHAPING_CAP_SECONDS: f64 = 10.0;

/// Rate presets offered by the UI, in bits per second.
pub const TRAFFIC_SHAPING_RATE_PRESETS_BPS: [u32; 4] = [2_000_000, 1_000_000, 500_000, 250_000];

/// Added-delay presets offered by the UI, in milliseconds.
pub const TRAFFIC_SHAPING_DELAY_PRESETS_MS: [u32; 3] = [0, 50, 150];

/// Rate applied by the step test (the same step the lab tests use).
pub const STEP_TEST_RATE_BPS: u32 = 500_000;

/// How long the step test keeps shaping on before turning it off.
pub const STEP_TEST_DURATION_MS: f64 = 15_000.0;

const CAP_MS: f64 = TRAFFIC_SHAPING_CAP_SECONDS * 1000.0;

/// What kind of outgoing media a packet carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapedKind {
    Audio,
    Camera,
    Screen,
}

impl ShapedKind {
    /// Camera and screen share both count as video.
    pub fn is_video(self) -> bool {
        matches!(self, ShapedKind::Camera | ShapedKind::Screen)
    }
}

/// Which media the shaper applies to. Everything else bypasses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TrafficShapingScope {
    Audio,
    Video,
    #[default]
    Both,
}

impl TrafficShapingScope {
    /// Whether packets of `kind` go through the simulated link.
    pub fn shapes(self, kind: ShapedKind) -> bool {
        match self {
            TrafficShapingScope::Audio => kind == ShapedKind::Audio,
            TrafficShapingScope::Video => kind.is_video(),
            TrafficShapingScope::Both => true,
        }
    }
}

/// User-chosen shaping parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrafficShapingSettings {
    /// Link rate in bits per second. Values under 1 kbps are raised to 1 kbps.
    pub rate_bps: u32,
    /// Fixed delay added after the link, in milliseconds.
    pub delay_ms: u32,
    pub scope: TrafficShapingScope,
}

impl Default for TrafficShapingSettings {
    fn default() -> Self {
        Self {
            rate_bps: 1_000_000,
            delay_ms: 0,
            scope: TrafficShapingScope::Both,
        }
    }
}

impl TrafficShapingSettings {
    /// The settings the step test applies.
    pub fn step_test() -> Self {
        Self {
            rate_bps: STEP_TEST_RATE_BPS,
            delay_ms: 0,
            scope: TrafficShapingScope::Both,
        }
    }
}

/// How many queued packets were thrown away, and which local encoders must
/// produce a keyframe because of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DropReport {
    pub packets: u32,
    /// Force a camera keyframe.
    pub camera: bool,
    /// Force a screen keyframe.
    pub screen: bool,
}

impl DropReport {
    fn record(&mut self, kind: ShapedKind) {
        self.packets += 1;
        match kind {
            ShapedKind::Camera => self.camera = true,
            ShapedKind::Screen => self.screen = true,
            ShapedKind::Audio => {}
        }
    }

    fn request(&mut self, slot: usize) {
        if slot == 0 {
            self.camera = true;
        } else {
            self.screen = true;
        }
    }

    pub fn merge(&mut self, other: DropReport) {
        self.packets += other.packets;
        self.camera |= other.camera;
        self.screen |= other.screen;
    }
}

/// Result of offering a packet to the shaper.
#[derive(Debug, PartialEq)]
pub enum Admission<P> {
    /// The scope does not cover this kind; send it now, untouched.
    Bypass(P),
    /// The packet is queued. The report says what the cap dropped, if anything.
    Queued(DropReport),
}

/// Result of [`TrafficShaper::poll`].
#[derive(Debug, PartialEq)]
pub struct Polled<P> {
    /// Due packets, in order, to send now.
    pub due: Vec<P>,
    /// Packets overdue by more than the cap duration (dropped instead of sent),
    /// plus keyframe requests that became due; see [`TrafficShaper::enqueue`].
    pub report: DropReport,
}

/// Readout of the shaper's state.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TrafficShaperStats {
    pub queued_bytes: usize,
    pub queued_packets: usize,
    /// How long the packet at the head of the queue has been waiting.
    pub head_delay_ms: f64,
    /// Packets dropped by the memory valve (cap overflow or stale) since this
    /// shaper was created, that is since shaping was last turned on.
    pub dropped_by_cap: u64,
}

/// What the UI readout shows while shaping is on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrafficShapingStatus {
    /// Settings the live shaper runs with (the step test's during a step test).
    pub settings: TrafficShapingSettings,
    pub queued_bytes: usize,
    pub queued_packets: usize,
    /// How long the packet at the head of the queue has been waiting.
    pub head_delay_ms: f64,
    /// Packets dropped by the memory valve since shaping was turned on.
    pub dropped_by_cap: u64,
    /// Time left in a running step test, `None` when no step test runs.
    pub step_test_remaining_ms: Option<f64>,
}

#[derive(Debug)]
struct Entry<P> {
    payload: P,
    bytes: usize,
    kind: ShapedKind,
    /// A video keyframe: nothing queued after it depends on anything before it.
    keyframe: bool,
    arrival_ms: f64,
    /// When the last bit of this packet leaves the simulated link.
    link_departure_ms: f64,
    /// When the packet leaves the delay line and is handed to the transport.
    release_ms: f64,
    /// Arrival order; monotonic along the queue.
    seq: u64,
}

/// A simulated narrow uplink. `P` is the packet payload type.
#[derive(Debug)]
pub struct TrafficShaper<P> {
    settings: TrafficShapingSettings,
    queue: VecDeque<Entry<P>>,
    queued_bytes: usize,
    /// Link departure of the most recently scheduled packet. The link is
    /// free from this instant on.
    link_free_at_ms: f64,
    /// Release time of the most recently scheduled packet.
    last_release_ms: f64,
    dropped_by_cap: u64,
    next_seq: u64,
    /// Per video kind (camera, screen): a kept keyframe whose tail the cap
    /// dropped. A keyframe request is due once it reaches the link. Reset by
    /// a newer keyframe and by `clear()`, so nothing fires after off/on.
    keyframe_due_after: [Option<u64>; 2],
}

impl<P> TrafficShaper<P> {
    pub fn new(settings: TrafficShapingSettings) -> Self {
        Self {
            settings: sanitize(settings),
            queue: VecDeque::new(),
            queued_bytes: 0,
            link_free_at_ms: f64::NEG_INFINITY,
            last_release_ms: f64::NEG_INFINITY,
            dropped_by_cap: 0,
            next_seq: 0,
            keyframe_due_after: [None, None],
        }
    }

    pub fn settings(&self) -> TrafficShapingSettings {
        self.settings
    }

    /// Byte budget of the queue: [`TRAFFIC_SHAPING_CAP_SECONDS`] of data at
    /// the current rate.
    pub fn cap_bytes(&self) -> usize {
        (self.settings.rate_bps as f64 * TRAFFIC_SHAPING_CAP_SECONDS / 8.0) as usize
    }

    /// Offer a packet of `bytes` wire bytes that arrived at `now_ms`.
    ///
    /// `now_ms` must not go backwards between calls.
    ///
    /// `keyframe` marks a video keyframe (ignored for audio).
    ///
    /// Cap rule, a memory valve. The delay line and the in-flight packet are
    /// on the wire and never dropped; the rest is "droppable". When the
    /// droppable bytes plus this packet exceed the cap, per video kind:
    ///
    /// 1. Drop the queued packets of that kind before its newest queued
    ///    keyframe; they are superseded. A keyframe arriving now supersedes
    ///    all of them. With no keyframe queued or arriving, drop all of them
    ///    and request one keyframe.
    /// 2. If still over the cap, drop the packets queued behind the kept
    ///    keyframe too, and request a keyframe once the kept one reaches the
    ///    link (asking earlier would let the answer supersede it unsent).
    /// 3. If still over the cap, drop the droppable audio.
    ///
    /// The arriving packet is always admitted. No request is made for a kind
    /// whose keyframe is the arriving packet. The newest queued keyframe of a
    /// kind is never dropped by the cap, so the queue holds at most
    /// cap + delay line + in-flight packet + one keyframe per video kind + the
    /// arriving packet.
    pub fn enqueue(
        &mut self,
        now_ms: f64,
        payload: P,
        bytes: usize,
        kind: ShapedKind,
        keyframe: bool,
    ) -> Admission<P> {
        if !self.settings.scope.shapes(kind) {
            return Admission::Bypass(payload);
        }
        // A keyframe restarts its stream, so a deferred request for that kind
        // is no longer needed, whether or not this arrival overflows.
        if let (true, Some(slot)) = (keyframe, video_slot(kind)) {
            self.keyframe_due_after[slot] = None;
        }
        let mut report = self.make_room(now_ms, bytes, kind, keyframe);

        // Work-conserving FIFO: start when both the packet and the link are
        // ready. An idle link earns no credit, because a link that went free
        // in the past simply starts at `now_ms`.
        let start = now_ms.max(self.link_free_at_ms);
        let link_departure_ms = start + self.transmission_ms(bytes);
        let release_ms =
            (link_departure_ms + self.settings.delay_ms as f64).max(self.last_release_ms);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.link_free_at_ms = link_departure_ms;
        self.last_release_ms = release_ms;
        self.queue.push_back(Entry {
            payload,
            bytes,
            kind,
            keyframe: keyframe && kind.is_video(),
            arrival_ms: now_ms,
            link_departure_ms,
            release_ms,
            seq,
        });
        self.queued_bytes += bytes;
        report.merge(self.take_due_keyframes(now_ms));
        Admission::Queued(report)
    }

    /// Remove every packet whose release time is at or before `now_ms`.
    /// Packets overdue by more than the cap duration (a throttled timer and
    /// no arrivals to clock the shaper) are dropped, not sent: bursting stale
    /// media to the viewers would be worse than losing it.
    pub fn poll(&mut self, now_ms: f64) -> Polled<P> {
        let stale_before = now_ms - CAP_MS;
        let mut due = Vec::new();
        let mut report = DropReport::default();
        // Release times are monotonic, so the due packets are a prefix.
        while self
            .queue
            .front()
            .is_some_and(|head| head.release_ms <= now_ms)
        {
            let Some(entry) = self.queue.pop_front() else {
                break;
            };
            self.queued_bytes -= entry.bytes;
            if entry.release_ms < stale_before {
                report.record(entry.kind);
            } else {
                due.push(entry.payload);
            }
        }
        self.dropped_by_cap += report.packets as u64;
        report.merge(self.take_due_keyframes(now_ms));
        Polled { due, report }
    }

    /// Release time of the head packet, or `None` when the queue is empty.
    pub fn next_wake_ms(&self) -> Option<f64> {
        self.queue.front().map(|head| head.release_ms)
    }

    /// Change rate, delay and scope at `now_ms`.
    ///
    /// Packets still on the link are rescheduled at the new rate and delay;
    /// packets already in the delay line keep their release time. Queued
    /// packets of a kind the new scope no longer covers are dropped (all of
    /// them, wherever they are), because new packets of that kind now bypass
    /// the queue and would overtake them. The report says what was dropped.
    /// The cap is applied on the next enqueue.
    pub fn update_settings(&mut self, now_ms: f64, settings: TrafficShapingSettings) -> DropReport {
        let old = self.settings;
        self.settings = sanitize(settings);
        let scope = self.settings.scope;
        let mut report = DropReport::default();
        let mut freed = 0;
        self.queue.retain(|entry| {
            let keep = scope.shapes(entry.kind);
            if !keep {
                report.record(entry.kind);
                freed += entry.bytes;
            }
            keep
        });
        for (slot, video) in [ShapedKind::Camera, ShapedKind::Screen]
            .into_iter()
            .enumerate()
        {
            if !scope.shapes(video) {
                self.keyframe_due_after[slot] = None;
            }
        }
        self.queued_bytes -= freed;
        if report.packets > 0
            || old.rate_bps != self.settings.rate_bps
            || old.delay_ms != self.settings.delay_ms
        {
            self.reschedule(now_ms, old.rate_bps);
        }
        report
    }

    /// Drop everything queued, including packets already on the wire. The
    /// link is free immediately afterwards.
    pub fn clear(&mut self) -> DropReport {
        let mut report = DropReport::default();
        for entry in self.queue.drain(..) {
            report.record(entry.kind);
        }
        self.queued_bytes = 0;
        self.link_free_at_ms = f64::NEG_INFINITY;
        self.last_release_ms = f64::NEG_INFINITY;
        self.keyframe_due_after = [None, None];
        report
    }

    pub fn stats(&self, now_ms: f64) -> TrafficShaperStats {
        TrafficShaperStats {
            queued_bytes: self.queued_bytes,
            queued_packets: self.queue.len(),
            head_delay_ms: self
                .queue
                .front()
                .map(|head| (now_ms - head.arrival_ms).max(0.0))
                .unwrap_or(0.0),
            dropped_by_cap: self.dropped_by_cap,
        }
    }

    /// Stored release times, head first.
    #[cfg(test)]
    fn release_times_ms(&self) -> Vec<f64> {
        self.queue.iter().map(|entry| entry.release_ms).collect()
    }

    fn transmission_ms(&self, bytes: usize) -> f64 {
        transmission_ms(bytes, self.settings.rate_bps)
    }

    /// Index of the first packet still on the link. Every queued packet has
    /// arrived by `now_ms` and the link is work-conserving, so this is the
    /// packet being transmitted. Link departures are monotonic, so a binary
    /// search finds it.
    fn in_flight_index(&self, now_ms: f64) -> usize {
        self.queue
            .partition_point(|entry| entry.link_departure_ms <= now_ms)
    }

    /// Apply the cap rule before admitting a packet; see [`Self::enqueue`].
    fn make_room(
        &mut self,
        now_ms: f64,
        bytes: usize,
        kind: ShapedKind,
        keyframe: bool,
    ) -> DropReport {
        let cap = self.cap_bytes();
        let len = self.queue.len();
        // Everything up to and including the in-flight packet is on the wire.
        let first = (self.in_flight_index(now_ms) + 1).min(len);
        let on_wire: usize = self.queue.range(..first).map(|entry| entry.bytes).sum();
        let mut remaining = self.queued_bytes - on_wire;
        if remaining + bytes <= cap {
            return DropReport::default();
        }

        // Newest droppable keyframe per video kind.
        let mut newest_key = [None; 2];
        for (index, entry) in self.queue.iter().enumerate().skip(first) {
            if let (true, Some(slot)) = (entry.keyframe, video_slot(entry.kind)) {
                newest_key[slot] = Some(index);
            }
        }
        // A keyframe arriving now supersedes everything of its kind.
        let mut keep_from = newest_key;
        if let (true, Some(slot)) = (keyframe, video_slot(kind)) {
            keep_from[slot] = None;
        }
        let arriving_key = |slot: usize| keyframe && video_slot(kind) == Some(slot);

        let mut report = DropReport::default();
        let mut drop = vec![false; len];
        let mut lost = [false; 2];
        // 1. Video superseded by the newest keyframe of its kind, or all of
        //    that kind when there is no keyframe to restart from.
        for (index, entry) in self.queue.iter().enumerate().skip(first) {
            let Some(slot) = video_slot(entry.kind) else {
                continue;
            };
            if keep_from[slot].is_none_or(|key| index < key) {
                drop[index] = true;
                remaining -= entry.bytes;
                report.packets += 1;
                lost[slot] = true;
            }
        }
        for slot in 0..2 {
            if lost[slot] && keep_from[slot].is_none() && !arriving_key(slot) {
                report.request(slot);
                self.keyframe_due_after[slot] = None;
            }
        }
        // 2. Still over: the tail behind each kept keyframe. That breaks the
        //    chain after the keyframe, so a keyframe is needed; it is asked for
        //    once the kept keyframe is on the link, so the answer cannot
        //    supersede it before the viewers get it.
        if remaining + bytes > cap {
            for (slot, key) in keep_from.into_iter().enumerate() {
                let Some(key) = key else {
                    continue;
                };
                let mut tail = false;
                for (index, entry) in self.queue.iter().enumerate().skip(key + 1) {
                    if video_slot(entry.kind) == Some(slot) && !drop[index] {
                        drop[index] = true;
                        remaining -= entry.bytes;
                        report.packets += 1;
                        tail = true;
                    }
                }
                if tail {
                    self.keyframe_due_after[slot] = Some(self.queue[key].seq);
                }
            }
        }
        // 3. Still over: audio, which is dropped only when no droppable video
        //    is left but the kept keyframes.
        if remaining + bytes > cap {
            for (index, entry) in self.queue.iter().enumerate().skip(first) {
                if entry.kind == ShapedKind::Audio && !drop[index] {
                    drop[index] = true;
                    report.packets += 1;
                }
            }
        }

        if report.packets > 0 {
            let mut index = 0;
            let mut freed = 0;
            self.queue.retain(|entry| {
                let dropped = drop[index];
                index += 1;
                if dropped {
                    freed += entry.bytes;
                }
                !dropped
            });
            self.queued_bytes -= freed;
            self.dropped_by_cap += report.packets as u64;
            let rate = self.settings.rate_bps;
            self.reschedule(now_ms, rate);
        }
        report
    }

    /// Deferred keyframe requests whose kept keyframe has reached the link
    /// (or left the queue).
    fn take_due_keyframes(&mut self, now_ms: f64) -> DropReport {
        let mut report = DropReport::default();
        if self.keyframe_due_after == [None, None] {
            return report;
        }
        let in_flight = self.in_flight_index(now_ms);
        for slot in 0..2 {
            let Some(seq) = self.keyframe_due_after[slot] else {
                continue;
            };
            let on_link = match self.queue.binary_search_by_key(&seq, |entry| entry.seq) {
                Ok(index) => index <= in_flight,
                Err(_) => true,
            };
            if on_link {
                report.request(slot);
                self.keyframe_due_after[slot] = None;
            }
        }
        report
    }

    /// Recompute link departures and release times of the packets still on
    /// the link, after a rate or delay change or a drop.
    ///
    /// The in-flight packet keeps the work it has left, converted to the new
    /// rate. Packets behind it go back to back at the new rate. Packets in
    /// the delay line are not touched.
    fn reschedule(&mut self, now_ms: f64, old_rate_bps: u32) {
        let first = self.in_flight_index(now_ms);
        let new_rate = self.settings.rate_bps;
        let delay = self.settings.delay_ms as f64;
        let mut link_free = now_ms;
        let mut last_release = if first > 0 {
            self.queue[first - 1].release_ms
        } else {
            f64::NEG_INFINITY
        };
        for (index, entry) in self.queue.iter_mut().enumerate().skip(first) {
            let old_tx = transmission_ms(entry.bytes, old_rate_bps);
            let in_flight = index == first && entry.link_departure_ms - old_tx <= now_ms;
            entry.link_departure_ms = if in_flight {
                now_ms + (entry.link_departure_ms - now_ms) * old_rate_bps as f64 / new_rate as f64
            } else {
                // Every queued packet arrived by now, so this is `link_free`;
                // `max` keeps the formula honest.
                link_free.max(entry.arrival_ms) + transmission_ms(entry.bytes, new_rate)
            };
            link_free = entry.link_departure_ms;
            entry.release_ms = (entry.link_departure_ms + delay).max(last_release);
            last_release = entry.release_ms;
        }
        // The link is free after the last scheduled packet. With nothing left
        // on the link, its last departure is in the past, so the next arrival
        // starts at its own arrival time.
        self.link_free_at_ms = self
            .queue
            .back()
            .map_or(f64::NEG_INFINITY, |entry| entry.link_departure_ms);
        self.last_release_ms = self
            .queue
            .back()
            .map_or(f64::NEG_INFINITY, |entry| entry.release_ms);
    }
}

/// Index of a video kind in per-kind arrays; `None` for audio.
fn video_slot(kind: ShapedKind) -> Option<usize> {
    match kind {
        ShapedKind::Camera => Some(0),
        ShapedKind::Screen => Some(1),
        ShapedKind::Audio => None,
    }
}

fn transmission_ms(bytes: usize, rate_bps: u32) -> f64 {
    bytes as f64 * 8.0 * 1000.0 / rate_bps as f64
}

/// Guard against a zero rate, which would make every packet take forever.
fn sanitize(mut settings: TrafficShapingSettings) -> TrafficShapingSettings {
    settings.rate_bps = settings.rate_bps.max(1_000);
    settings
}

/// A running step test: shaping on for [`STEP_TEST_DURATION_MS`], then off.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StepTest {
    pub ends_at_ms: f64,
}

impl StepTest {
    pub fn start(now_ms: f64) -> Self {
        Self {
            ends_at_ms: now_ms + STEP_TEST_DURATION_MS,
        }
    }

    pub fn remaining_ms(&self, now_ms: f64) -> f64 {
        (self.ends_at_ms - now_ms).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-6;

    fn settings(
        rate_bps: u32,
        delay_ms: u32,
        scope: TrafficShapingScope,
    ) -> TrafficShapingSettings {
        TrafficShapingSettings {
            rate_bps,
            delay_ms,
            scope,
        }
    }

    fn tx_ms(bytes: usize, rate_bps: u32) -> f64 {
        transmission_ms(bytes, rate_bps)
    }

    fn queued(
        shaper: &mut TrafficShaper<u32>,
        now: f64,
        id: u32,
        bytes: usize,
        kind: ShapedKind,
    ) -> DropReport {
        enqueue(shaper, now, id, bytes, kind, false)
    }

    fn queued_key(
        shaper: &mut TrafficShaper<u32>,
        now: f64,
        id: u32,
        bytes: usize,
        kind: ShapedKind,
    ) -> DropReport {
        enqueue(shaper, now, id, bytes, kind, true)
    }

    fn enqueue(
        shaper: &mut TrafficShaper<u32>,
        now: f64,
        id: u32,
        bytes: usize,
        kind: ShapedKind,
        keyframe: bool,
    ) -> DropReport {
        match shaper.enqueue(now, id, bytes, kind, keyframe) {
            Admission::Queued(report) => report,
            Admission::Bypass(_) => panic!("packet {id} unexpectedly bypassed"),
        }
    }

    /// Walk time forward wake by wake and record when each packet is released.
    fn release_times(shaper: &mut TrafficShaper<u32>, until_ms: f64) -> Vec<(u32, f64)> {
        let mut out = Vec::new();
        while let Some(wake) = shaper.next_wake_ms() {
            if wake > until_ms {
                break;
            }
            assert!(shaper.poll(wake - 0.001).due.is_empty(), "released early");
            for id in shaper.poll(wake).due {
                out.push((id, wake));
            }
        }
        out
    }

    fn ids(shaper: &mut TrafficShaper<u32>) -> Vec<u32> {
        release_times(shaper, 1e12)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    /// Queued payloads, head first, without draining.
    fn ids_queued(shaper: &TrafficShaper<u32>) -> Vec<u32> {
        shaper.queue.iter().map(|entry| entry.payload).collect()
    }

    #[test]
    fn departures_match_fifo_formula_for_mixed_sizes() {
        let rate = 1_000_000;
        let mut shaper = TrafficShaper::new(settings(rate, 0, TrafficShapingScope::Both));
        let arrivals = [
            (0.0, 1250usize),
            (1.0, 500),
            (2.0, 4000),
            (60.0, 100),
            (60.0, 2500),
        ];
        for (id, (at, bytes)) in arrivals.iter().enumerate() {
            queued(&mut shaper, *at, id as u32, *bytes, ShapedKind::Camera);
        }
        let mut expected = Vec::new();
        let mut prev = f64::NEG_INFINITY;
        for (id, (at, bytes)) in arrivals.iter().enumerate() {
            let dep = at.max(prev) + tx_ms(*bytes, rate);
            expected.push((id as u32, dep));
            prev = dep;
        }
        let got = release_times(&mut shaper, 1e9);
        assert_eq!(got.len(), expected.len());
        for ((gid, gt), (eid, et)) in got.iter().zip(expected.iter()) {
            assert_eq!(gid, eid);
            assert!((gt - et).abs() < EPS, "packet {gid}: got {gt}, want {et}");
        }
    }

    #[test]
    fn fixed_delay_adds_exactly() {
        let rate = 500_000;
        let mut plain = TrafficShaper::new(settings(rate, 0, TrafficShapingScope::Both));
        let mut delayed = TrafficShaper::new(settings(rate, 150, TrafficShapingScope::Both));
        for (id, (at, bytes)) in [(0.0, 1000usize), (3.0, 7000), (100.0, 200)]
            .iter()
            .enumerate()
        {
            queued(&mut plain, *at, id as u32, *bytes, ShapedKind::Audio);
            queued(&mut delayed, *at, id as u32, *bytes, ShapedKind::Audio);
        }
        let a = release_times(&mut plain, 1e9);
        let b = release_times(&mut delayed, 1e9);
        assert_eq!(a.len(), 3);
        for ((ia, ta), (ib, tb)) in a.iter().zip(b.iter()) {
            assert_eq!(ia, ib);
            assert!((tb - ta - 150.0).abs() < EPS);
        }
    }

    #[test]
    fn idle_link_does_not_accumulate_credit() {
        let rate = 250_000;
        let mut shaper = TrafficShaper::new(settings(rate, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 1000, ShapedKind::Camera);
        assert_eq!(release_times(&mut shaper, 1e9).len(), 1);
        let t = 10_000.0;
        for id in 1..=5 {
            queued(&mut shaper, t, id, 1000, ShapedKind::Camera);
        }
        assert!(shaper.poll(t).due.is_empty());
        let got = release_times(&mut shaper, 1e9);
        for (k, (_, at)) in got.iter().enumerate() {
            let want = t + (k as f64 + 1.0) * tx_ms(1000, rate);
            assert!(
                (at - want).abs() < EPS,
                "burst packet {k}: got {at}, want {want}"
            );
        }
    }

    #[test]
    fn rate_change_mid_queue_reschedules_remaining_work() {
        let mut shaper = TrafficShaper::new(settings(1_000_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 1250, ShapedKind::Camera);
        queued(&mut shaper, 0.0, 1, 1250, ShapedKind::Camera);
        let report = shaper.update_settings(5.0, settings(500_000, 0, TrafficShapingScope::Both));
        assert_eq!(report, DropReport::default());
        let got = release_times(&mut shaper, 1e9);
        assert_eq!(got.len(), 2);
        assert!((got[0].1 - 15.0).abs() < EPS, "got {}", got[0].1);
        assert!((got[1].1 - 35.0).abs() < EPS, "got {}", got[1].1);
        queued(&mut shaper, 100.0, 2, 1250, ShapedKind::Camera);
        assert!((shaper.next_wake_ms().unwrap() - 120.0).abs() < EPS);
    }

    #[test]
    fn two_rate_changes_on_the_same_packet_keep_remaining_work() {
        let mut shaper = TrafficShaper::new(settings(1_000_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 1250, ShapedKind::Camera); // 10 ms
        shaper.update_settings(5.0, settings(500_000, 0, TrafficShapingScope::Both)); // 5 ms left -> 10 ms
        shaper.update_settings(10.0, settings(1_000_000, 0, TrafficShapingScope::Both)); // 5 ms left at 500k -> 2.5 ms
        assert!((shaper.next_wake_ms().unwrap() - 12.5).abs() < EPS);
    }

    #[test]
    fn rate_increase_pulls_wake_time_earlier() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 2500, ShapedKind::Camera);
        assert!((shaper.next_wake_ms().unwrap() - 80.0).abs() < EPS);
        shaper.update_settings(0.0, settings(2_000_000, 0, TrafficShapingScope::Both));
        assert!((shaper.next_wake_ms().unwrap() - 10.0).abs() < EPS);
    }

    #[test]
    fn scope_filter_bypasses_unshaped_kinds() {
        let mut video_only = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Video));
        assert_eq!(
            video_only.enqueue(0.0, 7, 100, ShapedKind::Audio, false),
            Admission::Bypass(7)
        );
        assert!(matches!(
            video_only.enqueue(0.0, 8, 100, ShapedKind::Camera, false),
            Admission::Queued(_)
        ));
        assert!(matches!(
            video_only.enqueue(0.0, 9, 100, ShapedKind::Screen, false),
            Admission::Queued(_)
        ));
        let mut audio_only = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Audio));
        assert_eq!(
            audio_only.enqueue(0.0, 1, 100, ShapedKind::Camera, false),
            Admission::Bypass(1)
        );
        assert_eq!(
            audio_only.enqueue(0.0, 2, 100, ShapedKind::Screen, false),
            Admission::Bypass(2)
        );
        assert!(matches!(
            audio_only.enqueue(0.0, 3, 100, ShapedKind::Audio, false),
            Admission::Queued(_)
        ));
        assert_eq!(video_only.stats(0.0).queued_packets, 2);
        assert_eq!(audio_only.stats(0.0).queued_packets, 1);
    }

    #[test]
    fn audio_and_video_share_one_link_in_send_order() {
        let mut shaper = TrafficShaper::new(settings(1_000_000, 0, TrafficShapingScope::Both));
        let order = [
            ShapedKind::Camera,
            ShapedKind::Audio,
            ShapedKind::Screen,
            ShapedKind::Audio,
        ];
        for (id, kind) in order.iter().enumerate() {
            queued(&mut shaper, 0.0, id as u32, 1250, *kind);
        }
        let got = release_times(&mut shaper, 1e9);
        let got_ids: Vec<u32> = got.iter().map(|(id, _)| *id).collect();
        assert_eq!(got_ids, vec![0, 1, 2, 3]);
        for (k, (_, at)) in got.iter().enumerate() {
            assert!((at - 10.0 * (k as f64 + 1.0)).abs() < EPS);
        }
    }

    #[test]
    fn cap_is_ten_seconds_at_current_rate() {
        let mut shaper: TrafficShaper<u32> =
            TrafficShaper::new(settings(1_000_000, 0, TrafficShapingScope::Both));
        assert_eq!(shaper.cap_bytes(), 1_250_000);
        shaper.update_settings(0.0, settings(250_000, 0, TrafficShapingScope::Both));
        assert_eq!(shaper.cap_bytes(), 312_500);
    }

    // ---- cap rule ----
    //
    // 250 kbps throughout: cap = 312_500 bytes.

    #[test]
    fn overflow_without_any_keyframe_drops_all_queued_video_and_requests_once_per_kind() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 1_000, ShapedKind::Audio); // in flight
        for id in 1..=5 {
            assert_eq!(
                queued(&mut shaper, 0.0, id, 50_000, ShapedKind::Camera),
                DropReport::default()
            );
        }
        queued(&mut shaper, 0.0, 6, 30_000, ShapedKind::Screen);
        queued(&mut shaper, 0.0, 7, 1_000, ShapedKind::Audio);
        // 281_000 droppable + 50_000 > cap.
        let report = queued(&mut shaper, 0.0, 8, 50_000, ShapedKind::Camera);
        assert_eq!(
            report,
            DropReport {
                packets: 6,
                camera: true,
                screen: true
            }
        );
        assert_eq!(shaper.stats(0.0).dropped_by_cap, 6);
        assert_eq!(ids(&mut shaper), vec![0, 7, 8]);
    }

    #[test]
    fn keyframe_arriving_behind_p_frames_drops_them_without_a_request() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 1_000, ShapedKind::Audio); // in flight
        for id in 1..=5 {
            queued(&mut shaper, 0.0, id, 50_000, ShapedKind::Camera);
        }
        queued(&mut shaper, 0.0, 6, 1_000, ShapedKind::Audio);
        // The encoder's keyframe overflows the queue: everything before it is
        // superseded.
        let report = queued_key(&mut shaper, 0.0, 7, 100_000, ShapedKind::Camera);
        assert_eq!(report.packets, 5);
        assert!(!report.camera && !report.screen, "{report:?}");
        assert_eq!(ids(&mut shaper), vec![0, 6, 7]);
    }

    #[test]
    fn p_frame_overflow_keeps_the_queued_keyframe_and_what_follows_it() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 1_000, ShapedKind::Camera); // in flight
        queued(&mut shaper, 0.0, 1, 50_000, ShapedKind::Camera);
        queued_key(&mut shaper, 0.0, 2, 100_000, ShapedKind::Camera);
        queued(&mut shaper, 0.0, 3, 50_000, ShapedKind::Camera);
        queued(&mut shaper, 0.0, 4, 50_000, ShapedKind::Camera);
        // 250_000 droppable + 80_000 > cap: only id 1 (before the keyframe) goes.
        let report = queued(&mut shaper, 0.0, 5, 80_000, ShapedKind::Camera);
        assert_eq!(
            report,
            DropReport {
                packets: 1,
                camera: false,
                screen: false
            }
        );
        assert_eq!(ids(&mut shaper), vec![0, 2, 3, 4, 5]);
    }

    #[test]
    fn keyframes_of_one_kind_do_not_protect_the_other_kind() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 1_000, ShapedKind::Audio); // in flight
        queued_key(&mut shaper, 0.0, 1, 100_000, ShapedKind::Camera);
        queued(&mut shaper, 0.0, 2, 150_000, ShapedKind::Screen);
        let report = queued(&mut shaper, 0.0, 3, 100_000, ShapedKind::Camera);
        // Screen has no keyframe queued: all of it goes, one screen request.
        // Camera keeps its keyframe: no camera request.
        assert_eq!(
            report,
            DropReport {
                packets: 1,
                camera: false,
                screen: true
            }
        );
        assert_eq!(ids(&mut shaper), vec![0, 1, 3]);
    }

    /// Feed `count` small camera P-frames 33 ms apart starting at `from`,
    /// polling every millisecond; returns (delivered ids, keyframe requests).
    fn run_camera(
        shaper: &mut TrafficShaper<u32>,
        from: f64,
        until: f64,
        first_id: u32,
        p_bytes: usize,
    ) -> (Vec<u32>, u32) {
        let mut delivered = Vec::new();
        let mut requests = 0;
        let mut next_frame = from;
        let mut id = first_id;
        let mut now = from;
        while now < until {
            let polled = shaper.poll(now);
            requests += polled.report.camera as u32;
            delivered.extend(polled.due);
            if now >= next_frame {
                requests += queued(shaper, now, id, p_bytes, ShapedKind::Camera).camera as u32;
                id += 1;
                next_frame += 33.0;
            }
            now += 1.0;
        }
        (delivered, requests)
    }

    #[test]
    fn answering_keyframe_near_cap_size_is_kept_and_delivered_without_another_request() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        let cap = shaper.cap_bytes();
        queued(&mut shaper, 0.0, 0, 1_000, ShapedKind::Camera); // in flight
        for id in 1..=6 {
            queued(&mut shaper, 0.0, id, 50_000, ShapedKind::Camera);
        }
        // Overflow with no keyframe anywhere: exactly one request.
        let report = queued(&mut shaper, 0.0, 7, 50_000, ShapedKind::Camera);
        assert!(report.camera);
        // The encoder answers on its next frame with a keyframe at 90 % of the cap.
        let key_id = 100;
        let report = queued_key(&mut shaper, 33.0, key_id, cap * 9 / 10, ShapedKind::Camera);
        assert!(!report.camera, "keyframe arrival must not request another");
        // Small P-frames keep coming while the keyframe crosses the link (~9 s).
        let (delivered, requests) = run_camera(&mut shaper, 66.0, 12_000.0, 200, 500);
        assert!(
            delivered.contains(&key_id),
            "keyframe not delivered: {delivered:?}"
        );
        assert_eq!(
            requests, 0,
            "no further request while the keyframe is delivered"
        );
    }

    #[test]
    fn keyframe_larger_than_the_cap_is_delivered() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 3_000, ShapedKind::Camera); // in flight until 96 ms
        let report = queued_key(&mut shaper, 1.0, 1, 400_000, ShapedKind::Camera);
        assert!(!report.camera);
        // P-frames pile up behind it for the 12.8 s it takes on the link.
        let (delivered, _) = run_camera(&mut shaper, 2.0, 20_000.0, 2, 4_000);
        assert_eq!(&delivered[..2], &[0, 1]);
    }

    #[test]
    fn tail_behind_a_kept_keyframe_is_dropped_and_requested_only_once_it_reaches_the_link() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 10_000, ShapedKind::Camera); // in flight until 320 ms
        queued_key(&mut shaper, 0.0, 1, 200_000, ShapedKind::Camera);
        queued(&mut shaper, 0.0, 2, 100_000, ShapedKind::Camera);
        // 300_000 droppable + 50_000 > cap, nothing before the keyframe: its
        // tail (id 2) goes. The chain after the keyframe is broken, but asking
        // now would make the answer supersede the keyframe before it is sent.
        let report = queued(&mut shaper, 10.0, 3, 50_000, ShapedKind::Camera);
        assert_eq!(report.packets, 1);
        assert!(!report.camera);
        // Still waiting behind the in-flight packet: no request yet.
        assert!(!shaper.poll(300.0).report.camera);
        // The keyframe is on the link at 320 ms: the request comes, once.
        let polled = shaper.poll(320.0);
        assert_eq!(polled.due, vec![0]);
        assert!(polled.report.camera);
        assert!(!shaper.poll(321.0).report.camera);
        assert!(!queued(&mut shaper, 322.0, 4, 1_000, ShapedKind::Camera).camera);
    }

    #[test]
    fn a_new_keyframe_cancels_the_deferred_request() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 10_000, ShapedKind::Camera); // in flight until 320 ms
        queued_key(&mut shaper, 0.0, 1, 200_000, ShapedKind::Camera);
        queued(&mut shaper, 0.0, 2, 100_000, ShapedKind::Camera);
        queued(&mut shaper, 10.0, 3, 50_000, ShapedKind::Camera); // tail dropped, deferred
                                                                  // A periodic keyframe arrives and supersedes everything queued.
        let report = queued_key(&mut shaper, 20.0, 4, 300_000, ShapedKind::Camera);
        assert!(!report.camera);
        let (_, requests) = run_camera(&mut shaper, 21.0, 2_000.0, 5, 100);
        assert_eq!(requests, 0);
    }

    #[test]
    fn a_new_keyframe_under_the_cap_cancels_the_deferred_request() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 10_000, ShapedKind::Camera); // in flight until 320 ms
        queued_key(&mut shaper, 0.0, 1, 200_000, ShapedKind::Camera);
        queued(&mut shaper, 0.0, 2, 100_000, ShapedKind::Camera);
        queued(&mut shaper, 10.0, 3, 50_000, ShapedKind::Camera); // tail dropped, deferred
                                                                  // A small keyframe fits under the cap: no overflow, but it restarts
                                                                  // the stream, so the deferred request is no longer needed.
        assert_eq!(
            queued_key(&mut shaper, 20.0, 4, 20_000, ShapedKind::Camera),
            DropReport::default()
        );
        let mut now = 21.0;
        while now <= 400.0 {
            assert!(!shaper.poll(now).report.camera, "request fired at {now} ms");
            now += 1.0;
        }
    }

    #[test]
    fn kept_keyframe_superseded_by_a_newer_one_does_not_fire_the_deferred_request() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 10_000, ShapedKind::Camera); // in flight until 320 ms
        queued_key(&mut shaper, 0.0, 1, 200_000, ShapedKind::Camera);
        queued(&mut shaper, 0.0, 2, 100_000, ShapedKind::Camera);
        queued(&mut shaper, 10.0, 3, 50_000, ShapedKind::Camera); // tail dropped, deferred
        queued_key(&mut shaper, 20.0, 4, 20_000, ShapedKind::Camera);
        // A later overflow drops K (id 1), superseded by the newer keyframe.
        let report = queued(&mut shaper, 30.0, 5, 60_000, ShapedKind::Camera);
        assert!(!report.camera);
        assert_eq!(ids_queued(&shaper), vec![0, 4, 5]);
        let mut now = 31.0;
        while now <= 400.0 {
            assert!(!shaper.poll(now).report.camera, "request fired at {now} ms");
            now += 1.0;
        }
    }

    #[test]
    fn clear_leaves_no_deferred_request_behind() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 10_000, ShapedKind::Camera);
        queued_key(&mut shaper, 0.0, 1, 200_000, ShapedKind::Camera);
        queued(&mut shaper, 0.0, 2, 100_000, ShapedKind::Camera);
        queued(&mut shaper, 10.0, 3, 50_000, ShapedKind::Camera); // deferred
                                                                  // A reconnect clears the queue; clear reports the dropped camera itself.
        assert!(shaper.clear().camera);
        let (_, requests) = run_camera(&mut shaper, 20.0, 2_000.0, 4, 100);
        assert_eq!(requests, 0, "a deferred request fired after clear()");
    }

    #[test]
    fn overflow_does_not_recur_until_the_queue_refills() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 1_000, ShapedKind::Camera);
        for id in 1..=7 {
            queued(&mut shaper, 0.0, id, 50_000, ShapedKind::Camera);
        }
        assert_eq!(shaper.stats(0.0).dropped_by_cap, 6);
        for id in 8..=12 {
            assert_eq!(
                queued(&mut shaper, 0.0, id, 50_000, ShapedKind::Camera),
                DropReport::default()
            );
        }
    }

    #[test]
    fn delay_line_and_in_flight_packets_are_protected() {
        // 250 kbps, 150 ms delay: 1000-byte packets leave the link at 32, 64, 96.
        let mut shaper = TrafficShaper::new(settings(250_000, 150, TrafficShapingScope::Both));
        for id in 0..3 {
            queued(&mut shaper, 0.0, id, 1_000, ShapedKind::Camera);
        }
        // At 70 ms: ids 0 and 1 are in the delay line, id 2 is in flight.
        for id in 3..8 {
            queued(&mut shaper, 70.0, id, 200_000, ShapedKind::Camera);
        }
        let got = ids(&mut shaper);
        assert_eq!(&got[..3], &[0, 1, 2]);
    }

    #[test]
    fn audio_scope_overflow_drops_all_queued_audio() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Audio));
        for id in 0..313 {
            queued(&mut shaper, 0.0, id, 1_000, ShapedKind::Audio);
        }
        let report = queued(&mut shaper, 0.0, 313, 1_000, ShapedKind::Audio);
        assert_eq!(report.packets, 312);
        assert!(!report.camera && !report.screen);
        assert_eq!(ids(&mut shaper), vec![0, 313]);
    }

    #[test]
    fn audio_is_kept_while_video_can_be_dropped() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 1_000, ShapedKind::Audio);
        for id in 1..=100 {
            queued(&mut shaper, 0.0, id, 1_000, ShapedKind::Audio);
        }
        for id in 101..=104 {
            queued(&mut shaper, 0.0, id, 50_000, ShapedKind::Camera);
        }
        // 300_000 droppable + 50_000 > cap: video goes, audio stays.
        let report = queued(&mut shaper, 0.0, 105, 50_000, ShapedKind::Camera);
        assert_eq!(report.packets, 4);
        assert_eq!(ids(&mut shaper).len(), 102);
    }

    /// A camera encoder that answers a keyframe request on its next frame
    /// with a keyframe ten times a P-frame, plus 50 Hz audio, at `offered`
    /// times the link rate, polled every millisecond.
    struct Overload {
        delivered_bytes: usize,
        delivered_keyframes: u32,
        requests: u32,
        max_queued: usize,
        max_bound: usize,
    }

    fn overload(rate: u32, delay: u32, offered: f64, duration: f64) -> Overload {
        let mut shaper: TrafficShaper<(usize, bool)> =
            TrafficShaper::new(settings(rate, delay, TrafficShapingScope::Both));
        let cap = shaper.cap_bytes();
        let p_bytes = (rate as f64 * offered / 8.0 * 0.033) as usize;
        let key_bytes = p_bytes * 10;
        let mut result = Overload {
            delivered_bytes: 0,
            delivered_keyframes: 0,
            requests: 0,
            max_queued: 0,
            max_bound: 0,
        };
        let mut want_key = true;
        let (mut next_audio, mut next_video, mut now) = (0.0, 0.0, 0.0);
        while now < duration {
            let polled = shaper.poll(now);
            if polled.report.camera {
                result.requests += 1;
                want_key = true;
            }
            for (bytes, key) in polled.due {
                result.delivered_bytes += bytes;
                result.delivered_keyframes += key as u32;
            }
            if now >= next_audio {
                let _ = shaper.enqueue(now, (160, false), 160, ShapedKind::Audio, false);
                next_audio += 20.0;
            }
            if now >= next_video {
                let key = std::mem::take(&mut want_key);
                let bytes = if key { key_bytes } else { p_bytes };
                if let Admission::Queued(report) =
                    shaper.enqueue(now, (bytes, key), bytes, ShapedKind::Camera, key)
                {
                    if report.camera {
                        result.requests += 1;
                        want_key = true;
                    }
                }
                next_video += 33.0;
            }
            // cap + delay line + in-flight + one keyframe + the arriving packet.
            let bound =
                cap + rate as usize / 8 * delay as usize / 1000 + key_bytes + key_bytes + key_bytes;
            result.max_bound = bound;
            result.max_queued = result.max_queued.max(shaper.stats(now).queued_bytes);
            now += 1.0;
        }
        result
    }

    #[test]
    fn sustained_overload_keeps_the_link_busy_and_delivers_a_keyframe_per_cycle() {
        let rate = 250_000;
        let duration = 120_000.0;
        let run = overload(rate, 0, 4.0, duration);
        let capacity = rate as f64 / 8.0 * duration / 1000.0;
        assert!(
            run.delivered_bytes as f64 >= 0.9 * capacity,
            "delivered {} of {capacity}",
            run.delivered_bytes
        );
        assert!(
            run.requests >= 2,
            "expected overflow cycles, got {} requests",
            run.requests
        );
        // Every request is answered by a keyframe that reaches the viewers;
        // only the last one may still be on its way.
        assert!(
            run.delivered_keyframes + 1 >= run.requests,
            "{} keyframes delivered for {} requests",
            run.delivered_keyframes,
            run.requests
        );
        assert!(
            run.max_queued <= run.max_bound,
            "{} > {}",
            run.max_queued,
            run.max_bound
        );
    }

    #[test]
    fn memory_is_bounded_at_every_preset() {
        for &rate in &TRAFFIC_SHAPING_RATE_PRESETS_BPS {
            for &delay in &TRAFFIC_SHAPING_DELAY_PRESETS_MS {
                let run = overload(rate, delay, 4.0, 60_000.0);
                assert!(
                    run.max_queued <= run.max_bound,
                    "rate {rate} delay {delay}: {} > {}",
                    run.max_queued,
                    run.max_bound
                );
                assert!(run.requests > 0);
            }
        }
    }

    // ---- stale packets ----

    #[test]
    fn packets_overdue_by_more_than_the_cap_are_dropped_not_sent() {
        let mut shaper = TrafficShaper::new(settings(1_000_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 1_250, ShapedKind::Camera); // release at 10 ms
        queued(&mut shaper, 0.0, 1, 1_250, ShapedKind::Audio); // release at 20 ms
                                                               // A throttled timer wakes 15 s later with no arrivals in between.
        let polled = shaper.poll(15_000.0);
        assert!(polled.due.is_empty());
        assert_eq!(
            polled.report,
            DropReport {
                packets: 2,
                camera: true,
                screen: false
            }
        );
        assert_eq!(shaper.stats(15_000.0).queued_bytes, 0);
        assert_eq!(shaper.stats(15_000.0).dropped_by_cap, 2);
    }

    #[test]
    fn late_but_not_stale_packets_are_still_sent() {
        let mut shaper = TrafficShaper::new(settings(1_000_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 1_250, ShapedKind::Camera); // release at 10 ms
        let polled = shaper.poll(9_000.0);
        assert_eq!(polled.due, vec![0]);
        assert_eq!(polled.report, DropReport::default());
    }

    // ---- settings changes ----

    #[test]
    fn narrowing_scope_drops_the_kinds_that_left_it() {
        let rate = 1_000_000;
        let mut shaper = TrafficShaper::new(settings(rate, 0, TrafficShapingScope::Both));
        let kinds = [
            ShapedKind::Camera,
            ShapedKind::Audio,
            ShapedKind::Screen,
            ShapedKind::Audio,
            ShapedKind::Camera,
        ];
        for (id, kind) in kinds.iter().enumerate() {
            queued(&mut shaper, 0.0, id as u32, 1_250, *kind);
        }
        // At 5 ms the camera packet (id 0) is half sent.
        let report = shaper.update_settings(5.0, settings(rate, 0, TrafficShapingScope::Audio));
        assert_eq!(
            report,
            DropReport {
                packets: 3,
                camera: true,
                screen: true
            }
        );
        assert_eq!(shaper.stats(5.0).queued_packets, 2);
        // Audio keeps its order and is rescheduled back to back from now.
        let got = release_times(&mut shaper, 1e9);
        assert_eq!(
            got.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert!((got[0].1 - 15.0).abs() < EPS, "got {}", got[0].1);
        assert!((got[1].1 - 25.0).abs() < EPS, "got {}", got[1].1);
    }

    #[test]
    fn delay_change_does_not_move_packets_in_the_delay_line() {
        let mut shaper = TrafficShaper::new(settings(250_000, 150, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 1_000, ShapedKind::Camera); // link 32, release 182
        queued(&mut shaper, 0.0, 1, 1_000, ShapedKind::Camera); // link 64, still on the link at 40
        shaper.update_settings(40.0, settings(250_000, 0, TrafficShapingScope::Both));
        // id 0 is in the delay line: unchanged. id 1 gets the new delay, but
        // never leaves before id 0.
        let got = release_times(&mut shaper, 1e9);
        assert!((got[0].1 - 182.0).abs() < EPS, "got {}", got[0].1);
        assert!((got[1].1 - 182.0).abs() < EPS, "got {}", got[1].1);
        // A new packet at 200 ms, after the delay line drained: the link is
        // idle, so it leaves at 200 + 32 = 232 with no added delay.
        queued(&mut shaper, 200.0, 2, 1_000, ShapedKind::Camera);
        assert!((shaper.next_wake_ms().unwrap() - 232.0).abs() < EPS);
    }

    #[test]
    fn release_times_stay_fifo_after_a_delay_decrease() {
        let mut shaper = TrafficShaper::new(settings(250_000, 150, TrafficShapingScope::Both));
        for id in 0..6 {
            queued(&mut shaper, 0.0, id, 1_000, ShapedKind::Camera); // link 32, 64, ... 192
        }
        // At 100 ms ids 0..2 are in the delay line (release 182, 214, 246); the
        // rest are still on the link and get no delay at all.
        shaper.update_settings(100.0, settings(250_000, 0, TrafficShapingScope::Both));
        let releases = shaper.release_times_ms();
        assert!(
            releases.windows(2).all(|pair| pair[0] <= pair[1]),
            "release times not FIFO: {releases:?}"
        );
        // The packets on the link wait for the delay line instead of jumping it.
        assert!((releases[3] - 246.0).abs() < EPS, "{releases:?}");
        // New arrivals too.
        queued(&mut shaper, 101.0, 6, 1_000, ShapedKind::Camera);
        let releases = shaper.release_times_ms();
        assert!(
            releases.windows(2).all(|pair| pair[0] <= pair[1]),
            "{releases:?}"
        );
        let mut last = f64::NEG_INFINITY;
        while let Some(wake) = shaper.next_wake_ms() {
            assert!(wake >= last, "next_wake_ms went back: {wake} < {last}");
            last = wake;
            shaper.poll(wake);
        }
    }

    #[test]
    fn delay_increase_applies_to_packets_still_on_the_link() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 1_000, ShapedKind::Camera); // link 32
        shaper.update_settings(10.0, settings(250_000, 150, TrafficShapingScope::Both));
        assert!((shaper.next_wake_ms().unwrap() - 182.0).abs() < EPS);
    }

    #[test]
    fn update_settings_applies_new_scope_and_delay() {
        let mut shaper: TrafficShaper<u32> =
            TrafficShaper::new(settings(1_000_000, 0, TrafficShapingScope::Both));
        let new = settings(500_000, 50, TrafficShapingScope::Audio);
        shaper.update_settings(0.0, new);
        assert_eq!(shaper.settings(), new);
        assert_eq!(
            shaper.enqueue(0.0, 1, 10, ShapedKind::Camera, false),
            Admission::Bypass(1)
        );
        queued(&mut shaper, 0.0, 2, 1250, ShapedKind::Audio);
        assert!((shaper.next_wake_ms().unwrap() - 70.0).abs() < EPS);
    }

    // ---- clear, stats, step ----

    #[test]
    fn clear_drops_everything_and_reports_video() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 1000, ShapedKind::Audio);
        queued(&mut shaper, 0.0, 1, 1000, ShapedKind::Screen);
        let report = shaper.clear();
        assert_eq!(
            report,
            DropReport {
                packets: 2,
                camera: false,
                screen: true
            }
        );
        assert_eq!(shaper.stats(0.0).queued_bytes, 0);
        assert_eq!(shaper.next_wake_ms(), None);
        assert!(shaper.poll(1e9).due.is_empty());
        assert_eq!(shaper.stats(0.0).dropped_by_cap, 0);
    }

    #[test]
    fn clear_does_not_leave_the_link_busy() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        queued(&mut shaper, 0.0, 0, 100_000, ShapedKind::Camera);
        shaper.clear();
        queued(&mut shaper, 10.0, 1, 1000, ShapedKind::Camera);
        assert!((shaper.next_wake_ms().unwrap() - (10.0 + tx_ms(1000, 250_000))).abs() < EPS);
    }

    #[test]
    fn stats_report_head_delay() {
        let mut shaper = TrafficShaper::new(settings(250_000, 0, TrafficShapingScope::Both));
        assert_eq!(shaper.stats(0.0), TrafficShaperStats::default());
        queued(&mut shaper, 100.0, 0, 10_000, ShapedKind::Camera);
        queued(&mut shaper, 110.0, 1, 5_000, ShapedKind::Audio);
        let stats = shaper.stats(150.0);
        assert_eq!(stats.queued_bytes, 15_000);
        assert_eq!(stats.queued_packets, 2);
        assert!((stats.head_delay_ms - 50.0).abs() < EPS);
    }

    #[test]
    fn step_test_timing() {
        let step = StepTest::start(1_000.0);
        assert_eq!(step.ends_at_ms, 16_000.0);
        assert!(step.remaining_ms(1_000.0) > 0.0);
        assert!(step.remaining_ms(15_999.0) > 0.0);
        assert_eq!(step.remaining_ms(16_000.0), 0.0);
        assert!((step.remaining_ms(6_000.0) - 10_000.0).abs() < EPS);
        assert_eq!(step.remaining_ms(20_000.0), 0.0);
        let s = TrafficShapingSettings::step_test();
        assert_eq!(s.rate_bps, 500_000);
        assert_eq!(s.delay_ms, 0);
        assert_eq!(s.scope, TrafficShapingScope::Both);
    }

    #[test]
    fn scope_shapes_table() {
        use ShapedKind::*;
        use TrafficShapingScope as S;
        assert!(S::Audio.shapes(Audio) && !S::Audio.shapes(Camera) && !S::Audio.shapes(Screen));
        assert!(!S::Video.shapes(Audio) && S::Video.shapes(Camera) && S::Video.shapes(Screen));
        assert!(S::Both.shapes(Audio) && S::Both.shapes(Camera) && S::Both.shapes(Screen));
    }
}

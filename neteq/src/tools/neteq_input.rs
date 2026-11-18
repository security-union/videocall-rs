// Port target: WebRTC
// modules/audio_coding/neteq/tools/neteq_input.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd, with `RtpPacketReceived`
// replaced by `PacketData`.
// Copyright (c) 2016 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! The event source `NetEqTest` replays.

use crate::api::rtp::RtpHeader;

/// One packet to insert at `arrival_time_ms` of simulated time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketData {
    pub header: RtpHeader,
    pub payload: Vec<u8>,
    pub arrival_time_ms: i64,
}

/// C++ `NetEqInput::SetMinimumDelayInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetMinimumDelayInfo {
    pub timestamp_ms: i64,
    pub delay_ms: i32,
}

/// C++ `NetEqInput`.
pub trait NetEqInput {
    /// Arrival time of the next packet, if any.
    fn next_packet_time(&self) -> Option<i64>;

    /// Time of the next `GetAudio` event, if any.
    fn next_output_event_time(&self) -> Option<i64>;

    /// The next `set_minimum_delay` event, if any.
    fn next_set_minimum_delay_info(&self) -> Option<SetMinimumDelayInfo> {
        None
    }

    /// Earliest of the three event times above.
    fn next_event_time(&self) -> Option<i64> {
        [
            self.next_packet_time(),
            self.next_output_event_time(),
            self.next_set_minimum_delay_info()
                .map(|info| info.timestamp_ms),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Removes and returns the next packet.
    fn pop_packet(&mut self) -> Option<PacketData>;

    /// Moves to the next output event.
    fn advance_output_event(&mut self);

    /// Moves to the next set-minimum-delay event.
    fn advance_set_minimum_delay(&mut self) {}

    /// True when there are no more events.
    fn ended(&self) -> bool;

    /// The next packet, without removing it.
    fn next_packet(&self) -> Option<&PacketData>;
}

/// A fixed script: packets in arrival order plus `GetAudio` every
/// `output_period_ms` from `start_ms` up to and including `end_ms`. Not in
/// C++ (which reads RTP dumps); this is the harness-style input
/// (PARITY_PLAN.md 7.1).
#[derive(Debug, Clone)]
pub struct ScriptedInput {
    packets: std::collections::VecDeque<PacketData>,
    next_output_ms: Option<i64>,
    output_period_ms: i64,
    end_ms: i64,
    min_delay_events: std::collections::VecDeque<SetMinimumDelayInfo>,
}

impl ScriptedInput {
    /// `packets` are sorted by arrival time (stable, so equal arrival times
    /// keep script order). Output events run every `output_period_ms` from
    /// `start_ms` to `end_ms` inclusive.
    pub fn new(
        mut packets: Vec<PacketData>,
        start_ms: i64,
        end_ms: i64,
        output_period_ms: i64,
    ) -> Self {
        assert!(output_period_ms > 0);
        packets.sort_by_key(|packet| packet.arrival_time_ms);
        Self {
            packets: packets.into(),
            next_output_ms: (start_ms <= end_ms).then_some(start_ms),
            output_period_ms,
            end_ms,
            min_delay_events: Default::default(),
        }
    }

    /// Adds `set_base_minimum_delay_ms` events (sorted by time).
    pub fn with_minimum_delay_events(mut self, mut events: Vec<SetMinimumDelayInfo>) -> Self {
        events.sort_by_key(|event| event.timestamp_ms);
        self.min_delay_events = events.into();
        self
    }
}

impl NetEqInput for ScriptedInput {
    fn next_packet_time(&self) -> Option<i64> {
        self.packets.front().map(|packet| packet.arrival_time_ms)
    }

    fn next_output_event_time(&self) -> Option<i64> {
        self.next_output_ms
    }

    fn next_set_minimum_delay_info(&self) -> Option<SetMinimumDelayInfo> {
        self.min_delay_events.front().copied()
    }

    fn pop_packet(&mut self) -> Option<PacketData> {
        self.packets.pop_front()
    }

    fn advance_output_event(&mut self) {
        self.next_output_ms = self
            .next_output_ms
            .map(|time| time + self.output_period_ms)
            .filter(|time| *time <= self.end_ms);
    }

    fn advance_set_minimum_delay(&mut self) {
        self.min_delay_events.pop_front();
    }

    fn ended(&self) -> bool {
        self.next_output_ms.is_none()
    }

    fn next_packet(&self) -> Option<&PacketData> {
        self.packets.front()
    }
}

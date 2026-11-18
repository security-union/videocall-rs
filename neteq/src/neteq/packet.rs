// Port of WebRTC modules/audio_coding/neteq/packet.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! An RTP packet as NetEq stores it, and its C++ ordering.

use std::cmp::Ordering;
use std::collections::VecDeque;

use crate::api::audio_decoder::EncodedAudioFrame;
use crate::api::rtp::RtpPacketInfo;
use crate::api::tick_timer::Stopwatch;

/// C++ `Packet::Priority`. Lower values are higher priority. `codec_level`
/// is the codec's own redundancy level (Opus FEC is 1); `red_level` is the
/// RED redundancy level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Priority {
    pub codec_level: i32,
    pub red_level: i32,
}

impl Priority {
    /// C++ `Priority(codec_level, red_level)`. Both must be non-negative.
    pub fn new(codec_level: i32, red_level: i32) -> Self {
        let priority = Self {
            codec_level,
            red_level,
        };
        priority.check_invariant();
        priority
    }

    fn check_invariant(&self) {
        debug_assert!(self.codec_level >= 0);
        debug_assert!(self.red_level >= 0);
    }
}

impl PartialOrd for Priority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// C++ `Priority::operator<`: by `codec_level`, then `red_level`.
impl Ord for Priority {
    fn cmp(&self, b: &Self) -> Ordering {
        self.check_invariant();
        b.check_invariant();
        if self.codec_level == b.codec_level {
            self.red_level.cmp(&b.red_level)
        } else {
            self.codec_level.cmp(&b.codec_level)
        }
    }
}

/// C++ `Packet`.
#[derive(Default)]
pub struct Packet {
    pub timestamp: u32,
    pub sequence_number: u16,
    pub payload_type: u8,
    /// Raw payload. Empty once the payload has been parsed into `frame`.
    pub payload: Vec<u8>,
    pub priority: Priority,
    pub packet_info: Option<RtpPacketInfo>,
    /// Started by `PacketBuffer::insert_packet`; measures time in the buffer.
    pub waiting_time: Option<Stopwatch>,
    /// The parsed frame, set by `NetEqImpl` from `AudioDecoder::parse_payload`.
    pub frame: Option<Box<dyn EncodedAudioFrame>>,
}

impl std::fmt::Debug for Packet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Packet")
            .field("timestamp", &self.timestamp)
            .field("sequence_number", &self.sequence_number)
            .field("payload_type", &self.payload_type)
            .field("payload_len", &self.payload.len())
            .field("priority", &self.priority)
            .field("packet_info", &self.packet_info)
            .field("has_waiting_time", &self.waiting_time.is_some())
            .field(
                "frame_duration",
                &self.frame.as_ref().map(|frame| frame.duration()),
            )
            .finish()
    }
}

impl Packet {
    /// An empty packet (C++ default constructor).
    pub fn new() -> Self {
        Self::default()
    }

    /// C++ `Clone()`: copies everything except `waiting_time`. Panics if the
    /// packet holds a parsed `frame`, like the C++ `RTC_CHECK(!frame)`.
    pub fn clone_packet(&self) -> Packet {
        assert!(
            self.frame.is_none(),
            "Packet::clone_packet with a parsed frame"
        );
        Packet {
            timestamp: self.timestamp,
            sequence_number: self.sequence_number,
            payload_type: self.payload_type,
            payload: self.payload.clone(),
            priority: self.priority,
            packet_info: self.packet_info,
            waiting_time: None,
            frame: None,
        }
    }

    /// True if there is neither a parsed frame nor a raw payload.
    pub fn empty(&self) -> bool {
        self.frame.is_none() && self.payload.is_empty()
    }

    /// C++ `operator<`: earlier timestamp first (with wraparound), then
    /// earlier sequence number (with wraparound), then higher priority
    /// (lower `Priority`) first. Not a strict weak ordering across wraps.
    pub fn lt(&self, rhs: &Packet) -> bool {
        if self.timestamp == rhs.timestamp {
            if self.sequence_number == rhs.sequence_number {
                return self.priority < rhs.priority;
            }
            return rhs.sequence_number.wrapping_sub(self.sequence_number) < 0xFFFF / 2;
        }
        rhs.timestamp.wrapping_sub(self.timestamp) < 0xFFFF_FFFF / 2
    }

    /// C++ `operator>`.
    pub fn gt(&self, rhs: &Packet) -> bool {
        rhs.lt(self)
    }

    /// C++ `operator<=`.
    pub fn le(&self, rhs: &Packet) -> bool {
        !self.gt(rhs)
    }

    /// C++ `operator>=`.
    pub fn ge(&self, rhs: &Packet) -> bool {
        !self.lt(rhs)
    }
}

/// C++ `operator==`: same timestamp, sequence number and priority.
impl PartialEq for Packet {
    fn eq(&self, rhs: &Self) -> bool {
        self.timestamp == rhs.timestamp
            && self.sequence_number == rhs.sequence_number
            && self.priority == rhs.priority
    }
}

/// C++ `std::list<Packet>`. Insertion order matters; see `PacketBuffer`.
pub type PacketList = VecDeque<Packet>;

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(timestamp: u32, sequence_number: u16, codec_level: i32, red_level: i32) -> Packet {
        Packet {
            timestamp,
            sequence_number,
            priority: Priority::new(codec_level, red_level),
            ..Packet::default()
        }
    }

    #[test]
    fn priority_orders_codec_level_then_red_level() {
        assert!(Priority::new(0, 1) < Priority::new(1, 0));
        assert!(Priority::new(1, 0) < Priority::new(1, 1));
        assert_eq!(Priority::new(2, 3), Priority::new(2, 3));
    }

    #[test]
    fn ordering_by_timestamp_with_wrap() {
        let a = packet(0xFFFF_FFF0, 0, 0, 0);
        let b = packet(0x10, 0, 0, 0);
        assert!(a.lt(&b));
        assert!(b.gt(&a));
        assert!(!b.lt(&a));
    }

    #[test]
    fn ordering_by_sequence_number_with_wrap() {
        let a = packet(100, 0xFFFF, 0, 0);
        let b = packet(100, 0x0001, 0, 0);
        assert!(a.lt(&b));
        assert!(a.le(&b));
        assert!(b.ge(&a));
    }

    #[test]
    fn ordering_by_priority_and_equality() {
        let primary = packet(100, 5, 0, 0);
        let fec = packet(100, 5, 1, 0);
        assert!(primary.lt(&fec));
        assert!(primary != fec);
        assert!(primary == packet(100, 5, 0, 0));
        assert!(primary.le(&packet(100, 5, 0, 0)));
        assert!(primary.ge(&packet(100, 5, 0, 0)));
    }

    #[test]
    fn clone_packet_copies_fields_but_not_waiting_time() {
        let timer = crate::api::tick_timer::TickTimer::new();
        let mut original = packet(7, 8, 1, 2);
        original.payload_type = 96;
        original.payload = vec![1, 2, 3];
        original.waiting_time = Some(timer.get_new_stopwatch());
        let copy = original.clone_packet();
        assert!(copy == original);
        assert_eq!(96, copy.payload_type);
        assert_eq!(vec![1, 2, 3], copy.payload);
        assert!(copy.waiting_time.is_none());
        assert!(!copy.empty());
        assert!(Packet::new().empty());
    }
}

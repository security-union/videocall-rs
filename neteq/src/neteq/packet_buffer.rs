// `set_max_number_of_packets` takes `stats`, because C++ logs each packet it
// discards there through `DiscardNextPacket`.
//
// Port target: WebRTC modules/audio_coding/neteq/packet_buffer.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: packet_buffer_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// C++ keeps `StatisticsCalculator*`; here the methods that log discards or
// flushes take `stats: &mut StatisticsCalculator` as their last argument.
// `SmartFlushingConfig` lives in `api::config` (no field trials). C++
// `MockStatisticsCalculator` expectations become assertions on
// `stats.get_lifetime_statistics()`.

//! The jitter buffer proper: an ordered list of packets.

use crate::api::config::SmartFlushingConfig;
use crate::api::tick_timer::TickTimer;
use crate::neteq::packet::{Packet, PacketList, Priority};
use crate::neteq::statistics_calculator::StatisticsCalculator;
use crate::rtc_base::{dchecked_cast, is_newer_timestamp};

/// The one `DecoderDatabase` query `PacketBuffer` needs. A trait so the
/// packet buffer tests can run without a `DecoderDatabase`, which implements
/// it.
pub trait ComfortNoiseLookup {
    /// C++ `DecoderDatabase::IsComfortNoise`.
    fn is_comfort_noise(&self, rtp_payload_type: u8) -> bool;
}

/// C++ `PacketBuffer`.
#[derive(Debug)]
pub struct PacketBuffer {
    smart_flushing_config: Option<SmartFlushingConfig>,
    max_number_of_packets: usize,
    buffer: PacketList,
    tick_timer: TickTimer,
}

impl PacketBuffer {
    /// C++ `kOK`.
    pub const K_OK: i32 = 0;
    /// C++ `kFlushed`.
    pub const K_FLUSHED: i32 = 1;
    /// C++ `kPartialFlush`.
    pub const K_PARTIAL_FLUSH: i32 = 2;
    /// C++ `kNotFound`.
    pub const K_NOT_FOUND: i32 = 3;
    /// C++ `kBufferEmpty`.
    pub const K_BUFFER_EMPTY: i32 = 4;
    /// C++ `kInvalidPacket`.
    pub const K_INVALID_PACKET: i32 = 5;
    /// C++ `kInvalidPointer`.
    pub const K_INVALID_POINTER: i32 = 6;

    /// Buffer of at most `max_number_of_packets` packets. `smart_flushing`
    /// is `Config::smart_flushing`.
    pub fn new(
        max_number_of_packets: usize,
        tick_timer: TickTimer,
        smart_flushing: Option<SmartFlushingConfig>,
    ) -> Self {
        Self {
            smart_flushing_config: smart_flushing,
            max_number_of_packets,
            buffer: PacketList::new(),
            tick_timer,
        }
    }

    /// Deletes all packets, logging each as discarded, and counts a flush.
    pub fn flush(&mut self, stats: &mut StatisticsCalculator) {
        for packet in self.buffer.iter() {
            log_packet_discarded(packet.priority.codec_level, stats);
        }
        self.buffer.clear();
        stats.flushed_packet_buffer();
    }

    /// Discards packets from the front until the span is under the target
    /// level and at most half the capacity is used.
    pub fn partial_flush(
        &mut self,
        target_level_ms: i32,
        sample_rate: usize,
        last_decoded_length: usize,
        stats: &mut StatisticsCalculator,
    ) {
        // Make sure that at least half the packet buffer capacity will be
        // available after the flush. This is done to avoid getting stuck if
        // the target level is very high.
        let mut target_level_samples = std::cmp::min(
            target_level_ms.wrapping_mul(sample_rate as i32) / 1000,
            (self.max_number_of_packets.wrapping_mul(last_decoded_length) / 2) as i32,
        );
        // We should avoid flushing to very low levels.
        if let Some(config) = self.smart_flushing_config.as_ref() {
            target_level_samples = std::cmp::max(
                target_level_samples,
                config
                    .target_level_threshold_ms
                    .wrapping_mul(sample_rate as i32)
                    / 1000,
            );
        }
        while !self.buffer.is_empty()
            && (self.get_span_samples(last_decoded_length, sample_rate, false)
                > target_level_samples as usize
                || self.buffer.len() > self.max_number_of_packets / 2)
        {
            if let Some(packet) = self.buffer.pop_front() {
                log_packet_discarded(packet.priority.codec_level, stats);
            }
        }
    }

    /// Sets the capacity. If the buffer holds more packets, the oldest are
    /// discarded (and logged to `stats`).
    pub fn set_max_number_of_packets(
        &mut self,
        max_number_of_packets: usize,
        stats: &mut StatisticsCalculator,
    ) {
        self.max_number_of_packets = max_number_of_packets;
        while self.buffer.len() > self.max_number_of_packets {
            self.discard_next_packet(stats);
        }
    }

    /// True if there are no packets.
    pub fn empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Inserts `packet` in order, starting its waiting-time stopwatch.
    /// Returns `K_OK`, `K_FLUSHED`, `K_PARTIAL_FLUSH` or `K_INVALID_PACKET`.
    pub fn insert_packet(
        &mut self,
        mut packet: Packet,
        last_decoded_length: usize,
        sample_rate: usize,
        target_level_ms: i32,
        stats: &mut StatisticsCalculator,
    ) -> i32 {
        if packet.empty() {
            log::warn!("InsertPacket invalid packet");
            return Self::K_INVALID_PACKET;
        }

        debug_assert!(packet.priority.codec_level >= 0);
        debug_assert!(packet.priority.red_level >= 0);

        let mut return_val = Self::K_OK;

        packet.waiting_time = Some(self.tick_timer.get_new_stopwatch());

        // Perform a smart flush if the buffer size exceeds a multiple of the
        // target level.
        let span_threshold: usize = match self.smart_flushing_config.as_ref() {
            Some(config) => {
                (config.target_level_multiplier
                    * std::cmp::max(config.target_level_threshold_ms, target_level_ms) as f64
                    * (sample_rate as i32) as f64
                    / 1000.0) as usize
            }
            None => 0,
        };
        let smart_flush = self.smart_flushing_config.is_some()
            && self.get_span_samples(last_decoded_length, sample_rate, false) >= span_threshold;
        if self.buffer.len() >= self.max_number_of_packets || smart_flush {
            let buffer_size_before_flush = self.buffer.len();
            if self.smart_flushing_config.is_some() {
                // Flush down to the target level.
                self.partial_flush(target_level_ms, sample_rate, last_decoded_length, stats);
                return_val = Self::K_PARTIAL_FLUSH;
                let discarded = buffer_size_before_flush - self.buffer.len();
                if discarded > 0 {
                    log::trace!("Partial flushed {discarded} packets to reach target level.");
                }
            } else {
                // Buffer is full.
                self.flush(stats);
                return_val = Self::K_FLUSHED;
                log::warn!("Packet buffer fully flushed.");
            }
        }

        // Find the place in the buffer where the new packet should be
        // inserted. The list is searched from the back, since the most likely
        // case is that the new packet should be near the end of the list.
        // C++ `NewTimestampIsLarger`: `packet` goes before `new_packet` when
        // `new_packet >= packet`.
        let rit = self.buffer.iter().rposition(|existing| packet.ge(existing));

        // The new packet is to be inserted to the right of `rit`. If it has
        // the same timestamp as `rit`, which has a higher priority, do not
        // insert the new packet to list.
        if let Some(index) = rit {
            if packet.timestamp == self.buffer[index].timestamp {
                log_packet_discarded(packet.priority.codec_level, stats);
                return return_val;
            }
        }

        // The new packet is to be inserted to the left of `it`. If it has the
        // same timestamp as `it`, which has a lower priority, replace `it`
        // with the new packet.
        let it = rit.map_or(0, |index| index + 1);
        if it < self.buffer.len() && packet.timestamp == self.buffer[it].timestamp {
            log_packet_discarded(self.buffer[it].priority.codec_level, stats);
            self.buffer.remove(it);
        }
        self.buffer.insert(it, packet); // Insert the packet at that position.

        return_val
    }

    /// Timestamp of the first packet. Returns `K_OK` or `K_BUFFER_EMPTY`.
    pub fn next_timestamp(&self, next_timestamp: &mut u32) -> i32 {
        match self.buffer.front() {
            None => Self::K_BUFFER_EMPTY,
            Some(front) => {
                *next_timestamp = front.timestamp;
                Self::K_OK
            }
        }
    }

    /// First timestamp at or after `timestamp` (plain unsigned compare, no
    /// wraparound). Returns `K_OK`, `K_BUFFER_EMPTY` or `K_NOT_FOUND`.
    pub fn next_higher_timestamp(&self, timestamp: u32, next_timestamp: &mut u32) -> i32 {
        if self.empty() {
            return Self::K_BUFFER_EMPTY;
        }
        for packet in self.buffer.iter() {
            if packet.timestamp >= timestamp {
                // Found a packet matching the search.
                *next_timestamp = packet.timestamp;
                return Self::K_OK;
            }
        }
        Self::K_NOT_FOUND
    }

    /// The first packet, if any.
    pub fn peek_next_packet(&self) -> Option<&Packet> {
        self.buffer.front()
    }

    /// Removes and returns the first packet.
    pub fn get_next_packet(&mut self) -> Option<Packet> {
        let packet = self.buffer.pop_front()?;
        // Assert that the packet sanity checks in insert_packet work.
        debug_assert!(!packet.empty());
        Some(packet)
    }

    /// Discards the first packet. Returns `K_OK` or `K_BUFFER_EMPTY`.
    pub fn discard_next_packet(&mut self, stats: &mut StatisticsCalculator) -> i32 {
        let Some(packet) = self.buffer.pop_front() else {
            return Self::K_BUFFER_EMPTY;
        };
        // Assert that the packet sanity checks in insert_packet work.
        debug_assert!(!packet.empty());
        log_packet_discarded(packet.priority.codec_level, stats);
        Self::K_OK
    }

    /// Discards packets older than `timestamp_limit` but not older than
    /// `timestamp_limit - horizon_samples` (0 means half the timestamp
    /// range).
    pub fn discard_old_packets(
        &mut self,
        timestamp_limit: u32,
        horizon_samples: u32,
        stats: &mut StatisticsCalculator,
    ) {
        self.buffer.retain(|packet| {
            if timestamp_limit == packet.timestamp
                || !Self::is_obsolete_timestamp(packet.timestamp, timestamp_limit, horizon_samples)
            {
                return true;
            }
            log_packet_discarded(packet.priority.codec_level, stats);
            false
        });
    }

    /// Discards all packets older than `timestamp_limit`.
    pub fn discard_all_old_packets(
        &mut self,
        timestamp_limit: u32,
        stats: &mut StatisticsCalculator,
    ) {
        self.discard_old_packets(timestamp_limit, 0, stats);
    }

    /// Discards all packets with `payload_type`.
    pub fn discard_packets_with_payload_type(
        &mut self,
        payload_type: u8,
        stats: &mut StatisticsCalculator,
    ) {
        self.buffer.retain(|packet| {
            if packet.payload_type != payload_type {
                return true;
            }
            log_packet_discarded(packet.priority.codec_level, stats);
            false
        });
    }

    /// Number of packets, including duplicates and redundant packets.
    pub fn num_packets_in_buffer(&self) -> usize {
        self.buffer.len()
    }

    /// Number of samples in the buffer, counting packets without a known
    /// duration as the most recent valid duration (initially
    /// `last_decoded_length`).
    pub fn num_samples_in_buffer(&self, last_decoded_length: usize) -> usize {
        let mut num_samples: usize = 0;
        let mut last_duration = last_decoded_length;
        for packet in self.buffer.iter() {
            if let Some(frame) = packet.frame.as_ref() {
                // TODO(hlundin): Verify that it's fine to count all packets
                // and remove this check.
                if packet.priority != Priority::new(0, 0) {
                    continue;
                }
                let duration = frame.duration();
                if duration > 0 {
                    last_duration = duration; // Save the most up-to-date (valid) duration.
                }
            }
            num_samples = num_samples.wrapping_add(last_duration);
        }
        num_samples
    }

    /// Span from the first to the last timestamp plus the last duration,
    /// or plus the last packet's waiting time if `count_waiting_time`.
    pub fn get_span_samples(
        &self,
        last_decoded_length: usize,
        sample_rate: usize,
        count_waiting_time: bool,
    ) -> usize {
        let (Some(front), Some(back)) = (self.buffer.front(), self.buffer.back()) else {
            return 0;
        };

        // C++: `uint32_t - uint32_t` converted to `size_t`.
        let mut span = back.timestamp.wrapping_sub(front.timestamp) as usize;
        // Every buffered packet got a stopwatch in `insert_packet`.
        let elapsed_ms = back
            .waiting_time
            .as_ref()
            .map_or(0, |waiting_time| waiting_time.elapsed_ms());
        let waiting_time_samples: usize =
            dchecked_cast(elapsed_ms.wrapping_mul((sample_rate / 1000) as u64));
        if count_waiting_time {
            span = span.wrapping_add(waiting_time_samples);
        } else if let Some(frame) = back.frame.as_ref().filter(|frame| frame.duration() > 0) {
            let mut duration = frame.duration();
            if frame.is_dtx_packet() {
                duration = std::cmp::max(duration, waiting_time_samples);
            }
            span = span.wrapping_add(duration);
        } else {
            span = span.wrapping_add(last_decoded_length);
        }
        span
    }

    /// True if any packet is DTX or comfort noise.
    pub fn contains_dtx_or_cng_packet(&self, decoder_database: &dyn ComfortNoiseLookup) -> bool {
        self.buffer.iter().any(|packet| {
            packet
                .frame
                .as_ref()
                .is_some_and(|frame| frame.is_dtx_packet())
                || decoder_database.is_comfort_noise(packet.payload_type)
        })
    }

    /// C++ static `IsObsoleteTimestamp`: `timestamp` is older than
    /// `timestamp_limit` but within `horizon_samples` (0 = unlimited).
    pub fn is_obsolete_timestamp(
        timestamp: u32,
        timestamp_limit: u32,
        horizon_samples: u32,
    ) -> bool {
        is_newer_timestamp(timestamp_limit, timestamp)
            && (horizon_samples == 0
                || is_newer_timestamp(timestamp, timestamp_limit.wrapping_sub(horizon_samples)))
    }
}

/// C++ `PacketBuffer::LogPacketDiscarded`.
fn log_packet_discarded(codec_level: i32, stats: &mut StatisticsCalculator) {
    if codec_level > 0 {
        stats.secondary_packets_discarded(1);
    } else {
        stats.packets_discarded(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::audio_decoder::{AudioDecoder, DecodeResult, EncodedAudioFrame};

    /// Stands in for C++ `MockEncodedAudioFrame`.
    struct FakeEncodedAudioFrame {
        duration: usize,
        is_dtx: bool,
    }

    impl EncodedAudioFrame for FakeEncodedAudioFrame {
        fn duration(&self) -> usize {
            self.duration
        }
        fn is_dtx_packet(&self) -> bool {
            self.is_dtx
        }
        fn decode(
            &self,
            _decoder: &mut dyn AudioDecoder,
            _decoded: &mut [i16],
        ) -> Option<DecodeResult> {
            None
        }
    }

    /// C++ `PacketGenerator`.
    struct PacketGenerator {
        seq_no: u16,
        ts: u32,
        pt: u8,
        frame_size: i32,
    }

    impl PacketGenerator {
        fn new(seq_no: u16, ts: u32, pt: u8, frame_size: i32) -> Self {
            Self {
                seq_no,
                ts,
                pt,
                frame_size,
            }
        }

        fn reset(&mut self, seq_no: u16, ts: u32, pt: u8, frame_size: i32) {
            *self = Self::new(seq_no, ts, pt, frame_size);
        }

        fn next_packet(
            &mut self,
            payload_size_bytes: usize,
            audio_frame: Option<Box<dyn EncodedAudioFrame>>,
        ) -> Packet {
            let packet = Packet {
                sequence_number: self.seq_no,
                timestamp: self.ts,
                payload_type: self.pt,
                payload: vec![0; payload_size_bytes],
                frame: audio_frame,
                ..Packet::default()
            };
            self.seq_no = self.seq_no.wrapping_add(1);
            self.ts = self.ts.wrapping_add(self.frame_size as u32);
            packet
        }
    }

    struct PacketsToInsert {
        sequence_number: u16,
        timestamp: u32,
        payload_type: u8,
        primary: bool,
        /// Order of this packet to appear upon extraction, after inserting a
        /// series of packets. A negative number means that it should have
        /// been discarded before extraction.
        extract_order: i32,
    }

    fn discarded(stats: &StatisticsCalculator) -> u64 {
        stats.get_lifetime_statistics().packets_discarded
    }

    fn secondary_discarded(stats: &StatisticsCalculator) -> u64 {
        stats.get_lifetime_statistics().fec_packets_discarded
    }

    fn smart_flushing_test_config() -> Option<SmartFlushingConfig> {
        // "WebRTC-Audio-NetEqSmartFlushing/enabled:true,
        //  target_level_threshold_ms:0,target_level_multiplier:2/"
        Some(SmartFlushingConfig {
            target_level_threshold_ms: 0,
            target_level_multiplier: 2.0,
        })
    }

    #[test]
    fn create_and_destroy() {
        let tick_timer = TickTimer::new();
        let buffer = PacketBuffer::new(10, tick_timer, None); // 10 packets.
        assert!(buffer.empty());
    }

    #[test]
    fn insert_packet() {
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(10, tick_timer, None); // 10 packets.
        let mut gen = PacketGenerator::new(17, 4711, 0, 10);

        let payload_len = 100;
        let packet = gen.next_packet(payload_len, None);
        assert_eq!(
            0,
            buffer.insert_packet(packet.clone_packet(), 10, 8000, 60, &mut stats)
        );
        let mut next_ts = 0;
        assert_eq!(PacketBuffer::K_OK, buffer.next_timestamp(&mut next_ts));
        assert_eq!(4711, next_ts);
        assert!(!buffer.empty());
        assert_eq!(1, buffer.num_packets_in_buffer());
        let next_packet = buffer.peek_next_packet().expect("packet");
        assert!(packet == *next_packet); // Compare contents.
        assert_eq!(0, discarded(&stats));
    }

    // Test to flush buffer.
    #[test]
    fn flush_buffer() {
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(10, tick_timer, None); // 10 packets.
        let mut gen = PacketGenerator::new(0, 0, 0, 10);
        let payload_len = 10;

        // Insert 10 small packets; should be ok.
        for _ in 0..10 {
            assert_eq!(
                PacketBuffer::K_OK,
                buffer.insert_packet(gen.next_packet(payload_len, None), 10, 8000, 60, &mut stats)
            );
        }
        assert_eq!(10, buffer.num_packets_in_buffer());
        assert!(!buffer.empty());

        buffer.flush(&mut stats);
        assert_eq!(10, discarded(&stats));
        assert_eq!(1, stats.get_operations_and_state().packet_buffer_flushes);
        // Buffer should delete the payloads itself.
        assert_eq!(0, buffer.num_packets_in_buffer());
        assert!(buffer.empty());
    }

    // Test to fill the buffer over the limits, and verify that it flushes.
    #[test]
    fn overfill_buffer() {
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(10, tick_timer, None); // 10 packets.
        let mut gen = PacketGenerator::new(0, 0, 0, 10);

        // Insert 10 small packets; should be ok.
        let payload_len = 10;
        for _ in 0..10 {
            assert_eq!(
                PacketBuffer::K_OK,
                buffer.insert_packet(gen.next_packet(payload_len, None), 10, 8000, 60, &mut stats)
            );
        }
        assert_eq!(10, buffer.num_packets_in_buffer());
        let mut next_ts = 0;
        assert_eq!(PacketBuffer::K_OK, buffer.next_timestamp(&mut next_ts));
        assert_eq!(0, next_ts); // Expect first inserted packet to be first in line.

        let packet = gen.next_packet(payload_len, None);
        // Insert 11th packet; should flush the buffer and insert it after
        // flushing.
        assert_eq!(
            PacketBuffer::K_FLUSHED,
            buffer.insert_packet(packet.clone_packet(), 10, 8000, 60, &mut stats)
        );
        assert_eq!(10, discarded(&stats));
        assert_eq!(1, buffer.num_packets_in_buffer());
        assert_eq!(PacketBuffer::K_OK, buffer.next_timestamp(&mut next_ts));
        // Expect last inserted packet to be first in line.
        assert_eq!(packet.timestamp, next_ts);
    }

    #[test]
    fn extract_order_redundancy() {
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(100, tick_timer, None); // 100 packets.
        const FRAME_SIZE: i32 = 10;
        const PAYLOAD_LENGTH: usize = 10;

        let facts =
            |sequence_number, timestamp, payload_type, primary, extract_order| PacketsToInsert {
                sequence_number,
                timestamp,
                payload_type,
                primary,
                extract_order,
            };
        let packet_facts = [
            facts(0xFFFD, 0xFFFFFFD7, 0, true, 0),
            facts(0xFFFE, 0xFFFFFFE1, 0, true, 1),
            facts(0xFFFE, 0xFFFFFFD7, 1, false, -1),
            facts(0xFFFF, 0xFFFFFFEB, 0, true, 2),
            facts(0xFFFF, 0xFFFFFFE1, 1, false, -1),
            facts(0x0000, 0xFFFFFFF5, 0, true, 3),
            facts(0x0000, 0xFFFFFFEB, 1, false, -1),
            facts(0x0001, 0xFFFFFFFF, 0, true, 4),
            facts(0x0001, 0xFFFFFFF5, 1, false, -1),
            facts(0x0002, 0x0000000A, 0, true, 5),
            facts(0x0002, 0xFFFFFFFF, 1, false, -1),
            facts(0x0003, 0x0000000A, 1, false, -1),
            facts(0x0004, 0x0000001E, 0, true, 7),
            facts(0x0004, 0x00000014, 1, false, 6),
            facts(0x0005, 0x0000001E, 0, true, -1),
            facts(0x0005, 0x00000014, 1, false, -1),
            facts(0x0006, 0x00000028, 0, true, 8),
            facts(0x0006, 0x0000001E, 1, false, -1),
        ];

        const EXPECT_PACKETS_IN_BUFFER: usize = 9;
        let mut expect_order: Vec<Option<Packet>> =
            (0..EXPECT_PACKETS_IN_BUFFER).map(|_| None).collect();

        let mut gen = PacketGenerator::new(0, 0, 0, FRAME_SIZE);

        // Checking the discard counters after every insert replaces the C++
        // InSequence + MockFunction check: exactly one discard of the right
        // kind happens for each packet that must not be extracted.
        for fact in packet_facts.iter() {
            gen.reset(
                fact.sequence_number,
                fact.timestamp,
                fact.payload_type,
                FRAME_SIZE,
            );
            let mut packet = gen.next_packet(PAYLOAD_LENGTH, None);
            packet.priority.codec_level = if fact.primary { 0 } else { 1 };
            let primary_before = discarded(&stats);
            let secondary_before = secondary_discarded(&stats);
            assert_eq!(
                PacketBuffer::K_OK,
                buffer.insert_packet(
                    packet.clone_packet(),
                    FRAME_SIZE as usize,
                    8000,
                    60,
                    &mut stats
                )
            );
            let (expect_primary, expect_secondary) = match (fact.extract_order < 0, fact.primary) {
                (false, _) => (0, 0),
                (true, true) => (1, 0),
                (true, false) => (0, 1),
            };
            assert_eq!(expect_primary, discarded(&stats) - primary_before);
            assert_eq!(
                expect_secondary,
                secondary_discarded(&stats) - secondary_before
            );
            if fact.extract_order >= 0 {
                expect_order[fact.extract_order as usize] = Some(packet);
            }
        }

        assert_eq!(EXPECT_PACKETS_IN_BUFFER, buffer.num_packets_in_buffer());

        for expected in expect_order.iter() {
            let packet = buffer.get_next_packet();
            assert!(packet.as_ref() == expected.as_ref()); // Compare contents.
        }
        assert!(buffer.empty());
    }

    #[test]
    fn discard_packets() {
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(100, tick_timer, None); // 100 packets.
        const START_SEQ_NO: u16 = 17;
        const START_TS: u32 = 4711;
        const TS_INCREMENT: u32 = 10;
        let mut gen = PacketGenerator::new(START_SEQ_NO, START_TS, 0, TS_INCREMENT as i32);
        let payload_len = 10;

        const TOTAL_PACKETS: u32 = 10;
        // Insert 10 small packets.
        for _ in 0..TOTAL_PACKETS {
            buffer.insert_packet(
                gen.next_packet(payload_len, None),
                TS_INCREMENT as usize,
                8000,
                60,
                &mut stats,
            );
        }
        assert_eq!(10, buffer.num_packets_in_buffer());

        let mut current_ts = START_TS;

        // Discard them one by one and make sure that the right packets are at
        // the front of the buffer.
        const DISCARD_PACKETS: u32 = 5;
        for i in 0..DISCARD_PACKETS {
            let mut ts = 0;
            assert_eq!(PacketBuffer::K_OK, buffer.next_timestamp(&mut ts));
            assert_eq!(current_ts, ts);
            assert_eq!(PacketBuffer::K_OK, buffer.discard_next_packet(&mut stats));
            // Exactly one discard per call.
            assert_eq!(u64::from(i + 1), discarded(&stats));
            current_ts += TS_INCREMENT;
        }

        const REMAINING_PACKETS: u32 = TOTAL_PACKETS - DISCARD_PACKETS;
        // This will discard all remaining packets but one. The oldest packet
        // is older than the indicated horizon_samples, and will thus be left
        // in the buffer.
        const SKIP_PACKETS: usize = 1;
        buffer.discard_old_packets(
            START_TS + TOTAL_PACKETS * TS_INCREMENT,
            REMAINING_PACKETS * TS_INCREMENT,
            &mut stats,
        );
        assert_eq!(
            u64::from(DISCARD_PACKETS + REMAINING_PACKETS) - SKIP_PACKETS as u64,
            discarded(&stats)
        );

        assert_eq!(SKIP_PACKETS, buffer.num_packets_in_buffer());
        let mut ts = 0;
        assert_eq!(PacketBuffer::K_OK, buffer.next_timestamp(&mut ts));
        assert_eq!(current_ts, ts);

        // Discard all remaining packets.
        buffer.discard_all_old_packets(START_TS + TOTAL_PACKETS * TS_INCREMENT, &mut stats);
        assert_eq!(u64::from(TOTAL_PACKETS), discarded(&stats));

        assert!(buffer.empty());
    }

    #[test]
    fn reordering() {
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(100, tick_timer, None); // 100 packets.
        const START_SEQ_NO: u16 = 17;
        const START_TS: u32 = 4711;
        const TS_INCREMENT: u32 = 10;
        let mut gen = PacketGenerator::new(START_SEQ_NO, START_TS, 0, TS_INCREMENT as i32);
        let payload_len = 10;

        // Generate 10 small packets and insert them into a PacketList. Insert
        // every odd packet to the front, and every even packet to the back,
        // thus creating a (rather strange) reordering.
        let mut list = PacketList::new();
        for i in 0..10 {
            let packet = gen.next_packet(payload_len, None);
            if i % 2 == 1 {
                list.push_front(packet);
            } else {
                list.push_back(packet);
            }
        }

        for packet in list {
            assert_eq!(
                PacketBuffer::K_OK,
                buffer.insert_packet(packet, 10, 8000, 60, &mut stats)
            );
        }
        assert_eq!(10, buffer.num_packets_in_buffer());

        // Extract them and make sure that come out in the right order.
        let mut current_ts = START_TS;
        for _ in 0..10 {
            let packet = buffer.get_next_packet().expect("packet");
            assert_eq!(current_ts, packet.timestamp);
            current_ts += TS_INCREMENT;
        }
        assert!(buffer.empty());
    }

    #[test]
    fn failures() {
        const START_SEQ_NO: u16 = 17;
        const START_TS: u32 = 4711;
        const TS_INCREMENT: i32 = 10;
        let payload_len = 100;
        let mut gen = PacketGenerator::new(START_SEQ_NO, START_TS, 0, TS_INCREMENT);
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();

        let mut buffer = PacketBuffer::new(100, tick_timer, None); // 100 packets.
        {
            let mut packet = gen.next_packet(payload_len, None);
            packet.payload.clear();
            assert_eq!(
                PacketBuffer::K_INVALID_PACKET,
                buffer.insert_packet(packet, 10, 8000, 60, &mut stats)
            );
        }
        // Buffer should still be empty. Test all empty-checks.
        let mut temp_ts = 0;
        assert_eq!(
            PacketBuffer::K_BUFFER_EMPTY,
            buffer.next_timestamp(&mut temp_ts)
        );
        assert_eq!(
            PacketBuffer::K_BUFFER_EMPTY,
            buffer.next_higher_timestamp(0, &mut temp_ts)
        );
        assert!(buffer.peek_next_packet().is_none());
        assert!(buffer.get_next_packet().is_none());

        // Discarding packets will not invoke stats.packets_discarded() because
        // the packet buffer is empty.
        assert_eq!(
            PacketBuffer::K_BUFFER_EMPTY,
            buffer.discard_next_packet(&mut stats)
        );
        buffer.discard_all_old_packets(0, &mut stats);
        assert_eq!(0, discarded(&stats));
    }

    // Test packet comparison function.
    // The function should return true if the first packet "goes before" the
    // second.
    #[test]
    fn compare_packets() {
        let mut gen = PacketGenerator::new(0, 0, 0, 10);
        let mut a = gen.next_packet(10, None); // SN = 0, TS = 0.
        let mut b = gen.next_packet(10, None); // SN = 1, TS = 10.
        let check = |x: &Packet, y: &Packet, eq: bool, lt: bool, gt: bool| {
            assert_eq!(x == y, eq);
            assert_eq!(x != y, !eq);
            assert_eq!(x.lt(y), lt);
            assert_eq!(x.gt(y), gt);
            assert_eq!(x.le(y), !gt);
            assert_eq!(x.ge(y), !lt);
        };
        check(&a, &b, false, true, false);

        // Testing wrap-around case; 'a' is earlier but has a larger timestamp
        // value.
        a.timestamp = 0xFFFFFFFF - 10;
        check(&a, &b, false, true, false);

        // Test equal packets.
        check(&a, &a, true, false, false);

        // Test equal timestamps but different sequence numbers (0 and 1).
        a.timestamp = b.timestamp;
        check(&a, &b, false, true, false);

        // Test equal timestamps but different sequence numbers (32767 and 1).
        a.sequence_number = 0xFFFF;
        check(&a, &b, false, true, false);

        // Test equal timestamps and sequence numbers, but differing
        // priorities.
        a.sequence_number = b.sequence_number;
        a.priority = Priority::new(1, 0);
        b.priority = Priority::new(0, 0);
        // a after b
        check(&a, &b, false, false, true);

        let mut c = gen.next_packet(0, None); // SN = 2, TS = 20.
        let mut d = gen.next_packet(0, None); // SN = 3, TS = 20.
        c.timestamp = b.timestamp;
        d.timestamp = b.timestamp;
        c.sequence_number = b.sequence_number;
        d.sequence_number = b.sequence_number;
        c.priority = Priority::new(1, 1);
        d.priority = Priority::new(0, 1);
        // c after d
        check(&c, &d, false, false, true);
        // c after a
        check(&c, &a, false, false, true);
        // c after b
        check(&c, &b, false, false, true);
        // a after d
        check(&a, &d, false, false, true);
        // d after b
        check(&d, &b, false, false, true);
    }

    #[test]
    fn get_span_samples() {
        const FRAME_SIZE_SAMPLES: usize = 10;
        const PAYLOAD_SIZE_BYTES: usize = 1; // Does not matter to this test;
        const START_TIME_STAMP: u32 = 0xFFFFFFFE; // Close to wrap around.
        const SAMPLE_RATE_HZ: usize = 48000;
        const COUNT_WAITING_TIME: bool = false;
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(3, tick_timer, None);
        let mut gen = PacketGenerator::new(0, START_TIME_STAMP, 0, FRAME_SIZE_SAMPLES as i32);

        let packet_1 = gen.next_packet(PAYLOAD_SIZE_BYTES, None);

        let audio_frame = Box::new(FakeEncodedAudioFrame {
            duration: FRAME_SIZE_SAMPLES,
            is_dtx: false,
        });
        let packet_2 = gen.next_packet(PAYLOAD_SIZE_BYTES, Some(audio_frame));

        assert!(packet_1.timestamp > packet_2.timestamp); // Timestamp wrapped around.

        assert_eq!(
            PacketBuffer::K_OK,
            buffer.insert_packet(packet_1, FRAME_SIZE_SAMPLES, SAMPLE_RATE_HZ, 60, &mut stats)
        );

        const LAST_DECODED_SIZE_SAMPLES: usize = 2;
        // packet_1 has no access to duration, and relies last decoded duration
        // as input.
        assert_eq!(
            LAST_DECODED_SIZE_SAMPLES,
            buffer.get_span_samples(
                LAST_DECODED_SIZE_SAMPLES,
                SAMPLE_RATE_HZ,
                COUNT_WAITING_TIME
            )
        );

        assert_eq!(
            PacketBuffer::K_OK,
            buffer.insert_packet(packet_2, FRAME_SIZE_SAMPLES, SAMPLE_RATE_HZ, 60, &mut stats)
        );

        assert_eq!(
            FRAME_SIZE_SAMPLES * 2,
            buffer.get_span_samples(0, SAMPLE_RATE_HZ, COUNT_WAITING_TIME)
        );

        // packet_2 has access to duration, and ignores last decoded duration
        // as input.
        assert_eq!(
            FRAME_SIZE_SAMPLES * 2,
            buffer.get_span_samples(
                LAST_DECODED_SIZE_SAMPLES,
                SAMPLE_RATE_HZ,
                COUNT_WAITING_TIME
            )
        );
    }

    #[test]
    fn get_span_samples_count_waiting_time() {
        const FRAME_SIZE_SAMPLES: usize = 10;
        const PAYLOAD_SIZE_BYTES: usize = 1; // Does not matter to this test;
        const START_TIME_STAMP: u32 = 0xFFFFFFFE; // Close to wrap around.
        const SAMPLE_RATE_HZ: usize = 48000;
        const COUNT_WAITING_TIME: bool = true;
        const LAST_DECODED_SIZE_SAMPLES: usize = 0;
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(3, tick_timer.clone(), None);
        let mut gen = PacketGenerator::new(0, START_TIME_STAMP, 0, FRAME_SIZE_SAMPLES as i32);

        let packet = gen.next_packet(PAYLOAD_SIZE_BYTES, None);

        assert_eq!(
            PacketBuffer::K_OK,
            buffer.insert_packet(packet, FRAME_SIZE_SAMPLES, SAMPLE_RATE_HZ, 60, &mut stats)
        );

        assert_eq!(
            0,
            buffer.get_span_samples(
                LAST_DECODED_SIZE_SAMPLES,
                SAMPLE_RATE_HZ,
                COUNT_WAITING_TIME
            )
        );

        tick_timer.increment();
        assert_eq!(
            480,
            buffer.get_span_samples(0, SAMPLE_RATE_HZ, COUNT_WAITING_TIME)
        );

        tick_timer.increment();
        assert_eq!(
            960,
            buffer.get_span_samples(0, SAMPLE_RATE_HZ, COUNT_WAITING_TIME)
        );
    }

    // Test a partial buffer flush.
    #[test]
    fn partial_flush() {
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(10, tick_timer, smart_flushing_test_config());
        let mut gen = PacketGenerator::new(0, 0, 0, 10);
        let payload_len = 10;

        // Insert 10 small packets; should be ok.
        for _ in 0..10 {
            assert_eq!(
                PacketBuffer::K_OK,
                buffer.insert_packet(
                    gen.next_packet(payload_len, None),
                    10,
                    1000,
                    100,
                    &mut stats
                )
            );
        }
        assert_eq!(10, buffer.num_packets_in_buffer());
        assert!(!buffer.empty());

        buffer.partial_flush(30, 1000, 10, &mut stats);
        assert_eq!(7, discarded(&stats));
        // There should still be some packets left in the buffer.
        assert_eq!(3, buffer.num_packets_in_buffer());
        assert!(!buffer.empty());
    }

    // Test to fill the buffer over the limits, and verify that the smart
    // flush functionality works as expected.
    #[test]
    fn smart_flush_overfill_buffer() {
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(10, tick_timer, smart_flushing_test_config());
        let mut gen = PacketGenerator::new(0, 0, 0, 10);

        // Insert 10 small packets; should be ok.
        let payload_len = 10;
        for _ in 0..10 {
            assert_eq!(
                PacketBuffer::K_OK,
                buffer.insert_packet(
                    gen.next_packet(payload_len, None),
                    10,
                    1000,
                    100,
                    &mut stats
                )
            );
        }
        assert_eq!(10, buffer.num_packets_in_buffer());
        let mut next_ts = 0;
        assert_eq!(PacketBuffer::K_OK, buffer.next_timestamp(&mut next_ts));
        assert_eq!(0, next_ts); // Expect first inserted packet to be first in line.

        let packet = gen.next_packet(payload_len, None);
        // Insert 11th packet; should cause a partial flush and insert the
        // packet after flushing.
        assert_eq!(
            PacketBuffer::K_PARTIAL_FLUSH,
            buffer.insert_packet(packet.clone_packet(), 10, 1000, 40, &mut stats)
        );
        assert_eq!(6, discarded(&stats));
        assert_eq!(5, buffer.num_packets_in_buffer());
    }

    fn test_is_obsolete_timestamp(limit_timestamp: u32) {
        // Check with zero horizon, which implies that the horizon is at 2^31,
        // i.e., half the timestamp range.
        const ZERO_HORIZON: u32 = 0;
        const TWO_POW_31_MINUS_1: u32 = 0x7FFFFFFF;
        let obsolete = PacketBuffer::is_obsolete_timestamp;
        // Timestamp on the limit is not old.
        assert!(!obsolete(limit_timestamp, limit_timestamp, ZERO_HORIZON));
        // 1 sample behind is old.
        assert!(obsolete(
            limit_timestamp.wrapping_sub(1),
            limit_timestamp,
            ZERO_HORIZON
        ));
        // 2^31 - 1 samples behind is old.
        assert!(obsolete(
            limit_timestamp.wrapping_sub(TWO_POW_31_MINUS_1),
            limit_timestamp,
            ZERO_HORIZON
        ));
        // 1 sample ahead is not old.
        assert!(!obsolete(
            limit_timestamp.wrapping_add(1),
            limit_timestamp,
            ZERO_HORIZON
        ));
        // If |t1-t2|=2^31 and t1>t2, t2 is older than t1 but not the opposite.
        let other_timestamp = limit_timestamp.wrapping_add(1 << 31);
        let lowest_timestamp = std::cmp::min(limit_timestamp, other_timestamp);
        let highest_timestamp = std::cmp::max(limit_timestamp, other_timestamp);
        assert!(obsolete(lowest_timestamp, highest_timestamp, ZERO_HORIZON));
        assert!(!obsolete(highest_timestamp, lowest_timestamp, ZERO_HORIZON));

        // Fixed horizon at 10 samples.
        const HORIZON: u32 = 10;
        // Timestamp on the limit is not old.
        assert!(!obsolete(limit_timestamp, limit_timestamp, HORIZON));
        // 1 sample behind is old.
        assert!(obsolete(
            limit_timestamp.wrapping_sub(1),
            limit_timestamp,
            HORIZON
        ));
        // 9 samples behind is old.
        assert!(obsolete(
            limit_timestamp.wrapping_sub(9),
            limit_timestamp,
            HORIZON
        ));
        // 10 samples behind is not old.
        assert!(!obsolete(
            limit_timestamp.wrapping_sub(10),
            limit_timestamp,
            HORIZON
        ));
        // 2^31 - 1 samples behind is not old.
        assert!(!obsolete(
            limit_timestamp.wrapping_sub(TWO_POW_31_MINUS_1),
            limit_timestamp,
            HORIZON
        ));
        // 1 sample ahead is not old.
        assert!(!obsolete(
            limit_timestamp.wrapping_add(1),
            limit_timestamp,
            HORIZON
        ));
        // 2^31 samples ahead is not old.
        assert!(!obsolete(
            limit_timestamp.wrapping_add(1 << 31),
            limit_timestamp,
            HORIZON
        ));
    }

    // Test the IsObsoleteTimestamp method with different limit timestamps.
    #[test]
    fn is_obsolete_timestamp() {
        test_is_obsolete_timestamp(0);
        test_is_obsolete_timestamp(1);
        test_is_obsolete_timestamp(0xFFFFFFFF); // -1 in uint32_t.
        test_is_obsolete_timestamp(0x80000000); // 2^31.
        test_is_obsolete_timestamp(0x80000001); // 2^31 + 1.
        test_is_obsolete_timestamp(0x7FFFFFFF); // 2^31 - 1.
    }

    // The tests below are not in C++.

    struct CngPayloadType(u8);

    impl ComfortNoiseLookup for CngPayloadType {
        fn is_comfort_noise(&self, rtp_payload_type: u8) -> bool {
            rtp_payload_type == self.0
        }
    }

    #[test]
    fn contains_dtx_or_cng_packet() {
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(10, tick_timer, None);
        let mut gen = PacketGenerator::new(0, 0, 0, 10);
        let decoder_database = CngPayloadType(13);
        buffer.insert_packet(gen.next_packet(10, None), 10, 8000, 60, &mut stats);
        assert!(!buffer.contains_dtx_or_cng_packet(&decoder_database));
        let dtx = Box::new(FakeEncodedAudioFrame {
            duration: 10,
            is_dtx: true,
        });
        buffer.insert_packet(gen.next_packet(1, Some(dtx)), 10, 8000, 60, &mut stats);
        assert!(buffer.contains_dtx_or_cng_packet(&decoder_database));
        buffer.discard_packets_with_payload_type(0, &mut stats);
        assert!(buffer.empty());
        assert_eq!(2, discarded(&stats));
        gen.reset(5, 100, 13, 10);
        buffer.insert_packet(gen.next_packet(1, None), 10, 8000, 60, &mut stats);
        assert!(buffer.contains_dtx_or_cng_packet(&decoder_database));
    }

    #[test]
    fn num_samples_in_buffer_and_next_higher_timestamp() {
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(10, tick_timer, None);
        let mut gen = PacketGenerator::new(0, 100, 0, 20);
        let frame = |duration| {
            Some(Box::new(FakeEncodedAudioFrame {
                duration,
                is_dtx: false,
            }) as Box<dyn EncodedAudioFrame>)
        };
        // Raw payload: counts last_decoded_length (7).
        buffer.insert_packet(gen.next_packet(1, None), 10, 8000, 60, &mut stats);
        // Frame of 20: counts 20 and becomes the running duration.
        buffer.insert_packet(gen.next_packet(1, frame(20)), 10, 8000, 60, &mut stats);
        // Zero-duration frame: counts the running duration (20).
        buffer.insert_packet(gen.next_packet(1, frame(0)), 10, 8000, 60, &mut stats);
        // Secondary frame: skipped entirely.
        let mut fec = gen.next_packet(1, frame(30));
        fec.priority = Priority::new(1, 0);
        buffer.insert_packet(fec, 10, 8000, 60, &mut stats);
        assert_eq!(7 + 20 + 20, buffer.num_samples_in_buffer(7));

        let mut next = 0;
        assert_eq!(
            PacketBuffer::K_OK,
            buffer.next_higher_timestamp(130, &mut next)
        );
        assert_eq!(140, next);
        assert_eq!(
            PacketBuffer::K_NOT_FOUND,
            buffer.next_higher_timestamp(181, &mut next)
        );
    }

    #[test]
    fn set_max_number_of_packets_discards_oldest() {
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(10, tick_timer, None);
        let mut gen = PacketGenerator::new(0, 0, 0, 10);
        for _ in 0..5 {
            buffer.insert_packet(gen.next_packet(10, None), 10, 8000, 60, &mut stats);
        }
        buffer.set_max_number_of_packets(2, &mut stats);
        assert_eq!(2, buffer.num_packets_in_buffer());
        assert_eq!(3, discarded(&stats));
        let mut next = 0;
        buffer.next_timestamp(&mut next);
        assert_eq!(30, next);
    }

    #[test]
    fn get_span_samples_dtx_uses_waiting_time() {
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(10, tick_timer.clone(), None);
        let mut gen = PacketGenerator::new(0, 0, 0, 10);
        let dtx = Box::new(FakeEncodedAudioFrame {
            duration: 10,
            is_dtx: true,
        });
        buffer.insert_packet(gen.next_packet(1, Some(dtx)), 10, 8000, 60, &mut stats);
        assert_eq!(10, buffer.get_span_samples(0, 8000, false));
        tick_timer.increment_by(3);
        // 30 ms at 8 kHz is 240 samples, more than the 10-sample duration.
        assert_eq!(240, buffer.get_span_samples(0, 8000, false));
    }

    #[test]
    fn lower_priority_duplicate_is_replaced() {
        let tick_timer = TickTimer::new();
        let mut stats = StatisticsCalculator::new();
        let mut buffer = PacketBuffer::new(10, tick_timer, None);
        let fec = Packet {
            timestamp: 100,
            sequence_number: 1,
            payload: vec![1],
            priority: Priority::new(1, 0),
            ..Packet::default()
        };
        assert_eq!(
            PacketBuffer::K_OK,
            buffer.insert_packet(fec.clone_packet(), 10, 8000, 60, &mut stats)
        );
        // Same timestamp and sequence number, as Opus FEC parsing produces.
        let primary = Packet {
            timestamp: 100,
            sequence_number: 1,
            payload: vec![1],
            ..Packet::default()
        };
        assert_eq!(
            PacketBuffer::K_OK,
            buffer.insert_packet(primary, 10, 8000, 60, &mut stats)
        );
        assert_eq!(1, buffer.num_packets_in_buffer());
        assert_eq!(1, secondary_discarded(&stats));
        assert_eq!(
            Priority::new(0, 0),
            buffer.peek_next_packet().expect("packet").priority
        );
        // The FEC copy arriving again is dropped.
        buffer.insert_packet(fec, 10, 8000, 60, &mut stats);
        assert_eq!(1, buffer.num_packets_in_buffer());
        assert_eq!(2, secondary_discarded(&stats));
    }
}

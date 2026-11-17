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

use std::collections::VecDeque;
use web_time::{Duration, Instant};

use crate::statistics::StatisticsCalculator;
use crate::{AudioPacket, Result};

/// Configuration for smart flushing behavior
#[derive(Debug, Clone)]
pub struct SmartFlushConfig {
    /// Minimum target level for flushing threshold calculation
    pub target_level_threshold_ms: u32,
    /// Multiplier for target level to trigger smart flush
    pub target_level_multiplier: u32,
}

impl Default for SmartFlushConfig {
    fn default() -> Self {
        Self {
            target_level_threshold_ms: 500,
            target_level_multiplier: 3,
        }
    }
}

/// Return codes for buffer operations
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BufferReturnCode {
    Ok,
    Flushed,
    PartialFlush,
    NotFound,
    BufferEmpty,
    InvalidPacket,
}

/// Packet buffer for storing and managing audio packets
#[derive(Debug)]
pub struct PacketBuffer {
    /// Maximum number of packets the buffer can hold
    max_packets: usize,
    /// Buffer storing the packets
    buffer: VecDeque<AudioPacket>,
    /// Smart flushing configuration
    smart_flush_config: SmartFlushConfig,
    /// Maximum age for packets before they're considered stale
    max_packet_age: Duration,
    /// Sum of `expected_samples()` over `buffer`, kept as packets come and
    /// go so `num_samples_in_buffer` does not walk the buffer.
    total_samples: usize,
}

impl PacketBuffer {
    /// The buffered packets, oldest timestamp first.
    pub fn iter(&self) -> impl Iterator<Item = &AudioPacket> {
        self.buffer.iter()
    }

    /// Create a new packet buffer
    pub fn new(max_packets: usize) -> Self {
        Self {
            max_packets,
            buffer: VecDeque::with_capacity(max_packets),
            smart_flush_config: SmartFlushConfig::default(),
            max_packet_age: Duration::from_secs(2),
            total_samples: 0,
        }
    }

    /// Create a new packet buffer with custom configuration
    pub fn with_config(max_packets: usize, smart_flush_config: SmartFlushConfig) -> Self {
        Self {
            max_packets,
            buffer: VecDeque::with_capacity(max_packets),
            smart_flush_config,
            max_packet_age: Duration::from_secs(2),
            total_samples: 0,
        }
    }

    /// Set the maximum age for packets. Kept for API compatibility: the
    /// buffer no longer discards by wall-clock age (libwebrtc's packet
    /// buffer has no such rule; obsolete packets go by RTP timestamp).
    pub fn set_max_packet_age(&mut self, max_age: Duration) {
        self.max_packet_age = max_age;
    }

    /// The value last given to [`Self::set_max_packet_age`].
    pub fn max_packet_age(&self) -> Duration {
        self.max_packet_age
    }

    /// Check if the buffer is empty
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Get the number of packets in the buffer
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Get the current buffer capacity utilization as a percentage
    pub fn utilization(&self) -> f32 {
        (self.buffer.len() as f32 / self.max_packets as f32) * 100.0
    }

    /// Flush the entire buffer
    pub fn flush(&mut self, stats: &mut StatisticsCalculator) {
        let flushed_count = self.buffer.len();
        self.buffer.clear();
        self.total_samples = 0;

        if flushed_count > 0 {
            stats.buffer_flush();
            log::debug!("Flushed {flushed_count} packets from buffer");
        }
    }

    /// Partial flush (packet_buffer.cc `PacketBuffer::PartialFlush`): drop
    /// the oldest packets until the span is at most the target level (but
    /// never below the smart-flush threshold) and at most half the capacity
    /// is used.
    pub fn partial_flush(
        &mut self,
        target_level_ms: u32,
        sample_rate: u32,
        stats: &mut StatisticsCalculator,
        now: Instant,
    ) -> Result<BufferReturnCode> {
        let Some(back) = self.buffer.back() else {
            return Ok(BufferReturnCode::BufferEmpty);
        };
        let last_decoded_length = self.last_duration_samples(sample_rate);
        let target_level_samples = (u64::from(target_level_ms) * u64::from(sample_rate) / 1000)
            .min((self.max_packets * last_decoded_length / 2) as u64)
            .max(
                u64::from(self.smart_flush_config.target_level_threshold_ms)
                    * u64::from(sample_rate)
                    / 1000,
            );
        // The last packet stays while the front is dropped, so the span is
        // its timestamp minus the front's plus a fixed tail.
        let back_timestamp = back.header.timestamp;
        let tail = Self::span_tail(back, last_decoded_length, sample_rate, false, now);
        let mut removed = 0;
        while let Some(front) = self.buffer.front() {
            let span = back_timestamp.wrapping_sub(front.header.timestamp) as u64 + tail as u64;
            if span <= target_level_samples && self.buffer.len() <= self.max_packets / 2 {
                break;
            }
            self.pop_front();
            stats.packet_discarded(false);
            removed += 1;
        }
        if removed > 0 {
            log::debug!("Partial flush: removed {removed} packets");
            return Ok(BufferReturnCode::PartialFlush);
        }
        Ok(BufferReturnCode::Ok)
    }

    /// Insert a packet (packet_buffer.cc `PacketBuffer::InsertPacket`).
    ///
    /// Packets are ordered by RTP timestamp with the sequence number as the
    /// tie-break, both wrap-aware (packet.h `Packet::operator<`). A packet
    /// whose timestamp equals one already buffered is a duplicate: the one
    /// with the earlier sequence number is kept.
    pub fn insert_packet(
        &mut self,
        packet: AudioPacket,
        stats: &mut StatisticsCalculator,
        target_level_ms: u32,
    ) -> Result<BufferReturnCode> {
        if packet.payload.is_empty() {
            return Ok(BufferReturnCode::InvalidPacket);
        }
        let sample_rate = packet.sample_rate;
        // The packet's arrival time is "now" for this insert.
        let now = packet.arrival_time;
        let mut return_code = BufferReturnCode::Ok;

        // Smart flush when the span exceeds a multiple of the target level.
        let span_threshold = u64::from(self.smart_flush_config.target_level_multiplier)
            * u64::from(
                self.smart_flush_config
                    .target_level_threshold_ms
                    .max(target_level_ms),
            )
            * u64::from(sample_rate)
            / 1000;
        let last_duration = self.last_duration_samples(sample_rate);
        let smart_flush = !self.buffer.is_empty()
            && self.get_span_samples(last_duration, sample_rate, false, now) as u64
                >= span_threshold;
        if self.buffer.len() >= self.max_packets || smart_flush {
            let before = self.buffer.len();
            self.partial_flush(target_level_ms, sample_rate, stats, now)?;
            if self.buffer.len() >= self.max_packets {
                self.flush(stats);
                return_code = BufferReturnCode::Flushed;
            } else if self.buffer.len() < before {
                return_code = BufferReturnCode::PartialFlush;
            }
        }

        // Search from the back for the first packet the new one is not
        // earlier than (packet_buffer.cc NewTimestampIsLarger).
        let mut pos = self.buffer.len();
        while pos > 0 && packet_is_earlier(&packet, &self.buffer[pos - 1]) {
            pos -= 1;
        }
        if pos > 0 && self.buffer[pos - 1].header.timestamp == packet.header.timestamp {
            log::debug!(
                "Discarding duplicate timestamp: seq={}, ts={}",
                packet.header.sequence_number,
                packet.header.timestamp
            );
            stats.packet_discarded(false);
            return Ok(return_code);
        }
        if pos < self.buffer.len() && self.buffer[pos].header.timestamp == packet.header.timestamp {
            if let Some(old) = self.buffer.remove(pos) {
                self.total_samples -= old.expected_samples();
            }
            stats.packet_discarded(false);
        }
        if pos < self.buffer.len() {
            stats.packet_reordered((self.buffer.len() - pos) as u16);
        } else {
            stats.packet_in_order();
        }
        self.total_samples += packet.expected_samples();
        self.buffer.insert(pos, packet);
        stats.packet_arrived(0);
        Ok(return_code)
    }

    /// Get the next packet timestamp without removing it
    pub fn peek_next_timestamp(&self) -> Option<u32> {
        self.buffer.front().map(|packet| packet.header.timestamp)
    }

    /// The next packet, without removing it (`PeekNextPacket`).
    pub fn peek_next_packet(&self) -> Option<&AudioPacket> {
        self.buffer.front()
    }

    /// Get the next packet with timestamp >= the given timestamp
    pub fn peek_next_packet_from_timestamp(&self, timestamp: u32) -> Option<&AudioPacket> {
        self.buffer
            .iter()
            .find(|packet| !is_newer_timestamp(timestamp, packet.header.timestamp))
    }

    /// Get the oldest packet from the buffer
    pub fn get_next_packet(&mut self) -> Option<AudioPacket> {
        self.pop_front()
    }

    fn pop_front(&mut self) -> Option<AudioPacket> {
        let packet = self.buffer.pop_front()?;
        self.total_samples -= packet.expected_samples();
        Some(packet)
    }

    /// Discard the next packet without returning it
    pub fn discard_next_packet(
        &mut self,
        stats: &mut StatisticsCalculator,
    ) -> Result<BufferReturnCode> {
        if self.pop_front().is_some() {
            stats.packet_discarded(true);
            Ok(BufferReturnCode::Ok)
        } else {
            Ok(BufferReturnCode::BufferEmpty)
        }
    }

    /// packet_buffer.cc `DiscardOldPackets`: drop packets older than
    /// `timestamp_limit` but no more than `horizon_samples` older (0: any).
    pub fn discard_old_packets(
        &mut self,
        timestamp_limit: u32,
        horizon_samples: u32,
        stats: &mut StatisticsCalculator,
    ) {
        let total_samples = &mut self.total_samples;
        self.buffer.retain(|p| {
            let obsolete = timestamp_limit != p.header.timestamp
                && is_obsolete_timestamp(p.header.timestamp, timestamp_limit, horizon_samples);
            if obsolete {
                stats.packet_discarded(true);
                *total_samples -= p.expected_samples();
            }
            !obsolete
        });
    }

    /// Discard every packet older than the given timestamp
    /// (`DiscardAllOldPackets`).
    pub fn discard_old_packets_by_timestamp(
        &mut self,
        timestamp_limit: u32,
        stats: &mut StatisticsCalculator,
    ) {
        self.discard_old_packets(timestamp_limit, 0, stats);
    }

    /// Get the total duration of packets in the buffer (timestamp span)
    pub fn get_span_duration_ms(&self) -> u32 {
        let Some(front) = self.buffer.front() else {
            return 0;
        };
        let back = self.buffer.back().unwrap_or(front);
        let span = back.header.timestamp.wrapping_sub(front.header.timestamp);
        (u64::from(span) * 1000 / u64::from(front.sample_rate.max(1))) as u32
    }

    /// packet_buffer.cc `PacketBuffer::GetSpanSamples`: timestamp span of the
    /// buffer plus either the last packet's waiting time
    /// (`count_waiting_time`) or its duration (for a DTX packet, at least its
    /// waiting time). In samples per channel.
    pub fn get_span_samples(
        &self,
        last_decoded_length: usize,
        sample_rate: u32,
        count_waiting_time: bool,
        now: Instant,
    ) -> usize {
        let (Some(front), Some(back)) = (self.buffer.front(), self.buffer.back()) else {
            return 0;
        };
        back.header.timestamp.wrapping_sub(front.header.timestamp) as usize
            + Self::span_tail(
                back,
                last_decoded_length,
                sample_rate,
                count_waiting_time,
                now,
            )
    }

    /// What `GetSpanSamples` adds after the last packet's timestamp: its
    /// waiting time, or its duration (at least its waiting time if DTX).
    /// The waiting time is only computed when it is used.
    fn span_tail(
        back: &AudioPacket,
        last_decoded_length: usize,
        sample_rate: u32,
        count_waiting_time: bool,
        now: Instant,
    ) -> usize {
        let waiting = || {
            now.saturating_duration_since(back.arrival_time).as_millis() as usize
                * (sample_rate / 1000) as usize
        };
        if count_waiting_time {
            return waiting();
        }
        let mut duration = back.duration_samples();
        if duration == 0 {
            duration = last_decoded_length;
        }
        if back.is_dtx() {
            duration = duration.max(waiting());
        }
        duration
    }

    /// packet_buffer.cc `ContainsDtxOrCngPacket`.
    pub fn contains_dtx_packet(&self) -> bool {
        self.buffer.iter().any(AudioPacket::is_dtx)
    }

    /// Get the total content duration of packets in the buffer (sum of packet durations)
    pub fn get_total_content_duration_ms(&self) -> u32 {
        self.buffer.iter().map(|packet| packet.duration_ms).sum()
    }

    /// Get the total number of samples in the buffer
    pub fn num_samples_in_buffer(&self) -> usize {
        debug_assert_eq!(
            self.total_samples,
            self.buffer
                .iter()
                .map(AudioPacket::expected_samples)
                .sum::<usize>()
        );
        self.total_samples
    }

    fn last_duration_samples(&self, sample_rate: u32) -> usize {
        self.buffer.back().map_or(
            (sample_rate as usize / 1000) * 20,
            AudioPacket::duration_samples,
        )
    }
}

/// `a` is newer than `b`, wrap-aware (module_common_types_public.h
/// `IsNewerTimestamp`).
pub fn is_newer_timestamp(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000_0000
}

/// packet_buffer.h `PacketBuffer::IsObsoleteTimestamp`.
pub fn is_obsolete_timestamp(timestamp: u32, timestamp_limit: u32, horizon_samples: u32) -> bool {
    is_newer_timestamp(timestamp_limit, timestamp)
        && (horizon_samples == 0
            || is_newer_timestamp(timestamp, timestamp_limit.wrapping_sub(horizon_samples)))
}

/// packet.h `Packet::operator<`: by timestamp, then sequence number, both
/// wrap-aware.
fn packet_is_earlier(a: &AudioPacket, b: &AudioPacket) -> bool {
    if a.header.timestamp == b.header.timestamp {
        if a.header.sequence_number == b.header.sequence_number {
            return false;
        }
        return b
            .header
            .sequence_number
            .wrapping_sub(a.header.sequence_number)
            < 0xFFFF / 2;
    }
    b.header.timestamp.wrapping_sub(a.header.timestamp) < 0xFFFF_FFFF / 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{AudioPacket, RtpHeader};

    fn create_test_packet(seq: u16, ts: u32, duration_ms: u32) -> AudioPacket {
        let header = RtpHeader::new(seq, ts, 12345, 96, false);
        AudioPacket::new(header, vec![0; 160], 16000, 1, duration_ms)
    }

    #[test]
    fn test_buffer_creation() {
        let buffer = PacketBuffer::new(100);
        assert!(buffer.is_empty());
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.utilization(), 0.0);
    }

    #[test]
    fn test_packet_insertion_and_ordering() {
        let mut buffer = PacketBuffer::new(10);
        let mut stats = StatisticsCalculator::new();

        // Insert packets out of order
        let packet3 = create_test_packet(3, 3000, 20);
        let packet1 = create_test_packet(1, 1000, 20);
        let packet2 = create_test_packet(2, 2000, 20);

        buffer.insert_packet(packet3, &mut stats, 100).unwrap();
        buffer.insert_packet(packet1, &mut stats, 100).unwrap();
        buffer.insert_packet(packet2, &mut stats, 100).unwrap();

        assert_eq!(buffer.len(), 3);

        // Check ordering
        assert_eq!(buffer.peek_next_timestamp(), Some(1000));

        let p1 = buffer.get_next_packet().unwrap();
        assert_eq!(p1.header.timestamp, 1000);

        let p2 = buffer.get_next_packet().unwrap();
        assert_eq!(p2.header.timestamp, 2000);

        let p3 = buffer.get_next_packet().unwrap();
        assert_eq!(p3.header.timestamp, 3000);
    }

    #[test]
    fn test_duplicate_detection() {
        let mut buffer = PacketBuffer::new(10);
        let mut stats = StatisticsCalculator::new();

        let packet1 = create_test_packet(1, 1000, 20);
        let packet1_dup = create_test_packet(1, 1000, 20);

        buffer.insert_packet(packet1, &mut stats, 100).unwrap();
        buffer.insert_packet(packet1_dup, &mut stats, 100).unwrap();

        // Should only have one packet
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn test_buffer_overflow() {
        let mut buffer = PacketBuffer::new(2);
        let mut stats = StatisticsCalculator::new();

        // Fill buffer
        buffer
            .insert_packet(create_test_packet(1, 1000, 20), &mut stats, 100)
            .unwrap();
        buffer
            .insert_packet(create_test_packet(2, 2000, 20), &mut stats, 100)
            .unwrap();

        // This should trigger overflow handling
        buffer
            .insert_packet(create_test_packet(3, 3000, 20), &mut stats, 100)
            .unwrap();

        // Buffer should not exceed max capacity
        assert!(buffer.len() <= 2);
    }

    #[test]
    fn same_timestamp_keeps_the_earlier_sequence_number() {
        let mut buffer = PacketBuffer::new(10);
        let mut stats = StatisticsCalculator::new();
        buffer
            .insert_packet(create_test_packet(5, 1000, 20), &mut stats, 100)
            .unwrap();
        buffer
            .insert_packet(create_test_packet(6, 1000, 20), &mut stats, 100)
            .unwrap();
        buffer
            .insert_packet(create_test_packet(4, 1000, 20), &mut stats, 100)
            .unwrap();
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.get_next_packet().unwrap().header.sequence_number, 4);
    }

    #[test]
    fn ordering_is_wrap_aware() {
        let mut buffer = PacketBuffer::new(10);
        let mut stats = StatisticsCalculator::new();
        buffer
            .insert_packet(create_test_packet(1, 320, 20), &mut stats, 100)
            .unwrap();
        buffer
            .insert_packet(create_test_packet(0, u32::MAX - 319, 20), &mut stats, 100)
            .unwrap();
        assert_eq!(buffer.peek_next_timestamp(), Some(u32::MAX - 319));
        assert_eq!(
            buffer.get_span_samples(320, 16000, false, crate::clock::now()),
            640 + 320
        );
    }

    #[test]
    fn discard_old_packets_respects_the_horizon() {
        let mut buffer = PacketBuffer::new(10);
        let mut stats = StatisticsCalculator::new();
        for (seq, ts) in [(1u16, 100u32), (2, 10_000), (3, 20_000)] {
            buffer
                .insert_packet(create_test_packet(seq, ts, 20), &mut stats, 100)
                .unwrap();
        }
        // Limit 20_000, horizon 15_000: 10_000 is obsolete, 100 is beyond
        // the horizon and kept.
        buffer.discard_old_packets(20_000, 15_000, &mut stats);
        assert_eq!(buffer.len(), 2);
    }

    #[test]
    fn test_span_duration_calculation() {
        let mut buffer = PacketBuffer::new(10);
        let mut stats = StatisticsCalculator::new();

        // Insert packets with 20ms duration each
        buffer
            .insert_packet(create_test_packet(1, 0, 20), &mut stats, 100)
            .unwrap();
        buffer
            .insert_packet(create_test_packet(2, 320, 20), &mut stats, 100)
            .unwrap(); // 20ms later at 16kHz
        buffer
            .insert_packet(create_test_packet(3, 640, 20), &mut stats, 100)
            .unwrap(); // 40ms from start

        // Span should be 40ms (640 samples at 16kHz = 40ms)
        assert_eq!(buffer.get_span_duration_ms(), 40);
    }
}

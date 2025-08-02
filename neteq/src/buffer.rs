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
use web_time::Duration;

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
}

impl PacketBuffer {
    /// Create a new packet buffer
    pub fn new(max_packets: usize) -> Self {
        Self {
            max_packets,
            buffer: VecDeque::with_capacity(max_packets),
            smart_flush_config: SmartFlushConfig::default(),
            max_packet_age: Duration::from_secs(2),
        }
    }

    /// Create a new packet buffer with custom configuration
    pub fn with_config(max_packets: usize, smart_flush_config: SmartFlushConfig) -> Self {
        Self {
            max_packets,
            buffer: VecDeque::with_capacity(max_packets),
            smart_flush_config,
            max_packet_age: Duration::from_secs(2),
        }
    }

    /// Set the maximum age for packets
    pub fn set_max_packet_age(&mut self, max_age: Duration) {
        self.max_packet_age = max_age;
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

        if flushed_count > 0 {
            stats.buffer_flush();
            log::debug!("Flushed {flushed_count} packets from buffer");
        }
    }

    /// Partial flush - remove older packets but keep some recent ones
    pub fn partial_flush(
        &mut self,
        target_level_ms: u32,
        _sample_rate: u32,
        stats: &mut StatisticsCalculator,
    ) -> Result<BufferReturnCode> {
        if self.buffer.is_empty() {
            return Ok(BufferReturnCode::BufferEmpty);
        }

        // Calculate target number of packets to keep
        let target_duration = Duration::from_millis(target_level_ms as u64);
        let mut current_duration = Duration::from_millis(0);
        let mut keep_count = 0;

        // Count from the newest packets backwards
        for packet in self.buffer.iter().rev() {
            current_duration += Duration::from_millis(packet.duration_ms as u64);
            keep_count += 1;

            if current_duration >= target_duration {
                break;
            }
        }

        // Remove older packets
        let remove_count = self.buffer.len().saturating_sub(keep_count);
        if remove_count > 0 {
            for _ in 0..remove_count {
                if let Some(packet) = self.buffer.pop_front() {
                    stats.packet_discarded(packet.is_older_than(self.max_packet_age));
                }
            }

            log::debug!("Partial flush: removed {remove_count} packets, kept {keep_count}");
            return Ok(BufferReturnCode::PartialFlush);
        }

        Ok(BufferReturnCode::Ok)
    }

    /// Insert a packet into the buffer with proper ordering
    pub fn insert_packet(
        &mut self,
        packet: AudioPacket,
        stats: &mut StatisticsCalculator,
        target_level_ms: u32,
    ) -> Result<BufferReturnCode> {
        // Check for stale packets and remove them
        self.discard_old_packets(stats);

        // Check if we need to trigger smart flushing
        if self.should_smart_flush(target_level_ms) {
            self.partial_flush(target_level_ms, packet.sample_rate, stats)?;
        }

        // Check if buffer is full
        if self.buffer.len() >= self.max_packets {
            // Try partial flush first
            self.partial_flush(target_level_ms, packet.sample_rate, stats)?;

            // If still full, flush completely
            if self.buffer.len() >= self.max_packets {
                self.flush(stats);
                log::warn!("Buffer overflow: performed full flush");
                stats.buffer_flush();
            }
        }

        // Find the correct position to insert the packet (ordered by timestamp)
        let insert_pos = self.find_insert_position(&packet);

        // Check for duplicate packets
        if self.is_duplicate(&packet, insert_pos) {
            log::debug!(
                "Discarding duplicate packet: seq={}, ts={}",
                packet.header.sequence_number,
                packet.header.timestamp
            );
            stats.packet_discarded(false);
            return Ok(BufferReturnCode::Ok);
        }

        // Detect reordering: if packet is not inserted at the end, it's reordered
        let is_reordered = insert_pos < self.buffer.len();

        if is_reordered {
            // Calculate reorder distance (how far out of order)
            let expected_pos = self.buffer.len();
            let distance = (expected_pos - insert_pos) as u16;
            stats.packet_reordered(distance);

            log::debug!(
                "Reordered packet detected: seq={}, ts={}, insert_pos={}, expected_pos={}, distance={}",
                packet.header.sequence_number,
                packet.header.timestamp,
                insert_pos,
                expected_pos,
                distance
            );
        } else {
            stats.packet_in_order();
        }

        // Insert the packet
        self.buffer.insert(insert_pos, packet);

        // Update statistics
        let arrival_delay = self.calculate_arrival_delay(insert_pos);
        stats.packet_arrived(arrival_delay);

        Ok(BufferReturnCode::Ok)
    }

    /// Get the next packet timestamp without removing it
    pub fn peek_next_timestamp(&self) -> Option<u32> {
        self.buffer.front().map(|packet| packet.header.timestamp)
    }

    /// Get the next packet with timestamp >= the given timestamp
    pub fn peek_next_packet_from_timestamp(&self, timestamp: u32) -> Option<&AudioPacket> {
        self.buffer
            .iter()
            .find(|packet| packet.header.timestamp >= timestamp)
    }

    /// Get the oldest packet from the buffer
    pub fn get_next_packet(&mut self) -> Option<AudioPacket> {
        self.buffer.pop_front()
    }

    /// Discard the next packet without returning it
    pub fn discard_next_packet(
        &mut self,
        stats: &mut StatisticsCalculator,
    ) -> Result<BufferReturnCode> {
        if let Some(packet) = self.buffer.pop_front() {
            stats.packet_discarded(packet.is_older_than(self.max_packet_age));
            Ok(BufferReturnCode::Ok)
        } else {
            Ok(BufferReturnCode::BufferEmpty)
        }
    }

    /// Discard packets older than the given timestamp
    pub fn discard_old_packets_by_timestamp(
        &mut self,
        timestamp_limit: u32,
        stats: &mut StatisticsCalculator,
    ) {
        let initial_len = self.buffer.len();

        self.buffer.retain(|packet| {
            let should_keep = packet.header.timestamp >= timestamp_limit;
            if !should_keep {
                stats.packet_discarded(true);
            }
            should_keep
        });

        let discarded = initial_len - self.buffer.len();
        if discarded > 0 {
            log::debug!("Discarded {discarded} old packets by timestamp");
        }
    }

    /// Get the total duration of packets in the buffer (timestamp span)
    pub fn get_span_duration_ms(&self) -> u32 {
        if self.buffer.is_empty() {
            return 0;
        }

        let oldest_ts = self.buffer.front().unwrap().header.timestamp;
        let newest_ts = self.buffer.back().unwrap().header.timestamp;

        // Handle timestamp wraparound
        let span_samples = if newest_ts >= oldest_ts {
            newest_ts - oldest_ts
        } else {
            // Wraparound case
            (u32::MAX - oldest_ts) + newest_ts + 1
        };

        // Convert samples to milliseconds (assuming first packet sample rate)
        let sample_rate = self.buffer.front().unwrap().sample_rate;
        (span_samples * 1000) / sample_rate
    }

    /// Get the total content duration of packets in the buffer (sum of packet durations)
    /// This is more reliable than span_duration when packets have close timestamps
    pub fn get_total_content_duration_ms(&self) -> u32 {
        self.buffer.iter().map(|packet| packet.duration_ms).sum()
    }

    /// Get the total number of samples in the buffer
    pub fn num_samples_in_buffer(&self) -> usize {
        self.buffer
            .iter()
            .map(|packet| packet.expected_samples())
            .sum()
    }

    fn find_insert_position(&self, packet: &AudioPacket) -> usize {
        // Binary search for the correct insertion position based on timestamp
        let mut low = 0;
        let mut high = self.buffer.len();

        while low < high {
            let mid = (low + high) / 2;
            if self.buffer[mid].header.timestamp <= packet.header.timestamp {
                low = mid + 1;
            } else {
                high = mid;
            }
        }

        low
    }

    fn is_duplicate(&self, packet: &AudioPacket, insert_pos: usize) -> bool {
        // Check adjacent packets for duplicates
        let check_positions = [
            insert_pos.saturating_sub(1),
            insert_pos,
            (insert_pos + 1).min(self.buffer.len().saturating_sub(1)),
        ];

        for &pos in &check_positions {
            if pos < self.buffer.len() {
                if let Some(existing) = self.buffer.get(pos) {
                    if existing.header.timestamp == packet.header.timestamp
                        && existing.header.sequence_number == packet.header.sequence_number
                        && existing.header.ssrc == packet.header.ssrc
                    {
                        return true;
                    }
                }
            }
        }

        false
    }

    fn calculate_arrival_delay(&self, insert_pos: usize) -> i32 {
        // Simple arrival delay calculation based on position in buffer
        // In a real implementation, this would be more sophisticated
        if insert_pos == 0 {
            0
        } else {
            (insert_pos as i32) * 10 // Rough estimate: 10ms per position
        }
    }

    fn should_smart_flush(&self, target_level_ms: u32) -> bool {
        if self.buffer.is_empty() {
            return false;
        }

        let current_span_ms = self.get_span_duration_ms();
        let flush_threshold = self
            .smart_flush_config
            .target_level_threshold_ms
            .max(target_level_ms)
            * self.smart_flush_config.target_level_multiplier;

        current_span_ms > flush_threshold
    }

    fn discard_old_packets(&mut self, stats: &mut StatisticsCalculator) {
        let initial_len = self.buffer.len();

        self.buffer.retain(|packet| {
            let should_keep = !packet.is_older_than(self.max_packet_age);
            if !should_keep {
                stats.packet_discarded(true);
            }
            should_keep
        });

        let discarded = initial_len - self.buffer.len();
        if discarded > 0 {
            log::debug!("Discarded {discarded} stale packets");
        }
    }
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

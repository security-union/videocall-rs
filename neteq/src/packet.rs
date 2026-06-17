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

use serde::{Deserialize, Serialize};
use web_time::{Duration, Instant};

/// RTP Header information
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtpHeader {
    /// RTP sequence number — intentionally `u16` to match the RFC 3550 wire format.
    ///
    /// Wrap-safety: comparisons are performed via [`RtpHeader::is_sequence_newer`], which
    /// uses a 0x8000 half-window (`wrapping_sub`) and is therefore correct across the
    /// natural 65535 → 0 rollover that occurs roughly every 21.8 minutes at 50 pps.
    ///
    /// Role in packet ordering and deduplication:
    /// - Ordering and jitter-buffer decisions are driven exclusively by
    ///   `header.timestamp` (the sample-domain RTP timestamp set per issue #624);
    ///   `sequence_number` is never used for ordering, flush triggers, or reject logic.
    /// - `sequence_number` participates only in (a) diagnostic logging and (b) the
    ///   three-field duplicate-suppression tuple `(timestamp, sequence_number, ssrc)`
    ///   in `buffer.rs::is_duplicate`. Because `timestamp` is part of that tuple, two
    ///   packets whose u16 seq wraps to the same value but carry different sample-domain
    ///   timestamps are correctly treated as distinct — they will NOT be deduped.
    ///
    /// See the regression tests `test_seq_wrap_no_buffer_flush` and
    /// `test_seq_collision_at_wrap_not_deduped` in `neteq.rs` for live proof.
    pub sequence_number: u16,
    /// RTP timestamp
    pub timestamp: u32,
    /// Synchronization source identifier
    pub ssrc: u32,
    /// Payload type
    pub payload_type: u8,
    /// Marker bit
    pub marker: bool,
}

impl RtpHeader {
    /// Create a new RTP header
    pub fn new(
        sequence_number: u16,
        timestamp: u32,
        ssrc: u32,
        payload_type: u8,
        marker: bool,
    ) -> Self {
        Self {
            sequence_number,
            timestamp,
            ssrc,
            payload_type,
            marker,
        }
    }

    /// Check if this sequence number is newer than another
    pub fn is_sequence_newer(&self, other_seq: u16) -> bool {
        // Handle sequence number wrap-around
        let diff = self.sequence_number.wrapping_sub(other_seq);
        diff < 0x8000
    }

    /// Check if this timestamp is newer than another
    pub fn is_timestamp_newer(&self, other_timestamp: u32) -> bool {
        // Handle timestamp wrap-around
        let diff = self.timestamp.wrapping_sub(other_timestamp);
        diff < 0x80000000
    }
}

/// Audio packet containing RTP header and audio data
#[derive(Debug, Clone)]
pub struct AudioPacket {
    /// RTP header information
    pub header: RtpHeader,
    /// Audio payload data
    pub payload: Vec<u8>,
    /// Time when packet was received
    pub arrival_time: Instant,
    /// Sample rate of the audio data
    pub sample_rate: u32,
    /// Number of audio channels
    pub channels: u8,
    /// Duration of audio in this packet
    pub duration_ms: u32,
}

impl AudioPacket {
    /// Create a new audio packet
    pub fn new(
        header: RtpHeader,
        payload: Vec<u8>,
        sample_rate: u32,
        channels: u8,
        duration_ms: u32,
    ) -> Self {
        Self {
            header,
            payload,
            arrival_time: Instant::now(),
            sample_rate,
            channels,
            duration_ms,
        }
    }

    /// Get the size of the payload in bytes
    pub fn payload_size(&self) -> usize {
        self.payload.len()
    }

    /// Get the age of this packet since arrival
    pub fn age(&self) -> Duration {
        self.arrival_time.elapsed()
    }

    /// Check if this packet is older than the given duration
    pub fn is_older_than(&self, max_age: Duration) -> bool {
        self.age() > max_age
    }

    /// Calculate expected number of samples in this packet
    pub fn expected_samples(&self) -> usize {
        ((self.sample_rate as u64 * self.duration_ms as u64) / 1000) as usize
            * self.channels as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_rtp_header_sequence_comparison() {
        let header1 = RtpHeader::new(100, 1000, 12345, 96, false);
        let header2 = RtpHeader::new(101, 1000, 12345, 96, false);

        assert!(header2.is_sequence_newer(header1.sequence_number));
        assert!(!header1.is_sequence_newer(header2.sequence_number));
    }

    #[test]
    fn test_rtp_header_sequence_wraparound() {
        let header1 = RtpHeader::new(65535, 1000, 12345, 96, false);
        let header2 = RtpHeader::new(0, 1000, 12345, 96, false);

        assert!(header2.is_sequence_newer(header1.sequence_number));
    }

    #[test]
    fn test_audio_packet_age() {
        let header = RtpHeader::new(100, 1000, 12345, 96, false);
        let packet = AudioPacket::new(header, vec![0; 160], 16000, 1, 20);

        thread::sleep(Duration::from_millis(10));
        assert!(packet.age() >= Duration::from_millis(10));
    }

    #[test]
    fn test_expected_samples() {
        let header = RtpHeader::new(100, 1000, 12345, 96, false);
        let packet = AudioPacket::new(header, vec![0; 320], 16000, 1, 20);

        // 16000 Hz * 20ms / 1000ms * 1 channel = 320 samples
        assert_eq!(packet.expected_samples(), 320);
    }
}

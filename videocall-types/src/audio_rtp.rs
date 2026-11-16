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

//! RTP-style media timestamps for audio frames (`AudioMetadata.rtp_timestamp`).
//!
//! Units are samples at the codec clock rate, wrapping modulo 2^32. Opus always
//! uses a 48 kHz RTP clock regardless of the encoder's internal rate
//! (RFC 7587 section 4.1), so a 20 ms frame advances the timestamp by 960.
//!
//! The sender side mirrors libwebrtc's `ChannelSend::ProcessAndEncodeAudio`:
//! the counter advances by exactly one frame per encoded frame, and on the
//! first frame after a send pause it additionally jumps forward by the
//! capture-time gap truncated to whole frames, so the receiver can see that
//! audio was skipped.

/// RTP clock rate for Opus (RFC 7587): always 48 kHz.
pub const OPUS_RTP_CLOCK_RATE: u32 = 48_000;

/// Timestamp increment of one 20 ms Opus frame at the 48 kHz RTP clock.
pub const OPUS_RTP_SAMPLES_PER_20MS_FRAME: u32 = 960;

/// Timestamp to hand NetEq for a packet from a sender that predates
/// `rtp_timestamp`.
///
/// Legacy senders emit exactly one 20 ms frame per sequence number, so
/// `sequence * 960` (mod 2^32) is the exact media timestamp of each frame
/// relative to the start of that sender's stream.
pub fn legacy_rtp_timestamp(sequence: u64) -> u32 {
    // Truncating first is exact: (a mod 2^32) * b == a * b (mod 2^32).
    (sequence as u32).wrapping_mul(OPUS_RTP_SAMPLES_PER_20MS_FRAME)
}

/// Result of stamping one encoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StampedFrame {
    /// RTP timestamp of this frame.
    pub rtp_timestamp: u32,
    /// True when this is the first frame after a send pause that followed an
    /// earlier frame. The previous frame is not contiguous with this one, so
    /// it must not be attached as RED redundancy.
    pub after_gap: bool,
}

/// Per-sender audio RTP timestamp counter.
///
/// One instance lives for the whole sender session (across mute/unmute and
/// device switches) so the timestamp never goes backward. Each call is O(1).
#[derive(Debug, Clone)]
pub struct AudioRtpClock {
    clock_rate: u32,
    samples_per_frame: u32,
    /// Timestamp the next frame gets if no pause intervenes.
    next_timestamp: u32,
    /// Capture time (ms, monotonic) of the last stamped frame.
    last_capture_ms: Option<f64>,
    /// Set by [`AudioRtpClock::pause`]; consumed by the next frame.
    resume_pending: bool,
}

impl AudioRtpClock {
    /// A clock for `samples_per_frame`-sample frames at `clock_rate` Hz,
    /// starting at timestamp 0.
    pub fn new(clock_rate: u32, samples_per_frame: u32) -> Self {
        Self {
            clock_rate,
            samples_per_frame,
            next_timestamp: 0,
            last_capture_ms: None,
            resume_pending: false,
        }
    }

    /// A clock for 20 ms Opus frames at the 48 kHz RTP clock.
    pub fn opus_20ms() -> Self {
        Self::new(OPUS_RTP_CLOCK_RATE, OPUS_RTP_SAMPLES_PER_20MS_FRAME)
    }

    /// Start the counter at `timestamp` instead of 0.
    pub fn with_initial_timestamp(mut self, timestamp: u32) -> Self {
        self.next_timestamp = timestamp;
        self
    }

    /// Record that sending stopped (mute, encoder torn down, device switch).
    /// The next frame will jump forward by the elapsed capture time.
    pub fn pause(&mut self) {
        self.resume_pending = true;
    }

    /// Stamp the next encoded frame, captured at `capture_ms` on a monotonic
    /// millisecond clock.
    pub fn stamp(&mut self, capture_ms: f64) -> StampedFrame {
        let mut after_gap = false;
        if std::mem::take(&mut self.resume_pending) {
            if let Some(last_ms) = self.last_capture_ms {
                after_gap = true;
                // Same arithmetic as libwebrtc: whole frames of capture time
                // since the last frame, minus the one frame `next_timestamp`
                // already advanced by. Clamped at 0 so a short pause or a
                // clock that went backward never moves the timestamp back.
                let frame_ms =
                    f64::from(self.samples_per_frame) * 1_000.0 / f64::from(self.clock_rate);
                let gap_frames = ((capture_ms - last_ms) / frame_ms).floor() - 1.0;
                if gap_frames > 0.0 {
                    // Only the result modulo 2^32 matters, so truncating the
                    // frame count to u32 before the wrapping multiply is exact.
                    let skip = (gap_frames as u64 as u32).wrapping_mul(self.samples_per_frame);
                    self.next_timestamp = self.next_timestamp.wrapping_add(skip);
                }
            }
        }
        let rtp_timestamp = self.next_timestamp;
        self.next_timestamp = self.next_timestamp.wrapping_add(self.samples_per_frame);
        self.last_capture_ms = Some(capture_ms);
        StampedFrame {
            rtp_timestamp,
            after_gap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: u32 = OPUS_RTP_SAMPLES_PER_20MS_FRAME;

    #[test]
    fn consecutive_frames_advance_by_exactly_960() {
        let mut clock = AudioRtpClock::opus_20ms();
        let mut capture_ms = 1_000.0;
        let mut previous = clock.stamp(capture_ms);
        assert_eq!(previous.rtp_timestamp, 0);
        assert!(!previous.after_gap);
        for _ in 0..100 {
            // Delivery jitter must not leak into the timestamp while running.
            capture_ms += 17.0;
            let frame = clock.stamp(capture_ms);
            assert_eq!(frame.rtp_timestamp, previous.rtp_timestamp + FRAME);
            assert!(!frame.after_gap);
            previous = frame;
        }
    }

    #[test]
    fn pause_jumps_forward_by_gap_in_whole_frames() {
        let mut clock = AudioRtpClock::opus_20ms();
        assert_eq!(clock.stamp(0.0).rtp_timestamp, 0);
        assert_eq!(clock.stamp(20.0).rtp_timestamp, FRAME);
        clock.pause();
        // 1 s + 15 ms after the last frame: 50 whole frames of wall time,
        // so the resumed frame sits 50 frames after the last one.
        let resumed = clock.stamp(1_035.0);
        assert_eq!(resumed.rtp_timestamp, FRAME + 50 * FRAME);
        assert!(resumed.after_gap);
        // And continues contiguously from there.
        let next = clock.stamp(1_055.0);
        assert_eq!(next.rtp_timestamp, FRAME + 51 * FRAME);
        assert!(!next.after_gap);
    }

    #[test]
    fn short_pause_never_goes_backward_or_repeats() {
        let mut clock = AudioRtpClock::opus_20ms();
        let first = clock.stamp(100.0).rtp_timestamp;
        clock.pause();
        // Resume after less than one frame, and with a clock that went
        // backward: still exactly one frame later, never the same or earlier.
        let resumed = clock.stamp(105.0);
        assert_eq!(resumed.rtp_timestamp, first + FRAME);
        assert!(resumed.after_gap);
        clock.pause();
        let backward = clock.stamp(50.0);
        assert_eq!(backward.rtp_timestamp, first + 2 * FRAME);
    }

    #[test]
    fn pause_before_first_frame_is_not_a_gap() {
        let mut clock = AudioRtpClock::opus_20ms();
        clock.pause();
        let first = clock.stamp(5_000.0);
        assert_eq!(first.rtp_timestamp, 0);
        assert!(!first.after_gap);
    }

    #[test]
    fn wraps_modulo_2_pow_32() {
        let start = u32::MAX - FRAME + 1; // exactly one frame before wrap
        let mut clock = AudioRtpClock::opus_20ms().with_initial_timestamp(start);
        assert_eq!(clock.stamp(0.0).rtp_timestamp, start);
        assert_eq!(clock.stamp(20.0).rtp_timestamp, 0);
        assert_eq!(clock.stamp(40.0).rtp_timestamp, FRAME);
        // A jump across the wrap point also wraps.
        let mut clock = AudioRtpClock::opus_20ms().with_initial_timestamp(start);
        clock.stamp(0.0);
        clock.pause();
        let resumed = clock.stamp(100.0); // 5 frames later
        assert_eq!(resumed.rtp_timestamp, start.wrapping_add(5 * FRAME));
    }

    #[test]
    fn legacy_fallback_is_sequence_times_960_wrapping() {
        assert_eq!(legacy_rtp_timestamp(0), 0);
        assert_eq!(legacy_rtp_timestamp(1), 960);
        assert_eq!(legacy_rtp_timestamp(392), 392 * 960);
        // 2^32 / 960 is not an integer: check true modular arithmetic.
        let seq = 4_473_925u64; // 4_473_925 * 960 = 4_294_968_000 > 2^32
        assert_eq!(
            legacy_rtp_timestamp(seq),
            ((seq * 960) % (1u64 << 32)) as u32
        );
        assert_eq!(
            legacy_rtp_timestamp(u64::MAX),
            ((u64::MAX as u128 * 960) % (1u128 << 32)) as u32
        );
    }

    /// Wire compatibility of `AudioMetadata.rtp_timestamp` (tag 6, varint).
    #[test]
    fn rtp_timestamp_field_is_additive_on_the_wire() {
        use crate::protos::media_packet::AudioMetadata;
        use protobuf::Message;

        let legacy = AudioMetadata {
            sequence: 392,
            ..Default::default()
        };
        let legacy_bytes = legacy.write_to_bytes().unwrap();
        // Old senders' bytes (no field 6) decode as "absent" -> fallback.
        let decoded = AudioMetadata::parse_from_bytes(&legacy_bytes).unwrap();
        assert_eq!(decoded.rtp_timestamp, None);

        // New senders append exactly one varint field (key 6 << 3 | 0 = 0x30)
        // to the legacy encoding; an old receiver skips it as unknown.
        let new = AudioMetadata {
            rtp_timestamp: Some(0),
            ..legacy.clone()
        };
        let new_bytes = new.write_to_bytes().unwrap();
        assert_eq!(&new_bytes[..legacy_bytes.len()], &legacy_bytes[..]);
        assert_eq!(&new_bytes[legacy_bytes.len()..], &[0x30, 0x00]);
        // Explicit presence: 0 is a real value, distinct from absent.
        let decoded = AudioMetadata::parse_from_bytes(&new_bytes).unwrap();
        assert_eq!(decoded.rtp_timestamp, Some(0));
        assert_eq!(decoded.sequence, 392);
    }

    #[test]
    fn legacy_fallback_is_strictly_advancing() {
        for seq in 0..1_000u64 {
            assert_eq!(
                legacy_rtp_timestamp(seq + 1).wrapping_sub(legacy_rtp_timestamp(seq)),
                960
            );
        }
    }
}

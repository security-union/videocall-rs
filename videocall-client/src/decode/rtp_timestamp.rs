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

//! Receiver-side RTP timestamp derivation for NetEq (PARITY_PLAN.md 8.1).
//!
//! WebRTC NetEq orders, deduplicates and schedules packets by their RTP
//! timestamp, counted in samples of the codec clock (48 kHz for Opus). Our
//! wire format does not carry one: `MediaPacket.timestamp` is the sender's
//! wall clock in milliseconds as an `f64`, and `AudioMetadata.sequence` is a
//! `u64` packet counter. (Forwarding `timestamp as u32` does not work: epoch
//! milliseconds exceed `u32::MAX`, so the cast saturates and every packet
//! would reach NetEq with timestamp `u32::MAX`.)
//!
//! [`RtpTimestampMapper`] rebuilds an RTP-style timeline without a wire
//! change:
//!
//! - The sequence number drives the timeline: each packet advances it by its
//!   duration in samples, read from the Opus TOC byte (RFC 6716 section 3.1),
//!   so 10, 40 or 60 ms frames work too. A lost packet still advances the
//!   timeline, because the next sequence number skips it.
//! - The sender clock only detects pauses. When the sender stops producing
//!   packets (mic muted, Opus DTX) the sequence stays contiguous but real
//!   time moves on. RTP senders reflect that as a timestamp jump, and NetEq
//!   relies on it: after a long expand, a packet whose timestamp lands behind
//!   the playout point is discarded as old. So a sender-clock excess of at
//!   least [`SENDER_GAP_THRESHOLD_MS`] becomes a timestamp jump rounded to
//!   whole frames. Sender scheduling jank stays under that threshold.
//! - A restarted sender counter (encoder restarted after a device switch)
//!   starts a new stream epoch. The new timeline continues from the old one,
//!   moved forward by the elapsed sender time, so NetEq sees a gap rather
//!   than a backward jump.
//!
//! The mapper outputs exactly what the NetEq worker's `Insert { seq,
//! timestamp }` message takes (`u16` sequence, `u32` timestamp). NetEq
//! handles `u16` sequence and `u32` timestamp wraparound itself.
//!
//! The sequence number and sender clock come from the remote peer, so every
//! step here tolerates hostile values (`u64::MAX`, NaN) without panicking,
//! debug builds included.

use std::collections::VecDeque;

/// RTP clock rate for Opus (RFC 7587): always 48 kHz.
pub const RTP_CLOCK_RATE_HZ: u32 = 48_000;

/// Samples per packet assumed until a TOC byte says otherwise: 20 ms Opus.
pub const DEFAULT_SAMPLES_PER_PACKET: u32 = 960;

/// Sender-clock excess over the sequence-implied duration that counts as a
/// pause in sending. Opus DTX pauses are at least 400 ms and mic mutes are
/// seconds, while sender main-thread jank stays well below this.
pub const SENDER_GAP_THRESHOLD_MS: f64 = 200.0;

/// How far behind the highest sequence number a packet may arrive and still
/// count as reordered. 250 packets of 20 ms is 5 s, the same horizon NetEq
/// uses to discard old packets.
pub const MAX_REORDER_PACKETS: u64 = 250;

/// A packet with an older sequence number whose sender clock is newer than
/// the highest packet's by more than this comes from a restarted counter,
/// not from reordering.
pub const RESTART_CLOCK_TOLERANCE_MS: f64 = 100.0;

/// A forward sequence jump that outruns the sender clock by more than this
/// cannot be packet loss: the counter changed.
pub const FORWARD_JUMP_CLOCK_TOLERANCE_MS: f64 = 2_000.0;

/// Longest Opus packet (RFC 6716): 120 ms at 48 kHz.
const MAX_OPUS_PACKET_SAMPLES: u32 = 5_760;

/// Bound on the timeline segments kept for late-packet lookups.
const MAX_SEGMENTS: usize = 32;

/// One packet's RTP header fields for NetEq.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpMapping {
    /// Low 16 bits of the wire sequence number.
    pub sequence_number: u16,
    /// Timestamp in 48 kHz samples. Wraps at `u32::MAX` like RTP.
    pub timestamp: u32,
    /// Counts restarts of the sender's sequence counter. Usable as an
    /// SSRC-like stream id.
    pub stream_epoch: u32,
    /// True on the first packet of a new epoch after a restart. The caller
    /// may flush NetEq here; the timeline stays continuous either way.
    pub stream_reset: bool,
}

/// A run of packets with the same duration and no sender pause inside.
#[derive(Debug, Clone, Copy)]
struct Segment {
    start_seq: u64,
    start_ts: u32,
    samples_per_packet: u32,
}

impl Segment {
    fn timestamp_at(&self, seq: u64) -> u32 {
        // Modular arithmetic: (a * b mod 2^64) mod 2^32 == a * b mod 2^32,
        // so wrapping u64 math truncated to u32 is exact RTP wraparound.
        let spp = u64::from(self.samples_per_packet);
        if seq >= self.start_seq {
            let offset = (seq - self.start_seq).wrapping_mul(spp) as u32;
            self.start_ts.wrapping_add(offset)
        } else {
            let offset = (self.start_seq - seq).wrapping_mul(spp) as u32;
            self.start_ts.wrapping_sub(offset)
        }
    }

    fn frame_ms(&self) -> f64 {
        f64::from(self.samples_per_packet) * 1000.0 / f64::from(RTP_CLOCK_RATE_HZ)
    }
}

#[derive(Debug, Clone)]
struct StreamState {
    /// Ordered by `start_seq`, never empty.
    segments: VecDeque<Segment>,
    highest_seq: u64,
    highest_sender_ms: Option<f64>,
}

impl StreamState {
    fn new(seq: u64, ts: u32, samples_per_packet: u32, sender_ms: Option<f64>) -> Self {
        let mut segments = VecDeque::with_capacity(4);
        segments.push_back(Segment {
            start_seq: seq,
            start_ts: ts,
            samples_per_packet,
        });
        Self {
            segments,
            highest_seq: seq,
            highest_sender_ms: sender_ms,
        }
    }

    fn current(&self) -> Segment {
        // Invariant: `segments` is never empty.
        self.segments[self.segments.len() - 1]
    }

    fn timestamp_at(&self, seq: u64) -> u32 {
        self.segments
            .iter()
            .rev()
            .find(|segment| segment.start_seq <= seq)
            .unwrap_or(&self.segments[0])
            .timestamp_at(seq)
    }

    fn push_segment(&mut self, segment: Segment) {
        self.segments.push_back(segment);
        // Drop segments no in-window late packet can land in.
        while self.segments.len() > 1
            && (self.segments.len() > MAX_SEGMENTS
                || self.segments[1]
                    .start_seq
                    .saturating_add(MAX_REORDER_PACKETS)
                    <= self.highest_seq)
        {
            self.segments.pop_front();
        }
    }
}

/// Derives RTP sequence numbers and 48 kHz timestamps for one remote audio
/// stream. Create one per remote peer decoder.
#[derive(Debug, Clone, Default)]
pub struct RtpTimestampMapper {
    state: Option<StreamState>,
    epoch: u32,
    initial_timestamp: u32,
}

impl RtpTimestampMapper {
    /// A mapper whose first packet gets timestamp 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// A mapper whose first packet gets `initial_timestamp`. RTP senders pick
    /// a random start; tests use this to exercise `u32` wraparound.
    pub fn with_initial_timestamp(initial_timestamp: u32) -> Self {
        Self {
            initial_timestamp,
            ..Self::default()
        }
    }

    /// Current stream epoch (number of detected sender restarts).
    pub fn stream_epoch(&self) -> u32 {
        self.epoch
    }

    /// Forget the stream. The next packet starts a fresh timeline at the
    /// initial timestamp, in the same epoch. Pair with a NetEq flush.
    pub fn reset(&mut self) {
        self.state = None;
    }

    /// Map one received packet.
    ///
    /// - `sequence`: `AudioMetadata.sequence` from the wire.
    /// - `sender_time_ms`: `MediaPacket.timestamp` (sender wall clock). A
    ///   non-finite or non-positive value disables pause detection.
    /// - `opus_payload`: the Opus packet (the RED primary for RED packets),
    ///   used only for its TOC byte. Pass an empty slice when the payload is
    ///   not a plain Opus packet, to keep the current frame size.
    ///
    /// Returns `None` for a straggler from before a sender restart; drop it.
    pub fn map(
        &mut self,
        sequence: u64,
        sender_time_ms: f64,
        opus_payload: &[u8],
    ) -> Option<RtpMapping> {
        let sender_ms =
            (sender_time_ms.is_finite() && sender_time_ms > 0.0).then_some(sender_time_ms);
        let toc_samples = opus_packet_samples_48k(opus_payload);

        let Some(state) = self.state.as_mut() else {
            let samples = toc_samples.unwrap_or(DEFAULT_SAMPLES_PER_PACKET);
            self.state = Some(StreamState::new(
                sequence,
                self.initial_timestamp,
                samples,
                sender_ms,
            ));
            return Some(self.mapping(sequence, false));
        };

        let current = state.current();
        let wall_delta_ms = match (sender_ms, state.highest_sender_ms) {
            (Some(now), Some(prev)) => Some(now - prev),
            _ => None,
        };

        if sequence == state.highest_seq {
            // Duplicate: same timestamp, NetEq discards it.
            return Some(self.mapping(sequence, false));
        }

        if sequence < state.highest_seq {
            let behind = state.highest_seq - sequence;
            let clock_says_restart =
                matches!(wall_delta_ms, Some(delta) if delta > RESTART_CLOCK_TOLERANCE_MS);
            if behind > MAX_REORDER_PACKETS || clock_says_restart {
                return Some(self.restart(sequence, sender_ms, wall_delta_ms, toc_samples));
            }
            // Reordered or late packet: look it up, leave state alone.
            return Some(self.mapping(sequence, false));
        }

        let ahead = sequence - state.highest_seq;
        let sequence_ms = ahead as f64 * current.frame_ms();
        if ahead > MAX_REORDER_PACKETS {
            match wall_delta_ms {
                // Far ahead in sequence but sent earlier: a straggler from the
                // counter that a restart replaced.
                Some(delta) if delta < 0.0 => return None,
                Some(delta) if sequence_ms - delta > FORWARD_JUMP_CLOCK_TOLERANCE_MS => {
                    return Some(self.restart(sequence, sender_ms, wall_delta_ms, toc_samples));
                }
                // Long loss (e.g. a transport reconnect): just advance.
                _ => {}
            }
        }

        let gap_packets = match wall_delta_ms {
            Some(delta) if delta - sequence_ms >= SENDER_GAP_THRESHOLD_MS => {
                ((delta - sequence_ms) / current.frame_ms()).round() as u64
            }
            _ => 0,
        };
        let samples = toc_samples.unwrap_or(current.samples_per_packet);
        if gap_packets > 0 || samples != current.samples_per_packet {
            // The pause (or frame-size change) starts right after the last
            // packet seen; lost packets after it belong to the new segment.
            let start_seq = state.highest_seq.wrapping_add(1);
            let gap_samples = gap_packets.wrapping_mul(u64::from(current.samples_per_packet));
            let start_ts = current
                .timestamp_at(start_seq)
                .wrapping_add(gap_samples as u32);
            state.highest_seq = sequence;
            state.push_segment(Segment {
                start_seq,
                start_ts,
                samples_per_packet: samples,
            });
        } else {
            state.highest_seq = sequence;
        }
        if sender_ms.is_some() {
            state.highest_sender_ms = sender_ms;
        }
        Some(self.mapping(sequence, false))
    }

    /// Map a frame recovered from a RED packet. `redundant_sequence` is the
    /// 32-bit sequence carried in the RED payload; it is widened against the
    /// primary's 64-bit sequence. Call after [`Self::map`] for the primary.
    /// Does not change state. Returns `None` if the distance is implausible.
    pub fn map_red_recovered(
        &self,
        primary_sequence: u64,
        redundant_sequence: u32,
    ) -> Option<RtpMapping> {
        self.state.as_ref()?;
        let behind = u64::from((primary_sequence as u32).wrapping_sub(redundant_sequence));
        if behind == 0 || behind > MAX_REORDER_PACKETS {
            return None;
        }
        let sequence = primary_sequence.checked_sub(behind)?;
        Some(self.mapping(sequence, false))
    }

    fn restart(
        &mut self,
        sequence: u64,
        sender_ms: Option<f64>,
        wall_delta_ms: Option<f64>,
        toc_samples: Option<u32>,
    ) -> RtpMapping {
        let start_ts = match self.state.as_ref() {
            Some(state) => {
                let current = state.current();
                let next = current.timestamp_at(state.highest_seq.wrapping_add(1));
                let elapsed_packets = match wall_delta_ms {
                    Some(delta) if delta > current.frame_ms() => {
                        ((delta - current.frame_ms()) / current.frame_ms()).round() as u64
                    }
                    _ => 0,
                };
                next.wrapping_add(
                    elapsed_packets.wrapping_mul(u64::from(current.samples_per_packet)) as u32,
                )
            }
            None => self.initial_timestamp,
        };
        let samples = toc_samples.unwrap_or_else(|| {
            self.state
                .as_ref()
                .map_or(DEFAULT_SAMPLES_PER_PACKET, |state| {
                    state.current().samples_per_packet
                })
        });
        self.epoch = self.epoch.wrapping_add(1);
        self.state = Some(StreamState::new(sequence, start_ts, samples, sender_ms));
        self.mapping(sequence, true)
    }

    fn mapping(&self, sequence: u64, stream_reset: bool) -> RtpMapping {
        let timestamp = self
            .state
            .as_ref()
            .map_or(self.initial_timestamp, |state| state.timestamp_at(sequence));
        RtpMapping {
            sequence_number: sequence as u16,
            timestamp,
            stream_epoch: self.epoch,
            stream_reset,
        }
    }
}

/// Duration of an Opus packet in 48 kHz samples, from its TOC byte and frame
/// count (RFC 6716 section 3.1). `None` for an empty or malformed packet.
pub fn opus_packet_samples_48k(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let frame_samples = match config {
        // SILK-only: 10, 20, 40, 60 ms.
        0..=11 => [480, 960, 1920, 2880][usize::from(config & 3)],
        // Hybrid: 10, 20 ms.
        12..=15 => [480, 960][usize::from(config & 1)],
        // CELT-only: 2.5, 5, 10, 20 ms.
        _ => [120, 240, 480, 960][usize::from(config & 3)],
    };
    let frame_count = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1)? & 0x3F),
    };
    let total = frame_samples * frame_count;
    (total > 0 && total <= MAX_OPUS_PACKET_SAMPLES).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CELT fullband 20 ms, one frame (config 31, code 0).
    const OPUS_20MS: &[u8] = &[0xF8, 0x00];
    /// SILK wideband 40 ms, one frame (config 10, code 0).
    const OPUS_40MS: &[u8] = &[0x50, 0x00];
    const FRAME: u32 = 960;
    const FRAME_MS: f64 = 20.0;

    fn ts(packets: u64) -> u32 {
        (packets * u64::from(FRAME)) as u32
    }

    /// Feeds `count` contiguous 20 ms packets starting at `seq`/`ms`.
    fn feed(mapper: &mut RtpTimestampMapper, seq: u64, ms: f64, count: u64) -> Vec<RtpMapping> {
        (0..count)
            .map(|i| {
                mapper
                    .map(seq + i, ms + i as f64 * FRAME_MS, OPUS_20MS)
                    .expect("in-stream packet maps")
            })
            .collect()
    }

    #[test]
    fn toc_parsing_covers_frame_sizes_and_counts() {
        assert_eq!(opus_packet_samples_48k(&[0xF8]), Some(960)); // CELT 20 ms
        assert_eq!(opus_packet_samples_48k(&[0xF0]), Some(480)); // CELT 10 ms
        assert_eq!(opus_packet_samples_48k(&[0xE8]), Some(240)); // CELT 5 ms
        assert_eq!(opus_packet_samples_48k(&[0x80]), Some(120)); // CELT 2.5 ms
        assert_eq!(opus_packet_samples_48k(&[0x48]), Some(960)); // SILK 20 ms
        assert_eq!(opus_packet_samples_48k(&[0x58]), Some(2880)); // SILK 60 ms
        assert_eq!(opus_packet_samples_48k(&[0x78]), Some(960)); // Hybrid 20 ms
        assert_eq!(opus_packet_samples_48k(&[0xF9]), Some(1920)); // 2 x 20 ms
        assert_eq!(opus_packet_samples_48k(&[0xFA]), Some(1920)); // 2 x 20 ms, VBR
        assert_eq!(opus_packet_samples_48k(&[0xFB, 0x03]), Some(2880)); // 3 x 20 ms
        assert_eq!(opus_packet_samples_48k(&[0xFB, 0x07]), None); // 140 ms > max
        assert_eq!(opus_packet_samples_48k(&[0xFB, 0x00]), None); // zero frames
        assert_eq!(opus_packet_samples_48k(&[0xFB]), None); // missing count
        assert_eq!(opus_packet_samples_48k(&[]), None);
    }

    #[test]
    fn contiguous_packets_advance_one_frame() {
        let mut mapper = RtpTimestampMapper::new();
        let out = feed(&mut mapper, 0, 1_000.0, 5);
        for (i, m) in out.iter().enumerate() {
            assert_eq!(m.sequence_number, i as u16);
            assert_eq!(m.timestamp, ts(i as u64));
            assert_eq!(m.stream_epoch, 0);
            assert!(!m.stream_reset);
        }
    }

    #[test]
    fn first_packet_mid_stream_starts_at_initial_timestamp() {
        let mut mapper = RtpTimestampMapper::new();
        let out = feed(&mut mapper, 12_345, 1_000.0, 2);
        assert_eq!(out[0].timestamp, 0);
        assert_eq!(out[1].timestamp, FRAME);
    }

    #[test]
    fn sequence_wraps_at_u16_with_continuous_timestamps() {
        let mut mapper = RtpTimestampMapper::new();
        let out = feed(&mut mapper, 65_534, 1_000.0, 4);
        let seqs: Vec<u16> = out.iter().map(|m| m.sequence_number).collect();
        assert_eq!(seqs, vec![65_534, 65_535, 0, 1]);
        let stamps: Vec<u32> = out.iter().map(|m| m.timestamp).collect();
        assert_eq!(stamps, vec![0, FRAME, 2 * FRAME, 3 * FRAME]);
    }

    #[test]
    fn timestamp_wraps_at_u32() {
        let mut mapper = RtpTimestampMapper::with_initial_timestamp(u32::MAX - (FRAME - 1));
        let out = feed(&mut mapper, 0, 1_000.0, 3);
        assert_eq!(out[0].timestamp, u32::MAX - (FRAME - 1));
        assert_eq!(out[1].timestamp, 0);
        assert_eq!(out[2].timestamp, FRAME);
    }

    #[test]
    fn long_call_wraps_timestamp_naturally() {
        // 2^32 / 960 packets is about 24.9 hours of 20 ms audio.
        let mut mapper = RtpTimestampMapper::new();
        mapper.map(0, 1_000.0, OPUS_20MS).unwrap();
        let seq = 4_473_925u64; // first packet past the u32 wrap
        let m = mapper
            .map(seq, 1_000.0 + seq as f64 * FRAME_MS, OPUS_20MS)
            .unwrap();
        assert_eq!(u64::from(m.timestamp), (seq * 960) % (1u64 << 32));
        assert!(!m.stream_reset);
    }

    #[test]
    fn lost_packets_still_advance_the_timeline() {
        let mut mapper = RtpTimestampMapper::new();
        feed(&mut mapper, 0, 1_000.0, 2);
        let m = mapper.map(5, 1_100.0, OPUS_20MS).unwrap();
        assert_eq!(m.timestamp, ts(5));
    }

    #[test]
    fn reordered_and_duplicate_packets_keep_their_slot() {
        let mut mapper = RtpTimestampMapper::new();
        feed(&mut mapper, 0, 1_000.0, 2);
        assert_eq!(mapper.map(3, 1_060.0, OPUS_20MS).unwrap().timestamp, ts(3));
        assert_eq!(mapper.map(2, 1_040.0, OPUS_20MS).unwrap().timestamp, ts(2));
        assert_eq!(mapper.map(3, 1_060.0, OPUS_20MS).unwrap().timestamp, ts(3));
        assert_eq!(mapper.map(4, 1_080.0, OPUS_20MS).unwrap().timestamp, ts(4));
    }

    #[test]
    fn sender_pause_becomes_timestamp_jump() {
        // Mic muted for 3 s: the sequence stays contiguous, the clock does not.
        let mut mapper = RtpTimestampMapper::new();
        // Last packet at 181 ms.
        feed(&mut mapper, 0, 1.0, 10);
        let m = mapper.map(10, 181.0 + 3_000.0, OPUS_20MS).unwrap();
        // 3000 ms covers 1 frame plus 149 frames of pause.
        assert_eq!(m.timestamp, ts(10 + 149));
        assert!(!m.stream_reset);
        // The following packets continue from there.
        assert_eq!(
            mapper.map(11, 3_201.0, OPUS_20MS).unwrap().timestamp,
            ts(160)
        );
    }

    #[test]
    fn sender_jank_below_threshold_is_not_a_pause() {
        let mut mapper = RtpTimestampMapper::new();
        feed(&mut mapper, 0, 1_000.0, 2);
        // Stamped 150 ms late, then the backlog arrives in a burst.
        assert_eq!(mapper.map(2, 1_190.0, OPUS_20MS).unwrap().timestamp, ts(2));
        assert_eq!(mapper.map(3, 1_191.0, OPUS_20MS).unwrap().timestamp, ts(3));
    }

    #[test]
    fn late_packet_after_pause_uses_post_pause_timeline() {
        let mut mapper = RtpTimestampMapper::new();
        // Last packet at 1180 ms. Packet 10 is late; 11 arrives after a 1 s pause.
        feed(&mut mapper, 0, 1_000.0, 10);
        let eleven = mapper.map(11, 2_200.0, OPUS_20MS).unwrap();
        let ten = mapper.map(10, 2_180.0, OPUS_20MS).unwrap();
        assert_eq!(eleven.timestamp.wrapping_sub(ten.timestamp), FRAME);
        assert!(ten.timestamp > ts(10));
    }

    #[test]
    fn frame_size_change_follows_the_toc() {
        let mut mapper = RtpTimestampMapper::new();
        feed(&mut mapper, 0, 1_000.0, 2); // 0, 960
        let a = mapper.map(2, 1_040.0, OPUS_40MS).unwrap();
        let b = mapper.map(3, 1_080.0, OPUS_40MS).unwrap();
        assert_eq!(a.timestamp, ts(2));
        assert_eq!(b.timestamp, ts(2) + 1920);
    }

    #[test]
    fn counter_restart_starts_new_epoch_without_going_backwards() {
        let mut mapper = RtpTimestampMapper::new();
        let before = feed(&mut mapper, 0, 1_000.0, 500);
        // Seq 499 went out at 10_980 ms. Then a device switch restarts the
        // encoder at seq 0, 2 s later.
        let last = before.last().unwrap().timestamp;
        let m = mapper.map(0, 12_980.0, OPUS_20MS).unwrap();
        assert!(m.stream_reset);
        assert_eq!(m.stream_epoch, 1);
        assert_eq!(m.sequence_number, 0);
        assert_eq!(m.timestamp.wrapping_sub(last), ts(100));
        let next = mapper.map(1, 13_000.0, OPUS_20MS).unwrap();
        assert!(!next.stream_reset);
        assert_eq!(next.timestamp, m.timestamp + FRAME);
    }

    #[test]
    fn counter_restart_inside_reorder_window_is_caught_by_clock() {
        let mut mapper = RtpTimestampMapper::new();
        feed(&mut mapper, 0, 1_000.0, 20); // last at 1380 ms
        let m = mapper.map(0, 1_900.0, OPUS_20MS).unwrap();
        assert!(m.stream_reset);
        assert_eq!(m.stream_epoch, 1);
    }

    #[test]
    fn straggler_from_previous_counter_is_dropped() {
        let mut mapper = RtpTimestampMapper::new();
        feed(&mut mapper, 0, 1_000.0, 1_000); // last at 20_980 ms
        mapper.map(0, 22_000.0, OPUS_20MS).unwrap(); // restart
        feed(&mut mapper, 1, 22_020.0, 5);
        // A packet from the old counter, sent before the restart, arrives late.
        assert_eq!(mapper.map(999, 20_980.0, OPUS_20MS), None);
        // The new stream is unaffected.
        let m = mapper.map(6, 22_120.0, OPUS_20MS).unwrap();
        assert!(!m.stream_reset);
        assert_eq!(m.stream_epoch, 1);
    }

    #[test]
    fn long_outage_with_matching_clock_is_loss_not_restart() {
        let mut mapper = RtpTimestampMapper::new();
        feed(&mut mapper, 0, 1_000.0, 2);
        // 30 s transport outage: 1500 packets lost, clock agrees.
        let m = mapper
            .map(1_501, 1_020.0 + 1_500.0 * 20.0, OPUS_20MS)
            .unwrap();
        assert!(!m.stream_reset);
        assert_eq!(m.timestamp, ts(1_501));
    }

    #[test]
    fn missing_sender_clock_falls_back_to_sequence_only() {
        let mut mapper = RtpTimestampMapper::new();
        mapper.map(0, 0.0, OPUS_20MS).unwrap();
        assert_eq!(mapper.map(1, f64::NAN, OPUS_20MS).unwrap().timestamp, FRAME);
        // A backward jump beyond the reorder window still restarts.
        mapper.map(1_000, 0.0, OPUS_20MS).unwrap();
        let m = mapper.map(0, 0.0, OPUS_20MS).unwrap();
        assert!(m.stream_reset);
    }

    #[test]
    fn red_recovered_frame_lands_one_frame_before_primary() {
        let mut mapper = RtpTimestampMapper::new();
        feed(&mut mapper, 0, 1_000.0, 10);
        let primary = mapper.map(11, 1_220.0, OPUS_20MS).unwrap();
        let recovered = mapper.map_red_recovered(11, 10).unwrap();
        assert_eq!(recovered.sequence_number, 10);
        assert_eq!(primary.timestamp - recovered.timestamp, FRAME);
    }

    #[test]
    fn red_recovered_sequence_is_widened_across_u32_wrap() {
        let mut mapper = RtpTimestampMapper::new();
        let primary_seq = 1u64 << 32; // low 32 bits are 0
        mapper.map(primary_seq - 1, 1_000.0, OPUS_20MS).unwrap();
        let primary = mapper.map(primary_seq, 1_020.0, OPUS_20MS).unwrap();
        let recovered = mapper.map_red_recovered(primary_seq, u32::MAX).unwrap();
        assert_eq!(recovered.sequence_number, u16::MAX);
        assert_eq!(primary.timestamp.wrapping_sub(recovered.timestamp), FRAME);
    }

    #[test]
    fn red_with_implausible_distance_is_rejected() {
        let mut mapper = RtpTimestampMapper::new();
        assert_eq!(mapper.map_red_recovered(10, 9), None); // no stream yet
        mapper.map(10, 1_000.0, OPUS_20MS).unwrap();
        assert_eq!(mapper.map_red_recovered(10, 10), None);
        assert_eq!(mapper.map_red_recovered(10, 11), None);
        assert_eq!(mapper.map_red_recovered(1_000, 1), None);
    }

    #[test]
    fn explicit_reset_restarts_timeline_in_same_epoch() {
        let mut mapper = RtpTimestampMapper::new();
        feed(&mut mapper, 0, 1_000.0, 3);
        mapper.reset();
        let m = mapper.map(50, 5_000.0, OPUS_20MS).unwrap();
        assert_eq!(m.timestamp, 0);
        assert_eq!(m.stream_epoch, 0);
    }

    #[test]
    fn segment_history_stays_bounded() {
        let mut mapper = RtpTimestampMapper::new();
        let mut ms = 1_000.0;
        for seq in 0..1_000u64 {
            // A 1 s pause before every packet.
            ms += 1_000.0;
            mapper.map(seq, ms, OPUS_20MS).unwrap();
        }
        let segments = mapper.state.as_ref().unwrap().segments.len();
        assert!(segments <= MAX_SEGMENTS, "{segments} segments kept");
    }

    #[test]
    fn hostile_sequence_and_clock_values_do_not_panic() {
        let mut mapper = RtpTimestampMapper::new();
        let hostile_clocks = [f64::NAN, f64::INFINITY, -1.0, 0.0, 1e300, 1_000.0];
        let hostile_sequences = [u64::MAX, 0, u64::MAX - 1, u64::MAX / 2, 1, u64::MAX];
        for &sequence in &hostile_sequences {
            for &clock in &hostile_clocks {
                let _ = mapper.map(sequence, clock, OPUS_20MS);
                let _ = mapper.map(sequence, clock, &[0xFB, 0x3F]);
                let _ = mapper.map_red_recovered(sequence, sequence as u32);
                let _ = mapper.map_red_recovered(sequence, u32::MAX);
            }
        }
        // Pause detection on the u64::MAX packet pushes a segment there.
        let mut mapper = RtpTimestampMapper::new();
        mapper.map(u64::MAX - 1, 1_000.0, OPUS_20MS).unwrap();
        mapper.map(u64::MAX, 5_000.0, OPUS_20MS).unwrap();
        mapper.map(u64::MAX, 9_000.0, OPUS_40MS).unwrap();
    }
}

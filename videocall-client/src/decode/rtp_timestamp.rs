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
//! wire format carries the same information in milliseconds: the sender
//! stamps `MediaPacket.timestamp` as `session start + frame index * 20 ms`,
//! where the frame index counts every encoded 20 ms frame, including the
//! ones Opus DTX did not send. That is libwebrtc's RTP timestamp (960
//! samples per frame, advancing over dropped DTX frames) in ms, as an `f64`
//! anchored to the sender's wall clock when the session starts.
//! `AudioMetadata.sequence` is a `u64` counter of sent packets only.
//! (Forwarding `timestamp as u32` does not work: epoch milliseconds exceed
//! `u32::MAX`, so the cast saturates.)
//!
//! [`RtpTimestampMapper`] turns that into an RTP timeline:
//!
//! - The timestamp is the stamp's distance from the epoch's first stamp,
//!   in 48 kHz samples, rounded to whole packets (the packet duration comes
//!   from the Opus TOC byte, RFC 6716 section 3.1). DTX pauses, mutes and
//!   lost packets all fall out of the stamp; nothing is guessed from
//!   thresholds, and when the sender's main thread posted a packet does not
//!   matter.
//! - A late or reordered packet maps from its own stamp.
//! - A restarted sender counter (a new capture session after unmute or a
//!   device switch) starts a new stream epoch on the new session's grid.
//!   It continues forward from the old timeline, by the stamps' distance or
//!   at least one packet, so NetEq never sees a backward jump. Frame-derived
//!   stamps drift from the wall clock by the capture clock's error, and the
//!   new session re-anchors to the wall clock, so this can matter.
//! - With no usable stamp (NaN, zero), the sequence number drives the
//!   timeline from the last mapped packet.
//! - A stamp that disagrees with the sequence numbers is not trusted, and
//!   its packet maps by sequence. Within one session two sent packets are
//!   at least one frame apart and, because DTX still sends a noise refresh
//!   every 400 ms, at most [`MAX_SENT_PACKET_SPACING_MS`] apart per
//!   sequence step; [`FORWARD_JUMP_CLOCK_TOLERANCE_MS`] on top covers
//!   jitter and worker stalls. If the stamps keep disagreeing but agree
//!   with each other ([`REANCHOR_PACKETS`] in a row), the epoch re-anchors
//!   on them: its own first stamp was the bad one.
//!
//! The mapper outputs exactly what the NetEq worker's `Insert { seq,
//! timestamp }` message takes (`u16` sequence, `u32` timestamp). NetEq
//! handles `u16` sequence and `u32` timestamp wraparound itself.
//!
//! The sequence number and stamp come from the remote peer, so every step
//! here tolerates hostile values (`u64::MAX`, NaN, 1e300) without panicking,
//! debug builds included. A hostile stamp on an in-order or late packet is
//! ignored (the packet maps by sequence) and is never used as a reference
//! for later packets. Two cases cost more than one packet: a bad first
//! stamp of an epoch misplaces the next few packets until the epoch
//! re-anchors, and a late packet stamped newer than the stream forces a
//! restart (a new epoch). Either way only the sending peer's own stream is
//! affected.

/// RTP clock rate for Opus (RFC 7587): always 48 kHz.
pub const RTP_CLOCK_RATE_HZ: u32 = 48_000;

/// Samples per packet assumed until a TOC byte says otherwise: 20 ms Opus.
pub const DEFAULT_SAMPLES_PER_PACKET: u32 = 960;

/// How far behind the highest sequence number a packet may arrive and still
/// count as reordered. 250 packets of 20 ms is 5 s, the same horizon NetEq
/// uses to discard old packets.
pub const MAX_REORDER_PACKETS: u64 = 250;

/// A packet with an older sequence number whose stamp is newer than the
/// highest packet's by more than this comes from a restarted counter, not
/// from reordering.
pub const RESTART_CLOCK_TOLERANCE_MS: f64 = 100.0;

/// A forward sequence jump that outruns the stamps by more than this cannot
/// be packet loss: the counter changed.
pub const FORWARD_JUMP_CLOCK_TOLERANCE_MS: f64 = 2_000.0;

/// Upper bound on the stamp distance between two consecutive sent packets
/// of one session. libopus in DTX still sends a background-noise refresh
/// every 400 ms; this leaves room for lost refreshes.
pub const MAX_SENT_PACKET_SPACING_MS: f64 = 1_000.0;

/// Mutually consistent packets, all disagreeing with the epoch's stamps,
/// after which the epoch re-anchors on them.
pub const REANCHOR_PACKETS: u32 = 5;

/// Longest Opus packet (RFC 6716): 120 ms at 48 kHz.
const MAX_OPUS_PACKET_SAMPLES: u32 = 5_760;

/// Samples per millisecond at the RTP clock rate.
const SAMPLES_PER_MS: f64 = RTP_CLOCK_RATE_HZ as f64 / 1000.0;

/// Bound on packets counted from one stamp difference: far more than a
/// `u32` timeline holds, and small enough that `i64` math cannot overflow.
const MAX_STAMP_PACKETS: f64 = (1u64 << 40) as f64;

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

#[derive(Debug, Clone, Copy)]
struct StreamState {
    /// First stamp of the epoch and the timestamp it maps to. `None` while
    /// the epoch has seen no usable stamp.
    base: Option<(f64, u32)>,
    highest_seq: u64,
    /// Timestamp and duration of the packet at `highest_seq`.
    highest_ts: u32,
    highest_samples: u32,
    highest_stamp_ms: Option<f64>,
    /// The packet mapped last, for RED recovery right after it.
    last_mapped: Option<(u64, u32)>,
    /// Whether a later packet has agreed with the epoch's stamps. Until
    /// then the first stamp may be garbage and cannot mark stragglers.
    confirmed: bool,
    /// The last packet whose stamp disagreed with the epoch, and how many
    /// such packets in a row agree with each other.
    suspect: Option<(u64, f64)>,
    suspect_streak: u32,
}

impl StreamState {
    fn new(seq: u64, ts: u32, samples: u32, stamp_ms: Option<f64>) -> Self {
        Self {
            base: stamp_ms.map(|stamp| (stamp, ts)),
            highest_seq: seq,
            highest_ts: ts,
            highest_samples: samples,
            highest_stamp_ms: stamp_ms,
            last_mapped: Some((seq, ts)),
            confirmed: false,
            suspect: None,
            suspect_streak: 0,
        }
    }

    fn packet_ms(&self) -> f64 {
        f64::from(self.highest_samples) / SAMPLES_PER_MS
    }

    /// Record a packet whose stamp disagrees with the epoch. True when
    /// enough of them in a row agree with each other to re-anchor.
    fn note_suspect(&mut self, seq: u64, stamp: f64) -> bool {
        let packet_ms = self.packet_ms();
        let follows = matches!(self.suspect, Some((prev_seq, prev_stamp))
            if seq > prev_seq && stamp_plausible(seq - prev_seq, stamp - prev_stamp, packet_ms));
        self.suspect_streak = if follows { self.suspect_streak + 1 } else { 1 };
        self.suspect = Some((seq, stamp));
        self.suspect_streak >= REANCHOR_PACKETS
    }

    /// Timestamp for a packet stamped `stamp_ms` lasting `samples`, or the
    /// sequence-derived one when there is no stamp or no base.
    fn timestamp_for(&self, seq: u64, stamp_ms: Option<f64>, samples: u32) -> u32 {
        match (stamp_ms, self.base) {
            (Some(stamp), Some((base_ms, base_ts))) => {
                base_ts.wrapping_add(stamp_offset_samples(stamp - base_ms, samples))
            }
            _ => {
                // Modular arithmetic: wrapping u64 math truncated to u32 is
                // exact RTP wraparound.
                let spp = u64::from(self.highest_samples);
                if seq >= self.highest_seq {
                    let offset = (seq - self.highest_seq).wrapping_mul(spp) as u32;
                    self.highest_ts.wrapping_add(offset)
                } else {
                    let offset = (self.highest_seq - seq).wrapping_mul(spp) as u32;
                    self.highest_ts.wrapping_sub(offset)
                }
            }
        }
    }
}

/// Whether two packets `steps` sequence numbers apart can be `delta_ms`
/// apart in stamp: at least a frame per step (loss included), at most
/// [`MAX_SENT_PACKET_SPACING_MS`] per step, give or take
/// [`FORWARD_JUMP_CLOCK_TOLERANCE_MS`].
fn stamp_plausible(steps: u64, delta_ms: f64, packet_ms: f64) -> bool {
    let steps = steps as f64;
    delta_ms >= steps * packet_ms - FORWARD_JUMP_CLOCK_TOLERANCE_MS
        && delta_ms <= steps * MAX_SENT_PACKET_SPACING_MS + FORWARD_JUMP_CLOCK_TOLERANCE_MS
}

/// `delta_ms` in 48 kHz samples, rounded to whole packets of `samples`,
/// wrapped to `u32`.
fn stamp_offset_samples(delta_ms: f64, samples: u32) -> u32 {
    let packets = (delta_ms * SAMPLES_PER_MS / f64::from(samples))
        .round()
        .clamp(-MAX_STAMP_PACKETS, MAX_STAMP_PACKETS);
    // NaN cannot reach here (stamps are finite), and the clamp keeps the
    // product well inside i64.
    ((packets as i64) * i64::from(samples)) as u32
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
    /// - `sender_time_ms`: `MediaPacket.timestamp`, the frame-exact stamp.
    ///   A non-finite or non-positive value falls back to the sequence.
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
        let stamp_ms =
            (sender_time_ms.is_finite() && sender_time_ms > 0.0).then_some(sender_time_ms);
        let toc_samples = opus_packet_samples_48k(opus_payload);

        let Some(state) = self.state.as_mut() else {
            let samples = toc_samples.unwrap_or(DEFAULT_SAMPLES_PER_PACKET);
            self.state = Some(StreamState::new(
                sequence,
                self.initial_timestamp,
                samples,
                stamp_ms,
            ));
            return Some(self.mapping(sequence, self.initial_timestamp, false));
        };
        let samples = toc_samples.unwrap_or(state.highest_samples);
        let stamp_delta_ms = match (stamp_ms, state.highest_stamp_ms) {
            (Some(now), Some(prev)) => Some(now - prev),
            _ => None,
        };

        if sequence <= state.highest_seq {
            let behind = state.highest_seq - sequence;
            let clock_says_restart =
                matches!(stamp_delta_ms, Some(delta) if delta > RESTART_CLOCK_TOLERANCE_MS);
            if behind > MAX_REORDER_PACKETS || (behind > 0 && clock_says_restart) {
                return Some(self.restart(sequence, stamp_ms, samples));
            }
            // Reordered, late or duplicate: its own stamp places it, if the
            // stamp fits where the sequence says it belongs.
            let stamp_ms = stamp_ms.filter(|_| {
                stamp_delta_ms
                    .is_none_or(|delta| stamp_plausible(behind, -delta, state.packet_ms()))
            });
            let ts = state.timestamp_for(sequence, stamp_ms, samples);
            state.last_mapped = Some((sequence, ts));
            return Some(self.mapping(sequence, ts, false));
        }

        let ahead = sequence - state.highest_seq;
        if ahead > MAX_REORDER_PACKETS {
            let packet_ms = f64::from(state.highest_samples) / SAMPLES_PER_MS;
            let sequence_ms = ahead as f64 * packet_ms;
            match stamp_delta_ms {
                // Far ahead in sequence but stamped earlier: a straggler from
                // the counter that a restart replaced. Only once the epoch's
                // stamps are confirmed; a bad first stamp must not strand
                // the stream.
                Some(delta) if delta < 0.0 && state.confirmed => return None,
                Some(delta) if sequence_ms - delta > FORWARD_JUMP_CLOCK_TOLERANCE_MS => {
                    return Some(self.restart(sequence, stamp_ms, samples));
                }
                // Long loss (e.g. a transport reconnect): just advance.
                _ => {}
            }
        }

        if state.base.is_none() {
            // First usable stamp of the epoch anchors it.
            if let Some(stamp) = stamp_ms {
                let ts = state.timestamp_for(sequence, None, samples);
                state.base = Some((stamp, ts));
            }
        }
        let mut stamp_ms = stamp_ms;
        if let (Some(stamp), Some(delta)) = (stamp_ms, stamp_delta_ms) {
            if stamp_plausible(ahead, delta, state.packet_ms()) {
                state.confirmed = true;
                state.suspect = None;
                state.suspect_streak = 0;
            } else if state.note_suspect(sequence, stamp) {
                // The stamps agree with each other, not with the epoch: the
                // epoch's reference stamp was the bad one. Keep the
                // sequence-derived timeline and continue from these stamps.
                let ts = state.timestamp_for(sequence, None, samples);
                state.base = Some((stamp, ts));
                state.confirmed = true;
                state.suspect = None;
                state.suspect_streak = 0;
            } else {
                stamp_ms = None;
            }
        }
        let ts = state.timestamp_for(sequence, stamp_ms, samples);
        state.highest_seq = sequence;
        state.highest_ts = ts;
        state.highest_samples = samples;
        if stamp_ms.is_some() {
            state.highest_stamp_ms = stamp_ms;
        }
        state.last_mapped = Some((sequence, ts));
        Some(self.mapping(sequence, ts, false))
    }

    /// Map a frame recovered from a RED packet. `redundant_sequence` is the
    /// 32-bit sequence carried in the RED payload; it is widened against the
    /// primary's 64-bit sequence. Call right after [`Self::map`] for the
    /// primary. The sender attaches RED only when the redundant frame is the
    /// frame just before the primary, so only a distance of one is accepted
    /// and the frame lands one packet before the primary.
    pub fn map_red_recovered(
        &self,
        primary_sequence: u64,
        redundant_sequence: u32,
    ) -> Option<RtpMapping> {
        let state = self.state.as_ref()?;
        let (last_seq, last_ts) = state.last_mapped?;
        if last_seq != primary_sequence
            || (primary_sequence as u32).wrapping_sub(redundant_sequence) != 1
        {
            return None;
        }
        let sequence = primary_sequence.checked_sub(1)?;
        Some(self.mapping(sequence, last_ts.wrapping_sub(state.highest_samples), false))
    }

    /// Start a new epoch at `sequence`, on the new session's stamp grid,
    /// at least one packet after the old timeline's newest packet.
    fn restart(&mut self, sequence: u64, stamp_ms: Option<f64>, samples: u32) -> RtpMapping {
        let start_ts = match self.state.as_ref() {
            Some(state) => {
                let next = state.highest_ts.wrapping_add(state.highest_samples);
                let from_stamp = match (stamp_ms, state.base) {
                    (Some(_), Some(_)) => state.timestamp_for(sequence, stamp_ms, samples),
                    _ => next,
                };
                // Signed distance on the wrapping timeline.
                if (from_stamp.wrapping_sub(next) as i32) < 0 {
                    next
                } else {
                    from_stamp
                }
            }
            None => self.initial_timestamp,
        };
        self.epoch = self.epoch.wrapping_add(1);
        self.state = Some(StreamState::new(sequence, start_ts, samples, stamp_ms));
        self.mapping(sequence, start_ts, true)
    }

    fn mapping(&self, sequence: u64, timestamp: u32, stream_reset: bool) -> RtpMapping {
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

    // Three bytes: one or two would be a DTX packet (TOC only).
    /// CELT fullband 20 ms, one frame (config 31, code 0).
    const OPUS_20MS: &[u8] = &[0xF8, 0x00, 0x00];
    /// SILK wideband 40 ms, one frame (config 10, code 0).
    const OPUS_40MS: &[u8] = &[0x50, 0x00, 0x00];
    /// SILK wideband 20 ms TOC with no frame data: a DTX packet.
    const OPUS_DTX: &[u8] = &[0x48];
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

    /// Opus DTX: one TOC-only packet, then nothing until speech resumes some
    /// whole number of frames later. The sequence stays contiguous; the
    /// stamp carries the pause, at any length.
    #[test]
    fn dtx_pause_of_any_length_follows_the_stamp() {
        let mut mapper = RtpTimestampMapper::new();
        feed(&mut mapper, 0, 1_000.0, 3);
        assert_eq!(mapper.map(3, 1_060.0, OPUS_DTX).unwrap().timestamp, ts(3));
        // Speech resumes 7 frames after the DTX packet.
        let resumed = mapper.map(4, 1_060.0 + 7.0 * FRAME_MS, OPUS_20MS).unwrap();
        assert_eq!(resumed.timestamp, ts(10));
        // A one-frame gap is kept too: there is no pause threshold.
        assert_eq!(mapper.map(5, 1_240.0, OPUS_20MS).unwrap().timestamp, ts(12));
        assert!(!resumed.stream_reset);
    }

    /// Stamps are frame-exact (`session start + frame index * 20 ms`), so
    /// the time a packet happened to be sent never shows up. A stamp off the
    /// grid (an older sender) still lands on a whole frame.
    #[test]
    fn stamps_round_to_whole_frames() {
        let mut mapper = RtpTimestampMapper::new();
        mapper.map(0, 1_000.0, OPUS_20MS).unwrap();
        assert_eq!(mapper.map(1, 1_027.0, OPUS_20MS).unwrap().timestamp, ts(1));
        assert_eq!(mapper.map(2, 1_049.9, OPUS_20MS).unwrap().timestamp, ts(2));
        assert_eq!(mapper.map(3, 1_120.0, OPUS_20MS).unwrap().timestamp, ts(6));
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
    fn late_packet_after_pause_uses_its_own_stamp() {
        let mut mapper = RtpTimestampMapper::new();
        feed(&mut mapper, 0, 1_000.0, 10); // last at 1180 ms
                                           // Packet 10 (DTX) is late; 11 comes after a 1 s pause and arrives first.
        let eleven = mapper.map(11, 2_200.0, OPUS_20MS).unwrap();
        let ten = mapper.map(10, 1_200.0, OPUS_DTX).unwrap();
        assert_eq!(ten.timestamp, ts(10));
        assert_eq!(eleven.timestamp, ts(60));
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

    /// A new session's stamps start on its own grid, possibly off the old
    /// one; the new epoch follows the new grid exactly.
    #[test]
    fn restart_rebases_on_the_new_sessions_grid() {
        let mut mapper = RtpTimestampMapper::new();
        let before = feed(&mut mapper, 0, 1_000.0, 100); // last at 2_980 ms
        let last = before.last().unwrap().timestamp;
        let first = mapper.map(0, 5_007.0, OPUS_20MS).unwrap();
        assert!(first.stream_reset);
        assert_eq!(first.timestamp.wrapping_sub(last), ts(101));
        for frame in 1..5u64 {
            let m = mapper
                .map(frame, 5_007.0 + frame as f64 * FRAME_MS, OPUS_20MS)
                .unwrap();
            assert_eq!(m.timestamp.wrapping_sub(first.timestamp), ts(frame));
        }
    }

    /// Frame-derived stamps drift from the wall clock by the audio clock's
    /// error. If a new session's stamp lands behind the old one, the new
    /// epoch still moves forward.
    #[test]
    fn restart_never_goes_backwards() {
        let mut mapper = RtpTimestampMapper::new();
        let before = feed(&mut mapper, 0, 1_000.0, 500); // last at 10_980 ms
        let last = before.last().unwrap().timestamp;
        let m = mapper.map(0, 10_900.0, &[0xF8, 0x01, 0x02]).unwrap();
        assert!(m.stream_reset);
        assert_eq!(m.timestamp.wrapping_sub(last), FRAME);
        let next = mapper.map(1, 10_920.0, OPUS_20MS).unwrap();
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

    /// The sender attaches RED only when the previous packet is the previous
    /// frame, so only a distance of one is trusted.
    #[test]
    fn red_with_implausible_distance_is_rejected() {
        let mut mapper = RtpTimestampMapper::new();
        assert_eq!(mapper.map_red_recovered(10, 9), None); // no stream yet
        mapper.map(10, 1_000.0, OPUS_20MS).unwrap();
        assert_eq!(mapper.map_red_recovered(10, 10), None);
        assert_eq!(mapper.map_red_recovered(10, 11), None);
        assert_eq!(mapper.map_red_recovered(10, 8), None);
        assert_eq!(mapper.map_red_recovered(1_000, 1), None);
        // Only right after its primary was mapped.
        assert_eq!(mapper.map_red_recovered(11, 10), None);
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

    /// One in-order packet stamped far in the future must not become the
    /// reference: later packets, even after a long loss, still map.
    #[test]
    fn far_future_stamp_does_not_strand_later_packets() {
        let mut mapper = RtpTimestampMapper::new();
        feed(&mut mapper, 0, 1_000.0, 10);
        let hostile = mapper.map(10, 1_000.0 + 1e9, OPUS_20MS).unwrap();
        // Mapped by its sequence number instead.
        assert_eq!(hostile.timestamp, ts(10));
        // More than the reorder window later, with an honest stamp.
        let m = mapper
            .map(400, 1_000.0 + 400.0 * FRAME_MS, OPUS_20MS)
            .expect("not taken for a straggler");
        assert!(!m.stream_reset);
        assert_eq!(m.timestamp, ts(400));
        let next = mapper
            .map(401, 1_000.0 + 401.0 * FRAME_MS, OPUS_20MS)
            .unwrap();
        assert_eq!(next.timestamp, ts(401));
    }

    /// A garbage first stamp would anchor the epoch and collapse every later
    /// timestamp onto one value. Consistent stamps re-anchor the epoch.
    #[test]
    fn garbage_first_stamp_is_reanchored() {
        let mut mapper = RtpTimestampMapper::new();
        mapper.map(0, 1e15, OPUS_20MS).unwrap();
        let out: Vec<u32> = (1..40)
            .map(|i| {
                mapper
                    .map(i, 1_000.0 + i as f64 * FRAME_MS, OPUS_20MS)
                    .unwrap()
                    .timestamp
            })
            .collect();
        for (i, timestamp) in out.iter().enumerate() {
            assert_eq!(*timestamp, ts(i as u64 + 1), "packet {}", i + 1);
        }
        // Re-anchored: a DTX pause now follows the stamp again.
        let resumed = mapper
            .map(40, 1_000.0 + 50.0 * FRAME_MS, OPUS_20MS)
            .unwrap();
        assert_eq!(resumed.timestamp, ts(50));
    }

    /// A late packet with a nonsense stamp falls back to its sequence slot.
    #[test]
    fn late_packet_with_nonsense_stamp_keeps_its_sequence_slot() {
        let mut mapper = RtpTimestampMapper::new();
        feed(&mut mapper, 0, 1e9, 20);
        // Stamped 11 days before the stream started.
        assert_eq!(mapper.map(15, 10.0, OPUS_20MS).unwrap().timestamp, ts(15));
        assert_eq!(
            mapper.map(20, 1e9 + 400.0, OPUS_20MS).unwrap().timestamp,
            ts(20)
        );
    }

    #[test]
    fn hostile_sequence_and_clock_values_do_not_panic() {
        let mut mapper = RtpTimestampMapper::new();
        let hostile_clocks = [f64::NAN, f64::INFINITY, -1.0, 0.0, 1e300, 1_000.0, -1e300];
        let hostile_sequences = [u64::MAX, 0, u64::MAX - 1, u64::MAX / 2, 1, u64::MAX];
        for &sequence in &hostile_sequences {
            for &clock in &hostile_clocks {
                let _ = mapper.map(sequence, clock, OPUS_20MS);
                let _ = mapper.map(sequence, clock, &[0xFB, 0x3F]);
                let _ = mapper.map_red_recovered(sequence, sequence as u32);
                let _ = mapper.map_red_recovered(sequence, u32::MAX);
            }
        }
        let mut mapper = RtpTimestampMapper::new();
        mapper.map(u64::MAX - 1, 1_000.0, OPUS_20MS).unwrap();
        mapper.map(u64::MAX, 1e300, OPUS_20MS).unwrap();
        mapper.map(u64::MAX, -1e300, OPUS_40MS).unwrap();
    }
}

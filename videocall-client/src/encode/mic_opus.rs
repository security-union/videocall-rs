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

//! Microphone PCM to Opus packets, in pure Rust.
//!
//! This is the core of the `audio_encoder_worker` bin. The capture
//! AudioWorklet hands the worker mono `f32` blocks at the AudioContext's rate
//! (any size, typically 128 or 256 frames); [`MicOpusEncoder::push`] buffers
//! them, resamples to 48 kHz when needed, cuts 20 ms frames and encodes each
//! one with ropus. It has no browser dependencies, so it is tested natively.
//!
//! DTX follows WebRTC's sender rule (`WebRtcOpus_Encode` in
//! `modules/audio_coding/codecs/opus/opus_interface.cc`): an encoder output
//! of 2 bytes or less carries no audio. The first one is sent so the
//! receiver's decoder enters DTX (comfort noise) mode; the rest are dropped
//! until a real packet comes out, which includes libopus's periodic
//! background-noise refresh every 400 ms. Dropped frames never get a
//! sequence number, but they do count in the frame index each packet
//! carries. [`PacketClock`] turns that index into the packet's
//! `MediaPacket.timestamp`, `session start + index * 20 ms`, which is
//! libwebrtc's RTP timestamp in milliseconds; the receiver's
//! `RtpTimestampMapper` reads the DTX pause straight from it.
//!
//! Internal to the encoder worker and its tests; not a stable API.

use crate::constants::MIC_OPUS_COMPLEXITY;
use ropus::{Application, Bitrate, Channels, Encoder, Signal};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Resampler, SincInterpolationParameters, WindowFunction};
use std::fmt;

/// Opus encode rate. The wire format and NetEq both assume 48 kHz.
pub const OPUS_SAMPLE_RATE_HZ: u32 = 48_000;

/// Samples in one 20 ms frame at 48 kHz.
pub const OPUS_FRAME_SAMPLES: usize = 960;

/// libopus's recommended output buffer size; a 20 ms mono frame at our
/// bitrates is a few hundred bytes at most.
const MAX_PACKET_BYTES: usize = 4000;

/// Encoder outputs of this size or less are DTX frames (TOC byte only).
pub const DTX_PACKET_MAX_BYTES: usize = 2;

/// Duration of one Opus frame.
pub const OPUS_FRAME_MS: f64 = 20.0;

/// Opus's bitrate range (RFC 6716 section 2.1.1 for mono goes lower, but
/// libopus clamps below 6 kbps anyway).
const BITRATE_RANGE_BPS: std::ops::RangeInclusive<u32> = 6_000..=510_000;

/// Settings for one microphone encode session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicEncoderConfig {
    /// The AudioContext rate the worklet captures at (e.g. 44100, 48000).
    pub input_sample_rate: u32,
    /// Target Opus bitrate in bits per second.
    pub bitrate_bps: u32,
    /// Opus discontinuous transmission.
    pub dtx: bool,
}

/// Why an encoder could not be built or a block could not be encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MicEncoderError {
    InvalidSampleRate(u32),
    InvalidBitrate(u32),
    Resampler(String),
    Opus(String),
}

impl fmt::Display for MicEncoderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate(rate) => write!(f, "invalid input sample rate {rate}"),
            Self::InvalidBitrate(bps) => write!(f, "invalid bitrate {bps} bps"),
            Self::Resampler(e) => write!(f, "resampler: {e}"),
            Self::Opus(e) => write!(f, "opus: {e}"),
        }
    }
}

impl std::error::Error for MicEncoderError {}

/// WebRTC's DTX send rule: pass the first DTX frame, drop the ones after it.
#[derive(Debug, Default, Clone, Copy)]
pub struct DtxGate {
    in_dtx: bool,
}

impl DtxGate {
    /// Whether an encoder output of `len` bytes should be sent.
    pub fn admit(&mut self, len: usize) -> bool {
        if len <= DTX_PACKET_MAX_BYTES {
            // Send the first one so the decoder switches to comfort noise.
            let first = !self.in_dtx;
            self.in_dtx = true;
            first
        } else {
            self.in_dtx = false;
            true
        }
    }
}

/// Buffers capture blocks, resamples to 48 kHz and emits Opus packets.
pub struct MicOpusEncoder {
    encoder: Encoder,
    /// `None` when the input is already 48 kHz.
    resampler: Option<Async<f32>>,
    /// Input samples not yet consumed, at the input rate.
    pending: Vec<f32>,
    /// One 20 ms frame at 48 kHz, the resampler's output.
    frame: Vec<f32>,
    packet: Vec<u8>,
    dtx_gate: DtxGate,
    /// Index of the next frame to encode, counting frames DTX drops and
    /// frames of capture the worklet dropped.
    next_frame: u64,
    input_rate: u32,
    /// Dropped capture, in 48 kHz samples, not yet a whole frame.
    skipped_samples_48k: f64,
}

impl MicOpusEncoder {
    pub fn new(config: MicEncoderConfig) -> Result<Self, MicEncoderError> {
        let input_rate = config.input_sample_rate;
        // Browsers capture between 8 kHz and 384 kHz.
        if !(8_000..=384_000).contains(&input_rate) {
            return Err(MicEncoderError::InvalidSampleRate(input_rate));
        }
        if !BITRATE_RANGE_BPS.contains(&config.bitrate_bps) {
            return Err(MicEncoderError::InvalidBitrate(config.bitrate_bps));
        }
        let resampler = if input_rate == OPUS_SAMPLE_RATE_HZ {
            None
        } else {
            // Fixed output: every call yields exactly one 20 ms Opus frame,
            // so framing and resampling are one step. A 128-tap windowed sinc
            // delays the signal by half its length (64 input samples, 1.5 ms
            // at 44.1 kHz) and keeps the FFT crates out of the worker.
            let ratio = f64::from(OPUS_SAMPLE_RATE_HZ) / f64::from(input_rate);
            Some(
                Async::<f32>::new_sinc(
                    ratio,
                    1.0,
                    &SincInterpolationParameters::new(128, WindowFunction::BlackmanHarris2),
                    OPUS_FRAME_SAMPLES,
                    1,
                    FixedAsync::Output,
                )
                .map_err(|e| MicEncoderError::Resampler(e.to_string()))?,
            )
        };

        // Voice signal keeps libopus in SILK/hybrid, where DTX produces comfort
        // noise on the receiver; in CELT mode DTX concealment fades to silence.
        // In-band FEC stays off: NetEq conceals loss and RED carries redundancy.
        let encoder = Encoder::builder(OPUS_SAMPLE_RATE_HZ, Channels::Mono, Application::Voip)
            .bitrate(Bitrate::Bits(config.bitrate_bps))
            .complexity(MIC_OPUS_COMPLEXITY)
            .signal(Signal::Voice)
            .dtx(config.dtx)
            .build()
            .map_err(|e| MicEncoderError::Opus(e.to_string()))?;

        let pending_capacity = resampler
            .as_ref()
            .map_or(OPUS_FRAME_SAMPLES, |r| r.input_frames_max())
            * 2;
        Ok(Self {
            encoder,
            resampler,
            pending: Vec::with_capacity(pending_capacity),
            frame: vec![0.0; OPUS_FRAME_SAMPLES],
            packet: vec![0; MAX_PACKET_BYTES],
            dtx_gate: DtxGate::default(),
            next_frame: 0,
            input_rate,
            skipped_samples_48k: 0.0,
        })
    }

    /// Change the target bitrate from the next frame on (audio tier change).
    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), MicEncoderError> {
        if !BITRATE_RANGE_BPS.contains(&bitrate_bps) {
            return Err(MicEncoderError::InvalidBitrate(bitrate_bps));
        }
        self.encoder
            .set_bitrate(Bitrate::Bits(bitrate_bps))
            .map_err(|e| MicEncoderError::Opus(e.to_string()))
    }

    /// The capture worklet dropped `input_samples` (at the input rate)
    /// because this encoder fell behind. Nothing is encoded for them, but
    /// the frame index advances by the whole frames they span, so packet
    /// stamps keep following real time. Remainders carry over.
    pub fn skip_input(&mut self, input_samples: usize) {
        self.skipped_samples_48k +=
            input_samples as f64 * f64::from(OPUS_SAMPLE_RATE_HZ) / f64::from(self.input_rate);
        let frames = (self.skipped_samples_48k / OPUS_FRAME_SAMPLES as f64).floor();
        self.skipped_samples_48k -= frames * OPUS_FRAME_SAMPLES as f64;
        self.next_frame += frames as u64;
    }

    /// Input samples needed for the next 20 ms frame.
    fn input_needed(&self) -> usize {
        self.resampler
            .as_ref()
            .map_or(OPUS_FRAME_SAMPLES, |r| r.input_frames_next())
    }

    /// Feed one capture block. Calls `emit(frame_index, packet)` for each
    /// Opus packet to send, in order. The index counts every encoded frame
    /// of this encoder, including the DTX frames that are not sent. Leftover
    /// samples wait for the next block.
    pub fn push(
        &mut self,
        samples: &[f32],
        mut emit: impl FnMut(u64, &[u8]),
    ) -> Result<(), MicEncoderError> {
        self.pending.extend_from_slice(samples);
        loop {
            let needed = self.input_needed();
            if self.pending.len() < needed {
                return Ok(());
            }
            match self.resampler.as_mut() {
                None => self.frame.copy_from_slice(&self.pending[..needed]),
                Some(resampler) => {
                    let input = InterleavedSlice::new(&self.pending[..needed], 1, needed)
                        .map_err(|e| MicEncoderError::Resampler(e.to_string()))?;
                    let mut output =
                        InterleavedSlice::new_mut(&mut self.frame, 1, OPUS_FRAME_SAMPLES)
                            .map_err(|e| MicEncoderError::Resampler(e.to_string()))?;
                    resampler
                        .process_into_buffer(&input, &mut output, None)
                        .map_err(|e| MicEncoderError::Resampler(e.to_string()))?;
                }
            }
            self.pending.drain(..needed);

            let len = self
                .encoder
                .encode_float(&self.frame, &mut self.packet)
                .map_err(|e| MicEncoderError::Opus(e.to_string()))?;
            let frame = self.next_frame;
            self.next_frame += 1;
            if self.dtx_gate.admit(len) {
                emit(frame, &self.packet[..len]);
            }
        }
    }
}

/// Stamps packets for `MediaPacket.timestamp` on the main thread.
///
/// A session's first packet anchors it to the wall clock (`now` minus the
/// frames before it); every later packet is `anchor + frame_index * 20 ms`,
/// whenever it happens to be posted. A new session (unmute, device switch)
/// re-anchors, but never at or before the previous session's last stamp:
/// frame-derived stamps drift from the wall clock by the capture clock's
/// error, and the receiver must never see time go backwards.
#[derive(Debug, Default, Clone, Copy)]
pub struct PacketClock {
    anchor_ms: Option<f64>,
    /// The earliest stamp the next packet may get.
    floor_ms: f64,
}

impl PacketClock {
    /// The next packet starts a new session with frame indices from 0.
    pub fn start_session(&mut self) {
        self.anchor_ms = None;
    }

    /// The stamp for the packet of `frame_index`, sent at `now_ms`.
    pub fn stamp(&mut self, now_ms: f64, frame_index: u64) -> f64 {
        let offset_ms = frame_index as f64 * OPUS_FRAME_MS;
        let anchor = *self
            .anchor_ms
            .get_or_insert_with(|| (now_ms - offset_ms).max(self.floor_ms - offset_ms));
        let stamp = anchor + offset_ms;
        self.floor_ms = self.floor_ms.max(stamp + OPUS_FRAME_MS);
        stamp
    }
}

/// Follows the audio tier's bitrate (written by the camera encoder's
/// quality manager) and says when the encoder needs a `reconfigure`.
#[derive(Debug, Clone, Copy)]
pub struct BitrateFollower {
    applied_bps: u32,
}

impl BitrateFollower {
    /// The encoder was initialized at `initial_bps`.
    pub fn new(initial_bps: u32) -> Self {
        Self {
            applied_bps: initial_bps,
        }
    }

    /// The bitrate to send to the encoder, if the tier's `target_bps` moved
    /// since the last call and is a valid Opus bitrate.
    pub fn poll(&mut self, target_bps: u32) -> Option<u32> {
        if target_bps == self.applied_bps || !BITRATE_RANGE_BPS.contains(&target_bps) {
            return None;
        }
        self.applied_bps = target_bps;
        Some(target_bps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropus::{DecodeMode, Decoder};

    const BLOCK: usize = 128;

    fn config(input_sample_rate: u32, dtx: bool) -> MicEncoderConfig {
        MicEncoderConfig {
            input_sample_rate,
            bitrate_bps: 50_000,
            dtx,
        }
    }

    /// A voice-like signal: a few harmonics with a slow amplitude wobble.
    fn voice(rate: u32, samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|n| {
                let t = n as f32 / rate as f32;
                let envelope = 0.6 + 0.4 * (2.0 * std::f32::consts::PI * 3.0 * t).sin();
                let tone: f32 = [180.0f32, 360.0, 540.0, 900.0]
                    .iter()
                    .enumerate()
                    .map(|(k, f)| (2.0 * std::f32::consts::PI * f * t).sin() / (k as f32 + 1.0))
                    .sum();
                0.25 * envelope * tone
            })
            .collect()
    }

    /// Encode `input` in `block`-sized pushes, collecting the packets.
    fn encode_in_blocks(encoder: &mut MicOpusEncoder, input: &[f32], block: usize) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        for chunk in input.chunks(block) {
            encoder
                .push(chunk, |_frame, packet| packets.push(packet.to_vec()))
                .unwrap();
        }
        packets
    }

    fn decode_all(packets: &[Vec<u8>]) -> Vec<Vec<f32>> {
        let mut decoder = Decoder::new(OPUS_SAMPLE_RATE_HZ, Channels::Mono).unwrap();
        let mut out = vec![0.0f32; 5760];
        packets
            .iter()
            .map(|packet| {
                let n = decoder
                    .decode_float(packet, &mut out, DecodeMode::Normal)
                    .unwrap();
                out[..n].to_vec()
            })
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
    }

    fn encode_indexed(encoder: &mut MicOpusEncoder, input: &[f32]) -> Vec<(u64, Vec<u8>)> {
        let mut packets = Vec::new();
        for chunk in input.chunks(BLOCK) {
            encoder
                .push(chunk, |frame, packet| {
                    packets.push((frame, packet.to_vec()))
                })
                .unwrap();
        }
        packets
    }

    #[test]
    fn uses_the_mic_complexity() {
        let encoder = MicOpusEncoder::new(config(48_000, true)).unwrap();
        assert_eq!(encoder.encoder.complexity(), MIC_OPUS_COMPLEXITY);
        assert!(encoder.encoder.dtx());
        assert_eq!(encoder.encoder.inband_fec(), ropus::InbandFec::Disabled);
    }

    /// Every encoded frame gets the next index, sent or not, so the main
    /// thread can stamp `start + index * 20 ms`, libwebrtc's RTP timestamp.
    #[test]
    fn frame_indices_count_dropped_dtx_frames() {
        let mut encoder = MicOpusEncoder::new(config(48_000, true)).unwrap();
        let mut input = voice(48_000, 48_000);
        input.extend(vec![0.0f32; 96_000]);
        input.extend(voice(48_000, 48_000));
        let packets = encode_indexed(&mut encoder, &input);
        let indices: Vec<u64> = packets.iter().map(|(frame, _)| *frame).collect();
        assert_eq!(&indices[..50], (0..50).collect::<Vec<_>>().as_slice());
        assert!(indices.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            indices.windows(2).any(|pair| pair[1] - pair[0] > 1),
            "DTX skips frames"
        );
        // The final speech second is frames 150..200, all sent.
        let tail = &indices[indices.len() - 50..];
        assert_eq!(tail, (150..200).collect::<Vec<_>>().as_slice());
    }

    /// The capture worklet drops audio when the worker stalls and reports
    /// how many samples it dropped. The frame index must keep advancing
    /// with real time, so the stamps carry the gap.
    #[test]
    fn dropped_capture_advances_the_frame_index() {
        for rate in [48_000u32, 44_100] {
            let mut encoder = MicOpusEncoder::new(config(rate, false)).unwrap();
            let before = encode_indexed(&mut encoder, &voice(rate, rate as usize));
            let last_before = before.last().unwrap().0;
            // Half a second lost, reported in worklet-sized pieces.
            let dropped = rate as usize / 2;
            for _ in 0..dropped / BLOCK {
                encoder.skip_input(BLOCK);
            }
            encoder.skip_input(dropped % BLOCK);
            let after = encode_indexed(&mut encoder, &voice(rate, rate as usize));
            let first_after = after.first().unwrap().0;
            // 25 frames of real time passed between the two packets.
            let jump = first_after - last_before;
            assert!(
                (25..=27).contains(&jump),
                "{rate} Hz: jump of {jump} frames"
            );
            // Contiguous again after the gap.
            for pair in after.windows(2) {
                assert_eq!(pair[1].0, pair[0].0 + 1);
            }
            // Total frames still match total time (1 s + 0.5 s + 1 s = 125).
            let last_after = after.last().unwrap().0;
            assert!((123..=125).contains(&last_after), "{rate} Hz: {last_after}");
        }
    }

    #[test]
    fn frame_indices_restart_with_a_new_encoder() {
        let mut encoder = MicOpusEncoder::new(config(44_100, false)).unwrap();
        let first = encode_indexed(&mut encoder, &voice(44_100, 8_820));
        assert_eq!(first.first().map(|p| p.0), Some(0));
        let mut encoder = MicOpusEncoder::new(config(44_100, false)).unwrap();
        let second = encode_indexed(&mut encoder, &voice(44_100, 8_820));
        assert_eq!(second.first().map(|p| p.0), Some(0));
    }

    /// The quality manager's audio tier reaches the encoder at runtime.
    #[test]
    fn set_bitrate_changes_packet_sizes() {
        let mut encoder = MicOpusEncoder::new(config(48_000, false)).unwrap();
        let speech = voice(48_000, 48_000);
        let high = encode_in_blocks(&mut encoder, &speech, BLOCK);
        encoder.set_bitrate(16_000).unwrap();
        assert_eq!(encoder.encoder.bitrate(), Bitrate::Bits(16_000));
        let low = encode_in_blocks(&mut encoder, &speech, BLOCK);
        let mean = |packets: &[Vec<u8>]| {
            // Skip the first frames while the rate control settles.
            let tail = &packets[10..];
            tail.iter().map(Vec::len).sum::<usize>() as f64 / tail.len() as f64
        };
        let (high, low) = (mean(&high), mean(&low));
        // 16 kbps is about 40 bytes per 20 ms frame; VBR at 50 kbps spends
        // well over twice that on this signal.
        assert!(low < 45.0 && low * 2.0 < high, "high {high} B, low {low} B");
    }

    /// Tier changes written to the shared atomic reach the encoder once.
    #[test]
    fn tier_bitrate_changes_reach_the_encoder() {
        use crate::adaptive_quality_constants::AUDIO_QUALITY_TIERS;
        let tier_bps = |index: usize| AUDIO_QUALITY_TIERS[index].bitrate_kbps * 1000;
        let mut encoder = MicOpusEncoder::new(MicEncoderConfig {
            input_sample_rate: 48_000,
            bitrate_bps: tier_bps(0),
            dtx: true,
        })
        .unwrap();
        let mut follower = BitrateFollower::new(tier_bps(0));
        assert_eq!(follower.poll(tier_bps(0)), None, "no change, no message");
        for index in 1..AUDIO_QUALITY_TIERS.len() {
            let bps = follower.poll(tier_bps(index)).expect("tier change");
            encoder.set_bitrate(bps).unwrap();
            assert_eq!(encoder.encoder.bitrate(), Bitrate::Bits(tier_bps(index)));
            assert_eq!(follower.poll(tier_bps(index)), None);
        }
        let back_up = follower.poll(tier_bps(0)).unwrap();
        encoder.set_bitrate(back_up).unwrap();
        assert_eq!(encoder.encoder.bitrate(), Bitrate::Bits(tier_bps(0)));
        // A garbage value is ignored rather than sent.
        assert_eq!(follower.poll(0), None);
    }

    #[test]
    fn set_bitrate_rejects_out_of_range() {
        let mut encoder = MicOpusEncoder::new(config(48_000, false)).unwrap();
        assert!(encoder.set_bitrate(u32::MAX).is_err());
    }

    #[test]
    fn packet_clock_stamps_frames_not_send_times() {
        let mut clock = PacketClock::default();
        // The first packet anchors the session; later packets ignore `now`,
        // so main-thread jank and startup bursts do not move them.
        assert_eq!(clock.stamp(10_000.0, 0), 10_000.0);
        assert_eq!(clock.stamp(10_090.0, 1), 10_020.0);
        assert_eq!(clock.stamp(10_090.0, 2), 10_040.0);
        assert_eq!(clock.stamp(10_095.0, 9), 10_180.0);
        // A burst after a stall: all on the grid.
        for frame in 10..20u64 {
            assert_eq!(clock.stamp(11_000.0, frame), 10_000.0 + frame as f64 * 20.0);
        }
    }

    #[test]
    fn packet_clock_anchors_on_the_first_packet_index() {
        let mut clock = PacketClock::default();
        // The first sent packet is frame 3 (frames 0..2 were DTX-dropped).
        assert_eq!(clock.stamp(5_060.0, 3), 5_060.0);
        assert_eq!(clock.stamp(5_500.0, 4), 5_080.0);
    }

    /// A new session re-anchors to the wall clock but never stamps at or
    /// before the previous session's last packet, however the capture
    /// clock drifted.
    #[test]
    fn packet_clock_new_session_moves_forward() {
        let mut clock = PacketClock::default();
        clock.stamp(1_000.0, 0);
        assert_eq!(clock.stamp(1_000.0, 499), 10_980.0);
        // Restarted 2 s later: re-anchored to now.
        clock.start_session();
        assert_eq!(clock.stamp(12_980.0, 0), 12_980.0);
        // Restarted with a wall clock behind the drifted stamps.
        clock.start_session();
        assert_eq!(clock.stamp(12_000.0, 0), 13_000.0);
        assert_eq!(clock.stamp(12_000.0, 1), 13_020.0);
    }

    #[test]
    fn rejects_bad_sample_rate() {
        assert!(matches!(
            MicOpusEncoder::new(config(0, false)),
            Err(MicEncoderError::InvalidSampleRate(0))
        ));
    }

    #[test]
    fn input_48k_in_worklet_quanta_yields_one_packet_per_20ms() {
        let mut encoder = MicOpusEncoder::new(config(48_000, false)).unwrap();
        // 1 s of audio in 128-sample quanta: 375 pushes, 50 frames.
        let packets = encode_in_blocks(&mut encoder, &voice(48_000, 48_000), BLOCK);
        assert_eq!(packets.len(), 50);
        for pcm in decode_all(&packets) {
            assert_eq!(pcm.len(), OPUS_FRAME_SAMPLES);
        }
    }

    #[test]
    fn input_44_1k_resamples_to_20ms_frames_at_48k() {
        let mut encoder = MicOpusEncoder::new(config(44_100, false)).unwrap();
        // 1 s at 44.1 kHz is 48000 samples at 48 kHz: 50 frames, give or
        // take the frame the resampler is still filling.
        let packets = encode_in_blocks(&mut encoder, &voice(44_100, 44_100), BLOCK);
        assert!(
            (49..=50).contains(&packets.len()),
            "got {} packets",
            packets.len()
        );
        for pcm in decode_all(&packets) {
            assert_eq!(pcm.len(), OPUS_FRAME_SAMPLES);
        }
    }

    #[test]
    fn partial_blocks_buffer_until_a_frame_is_complete() {
        let mut encoder = MicOpusEncoder::new(config(48_000, false)).unwrap();
        let input = voice(48_000, OPUS_FRAME_SAMPLES * 2);
        let mut count = 0;
        // 959 samples: one short of a frame.
        encoder.push(&input[..959], |_, _| count += 1).unwrap();
        assert_eq!(count, 0);
        encoder.push(&input[959..960], |_, _| count += 1).unwrap();
        assert_eq!(count, 1);
        // Odd sizes straddling the next boundary.
        encoder.push(&input[960..1500], |_, _| count += 1).unwrap();
        assert_eq!(count, 1);
        encoder.push(&input[1500..], |_, _| count += 1).unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn block_size_does_not_change_the_output() {
        let input = voice(44_100, 22_050);
        let mut a = MicOpusEncoder::new(config(44_100, false)).unwrap();
        let mut b = MicOpusEncoder::new(config(44_100, false)).unwrap();
        assert_eq!(
            encode_in_blocks(&mut a, &input, 128),
            encode_in_blocks(&mut b, &input, 333)
        );
    }

    #[test]
    fn packets_decode_to_non_silent_audio_of_the_expected_length() {
        for rate in [48_000, 44_100] {
            let mut encoder = MicOpusEncoder::new(config(rate, true)).unwrap();
            let packets = encode_in_blocks(&mut encoder, &voice(rate, rate as usize), BLOCK);
            let decoded: Vec<f32> = decode_all(&packets).into_iter().flatten().collect();
            assert_eq!(decoded.len(), packets.len() * OPUS_FRAME_SAMPLES);
            // Skip the codec's start-up frames.
            let level = rms(&decoded[OPUS_FRAME_SAMPLES * 5..]);
            assert!(level > 0.05, "{rate} Hz decoded RMS {level}");
        }
    }

    /// A 1 kHz tone captured at 44.1 kHz must still be 1 kHz at 48 kHz.
    #[test]
    fn resampling_keeps_pitch() {
        let rate = 44_100;
        let tone: Vec<f32> = (0..rate)
            .map(|n| 0.5 * (2.0 * std::f32::consts::PI * 1_000.0 * n as f32 / rate as f32).sin())
            .collect();
        let mut encoder = MicOpusEncoder::new(config(rate, false)).unwrap();
        let packets = encode_in_blocks(&mut encoder, &tone, BLOCK);
        let decoded: Vec<f32> = decode_all(&packets).into_iter().flatten().collect();
        // Skip start-up, then count upward zero crossings over 0.5 s.
        let window = &decoded[OPUS_FRAME_SAMPLES * 5..OPUS_FRAME_SAMPLES * 5 + 24_000];
        let crossings = window
            .windows(2)
            .filter(|pair| pair[0] < 0.0 && pair[1] >= 0.0)
            .count();
        assert!(
            (495..=505).contains(&crossings),
            "{crossings} cycles in 0.5 s"
        );
    }

    #[test]
    fn dtx_gate_sends_first_dtx_frame_only() {
        let mut gate = DtxGate::default();
        assert!(gate.admit(80));
        assert!(gate.admit(1), "first DTX frame tells the decoder");
        assert!(!gate.admit(1));
        assert!(!gate.admit(2));
        assert!(gate.admit(12), "noise refresh frames are sent");
        // As in WebRTC, a real packet ends DTX, so the next DTX frame is
        // sent again to put the decoder back into comfort noise.
        assert!(gate.admit(1), "re-entering DTX after a refresh");
        assert!(!gate.admit(1));
        assert!(gate.admit(90));
        assert!(
            gate.admit(1),
            "a new silence period starts with a DTX frame"
        );
    }

    /// Speech, then two seconds of silence, then speech again, with DTX on.
    #[test]
    fn dtx_suppresses_silence_but_keeps_noise_refreshes() {
        let mut encoder = MicOpusEncoder::new(config(48_000, true)).unwrap();
        let speech = voice(48_000, 48_000);
        let silence = vec![0.0f32; 96_000];

        let mut packets = encode_in_blocks(&mut encoder, &speech, BLOCK);
        assert_eq!(packets.len(), 50, "speech is never suppressed");

        let silent = encode_in_blocks(&mut encoder, &silence, BLOCK);
        // 100 frames of silence. libopus waits 200 ms before DTX, then sends
        // one refresh per 400 ms; everything else is dropped.
        assert!(
            silent.len() < 25,
            "only {} of 100 silent frames should be sent",
            silent.len()
        );
        let dtx_frames = silent
            .iter()
            .filter(|p| p.len() <= DTX_PACKET_MAX_BYTES)
            .count();
        assert!(dtx_frames >= 1, "the first DTX frame is sent");
        let refreshes = silent
            .iter()
            .skip_while(|p| p.len() > DTX_PACKET_MAX_BYTES)
            .filter(|p| p.len() > DTX_PACKET_MAX_BYTES)
            .count();
        assert!(refreshes >= 3, "noise refreshes keep coming: {refreshes}");
        // Never two DTX frames in a row: the second would be redundant.
        for pair in silent.windows(2) {
            assert!(
                !(pair[0].len() <= DTX_PACKET_MAX_BYTES && pair[1].len() <= DTX_PACKET_MAX_BYTES)
            );
        }
        packets.extend(silent);

        let resumed = encode_in_blocks(&mut encoder, &speech, BLOCK);
        assert_eq!(resumed.len(), 50, "speech after silence is all sent");
        assert!(resumed.iter().all(|p| p.len() > DTX_PACKET_MAX_BYTES));
    }

    #[test]
    fn dtx_off_sends_every_silent_frame() {
        let mut encoder = MicOpusEncoder::new(config(48_000, false)).unwrap();
        let packets = encode_in_blocks(&mut encoder, &vec![0.0f32; 48_000], BLOCK);
        assert_eq!(packets.len(), 50);
    }
}

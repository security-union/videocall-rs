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
//! The AudioWorklet hands me mono `f32` blocks at the AudioContext's rate
//! (128 frames at a time today). [`MicOpusEncoder::push`] buffers them,
//! resamples to 48 kHz when needed, cuts 20 ms frames and encodes each one
//! with ropus. Nothing here touches the browser, so it is tested natively.

mod constants;

pub use constants::MIC_OPUS_COMPLEXITY;
use ropus::{Application, Bitrate, Channels, Encoder};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Resampler, SincInterpolationParameters, WindowFunction};

/// Opus encode rate. The wire format and NetEq both assume 48 kHz.
pub const OPUS_SAMPLE_RATE_HZ: u32 = 48_000;

/// Samples in one 20 ms frame at 48 kHz.
pub const OPUS_FRAME_SAMPLES: usize = 960;

/// libopus's recommended output buffer size.
const MAX_PACKET_BYTES: usize = 4000;

/// Buffers capture blocks, resamples to 48 kHz and emits Opus packets.
pub struct MicOpusEncoder {
    encoder: Encoder,
    /// `None` when the input is already 48 kHz.
    resampler: Option<Async<f32>>,
    /// Input samples not yet consumed, at the input rate.
    pending: Vec<f32>,
    /// One 20 ms frame at 48 kHz.
    frame: Vec<f32>,
    packet: Vec<u8>,
}

impl MicOpusEncoder {
    /// A VoIP encoder at `bitrate_bps` for audio captured at `input_rate`.
    /// DTX and in-band FEC stay off, as they were with opus-recorder.
    pub fn new(input_rate: u32, bitrate_bps: u32) -> Result<Self, String> {
        // Browsers capture between 8 kHz and 384 kHz.
        if !(8_000..=384_000).contains(&input_rate) {
            return Err(format!("invalid input sample rate {input_rate}"));
        }
        let resampler = if input_rate == OPUS_SAMPLE_RATE_HZ {
            None
        } else {
            // Fixed output: every call yields exactly one 20 ms frame, so
            // framing and resampling are one step. A 128-tap windowed sinc
            // delays the signal by 64 input samples (1.5 ms at 44.1 kHz).
            let ratio = f64::from(OPUS_SAMPLE_RATE_HZ) / f64::from(input_rate);
            let params = SincInterpolationParameters::new(128, WindowFunction::BlackmanHarris2);
            let resampler = Async::<f32>::new_sinc(
                ratio,
                1.0,
                &params,
                OPUS_FRAME_SAMPLES,
                1,
                FixedAsync::Output,
            )
            .map_err(|e| format!("resampler: {e}"))?;
            Some(resampler)
        };
        let encoder = Encoder::builder(OPUS_SAMPLE_RATE_HZ, Channels::Mono, Application::Voip)
            .bitrate(Bitrate::Bits(bitrate_bps))
            .complexity(MIC_OPUS_COMPLEXITY)
            .build()
            .map_err(|e| format!("opus: {e}"))?;
        Ok(Self {
            encoder,
            resampler,
            pending: Vec::with_capacity(OPUS_FRAME_SAMPLES * 2),
            frame: vec![0.0; OPUS_FRAME_SAMPLES],
            packet: vec![0; MAX_PACKET_BYTES],
        })
    }

    /// Input samples needed for the next 20 ms frame.
    fn input_needed(&self) -> usize {
        self.resampler
            .as_ref()
            .map_or(OPUS_FRAME_SAMPLES, |r| r.input_frames_next())
    }

    /// Feeds one capture block and calls `emit` with each finished Opus
    /// packet, in order. Leftover samples wait for the next block.
    pub fn push(&mut self, samples: &[f32], mut emit: impl FnMut(&[u8])) -> Result<(), String> {
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
                        .map_err(|e| format!("resampler: {e}"))?;
                    let mut output =
                        InterleavedSlice::new_mut(&mut self.frame, 1, OPUS_FRAME_SAMPLES)
                            .map_err(|e| format!("resampler: {e}"))?;
                    resampler
                        .process_into_buffer(&input, &mut output, None)
                        .map_err(|e| format!("resampler: {e}"))?;
                }
            }
            self.pending.drain(..needed);
            let len = self
                .encoder
                .encode_float(&self.frame, &mut self.packet)
                .map_err(|e| format!("opus: {e}"))?;
            emit(&self.packet[..len]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropus::{DecodeMode, Decoder};

    /// One AudioWorklet render quantum.
    const BLOCK: usize = 128;

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

    fn encode_in_blocks(encoder: &mut MicOpusEncoder, input: &[f32], block: usize) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        for chunk in input.chunks(block) {
            encoder
                .push(chunk, |packet| packets.push(packet.to_vec()))
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

    #[test]
    fn encoder_settings_match_the_old_worklet() {
        let encoder = MicOpusEncoder::new(48_000, 50_000).unwrap();
        assert_eq!(encoder.encoder.complexity(), MIC_OPUS_COMPLEXITY);
        assert_eq!(encoder.encoder.bitrate(), Bitrate::Bits(50_000));
        assert!(!encoder.encoder.dtx());
        assert_eq!(encoder.encoder.inband_fec(), ropus::InbandFec::Disabled);
    }

    #[test]
    fn rejects_bad_sample_rate() {
        assert!(MicOpusEncoder::new(0, 50_000).is_err());
    }

    #[test]
    fn input_48k_in_worklet_quanta_yields_one_packet_per_20ms() {
        let mut encoder = MicOpusEncoder::new(48_000, 50_000).unwrap();
        // 1 s in 128-sample quanta: 375 pushes, 50 frames.
        let packets = encode_in_blocks(&mut encoder, &voice(48_000, 48_000), BLOCK);
        assert_eq!(packets.len(), 50);
        for pcm in decode_all(&packets) {
            assert_eq!(pcm.len(), OPUS_FRAME_SAMPLES);
        }
    }

    #[test]
    fn input_44_1k_resamples_to_20ms_frames_at_48k() {
        let mut encoder = MicOpusEncoder::new(44_100, 50_000).unwrap();
        // 1 s at 44.1 kHz is 50 frames at 48 kHz, give or take the one the
        // resampler is still filling.
        let packets = encode_in_blocks(&mut encoder, &voice(44_100, 44_100), BLOCK);
        assert!(
            (49..=50).contains(&packets.len()),
            "{} packets",
            packets.len()
        );
        for pcm in decode_all(&packets) {
            assert_eq!(pcm.len(), OPUS_FRAME_SAMPLES);
        }
    }

    #[test]
    fn partial_blocks_buffer_until_a_frame_is_complete() {
        let mut encoder = MicOpusEncoder::new(48_000, 50_000).unwrap();
        let input = voice(48_000, OPUS_FRAME_SAMPLES * 2);
        let mut count = 0;
        encoder.push(&input[..959], |_| count += 1).unwrap();
        assert_eq!(count, 0);
        encoder.push(&input[959..960], |_| count += 1).unwrap();
        assert_eq!(count, 1);
        encoder.push(&input[960..1500], |_| count += 1).unwrap();
        assert_eq!(count, 1);
        encoder.push(&input[1500..], |_| count += 1).unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn block_size_does_not_change_the_output() {
        for rate in [48_000, 44_100] {
            let input = voice(rate, rate as usize / 2);
            let mut a = MicOpusEncoder::new(rate, 50_000).unwrap();
            let mut b = MicOpusEncoder::new(rate, 50_000).unwrap();
            assert_eq!(
                encode_in_blocks(&mut a, &input, BLOCK),
                encode_in_blocks(&mut b, &input, 333),
                "{rate} Hz"
            );
        }
    }

    #[test]
    fn packets_decode_to_non_silent_audio() {
        for rate in [48_000, 44_100] {
            let mut encoder = MicOpusEncoder::new(rate, 50_000).unwrap();
            let packets = encode_in_blocks(&mut encoder, &voice(rate, rate as usize), BLOCK);
            let decoded: Vec<f32> = decode_all(&packets).into_iter().flatten().collect();
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
        let mut encoder = MicOpusEncoder::new(rate, 50_000).unwrap();
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
}

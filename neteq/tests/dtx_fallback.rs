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

//! DTX with a decoder that does not support it (like the browser decoders,
//! whose behaviour on DTX frames is unknown): NetEq must never hand it a DTX
//! frame or an empty payload, and must generate comfort noise itself at the
//! measured noise floor, capped. Runs without the `native` feature.

use neteq::codec::AudioDecoder;
use neteq::neteq::SpeechType;
use neteq::{AudioPacket, NetEq, NetEqConfig, RtpHeader};
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
const PT: u8 = 111;
const SLOT: u32 = 960;

/// A decoder without DTX support. A payload is `[TOC 0x78, level]`: it
/// decodes to 20 ms of noise with RMS `level / 1000`. Being handed a DTX
/// frame or an empty payload is a test failure.
struct LevelDecoder {
    seed: u32,
}

impl AudioDecoder for LevelDecoder {
    fn sample_rate(&self) -> u32 {
        RATE
    }
    fn channels(&self) -> u8 {
        1
    }
    fn decode(&mut self, encoded: &[u8]) -> neteq::Result<Vec<f32>> {
        assert!(
            encoded.len() > 2,
            "a DTX frame or empty payload reached a decoder without DTX support"
        );
        let rms = f32::from(encoded[1]) / 1000.0;
        let amplitude = rms * 3f32.sqrt();
        Ok((0..SLOT)
            .map(|_| {
                self.seed = self
                    .seed
                    .wrapping_mul(1_664_525)
                    .wrapping_add(1_013_904_223);
                ((self.seed >> 8) as f32 / 16_777_216.0 * 2.0 - 1.0) * amplitude
            })
            .collect())
    }
}

fn new_neteq() -> NetEq {
    let mut neteq = NetEq::new(NetEqConfig {
        sample_rate: RATE,
        channels: 1,
        ..NetEqConfig::default()
    })
    .unwrap();
    neteq.register_decoder(PT, Box::new(LevelDecoder { seed: 1 }));
    neteq
}

fn pace(t0: Instant, k: u32) {
    let due = t0 + Duration::from_millis(20 * u64::from(k));
    let now = Instant::now();
    if due > now {
        std::thread::sleep(due - now);
    }
}

/// What a sender transmits in one 20 ms slot.
#[derive(Clone, Copy)]
enum Tx {
    /// A frame with this level (RMS ‰).
    Frame(u8),
    /// A DTX frame (1 byte).
    Dtx,
    /// Nothing.
    None,
}

/// Play `slots` in real time; returns (speech type, RMS) per 10 ms frame.
fn play(slots: &[Tx]) -> Vec<(SpeechType, f32)> {
    play_with(&mut new_neteq(), slots)
}

fn play_with(neteq: &mut NetEq, slots: &[Tx]) -> Vec<(SpeechType, f32)> {
    let (mut seq, t0) = (0u16, Instant::now());
    let mut out = Vec::new();
    for (k, tx) in slots.iter().enumerate() {
        pace(t0, k as u32);
        let payload = match *tx {
            Tx::Frame(level) => Some(vec![0x78, level, 0, 0]),
            Tx::Dtx => Some(vec![0x78]),
            Tx::None => None,
        };
        if let Some(payload) = payload {
            let header = RtpHeader::new(seq, k as u32 * SLOT, 1, PT, false);
            neteq
                .insert_packet(AudioPacket::new(header, payload, RATE, 1, 20))
                .unwrap();
            seq = seq.wrapping_add(1);
        }
        for _ in 0..2 {
            let frame = neteq.get_audio().unwrap();
            let rms = (frame.samples.iter().map(|x| x * x).sum::<f32>()
                / frame.samples.len() as f32)
                .sqrt();
            out.push((frame.speech_type, rms));
        }
    }
    out
}

fn dbfs(rms: f32) -> f32 {
    20.0 * rms.max(1e-9).log10()
}

/// RMS over the comfort-noise frames.
fn comfort_level(out: &[(SpeechType, f32)]) -> (usize, f32) {
    let cng: Vec<f32> = out
        .iter()
        .filter(|o| o.0 == SpeechType::Cng)
        .map(|o| o.1)
        .collect();
    let rms = (cng.iter().map(|r| r * r).sum::<f32>() / cng.len().max(1) as f32).sqrt();
    (cng.len(), dbfs(rms))
}

/// Speech, a few quiet frames, then a libwebrtc-style DTX silence (a DTX
/// frame after every refresh at the quiet level).
fn talk_then_dtx(quiet_frames: usize) -> Vec<Tx> {
    let mut slots = vec![Tx::Frame(250); 60]; // speech, about -12 dBFS
    slots.extend(std::iter::repeat_n(Tx::Frame(1), quiet_frames)); // -60 dBFS
    slots.push(Tx::Dtx);
    for _ in 0..6 {
        slots.extend(std::iter::repeat_n(Tx::None, 19));
        slots.push(Tx::Frame(1)); // refresh at the background level
        slots.push(Tx::Dtx);
    }
    slots
}

#[test]
fn comfort_noise_follows_the_noise_floor_without_codec_support() {
    let out = play(&talk_then_dtx(5));
    let (frames, level) = comfort_level(&out);
    assert!(
        frames >= 150,
        "oracle needs data: {frames} comfort-noise frames"
    );
    assert!(
        (-63.0..=-57.0).contains(&level),
        "comfort noise at {level:.1} dBFS, background -60"
    );
    // After the start-up phase (until its first sample, the delay manager
    // targets its maximum, so the first ~0.5 s conceal while the buffer
    // fills), no concealment through speech and DTX.
    let concealed = out[2 * 60..]
        .iter()
        .filter(|o| o.0 == SpeechType::Expand)
        .count();
    assert_eq!(concealed, 0, "no concealment through speech and DTX");
}

#[test]
fn comfort_noise_counts_as_generated_noise_not_concealment() {
    let mut neteq = new_neteq();
    let out = play_with(&mut neteq, &talk_then_dtx(5));
    let lifetime = neteq.get_statistics().lifetime;
    let count = |kind| out.iter().filter(|o| o.0 == kind).count() as u64 * 480;
    assert!(count(SpeechType::Cng) > 0, "oracle needs data");
    assert_eq!(lifetime.generated_noise_samples, count(SpeechType::Cng));
    assert_eq!(lifetime.concealed_samples, count(SpeechType::Expand));
}

#[test]
fn comfort_noise_is_capped_when_no_quiet_frame_was_decoded() {
    // The talker goes silent straight from speech: the quietest decoded
    // frame is speech, so the comfort noise must not be played at speech
    // level.
    let mut slots = vec![Tx::Frame(250); 60];
    slots.push(Tx::Dtx);
    slots.extend(std::iter::repeat_n(Tx::None, 60));
    let (frames, level) = comfort_level(&play(&slots));
    assert!(
        frames >= 50,
        "oracle needs data: {frames} comfort-noise frames"
    );
    assert!(
        level <= -44.5,
        "comfort noise at {level:.1} dBFS must be capped near -45"
    );
}

#[test]
fn timing_fallback_never_hands_an_empty_payload_to_the_decoder() {
    // A sender without DTX frames: speech, then refreshes only. The decoder
    // panics if NetEq tries to continue comfort noise through it.
    let mut slots = vec![Tx::Frame(250); 60];
    slots.extend(std::iter::repeat_n(Tx::Frame(1), 5));
    for _ in 0..6 {
        slots.extend(std::iter::repeat_n(Tx::None, 20));
        slots.push(Tx::Frame(1));
    }
    let out = play(&slots);
    let (frames, level) = comfort_level(&out);
    assert!(
        frames >= 100,
        "oracle needs data: {frames} comfort-noise frames"
    );
    assert!(
        (-63.0..=-57.0).contains(&level),
        "comfort noise at {level:.1} dBFS"
    );
}

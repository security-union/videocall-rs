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

//! Opus DTX through NetEq, driven by real encoder output: real speech (the
//! repository's `bot/BundyBests2.wav`, 48 kHz mono) over a quiet room-noise
//! floor, silences of that noise alone, encoded with DTX on. Two sender models: libwebrtc's (it transmits the first DTX frame
//! after every non-DTX frame, `WebRtcOpus_Encode`) and one that drops every
//! DTX frame (allowed by RFC 7587), which exercises the timing fallback.
//! Time is real, as in a call: each 20 ms slot inserts what arrives, then
//! pulls two 10 ms frames (the delay manager measures arrivals with the
//! wall clock, so a faster-than-real-time loop would not be a call).

#![cfg(all(feature = "native", not(target_arch = "wasm32")))]

use neteq::codec::NativeOpusDecoder;
use neteq::neteq::SpeechType;
use neteq::{AudioPacket, NetEq, NetEqConfig, RtpHeader};
use ropus::{Application, Bitrate, Channels, Encoder};
use std::time::{Duration, Instant};

/// Sleep until slot `k` (20 ms each) after `t0`.
fn pace(t0: Instant, k: usize) {
    let due = t0 + Duration::from_millis(20 * k as u64);
    let now = Instant::now();
    if due > now {
        std::thread::sleep(due - now);
    }
}

const RATE: u32 = 48_000;
const PT: u8 = 111;
const SLOT: usize = 960; // 20 ms at 48 kHz
const NOISE_AMP: f32 = 0.003; // background noise, about -50 dBFS RMS
/// Talkspurts are taken from here on (continuous speech in the recording).
const SPEECH_START_S: usize = 10;

/// The repository's speech recording (48 kHz, mono, 16 bit).
fn speech() -> Vec<f32> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../bot/BundyBests2.wav");
    let mut reader = hound::WavReader::open(path).expect("speech recording bot/BundyBests2.wav");
    let spec = reader.spec();
    assert_eq!(
        (spec.sample_rate, spec.channels),
        (RATE, 1),
        "48 kHz mono expected"
    );
    reader
        .samples::<i16>()
        .map(|s| f32::from(s.unwrap()) / 32_768.0)
        .collect()
}

struct Encoded {
    seq: u16,
    ts: u32,
    payload: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq)]
enum Sender {
    /// libwebrtc: every non-DTX frame, plus the first DTX frame after one.
    Libwebrtc,
    /// Drops every DTX frame.
    DropAll,
}

/// Encode `segments` of (voiced, milliseconds) in 20 ms frames: voiced
/// segments continue the speech recording, every frame carries the room
/// noise.
fn encode(segments: &[(bool, u32)]) -> Vec<Encoded> {
    let speech = speech();
    let mut speech_at = SPEECH_START_S * RATE as usize;
    let mut enc = Encoder::builder(RATE, Channels::Mono, Application::Voip)
        .dtx(true)
        .bitrate(Bitrate::Bits(32_000))
        .build()
        .unwrap();
    let mut seed = 0x1234_5678u32;
    let mut noise = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / 16_777_216.0 * 2.0 - 1.0
    };
    let mut out = Vec::new();
    let mut n = 0u32;
    for &(voiced, ms) in segments {
        for _ in 0..ms / 20 {
            let mut pcm = [0i16; SLOT];
            for s in pcm.iter_mut() {
                let v = if voiced {
                    let v = speech[speech_at % speech.len()];
                    speech_at += 1;
                    v
                } else {
                    0.0
                };
                *s = ((v + NOISE_AMP * 1.7 * noise()).clamp(-1.0, 1.0) * 32_767.0) as i16;
            }
            let mut buf = [0u8; 1500];
            let len = enc.encode(&pcm, &mut buf).unwrap();
            out.push(Encoded {
                seq: n as u16,
                ts: n * SLOT as u32,
                payload: buf[..len].to_vec(),
            });
            n += 1;
        }
    }
    out
}

/// The RTP sequence number each encoded frame is sent with, `None` if the
/// sender drops it. Only transmitted packets consume sequence numbers.
fn transmitted(frames: &[Encoded], sender: Sender) -> Vec<Option<u16>> {
    let mut prev_dtx = false;
    let mut seq = 0u16;
    frames
        .iter()
        .map(|f| {
            let dtx = f.payload.len() <= 2;
            let send = match sender {
                Sender::Libwebrtc => !dtx || !prev_dtx,
                Sender::DropAll => !dtx,
            };
            prev_dtx = dtx;
            send.then(|| {
                seq = seq.wrapping_add(1);
                seq
            })
        })
        .collect()
}

fn new_neteq() -> NetEq {
    let mut neteq = NetEq::new(NetEqConfig {
        sample_rate: RATE,
        channels: 1,
        ..NetEqConfig::default()
    })
    .unwrap();
    neteq.register_decoder(PT, Box::new(NativeOpusDecoder::new(RATE, 1).unwrap()));
    neteq
}

fn packet(seq: u16, ts: u32, payload: Vec<u8>) -> AudioPacket {
    // Declared as 20 ms, as a simple caller would: the duration that
    // matters is read from the Opus TOC.
    AudioPacket::new(RtpHeader::new(seq, ts, 1, PT, false), payload, RATE, 1, 20)
}

/// One output frame: its speech type and RMS.
struct Out {
    kind: SpeechType,
    rms: f32,
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

fn dbfs(r: f32) -> f32 {
    20.0 * r.max(1e-9).log10()
}

/// Play `frames` (sent where `send[i]`) slot by slot; returns two output
/// frames per slot.
fn play(neteq: &mut NetEq, frames: &[Encoded], send: &[Option<u16>]) -> Vec<Out> {
    let mut outs = Vec::new();
    let t0 = Instant::now();
    for (k, (f, s)) in frames.iter().zip(send).enumerate() {
        pace(t0, k);
        if let Some(seq) = *s {
            neteq
                .insert_packet(packet(seq, f.ts, f.payload.clone()))
                .unwrap();
        }
        for _ in 0..2 {
            let frame = neteq.get_audio().unwrap();
            outs.push(Out {
                kind: frame.speech_type,
                rms: rms(&frame.samples),
            });
        }
    }
    outs
}

fn count(outs: &[Out], kind: SpeechType) -> usize {
    outs.iter().filter(|o| o.kind == kind).count()
}

/// From the first comfort-noise frame to the last: no concealment, and the
/// comfort noise follows the real background (-50 dBFS in) without
/// pumping — every 200 ms of it within a few dB, never digital silence.
fn assert_comfort_noise(outs: &[Out], what: &str) {
    let first = outs
        .iter()
        .position(|o| o.kind == SpeechType::Cng)
        .expect("comfort noise played");
    let last = outs
        .iter()
        .rposition(|o| o.kind == SpeechType::Cng)
        .unwrap();
    let silence = &outs[first..=last];
    assert_eq!(
        count(silence, SpeechType::Expand),
        0,
        "{what}: no concealment during a DTX silence"
    );
    let cng: Vec<f32> = silence
        .iter()
        .filter(|o| o.kind == SpeechType::Cng)
        .map(|o| o.rms)
        .collect();
    assert!(
        cng.len() >= 150,
        "{what}: oracle needs data: {} comfort-noise frames",
        cng.len()
    );
    for (i, w) in cng.chunks(20).enumerate() {
        let level = dbfs(rms(w));
        assert!(
            (-66.0..=-44.0).contains(&level),
            "{what}: window {i} at {level:.1} dBFS"
        );
    }
}

#[test]
fn dtx_silence_plays_codec_comfort_noise_libwebrtc_sender() {
    let frames = encode(&[(true, 1000), (false, 3000), (true, 1000)]);
    let send = transmitted(&frames, Sender::Libwebrtc);
    let outs = play(&mut new_neteq(), &frames, &send);
    assert_comfort_noise(&outs, "libwebrtc sender");
}

#[test]
fn dtx_silence_plays_codec_comfort_noise_drop_all_sender() {
    // Without DTX frames the receiver cannot tell silence from loss until a
    // comfort-noise refresh arrives (contiguous sequence number, timestamp
    // jump): from then on the silence is comfort noise, never concealment.
    let frames = encode(&[(true, 1000), (false, 3000), (true, 1000)]);
    let send = transmitted(&frames, Sender::DropAll);
    let outs = play(&mut new_neteq(), &frames, &send);
    let mut after_refresh = 0;
    for slot in 1000 / 20..4000 / 20 {
        // The newest packet sent so far, and whether it followed a gap.
        let Some(last) = (0..=slot).rev().find(|&k| send[k].is_some()) else {
            continue;
        };
        let refresh = last > 0 && send[last - 1].is_none();
        // Once the refresh has been played (two slots of buffer), the rest
        // of the gap must be comfort noise.
        if refresh && slot >= last + 2 {
            for o in &outs[2 * slot..2 * slot + 2] {
                assert_ne!(
                    o.kind,
                    SpeechType::Expand,
                    "concealment at slot {slot} after a refresh"
                );
                after_refresh += 1;
            }
        }
    }
    assert!(
        after_refresh >= 50,
        "oracle needs data: {after_refresh} frames after refreshes"
    );
}

#[test]
fn speech_after_silence_waits_for_the_buffer_then_plays_without_concealment() {
    let frames = encode(&[(true, 600), (false, 2000), (true, 1000)]);
    let send = transmitted(&frames, Sender::Libwebrtc);
    let outs = play(&mut new_neteq(), &frames, &send);
    // The second talkspurt: no concealment once it has started, and it
    // starts within the pre-roll (half the 80 ms start target ≈ 40 ms, plus
    // the encoder's ramp-in), not seconds late.
    let resumed = &outs[2600 / 10..];
    let first_speech = resumed
        .iter()
        .position(|o| o.kind == SpeechType::Normal && o.rms > 0.01)
        .expect("the second talkspurt plays");
    assert!(
        first_speech <= 12,
        "speech resumed after {first_speech} frames"
    );
    assert_eq!(count(&resumed[first_speech..], SpeechType::Expand), 0);
}

#[test]
fn comfort_noise_times_out_into_concealment_without_packets() {
    let frames = encode(&[(true, 600), (false, 600)]);
    let send = transmitted(&frames, Sender::Libwebrtc);
    let mut neteq = new_neteq();
    let mut kinds: Vec<SpeechType> = play(&mut neteq, &frames, &send)
        .into_iter()
        .map(|o| o.kind)
        .collect();
    // The sender goes away: 3 s without a single packet (generous: playout
    // still lags the input by the start-up buffering).
    let stopped = kinds.len();
    kinds.extend((0..300).map(|_| neteq.get_audio().unwrap().speech_type));
    // Comfort noise runs until 1 s after the last decoded packet (the DTX
    // frame, itself played as comfort noise), then concealment takes over.
    let first_expand = stopped
        + kinds[stopped..]
            .iter()
            .position(|k| *k == SpeechType::Expand)
            .expect("concealment after the timeout");
    let run = kinds[..first_expand]
        .iter()
        .rev()
        .take_while(|k| **k == SpeechType::Cng)
        .count();
    assert!(
        (100..=104).contains(&run),
        "comfort noise for {run} frames before concealment, 1 s expected"
    );
}

#[test]
fn flush_ends_dtx() {
    let frames = encode(&[(true, 600), (false, 1000)]);
    let send = transmitted(&frames, Sender::Libwebrtc);
    let mut neteq = new_neteq();
    let outs = play(&mut neteq, &frames, &send);
    assert_eq!(outs.last().map(|o| o.kind), Some(SpeechType::Cng));
    neteq.flush();
    assert_eq!(neteq.get_audio().unwrap().speech_type, SpeechType::Expand);
}

#[test]
fn a_duplicate_refresh_does_not_end_dtx() {
    // Deterministic: 2 s of speech packets (the buffer settles), then a
    // refresh after a 400 ms gap (timing fallback: DTX), then the same
    // refresh again.
    let frames = encode(&[(true, 2000)]);
    let mut neteq = new_neteq();
    let t0 = Instant::now();
    let mut k = 0;
    for (i, f) in frames.iter().enumerate() {
        pace(t0, k);
        neteq
            .insert_packet(packet(i as u16, f.ts, f.payload.clone()))
            .unwrap();
        neteq.get_audio().unwrap();
        neteq.get_audio().unwrap();
        k += 1;
    }
    let seq = frames.len() as u16;
    let ts = frames.len() as u32 * SLOT as u32 + 20 * SLOT as u32; // 400 ms later
    let refresh = frames[5].payload.clone();
    for _ in 0..20 {
        pace(t0, k);
        neteq.get_audio().unwrap();
        neteq.get_audio().unwrap();
        k += 1;
    }
    let refresh = {
        // A real comfort-noise refresh from the encoder's silence.
        let quiet = encode(&[(true, 200), (false, 1500)]);
        quiet
            .iter()
            .skip(20)
            .find(|f| f.payload.len() > 2)
            .map(|f| f.payload.clone())
            .unwrap_or(refresh)
    };
    neteq
        .insert_packet(packet(seq, ts, refresh.clone()))
        .unwrap();
    for _ in 0..3 {
        pace(t0, k);
        neteq.get_audio().unwrap();
        neteq.get_audio().unwrap();
        k += 1;
    }
    neteq.insert_packet(packet(seq, ts, refresh)).unwrap(); // the duplicate
    let mut kinds = Vec::new();
    for _ in 0..15 {
        pace(t0, k);
        kinds.push(neteq.get_audio().unwrap().speech_type);
        kinds.push(neteq.get_audio().unwrap().speech_type);
        k += 1;
    }
    assert_eq!(count_kinds(&kinds, SpeechType::Expand), 0, "{kinds:?}");
    assert!(count_kinds(&kinds, SpeechType::Cng) >= 20, "{kinds:?}");
}

fn count_kinds(kinds: &[SpeechType], kind: SpeechType) -> usize {
    kinds.iter().filter(|k| **k == kind).count()
}

/// Timestamp conventions a caller may use.
#[derive(Clone, Copy, Debug)]
enum Stamps {
    /// RTP units (samples): frame k at k × 960.
    Samples,
    /// Every timestamp `u32::MAX`, as videocall-client's browser path
    /// delivers today (milliseconds since 1970 cast `as u32` saturate).
    Saturated,
}

/// RED recovery exactly as videocall-client inserts it
/// (`neteq_audio_decoder.rs`): frame 10 is lost; packet 11 carries it as
/// redundancy, so the recovered frame (sequence 10, timestamp of packet 11
/// minus `OPUS_FRAME_DURATION_MS` = 20) is inserted first, then the primary.
/// Then the stream stops, so nothing later can correct a wrong state: what
/// follows is loss, never comfort noise — the recovered frame must not be
/// taken for the end of a DTX silence.
fn red_recovery_is_not_dtx(stamps: Stamps) {
    let frames = encode(&[(true, 600)]);
    let ts_of = |k: usize| match stamps {
        Stamps::Samples => k as u32 * SLOT as u32,
        Stamps::Saturated => u32::MAX,
    };
    let mut neteq = new_neteq();
    let mut kinds = Vec::new();
    let t0 = Instant::now();
    for k in 0..30 {
        pace(t0, k);
        match k {
            10 => {} // lost
            11 => {
                let recovered_ts = ts_of(11).saturating_sub(20);
                neteq
                    .insert_packet(packet(10, recovered_ts, frames[10].payload.clone()))
                    .unwrap();
                neteq
                    .insert_packet(packet(11, ts_of(11), frames[11].payload.clone()))
                    .unwrap();
            }
            k if k < 11 => neteq
                .insert_packet(packet(k as u16, ts_of(k), frames[k].payload.clone()))
                .unwrap(),
            _ => {} // the sender stops
        }
        kinds.push(neteq.get_audio().unwrap().speech_type);
        kinds.push(neteq.get_audio().unwrap().speech_type);
    }
    assert_eq!(
        count_kinds(&kinds, SpeechType::Cng),
        0,
        "{stamps:?}: {kinds:?}"
    );
    assert!(
        count_kinds(&kinds, SpeechType::Expand) > 0,
        "{stamps:?}: the loss is concealed"
    );
}

#[test]
fn a_red_recovered_frame_does_not_start_dtx_rtp_timestamps() {
    red_recovery_is_not_dtx(Stamps::Samples);
}

#[test]
fn a_red_recovered_frame_does_not_start_dtx_saturated_timestamps() {
    red_recovery_is_not_dtx(Stamps::Saturated);
}

#[test]
fn a_frame_size_switch_does_not_start_dtx() {
    // 20 ms packets, then 40 ms packets from a second encoder, with
    // contiguous sequence numbers and timestamps: the timestamp step grows
    // with the packet duration (read from the TOC), which is not DTX.
    let frames = encode(&[(true, 400)]);
    let mut enc40 = Encoder::builder(RATE, Channels::Mono, Application::Voip)
        .bitrate(Bitrate::Bits(32_000))
        .build()
        .unwrap();
    let mut neteq = new_neteq();
    for f in &frames {
        neteq
            .insert_packet(packet(f.seq, f.ts, f.payload.clone()))
            .unwrap();
    }
    let (mut seq, mut ts) = (frames.len() as u16, frames.len() as u32 * SLOT as u32);
    for k in 0..10 {
        let pcm: Vec<i16> = (0..2 * SLOT)
            .map(|i| {
                (0.3 * (2.0 * std::f32::consts::PI * 220.0 * (k * 2 * SLOT + i) as f32
                    / RATE as f32)
                    .sin()
                    * 32_767.0) as i16
            })
            .collect();
        let mut buf = [0u8; 1500];
        let len = enc40.encode(&pcm, &mut buf).unwrap();
        neteq
            .insert_packet(packet(seq, ts, buf[..len].to_vec()))
            .unwrap();
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(2 * SLOT as u32);
    }
    // Drain everything, then starve: loss, not DTX — no comfort noise.
    let kinds: Vec<SpeechType> = (0..140)
        .map(|_| neteq.get_audio().unwrap().speech_type)
        .collect();
    assert_eq!(count_kinds(&kinds, SpeechType::Cng), 0, "{kinds:?}");
}

#[test]
fn a_90_second_gap_does_not_overflow_the_delay_manager() {
    let frames = encode(&[(true, 200)]);
    let mut neteq = new_neteq();
    let f = &frames[0];
    neteq
        .insert_packet(packet(0, 0, f.payload.clone()))
        .unwrap();
    neteq.get_audio().unwrap();
    // 95 s later in RTP time (an on-hold or muted sender), next sequence.
    neteq
        .insert_packet(packet(1, 95 * RATE, f.payload.clone()))
        .unwrap();
    neteq.get_audio().unwrap();
    assert!(neteq.target_delay_ms() <= 2000);
}

/// Play with every packet delayed by 0-60 ms (deterministic pseudo-
/// random). Returns the output, and per output frame whether the newest
/// packet that had arrived by then was a DTX frame.
fn play_jittered(frames: &[Encoded], send: &[Option<u16>]) -> (Vec<Out>, Vec<bool>) {
    let mut neteq = new_neteq();
    let mut seed = 99u32;
    let mut delay_slots = || {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        ((seed >> 16) % 4) as usize // 0, 20, 40 or 60 ms
    };
    let mut arrivals: Vec<(usize, usize)> = send
        .iter()
        .enumerate()
        .filter(|(_, s)| s.is_some())
        .map(|(k, _)| (k + delay_slots(), k))
        .collect();
    arrivals.sort();
    let (mut outs, mut in_dtx_gap) = (Vec::new(), Vec::new());
    let mut newest: Option<usize> = None;
    let t0 = Instant::now();
    let mut next = 0;
    for slot in 0..frames.len() + 4 {
        pace(t0, slot);
        while next < arrivals.len() && arrivals[next].0 == slot {
            let k = arrivals[next].1;
            let f = &frames[k];
            neteq
                .insert_packet(packet(send[k].unwrap(), f.ts, f.payload.clone()))
                .unwrap();
            if newest.is_none_or(|n| k > n) {
                newest = Some(k);
            }
            next += 1;
        }
        let gap = newest.is_some_and(|n| frames[n].payload.len() <= 2);
        for _ in 0..2 {
            let frame = neteq.get_audio().unwrap();
            outs.push(Out {
                kind: frame.speech_type,
                rms: rms(&frame.samples),
            });
            in_dtx_gap.push(gap);
        }
    }
    (outs, in_dtx_gap)
}

#[test]
fn dtx_silence_under_jitter_is_comfort_noise() {
    // Once the newest arrived packet is a DTX frame, the sender is silent on
    // purpose: an empty buffer is comfort noise, never concealment — on a
    // jittery link too. (Underruns at the start of activity after a
    // silence are the pre-roll's trade-off, not DTX handling.)
    let frames = encode(&[(true, 1500), (false, 2500), (true, 800)]);
    let send = transmitted(&frames, Sender::Libwebrtc);
    let (outs, in_gap) = play_jittered(&frames, &send);
    let gap_frames: Vec<&Out> = outs
        .iter()
        .zip(&in_gap)
        .filter(|(_, g)| **g)
        .map(|(o, _)| o)
        .collect();
    assert!(
        gap_frames.len() >= 100,
        "oracle needs data: {} frames in DTX gaps",
        gap_frames.len()
    );
    let concealed = gap_frames
        .iter()
        .filter(|o| o.kind == SpeechType::Expand)
        .count();
    assert_eq!(
        concealed, 0,
        "concealment inside DTX gaps on a jittery link"
    );
}

/// The unedited recording (12 s from its start, with its natural pauses) and
/// a libwebrtc sender: inside every DTX gap the output is comfort noise,
/// never concealment, and speech is never concealed after start-up.
#[test]
fn natural_speech_pauses_are_comfort_noise() {
    let speech = speech();
    let mut enc = Encoder::builder(RATE, Channels::Mono, Application::Voip)
        .dtx(true)
        .bitrate(Bitrate::Bits(32_000))
        .build()
        .unwrap();
    let frames: Vec<Encoded> = speech[..12 * RATE as usize]
        .chunks_exact(SLOT)
        .enumerate()
        .map(|(n, chunk)| {
            let pcm: Vec<i16> = chunk.iter().map(|v| (v * 32_767.0) as i16).collect();
            let mut buf = [0u8; 1500];
            let len = enc.encode(&pcm, &mut buf).unwrap();
            Encoded {
                seq: n as u16,
                ts: n as u32 * SLOT as u32,
                payload: buf[..len].to_vec(),
            }
        })
        .collect();
    let send = transmitted(&frames, Sender::Libwebrtc);
    let dtx_frames = frames.iter().filter(|f| f.payload.len() <= 2).count();
    assert!(
        dtx_frames >= 25,
        "oracle needs data: the encoder went into DTX for {dtx_frames} frames"
    );
    let (outs, in_gap) = {
        let mut neteq = new_neteq();
        let t0 = Instant::now();
        let (mut outs, mut in_gap) = (Vec::new(), Vec::new());
        let mut newest_dtx = false;
        for (k, (f, s)) in frames.iter().zip(&send).enumerate() {
            pace(t0, k);
            if let Some(seq) = *s {
                neteq
                    .insert_packet(packet(seq, f.ts, f.payload.clone()))
                    .unwrap();
                newest_dtx = f.payload.len() <= 2;
            }
            for _ in 0..2 {
                let frame = neteq.get_audio().unwrap();
                outs.push(Out {
                    kind: frame.speech_type,
                    rms: rms(&frame.samples),
                });
                in_gap.push(newest_dtx);
            }
        }
        (outs, in_gap)
    };
    // From 600 ms on: the first ~0.5 s are the delay manager's start-up
    // (its initial maximum target), not DTX handling.
    let (outs, in_gap) = (&outs[2 * 30..], &in_gap[2 * 30..]);
    let gap: Vec<&Out> = outs
        .iter()
        .zip(in_gap)
        .filter(|(_, g)| **g)
        .map(|(o, _)| o)
        .collect();
    assert!(
        gap.len() >= 40,
        "oracle needs data: {} frames in DTX gaps",
        gap.len()
    );
    assert_eq!(
        gap.iter().filter(|o| o.kind == SpeechType::Expand).count(),
        0,
        "concealment in a DTX gap"
    );
    assert_eq!(
        outs.iter().filter(|o| o.kind == SpeechType::Expand).count(),
        0,
        "speech concealed"
    );
}

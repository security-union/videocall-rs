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

//! Sender to receiver with Opus DTX on: the microphone encoder's packets,
//! numbered and stamped the way `MicrophoneEncoder` does (sequence for sent
//! packets, `PacketClock` stamps from frame indices), go through the
//! receiver's `RtpTimestampMapper` into NetEq. Checks that the RTP timeline
//! is frame-exact however the main thread posts packets (steady, jittered,
//! or a startup burst), that DTX gaps play as comfort noise, that no packet
//! is thrown away as too old, and that speech after silence plays promptly.
//!
//! Native only: run with `cargo test -p videocall-client --test mic_dtx_neteq`.
#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use neteq::api::clock::{Clock, SimulatedClock};
use neteq::api::{
    AudioFrame, Config, NetEq, OutputSpeechType, RtpHeader, RtpPacketInfo, SdpAudioFormat,
};
use videocall_client::decode::rtp_timestamp::RtpTimestampMapper;
use videocall_client::encode::mic_opus::{
    MicEncoderConfig, MicOpusEncoder, PacketClock, DTX_PACKET_MAX_BYTES, OPUS_FRAME_SAMPLES,
};

const OPUS_PT: u8 = 111;
const FRAME_MS: i64 = 20;
const NETWORK_DELAY_MS: i64 = 40;
/// Sender wall clock at test time 0, like `SystemTime::now()` in ms.
const SENDER_EPOCH_MS: f64 = 1.7e12;
/// Packets the worker had ready before the main thread got to them.
const BURST_PACKETS: usize = 25;

const SPEECH_FRAMES: usize = 50; // 1 s
const SILENCE_FRAMES: usize = 150; // 3 s

fn voice_frame(index: usize) -> Vec<f32> {
    (0..OPUS_FRAME_SAMPLES)
        .map(|n| {
            let t = (index * OPUS_FRAME_SAMPLES + n) as f32 / 48_000.0;
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

/// Low-level background noise (about -60 dBFS), deterministic.
fn noise_frame(state: &mut u32) -> Vec<f32> {
    (0..OPUS_FRAME_SAMPLES)
        .map(|_| {
            *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((*state >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.002
        })
        .collect()
}

/// How the main thread gets around to posting the worker's packets.
#[derive(Debug, Clone, Copy)]
enum Posting {
    /// As soon as each frame is encoded.
    Steady,
    /// Main-thread jank: each post up to 60 ms late.
    Jittered,
    /// The first packets all posted at once, as after a slow startup.
    StartupBurst,
}

struct SentPacket {
    /// Encoder frame index; frame 0 was captured at test time 0.
    frame: u64,
    /// When the main thread posted it, ms of test time.
    posted_ms: i64,
    sequence: u64,
    stamp_ms: f64,
    payload: Vec<u8>,
}

/// Run the sender: speech, background noise, speech. Sequence numbers
/// advance only for packets that are sent, and stamps come from the frame
/// index, as in `MicrophoneEncoder`.
fn send(posting: Posting) -> Vec<SentPacket> {
    let mut encoder = MicOpusEncoder::new(MicEncoderConfig {
        input_sample_rate: 48_000,
        bitrate_bps: 50_000,
        dtx: true,
    })
    .unwrap();
    let mut noise_state = 1u32;
    let mut jitter_state = 7u32;
    let mut clock = PacketClock::default();
    let mut packets: Vec<SentPacket> = Vec::new();
    let mut sequence = 0u64;
    for index in 0..SPEECH_FRAMES * 2 + SILENCE_FRAMES {
        let silent = (SPEECH_FRAMES..SPEECH_FRAMES + SILENCE_FRAMES).contains(&index);
        let pcm = if silent {
            noise_frame(&mut noise_state)
        } else {
            voice_frame(index)
        };
        encoder
            .push(&pcm, |frame, payload| {
                // The frame is complete once its 20 ms are captured.
                let ready_ms = (frame as i64 + 1) * FRAME_MS;
                let posted_ms = match posting {
                    Posting::Steady => ready_ms,
                    Posting::Jittered => {
                        jitter_state = jitter_state
                            .wrapping_mul(1_664_525)
                            .wrapping_add(1_013_904_223);
                        ready_ms + i64::from(jitter_state >> 26)
                    }
                    Posting::StartupBurst if packets.len() < BURST_PACKETS => {
                        BURST_PACKETS as i64 * FRAME_MS + 200
                    }
                    Posting::StartupBurst => ready_ms,
                };
                // The main thread handles messages in order.
                let posted_ms = packets
                    .last()
                    .map_or(posted_ms, |last| posted_ms.max(last.posted_ms));
                packets.push(SentPacket {
                    frame,
                    posted_ms,
                    sequence,
                    stamp_ms: clock.stamp(SENDER_EPOCH_MS + posted_ms as f64, frame),
                    payload: payload.to_vec(),
                });
                sequence += 1;
            })
            .unwrap();
    }
    packets
}

struct Playout {
    cng_frames: usize,
    speech_frames_after_resume: usize,
    /// Test time of the first speech frame after the silence.
    first_resumed_speech_ms: Option<i64>,
    /// Test time the first packet after the silence arrived.
    resume_arrival_ms: i64,
    packets_discarded: u64,
    target_delay_ms: i32,
}

/// Deliver `packets` over a constant-delay network into NetEq, checking
/// each packet's RTP timestamp, and record what NetEq plays.
fn receive(packets: &[SentPacket]) -> Playout {
    let total_frames = (SPEECH_FRAMES * 2 + SILENCE_FRAMES) as u64;
    let resume_frame = (SPEECH_FRAMES + SILENCE_FRAMES) as u64;
    let resume_arrival_ms = packets
        .iter()
        .find(|packet| packet.frame >= resume_frame)
        .expect("speech after silence was sent")
        .posted_ms
        + NETWORK_DELAY_MS;

    let clock = SimulatedClock::new(1_000_000);
    let start_ms = clock.time_ms();
    let mut neteq = NetEq::builder(Config {
        sample_rate_hz: 48_000,
        ..Config::default()
    })
    .clock(Arc::new(clock.clone()) as Arc<dyn Clock>)
    .build();
    assert!(
        neteq.register_payload_type(i32::from(OPUS_PT), &SdpAudioFormat::new("opus", 48_000, 2))
    );

    let mut mapper = RtpTimestampMapper::new();
    let mut frame = AudioFrame::new();
    let mut next = 0;
    let mut playout = Playout {
        cng_frames: 0,
        speech_frames_after_resume: 0,
        first_resumed_speech_ms: None,
        resume_arrival_ms,
        packets_discarded: 0,
        target_delay_ms: 0,
    };
    let end_ms = total_frames as i64 * FRAME_MS + 1_000;

    while clock.time_ms() - start_ms < end_ms {
        let now_ms = clock.time_ms() - start_ms;
        while next < packets.len() && packets[next].posted_ms + NETWORK_DELAY_MS <= now_ms {
            let packet = &packets[next];
            let mapping = mapper
                .map(packet.sequence, packet.stamp_ms, &packet.payload)
                .expect("in-order packets always map");
            // libwebrtc's RTP timestamp: 960 samples per encoded frame,
            // across every DTX gap, whenever the packet was posted.
            assert_eq!(
                mapping.timestamp,
                (packet.frame * 960) as u32,
                "seq {} frame {} posted at {} ms",
                packet.sequence,
                packet.frame,
                packet.posted_ms
            );
            assert_eq!(mapping.sequence_number, packet.sequence as u16);
            let header = RtpHeader {
                marker_bit: false,
                payload_type: OPUS_PT,
                sequence_number: mapping.sequence_number,
                timestamp: mapping.timestamp,
                ssrc: 1,
            };
            neteq
                .insert_packet(
                    &header,
                    &packet.payload,
                    RtpPacketInfo::new(&header, Some(clock.current_time_us())),
                )
                .unwrap();
            next += 1;
        }

        neteq.get_audio(&mut frame, None).unwrap();
        let after_resume = now_ms >= resume_arrival_ms;
        match frame.speech_type {
            OutputSpeechType::Cng | OutputSpeechType::PlcCng if !after_resume => {
                playout.cng_frames += 1
            }
            OutputSpeechType::NormalSpeech if after_resume => {
                playout.first_resumed_speech_ms.get_or_insert(now_ms);
                playout.speech_frames_after_resume += 1;
            }
            _ => {}
        }
        clock.advance_time_ms(10);
    }

    assert_eq!(next, packets.len(), "every packet was inserted");
    let lifetime = neteq.lifetime_statistics();
    playout.packets_discarded = lifetime.packets_discarded;
    playout.target_delay_ms = neteq.target_delay_ms();
    eprintln!(
        "sent {} packets; cng frames {}; resumed speech frames {}; resumed {:?} ms \
         after arrival; discarded {}; concealed {}; target delay {} ms",
        packets.len(),
        playout.cng_frames,
        playout.speech_frames_after_resume,
        playout
            .first_resumed_speech_ms
            .map(|ms| ms - resume_arrival_ms),
        lifetime.packets_discarded,
        lifetime.concealed_samples,
        playout.target_delay_ms,
    );
    playout
}

fn check_dtx_was_used(packets: &[SentPacket]) {
    let total_frames = SPEECH_FRAMES * 2 + SILENCE_FRAMES;
    assert!(
        packets.len() < total_frames - SILENCE_FRAMES / 2,
        "DTX should drop most silent frames: sent {} of {total_frames}",
        packets.len()
    );
    assert!(packets
        .iter()
        .any(|packet| packet.payload.len() <= DTX_PACKET_MAX_BYTES));
}

fn check_playout(playout: &Playout) {
    assert_eq!(playout.packets_discarded, 0, "no packet arrived too old");
    assert!(
        playout.cng_frames > 100,
        "silence plays as comfort noise: {}",
        playout.cng_frames
    );
    // 1 s of speech after the silence is 100 frames of 10 ms.
    assert!(
        playout.speech_frames_after_resume >= 90,
        "speech after DTX plays: {} frames",
        playout.speech_frames_after_resume
    );
}

#[test]
fn dtx_stream_plays_through_mapper_and_neteq() {
    let packets = send(Posting::Steady);
    check_dtx_was_used(&packets);
    let playout = receive(&packets);
    check_playout(&playout);
    assert!(
        playout.target_delay_ms <= 60,
        "DTX must not inflate the jitter buffer on a steady network: {} ms",
        playout.target_delay_ms
    );
    let resumed_at = playout.first_resumed_speech_ms.expect("speech resumed");
    assert!(
        resumed_at - playout.resume_arrival_ms <= 100,
        "speech resumed {} ms after its first packet arrived",
        resumed_at - playout.resume_arrival_ms
    );
}

/// Main-thread jank delays posting, not stamping: the timeline stays exact
/// (checked per packet in `receive`), and NetEq only sees network jitter.
#[test]
fn jittered_posting_keeps_the_timeline_exact() {
    let packets = send(Posting::Jittered);
    check_dtx_was_used(&packets);
    assert!(packets
        .windows(2)
        .any(|pair| pair[1].posted_ms - pair[0].posted_ms != FRAME_MS));
    check_playout(&receive(&packets));
}

/// Packets posted in one burst at startup keep their 20 ms spacing.
#[test]
fn startup_burst_maps_to_frame_timestamps() {
    let packets = send(Posting::StartupBurst);
    assert!(packets[..BURST_PACKETS]
        .iter()
        .all(|packet| packet.posted_ms == packets[0].posted_ms));
    check_playout(&receive(&packets));
}

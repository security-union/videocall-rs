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

//! Play a WAV file through Opus, a simulated network and NetEq.
//!
//! 1. A producer thread encodes the WAV in 20 ms Opus packets (ropus) in
//!    real time, looping forever.
//! 2. A network thread delays each packet by random jitter and, optionally,
//!    holds some back to reorder them.
//! 3. The CPAL output callback pulls 10 ms frames from NetEq, so GetAudio is
//!    paced by the sound card, the same way the browser worker is paced by
//!    its AudioWorklet.
//! 4. Once a second the stats are logged and, with `--json-stats`, appended
//!    to `neteq_stats.jsonl` for `dashboard_server`.
//!
//! `cargo run -p neteq --example neteq_player --release -- song.wav --max-jitter-ms 80`

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use neteq::{
    AudioFrame, Config, NetEq, NetEqStats, RtpHeader, RtpPacketInfo, SdpAudioFormat,
    TelemetryCollector,
};
use rand::Rng;
use ropus::{Application as OpusApp, Channels as OpusChannels, Encoder as OpusEncoder};
use serde::Serialize;

const PAYLOAD_TYPE: u8 = 111;
/// NetEq's Opus decoder always outputs 48 kHz.
const OUTPUT_RATE_HZ: u32 = 48_000;
/// Opus RTP timestamps count 48 kHz samples whatever the input rate.
const RTP_SAMPLES_PER_PACKET: u32 = 960;

#[derive(Parser, Debug)]
#[clap(about = "NetEq player with jitter simulation", version)]
struct Args {
    #[clap(value_parser, help = "Path to a WAV file (8, 12, 16, 24 or 48 kHz)")]
    wav_path: String,

    #[clap(
        long,
        default_value_t = 0,
        help = "Maximum extra delay for reordered packets, in ms (0-200)"
    )]
    reorder_window_ms: u32,

    #[clap(
        long,
        default_value_t = 0,
        help = "Maximum network jitter in ms (0-500)"
    )]
    max_jitter_ms: u32,

    #[clap(
        long,
        help = "Append statistics to neteq_stats.jsonl for the dashboard"
    )]
    json_stats: bool,

    #[clap(long, default_value_t = 1.0, help = "Output volume (0.0 to 2.0)")]
    volume: f32,

    #[clap(
        long,
        default_value_t = 0,
        help = "NetEq minimum delay in ms (SetMinimumDelay)"
    )]
    min_delay_ms: u32,
}

/// One packet on its way through the simulated network.
struct Packet {
    header: RtpHeader,
    payload: Vec<u8>,
}

/// NetEq plus the wrapper-side counters, shared by the threads.
struct Receiver {
    neteq: NetEq,
    collector: TelemetryCollector,
    started: Instant,
}

impl Receiver {
    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

/// A dashboard line: the telemetry schema plus the player's own counters.
#[derive(Serialize)]
struct DashboardLine<'a> {
    timestamp: u128,
    callbacks_per_sec: u64,
    underruns: u64,
    #[serde(flatten)]
    stats: &'a NetEqStats,
}

static CALLBACK_CALLS: AtomicU64 = AtomicU64::new(0);
static UNDERRUNS: AtomicU64 = AtomicU64::new(0);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let args = Args::parse();
    let reorder_window_ms = args.reorder_window_ms.min(200);
    let max_jitter_ms = args.max_jitter_ms.min(500);
    let volume = args.volume.clamp(0.0, 2.0);

    // ── Read the WAV ─────────────────────────────────────────────────────────
    let mut reader = hound::WavReader::open(&args.wav_path)?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate;
    if ![8000, 12000, 16000, 24000, 48000].contains(&sample_rate) {
        return Err(format!("Opus cannot encode {sample_rate} Hz; resample the WAV first").into());
    }
    let input_channels = usize::from(spec.channels);
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|sample| sample.map(|s| f32::from(s) / 32768.0))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
    };
    // Downmix to mono: one stream, like a call.
    let mono: Vec<f32> = interleaved
        .chunks(input_channels)
        .map(|frame| frame.iter().sum::<f32>() / input_channels as f32)
        .collect();
    let samples_per_packet = sample_rate as usize / 50;
    log::info!(
        "{}: {} Hz, {} channel(s), {:.1} s",
        args.wav_path,
        sample_rate,
        input_channels,
        mono.len() as f32 / sample_rate as f32
    );

    // ── NetEq ────────────────────────────────────────────────────────────────
    let mut neteq = NetEq::builder(Config::default()).build();
    if !neteq.register_payload_type(
        i32::from(PAYLOAD_TYPE),
        &SdpAudioFormat::new("opus", 48_000, 2),
    ) {
        return Err("Opus payload type rejected".into());
    }
    if args.min_delay_ms > 0 {
        neteq.set_minimum_delay(args.min_delay_ms as i32);
    }
    let receiver = Arc::new(Mutex::new(Receiver {
        neteq,
        collector: TelemetryCollector::new(),
        started: Instant::now(),
    }));

    let _stream = start_playback(receiver.clone(), volume)?;
    spawn_stats_logger(receiver.clone(), args.json_stats)?;

    // ── Network: jitter and reordering ───────────────────────────────────────
    let (tx, rx) = mpsc::channel::<Packet>();
    let network_receiver = receiver.clone();
    thread::spawn(move || {
        let mut rng = rand::rng();
        // Min-heap on delivery time; the counter keeps equal times in order.
        let mut in_flight: BinaryHeap<Reverse<(Instant, u64)>> = BinaryHeap::new();
        let mut packets: std::collections::HashMap<u64, Packet> = Default::default();
        let mut next_id = 0u64;
        loop {
            while let Ok(packet) = rx.try_recv() {
                let mut delay_ms = rng.random::<f32>() * max_jitter_ms as f32;
                if reorder_window_ms > 0 && rng.random::<f32>() < 0.1 {
                    delay_ms += rng.random::<f32>() * reorder_window_ms as f32;
                }
                let deliver_at = Instant::now() + Duration::from_micros((delay_ms * 1000.0) as u64);
                in_flight.push(Reverse((deliver_at, next_id)));
                packets.insert(next_id, packet);
                next_id += 1;
            }
            let now = Instant::now();
            while let Some(Reverse((deliver_at, id))) = in_flight.peek().copied() {
                if deliver_at > now {
                    break;
                }
                in_flight.pop();
                let Some(packet) = packets.remove(&id) else {
                    continue;
                };
                let Ok(mut receiver) = network_receiver.lock() else {
                    return;
                };
                let now_ms = receiver.elapsed_ms();
                let info = RtpPacketInfo::new(&packet.header, None);
                if let Err(e) = receiver
                    .neteq
                    .insert_packet(&packet.header, &packet.payload, info)
                {
                    log::warn!("insert_packet: {e}");
                }
                receiver
                    .collector
                    .on_packet_inserted(packet.header.sequence_number, now_ms);
            }
            thread::sleep(Duration::from_millis(1));
        }
    });

    // ── Producer: 20 ms Opus packets in real time, looping ───────────────────
    let mut encoder =
        OpusEncoder::builder(sample_rate, OpusChannels::Mono, OpusApp::Audio).build()?;
    let mut sequence_number = 0u16;
    let mut timestamp = 0u32;
    let mut encoded = vec![0u8; 4000];
    let mut next_send = Instant::now();
    loop {
        for chunk in mono.chunks_exact(samples_per_packet) {
            let written = encoder.encode_float(chunk, &mut encoded)?;
            let packet = Packet {
                header: RtpHeader {
                    marker_bit: false,
                    payload_type: PAYLOAD_TYPE,
                    sequence_number,
                    timestamp,
                    ssrc: 0x1234_5678,
                },
                payload: encoded[..written].to_vec(),
            };
            if tx.send(packet).is_err() {
                return Ok(());
            }
            sequence_number = sequence_number.wrapping_add(1);
            timestamp = timestamp.wrapping_add(RTP_SAMPLES_PER_PACKET);
            next_send += Duration::from_millis(20);
            if let Some(wait) = next_send.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
        }
        log::info!("End of file, looping");
    }
}

fn spawn_stats_logger(
    receiver: Arc<Mutex<Receiver>>,
    json_stats: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut json_file = if json_stats {
        Some(File::create("neteq_stats.jsonl")?)
    } else {
        None
    };
    thread::spawn(move || {
        let mut previous_calls = 0;
        loop {
            thread::sleep(Duration::from_secs(1));
            let calls = CALLBACK_CALLS.load(Ordering::Relaxed);
            let callbacks_per_sec = calls - previous_calls;
            previous_calls = calls;
            let underruns = UNDERRUNS.load(Ordering::Relaxed);

            let stats = {
                let Ok(receiver) = receiver.lock() else {
                    return;
                };
                NetEqStats::from_neteq(&receiver.neteq, &receiver.collector)
            };
            log::info!(
                "buffer={}ms target={}ms packets={} expand={:.1}‰ accelerate={:.1}‰ concealed={} callbacks/s={} underruns={}",
                stats.current_buffer_size_ms,
                stats.target_delay_ms,
                stats.packets_awaiting_decode,
                neteq::q14::to_per_mille(stats.network.expand_rate),
                neteq::q14::to_per_mille(stats.network.accelerate_rate),
                stats.lifetime.concealed_samples,
                callbacks_per_sec,
                underruns,
            );

            if let Some(file) = json_file.as_mut() {
                let line = DashboardLine {
                    timestamp: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis(),
                    callbacks_per_sec,
                    underruns,
                    stats: &stats,
                };
                if let Ok(json) = serde_json::to_string(&line) {
                    let _ = writeln!(file, "{json}");
                    let _ = file.flush();
                }
            }
        }
    });
    Ok(())
}

/// Opens the default output at 48 kHz and pulls NetEq from its callback.
fn start_playback(
    receiver: Arc<Mutex<Receiver>>,
    volume: f32,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no default output device")?;
    let supported = device
        .supported_output_configs()?
        .find(|config| {
            config.sample_format() == cpal::SampleFormat::F32
                && (config.min_sample_rate().0..=config.max_sample_rate().0)
                    .contains(&OUTPUT_RATE_HZ)
        })
        .ok_or("output device has no 48 kHz f32 mode")?
        .with_sample_rate(cpal::SampleRate(OUTPUT_RATE_HZ));
    let config: cpal::StreamConfig = supported.config();
    let device_channels = usize::from(config.channels);
    log::info!("Output: 48 kHz, {device_channels} channel(s)");

    let mut frame = AudioFrame::new();
    // Mono samples decoded but not yet played. Reserved once; a callback
    // never needs more than one extra 10 ms frame.
    let mut pending: Vec<f32> = Vec::with_capacity(4096);
    let stream = device.build_output_stream(
        &config,
        move |output: &mut [f32], _| {
            CALLBACK_CALLS.fetch_add(1, Ordering::Relaxed);
            let wanted = output.len() / device_channels;
            while pending.len() < wanted {
                let Ok(mut receiver) = receiver.lock() else {
                    break;
                };
                let receiver = &mut *receiver;
                if receiver.neteq.get_audio(&mut frame, None).is_err() {
                    UNDERRUNS.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                let now_ms = receiver.elapsed_ms();
                receiver
                    .collector
                    .on_get_audio(receiver.neteq.last_operation(), now_ms);
                let channels = frame.num_channels.max(1);
                pending.extend(
                    frame
                        .data()
                        .chunks(channels)
                        .map(|sample| f32::from(sample[0]) / 32768.0 * volume),
                );
            }
            let available = pending.len().min(wanted);
            for (index, out_frame) in output.chunks_mut(device_channels).enumerate() {
                let sample = if index < available {
                    pending[index]
                } else {
                    0.0
                };
                out_frame.fill(sample);
            }
            pending.drain(..available);
        },
        |e| log::error!("Stream error: {e}"),
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

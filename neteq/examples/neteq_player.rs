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

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use web_time::{Duration, Instant};

use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::BufferSize;
use neteq::codec::OpusDecoder;
use neteq::{AudioPacket, NetEq, NetEqConfig, RtpHeader};
use opus::{Application as OpusApp, Channels as OpusChannels, Encoder as OpusEncoder};
use rand::Rng;

// This example does the following:
// 1. Loads a WAV file (passed on the command-line).
// 2. Splits it into 20 ms RTP-like packets.
// 3. Feeds the packets to NetEq on the main thread.
// 4. Sends the 10 ms audio frames produced by NetEq to a CPAL playback thread.
//
// The NetEq instance never crosses thread boundaries (it lives on the main
// thread) which avoids the need for `Send`/`Sync` on its internal objects.

#[derive(Parser, Debug)]
#[clap(about = "NetEq player with jitter simulation", version)]
struct Args {
    #[clap(value_parser, help = "Path to WAV file to play")]
    wav_path: String,

    #[clap(
        long,
        default_value_t = 0,
        help = "Maximum packet reordering window in milliseconds (0-200ms recommended)"
    )]
    reorder_window_ms: u32,

    #[clap(
        long,
        default_value_t = 0,
        help = "Maximum additional jitter delay in milliseconds (0-500ms recommended)"
    )]
    max_jitter_ms: u32,

    #[clap(long, help = "Output statistics to JSON file for web dashboard")]
    json_stats: bool,

    #[clap(
        long,
        default_value_t = 1.0,
        help = "Audio output volume (0.0 = mute, 1.0 = full volume)"
    )]
    volume: f32,

    #[clap(long = "h", action = clap::ArgAction::Help, hide = true)]
    _help_alias: Option<bool>,

    #[clap(long, help = "Disable NetEq and decode audio directly (A/B testing)")]
    no_neteq: bool,

    #[clap(
        long,
        default_value_t = 0,
        help = "Minimum delay in milliseconds for NetEQ buffer (0-500ms recommended)"
    )]
    min_delay_ms: u32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // ── Parse CLI ─────────────────────────────────────────────────────────────
    let args = Args::parse();
    let wav_path = args.wav_path;
    let reorder_window_ms = args.reorder_window_ms.min(200); // Cap at 200ms for sanity
    let max_jitter_ms = args.max_jitter_ms.min(500); // Cap at 500ms for sanity
    let json_stats = args.json_stats;
    let volume = args.volume.clamp(0.0, 2.0); // Allow up to 200% volume, minimum 0% (mute)

    log::info!("Loading WAV file: {}", wav_path);
    log::info!(
        "Audio volume: {:.1}% ({})",
        volume * 100.0,
        if volume == 0.0 { "MUTED" } else { "enabled" }
    );

    if reorder_window_ms > 0 || max_jitter_ms > 0 {
        log::info!(
            "Network simulation enabled: max_jitter={}ms, reorder_window={}ms",
            max_jitter_ms,
            reorder_window_ms
        );
    } else {
        log::info!("Network simulation disabled (no jitter or reordering)");
    }

    // ── Read WAV file ─────────────────────────────────────────────────────────
    let mut reader = hound::WavReader::open(&wav_path)?;
    let spec = reader.spec();
    let original_sample_rate = spec.sample_rate;
    let original_channels = spec.channels as u8;

    log::info!(
        "WAV spec -> sample_rate: {} Hz, channels: {}, bits_per_sample: {}",
        original_sample_rate,
        original_channels,
        spec.bits_per_sample
    );

    // Opus requirements: sample_rate must be 8k, 12k, 16k, 24k, or 48k Hz; channels must be 1 or 2
    let opus_sample_rates = [8000, 12000, 16000, 24000, 48000];
    let sample_rate = if opus_sample_rates.contains(&original_sample_rate) {
        original_sample_rate
    } else {
        // Default to 48kHz if not supported
        log::warn!(
            "Sample rate {} Hz not supported by Opus, using 48000 Hz",
            original_sample_rate
        );
        48000
    };

    let channels = if original_channels <= 2 {
        original_channels
    } else {
        log::warn!(
            "Opus only supports mono/stereo, downmixing {} channels to stereo",
            original_channels
        );
        2
    };

    if sample_rate != original_sample_rate || channels != original_channels {
        log::info!(
            "Audio will be converted: {}Hz/{}ch -> {}Hz/{}ch",
            original_sample_rate,
            original_channels,
            sample_rate,
            channels
        );
    }

    let wav_samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect(),
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
    };

    log::info!(
        "Total samples loaded: {} ({} seconds)",
        wav_samples.len(),
        wav_samples.len() as f32 / sample_rate as f32 / channels as f32
    );

    // ── Prepare NetEq ─────────────────────────────────────────────────────────
    let neteq_cfg: NetEqConfig = NetEqConfig {
        sample_rate,
        channels,
        bypass_mode: args.no_neteq,
        min_delay_ms: args.min_delay_ms,
        ..Default::default()
    };
    let neteq = Arc::new(Mutex::new(NetEq::new(neteq_cfg)?));

    log::info!(
        "NetEq initialised (sample_rate {} Hz, channels {}).",
        sample_rate,
        channels
    );

    if args.no_neteq {
        log::info!(
            "NetEq BYPASS MODE enabled - packets will be decoded directly without jitter buffering"
        );
    } else {
        log::info!("NetEq normal mode - using jitter buffer and adaptive algorithms");
    }

    // Register Opus decoder for payload type 111.
    {
        let mut n = neteq.lock().unwrap();
        n.register_decoder(
            111,
            Box::new(OpusDecoder::new(sample_rate, channels).await.unwrap()),
        );
    }

    // ── Warm-start NetEq with a few packets before audio begins ─────────────
    let warmup_packets = 25; // 25 × 20 ms = 500 ms - increased for better stability

    // Packet parameters
    let samples_per_channel_20ms = (sample_rate as f32 * 0.02) as usize;
    let packet_samples = samples_per_channel_20ms * channels as usize;

    let mut seq_no: u16 = 0;
    let mut timestamp: u32 = 0;
    let ssrc = 0x1234_5678;

    // Create Opus encoder (monophonic/stereo depending on WAV)
    let ch_enum = if channels == 1 {
        OpusChannels::Mono
    } else {
        OpusChannels::Stereo
    };
    let mut opus_encoder = OpusEncoder::new(sample_rate, ch_enum, OpusApp::Audio)?;

    let mut chunk_index_iter = wav_samples.chunks(packet_samples).enumerate();

    for _ in 0..warmup_packets {
        if let Some((_idx, chunk)) = chunk_index_iter.next() {
            // Encode 20 ms PCM chunk into Opus
            let mut encoded = vec![0u8; 4000];
            let bytes_written = opus_encoder.encode_float(chunk, &mut encoded)?;
            encoded.truncate(bytes_written as usize);
            let payload = encoded;
            let hdr = RtpHeader::new(seq_no, timestamp, ssrc, 111, false);
            let packet = AudioPacket::new(hdr, payload, sample_rate, channels, 20);
            if let Ok(mut n) = neteq.lock() {
                let _ = n.insert_packet(packet);
            }
            seq_no = seq_no.wrapping_add(1);
            timestamp = timestamp.wrapping_add(samples_per_channel_20ms as u32);
        }
    }

    // ── Start CPAL output; keep the stream alive for the duration of main ────
    let _stream = start_audio_playback(sample_rate, channels, neteq.clone(), volume)?;

    log::info!("CPAL stream started; feeding packets to NetEq ...");

    // ── Spawn stats logger thread (logs once per second) ────────────────────
    let stats_neteq = neteq.clone();
    thread::spawn(move || {
        let mut prev_calls = 0u64;
        let mut prev_frames = 0u64;
        let mut json_file = if json_stats {
            Some(
                OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open("neteq_stats.jsonl")
                    .expect("Failed to create stats file"),
            )
        } else {
            None
        };

        loop {
            thread::sleep(Duration::from_secs(1));

            let calls = CALLBACK_CALLS.load(Ordering::Relaxed);
            let frames = CALLBACK_FRAMES.load(Ordering::Relaxed);
            let delta_calls = calls - prev_calls;
            let delta_frames = frames - prev_frames;
            prev_calls = calls;
            prev_frames = frames;

            let underruns = BUFFER_UNDERRUNS.load(Ordering::Relaxed);
            if let Ok(eq) = stats_neteq.lock() {
                let stats = eq.get_statistics();
                let expand_rate = stats.network.expand_rate as f32 / 16.384; // Q14 to per-myriad
                let accel_rate = stats.network.accelerate_rate as f32 / 16.384;
                let avg_frames = delta_frames / delta_calls.max(1);

                log::info!(
                    "Stats: buffer={}ms target={}ms packets={} expand_rate={:.1}‰ accel_rate={:.1}‰ calls/s={} avg_frames={} UNDERRUNS={}",
                    stats.current_buffer_size_ms,
                    stats.target_delay_ms,
                    stats.packet_count,
                    expand_rate,
                    accel_rate,
                    delta_calls,
                    avg_frames,
                    underruns
                );

                // Write JSON stats if enabled
                if let Some(ref mut file) = json_file {
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis();

                    let json_line = format!(
                        "{{\"timestamp\":{},\"buffer_ms\":{},\"target_ms\":{},\"packets\":{},\"expand_rate\":{:.1},\"accel_rate\":{:.1},\"calls_per_sec\":{},\"avg_frames\":{},\"underruns\":{},\"reorder_rate\":{},\"reordered_packets\":{},\"max_reorder_distance\":{},\"sequence_number\":{},\"rtp_timestamp\":{}}}\n",
                        timestamp,
                        stats.current_buffer_size_ms,
                        stats.target_delay_ms,
                        stats.packet_count,
                        expand_rate,
                        accel_rate,
                        delta_calls,
                        avg_frames,
                        underruns,
                        stats.network.reorder_rate_permyriad,
                        stats.network.reordered_packets,
                        stats.network.max_reorder_distance,
                        0, // We'll update this when we track actual sequence numbers
                        0  // We'll update this when we track actual RTP timestamps
                    );

                    let _ = file.write_all(json_line.as_bytes());
                    let _ = file.flush();
                }
            }
        }
    });

    // ── Set up channel and network simulator ────────────────────────────────
    let (tx, rx) = mpsc::channel::<AudioPacket>();

    // Network simulator thread with improved timing
    let neteq_for_net = neteq.clone();
    thread::spawn(move || {
        let mut rng = rand::rng();
        let mut reorder_buffer: Vec<(AudioPacket, Instant)> = Vec::new();

        for packet in rx {
            let now = Instant::now();

            // Handle reordering by buffering some packets
            if reorder_window_ms > 0 && rng.random::<f32>() < 0.5 {
                // 10% chance to reorder
                let reorder_delay_ms = rng.random::<f32>() * reorder_window_ms as f32 * 0.5; // Reduce reorder delay
                let delivery_time = now + Duration::from_millis(reorder_delay_ms as u64);
                reorder_buffer.push((packet, delivery_time));
            } else {
                // Regular packet with optional jitter
                let jitter_delay_ms = if max_jitter_ms > 0 {
                    rng.random::<f32>() * max_jitter_ms as f32 * 0.3 // Reduce jitter significantly
                } else {
                    0.0
                };

                if jitter_delay_ms > 0.0 {
                    std::thread::sleep(Duration::from_millis(jitter_delay_ms as u64));
                }

                if let Ok(mut n) = neteq_for_net.lock() {
                    let _ = n.insert_packet(packet);
                }
            }

            // Deliver any reordered packets that are ready
            reorder_buffer.retain(|(packet, delivery_time)| {
                if now >= *delivery_time {
                    if let Ok(mut n) = neteq_for_net.lock() {
                        let _ = n.insert_packet(packet.clone());
                    }
                    false
                } else {
                    true
                }
            });
        }
    });

    // ── Spawn producer thread to feed remaining packets ─────────────────────-
    let remaining_samples: Vec<f32> = wav_samples
        .iter()
        .skip(warmup_packets * packet_samples)
        .cloned()
        .collect();
    let total_remaining_chunks = remaining_samples.len() / packet_samples;

    thread::spawn(move || {
        let _period = Duration::from_millis(20);
        let mut loc_seq_no = seq_no;
        let mut loc_timestamp = timestamp;

        loop {
            let loop_start = Instant::now();
            for idx in 0..total_remaining_chunks {
                let packet_start_time = loop_start + Duration::from_millis(idx as u64 * 20);

                let start = idx * packet_samples;
                let chunk = &remaining_samples[start..start + packet_samples];
                let mut encoded = vec![0u8; 4000];
                let bytes_written = opus_encoder
                    .encode_float(chunk, &mut encoded)
                    .expect("Opus encode");
                encoded.truncate(bytes_written as usize);
                let hdr = RtpHeader::new(loc_seq_no, loc_timestamp, ssrc, 111, false);
                let packet = AudioPacket::new(hdr, encoded, sample_rate, channels, 20);

                // More precise timing - sleep until the exact time this packet should be sent
                let now = Instant::now();
                if packet_start_time > now {
                    std::thread::sleep(packet_start_time - now);
                }

                tx.send(packet).expect("channel send");

                loc_seq_no = loc_seq_no.wrapping_add(1);
                loc_timestamp = loc_timestamp.wrapping_add(samples_per_channel_20ms as u32);
            }
            log::info!(
                "Producer finished feeding all packets – restarting loop for continuous play"
            );
        }
    });

    // keep main alive
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

// ───────────────────────────── Helpers ───────────────────────────────────────
fn start_audio_playback(
    _sample_rate: u32,
    channels: u8,
    neteq: Arc<Mutex<NetEq>>, // shared NetEq
    volume: f32,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    use cpal::SampleFormat::*;

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .expect("No default output device");

    // We prefer 48 kHz output for compatibility with Opus and many devices.
    let preferred_rate = 48_000u32;

    // Try to find a supported config that includes 48 kHz.
    let mut cfg: Option<cpal::SupportedStreamConfig> = None;
    for sc in device.supported_output_configs()? {
        let sr_range = sc.min_sample_rate().0..=sc.max_sample_rate().0;
        if sr_range.contains(&preferred_rate) {
            cfg = Some(sc.with_sample_rate(cpal::SampleRate(preferred_rate)));
            break;
        }
    }

    let supported = cfg.unwrap_or_else(|| device.default_output_config().unwrap());

    log::info!(
        "Output device will run at {} Hz (format {:?})",
        supported.sample_rate().0,
        supported.sample_format()
    );

    let sample_format = supported.sample_format();
    let mut cfg: cpal::StreamConfig = supported.config();

    // Request a fixed 10 ms buffer size per callback, based on chosen rate.
    let frames_per_buffer = cfg.sample_rate.0 / 100; // 10 ms worth
    cfg.buffer_size = BufferSize::Fixed(frames_per_buffer);
    cfg.channels = 1; // mono output

    log::info!(
        "Final stream config - sample_rate={} buffer_size={:?}",
        cfg.sample_rate.0,
        cfg.buffer_size
    );

    let stream = match sample_format {
        F32 => build_stream_f32(&device, &cfg, channels, neteq.clone(), volume)?,
        I16 => build_stream_i16(&device, &cfg, channels, neteq.clone(), volume)?,
        U16 => build_stream_u16(&device, &cfg, channels, neteq.clone(), volume)?,
        _ => unreachable!(),
    };
    stream.play()?;
    Ok(stream)
}

fn build_stream_f32(
    device: &cpal::Device,
    cfg: &cpal::StreamConfig,
    _channels: u8,
    neteq: Arc<Mutex<NetEq>>,
    volume: f32,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut leftover: Vec<f32> = Vec::new();
    let err_fn = |e| eprintln!("Stream error: {}", e);
    device.build_output_stream(
        cfg,
        move |output: &mut [f32], _| {
            fill_output_neteq(output, &neteq, &mut leftover, volume);
            log::debug!("CPAL callback filled {} samples (f32)", output.len());
            CALLBACK_CALLS.fetch_add(1, Ordering::Relaxed);
            CALLBACK_FRAMES.fetch_add(output.len() as u64, Ordering::Relaxed);
        },
        err_fn,
        None,
    )
}

fn build_stream_i16(
    device: &cpal::Device,
    cfg: &cpal::StreamConfig,
    _channels: u8,
    neteq: Arc<Mutex<NetEq>>,
    volume: f32,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut leftover: Vec<f32> = Vec::new();
    let err_fn = |e| eprintln!("Stream error: {}", e);
    device.build_output_stream(
        cfg,
        move |output: &mut [i16], _| {
            let mut tmp = vec![0.0f32; output.len()];
            fill_output_neteq(&mut tmp, &neteq, &mut leftover, volume);
            for (o, &v) in output.iter_mut().zip(tmp.iter()) {
                *o = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
            }
            log::debug!("CPAL callback filled {} samples (i16)", output.len());
            CALLBACK_CALLS.fetch_add(1, Ordering::Relaxed);
            CALLBACK_FRAMES.fetch_add(output.len() as u64, Ordering::Relaxed);
        },
        err_fn,
        None,
    )
}

fn build_stream_u16(
    device: &cpal::Device,
    cfg: &cpal::StreamConfig,
    _channels: u8,
    neteq: Arc<Mutex<NetEq>>,
    volume: f32,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut leftover: Vec<f32> = Vec::new();
    let err_fn = |e| eprintln!("Stream error: {}", e);
    device.build_output_stream(
        cfg,
        move |output: &mut [u16], _| {
            let mut tmp = vec![0.0f32; output.len()];
            fill_output_neteq(&mut tmp, &neteq, &mut leftover, volume);
            for (o, &v) in output.iter_mut().zip(tmp.iter()) {
                *o = ((v.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32) as u16;
            }
            log::debug!("CPAL callback filled {} samples (u16)", output.len());
            CALLBACK_CALLS.fetch_add(1, Ordering::Relaxed);
            CALLBACK_FRAMES.fetch_add(output.len() as u64, Ordering::Relaxed);
        },
        err_fn,
        None,
    )
}

fn fill_output_neteq(
    buffer: &mut [f32],
    neteq: &Arc<Mutex<NetEq>>,
    leftover: &mut Vec<f32>,
    volume: f32,
) {
    let mut idx = 0;
    let mut underrun_occurred = false;

    while idx < buffer.len() {
        if leftover.is_empty() {
            match neteq.lock() {
                Ok(mut n) => match n.get_audio() {
                    Ok(frame) => {
                        leftover.extend_from_slice(&frame.samples);
                    }
                    Err(e) => {
                        log::warn!("BUFFER UNDERRUN: NetEq get_audio error: {:?}", e);
                        underrun_occurred = true;
                        // fill silence and return
                        for s in &mut buffer[idx..] {
                            *s = 0.0;
                        }
                        break;
                    }
                },
                Err(poison) => {
                    log::error!("NetEq mutex poisoned: {}", poison);
                    underrun_occurred = true;
                    for s in &mut buffer[idx..] {
                        *s = 0.0;
                    }
                    break;
                }
            }
        }

        let n = std::cmp::min(leftover.len(), buffer.len() - idx);
        if n == 0 {
            // Still no data available - this is an underrun
            underrun_occurred = true;
            log::warn!("BUFFER UNDERRUN: No audio data available, filling with silence");
            for s in &mut buffer[idx..] {
                *s = 0.0;
            }
            break;
        }

        // Copy samples and apply volume scaling
        for i in 0..n {
            buffer[idx + i] = leftover[i] * volume;
        }
        leftover.drain(..n);
        idx += n;
    }

    if underrun_occurred {
        BUFFER_UNDERRUNS.fetch_add(1, Ordering::Relaxed);
    }
}

// global counters for callback diagnostics
static CALLBACK_CALLS: AtomicU64 = AtomicU64::new(0);
static CALLBACK_FRAMES: AtomicU64 = AtomicU64::new(0);
static BUFFER_UNDERRUNS: AtomicU64 = AtomicU64::new(0);

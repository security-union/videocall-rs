use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::BufferSize;
use opus::{Application as OpusApp, Channels as OpusChannels, Encoder as OpusEncoder};
use rand::Rng;
use videocall_neteq::codec::OpusDecoder;
use videocall_neteq::{AudioPacket, NetEq, NetEqConfig, RtpHeader};

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
        default_value_t = 0.0,
        help = "Packets out-of-order level 0.0-1.0"
    )]
    packets_out_of_order: f32,

    #[clap(
        long,
        default_value_t = 0.0,
        help = "Inter-frame extra delay level 0.0-1.0"
    )]
    inter_frame_delay: f32,

    #[clap(long = "h", action = clap::ArgAction::Help, hide = true)]
    _help_alias: Option<bool>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // ── Parse CLI ─────────────────────────────────────────────────────────────
    let args = Args::parse();
    let wav_path = args.wav_path;
    let disorder_level = args.packets_out_of_order.clamp(0.0, 1.0);
    let delay_level = args.inter_frame_delay.clamp(0.0, 1.0);

    log::info!("Loading WAV file: {}", wav_path);

    // ── Read WAV file ─────────────────────────────────────────────────────────
    let mut reader = hound::WavReader::open(&wav_path)?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate;
    let channels = spec.channels as u8;

    log::info!(
        "WAV spec -> sample_rate: {} Hz, channels: {}, bits_per_sample: {}",
        sample_rate,
        channels,
        spec.bits_per_sample
    );

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
    let mut neteq_cfg: NetEqConfig = NetEqConfig::default();
    neteq_cfg.sample_rate = sample_rate;
    neteq_cfg.channels = channels;
    let neteq = Arc::new(Mutex::new(NetEq::new(neteq_cfg)?));

    log::info!(
        "NetEq initialised (sample_rate {} Hz, channels {}).",
        sample_rate,
        channels
    );

    // Register Opus decoder for payload type 111.
    {
        let mut n = neteq.lock().unwrap();
        n.register_decoder(111, Box::new(OpusDecoder::new(sample_rate, channels)?));
    }

    // ── Warm-start NetEq with a few packets before audio begins ─────────────
    let warmup_packets = 10; // 10 × 20 ms = 200 ms

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
    let mut opus_encoder = OpusEncoder::new(sample_rate as u32, ch_enum, OpusApp::Audio)?;

    let mut chunk_index_iter = wav_samples.chunks(packet_samples).enumerate();

    for _ in 0..warmup_packets {
        if let Some((idx, chunk)) = chunk_index_iter.next() {
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
    let _stream = start_audio_playback(sample_rate, channels, neteq.clone())?;

    log::info!("CPAL stream started; feeding packets to NetEq ...");

    // ── Spawn stats logger thread (logs once per second) ────────────────────
    let stats_neteq = neteq.clone();
    thread::spawn(move || {
        let mut prev_calls = 0u64;
        let mut prev_frames = 0u64;
        loop {
            thread::sleep(Duration::from_secs(1));

            let calls = CALLBACK_CALLS.load(Ordering::Relaxed);
            let frames = CALLBACK_FRAMES.load(Ordering::Relaxed);
            let delta_calls = calls - prev_calls;
            let delta_frames = frames - prev_frames;
            prev_calls = calls;
            prev_frames = frames;

            let fps = delta_frames as f32 / calls_per_sec_den(channels) as f32; // will compute later
            if let Ok(eq) = stats_neteq.lock() {
                let stats = eq.get_statistics();
                log::info!(
                    "Stats: buffer={}ms target={}ms packets={} expand_rate={}‰ accel_rate={}‰ calls/s={} avg_frames={}",
                    stats.current_buffer_size_ms,
                    stats.target_delay_ms,
                    stats.packet_count,
                    stats.network.expand_rate as f32 / 16.384,  // Q14 to per-myriad
                    stats.network.accelerate_rate as f32 / 16.384,
                    delta_calls,
                    delta_frames / delta_calls.max(1)
                );
            }
        }
    });

    // ── Set up channel and network simulator ────────────────────────────────
    let (tx, rx) = mpsc::channel::<AudioPacket>();

    // Network simulator thread
    let neteq_for_net = neteq.clone();
    thread::spawn(move || {
        let mut rng = rand::thread_rng();
        for packet in rx {
            // Extra delay in 0..1000 ms scaled
            let extra_delay_ms = rng.gen::<f32>() * delay_level * 100.0;
            // Additional small randomness to create reordering within 0..40 ms window
            let reorder_delay_ms = rng.gen::<f32>() * disorder_level * 40.0;
            let total_delay = Duration::from_millis((extra_delay_ms + reorder_delay_ms) as u64);
            std::thread::sleep(total_delay);

            if let Ok(mut n) = neteq_for_net.lock() {
                let _ = n.insert_packet(packet);
            }
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
        let period = Duration::from_millis(20);
        let mut next = Instant::now();
        let mut loc_seq_no = seq_no;
        let mut loc_timestamp = timestamp;

        loop {
            for idx in 0..total_remaining_chunks {
                let start = idx * packet_samples;
                let chunk = &remaining_samples[start..start + packet_samples];
                let mut encoded = vec![0u8; 4000];
                let bytes_written = opus_encoder
                    .encode_float(chunk, &mut encoded)
                    .expect("Opus encode");
                encoded.truncate(bytes_written as usize);
                let hdr = RtpHeader::new(loc_seq_no, loc_timestamp, ssrc, 111, false);
                let packet = AudioPacket::new(hdr, encoded, sample_rate, channels, 20);
                tx.send(packet).expect("channel send");

                loc_seq_no = loc_seq_no.wrapping_add(1);
                loc_timestamp = loc_timestamp.wrapping_add(samples_per_channel_20ms as u32);

                next += period;
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
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
    sample_rate: u32,
    channels: u8,
    neteq: Arc<Mutex<NetEq>>, // shared NetEq
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
    let frames_per_buffer = (cfg.sample_rate.0 / 100) as u32; // 10 ms worth
    cfg.buffer_size = BufferSize::Fixed(frames_per_buffer);
    cfg.channels = 1; // mono output

    log::info!(
        "Final stream config - sample_rate={} buffer_size={:?}",
        cfg.sample_rate.0,
        cfg.buffer_size
    );

    let stream = match sample_format {
        F32 => build_stream_f32(&device, &cfg, channels, neteq.clone())?,
        I16 => build_stream_i16(&device, &cfg, channels, neteq.clone())?,
        U16 => build_stream_u16(&device, &cfg, channels, neteq.clone())?,
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
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut leftover: Vec<f32> = Vec::new();
    let err_fn = |e| eprintln!("Stream error: {}", e);
    device.build_output_stream(
        cfg,
        move |output: &mut [f32], _| {
            fill_output_neteq(output, &neteq, &mut leftover);
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
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut leftover: Vec<f32> = Vec::new();
    let err_fn = |e| eprintln!("Stream error: {}", e);
    device.build_output_stream(
        cfg,
        move |output: &mut [i16], _| {
            let mut tmp = vec![0.0f32; output.len()];
            fill_output_neteq(&mut tmp, &neteq, &mut leftover);
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
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut leftover: Vec<f32> = Vec::new();
    let err_fn = |e| eprintln!("Stream error: {}", e);
    device.build_output_stream(
        cfg,
        move |output: &mut [u16], _| {
            let mut tmp = vec![0.0f32; output.len()];
            fill_output_neteq(&mut tmp, &neteq, &mut leftover);
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

fn fill_output_neteq(buffer: &mut [f32], neteq: &Arc<Mutex<NetEq>>, leftover: &mut Vec<f32>) {
    let mut idx = 0;
    while idx < buffer.len() {
        if leftover.is_empty() {
            match neteq.lock() {
                Ok(mut n) => match n.get_audio() {
                    Ok(frame) => {
                        log::debug!("NetEq get_audio: {:?}", frame.speech_type);
                        leftover.extend_from_slice(&frame.samples);
                    }
                    Err(e) => {
                        log::error!("NetEq get_audio error: {:?}", e);
                        // fill silence and return
                        for s in &mut buffer[idx..] {
                            *s = 0.0;
                        }
                        return;
                    }
                },
                Err(poison) => {
                    log::error!("NetEq mutex poisoned: {}", poison);
                    for s in &mut buffer[idx..] {
                        *s = 0.0;
                    }
                    return;
                }
            }
        }
        let n = std::cmp::min(leftover.len(), buffer.len() - idx);
        buffer[idx..idx + n].copy_from_slice(&leftover[..n]);
        leftover.drain(..n);
        idx += n;
    }
}

// global counters for callback diagnostics
static CALLBACK_CALLS: AtomicU64 = AtomicU64::new(0);
static CALLBACK_FRAMES: AtomicU64 = AtomicU64::new(0);

// helper removed channels variable; adjust after compile
fn calls_per_sec_den(channels: u8) -> u64 {
    channels as u64
}

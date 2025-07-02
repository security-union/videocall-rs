use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use videocall_neteq::{AudioPacket, NetEq, NetEqConfig, RtpHeader};

// This example does the following:
// 1. Loads a WAV file (passed on the command-line).
// 2. Splits it into 20 ms RTP-like packets.
// 3. Feeds the packets to NetEq on the main thread.
// 4. Sends the 10 ms audio frames produced by NetEq to a CPAL playback thread.
//
// The NetEq instance never crosses thread boundaries (it lives on the main
// thread) which avoids the need for `Send`/`Sync` on its internal objects.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // ── Parse CLI ─────────────────────────────────────────────────────────────
    let wav_path = std::env::args().nth(1).expect(
        "Usage: cargo run --example neteq_player --features audio_files <wav_file>",
    );

    log::info!("Loading WAV file: {}", wav_path);

    // ── Read WAV file ─────────────────────────────────────────────────────────
    let mut reader = hound::WavReader::open(&wav_path)?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate;
    let channels = spec.channels as u8;

    log::info!(
        "WAV spec -> sample_rate: {} Hz, channels: {}, bits_per_sample: {}",
        sample_rate, channels, spec.bits_per_sample
    );

    let wav_samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect(),
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| s.unwrap())
            .collect(),
    };

    log::info!("Total samples loaded: {} ({} seconds)", wav_samples.len(), wav_samples.len() as f32 / sample_rate as f32 / channels as f32);

    // ── Prepare NetEq ─────────────────────────────────────────────────────────
    let mut neteq_cfg: NetEqConfig = NetEqConfig::default();
    neteq_cfg.sample_rate = sample_rate;
    neteq_cfg.channels = channels;
    let neteq = Arc::new(Mutex::new(NetEq::new(neteq_cfg)?));

    log::info!("NetEq initialised (sample_rate {} Hz, channels {}).", sample_rate, channels);

    // ── Start CPAL output; keep the stream alive for the duration of main ────
    let _stream = start_audio_playback(sample_rate, channels, neteq.clone())?;

    log::info!("CPAL stream started; feeding packets to NetEq ...");

    // ── Packetise and insert into NetEq (20 ms cadence) ───────────────────────
    let samples_per_channel_20ms = (sample_rate as f32 * 0.02) as usize;
    let packet_samples = samples_per_channel_20ms * channels as usize;

    let mut seq_no: u16 = 0;
    let mut timestamp: u32 = 0;
    let ssrc = 0x1234_5678;

    for (idx, chunk) in wav_samples.chunks(packet_samples).enumerate() {
        let mut payload = Vec::with_capacity(chunk.len() * 4);
        for &s in chunk {
            payload.extend_from_slice(&s.to_le_bytes());
        }
        let hdr = RtpHeader::new(seq_no, timestamp, ssrc, 96, false);
        let packet = AudioPacket::new(hdr, payload, sample_rate, channels, 20);
        if let Ok(mut n) = neteq.lock() {
            if let Err(e) = n.insert_packet(packet) {
                log::error!("NetEq insert_packet error: {:?}", e);
            }
        }

        seq_no = seq_no.wrapping_add(1);
        timestamp = timestamp.wrapping_add(samples_per_channel_20ms as u32);
        if idx % 50 == 0 {
            log::debug!("fed {} packets ({} seconds)", idx, idx as f32 * 0.02);
        }
        thread::sleep(Duration::from_millis(20));
    }

    log::info!("Finished feeding packets; waiting for playback to drain ...");

    // Wait a little to let playback finish.
    thread::sleep(Duration::from_secs(3));
    Ok(())
}

// ───────────────────────────── Helpers ───────────────────────────────────────
fn start_audio_playback(
    _sample_rate: u32,
    channels: u8,
    neteq: Arc<Mutex<NetEq>>, // shared NetEq
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    use cpal::SampleFormat::*;

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .expect("No default output device");

    // Pick a supported config matching our WAV if possible.
    let supported = device.default_output_config()?;
    let sample_format = supported.sample_format();
    let cfg: cpal::StreamConfig = supported.config();

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
                        log::warn!("NetEq get_audio: {:?}", frame.speech_type);
                        leftover.extend_from_slice(&frame.samples);
                    },
                    Err(e) => {
                        log::error!("NetEq get_audio error: {:?}", e);
                        // fill silence and return
                        for s in &mut buffer[idx..] { *s = 0.0; }
                        return;
                    }
                },
                Err(poison) => {
                    log::error!("NetEq mutex poisoned: {}", poison);
                    for s in &mut buffer[idx..] { *s = 0.0; }
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
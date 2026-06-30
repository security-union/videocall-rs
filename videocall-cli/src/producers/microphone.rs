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

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use protobuf::Message;
use ropus::{Application, Channels, Encoder};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tracing::{error, info};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{AudioMetadata, MediaPacket};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// Maximum encoded Opus packet size in bytes for a 20 ms mono VoIP frame.
/// 4000 is the conventional libopus upper bound and leaves ample headroom.
const MAX_OPUS_PACKET: usize = 4000;

pub struct MicrophoneDaemon {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<anyhow::Result<()>>>,
}

impl Default for MicrophoneDaemon {
    fn default() -> Self {
        Self::new()
    }
}

impl MicrophoneDaemon {
    pub fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            handles: vec![],
        }
    }

    pub fn start(
        &mut self,
        wt_tx: Sender<Vec<u8>>,
        device: String,
        email: String,
    ) -> anyhow::Result<()> {
        self.handles.push(start_microphone(
            device.clone(),
            wt_tx.clone(),
            email,
            self.stop.clone(),
        )?);
        Ok(())
    }

    pub fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for handle in self.handles.drain(..) {
            if let Err(e) = handle.join() {
                error!("Failed to join microphone thread: {:?}", e);
            }
        }
    }
}

fn start_microphone(
    device: String,
    wt_tx: Sender<Vec<u8>>,
    email: String,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<JoinHandle<anyhow::Result<()>>> {
    let host = cpal::default_host();

    // Set up the input device and stream with the default input config.
    let device = if device == "default" {
        host.default_input_device()
    } else {
        host.input_devices()?
            .find(|x| x.name().map(|y| y == device).unwrap_or(false))
    }
    .expect("failed to find input device");

    info!("Input device: {}", device.name()?);

    // Adapt to whatever the device actually supports instead of forcing a fixed
    // config: webcam mics are commonly 48 kHz stereo f32, not the mono i16 we
    // used to hard-code (which they reject with "configuration not supported").
    let supported = device
        .default_input_config()
        .map_err(|e| anyhow::anyhow!("no default input config for device: {e}"))?;
    let in_rate = supported.sample_rate().0;
    let in_channels = supported.channels() as usize;
    let in_format = supported.sample_format();
    let stream_config: cpal::StreamConfig = supported.config();

    if in_channels == 0 {
        anyhow::bail!("device reports 0 input channels");
    }
    // Opus runs at 8/12/16/24/48 kHz; encode at the device's native rate so we
    // never have to resample (all common capture devices default to 48 kHz).
    const OPUS_RATES: [u32; 5] = [8000, 12000, 16000, 24000, 48000];
    if !OPUS_RATES.contains(&in_rate) {
        anyhow::bail!(
            "device sample rate {in_rate} Hz is not an Opus rate (need 8/12/16/24/48 kHz); \
             resampling is not implemented"
        );
    }
    let frame_size = (in_rate / 50) as usize; // 20 ms of mono samples per packet

    let mut encoder = Encoder::builder(in_rate, Channels::Mono, Application::Voip).build()?;
    info!(
        "Opus encoder created (ropus pure-Rust, {in_rate} Hz mono, VoIP); \
         capturing {in_channels} ch {in_format:?} and down-mixing to mono"
    );

    let err_fn = |err| error!("an error occurred on stream: {err}");

    Ok(std::thread::spawn(move || {
        // Mono f32 accumulator persisted across callbacks; a packet is emitted
        // each time a full 20 ms frame has accumulated. Capturing it in the
        // (FnMut) data callback keeps framing stateful across invocations.
        let mut acc: Vec<f32> = Vec::with_capacity(frame_size * 2);
        let mut frames: u64 = 0;

        // One data callback per supported sample format: down-mix interleaved
        // input to mono, then encode full frames as the accumulator fills.
        macro_rules! data_callback {
            ($sample:ty, $to_f32:expr) => {
                move |data: &[$sample], _: &_| {
                    let mut out = [0u8; MAX_OPUS_PACKET];
                    for frame in data.chunks_exact(in_channels) {
                        let sum: f32 = frame.iter().copied().map($to_f32).sum();
                        acc.push(sum / in_channels as f32);
                    }
                    while acc.len() >= frame_size {
                        match encoder.encode_float(&acc[..frame_size], &mut out) {
                            Ok(n) => {
                                if let Err(e) = send_audio_packet(&out[..n], &wt_tx, &email, frames)
                                {
                                    error!("Failed to send audio: {e}");
                                }
                            }
                            Err(e) => error!("Opus encode failed: {e}"),
                        }
                        acc.drain(..frame_size);
                        frames += 1;
                        if frames % 50 == 0 {
                            info!(
                                "Streaming audio: {frames} frames encoded (~{}s)",
                                frames / 50
                            );
                        }
                    }
                }
            };
        }

        let build_result = match in_format {
            cpal::SampleFormat::F32 => {
                device.build_input_stream(&stream_config, data_callback!(f32, |s| s), err_fn, None)
            }
            cpal::SampleFormat::I16 => device.build_input_stream(
                &stream_config,
                data_callback!(i16, |s: i16| s as f32 / 32768.0),
                err_fn,
                None,
            ),
            other => {
                let e = anyhow::anyhow!("unsupported sample format '{other}'");
                error!("Microphone: {e:#}");
                return Err(e);
            }
        };

        // Surface build failures instead of swallowing them — the thread's
        // error was previously only observable on join(), which never runs for
        // the lifetime of the stream.
        let stream = match build_result {
            Ok(s) => s,
            Err(e) => {
                error!("Microphone: failed to open input stream: {e}");
                return Err(e.into());
            }
        };
        info!("Begin streaming audio...");
        if let Err(e) = stream.play() {
            error!("Microphone: failed to start input stream: {e}");
            return Err(e.into());
        }

        loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        Ok(())
    }))
}

/// Wrap an encoded Opus frame in a media packet and queue it for transport.
fn send_audio_packet(
    opus: &[u8],
    wt_tx: &Sender<Vec<u8>>,
    email: &str,
    sequence: u64,
) -> anyhow::Result<()> {
    let packet = transform_audio_chunk(opus.to_vec(), email.to_string(), sequence)?;
    let bytes = packet.write_to_bytes()?;
    wt_tx.try_send(bytes)?;
    Ok(())
}

fn transform_audio_chunk(
    data: Vec<u8>,
    email: String,
    sequence: u64,
) -> anyhow::Result<PacketWrapper> {
    let user_id_bytes = email.as_bytes();
    Ok(PacketWrapper {
        packet_type: PacketType::MEDIA.into(),
        user_id: user_id_bytes.to_vec(),
        data: MediaPacket {
            media_type: MediaType::AUDIO.into(),
            data,
            user_id: user_id_bytes.to_vec(),
            frame_type: String::from("key"),
            // Milliseconds — the NetEq decoder treats `timestamp` as ms (it
            // subtracts an Opus frame duration in ms during RED recovery).
            timestamp: get_millis_now(),
            duration: 0.0,
            // Audio packets MUST carry `audio_metadata` (not `video_metadata`):
            // the receiver skips any audio packet without it. The empty
            // `audio_format` marks this as plain Opus (non-RED). The monotonic
            // `sequence` is required for the jitter buffer to order/dedupe
            // frames — previously hard-coded to 0, which collapsed all packets.
            audio_metadata: Some(AudioMetadata {
                sequence,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        }
        .write_to_bytes()?,
        ..Default::default()
    })
}

fn get_millis_now() -> f64 {
    let now = std::time::SystemTime::now();
    let duration = now.duration_since(std::time::UNIX_EPOCH).unwrap();
    duration.as_millis() as f64
}

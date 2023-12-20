use cpal::SampleRate;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use opus::Channels;
use protobuf::{MessageField, Message};
use tokio::sync::mpsc::Sender;
use types::protos::packet_wrapper::PacketWrapper;
use types::protos::packet_wrapper::packet_wrapper::PacketType;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{info, error};
use types::protos::media_packet::{MediaPacket, VideoMetadata};
use types::protos::media_packet::media_packet::MediaType;

pub struct MicrophoneDaemon {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<anyhow::Result<()>>>
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
            handles: vec![]
        }
    }

    pub fn start(&mut self, quic_tx: Sender<Vec<u8>>, device: String, email: String) -> anyhow::Result<()> {
        self.handles.push(start_microphone(device.clone(), quic_tx.clone(), email, self.stop.clone())?);
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

fn start_microphone(device: String, quic_tx: Sender<Vec<u8>>, email: String, stop: Arc<AtomicBool>) -> anyhow::Result<JoinHandle<anyhow::Result<()>>> {
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

    let mut config = device.default_input_config()?;
    info!("Default input config: {:?}", config);
    // Opus only supports 48kHz sample rate so find a compatible config.
    for supported_config in device.supported_input_configs()?.map(|x: cpal::SupportedStreamConfigRange| x.with_sample_rate(SampleRate(48000))) {
        if supported_config.channels() != 1 {
            continue;
        }
        info!("Supported input config: {:?}", supported_config);
        if supported_config.sample_format() == cpal::SampleFormat::F32 || supported_config.sample_format() == cpal::SampleFormat::I16 {
            info!("Using supported input config: {:?}", supported_config);
            config = supported_config;
            break;
        }
    }

    let mut encoder = opus::Encoder::new(48000, Channels::Mono, opus::Application::Voip)?;

    info!("Opus encoder created {:?}", encoder);

    let err_fn = move |err| {
        error!("an error occurred on stream: {}", err);
    };

    Ok(std::thread::spawn(move || {
        let stream = match config.sample_format() {
            cpal::SampleFormat::I16 => device.build_input_stream(
                &config.into(),
                move |data, _: &_| {
                    for chunk in data.chunks_exact(240) {
                        match encode_and_send_i16(chunk, &mut encoder, &quic_tx, email.clone()) {
                            Ok(_) => {}
                            Err(e) => {
                                error!("Failed to encode and send audio: {}", e);
                            }
                        }
                    }
                },
                err_fn,
                None,
            )?,
            cpal::SampleFormat::F32 => device.build_input_stream(
                &config.into(),
                move |data, _: &_| {
                    for chunk in data.chunks_exact(240) {
                        match encode_and_send_f32(chunk, &mut encoder, &quic_tx, email.clone()) {
                            Ok(_) => {}
                            Err(e) => {
                                error!("Failed to encode and send audio: {}", e);
                            }
                        }
                    }
                },
                err_fn,
                None,
            )?,
            sample_format => {
                return Err(anyhow::Error::msg(format!(
                    "Unsupported sample format '{sample_format}'"
                )))
            }
        };
        info!("Begin streaming audio...");
        stream.play().expect("failed to play stream");

        loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        Ok(())
    }))
}

fn encode_and_send_i16(input: &[i16], encoder: &mut opus::Encoder, quic_tx: &Sender<Vec<u8>>, email: String) -> anyhow::Result<()>
{
    let output = encoder.encode_vec( input, 1024)?;
    let output = transform_audio_chunk(output, email, 0);
    let output = output?.write_to_bytes()?;
    quic_tx.try_send(output)?;
    Ok(())
}

fn encode_and_send_f32(input: &[f32], encoder: &mut opus::Encoder, quic_tx: &Sender<Vec<u8>>, email: String) -> anyhow::Result<()>
{
    let output = encoder.encode_vec_float(input, 1024)?;
    let output = transform_audio_chunk(output, email, 0);
    let output = output?.write_to_bytes()?;
    quic_tx.try_send(output)?;
    Ok(())
}

fn transform_audio_chunk(data: Vec<u8>, email: String, sequence: u64) -> anyhow::Result<PacketWrapper> {
    Ok(PacketWrapper {
        packet_type: PacketType::MEDIA.into(),
        email: email.clone(),
        data: MediaPacket {
            media_type: MediaType::AUDIO.into(),
            data,
            email,
            frame_type: String::from("key"),
            timestamp: get_micros_now(),
            // TODO: Duration of the audio in microseconds.
            duration: 0.0,
            video_metadata: MessageField(Some(Box::new(VideoMetadata {
                sequence,
                ..Default::default()
            }))),
            ..Default::default()
        }.write_to_bytes()?,
        ..Default::default()
    })
}

fn get_micros_now() -> f64 {
    let now = std::time::SystemTime::now();
    let duration = now.duration_since(std::time::UNIX_EPOCH).unwrap();
    duration.as_micros() as f64
}

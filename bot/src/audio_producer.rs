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

use opus::{Application as OpusApp, Channels as OpusChannels, Encoder as OpusEncoder};
use protobuf::Message;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, info, warn};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{AudioMetadata, MediaPacket, VideoMetadata};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

pub struct AudioProducer {
    user_id: String,
    quit: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioProducer {
    pub fn new(
        user_id: String,
        audio_data: Vec<f32>,
        packet_sender: Sender<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        let quit = Arc::new(AtomicBool::new(false));
        let quit_clone = quit.clone();
        let user_id_clone = user_id.clone();

        let handle = thread::spawn(move || {
            if let Err(e) = Self::audio_loop(user_id_clone, audio_data, packet_sender, quit_clone) {
                error!("Audio producer error: {}", e);
            }
        });

        Ok(AudioProducer {
            user_id,
            quit,
            handle: Some(handle),
        })
    }

    pub fn from_wav_file(
        user_id: String,
        wav_path: &str,
        packet_sender: Sender<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        info!("Loading WAV file for {}: {}", user_id, wav_path);

        let mut reader = hound::WavReader::open(wav_path)?;
        let spec = reader.spec();
        let sample_rate = spec.sample_rate;
        let channels = spec.channels as u8;

        info!(
            "WAV spec -> sample_rate: {} Hz, channels: {}",
            sample_rate, channels
        );

        // Validate Opus requirements
        let opus_sample_rates = [8000, 12000, 16000, 24000, 48000];
        if !opus_sample_rates.contains(&sample_rate) {
            warn!(
                "Sample rate {} Hz not supported by Opus, audio may not work correctly",
                sample_rate
            );
        }

        if channels > 2 {
            warn!(
                "Opus only supports mono/stereo, but WAV has {} channels",
                channels
            );
        }

        // Read samples and convert to f32
        let wav_samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Int => reader
                .samples::<i16>()
                .map(|s| s.unwrap() as f32 / 32768.0)
                .collect(),
            hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
        };

        info!(
            "Loaded {} samples ({:.2} seconds)",
            wav_samples.len(),
            wav_samples.len() as f32 / sample_rate as f32 / channels as f32
        );

        Self::new(user_id, wav_samples, packet_sender)
    }

    fn audio_loop(
        user_id: String,
        audio_data: Vec<f32>,
        packet_sender: Sender<Vec<u8>>,
        quit: Arc<AtomicBool>,
    ) -> anyhow::Result<()> {
        // Audio configuration - targeting 50fps (20ms packets)
        let sample_rate = 48000u32; // Standard Opus rate
        let channels = 1u8; // Mono for simplicity
        let samples_per_packet = (sample_rate as f32 * 0.02) as usize; // 20ms worth
        let packet_interval = Duration::from_millis(20);

        // Create Opus encoder
        let mut opus_encoder = OpusEncoder::new(sample_rate, OpusChannels::Mono, OpusApp::Voip)?;
        info!(
            "Audio producer started for {} ({}Hz, {}ch, {}ms packets)",
            user_id, sample_rate, channels, 20
        );

        let mut audio_position = 0;
        let mut sequence = 0u64;

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("Audio producer stopping for {}", user_id);
                break;
            }

            // Extract 20ms worth of samples (with looping)
            let mut packet_samples = vec![0.0f32; samples_per_packet];
            for item in packet_samples.iter_mut().take(samples_per_packet) {
                *item = audio_data[audio_position % audio_data.len()];
                audio_position += 1;
            }

            // Encode to Opus
            let mut encoded = vec![0u8; 4000];
            match opus_encoder.encode_float(&packet_samples, &mut encoded) {
                Ok(bytes_written) => {
                    encoded.truncate(bytes_written);

                    // Create media packet
                    let media_packet = MediaPacket {
                        email: user_id.clone(),
                        media_type: MediaType::AUDIO.into(),
                        data: encoded,
                        frame_type: "key".to_string(),
                        timestamp: get_timestamp_ms(),
                        audio_metadata: Some(AudioMetadata {
                            sequence,
                            ..Default::default()
                        })
                        .into(),
                        ..Default::default()
                    };

                    // Wrap in packet wrapper
                    let packet_wrapper = PacketWrapper {
                        data: media_packet.write_to_bytes()?,
                        email: user_id.clone(),
                        packet_type: PacketType::MEDIA.into(),
                        ..Default::default()
                    };

                    // Send packet
                    let packet_data = packet_wrapper.write_to_bytes()?;
                    if let Err(e) = packet_sender.try_send(packet_data) {
                        warn!("Failed to send audio packet for {}: {}", user_id, e);
                    }

                    sequence += 1;
                }
                Err(e) => {
                    error!("Opus encoding failed for {}: {}", user_id, e);
                }
            }

            thread::sleep(packet_interval);
        }

        Ok(())
    }

    pub fn stop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AudioProducer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn get_timestamp_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64
}

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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{AudioMetadata, MediaPacket};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

pub struct AudioProducer {
    #[allow(dead_code)]
    user_id: String,
    quit: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioProducer {
    /// Read WAV file duration without loading all samples.
    pub fn wav_duration(wav_path: &str) -> anyhow::Result<Duration> {
        let reader = hound::WavReader::open(wav_path)?;
        let spec = reader.spec();
        let num_samples = reader.len() as u64; // total sample frames
        let duration_ms = num_samples * 1000 / spec.sample_rate as u64;
        Ok(Duration::from_millis(duration_ms))
    }

    pub fn new(
        user_id: String,
        audio_data: Vec<f32>,
        packet_sender: Sender<Vec<u8>>,
        media_start: Instant,
        loop_duration: Duration,
    ) -> anyhow::Result<Self> {
        let quit = Arc::new(AtomicBool::new(false));
        let quit_clone = quit.clone();
        let user_id_clone = user_id.clone();

        let handle = tokio::spawn(async move {
            if let Err(e) = Self::audio_loop(
                user_id_clone,
                audio_data,
                packet_sender,
                quit_clone,
                media_start,
                loop_duration,
            )
            .await
            {
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
        media_start: Instant,
        loop_duration: Duration,
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

        Self::new(
            user_id,
            wav_samples,
            packet_sender,
            media_start,
            loop_duration,
        )
    }

    async fn audio_loop(
        user_id: String,
        audio_data: Vec<f32>,
        packet_sender: Sender<Vec<u8>>,
        quit: Arc<AtomicBool>,
        media_start: Instant,
        loop_duration: Duration,
    ) -> anyhow::Result<()> {
        // Audio configuration - targeting 50fps (20ms packets)
        let sample_rate = 48000u32;
        let channels = 1u8;
        let samples_per_packet = (sample_rate as f32 * 0.02) as usize; // 960 samples = 20ms
        let loop_duration_ms = loop_duration.as_millis() as u64;

        // Create Opus encoder
        let mut opus_encoder = OpusEncoder::new(sample_rate, OpusChannels::Mono, OpusApp::Voip)?;
        info!(
            "Audio producer started for {} ({}Hz, {}ch, {}ms packets)",
            user_id, sample_rate, channels, 20
        );

        // Global monotonic counter — packet metadata needs strictly increasing
        // values even when the audio loop wraps.
        let mut global_sequence: u64 = 0;

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("Audio producer stopping for {}", user_id);
                break;
            }

            // Position within the loop, derived from shared media clock.
            // Both audio and video wrap at loop_duration so they never drift apart.
            let elapsed_ms = media_start.elapsed().as_millis() as u64;
            let position_in_loop_ms = elapsed_ms % loop_duration_ms;
            let packet_in_loop = position_in_loop_ms / 20;
            let audio_position = (position_in_loop_ms as usize * sample_rate as usize / 1000)
                .min(audio_data.len().saturating_sub(samples_per_packet));

            // Sleep until next packet deadline
            let next_packet_ms = (packet_in_loop + 1) * 20;
            // If next packet crosses the loop boundary, sleep until loop restart
            let sleep_target_ms = if next_packet_ms >= loop_duration_ms {
                loop_duration_ms
            } else {
                next_packet_ms
            };
            let absolute_target = media_start
                + Duration::from_millis((elapsed_ms - position_in_loop_ms) + sleep_target_ms);
            let now = Instant::now();
            if now < absolute_target {
                tokio::time::sleep(absolute_target - now).await;
            }

            // Extract 20ms worth of samples from the time-derived position
            let mut packet_samples = vec![0.0f32; samples_per_packet];
            for (i, item) in packet_samples.iter_mut().enumerate() {
                *item = audio_data[(audio_position + i) % audio_data.len()];
            }

            let user_id_bytes = user_id.clone().into_bytes();

            // Encode to Opus
            let mut encoded = vec![0u8; 4000];
            match opus_encoder.encode_float(&packet_samples, &mut encoded) {
                Ok(bytes_written) => {
                    encoded.truncate(bytes_written);

                    // Create media packet
                    let media_packet = MediaPacket {
                        user_id: user_id_bytes.clone(),
                        media_type: MediaType::AUDIO.into(),
                        data: encoded,
                        frame_type: "key".to_string(),
                        timestamp: get_timestamp_ms(),
                        audio_metadata: Some(AudioMetadata {
                            sequence: global_sequence,
                            ..Default::default()
                        })
                        .into(),
                        ..Default::default()
                    };

                    // Wrap in packet wrapper
                    let packet_wrapper = PacketWrapper {
                        data: media_packet.write_to_bytes()?,
                        user_id: user_id_bytes,
                        packet_type: PacketType::MEDIA.into(),
                        ..Default::default()
                    };

                    // Send packet
                    let packet_data = packet_wrapper.write_to_bytes()?;
                    if let Err(e) = packet_sender.try_send(packet_data) {
                        warn!("Failed to send audio packet for {}: {}", user_id, e);
                    }

                    global_sequence += 1;
                }
                Err(e) => {
                    error!("Opus encoding failed for {}: {}", user_id, e);
                }
            }
        }

        Ok(())
    }

    pub fn stop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.abort();
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

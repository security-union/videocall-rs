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

use crate::aq_controller::BotAq;
use crate::transport::{MediaTypeLabel, OutboundFrame};
use opus::{
    Application as OpusApp, Bitrate as OpusBitrate, Channels as OpusChannels,
    Encoder as OpusEncoder,
};
use protobuf::Message;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Sender;
use tracing::{error, info, warn};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{AudioMetadata, MediaPacket};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

const RMS_SPEAKING_ENTER: f32 = 0.012;
const RMS_SPEAKING_EXIT: f32 = 0.008;

fn next_speaking_state(is_speaking: bool, rms: f32) -> bool {
    if is_speaking {
        rms > RMS_SPEAKING_EXIT
    } else {
        rms > RMS_SPEAKING_ENTER
    }
}

pub struct AudioProducer {
    #[allow(dead_code)]
    user_id: String,
    quit: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioProducer {
    /// Read WAV file duration without loading all samples.
    #[allow(dead_code)]
    pub fn wav_duration(wav_path: &str) -> anyhow::Result<Duration> {
        let reader = hound::WavReader::open(wav_path)?;
        let spec = reader.spec();
        let num_samples = reader.len() as u64; // total sample frames
        let duration_ms = num_samples * 1000 / spec.sample_rate as u64;
        Ok(Duration::from_millis(duration_ms))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        user_id: String,
        audio_data: Vec<f32>,
        packet_sender: Sender<OutboundFrame>,
        media_start: Instant,
        loop_duration: Duration,
        is_speaking: Arc<AtomicBool>,
        aq: Arc<BotAq>,
    ) -> anyhow::Result<Self> {
        let quit = Arc::new(AtomicBool::new(false));
        let quit_clone = quit.clone();
        let user_id_clone = user_id.clone();

        let handle = thread::spawn(move || {
            if let Err(e) = Self::audio_loop(
                user_id_clone,
                audio_data,
                packet_sender,
                quit_clone,
                media_start,
                loop_duration,
                is_speaking,
                aq,
            ) {
                error!("Audio producer error: {}", e);
            }
        });

        Ok(AudioProducer {
            user_id,
            quit,
            handle: Some(handle),
        })
    }

    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub fn from_wav_file(
        user_id: String,
        wav_path: &str,
        packet_sender: Sender<OutboundFrame>,
        media_start: Instant,
        loop_duration: Duration,
        is_speaking: Arc<AtomicBool>,
        aq: Arc<BotAq>,
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
            is_speaking,
            aq,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn audio_loop(
        user_id: String,
        audio_data: Vec<f32>,
        packet_sender: Sender<OutboundFrame>,
        quit: Arc<AtomicBool>,
        media_start: Instant,
        loop_duration: Duration,
        is_speaking: Arc<AtomicBool>,
        aq: Arc<BotAq>,
    ) -> anyhow::Result<()> {
        if audio_data.is_empty() {
            warn!("Audio producer for {} has no audio data, exiting", user_id);
            return Ok(());
        }

        // Audio configuration - targeting 50fps (20ms packets)
        let sample_rate = 48000u32;
        let channels = 1u8;
        let samples_per_packet = (sample_rate as f32 * 0.02) as usize; // 960 samples = 20ms
        let packet_interval_us: u64 = 20_000; // 20ms in microseconds
        let loop_duration_us = loop_duration.as_micros() as u64;
        if loop_duration_us == 0 {
            warn!(
                "Audio producer for {} has zero loop duration, exiting",
                user_id
            );
            return Ok(());
        }

        // Create Opus encoder seeded from the AQ controller's current audio tier.
        let mut a = aq.snapshot_audio();
        let mut last_epoch: u64 = aq.tier_epoch();

        let mut opus_encoder = OpusEncoder::new(sample_rate, OpusChannels::Mono, OpusApp::Voip)?;
        // Apply initial AQ-derived settings.
        if let Err(e) = opus_encoder.set_bitrate(OpusBitrate::Bits((a.bitrate_kbps * 1000) as i32))
        {
            warn!(
                "[{}] audio AQ: failed to set initial bitrate {}kbps: {}",
                user_id, a.bitrate_kbps, e
            );
        }
        if let Err(e) = opus_encoder.set_inband_fec(a.fec) {
            warn!(
                "[{}] audio AQ: failed to set initial FEC={}: {}",
                user_id, a.fec, e
            );
        }
        // NOTE: the `opus` 0.3 crate does not expose `set_dtx`. Opus defaults
        // to DTX=off at the codec level, and the software VAD below (RMS <
        // 0.005 skip) already produces DTX-like behavior at the frame layer.
        // When/if we upgrade to an opus crate that exposes OPUS_SET_DTX, the
        // `a.dtx` field is already plumbed through and ready to use.
        // TODO(bot-aq): plumb set_dtx once the opus bindings expose it.
        info!(
            "Audio producer started for {} ({}Hz, {}ch, {}ms packets, AQ tier={}, {}kbps, fec={}, dtx={} [software-only])",
            user_id,
            sample_rate,
            channels,
            20,
            aq.audio_tier_index(),
            a.bitrate_kbps,
            a.fec,
            a.dtx,
        );

        // Global monotonic counter -- packet metadata needs strictly increasing
        // values even when the audio loop wraps.
        let mut global_sequence: u64 = 0;
        let user_id_bytes = user_id.clone().into_bytes();

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("Audio producer stopping for {}", user_id);
                break;
            }

            // Cheap lock-free poll: only re-snapshot on a tier change.
            let current_epoch = aq.tier_epoch();
            if current_epoch != last_epoch {
                let new_a = aq.snapshot_audio();
                last_epoch = current_epoch;

                if new_a.bitrate_kbps != a.bitrate_kbps {
                    if let Err(e) = opus_encoder
                        .set_bitrate(OpusBitrate::Bits((new_a.bitrate_kbps * 1000) as i32))
                    {
                        warn!(
                            "[{}] audio AQ: failed to update bitrate to {}kbps: {}",
                            user_id, new_a.bitrate_kbps, e
                        );
                    }
                }
                if new_a.fec != a.fec {
                    if let Err(e) = opus_encoder.set_inband_fec(new_a.fec) {
                        warn!(
                            "[{}] audio AQ: failed to update FEC to {}: {}",
                            user_id, new_a.fec, e
                        );
                    }
                }
                // DTX flag tracked but not applied (see note above).
                info!(
                    "[{}] audio AQ: tier change bitrate {}->{}kbps, fec {}->{} (dtx {}->{})",
                    user_id, a.bitrate_kbps, new_a.bitrate_kbps, a.fec, new_a.fec, a.dtx, new_a.dtx,
                );
                a = new_a;
            }

            // Position within the loop, derived from shared media clock.
            // Both audio and video wrap at loop_duration so they never drift apart.
            // Position is read AFTER any sleep so it reflects the actual send time,
            // matching the video producer's pattern (encode-then-sleep).
            let elapsed_us = media_start.elapsed().as_micros() as u64;
            let position_in_loop_us = elapsed_us % loop_duration_us;
            let audio_position = (position_in_loop_us as usize * sample_rate as usize / 1_000_000)
                .min(audio_data.len().saturating_sub(samples_per_packet));

            // Extract 20ms worth of samples from the time-derived position
            let mut packet_samples = vec![0.0f32; samples_per_packet];
            for (i, item) in packet_samples.iter_mut().enumerate() {
                *item = audio_data[(audio_position + i) % audio_data.len()];
            }

            // DTX: compute RMS per packet, update is_speaking flag
            let rms = (packet_samples.iter().map(|s| s * s).sum::<f32>()
                / packet_samples.len() as f32)
                .sqrt();
            let speaking_now = next_speaking_state(is_speaking.load(Ordering::Relaxed), rms);
            is_speaking.store(speaking_now, Ordering::Relaxed);

            // Skip encode/send for near-silence packets
            if rms < 0.005 {
                global_sequence += 1;

                // Still need to sleep to maintain timing
                let packet_in_loop = position_in_loop_us / packet_interval_us;
                let next_packet_us = (packet_in_loop + 1) * packet_interval_us;
                let sleep_target_us = if next_packet_us >= loop_duration_us {
                    loop_duration_us
                } else {
                    next_packet_us
                };
                let loop_base_us = elapsed_us - position_in_loop_us;
                let absolute_target =
                    media_start + Duration::from_micros(loop_base_us + sleep_target_us);
                let now = Instant::now();
                if now < absolute_target {
                    thread::sleep(absolute_target - now);
                }
                continue;
            }

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
                        user_id: user_id_bytes.clone(),
                        packet_type: PacketType::MEDIA.into(),
                        ..Default::default()
                    };

                    // Send packet — tagged `Audio` so the outbound shim can
                    // label metrics without re-parsing the wrapper.
                    let packet_data = packet_wrapper.write_to_bytes()?;
                    let frame = OutboundFrame::new(MediaTypeLabel::Audio, packet_data);
                    if let Err(_e) = packet_sender.try_send(frame) {
                        static AUDIO_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
                        let count = AUDIO_DROP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                        if count % 100 == 1 {
                            warn!(
                                "Dropped audio packets due to full send channel (total: {})",
                                count,
                            );
                        }
                    }

                    global_sequence += 1;
                }
                Err(e) => {
                    error!("Opus encoding failed for {}: {}", user_id, e);
                }
            }

            // Sleep until next packet deadline (microsecond precision)
            let packet_in_loop = position_in_loop_us / packet_interval_us;
            let next_packet_us = (packet_in_loop + 1) * packet_interval_us;
            let sleep_target_us = if next_packet_us >= loop_duration_us {
                loop_duration_us
            } else {
                next_packet_us
            };
            let loop_base_us = elapsed_us - position_in_loop_us;
            let absolute_target =
                media_start + Duration::from_micros(loop_base_us + sleep_target_us);
            let now = Instant::now();
            if now < absolute_target {
                thread::sleep(absolute_target - now);
            }
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

#[cfg(test)]
mod tests {
    use super::{next_speaking_state, RMS_SPEAKING_ENTER, RMS_SPEAKING_EXIT};

    #[test]
    fn entering_speaking_requires_upper_threshold() {
        assert!(!next_speaking_state(false, RMS_SPEAKING_EXIT + 0.0005));
        assert!(next_speaking_state(false, RMS_SPEAKING_ENTER + 0.0005));
    }

    #[test]
    fn staying_speaking_uses_lower_threshold() {
        assert!(next_speaking_state(true, RMS_SPEAKING_ENTER - 0.0005));
        assert!(!next_speaking_state(true, RMS_SPEAKING_EXIT - 0.0005));
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

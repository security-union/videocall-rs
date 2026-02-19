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

use image::imageops::FilterType;
use image::{ImageBuffer, ImageReader, Rgb};
use protobuf::Message;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, trace, warn};
use videocall_client::NativeVideoCallClient;
use videocall_codecs::encoder::VideoEncoderBuilder;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{MediaPacket, VideoCodec, VideoMetadata};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

pub struct VideoProducer {
    #[allow(dead_code)]
    user_id: String,
    quit: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl VideoProducer {
    pub fn from_image_sequence(
        user_id: String,
        image_dir: &str,
        client: Arc<NativeVideoCallClient>,
    ) -> anyhow::Result<Self> {
        let quit = Arc::new(AtomicBool::new(false));
        let quit_clone = quit.clone();
        let user_id_clone = user_id.clone();
        let image_dir = image_dir.to_string();

        let handle = thread::spawn(move || {
            if let Err(e) = Self::video_loop(user_id_clone, &image_dir, client, quit_clone) {
                error!("Video producer error: {}", e);
            }
        });

        Ok(VideoProducer {
            user_id,
            quit,
            handle: Some(handle),
        })
    }

    fn video_loop(
        user_id: String,
        image_dir: &str,
        client: Arc<NativeVideoCallClient>,
        quit: Arc<AtomicBool>,
    ) -> anyhow::Result<()> {
        let width = 1280u32;
        let height = 720u32;
        let framerate = 30u32;
        let packet_interval = Duration::from_millis(1000 / framerate as u64);

        info!(
            "Video producer started for {} ({}x{} @ {}fps)",
            user_id, width, height, framerate
        );

        // Load image sequence
        let mut frames = Vec::new();
        for i in 120..125 {
            let path = format!("{image_dir}/output_{i}.jpg");
            match std::fs::read(&path) {
                Ok(img_data) => {
                    let img = ImageReader::new(std::io::Cursor::new(img_data))
                        .with_guessed_format()?
                        .decode()?;

                    let img = img.resize_exact(width, height, FilterType::Nearest);
                    let img = img.to_rgb8();
                    let i420_data = rgb_to_i420(&img);
                    frames.push(i420_data);
                    debug!("Loaded frame: {}", path);
                }
                Err(e) => {
                    warn!("Failed to load frame {}: {}", path, e);
                }
            }
        }

        if frames.is_empty() {
            return Err(anyhow::anyhow!("No frames loaded from {image_dir}"));
        }

        info!("Loaded {} frames for {}", frames.len(), user_id);

        // Initialize VP9 encoder
        let mut video_encoder = VideoEncoderBuilder::new(framerate, 5)
            .set_resolution(width, height)
            .build()?;
        video_encoder.update_bitrate_kbps(500)?;

        let mut frame_iterator = frames.into_iter().cycle();
        let mut sequence = 0u64;

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("Video producer stopping for {}", user_id);
                break;
            }

            let frame_data = frame_iterator.next().unwrap();
            let frames_result = video_encoder.encode(sequence as i64, &frame_data)?;

            for frame in frames_result {
                let media_packet = MediaPacket {
                    media_type: MediaType::VIDEO.into(),
                    data: frame.data.to_vec(),
                    email: user_id.clone(),
                    frame_type: if frame.key { "key" } else { "delta" }.to_string(),
                    timestamp: get_timestamp_ms(),
                    duration: (1000.0 / framerate as f64),
                    video_metadata: Some(VideoMetadata {
                        sequence,
                        codec: VideoCodec::VP9_PROFILE0_LEVEL10_8BIT.into(),
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                };

                let packet_wrapper = PacketWrapper {
                    packet_type: PacketType::MEDIA.into(),
                    email: user_id.clone(),
                    data: media_packet.write_to_bytes()?,
                    ..Default::default()
                };

                // Send via NativeVideoCallClient instead of raw channel
                if let Err(e) = client.send_packet(packet_wrapper) {
                    warn!("Failed to send video packet for {}: {}", user_id, e);
                } else {
                    trace!(
                        "Sent VP9 frame {} ({} bytes, {}) for {}",
                        sequence,
                        frame.data.len(),
                        if frame.key { "key" } else { "delta" },
                        user_id
                    );
                }
            }

            sequence += 1;
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

impl Drop for VideoProducer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn rgb_to_i420(image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> Vec<u8> {
    let width = image.width() as usize;
    let height = image.height() as usize;
    let mut i420_data = vec![0u8; width * height * 3 / 2];

    let rgb = image.as_raw();
    let (y_plane, uv_planes) = i420_data.split_at_mut(width * height);
    let (u_plane, v_plane) = uv_planes.split_at_mut(width * height / 4);

    for y in 0..height {
        for x in 0..width {
            let rgb_index = (y * width + x) * 3;
            let r = rgb[rgb_index] as f32;
            let g = rgb[rgb_index + 1] as f32;
            let b = rgb[rgb_index + 2] as f32;

            let y_value = (0.257 * r + 0.504 * g + 0.098 * b + 16.0).round() as u8;
            let u_value = (-0.148 * r - 0.291 * g + 0.439 * b + 128.0).round() as u8;
            let v_value = (0.439 * r - 0.368 * g - 0.071 * b + 128.0).round() as u8;

            y_plane[y * width + x] = y_value;

            if y % 2 == 0 && x % 2 == 0 {
                let uv_index = (y / 2) * (width / 2) + (x / 2);
                u_plane[uv_index] = u_value;
                v_plane[uv_index] = v_value;
            }
        }
    }

    i420_data
}

fn get_timestamp_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64
}

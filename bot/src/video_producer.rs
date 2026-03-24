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

use crate::video_encoder::VideoEncoderBuilder;
use image::imageops::FilterType;
use image::{ImageBuffer, ImageReader, Rgb};
use protobuf::Message;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, info, trace, warn};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{MediaPacket, VideoCodec, VideoMetadata};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

// Real VP9 encoder - exactly same approach as videocall-cli

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
        packet_sender: Sender<Vec<u8>>,
        media_start: Instant,
        loop_duration: Duration,
    ) -> anyhow::Result<Self> {
        let quit = Arc::new(AtomicBool::new(false));
        let quit_clone = quit.clone();
        let user_id_clone = user_id.clone();
        let image_dir = image_dir.to_string();

        let handle = thread::spawn(move || {
            if let Err(e) = Self::video_loop(
                user_id_clone,
                &image_dir,
                packet_sender,
                quit_clone,
                media_start,
                loop_duration,
            ) {
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
        packet_sender: Sender<Vec<u8>>,
        quit: Arc<AtomicBool>,
        media_start: Instant,
        loop_duration: Duration,
    ) -> anyhow::Result<()> {
        // Video configuration - 15fps (~66ms packets)
        // Lower fps makes small A/V timing offsets less perceptible.
        let width = 1280u32;
        let height = 720u32;
        let framerate = 15u32;
        let packet_interval = Duration::from_millis(1000 / framerate as u64);

        info!(
            "Video producer started for {} ({}x{} @ {}fps)",
            user_id, width, height, framerate
        );

        // Load image sequence — try frame_NNNNN.jpg pattern first, fall back to legacy
        let mut frame_paths: Vec<std::path::PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(image_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("frame_") && name.ends_with(".jpg") {
                        frame_paths.push(p);
                    }
                }
            }
        }
        frame_paths.sort();

        // Fall back to legacy output_120..124 pattern
        if frame_paths.is_empty() {
            for i in 120..125 {
                let p = std::path::PathBuf::from(format!("{image_dir}/output_{i}.jpg"));
                if p.exists() {
                    frame_paths.push(p);
                }
            }
        }

        // Load raw JPEG bytes into memory (much smaller than decoded I420).
        // Frames are decoded on-the-fly during encoding to avoid OOM with large sequences.
        let mut jpeg_frames: Vec<Vec<u8>> = Vec::new();
        for path in &frame_paths {
            match std::fs::read(path) {
                Ok(img_data) => {
                    jpeg_frames.push(img_data);
                    debug!("Loaded frame: {}", path.display());
                }
                Err(e) => {
                    warn!("Failed to load frame {}: {}", path.display(), e);
                }
            }
        }

        if jpeg_frames.is_empty() {
            return Err(anyhow::anyhow!("No frames loaded from {image_dir}"));
        }

        info!(
            "Loaded {} frames for {} ({:.1} MB compressed)",
            jpeg_frames.len(),
            user_id,
            jpeg_frames.iter().map(|f| f.len()).sum::<usize>() as f64 / 1_048_576.0
        );

        // Initialize VP9 encoder (exactly same as videocall-cli)
        let mut video_encoder = VideoEncoderBuilder::new(framerate, 5) // cpu_used=5 like videocall-cli
            .set_resolution(width, height)
            .build()?;
        video_encoder.update_bitrate_kbps(500)?; // 500kbps default like videocall-cli

        let interval_ms = packet_interval.as_millis() as u64;
        let loop_duration_ms = loop_duration.as_millis() as u64;
        let mut prev_frame_index: Option<usize> = None;
        // Global monotonic counter — VP9 encoder and packet metadata need
        // strictly increasing values. Only frame_index wraps with the loop.
        let mut global_sequence: u64 = 0;

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("Video producer stopping for {}", user_id);
                break;
            }

            // Position within the loop, derived from shared media clock.
            // Both audio and video wrap at loop_duration so they never drift apart.
            let elapsed_ms = media_start.elapsed().as_millis() as u64;
            let position_in_loop_ms = elapsed_ms % loop_duration_ms;
            let frame_in_loop = position_in_loop_ms / interval_ms;
            let frame_index = (frame_in_loop as usize).min(jpeg_frames.len() - 1);

            // Detect loop wrap: frame_index jumped backwards → force a keyframe
            // so the browser's VP9 decoder can immediately show the new content.
            let force_keyframe = match prev_frame_index {
                Some(prev) => frame_index < prev,
                None => true, // first frame is always a keyframe
            };
            prev_frame_index = Some(frame_index);

            // Periodic diagnostics (every 5 seconds)
            if global_sequence.is_multiple_of(framerate as u64 * 5) {
                let loop_num = elapsed_ms / loop_duration_ms;
                info!(
                    "[{}] seq={}, frame={}/{}, loop={}, pos={:.1}s/{}s{}",
                    user_id,
                    global_sequence,
                    frame_index,
                    jpeg_frames.len(),
                    loop_num,
                    position_in_loop_ms as f64 / 1000.0,
                    loop_duration_ms / 1000,
                    if force_keyframe { " KEYFRAME" } else { "" }
                );
            }

            // Decode the frame corresponding to current elapsed time
            let jpeg_data = &jpeg_frames[frame_index];

            let img = ImageReader::new(std::io::Cursor::new(jpeg_data))
                .with_guessed_format()?
                .decode()?;
            let img = img.resize_exact(width, height, FilterType::Nearest);
            let img = img.to_rgb8();
            let frame_data = rgb_to_i420(&img);

            // Encode to VP9 — force keyframe at loop boundaries.
            // PTS must be monotonically increasing (global_sequence), NOT the
            // loop-relative frame index, or the encoder drops/corrupts frames.
            let frames_result = if force_keyframe {
                info!(
                    "Forcing keyframe at loop boundary for {} (seq={})",
                    user_id, global_sequence
                );
                video_encoder.encode_keyframe(global_sequence as i64, &frame_data)?
            } else {
                video_encoder.encode(global_sequence as i64, &frame_data)?
            };

            for frame in frames_result {
                let media_packet = MediaPacket {
                    media_type: MediaType::VIDEO.into(),
                    data: frame.data.to_vec(),
                    user_id: user_id.clone().into_bytes(),
                    frame_type: if frame.key { "key" } else { "delta" }.to_string(),
                    timestamp: get_timestamp_ms(),
                    duration: (1000.0 / framerate as f64),
                    video_metadata: Some(VideoMetadata {
                        sequence: global_sequence,
                        codec: VideoCodec::VP9_PROFILE0_LEVEL10_8BIT.into(),
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                };

                let packet_wrapper = PacketWrapper {
                    packet_type: PacketType::MEDIA.into(),
                    user_id: user_id.clone().into_bytes(),
                    data: media_packet.write_to_bytes()?,
                    ..Default::default()
                };

                let packet_data = packet_wrapper.write_to_bytes()?;
                if let Err(e) = packet_sender.try_send(packet_data) {
                    warn!("Failed to send video packet for {}: {}", user_id, e);
                } else {
                    trace!(
                        "Sent VP9 frame {} ({} bytes, {}) for {}",
                        global_sequence,
                        frame.data.len(),
                        if frame.key { "key" } else { "delta" },
                        user_id
                    );
                }
            }

            global_sequence += 1;

            // Sleep until next frame deadline
            let next_frame_ms = (frame_in_loop + 1) * interval_ms;
            let sleep_target_ms = if next_frame_ms >= loop_duration_ms {
                loop_duration_ms
            } else {
                next_frame_ms
            };
            let loop_base_ms = elapsed_ms - position_in_loop_ms;
            let absolute_target =
                media_start + Duration::from_millis(loop_base_ms + sleep_target_ms);
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

impl Drop for VideoProducer {
    fn drop(&mut self) {
        self.stop();
    }
}

// VP9 encoder implemented using exact same approach as videocall-cli

// Convert RGB image to I420 format (same as videocall-cli)
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

            // Calculate Y, U, V components
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

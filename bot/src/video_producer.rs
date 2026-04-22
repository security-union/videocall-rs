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

use crate::costume_renderer::CostumeRenderer;
use crate::ekg_renderer::EkgRenderer;
use crate::video_encoder::VideoEncoderBuilder;
use image::{ImageBuffer, Rgb};
use protobuf::Message;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Sender;
use tracing::{error, info, trace, warn};
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
    /// Create a video producer that renders EKG frames on-the-fly.
    ///
    /// No pre-generation needed -- each frame is rendered from RMS data
    /// directly in the video loop (~0.5ms per frame vs ~5ms for VP9 encode).
    pub fn from_ekg(
        user_id: String,
        renderer: EkgRenderer,
        rms: Vec<f32>,
        max_rms: f32,
        packet_sender: Sender<Vec<u8>>,
        media_start: Instant,
        loop_duration: Duration,
    ) -> anyhow::Result<Self> {
        let quit = Arc::new(AtomicBool::new(false));
        let quit_clone = quit.clone();
        let user_id_clone = user_id.clone();

        let handle = thread::spawn(move || {
            if let Err(e) = Self::ekg_video_loop(
                user_id_clone,
                renderer,
                rms,
                max_rms,
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

    /// Create a video producer that renders costume sprite sheet frames.
    ///
    /// Reads pre-rendered I420 frames from the CostumeRenderer at 30fps.
    pub fn from_costume(
        user_id: String,
        renderer: CostumeRenderer,
        packet_sender: Sender<Vec<u8>>,
        media_start: Instant,
        loop_duration: Duration,
        is_speaking: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        let quit = Arc::new(AtomicBool::new(false));
        let quit_clone = quit.clone();
        let user_id_clone = user_id.clone();

        let handle = thread::spawn(move || {
            if let Err(e) = Self::costume_video_loop(
                user_id_clone,
                renderer,
                packet_sender,
                quit_clone,
                media_start,
                loop_duration,
                is_speaking,
            ) {
                error!("Costume video producer error: {}", e);
            }
        });

        Ok(VideoProducer {
            user_id,
            quit,
            handle: Some(handle),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn ekg_video_loop(
        user_id: String,
        renderer: EkgRenderer,
        rms: Vec<f32>,
        max_rms: f32,
        packet_sender: Sender<Vec<u8>>,
        quit: Arc<AtomicBool>,
        media_start: Instant,
        loop_duration: Duration,
    ) -> anyhow::Result<()> {
        let width = 1280u32;
        let height = 720u32;
        let framerate = 15u32;
        // Use microseconds to avoid truncation drift (1000/15 = 66.667ms,
        // truncating to 66ms drifts ~10ms/sec = ~840ms over 84s).
        let frame_interval_us: u64 = 1_000_000 / framerate as u64; // 66666us

        info!(
            "Video producer started for {} ({}x{} @ {}fps, on-the-fly EKG)",
            user_id, width, height, framerate
        );

        let mut video_encoder = VideoEncoderBuilder::new(framerate, 5)
            .set_resolution(width, height)
            .build()?;
        video_encoder.update_bitrate_kbps(500)?;

        let loop_duration_us = loop_duration.as_micros() as u64;
        if loop_duration_us == 0 {
            warn!(
                "Video producer for {} has zero loop duration, exiting",
                user_id
            );
            return Ok(());
        }
        let mut prev_frame_index: Option<usize> = None;
        let mut global_sequence: u64 = 0;

        // Pre-allocate reusable buffers to avoid per-frame heap allocation
        let mut frame_buf = renderer.create_frame_buffer();
        let mut i420_buf = vec![0u8; (width * height * 3 / 2) as usize];
        let user_id_bytes = user_id.clone().into_bytes();

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("Video producer stopping for {}", user_id);
                break;
            }

            let elapsed_us = media_start.elapsed().as_micros() as u64;
            let position_in_loop_us = elapsed_us % loop_duration_us;
            let frame_in_loop = (position_in_loop_us / frame_interval_us) as usize;

            // Force keyframe at loop wrap, first frame, or every 5 seconds
            let at_loop_wrap = match prev_frame_index {
                Some(prev) => frame_in_loop < prev,
                None => true,
            };
            let periodic_keyframe = global_sequence.is_multiple_of(framerate as u64 * 5);
            let force_keyframe = at_loop_wrap || periodic_keyframe;
            prev_frame_index = Some(frame_in_loop);

            if global_sequence.is_multiple_of(framerate as u64 * 5) {
                let loop_num = elapsed_us / loop_duration_us;
                info!(
                    "[{}] seq={}, frame={}, loop={}, pos={:.1}s/{:.1}s{}",
                    user_id,
                    global_sequence,
                    frame_in_loop,
                    loop_num,
                    position_in_loop_us as f64 / 1_000_000.0,
                    loop_duration_us as f64 / 1_000_000.0,
                    if force_keyframe { " KEYFRAME" } else { "" }
                );
            }

            // Render EKG frame on-the-fly (< 1ms)
            let rms_value = if frame_in_loop < rms.len() {
                rms[frame_in_loop]
            } else {
                0.0
            };
            renderer.render_frame_rgb_into(&mut frame_buf, rms_value, max_rms, frame_in_loop);
            rgb_to_i420_into(&frame_buf, &mut i420_buf);

            // Encode to VP9
            let frames_result = if force_keyframe {
                info!("Forcing keyframe for {} (seq={})", user_id, global_sequence);
                video_encoder.encode_keyframe(global_sequence as i64, &i420_buf)?
            } else {
                video_encoder.encode(global_sequence as i64, &i420_buf)?
            };

            for frame in frames_result {
                let media_packet = MediaPacket {
                    media_type: MediaType::VIDEO.into(),
                    data: frame.data.to_vec(),
                    user_id: user_id_bytes.clone(),
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
                    user_id: user_id_bytes.clone(),
                    data: media_packet.write_to_bytes()?,
                    ..Default::default()
                };

                let packet_data = packet_wrapper.write_to_bytes()?;
                if let Err(_e) = packet_sender.try_send(packet_data) {
                    static VIDEO_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
                    let count = VIDEO_DROP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                    if count % 100 == 1 {
                        warn!(
                            "Dropped video packets due to full send channel (total: {})",
                            count,
                        );
                    }
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

            // Sleep until next frame deadline (microsecond precision)
            let next_frame_us = (frame_in_loop as u64 + 1) * frame_interval_us;
            let sleep_target_us = if next_frame_us >= loop_duration_us {
                loop_duration_us
            } else {
                next_frame_us
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

    #[allow(clippy::too_many_arguments)]
    fn costume_video_loop(
        user_id: String,
        renderer: CostumeRenderer,
        packet_sender: Sender<Vec<u8>>,
        quit: Arc<AtomicBool>,
        media_start: Instant,
        loop_duration: Duration,
        is_speaking: Arc<AtomicBool>,
    ) -> anyhow::Result<()> {
        let width = renderer.width();
        let height = renderer.height();
        let framerate = 30u32;
        let frame_interval_us: u64 = 1_000_000 / framerate as u64; // 33333us

        info!(
            "Costume video producer started for {} ({}x{} @ {}fps)",
            user_id, width, height, framerate
        );

        let mut video_encoder = VideoEncoderBuilder::new(framerate, 5)
            .set_resolution(width, height)
            .build()?;
        video_encoder.update_bitrate_kbps(1000)?;

        let loop_duration_us = loop_duration.as_micros() as u64;
        if loop_duration_us == 0 {
            warn!(
                "Costume video producer for {} has zero loop duration, exiting",
                user_id
            );
            return Ok(());
        }
        let mut prev_frame_index: Option<usize> = None;
        let mut global_sequence: u64 = 0;
        let user_id_bytes = user_id.clone().into_bytes();

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("Costume video producer stopping for {}", user_id);
                break;
            }

            let elapsed_us = media_start.elapsed().as_micros() as u64;
            let position_in_loop_us = elapsed_us % loop_duration_us;
            let frame_in_loop = (position_in_loop_us / frame_interval_us) as usize;

            // Force keyframe at loop wrap, first frame, or every 5 seconds
            let at_loop_wrap = match prev_frame_index {
                Some(prev) => frame_in_loop < prev,
                None => true,
            };
            let periodic_keyframe = global_sequence.is_multiple_of(framerate as u64 * 5);
            let force_keyframe = at_loop_wrap || periodic_keyframe;
            prev_frame_index = Some(frame_in_loop);

            if global_sequence.is_multiple_of(framerate as u64 * 5) {
                let loop_num = elapsed_us / loop_duration_us;
                info!(
                    "[{}] costume seq={}, frame={}, loop={}, pos={:.1}s/{:.1}s{}",
                    user_id,
                    global_sequence,
                    frame_in_loop,
                    loop_num,
                    position_in_loop_us as f64 / 1_000_000.0,
                    loop_duration_us as f64 / 1_000_000.0,
                    if force_keyframe { " KEYFRAME" } else { "" }
                );
            }

            // Read I420 frame directly from costume renderer
            let speaking = is_speaking.load(Ordering::Relaxed);
            let rms_value = if speaking { 0.1 } else { 0.0 };
            let i420_data = renderer.frame_i420(rms_value, frame_in_loop);

            // Encode to VP9
            let frames_result = if force_keyframe {
                info!(
                    "Forcing keyframe for {} (costume seq={})",
                    user_id, global_sequence
                );
                video_encoder.encode_keyframe(global_sequence as i64, i420_data)?
            } else {
                video_encoder.encode(global_sequence as i64, i420_data)?
            };

            for frame in frames_result {
                let media_packet = MediaPacket {
                    media_type: MediaType::VIDEO.into(),
                    data: frame.data.to_vec(),
                    user_id: user_id_bytes.clone(),
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
                    user_id: user_id_bytes.clone(),
                    data: media_packet.write_to_bytes()?,
                    ..Default::default()
                };

                let packet_data = packet_wrapper.write_to_bytes()?;
                if let Err(_e) = packet_sender.try_send(packet_data) {
                    static COSTUME_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
                    let count = COSTUME_DROP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                    if count % 100 == 1 {
                        warn!(
                            "Dropped costume video packets due to full send channel (total: {})",
                            count,
                        );
                    }
                } else {
                    trace!(
                        "Sent costume VP9 frame {} ({} bytes, {}) for {}",
                        global_sequence,
                        frame.data.len(),
                        if frame.key { "key" } else { "delta" },
                        user_id
                    );
                }
            }

            global_sequence += 1;

            // Sleep until next frame deadline (microsecond precision)
            let next_frame_us = (frame_in_loop as u64 + 1) * frame_interval_us;
            let sleep_target_us = if next_frame_us >= loop_duration_us {
                loop_duration_us
            } else {
                next_frame_us
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

impl Drop for VideoProducer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn rgb_to_i420_into(image: &ImageBuffer<Rgb<u8>, Vec<u8>>, i420_data: &mut [u8]) {
    let width = image.width() as usize;
    let height = image.height() as usize;
    debug_assert_eq!(i420_data.len(), width * height * 3 / 2);

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
}

fn get_timestamp_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64
}

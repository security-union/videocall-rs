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
    #[allow(clippy::too_many_arguments)]
    pub fn from_ekg(
        user_id: String,
        renderer: EkgRenderer,
        rms: Vec<f32>,
        max_rms: f32,
        packet_sender: Sender<Vec<u8>>,
        media_start: Instant,
        loop_duration: Duration,
        aq: Arc<BotAq>,
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
                aq,
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
    #[allow(clippy::too_many_arguments)]
    pub fn from_costume(
        user_id: String,
        renderer: CostumeRenderer,
        packet_sender: Sender<Vec<u8>>,
        media_start: Instant,
        loop_duration: Duration,
        is_speaking: Arc<AtomicBool>,
        aq: Arc<BotAq>,
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
                aq,
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
        aq: Arc<BotAq>,
    ) -> anyhow::Result<()> {
        // Seed encoder configuration from the AQ controller's current tier.
        // `framerate` is driven by AQ (browser client EKG was 15 FPS; we honor
        // whatever the tier says so step-downs actually slow the encoder).
        let mut v = aq.snapshot_video();
        let mut last_epoch: u64 = aq.tier_epoch();
        let mut width: u32 = v.max_width;
        let mut height: u32 = v.max_height;
        let mut framerate: u32 = v.target_fps.max(1);
        let mut frames_per_keyframe: u32 = v.keyframe_interval.max(1);
        let mut frame_interval_us: u64 = 1_000_000 / framerate as u64;

        info!(
            "Video producer started for {} ({}x{} @ {}fps, bitrate={}kbps, kf_interval={}, on-the-fly EKG, AQ tier={})",
            user_id,
            width,
            height,
            framerate,
            v.bitrate_kbps,
            frames_per_keyframe,
            aq.video_tier_index(),
        );

        let mut video_encoder = VideoEncoderBuilder::new(framerate, 5)
            .set_resolution(width, height)
            .build()?;
        video_encoder.update_bitrate_kbps(v.bitrate_kbps)?;

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

        // Pre-allocate reusable buffers to avoid per-frame heap allocation.
        // These have to be rebuilt whenever the AQ tier changes resolution.
        let mut renderer = renderer;
        let renderer_color = renderer.color();
        let mut frame_buf = renderer.create_frame_buffer();
        let mut i420_buf = vec![0u8; (width * height * 3 / 2) as usize];
        let user_id_bytes = user_id.clone().into_bytes();

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("Video producer stopping for {}", user_id);
                break;
            }

            // Cheap lock-free poll: only re-snapshot when AQ actually changed
            // the tier. Steady-state cost is one relaxed load per frame.
            let current_epoch = aq.tier_epoch();
            if current_epoch != last_epoch {
                let new_v = aq.snapshot_video();
                last_epoch = current_epoch;

                // Bitrate always changes on a tier transition — cheap update.
                if new_v.bitrate_kbps != v.bitrate_kbps {
                    if let Err(e) = video_encoder.update_bitrate_kbps(new_v.bitrate_kbps) {
                        warn!(
                            "[{}] AQ: failed to update bitrate to {}kbps: {}",
                            user_id, new_v.bitrate_kbps, e
                        );
                    }
                }

                // Resolution or FPS change: rebuild the encoder + renderer.
                // This is expensive (codec re-init ~10-50ms) but rare — tier
                // transitions are rate-limited by MIN_TIER_TRANSITION_INTERVAL_MS
                // (3s) and STEP_UP_STABILIZATION_WINDOW_MS (5s) so this won't
                // run faster than once every few seconds.
                let fps = new_v.target_fps.max(1);
                if new_v.max_width != width || new_v.max_height != height || fps != framerate {
                    info!(
                        "[{}] AQ: rebuilding encoder {}x{}@{}fps -> {}x{}@{}fps",
                        user_id, width, height, framerate, new_v.max_width, new_v.max_height, fps,
                    );
                    width = new_v.max_width;
                    height = new_v.max_height;
                    framerate = fps;
                    frame_interval_us = 1_000_000 / framerate as u64;

                    // Rebuild encoder at new dimensions.
                    match VideoEncoderBuilder::new(framerate, 5)
                        .set_resolution(width, height)
                        .build()
                    {
                        Ok(mut enc) => {
                            let _ = enc.update_bitrate_kbps(new_v.bitrate_kbps);
                            video_encoder = enc;
                            // Rebuild renderer + reusable buffers.
                            renderer = EkgRenderer::new(renderer_color, width, height);
                            frame_buf = renderer.create_frame_buffer();
                            i420_buf = vec![0u8; (width * height * 3 / 2) as usize];
                            // Force a keyframe on the next frame — the decoder
                            // must see the new resolution as a clean IDR.
                            prev_frame_index = None;
                        }
                        Err(e) => {
                            error!(
                                "[{}] AQ: failed to rebuild encoder at {}x{}@{}fps: {} — keeping old encoder",
                                user_id, width, height, framerate, e
                            );
                            // Revert our shadow values so we don't diverge
                            // from the real encoder state.
                            width = v.max_width;
                            height = v.max_height;
                            framerate = v.target_fps.max(1);
                            frame_interval_us = 1_000_000 / framerate as u64;
                        }
                    }
                }

                frames_per_keyframe = new_v.keyframe_interval.max(1);
                v = new_v;
            }

            let elapsed_us = media_start.elapsed().as_micros() as u64;
            let position_in_loop_us = elapsed_us % loop_duration_us;
            let frame_in_loop = (position_in_loop_us / frame_interval_us) as usize;

            // Force keyframe at loop wrap, first frame, or every keyframe_interval frames.
            let at_loop_wrap = match prev_frame_index {
                Some(prev) => frame_in_loop < prev,
                None => true,
            };
            let periodic_keyframe = global_sequence.is_multiple_of(frames_per_keyframe as u64);
            let force_keyframe = at_loop_wrap || periodic_keyframe;
            prev_frame_index = Some(frame_in_loop);

            if global_sequence.is_multiple_of(framerate as u64 * 5) {
                let loop_num = elapsed_us / loop_duration_us;
                info!(
                    "[{}] seq={}, frame={}, loop={}, pos={:.1}s/{:.1}s, tier={}{}",
                    user_id,
                    global_sequence,
                    frame_in_loop,
                    loop_num,
                    position_in_loop_us as f64 / 1_000_000.0,
                    loop_duration_us as f64 / 1_000_000.0,
                    aq.video_tier_index(),
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
        aq: Arc<BotAq>,
    ) -> anyhow::Result<()> {
        let width = renderer.width();
        let height = renderer.height();
        // Costume path renders from a pre-baked 1280x720 I420 sprite sheet and
        // cannot be rescaled at runtime. AQ bitrate + FPS changes are honored;
        // resolution changes are logged-and-ignored (see below).
        let mut v = aq.snapshot_video();
        let mut last_epoch: u64 = aq.tier_epoch();
        let mut framerate: u32 = v.target_fps.max(1);
        let mut frames_per_keyframe: u32 = v.keyframe_interval.max(1);
        let mut frame_interval_us: u64 = 1_000_000 / framerate as u64;

        info!(
            "Costume video producer started for {} ({}x{} @ {}fps, bitrate={}kbps, AQ tier={})",
            user_id,
            width,
            height,
            framerate,
            v.bitrate_kbps,
            aq.video_tier_index(),
        );

        let mut video_encoder = VideoEncoderBuilder::new(framerate, 5)
            .set_resolution(width, height)
            .build()?;
        video_encoder.update_bitrate_kbps(v.bitrate_kbps)?;

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

        // Warn at most once when AQ requests a resolution below costume's
        // baked-in 1280x720. Subsequent resolution-change requests at this
        // tier are silently ignored.
        static COSTUME_RES_WARNED: AtomicBool = AtomicBool::new(false);

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("Costume video producer stopping for {}", user_id);
                break;
            }

            // Cheap lock-free poll: only re-snapshot when AQ actually changed
            // the tier. Costume path honors bitrate + FPS, ignores resolution.
            let current_epoch = aq.tier_epoch();
            if current_epoch != last_epoch {
                let new_v = aq.snapshot_video();
                last_epoch = current_epoch;

                // Bitrate change — cheap encoder update.
                if new_v.bitrate_kbps != v.bitrate_kbps {
                    if let Err(e) = video_encoder.update_bitrate_kbps(new_v.bitrate_kbps) {
                        warn!(
                            "[{}] AQ (costume): failed to update bitrate to {}kbps: {}",
                            user_id, new_v.bitrate_kbps, e
                        );
                    }
                }

                // FPS change — rebuild the encoder (VP9 timebase is in cfg).
                let fps = new_v.target_fps.max(1);
                if fps != framerate {
                    info!(
                        "[{}] AQ (costume): rebuilding encoder {}fps -> {}fps at fixed {}x{}",
                        user_id, framerate, fps, width, height,
                    );
                    framerate = fps;
                    frame_interval_us = 1_000_000 / framerate as u64;
                    match VideoEncoderBuilder::new(framerate, 5)
                        .set_resolution(width, height)
                        .build()
                    {
                        Ok(mut enc) => {
                            let _ = enc.update_bitrate_kbps(new_v.bitrate_kbps);
                            video_encoder = enc;
                            prev_frame_index = None; // force keyframe on next iter
                        }
                        Err(e) => {
                            error!(
                                "[{}] AQ (costume): failed to rebuild encoder at {}fps: {} — keeping old encoder",
                                user_id, framerate, e
                            );
                            framerate = v.target_fps.max(1);
                            frame_interval_us = 1_000_000 / framerate as u64;
                        }
                    }
                }

                // Resolution change request: log once, then ignore. Costumes
                // are pre-baked I420 sprite sheets at 1280x720 and cannot be
                // rescaled cheaply on the hot path. Lifting this is tracked
                // as a follow-up (dynamic rescale of costume frames).
                if (new_v.max_width != width || new_v.max_height != height)
                    && !COSTUME_RES_WARNED.swap(true, Ordering::Relaxed)
                {
                    warn!(
                        "[{}] AQ (costume): tier requested {}x{} but costume resolution is fixed at {}x{} in v1 — keeping native",
                        user_id, new_v.max_width, new_v.max_height, width, height,
                    );
                }

                frames_per_keyframe = new_v.keyframe_interval.max(1);
                v = new_v;
            }

            let elapsed_us = media_start.elapsed().as_micros() as u64;
            let position_in_loop_us = elapsed_us % loop_duration_us;
            let frame_in_loop = (position_in_loop_us / frame_interval_us) as usize;

            // Force keyframe at loop wrap, first frame, or every keyframe_interval frames.
            let at_loop_wrap = match prev_frame_index {
                Some(prev) => frame_in_loop < prev,
                None => true,
            };
            let periodic_keyframe = global_sequence.is_multiple_of(frames_per_keyframe as u64);
            let force_keyframe = at_loop_wrap || periodic_keyframe;
            prev_frame_index = Some(frame_in_loop);

            if global_sequence.is_multiple_of(framerate as u64 * 5) {
                let loop_num = elapsed_us / loop_duration_us;
                info!(
                    "[{}] costume seq={}, frame={}, loop={}, pos={:.1}s/{:.1}s, tier={}{}",
                    user_id,
                    global_sequence,
                    frame_in_loop,
                    loop_num,
                    position_in_loop_us as f64 / 1_000_000.0,
                    loop_duration_us as f64 / 1_000_000.0,
                    aq.video_tier_index(),
                    if force_keyframe { " KEYFRAME" } else { "" }
                );
            }

            // Read I420 frame directly from costume renderer
            let speaking = is_speaking.load(Ordering::Relaxed);
            let i420_data = renderer.frame_i420(speaking, frame_in_loop);

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

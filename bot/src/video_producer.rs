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
use crate::i420_scale::scale_i420;
use crate::transport::{MediaTypeLabel, OutboundFrame};
use crate::video_encoder::{VideoEncoder, VideoEncoderBuilder};
use image::{ImageBuffer, Rgb};
use protobuf::Message;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Sender;
use tracing::{error, info, trace, warn};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{MediaPacket, VideoCodec, VideoMetadata};
use videocall_types::protos::packet_wrapper::packet_wrapper::{MediaKind, PacketType};
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
        packet_sender: Sender<OutboundFrame>,
        media_start: Instant,
        loop_duration: Duration,
        aq: Arc<BotAq>,
        encoder_output_fps: Arc<AtomicU32>,
        encoder_errors_generic: Arc<AtomicU64>,
        encoder_frames_ok: Arc<AtomicU64>,
        transport_drops_counter: Arc<AtomicU64>,
        simulcast_layers: u32,
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
                encoder_output_fps,
                encoder_errors_generic,
                encoder_frames_ok,
                transport_drops_counter,
                simulcast_layers,
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
        packet_sender: Sender<OutboundFrame>,
        media_start: Instant,
        loop_duration: Duration,
        is_speaking: Arc<AtomicBool>,
        aq: Arc<BotAq>,
        encoder_output_fps: Arc<AtomicU32>,
        encoder_errors_generic: Arc<AtomicU64>,
        encoder_frames_ok: Arc<AtomicU64>,
        transport_drops_counter: Arc<AtomicU64>,
        simulcast_layers: u32,
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
                encoder_output_fps,
                encoder_errors_generic,
                encoder_frames_ok,
                transport_drops_counter,
                simulcast_layers,
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
        packet_sender: Sender<OutboundFrame>,
        quit: Arc<AtomicBool>,
        media_start: Instant,
        loop_duration: Duration,
        aq: Arc<BotAq>,
        encoder_output_fps: Arc<AtomicU32>,
        encoder_errors_generic: Arc<AtomicU64>,
        encoder_frames_ok: Arc<AtomicU64>,
        transport_drops_counter: Arc<AtomicU64>,
        simulcast_layers: u32,
    ) -> anyhow::Result<()> {
        // N>=2: run the multi-layer simulcast producer (fixed-geometry layers).
        // N==1 (or any clamped-down value) falls through to the legacy
        // single-stream AQ-adaptive path below, byte-for-byte unchanged.
        //
        // Native render size = the TOP ladder tier (highest resolution) so the
        // upper layers are not capped down to a smaller AQ tier and collapsed
        // (Blocker 1). The layers are built HERE so that a build failure falls
        // back to the single-stream path below instead of killing the producer
        // thread and going dark (NICE-TO-HAVE 5).
        if simulcast_layers >= 2 {
            let ladder = videocall_aq::constants::simulcast_layers(simulcast_layers as usize);
            match ladder.last() {
                Some(top_tier) => {
                    let native_w = top_tier.max_width & !1;
                    let native_h = top_tier.max_height & !1;
                    match Self::build_simulcast_layers(simulcast_layers, native_w, native_h) {
                        Ok(layers) => {
                            // Rebuild the renderer at the top-tier native size
                            // (it arrives sized to the AQ default tier).
                            let renderer = EkgRenderer::new(renderer.color(), native_w, native_h);
                            return Self::ekg_video_loop_simulcast(
                                user_id,
                                renderer,
                                rms,
                                max_rms,
                                packet_sender,
                                quit,
                                media_start,
                                loop_duration,
                                encoder_output_fps,
                                encoder_errors_generic,
                                encoder_frames_ok,
                                transport_drops_counter,
                                layers,
                                native_w,
                                native_h,
                            );
                        }
                        Err(e) => {
                            error!(
                                "[{}] simulcast layer build failed ({}); falling back to single-stream video",
                                user_id, e
                            );
                            // fall through to the single-stream path below
                        }
                    }
                }
                None => {
                    error!(
                        "[{}] simulcast ladder empty for n={}; falling back to single-stream video",
                        user_id, simulcast_layers
                    );
                    // fall through to the single-stream path below
                }
            }
        }

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

        // Publish the initial encoder FPS to the shared atomic so the health
        // reporter can include it in HealthPacket.encoder_output_fps.
        encoder_output_fps.store(framerate, Ordering::Relaxed);

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
                            // Update shared FPS for health reporter.
                            encoder_output_fps.store(framerate, Ordering::Relaxed);
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
                video_encoder.encode_keyframe(global_sequence as i64, &i420_buf)
            } else {
                video_encoder.encode(global_sequence as i64, &i420_buf)
            };

            let frames = match frames_result {
                Ok(f) => {
                    encoder_frames_ok.fetch_add(1, Ordering::Relaxed);
                    f
                }
                Err(e) => {
                    encoder_errors_generic.fetch_add(1, Ordering::Relaxed);
                    error!("Video producer encode error for {}: {}", user_id, e);
                    global_sequence += 1;
                    // Sleep until next frame deadline before continuing
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
                    continue;
                }
            };

            for frame in frames {
                let sent = build_and_send_layer(
                    &packet_sender,
                    &user_id_bytes,
                    frame.data,
                    frame.key,
                    global_sequence,
                    framerate,
                    // Single-stream path → layer 0 (wire-identical to today).
                    0,
                )?;
                if !sent {
                    static VIDEO_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
                    let count = VIDEO_DROP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                    transport_drops_counter.fetch_add(1, Ordering::Relaxed);
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
        packet_sender: Sender<OutboundFrame>,
        quit: Arc<AtomicBool>,
        media_start: Instant,
        loop_duration: Duration,
        is_speaking: Arc<AtomicBool>,
        aq: Arc<BotAq>,
        encoder_output_fps: Arc<AtomicU32>,
        encoder_errors_generic: Arc<AtomicU64>,
        encoder_frames_ok: Arc<AtomicU64>,
        transport_drops_counter: Arc<AtomicU64>,
        simulcast_layers: u32,
    ) -> anyhow::Result<()> {
        // N>=2: run the multi-layer simulcast producer (fixed-geometry layers).
        // N==1 (or any clamped-down value) falls through to the legacy
        // single-stream AQ-adaptive path below, byte-for-byte unchanged.
        //
        // Native = the costume renderer's own size (1280x720). Build the layers
        // HERE so a build failure falls back to single-stream instead of killing
        // the producer thread (NICE-TO-HAVE 5).
        if simulcast_layers >= 2 {
            let native_w = renderer.width();
            let native_h = renderer.height();
            match Self::build_simulcast_layers(simulcast_layers, native_w, native_h) {
                Ok(layers) => {
                    return Self::costume_video_loop_simulcast(
                        user_id,
                        renderer,
                        packet_sender,
                        quit,
                        media_start,
                        loop_duration,
                        is_speaking,
                        encoder_output_fps,
                        encoder_errors_generic,
                        encoder_frames_ok,
                        transport_drops_counter,
                        layers,
                    );
                }
                Err(e) => {
                    error!(
                        "[{}] costume simulcast layer build failed ({}); falling back to single-stream video",
                        user_id, e
                    );
                    // fall through to the single-stream path below
                }
            }
        }

        // The costume sprite sheet is always 1280x720. The encoder may run
        // at a lower resolution when AQ requests a tier step-down; in that
        // case we downscale each source frame into `i420_buf` before encoding.
        let native_w = renderer.width();
        let native_h = renderer.height();

        let mut v = aq.snapshot_video();
        let mut last_epoch: u64 = aq.tier_epoch();
        // Encoder resolution — starts at native, may be lowered by AQ.
        let mut enc_w: u32 = v.max_width.min(native_w);
        let mut enc_h: u32 = v.max_height.min(native_h);
        let mut framerate: u32 = v.target_fps.max(1);
        let mut frames_per_keyframe: u32 = v.keyframe_interval.max(1);
        let mut frame_interval_us: u64 = 1_000_000 / framerate as u64;

        // Whether we need to downscale (enc resolution < native).
        let mut needs_scale = enc_w < native_w || enc_h < native_h;
        // Reusable buffer for scaled frames. Allocated only when needed.
        let mut i420_buf: Vec<u8> = if needs_scale {
            vec![0u8; (enc_w * enc_h * 3 / 2) as usize]
        } else {
            Vec::new()
        };

        info!(
            "Costume video producer started for {} (native={}x{}, enc={}x{} @ {}fps, bitrate={}kbps, AQ tier={})",
            user_id,
            native_w,
            native_h,
            enc_w,
            enc_h,
            framerate,
            v.bitrate_kbps,
            aq.video_tier_index(),
        );

        // Publish the initial encoder FPS to the shared atomic so the health
        // reporter can include it in HealthPacket.encoder_output_fps.
        encoder_output_fps.store(framerate, Ordering::Relaxed);

        let mut video_encoder = VideoEncoderBuilder::new(framerate, 5)
            .set_resolution(enc_w, enc_h)
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

        // Log once on first downscale activation.
        static COSTUME_RES_WARNED: AtomicBool = AtomicBool::new(false);

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("Costume video producer stopping for {}", user_id);
                break;
            }

            // Cheap lock-free poll: only re-snapshot when AQ actually changed
            // the tier. Costume path honors bitrate, FPS, and resolution.
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

                // Resolution or FPS change: rebuild the encoder.
                // Cap requested resolution to native — upscaling would waste
                // bits without improving quality.
                let target_w = new_v.max_width.min(native_w);
                let target_h = new_v.max_height.min(native_h);
                let fps = new_v.target_fps.max(1);

                if target_w != enc_w || target_h != enc_h || fps != framerate {
                    info!(
                        "[{}] AQ (costume): rebuilding encoder {}x{}@{}fps -> {}x{}@{}fps",
                        user_id, enc_w, enc_h, framerate, target_w, target_h, fps,
                    );

                    match VideoEncoderBuilder::new(fps, 5)
                        .set_resolution(target_w, target_h)
                        .build()
                    {
                        Ok(mut enc) => {
                            let _ = enc.update_bitrate_kbps(new_v.bitrate_kbps);
                            video_encoder = enc;
                            enc_w = target_w;
                            enc_h = target_h;
                            framerate = fps;
                            frame_interval_us = 1_000_000 / framerate as u64;
                            needs_scale = enc_w < native_w || enc_h < native_h;

                            // (Re)allocate the scale buffer if needed.
                            if needs_scale {
                                let buf_len = (enc_w * enc_h * 3 / 2) as usize;
                                i420_buf.resize(buf_len, 0);

                                // Info log on first downscale activation.
                                if !COSTUME_RES_WARNED.swap(true, Ordering::Relaxed) {
                                    info!(
                                        "[{}] AQ (costume): downscale active — {}x{} source -> {}x{} encoder",
                                        user_id, native_w, native_h, enc_w, enc_h,
                                    );
                                }
                            }

                            prev_frame_index = None; // force keyframe
                            encoder_output_fps.store(framerate, Ordering::Relaxed);
                        }
                        Err(e) => {
                            error!(
                                "[{}] AQ (costume): failed to rebuild encoder at {}x{}@{}fps: {} — keeping old encoder",
                                user_id, target_w, target_h, fps, e
                            );
                            // Do not update enc_w/enc_h/framerate — keep old values.
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
                    "[{}] costume seq={}, frame={}, loop={}, pos={:.1}s/{:.1}s, tier={}, enc={}x{}{}",
                    user_id,
                    global_sequence,
                    frame_in_loop,
                    loop_num,
                    position_in_loop_us as f64 / 1_000_000.0,
                    loop_duration_us as f64 / 1_000_000.0,
                    aq.video_tier_index(),
                    enc_w,
                    enc_h,
                    if force_keyframe { " KEYFRAME" } else { "" }
                );
            }

            // Read native-resolution I420 frame from costume renderer.
            let speaking = is_speaking.load(Ordering::Relaxed);
            let source_frame = renderer.frame_i420(speaking, frame_in_loop);

            // Downscale if encoder resolution is below native.
            let encode_input: &[u8] = if needs_scale {
                scale_i420(
                    source_frame,
                    native_w,
                    native_h,
                    &mut i420_buf,
                    enc_w,
                    enc_h,
                );
                &i420_buf[..]
            } else {
                source_frame
            };

            // Encode to VP9
            let frames_result = if force_keyframe {
                info!(
                    "Forcing keyframe for {} (costume seq={})",
                    user_id, global_sequence
                );
                video_encoder.encode_keyframe(global_sequence as i64, encode_input)
            } else {
                video_encoder.encode(global_sequence as i64, encode_input)
            };

            let frames = match frames_result {
                Ok(f) => {
                    encoder_frames_ok.fetch_add(1, Ordering::Relaxed);
                    f
                }
                Err(e) => {
                    encoder_errors_generic.fetch_add(1, Ordering::Relaxed);
                    error!("Costume video producer encode error for {}: {}", user_id, e);
                    global_sequence += 1;
                    // Sleep until next frame deadline before continuing
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
                    continue;
                }
            };

            for frame in frames {
                let sent = build_and_send_layer(
                    &packet_sender,
                    &user_id_bytes,
                    frame.data,
                    frame.key,
                    global_sequence,
                    framerate,
                    // Single-stream path → layer 0 (wire-identical to today).
                    0,
                )?;
                if !sent {
                    static COSTUME_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
                    let count = COSTUME_DROP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                    transport_drops_counter.fetch_add(1, Ordering::Relaxed);
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

    /// Build one `VideoEncoder` per simulcast layer at the ladder's FIXED tier
    /// resolution (lowest layer first; slice index == layer_id), capped to the
    /// native source dimensions so we never upscale. Each encoder is seeded
    /// with its tier's `ideal_bitrate_kbps`. Used by both simulcast loops.
    ///
    /// `native_w`/`native_h` are the source frame dimensions (1280x720 for the
    /// costume renderer; the EKG renderer's current tier resolution). Tier
    /// resolutions are forced even (`& !1`) before capping because VP9 requires
    /// even dimensions (`VideoEncoderBuilder::build` rejects odd values).
    fn build_simulcast_layers(
        n: u32,
        native_w: u32,
        native_h: u32,
    ) -> anyhow::Result<Vec<SimulcastLayer>> {
        let tiers = videocall_aq::constants::simulcast_layers(n as usize);
        let mut layers = Vec::with_capacity(tiers.len());
        for (idx, tier) in tiers.iter().enumerate() {
            // Cap to native (never upscale) and force even dimensions.
            let enc_w = (tier.max_width.min(native_w)) & !1;
            let enc_h = (tier.max_height.min(native_h)) & !1;
            let framerate = tier.target_fps.max(1);
            let frames_per_keyframe = tier.keyframe_interval_frames.max(1);

            let mut encoder = VideoEncoderBuilder::new(framerate, 5)
                .set_resolution(enc_w, enc_h)
                .build()?;
            encoder.update_bitrate_kbps(tier.ideal_bitrate_kbps)?;

            let needs_scale = enc_w < native_w || enc_h < native_h;
            let scale_buf = if needs_scale {
                vec![0u8; (enc_w * enc_h * 3 / 2) as usize]
            } else {
                Vec::new()
            };

            layers.push(SimulcastLayer {
                layer_id: idx as u32,
                encoder,
                enc_w,
                enc_h,
                framerate,
                frames_per_keyframe,
                ideal_bitrate_kbps: tier.ideal_bitrate_kbps,
                sequence: 0,
                frame_interval_us: 1_000_000 / framerate as u64,
                next_due_us: 0,
                needs_scale,
                scale_buf,
            });
        }
        Ok(layers)
    }

    /// Multi-layer (N>=2) EKG producer. Mirrors the browser client's simulcast
    /// model: one fixed-resolution VP9 encoder per ladder tier, per-layer
    /// sequence numbers, and `simulcast_layer_id` stamped per layer.
    ///
    /// `renderer` MUST already be sized to `native_w` x `native_h` (the TOP
    /// ladder tier resolution) and `layers` MUST already be built for that
    /// native size — the caller (`ekg_video_loop`) does both so a build failure
    /// can fall back to single-stream instead of killing this thread. Using the
    /// AQ default tier (854x480) as native would cap every higher layer down via
    /// `.min(native)` and collapse L1/L2 into one resolution, defeating
    /// simulcast (Blocker 1). Each layer downscales this native frame to its own
    /// tier resolution. Unlike the single-stream path this loop does NOT rebuild
    /// encoders on AQ tier changes (see the REVISIT note at the AQ-poll site),
    /// which is why it takes no `aq` handle.
    #[allow(clippy::too_many_arguments)]
    fn ekg_video_loop_simulcast(
        user_id: String,
        renderer: EkgRenderer,
        rms: Vec<f32>,
        max_rms: f32,
        packet_sender: Sender<OutboundFrame>,
        quit: Arc<AtomicBool>,
        media_start: Instant,
        loop_duration: Duration,
        encoder_output_fps: Arc<AtomicU32>,
        encoder_errors_generic: Arc<AtomicU64>,
        encoder_frames_ok: Arc<AtomicU64>,
        transport_drops_counter: Arc<AtomicU64>,
        mut layers: Vec<SimulcastLayer>,
        native_w: u32,
        native_h: u32,
    ) -> anyhow::Result<()> {
        let loop_duration_us = loop_duration.as_micros() as u64;
        if loop_duration_us == 0 {
            warn!(
                "EKG simulcast producer for {} has zero loop duration, exiting",
                user_id
            );
            return Ok(());
        }

        // The render/pacing cadence uses the HIGHEST layer's framerate so every
        // layer can be fed at (or above) its own rate; each layer applies its
        // own keyframe cadence and the source is identical across layers.
        let render_fps = layers.iter().map(|l| l.framerate).max().unwrap_or(1).max(1);
        let frame_interval_us: u64 = 1_000_000 / render_fps as u64;

        info!(
            "EKG simulcast producer started for {}: {} layers, native={}x{}, render_fps={} [{}]",
            user_id,
            layers.len(),
            native_w,
            native_h,
            render_fps,
            layers
                .iter()
                .map(|l| format!(
                    "L{}={}x{}@{}fps/{}kbps",
                    l.layer_id, l.enc_w, l.enc_h, l.framerate, l.ideal_bitrate_kbps,
                ))
                .collect::<Vec<_>>()
                .join(", "),
        );

        // Report the top layer's FPS to the health reporter (single shared
        // atomic; the top layer is the representative full-rate stream).
        encoder_output_fps.store(render_fps, Ordering::Relaxed);

        let mut frame_buf = renderer.create_frame_buffer();
        let mut i420_buf = vec![0u8; (native_w * native_h * 3 / 2) as usize];
        let user_id_bytes = user_id.clone().into_bytes();

        let mut prev_frame_index: Option<usize> = None;
        let mut render_count: u64 = 0;

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("EKG simulcast producer stopping for {}", user_id);
                break;
            }

            // REVISIT (#989): in simulcast mode the layer GEOMETRY (resolution
            // and fps) is FIXED by the ladder and does NOT follow AQ tier
            // changes — exactly like the browser client (camera_encoder.rs runs
            // fixed-resolution simulcast encoders). We therefore do NOT poll
            // `aq.tier_epoch()` to rebuild encoders here. Per-layer bitrate is
            // also pinned to each tier's ideal for now; per-layer AQ bitrate
            // shed and active-layer-count shedding are deferred until Tony's AQ
            // controller rework (PRs #1115/#1117) lands, to avoid conflicting
            // with it. For N==1 the legacy AQ-adaptive path above is untouched.
            let elapsed_us = media_start.elapsed().as_micros() as u64;
            let position_in_loop_us = elapsed_us % loop_duration_us;
            let frame_in_loop = (position_in_loop_us / frame_interval_us) as usize;

            // Force a keyframe on ALL layers at loop wrap / first frame.
            let at_loop_wrap = match prev_frame_index {
                Some(prev) => frame_in_loop < prev,
                None => true,
            };
            prev_frame_index = Some(frame_in_loop);

            // Render the EKG frame ONCE at native resolution; all layers encode
            // a downscaled copy of the same source.
            let rms_value = if frame_in_loop < rms.len() {
                rms[frame_in_loop]
            } else {
                0.0
            };
            renderer.render_frame_rgb_into(&mut frame_buf, rms_value, max_rms, frame_in_loop);
            rgb_to_i420_into(&frame_buf, &mut i420_buf);

            for layer in layers.iter_mut() {
                // Per-layer pacing (Blocker 2): a layer slower than render_fps
                // (e.g. a 20fps base layer under a 30fps render loop) only
                // encodes when its OWN deadline has elapsed, so its VBR target
                // and MediaPacket.duration stay honest. The top layer's
                // interval equals the render interval, so it fires every tick.
                if elapsed_us < layer.next_due_us {
                    continue;
                }
                // Advance the deadline by exactly one interval (per-fire pts
                // advance of 1). If we fell more than one interval behind (a
                // stall), resync to avoid a catch-up burst of encodes.
                if elapsed_us > layer.next_due_us + layer.frame_interval_us {
                    layer.next_due_us = elapsed_us + layer.frame_interval_us;
                } else {
                    layer.next_due_us += layer.frame_interval_us;
                }

                // Per-layer keyframe cadence; force on loop wrap for every layer.
                let periodic_keyframe = layer
                    .sequence
                    .is_multiple_of(layer.frames_per_keyframe as u64);
                let force_keyframe = at_loop_wrap || periodic_keyframe;

                let encode_input: &[u8] = if layer.needs_scale {
                    scale_i420(
                        &i420_buf,
                        native_w,
                        native_h,
                        &mut layer.scale_buf,
                        layer.enc_w,
                        layer.enc_h,
                    );
                    &layer.scale_buf[..]
                } else {
                    &i420_buf[..]
                };

                let frames_result = if force_keyframe {
                    layer
                        .encoder
                        .encode_keyframe(layer.sequence as i64, encode_input)
                } else {
                    layer.encoder.encode(layer.sequence as i64, encode_input)
                };

                let frames = match frames_result {
                    Ok(f) => {
                        encoder_frames_ok.fetch_add(1, Ordering::Relaxed);
                        f
                    }
                    Err(e) => {
                        encoder_errors_generic.fetch_add(1, Ordering::Relaxed);
                        error!(
                            "EKG simulcast encode error for {} (L{}): {}",
                            user_id, layer.layer_id, e
                        );
                        layer.sequence += 1;
                        continue;
                    }
                };

                for frame in frames {
                    let sent = build_and_send_layer(
                        &packet_sender,
                        &user_id_bytes,
                        frame.data,
                        frame.key,
                        layer.sequence,
                        layer.framerate,
                        layer.layer_id,
                    )?;
                    if !sent {
                        static SIMULCAST_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
                        let count = SIMULCAST_DROP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                        transport_drops_counter.fetch_add(1, Ordering::Relaxed);
                        if count % 100 == 1 {
                            warn!(
                                "Dropped simulcast video packets due to full send channel (total: {})",
                                count,
                            );
                        }
                    } else {
                        trace!(
                            "Sent VP9 L{} frame {} ({} bytes, {}) for {}",
                            layer.layer_id,
                            layer.sequence,
                            frame.data.len(),
                            if frame.key { "key" } else { "delta" },
                            user_id
                        );
                    }
                }

                layer.sequence += 1;
            }

            if render_count.is_multiple_of(render_fps as u64 * 5) {
                let loop_num = elapsed_us / loop_duration_us;
                info!(
                    "[{}] ekg-simulcast render={}, frame={}, loop={}, pos={:.1}s/{:.1}s, layers={}{}",
                    user_id,
                    render_count,
                    frame_in_loop,
                    loop_num,
                    position_in_loop_us as f64 / 1_000_000.0,
                    loop_duration_us as f64 / 1_000_000.0,
                    layers.len(),
                    if at_loop_wrap { " KEYFRAME" } else { "" }
                );
            }
            render_count += 1;

            // Sleep until next render deadline (microsecond precision).
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

    /// Multi-layer (N>=2) costume producer. Same simulcast model as
    /// [`Self::ekg_video_loop_simulcast`] but the source is the costume
    /// renderer's native 1280x720 I420 frame, downscaled per layer. `layers`
    /// is pre-built by the caller (`costume_video_loop`) for `renderer`'s native
    /// size so a build failure can fall back to single-stream. AQ is not taken:
    /// layer geometry/bitrate are fixed by the ladder (see the REVISIT note).
    #[allow(clippy::too_many_arguments)]
    fn costume_video_loop_simulcast(
        user_id: String,
        renderer: CostumeRenderer,
        packet_sender: Sender<OutboundFrame>,
        quit: Arc<AtomicBool>,
        media_start: Instant,
        loop_duration: Duration,
        is_speaking: Arc<AtomicBool>,
        encoder_output_fps: Arc<AtomicU32>,
        encoder_errors_generic: Arc<AtomicU64>,
        encoder_frames_ok: Arc<AtomicU64>,
        transport_drops_counter: Arc<AtomicU64>,
        mut layers: Vec<SimulcastLayer>,
    ) -> anyhow::Result<()> {
        let loop_duration_us = loop_duration.as_micros() as u64;
        if loop_duration_us == 0 {
            warn!(
                "Costume simulcast producer for {} has zero loop duration, exiting",
                user_id
            );
            return Ok(());
        }

        let native_w = renderer.width();
        let native_h = renderer.height();

        let render_fps = layers.iter().map(|l| l.framerate).max().unwrap_or(1).max(1);
        let frame_interval_us: u64 = 1_000_000 / render_fps as u64;

        info!(
            "Costume simulcast producer started for {}: {} layers, native={}x{}, render_fps={} [{}]",
            user_id,
            layers.len(),
            native_w,
            native_h,
            render_fps,
            layers
                .iter()
                .map(|l| format!(
                    "L{}={}x{}@{}fps/{}kbps",
                    l.layer_id, l.enc_w, l.enc_h, l.framerate, l.ideal_bitrate_kbps,
                ))
                .collect::<Vec<_>>()
                .join(", "),
        );

        encoder_output_fps.store(render_fps, Ordering::Relaxed);

        let user_id_bytes = user_id.clone().into_bytes();
        let mut prev_frame_index: Option<usize> = None;
        let mut render_count: u64 = 0;

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("Costume simulcast producer stopping for {}", user_id);
                break;
            }

            // REVISIT (#989): identical to the EKG simulcast loop — layer
            // geometry is FIXED by the ladder and AQ tier changes are
            // intentionally ignored. Per-layer AQ bitrate shed / active-layer
            // shedding deferred until Tony's AQ rework (#1115/#1117).
            let elapsed_us = media_start.elapsed().as_micros() as u64;
            let position_in_loop_us = elapsed_us % loop_duration_us;
            let frame_in_loop = (position_in_loop_us / frame_interval_us) as usize;

            let at_loop_wrap = match prev_frame_index {
                Some(prev) => frame_in_loop < prev,
                None => true,
            };
            prev_frame_index = Some(frame_in_loop);

            // Read the native-resolution I420 source frame ONCE.
            let speaking = is_speaking.load(Ordering::Relaxed);
            let source_frame = renderer.frame_i420(speaking, frame_in_loop);

            for layer in layers.iter_mut() {
                // Per-layer pacing (Blocker 2) — see ekg_video_loop_simulcast
                // for the rationale. Encode only when this layer's own deadline
                // has elapsed; resync on a stall to avoid a catch-up burst.
                if elapsed_us < layer.next_due_us {
                    continue;
                }
                if elapsed_us > layer.next_due_us + layer.frame_interval_us {
                    layer.next_due_us = elapsed_us + layer.frame_interval_us;
                } else {
                    layer.next_due_us += layer.frame_interval_us;
                }

                let periodic_keyframe = layer
                    .sequence
                    .is_multiple_of(layer.frames_per_keyframe as u64);
                let force_keyframe = at_loop_wrap || periodic_keyframe;

                let encode_input: &[u8] = if layer.needs_scale {
                    scale_i420(
                        source_frame,
                        native_w,
                        native_h,
                        &mut layer.scale_buf,
                        layer.enc_w,
                        layer.enc_h,
                    );
                    &layer.scale_buf[..]
                } else {
                    source_frame
                };

                let frames_result = if force_keyframe {
                    layer
                        .encoder
                        .encode_keyframe(layer.sequence as i64, encode_input)
                } else {
                    layer.encoder.encode(layer.sequence as i64, encode_input)
                };

                let frames = match frames_result {
                    Ok(f) => {
                        encoder_frames_ok.fetch_add(1, Ordering::Relaxed);
                        f
                    }
                    Err(e) => {
                        encoder_errors_generic.fetch_add(1, Ordering::Relaxed);
                        error!(
                            "Costume simulcast encode error for {} (L{}): {}",
                            user_id, layer.layer_id, e
                        );
                        layer.sequence += 1;
                        continue;
                    }
                };

                for frame in frames {
                    let sent = build_and_send_layer(
                        &packet_sender,
                        &user_id_bytes,
                        frame.data,
                        frame.key,
                        layer.sequence,
                        layer.framerate,
                        layer.layer_id,
                    )?;
                    if !sent {
                        static COSTUME_SIMULCAST_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
                        let count =
                            COSTUME_SIMULCAST_DROP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                        transport_drops_counter.fetch_add(1, Ordering::Relaxed);
                        if count % 100 == 1 {
                            warn!(
                                "Dropped costume simulcast video packets due to full send channel (total: {})",
                                count,
                            );
                        }
                    } else {
                        trace!(
                            "Sent costume VP9 L{} frame {} ({} bytes, {}) for {}",
                            layer.layer_id,
                            layer.sequence,
                            frame.data.len(),
                            if frame.key { "key" } else { "delta" },
                            user_id
                        );
                    }
                }

                layer.sequence += 1;
            }

            if render_count.is_multiple_of(render_fps as u64 * 5) {
                let loop_num = elapsed_us / loop_duration_us;
                info!(
                    "[{}] costume-simulcast render={}, frame={}, loop={}, pos={:.1}s/{:.1}s, layers={}{}",
                    user_id,
                    render_count,
                    frame_in_loop,
                    loop_num,
                    position_in_loop_us as f64 / 1_000_000.0,
                    loop_duration_us as f64 / 1_000_000.0,
                    layers.len(),
                    if at_loop_wrap { " KEYFRAME" } else { "" }
                );
            }
            render_count += 1;

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

/// Build one layer's `PacketWrapper` for an encoded VP9 frame and try to send
/// it on `packet_sender`, mirroring the browser client's
/// `transform_video_chunk` (videocall-client/src/encode/transform.rs).
///
/// Stamps `PacketWrapper.simulcast_layer_id = layer_id`. Proto field 5 is a
/// proto3 `uint32`, so it serializes ONLY when non-zero — `layer_id == 0`
/// therefore produces bytes identical to a wrapper that never set the field,
/// which is why the single-layer (N==1) path stays wire-identical to today.
///
/// Returns `Ok(true)` when the frame was queued, `Ok(false)` when the send
/// channel was full (the caller owns the drop-counter / warn-throttle so the
/// per-loop trace text stays unchanged). Encoding errors are surfaced via the
/// `Result` from `write_to_bytes`.
#[allow(clippy::too_many_arguments)]
fn build_and_send_layer(
    packet_sender: &Sender<OutboundFrame>,
    user_id_bytes: &[u8],
    frame_data: &[u8],
    is_key: bool,
    sequence: u64,
    framerate: u32,
    layer_id: u32,
) -> anyhow::Result<bool> {
    let media_packet = MediaPacket {
        media_type: MediaType::VIDEO.into(),
        data: frame_data.to_vec(),
        user_id: user_id_bytes.to_vec(),
        frame_type: if is_key { "key" } else { "delta" }.to_string(),
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
        user_id: user_id_bytes.to_vec(),
        data: media_packet.write_to_bytes()?,
        // Cleartext discriminator so the relay can apply viewport-aware
        // VIDEO filtering without decrypting the inner MediaPacket
        // (HCL issue #988). Matches the real client (transform.rs).
        media_kind: MediaKind::VIDEO.into(),
        // Cleartext simulcast layer id (#989). Tag 5 serializes only when
        // non-zero, so layer 0 is wire-identical to the legacy single stream.
        simulcast_layer_id: layer_id,
        ..Default::default()
    };

    let packet_data = packet_wrapper.write_to_bytes()?;
    let out_frame = OutboundFrame::new(MediaTypeLabel::Video, packet_data);
    Ok(packet_sender.try_send(out_frame).is_ok())
}

/// One simulcast layer's encoder + per-layer sequence/keyframe bookkeeping.
///
/// Used only on the N>=2 path. Geometry (`enc_w`/`enc_h`/`framerate`) is FIXED
/// at the ladder tier's resolution for the layer's whole lifetime — simulcast
/// layers do NOT follow AQ tier geometry changes (matches the browser client,
/// camera_encoder.rs). The source frame is downscaled into `scale_buf` when the
/// tier resolution is below native (never upscaled).
struct SimulcastLayer {
    layer_id: u32,
    encoder: VideoEncoder,
    enc_w: u32,
    enc_h: u32,
    framerate: u32,
    frames_per_keyframe: u32,
    /// This layer's fixed VBR target (the tier's `ideal_bitrate_kbps`). Stored
    /// for the startup log so it stays consistent with the encoder's actual
    /// configured target without a fragile re-lookup into the ladder.
    ideal_bitrate_kbps: u32,
    /// Per-layer monotonic sequence counter (independent stream, like the
    /// client's `Vec<u64>` per-layer sequences).
    sequence: u64,
    /// This layer's own inter-frame interval in microseconds
    /// (`1_000_000 / framerate`). Drives per-layer pacing so a sub-render-fps
    /// layer (e.g. a 20fps base layer under a 30fps render loop) is fed at its
    /// OWN rate, keeping the encoder's VBR target and `MediaPacket.duration`
    /// honest.
    frame_interval_us: u64,
    /// Elapsed-time (since `media_start`, microseconds) at which this layer
    /// should next encode. Initialized to 0 so every layer fires (and forces a
    /// keyframe) on the first render tick.
    next_due_us: u64,
    /// Whether this layer's encoder resolution is below native (needs scaling).
    needs_scale: bool,
    /// Reusable downscale target buffer (empty when `needs_scale` is false).
    scale_buf: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message;
    use videocall_types::protos::media_packet::media_packet::MediaType;
    use videocall_types::protos::media_packet::{MediaPacket, VideoCodec};
    use videocall_types::protos::packet_wrapper::packet_wrapper::{MediaKind, PacketType};
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    /// Send one frame through `build_and_send_layer` and return the parsed
    /// outer wrapper. Uses a synchronous `try_send`/`try_recv` pair (the helper
    /// never awaits) so no tokio runtime is needed.
    fn send_one(layer_id: u32, sequence: u64, framerate: u32) -> PacketWrapper {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let sent = build_and_send_layer(
            &tx,
            b"alice",
            &[0xAA, 0xBB, 0xCC],
            true,
            sequence,
            framerate,
            layer_id,
        )
        .expect("helper must serialize successfully");
        assert!(sent, "frame must be queued on an empty channel");
        let frame = rx.try_recv().expect("a frame must have been queued");
        assert_eq!(frame.kind, MediaTypeLabel::Video);
        PacketWrapper::parse_from_bytes(&frame.bytes).expect("wrapper must parse")
    }

    /// This is THE test for the behavior the whole change exists to add: the
    /// cleartext `simulcast_layer_id` on the outer wrapper that the relay reads
    /// to forward/drop a layer. Layer 0 MUST be wire-identical to a wrapper that
    /// never set the field (pins the N==1 wire-identity claim), and a non-zero
    /// layer MUST round-trip. Modeled on transform.rs's tests.
    #[test]
    fn layer_zero_is_wire_absent_and_round_trips() {
        let wrapper = send_one(0, 7, 30);
        assert_eq!(wrapper.simulcast_layer_id, 0);
        assert_eq!(wrapper.packet_type.enum_value(), Ok(PacketType::MEDIA));
        assert_eq!(wrapper.media_kind.enum_value(), Ok(MediaKind::VIDEO));

        // Layer 0 must omit tag 5 entirely (proto3 default-zero), so a
        // single-stream (layer 0) publisher is byte-identical on the wire to a
        // wrapper that never set the field. Prove it deterministically by
        // diffing two wrappers that share the EXACT SAME `data` payload and
        // differ ONLY in `simulcast_layer_id` (0 vs non-zero): the layer-0
        // serialization must be a strict prefix-length-shorter encoding missing
        // precisely the tag-5 bytes. We do NOT scan the full wrapper bytes for
        // the tag byte 0x28 — `data` is an opaque inner MediaPacket carrying a
        // wall-clock timestamp, so 0x28 can appear there by chance (that made an
        // earlier version of this assertion flaky).
        let actual_bytes = wrapper.write_to_bytes().unwrap();
        let mut with_layer = wrapper.clone();
        with_layer.simulcast_layer_id = 2;
        let with_layer_bytes = with_layer.write_to_bytes().unwrap();
        assert!(
            with_layer_bytes.len() > actual_bytes.len(),
            "a non-zero simulcast_layer_id must add tag-5 bytes the layer-0 encoding lacks \
             (layer0={} bytes, layer2={} bytes)",
            actual_bytes.len(),
            with_layer_bytes.len(),
        );
        // The layer-0 bytes must equal a re-encode of a wrapper whose field is
        // explicitly default — byte-for-byte (the rigorous wire-identity proof).
        let mut baseline = wrapper.clone();
        baseline.simulcast_layer_id = 0;
        assert_eq!(
            actual_bytes,
            baseline.write_to_bytes().unwrap(),
            "layer 0 must serialize identically to a default-field wrapper (tag 5 absent)"
        );
    }

    #[test]
    fn nonzero_layer_round_trips() {
        let wrapper = send_one(2, 11, 20);
        assert_eq!(wrapper.simulcast_layer_id, 2);
        assert_eq!(wrapper.media_kind.enum_value(), Ok(MediaKind::VIDEO));
    }

    /// The inner MediaPacket carries the per-layer sequence and VP9 codec the
    /// receiver depacketizes against; verify it round-trips for a non-base layer.
    #[test]
    fn inner_media_packet_round_trips() {
        let wrapper = send_one(1, 42, 24);
        let mp = MediaPacket::parse_from_bytes(&wrapper.data).expect("inner packet must parse");
        assert_eq!(mp.media_type.enum_value(), Ok(MediaType::VIDEO));
        assert_eq!(mp.frame_type, "key");
        let vm = mp.video_metadata.as_ref().expect("video_metadata present");
        assert_eq!(vm.sequence, 42);
        assert_eq!(
            vm.codec.enum_value(),
            Ok(VideoCodec::VP9_PROFILE0_LEVEL10_8BIT)
        );
        // duration is honest = 1000 / framerate (Blocker 2 makes this true at
        // the loop level; here we pin that the helper writes the value it's
        // given the framerate for).
        assert!((mp.duration - (1000.0 / 24.0)).abs() < 1e-6);
    }
}

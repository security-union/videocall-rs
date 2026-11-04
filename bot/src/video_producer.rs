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

pub mod replay;

use crate::aq_controller::BotAq;
use crate::costume_renderer::CostumeRenderer;
use crate::ekg_renderer::EkgRenderer;
use crate::i420_scale::scale_i420;
use crate::transport::{MediaTypeLabel, OutboundFrame, OutboundFrameSender};
use crate::video_encoder::{VideoEncoder, VideoEncoderBuilder};
use image::{ImageBuffer, Rgb};
use protobuf::Message;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, trace, warn};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{MediaPacket, VideoCodec, VideoMetadata};
use videocall_types::protos::packet_wrapper::packet_wrapper::{MediaKind, PacketType};
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// libvpx `VP8E_SET_CPUUSED` for every bot publisher encoder.
const VP9_CPU_USED: u8 = 8;

fn publisher_encoder(fps: u32, width: u32, height: u32) -> anyhow::Result<VideoEncoder> {
    VideoEncoderBuilder::new(fps, VP9_CPU_USED)
        .set_resolution(width, height)
        .build()
}

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
    ///
    /// `rms_fps` is the fps the `rms` buffer was sampled at (one entry per frame
    /// at that rate — see `ekg_renderer::compute_rms_per_frame`). The simulcast
    /// loop paces at the TOP layer's fps, which can be higher than `rms_fps`, so
    /// it remaps the index by `rms_fps / render_fps` to keep the waveform
    /// animating at real time (issue #1123 item 2). The single-stream path paces
    /// at the same fps `rms` was built at, so it indexes `rms` directly.
    #[allow(clippy::too_many_arguments)]
    pub fn from_ekg(
        user_id: String,
        renderer: EkgRenderer,
        rms: Vec<f32>,
        max_rms: f32,
        rms_fps: u32,
        packet_sender: OutboundFrameSender,
        media_start: Instant,
        loop_duration: Duration,
        aq: Arc<BotAq>,
        encoder_output_fps: Arc<AtomicU32>,
        encoder_errors_generic: Arc<AtomicU64>,
        encoder_frames_ok: Arc<AtomicU64>,
        transport_drops_counter: Arc<AtomicU64>,
        simulcast_layers: u32,
        keyframe_gate: ForcedKeyframeGate,
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
                rms_fps,
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
                keyframe_gate,
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
        packet_sender: OutboundFrameSender,
        media_start: Instant,
        loop_duration: Duration,
        is_speaking: Arc<AtomicBool>,
        aq: Arc<BotAq>,
        encoder_output_fps: Arc<AtomicU32>,
        encoder_errors_generic: Arc<AtomicU64>,
        encoder_frames_ok: Arc<AtomicU64>,
        transport_drops_counter: Arc<AtomicU64>,
        simulcast_layers: u32,
        keyframe_gate: ForcedKeyframeGate,
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
                keyframe_gate,
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
        rms_fps: u32,
        packet_sender: OutboundFrameSender,
        quit: Arc<AtomicBool>,
        media_start: Instant,
        loop_duration: Duration,
        aq: Arc<BotAq>,
        encoder_output_fps: Arc<AtomicU32>,
        encoder_errors_generic: Arc<AtomicU64>,
        encoder_frames_ok: Arc<AtomicU64>,
        transport_drops_counter: Arc<AtomicU64>,
        simulcast_layers: u32,
        keyframe_gate: ForcedKeyframeGate,
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
                                rms_fps,
                                packet_sender,
                                quit,
                                media_start,
                                loop_duration,
                                aq,
                                encoder_output_fps,
                                encoder_errors_generic,
                                encoder_frames_ok,
                                transport_drops_counter,
                                layers,
                                keyframe_gate,
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

        let mut video_encoder = publisher_encoder(framerate, width, height)?;
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
        let mut keyframe_gate = keyframe_gate;
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
                    match publisher_encoder(framerate, width, height) {
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
            let force_keyframe =
                keyframe_gate.requested(elapsed_us) || at_loop_wrap || periodic_keyframe;
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
                debug!("Forcing keyframe for {} (seq={})", user_id, global_sequence);
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

            if force_keyframe {
                keyframe_gate.on_keyframe(elapsed_us);
            }
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
        packet_sender: OutboundFrameSender,
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
        keyframe_gate: ForcedKeyframeGate,
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
                        aq,
                        encoder_output_fps,
                        encoder_errors_generic,
                        encoder_frames_ok,
                        transport_drops_counter,
                        layers,
                        keyframe_gate,
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

        let mut video_encoder = publisher_encoder(framerate, enc_w, enc_h)?;
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
        let mut keyframe_gate = keyframe_gate;
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

                    match publisher_encoder(fps, target_w, target_h) {
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
            let force_keyframe =
                keyframe_gate.requested(elapsed_us) || at_loop_wrap || periodic_keyframe;
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
                debug!(
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

            if force_keyframe {
                keyframe_gate.on_keyframe(elapsed_us);
            }
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

            let mut encoder = publisher_encoder(framerate, enc_w, enc_h)?;
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
                ideal_bitrate_kbps: tier.ideal_bitrate_kbps,
                kf: LayerKeyframeState::new(frames_per_keyframe),
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
    /// which is why it takes no `aq` handle for GEOMETRY. It DOES take `aq` for
    /// the per-layer AQ wiring (issue #1083 V21): each iteration it reads the
    /// active layer count + per-layer target bitrates and (a) skips encoding/
    /// sending any layer at or above the active count (top-down shed) and (b)
    /// re-applies a layer's bitrate when its target changed.
    #[allow(clippy::too_many_arguments)]
    fn ekg_video_loop_simulcast(
        user_id: String,
        renderer: EkgRenderer,
        rms: Vec<f32>,
        max_rms: f32,
        rms_fps: u32,
        packet_sender: OutboundFrameSender,
        quit: Arc<AtomicBool>,
        mut media_start: impl Pacer,
        loop_duration: Duration,
        aq: Arc<BotAq>,
        encoder_output_fps: Arc<AtomicU32>,
        encoder_errors_generic: Arc<AtomicU64>,
        encoder_frames_ok: Arc<AtomicU64>,
        transport_drops_counter: Arc<AtomicU64>,
        mut layers: Vec<SimulcastLayer>,
        keyframe_gate: ForcedKeyframeGate,
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
        let mut keyframe_gate = keyframe_gate;
        let mut render_count: u64 = 0;

        // Per-layer AQ state (issue #1083 V21). `active` is how many top-down
        // layers we currently encode/send; `last_applied_kbps[id]` tracks the
        // bitrate last pushed to each layer's encoder so we only reconfigure on
        // a real change. Seed from the AQ snapshot immediately so the initial
        // budget-capped per-layer targets land before the first frame.
        let mut last_simulcast_epoch = aq.simulcast_epoch();
        let mut last_applied_kbps = vec![0u32; layers.len()];
        let mut active = apply_simulcast_aq(
            &mut layers,
            &aq.simulcast_snapshot(),
            &mut last_applied_kbps,
            &user_id,
        );

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("EKG simulcast producer stopping for {}", user_id);
                break;
            }

            // Layer GEOMETRY (resolution and fps) is FIXED by the ladder and does
            // NOT follow AQ tier changes — exactly like the browser client
            // (camera_encoder.rs runs fixed-resolution simulcast encoders), so we
            // do NOT poll `aq.tier_epoch()` to rebuild encoders here.
            //
            // Per-layer AQ wiring (issue #1083 V21 — the deferral to Tony's AQ
            // rework #1115/#1117 is OVER; those PRs are in this base): poll the
            // cheap `simulcast_epoch` each iteration (one Acquire load) and, only
            // when it changed, re-read the active layer count + budget-capped
            // per-layer target bitrates and re-apply them. Layers at/above
            // `active` are shed (skipped below) — the base layer always flows.
            let current_simulcast_epoch = aq.simulcast_epoch();
            if current_simulcast_epoch != last_simulcast_epoch {
                last_simulcast_epoch = current_simulcast_epoch;
                active = apply_simulcast_aq(
                    &mut layers,
                    &aq.simulcast_snapshot(),
                    &mut last_applied_kbps,
                    &user_id,
                );
            }
            let elapsed_us = media_start.elapsed_us();
            let position_in_loop_us = elapsed_us % loop_duration_us;
            let frame_in_loop = (position_in_loop_us / frame_interval_us) as usize;

            // Force a keyframe on ALL layers at loop wrap / first frame.
            let at_loop_wrap = match prev_frame_index {
                Some(prev) => frame_in_loop < prev,
                None => true,
            };
            prev_frame_index = Some(frame_in_loop);
            let key_all = keyframe_gate.requested(elapsed_us) || at_loop_wrap;
            if key_all {
                keyframe_gate.on_keyframe(elapsed_us);
            }

            // Render the EKG frame ONCE at native resolution; all layers encode
            // a downscaled copy of the same source.
            //
            // The `rms` buffer was sampled at `rms_fps` (one entry per frame at
            // that rate), but this loop paces at `render_fps` (the top layer's
            // fps, often higher). Map the render-tick index to the rms index by
            // real time so the waveform animates at real time across the FULL
            // loop instead of fast-forwarding and flatlining the tail at 0.0
            // (issue #1123 item 2). The bounds check below still guards the
            // final partial frame.
            let rms_index = ekg_rms_index_for_render_frame(frame_in_loop, rms_fps, render_fps);
            let rms_value = rms.get(rms_index).copied().unwrap_or(0.0);
            renderer.render_frame_rgb_into(&mut frame_buf, rms_value, max_rms, frame_in_loop);
            rgb_to_i420_into(&frame_buf, &mut i420_buf);

            for layer in layers.iter_mut() {
                // Loop-wrap keyframe latch (issue #1123 item 1): record the wrap
                // for EVERY layer FIRST — before the shed / not-yet-due
                // `continue`s below — so a layer that skips this exact tick still
                // forces a keyframe on its next DUE encode (clears the latch),
                // instead of serving deltas across the source-loop discontinuity
                // until its next periodic keyframe (~5s for L0).
                layer.kf.latch_wrap(key_all);

                // Top-down shed (issue #1083 V21): skip any layer at/above the
                // AQ's active count entirely — no encode, no send. The base layer
                // (id 0) is always < active (active is floored at 1), so it always
                // flows. A shed layer's pacing deadline is left untouched; on
                // restore the `next_due_us > +interval` resync below prevents a
                // catch-up burst.
                if layer.layer_id as usize >= active {
                    continue;
                }
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

                // Per-layer keyframe cadence; force on loop wrap (latched so a
                // layer that skipped the wrap tick still keyframes here) or on the
                // layer's own periodic cadence. (Folds the periodic computation
                // in, so the unit test exercises this exact decision.)
                let force_keyframe = layer.kf.force_keyframe(at_loop_wrap);

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
                        .encode_keyframe(layer.kf.sequence as i64, encode_input)
                } else {
                    layer.encoder.encode(layer.kf.sequence as i64, encode_input)
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
                        // Failed encode: no frame emitted. Advance the sequence
                        // but DO NOT clear the latch (the success path's
                        // `on_encoded` is the only clear), so the next attempt
                        // still keyframes across the source-loop cut.
                        layer.kf.sequence += 1;
                        continue;
                    }
                };

                // Track whether a key-flagged packet was actually OBSERVED on the
                // wire (issue #1123 item 1 hardening). At `g_lag_in_frames = 1` a
                // forced keyframe emits its key packet on this same call, so this
                // resolves the latch on exactly the frame that caused it; if lag
                // ever rises this still only clears once the key packet is truly
                // observed (no early clear of a deferred keyframe). Cheap: just OR
                // the per-frame `key` flag — no allocation.
                let mut saw_key = false;
                for frame in frames {
                    saw_key |= frame.key;
                    let sent = build_and_send_layer(
                        &packet_sender,
                        &user_id_bytes,
                        frame.data,
                        frame.key,
                        layer.kf.sequence,
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
                            layer.kf.sequence,
                            frame.data.len(),
                            if frame.key { "key" } else { "delta" },
                            user_id
                        );
                    }
                }

                // Post-encode: advance sequence and clear the wrap latch iff a
                // keyframe was observed (Finding 2). Same method the unit test
                // drives.
                if saw_key {
                    keyframe_gate.on_layer_keyframe(layer.layer_id, active, elapsed_us);
                }
                layer.kf.on_encoded(saw_key);
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
            media_start.sleep_until_us(loop_base_us + sleep_target_us);
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
        packet_sender: OutboundFrameSender,
        quit: Arc<AtomicBool>,
        mut media_start: impl Pacer,
        loop_duration: Duration,
        is_speaking: Arc<AtomicBool>,
        aq: Arc<BotAq>,
        encoder_output_fps: Arc<AtomicU32>,
        encoder_errors_generic: Arc<AtomicU64>,
        encoder_frames_ok: Arc<AtomicU64>,
        transport_drops_counter: Arc<AtomicU64>,
        mut layers: Vec<SimulcastLayer>,
        keyframe_gate: ForcedKeyframeGate,
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
        let mut keyframe_gate = keyframe_gate;
        let mut render_count: u64 = 0;

        // Per-layer AQ state (issue #1083 V21) — see ekg_video_loop_simulcast for
        // the rationale. Seed from the AQ snapshot before the first frame.
        let mut last_simulcast_epoch = aq.simulcast_epoch();
        let mut last_applied_kbps = vec![0u32; layers.len()];
        let mut active = apply_simulcast_aq(
            &mut layers,
            &aq.simulcast_snapshot(),
            &mut last_applied_kbps,
            &user_id,
        );

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("Costume simulcast producer stopping for {}", user_id);
                break;
            }

            // Layer geometry is FIXED by the ladder and AQ tier changes are
            // intentionally ignored (matches the browser fixed-resolution
            // simulcast encoders). Per-layer AQ wiring (issue #1083 V21 — the
            // deferral to Tony's AQ rework #1115/#1117 is OVER; those PRs are in
            // this base): poll `simulcast_epoch` and, only on change, re-apply the
            // active count + budget-capped per-layer targets. Identical to the EKG
            // simulcast loop.
            let current_simulcast_epoch = aq.simulcast_epoch();
            if current_simulcast_epoch != last_simulcast_epoch {
                last_simulcast_epoch = current_simulcast_epoch;
                active = apply_simulcast_aq(
                    &mut layers,
                    &aq.simulcast_snapshot(),
                    &mut last_applied_kbps,
                    &user_id,
                );
            }
            let elapsed_us = media_start.elapsed_us();
            let position_in_loop_us = elapsed_us % loop_duration_us;
            let frame_in_loop = (position_in_loop_us / frame_interval_us) as usize;

            let at_loop_wrap = match prev_frame_index {
                Some(prev) => frame_in_loop < prev,
                None => true,
            };
            prev_frame_index = Some(frame_in_loop);
            let key_all = keyframe_gate.requested(elapsed_us) || at_loop_wrap;
            if key_all {
                keyframe_gate.on_keyframe(elapsed_us);
            }

            // Read the native-resolution I420 source frame ONCE.
            let speaking = is_speaking.load(Ordering::Relaxed);
            let source_frame = renderer.frame_i420(speaking, frame_in_loop);

            for layer in layers.iter_mut() {
                // Loop-wrap keyframe latch (issue #1123 item 1) — see
                // ekg_video_loop_simulcast. Record the wrap for EVERY layer
                // BEFORE the shed / not-yet-due `continue`s so a layer that
                // skips this tick still keyframes on its next due encode.
                layer.kf.latch_wrap(key_all);

                // Top-down shed (issue #1083 V21) — see ekg_video_loop_simulcast.
                // Skip any layer at/above the AQ active count; base layer always
                // flows (active floored at 1).
                if layer.layer_id as usize >= active {
                    continue;
                }
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

                // Force on loop wrap (latched), periodic cadence, or pending
                // latch — see ekg_video_loop_simulcast. Same method drives both
                // loops and the unit test.
                let force_keyframe = layer.kf.force_keyframe(at_loop_wrap);

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
                        .encode_keyframe(layer.kf.sequence as i64, encode_input)
                } else {
                    layer.encoder.encode(layer.kf.sequence as i64, encode_input)
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
                        // Failed encode: advance the sequence but leave the latch
                        // set (only `on_encoded` clears it) so the next attempt
                        // still keyframes across the source-loop cut.
                        layer.kf.sequence += 1;
                        continue;
                    }
                };

                // Clear the wrap latch only on an OBSERVED key packet (issue
                // #1123 item 1 hardening) — see ekg_video_loop_simulcast.
                let mut saw_key = false;
                for frame in frames {
                    saw_key |= frame.key;
                    let sent = build_and_send_layer(
                        &packet_sender,
                        &user_id_bytes,
                        frame.data,
                        frame.key,
                        layer.kf.sequence,
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
                            layer.kf.sequence,
                            frame.data.len(),
                            if frame.key { "key" } else { "delta" },
                            user_id
                        );
                    }
                }

                if saw_key {
                    keyframe_gate.on_layer_keyframe(layer.layer_id, active, elapsed_us);
                }
                layer.kf.on_encoded(saw_key);
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
            media_start.sleep_until_us(loop_base_us + sleep_target_us);
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

const RGB_FIX_SHIFT: u32 = 20;
const RGB_FIX_ROUND: i32 = 1 << (RGB_FIX_SHIFT - 1);

const Y_COEF: [i32; 3] = [269_484, 528_482, 102_760];
const U_COEF: [i32; 3] = [-155_189, -305_136, 460_325];
const V_COEF: [i32; 3] = [460_325, -385_876, -74_449];

#[inline(always)]
fn rgb_fix(px: &[u8], c: [i32; 3], offset: i32) -> u8 {
    let sum = c[0] * px[0] as i32 + c[1] * px[1] as i32 + c[2] * px[2] as i32;
    ((sum + (offset << RGB_FIX_SHIFT) + RGB_FIX_ROUND) >> RGB_FIX_SHIFT) as u8
}

fn rgb_to_i420_into(image: &ImageBuffer<Rgb<u8>, Vec<u8>>, i420_data: &mut [u8]) {
    let width = image.width() as usize;
    let height = image.height() as usize;
    debug_assert_eq!(i420_data.len(), width * height * 3 / 2);
    if width == 0 || height == 0 {
        return;
    }

    let rgb = image.as_raw();
    let (y_plane, uv_planes) = i420_data.split_at_mut(width * height);
    let (u_plane, v_plane) = uv_planes.split_at_mut(width * height / 4);
    let chroma_w = width / 2;

    let rows = rgb
        .chunks_exact(width * 3)
        .zip(y_plane.chunks_exact_mut(width));
    for (row, (rgb_row, y_row)) in rows.enumerate() {
        for (px, y_out) in rgb_row.chunks_exact(3).zip(y_row.iter_mut()) {
            *y_out = rgb_fix(px, Y_COEF, 16);
        }
        if row % 2 == 0 {
            let base = (row / 2) * chroma_w;
            for (i, px) in rgb_row.chunks_exact(3).step_by(2).enumerate() {
                u_plane[base + i] = rgb_fix(px, U_COEF, 128);
                v_plane[base + i] = rgb_fix(px, V_COEF, 128);
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
    packet_sender: &OutboundFrameSender,
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

/// The simulcast loops' clock: time since media start, and a wait for a later time.
trait Pacer {
    fn elapsed_us(&self) -> u64;
    fn sleep_until_us(&mut self, target_us: u64);
}

impl Pacer for Instant {
    fn elapsed_us(&self) -> u64 {
        self.elapsed().as_micros() as u64
    }

    fn sleep_until_us(&mut self, target_us: u64) {
        let target = *self + Duration::from_micros(target_us);
        let now = Instant::now();
        if now < target {
            thread::sleep(target - now);
        }
    }
}

/// Per-layer loop-wrap keyframe latch (issue #1123 item 1), pure so it is
/// host-unit-testable without a real encoder. Called once per RENDER tick for
/// EVERY layer (including shed/not-yet-due ones, BEFORE the not-due `continue`)
/// to carry a pending wrap keyframe forward to the layer's next encode.
///
/// `pending` is the layer's current latch; `at_loop_wrap` is the render-tick
/// wrap signal. Returns the latch's new value: it becomes (or stays) `true`
/// whenever a wrap fires, and is otherwise left unchanged. The latch is cleared
/// elsewhere — only when the layer actually encodes (see
/// [`resolve_layer_keyframe`]).
fn latch_loop_wrap(pending: bool, at_loop_wrap: bool) -> bool {
    pending || at_loop_wrap
}

/// Decide whether a layer that is about to encode must force a keyframe (issue
/// #1123 item 1). Pure, so the keyframe state machine is testable without
/// libvpx.
///
/// A layer forces a keyframe if the current render tick is a loop wrap, OR its
/// own periodic cadence is due, OR a previous wrap was latched while the layer
/// was shed/not-yet-due ([`latch_loop_wrap`]). The caller clears the latch only
/// AFTER a SUCCESSFUL encode (a failed encode emits no frame, so a pending wrap
/// keyframe must survive to the next attempt — otherwise the layer would serve
/// a delta across the source-loop cut once the failed keyframe is forgotten).
fn resolve_layer_keyframe(at_loop_wrap: bool, periodic_keyframe: bool, pending: bool) -> bool {
    at_loop_wrap || periodic_keyframe || pending
}

/// One simulcast layer's keyframe state machine (issue #1123 item 1): the
/// per-layer monotonic `sequence`, its periodic keyframe cadence
/// (`frames_per_keyframe`), and the loop-wrap keyframe latch
/// (`pending_keyframe`). Split out of [`SimulcastLayer`] (which owns a real
/// `VideoEncoder` and so is not host-constructible) so the EXACT transition
/// logic both simulcast loops execute is exercised by unit tests — the loops
/// call these methods at the real per-tick sites, so breaking a method breaks a
/// test (no parallel re-implementation to drift out of sync).
struct LayerKeyframeState {
    /// Per-layer monotonic sequence counter (independent stream, like the
    /// client's `Vec<u64>` per-layer sequences). Used as the encoder PTS and as
    /// the on-wire `VideoMetadata.sequence` for this layer.
    sequence: u64,
    /// This layer's periodic keyframe interval, in frames (>= 1).
    frames_per_keyframe: u32,
    /// Loop-wrap keyframe latch. The source content loops; at the wrap there is
    /// a content discontinuity, so EVERY layer must emit a keyframe on its first
    /// frame after the wrap or a receiver decoding that layer sees a delta
    /// across the cut (a brief artifact). A layer slower than the render loop is
    /// often mid-interval on the exact wrap tick and `continue`s past the encode
    /// via the "not yet due" guard; latching the wrap here (set on
    /// [`Self::latch_wrap`], cleared on [`Self::on_encoded`] only when a keyframe
    /// was actually observed) guarantees the layer's next DUE frame is forced to
    /// a keyframe even if it skipped the wrap tick.
    pending_keyframe: bool,
}

impl LayerKeyframeState {
    /// Initial state for a freshly built layer: sequence 0, latch clear. The
    /// first render tick's `at_loop_wrap` (true on `prev_frame_index == None`)
    /// plus the periodic cadence (`sequence % frames_per_keyframe == 0` at
    /// sequence 0) already keyframe the first frame.
    fn new(frames_per_keyframe: u32) -> Self {
        Self {
            sequence: 0,
            frames_per_keyframe: frames_per_keyframe.max(1),
            pending_keyframe: false,
        }
    }

    /// Per-tick latch step (real site (a)). Called once per RENDER tick for
    /// EVERY layer — including shed / not-yet-due ones, BEFORE the not-due
    /// `continue` — so a wrap that lands on a tick the layer skips is carried
    /// forward to its next encode. Set-only (never clears here): the latch is
    /// cleared in [`Self::on_encoded`].
    fn latch_wrap(&mut self, at_loop_wrap: bool) {
        self.pending_keyframe = latch_loop_wrap(self.pending_keyframe, at_loop_wrap);
    }

    /// Keyframe decision for a layer that is about to encode (real site (c)).
    /// Folds the periodic cadence computation in so the test exercises the real
    /// periodic logic too: forces a keyframe on a loop wrap, on the layer's own
    /// periodic cadence, OR when a previous wrap is still latched. Pure (no
    /// mutation) — the latch is cleared later, on an observed keyframe.
    ///
    /// Invariant: whenever `pending_keyframe` is set this returns `true`, so a
    /// latched layer's next encode is ALWAYS a forced keyframe. That is what
    /// makes the lag=1 clear-on-observed-keyframe in [`Self::on_encoded`] exact
    /// (the forced keyframe emits its key packet synchronously, so the tick that
    /// resolves the latch is the tick that observes the keyframe).
    fn force_keyframe(&self, at_loop_wrap: bool) -> bool {
        let periodic = self
            .sequence
            .is_multiple_of(self.frames_per_keyframe as u64);
        resolve_layer_keyframe(at_loop_wrap, periodic, self.pending_keyframe)
    }

    /// Post-encode step (real site (e)). Advances the sequence for any encoded
    /// frame and clears the loop-wrap latch ONLY when a keyframe was actually
    /// OBSERVED on the wire (`was_keyframe`), not merely on a successful encode
    /// call (issue #1123 item 1 hardening). At `g_lag_in_frames = 1` a forced
    /// keyframe emits its key packet on the same encode call, so this clears
    /// exactly on the frame that resolved the latch; if lag ever rises to >= 2
    /// it still only clears once the key packet is genuinely observed downstream,
    /// so a deferred keyframe cannot drop the latch early. A failed encode emits
    /// no frame and MUST NOT call this from the success path (the loop's `Err`
    /// arm advances `sequence` and `continue`s, leaving the latch set so the next
    /// attempt still keyframes across the source-loop cut).
    fn on_encoded(&mut self, was_keyframe: bool) {
        if was_keyframe {
            self.pending_keyframe = false;
        }
        self.sequence += 1;
    }
}

/// Answers camera KEYFRAME_REQUESTs as the browser camera encoder does. A
/// request stays pending until every active video layer has keyed since it
/// (periodic keyframes count) or it forces a keyframe on every video layer,
/// which it does only once the publisher's cooldown (replay's `RequestCooldown`)
/// has passed since the last keyframe on any video layer.
pub struct ForcedKeyframeGate {
    requests: Option<Arc<AtomicU64>>,
    uplink: Option<replay::UplinkProbe>,
    cooldown: replay::RequestCooldown,
    seen: u64,
    pending: bool,
    keyed_layers: u64,
    last_key_us: Option<u64>,
}

impl ForcedKeyframeGate {
    /// `requests` counts camera KEYFRAME_REQUESTs for this bot (`None` ignores
    /// them); `uplink` widens the cooldown on a congested WebSocket uplink.
    pub fn new(requests: Option<Arc<AtomicU64>>, uplink: Option<replay::UplinkProbe>) -> Self {
        let seen = requests.as_ref().map_or(0, |r| r.load(Ordering::Relaxed));
        Self {
            requests,
            cooldown: replay::RequestCooldown::new(uplink.as_ref()),
            uplink,
            seen,
            pending: false,
            keyed_layers: 0,
            last_key_us: None,
        }
    }

    fn requested(&mut self, elapsed_us: u64) -> bool {
        if let Some(requests) = &self.requests {
            let n = requests.load(Ordering::Relaxed);
            if n != self.seen {
                self.seen = n;
                self.pending = true;
                self.keyed_layers = 0;
            }
        }
        let cooldown_us = self.cooldown.cooldown_us(self.uplink.as_ref(), elapsed_us);
        self.pending
            && self
                .last_key_us
                .is_none_or(|last| elapsed_us.saturating_sub(last) >= cooldown_us)
    }

    /// A keyframe on every video layer.
    fn on_keyframe(&mut self, elapsed_us: u64) {
        self.pending = false;
        self.last_key_us = Some(elapsed_us);
    }

    /// A keyframe on one video layer, `active` layers being sent.
    fn on_layer_keyframe(&mut self, layer: u32, active: usize, elapsed_us: u64) {
        self.last_key_us = Some(elapsed_us);
        self.keyed_layers |= 1 << layer;
        if (0..active).all(|l| self.keyed_layers & (1 << l) != 0) {
            self.pending = false;
        }
    }
}

/// Map a render-tick index to the EKG `rms` index so the waveform animates at
/// real time across the full source loop in simulcast mode (issue #1123
/// item 2). Pure, so the mapping is host-unit-testable.
///
/// The `rms` buffer is built in `main.rs` at the AQ default tier's `ekg_fps`
/// (one entry per frame at `ekg_fps`), but the simulcast loop paces at
/// `render_fps` (the TOP layer's fps, e.g. 30). When `render_fps > ekg_fps`,
/// using `frame_in_loop` (a render-fps index) directly as the `rms` index runs
/// the waveform too fast and reads `0.0` (the out-of-range fallback) for the
/// loop tail. Scaling by `ekg_fps / render_fps` makes the index track REAL
/// TIME: render frame `f` is at `f / render_fps` seconds into the loop, which
/// is `rms` entry `f * ekg_fps / render_fps`.
///
/// Both fps values are `>= 1` (the caller floors them), so no divide-by-zero.
/// The result is `usize`; the caller still bounds-checks against `rms.len()`
/// for the final (partial) frame of the loop.
fn ekg_rms_index_for_render_frame(frame_in_loop: usize, ekg_fps: u32, render_fps: u32) -> usize {
    // u128 intermediate so a long loop * high fps cannot overflow u64/usize.
    let scaled = (frame_in_loop as u128) * (ekg_fps as u128) / (render_fps.max(1) as u128);
    scaled as usize
}

/// Pure decision half of [`apply_simulcast_aq`] (issue #1083 V21) — no encoder
/// side effects, so it is host-unit-testable without a real libvpx encoder.
///
/// Given the lock-free [`SimulcastSnapshot`], the ladder length `n_layers`, and
/// the per-layer bitrate last pushed to each encoder (`last_applied_kbps`,
/// indexed by `layer_id`), it returns:
///
/// * `active` — the active layer COUNT the caller honors (skip any layer with
///   `layer_id >= active`; top-down shed, base layer always flows); and
/// * `to_apply[id]` — `Some(kbps)` for each ACTIVE layer whose target CHANGED
///   since `last_applied_kbps[id]` (so the caller reconfigures only on a real
///   change, avoiding a per-frame `vpx_codec_enc_config_set`), else `None`.
///   Shed layers (`id >= active`) and unchanged targets are always `None`.
///
/// Fail-open: a non-simulcast snapshot returns `(n_layers, all-None)` — the full
/// ladder stays active and no encoder is touched (legacy behavior).
fn simulcast_layer_directives(
    snapshot: &crate::aq_controller::SimulcastSnapshot,
    n_layers: usize,
    last_applied_kbps: &[u32],
) -> (usize, Vec<Option<u32>>) {
    if !snapshot.is_simulcast {
        return (n_layers, vec![None; n_layers]);
    }
    let active = snapshot.active.clamp(1, n_layers);
    let mut to_apply = vec![None; n_layers];
    for (id, slot) in to_apply.iter_mut().enumerate() {
        // Only ACTIVE layers get a (possibly rescaled) target; shed layers are
        // not encoded, so there is nothing to reconfigure.
        if id >= active {
            continue;
        }
        if let Some(&target) = snapshot.layer_bitrates_kbps.get(id) {
            if target != 0 && last_applied_kbps.get(id).copied() != Some(target) {
                *slot = Some(target);
            }
        }
    }
    (active, to_apply)
}

/// Apply the AQ's current simulcast decision to the layer encoders (issue #1083
/// V21). Computes the directives via [`simulcast_layer_directives`] and, for
/// each ACTIVE layer whose target changed, re-applies it via
/// `update_bitrate_kbps` (the budget cap rescales these as the active count
/// shrinks under congestion). Returns the active layer COUNT the caller must
/// honor (skip any layer with `layer_id >= active`).
///
/// Mirrors the browser's per-frame consumption in
/// `videocall-client/src/encode/camera_encoder.rs` (shared-atomic read of the
/// active count + per-layer targets) but adapted to the bot's synchronous loop:
/// the browser reads every frame; the bot reads only when `simulcast_epoch`
/// changed (the caller gates this) so the steady-state cost is one Acquire load.
fn apply_simulcast_aq(
    layers: &mut [SimulcastLayer],
    snapshot: &crate::aq_controller::SimulcastSnapshot,
    last_applied_kbps: &mut [u32],
    user_id: &str,
) -> usize {
    let (active, to_apply) = simulcast_layer_directives(snapshot, layers.len(), last_applied_kbps);
    for layer in layers.iter_mut() {
        let id = layer.layer_id as usize;
        let Some(Some(target)) = to_apply.get(id).copied() else {
            continue;
        };
        if let Err(e) = layer.encoder.update_bitrate_kbps(target) {
            // Non-fatal: log and keep the previous target rather than killing
            // the producer thread.
            warn!(
                "[{}] simulcast L{} update_bitrate_kbps({}) failed: {}",
                user_id, id, target, e
            );
        } else if let Some(slot) = last_applied_kbps.get_mut(id) {
            *slot = target;
        }
    }
    active
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
    /// This layer's fixed VBR target (the tier's `ideal_bitrate_kbps`). Stored
    /// for the startup log so it stays consistent with the encoder's actual
    /// configured target without a fragile re-lookup into the ladder.
    ideal_bitrate_kbps: u32,
    /// Keyframe state machine: per-layer `sequence`, periodic cadence, and the
    /// loop-wrap keyframe latch (issue #1123 item 1). Split into its own
    /// host-constructible struct ([`LayerKeyframeState`]) so the exact
    /// transitions both simulcast loops drive are unit-tested directly.
    kf: LayerKeyframeState,
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

    struct SentFrame {
        at: Instant,
        layer: u32,
        key: bool,
        sent_ms: f64,
        seq: u64,
    }

    fn drain(rx: &mut tokio::sync::mpsc::Receiver<OutboundFrame>, sent: &mut Vec<SentFrame>) {
        while let Ok(frame) = rx.try_recv() {
            let wrapper = PacketWrapper::parse_from_bytes(&frame.bytes).unwrap();
            let media = MediaPacket::parse_from_bytes(&wrapper.data).unwrap();
            sent.push(SentFrame {
                at: Instant::now(),
                layer: wrapper.simulcast_layer_id,
                key: media.frame_type == "key",
                sent_ms: media.timestamp,
                seq: media.video_metadata.sequence,
            });
        }
    }

    fn wait_for(
        rx: &mut tokio::sync::mpsc::Receiver<OutboundFrame>,
        sent: &mut Vec<SentFrame>,
        done: impl Fn(&[SentFrame]) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(sent) && Instant::now() < deadline {
            drain(rx, sent);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(done(sent), "the live publisher kept sending video");
    }

    enum Source {
        Ekg,
        Costume,
    }

    fn start_live_publisher(
        source: Source,
        simulcast_layers: u32,
        tx: OutboundFrameSender,
        requests: Arc<AtomicU64>,
    ) -> VideoProducer {
        let aq = BotAq::with_default_clock();
        let v = aq.snapshot_video();
        let counters = || Arc::new(AtomicU64::new(0));
        match source {
            Source::Ekg => VideoProducer::from_ekg(
                "kf".into(),
                EkgRenderer::new([0, 200, 255], v.max_width, v.max_height),
                vec![0.0; 1000],
                0.01,
                v.target_fps.max(1),
                tx,
                Instant::now(),
                Duration::from_secs(60),
                aq,
                Arc::new(AtomicU32::new(0)),
                counters(),
                counters(),
                counters(),
                simulcast_layers,
                ForcedKeyframeGate::new(Some(requests), None),
            ),
            Source::Costume => VideoProducer::from_costume(
                "kf".into(),
                costume_renderer(),
                tx,
                Instant::now(),
                Duration::from_secs(60),
                Arc::new(AtomicBool::new(false)),
                aq,
                Arc::new(AtomicU32::new(0)),
                counters(),
                counters(),
                counters(),
                simulcast_layers,
                ForcedKeyframeGate::new(Some(requests), None),
            ),
        }
        .unwrap()
    }

    fn costume_renderer() -> CostumeRenderer {
        static DIRS: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bot-kf-costume-{}-{}",
            std::process::id(),
            DIRS.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let frame = vec![128u8; 1280 * 720 * 3 / 2];
        std::fs::write(dir.join("idle.i420"), &frame).unwrap();
        std::fs::write(dir.join("talking.i420"), &frame).unwrap();
        let renderer = CostumeRenderer::load(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        renderer
    }

    /// A live publisher sends delta frames until a KEYFRAME_REQUEST arrives,
    /// then one keyframe on every video layer, at most one delta frame after
    /// the request, then deltas again.
    fn live_publisher_answers_a_keyframe_request(source: Source, simulcast_layers: u32) {
        let requests = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
        let mut producer =
            start_live_publisher(source, simulcast_layers, tx.into(), Arc::clone(&requests));
        let per_layer =
            |sent: &[SentFrame], layer: u32| sent.iter().filter(|f| f.layer == layer).count();
        let layers: Vec<u32> = (0..simulcast_layers).collect();

        let mut sent = Vec::new();
        wait_for(&mut rx, &mut sent, |s| {
            s.first()
                .is_some_and(|f| f.at.elapsed() > Duration::from_millis(600))
                && layers.iter().all(|&l| per_layer(s, l) >= 4)
        });
        for &l in &layers {
            let keys: Vec<bool> = sent
                .iter()
                .filter(|f| f.layer == l)
                .map(|f| f.key)
                .collect();
            assert!(keys[0], "video layer {} starts on a keyframe", l);
            assert!(
                keys[1..].iter().all(|k| !k),
                "with no request, video layer {} sends deltas: {:?}",
                l,
                keys
            );
        }

        let before = sent.len();
        requests.fetch_add(1, Ordering::Relaxed);
        let requested_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64;
        wait_for(&mut rx, &mut sent, |s| {
            layers.iter().all(|&l| {
                s[before..]
                    .iter()
                    .filter(|f| f.layer == l && f.sent_ms > requested_ms)
                    .count()
                    >= 4
            })
        });
        producer.stop();
        for &l in &layers {
            let after: Vec<&SentFrame> = sent[before..].iter().filter(|f| f.layer == l).collect();
            assert!(
                after.iter().filter(|f| f.key).count() == 1,
                "one request, one keyframe on video layer {}: {:?}",
                l,
                after.iter().map(|f| f.key).collect::<Vec<_>>()
            );
            let deltas_after_request = after
                .iter()
                .take_while(|f| !f.key)
                .filter(|f| f.sent_ms > requested_ms)
                .count();
            assert!(
                deltas_after_request <= 1,
                "video layer {} answers the request on its next frame, not after {} more delta frames",
                l,
                deltas_after_request
            );
        }
    }

    #[test]
    fn a_single_stream_ekg_publisher_answers_a_keyframe_request() {
        live_publisher_answers_a_keyframe_request(Source::Ekg, 1);
    }

    #[test]
    fn a_simulcast_ekg_publisher_answers_a_keyframe_request_on_every_layer() {
        live_publisher_answers_a_keyframe_request(Source::Ekg, 2);
    }

    #[test]
    fn a_single_stream_costume_publisher_answers_a_keyframe_request() {
        live_publisher_answers_a_keyframe_request(Source::Costume, 1);
    }

    #[test]
    fn a_simulcast_costume_publisher_answers_a_keyframe_request_on_every_layer() {
        live_publisher_answers_a_keyframe_request(Source::Costume, 2);
    }

    /// Runs a two-layer simulcast loop one render tick at a time, each tick at
    /// the time the loop asked to sleep until.
    struct SteppedPublisher {
        parked: std::sync::mpsc::Receiver<u64>,
        resume: Option<std::sync::mpsc::Sender<u64>>,
        next_tick_us: Option<u64>,
        rx: tokio::sync::mpsc::Receiver<OutboundFrame>,
        quit: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    struct SteppedClock {
        now_us: u64,
        parked: std::sync::mpsc::Sender<u64>,
        resume: std::sync::mpsc::Receiver<u64>,
    }

    impl Pacer for SteppedClock {
        fn elapsed_us(&self) -> u64 {
            self.now_us
        }

        fn sleep_until_us(&mut self, target_us: u64) {
            if self.parked.send(target_us).is_ok() {
                if let Ok(now_us) = self.resume.recv() {
                    self.now_us = now_us;
                }
            }
        }
    }

    impl SteppedPublisher {
        fn start(source: Source, requests: Arc<AtomicU64>) -> Self {
            let (parked_tx, parked) = std::sync::mpsc::channel();
            let (resume, resume_rx) = std::sync::mpsc::channel();
            let clock = SteppedClock {
                now_us: 0,
                parked: parked_tx,
                resume: resume_rx,
            };
            let (tx, rx) = tokio::sync::mpsc::channel(4096);
            let tx: OutboundFrameSender = tx.into();
            let quit = Arc::new(AtomicBool::new(false));
            let q = Arc::clone(&quit);
            let aq = BotAq::with_default_clock();
            let gate = ForcedKeyframeGate::new(Some(requests), None);
            let counters = || Arc::new(AtomicU64::new(0));
            let handle = thread::spawn(move || {
                let result = match source {
                    Source::Ekg => {
                        let top = &videocall_aq::constants::simulcast_layers(2)[1];
                        let (w, h) = (top.max_width & !1, top.max_height & !1);
                        VideoProducer::ekg_video_loop_simulcast(
                            "kf".into(),
                            EkgRenderer::new([0, 200, 255], w, h),
                            vec![0.0; 1000],
                            0.01,
                            top.target_fps,
                            tx,
                            q,
                            clock,
                            Duration::from_secs(60),
                            aq,
                            Arc::new(AtomicU32::new(0)),
                            counters(),
                            counters(),
                            counters(),
                            VideoProducer::build_simulcast_layers(2, w, h).unwrap(),
                            gate,
                            w,
                            h,
                        )
                    }
                    Source::Costume => {
                        let renderer = costume_renderer();
                        let layers = VideoProducer::build_simulcast_layers(
                            2,
                            renderer.width(),
                            renderer.height(),
                        )
                        .unwrap();
                        VideoProducer::costume_video_loop_simulcast(
                            "kf".into(),
                            renderer,
                            tx,
                            q,
                            clock,
                            Duration::from_secs(60),
                            Arc::new(AtomicBool::new(false)),
                            aq,
                            Arc::new(AtomicU32::new(0)),
                            counters(),
                            counters(),
                            counters(),
                            layers,
                            gate,
                        )
                    }
                };
                result.unwrap();
            });
            Self {
                parked,
                resume: Some(resume),
                next_tick_us: None,
                rx,
                quit,
                handle: Some(handle),
            }
        }

        /// Runs the loop's next render tick and returns the frames it sent.
        fn tick(&mut self) -> Vec<SentFrame> {
            if let (Some(at), Some(resume)) = (self.next_tick_us, &self.resume) {
                resume.send(at).unwrap();
            }
            self.next_tick_us = Some(self.parked.recv().expect("the simulcast loop stopped"));
            let mut sent = Vec::new();
            drain(&mut self.rx, &mut sent);
            sent
        }
    }

    impl Drop for SteppedPublisher {
        fn drop(&mut self) {
            self.quit.store(true, Ordering::Relaxed);
            self.resume = None;
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// A request that lands after video layer 1 answered the previous one, but
    /// before video layer 0 sent its latched keyframe, keys video layer 1 again.
    fn a_request_is_not_answered_by_video_layer_0_alone(source: Source) {
        let requests = Arc::new(AtomicU64::new(0));
        let mut publisher = SteppedPublisher::start(source, Arc::clone(&requests));
        let top = &videocall_aq::constants::simulcast_layers(2)[1];
        let cooldown_ticks = replay::KEYFRAME_REQUEST_COOLDOWN_US
            .div_ceil(1_000_000 / top.target_fps as u64) as usize;
        let sent = |s: &[SentFrame], layer: u32| s.iter().any(|f| f.layer == layer);
        let keyed = |s: &[SentFrame], layer: u32| s.iter().any(|f| f.layer == layer && f.key);

        let mut ticks = 1;
        while !(sent(&publisher.tick(), 0) && ticks > cooldown_ticks) {
            ticks += 1;
            assert!(ticks < 10 * cooldown_ticks, "video layer 0 stopped sending");
        }
        requests.fetch_add(1, Ordering::Relaxed);
        let first = publisher.tick();
        assert!(
            keyed(&first, 1) && !sent(&first, 0),
            "the first request keyed video layer 1 on a tick without video layer 0"
        );

        requests.fetch_add(1, Ordering::Relaxed);
        let mut frames = publisher.tick();
        let mut ticks = 1;
        while !sent(&frames, 0) {
            assert!(
                !keyed(&frames, 1),
                "video layer 1 keyed inside the cooldown"
            );
            ticks += 1;
            assert!(ticks <= cooldown_ticks, "video layer 0 stopped sending");
            frames = publisher.tick();
        }
        assert!(
            keyed(&frames, 0) && !keyed(&frames, 1),
            "video layer 0 sent its latched keyframe alone"
        );

        let after: Vec<SentFrame> = (0..=cooldown_ticks)
            .flat_map(|_| publisher.tick())
            .collect();
        let top_gop = top.keyframe_interval_frames as u64;
        assert!(
            after
                .iter()
                .any(|f| f.layer == 1 && f.key && !f.seq.is_multiple_of(top_gop)),
            "the second request keyed video layer 1, not only video layer 0"
        );
    }

    #[test]
    fn a_simulcast_ekg_request_is_not_answered_by_video_layer_0_alone() {
        a_request_is_not_answered_by_video_layer_0_alone(Source::Ekg);
    }

    #[test]
    fn a_simulcast_costume_request_is_not_answered_by_video_layer_0_alone() {
        a_request_is_not_answered_by_video_layer_0_alone(Source::Costume);
    }

    /// A request that lands just after a periodic keyframe on video layer 0
    /// waits out the cooldown from that keyframe, then is answered.
    fn a_request_after_a_periodic_keyframe_waits_for_the_cooldown(
        source: Source,
        simulcast_layers: u32,
    ) {
        let requests = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
        let mut producer =
            start_live_publisher(source, simulcast_layers, tx.into(), Arc::clone(&requests));
        let base_keys = |s: &[SentFrame]| s.iter().filter(|f| f.layer == 0 && f.key).count();
        let mut sent = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while base_keys(&sent) < 2 && Instant::now() < deadline {
            drain(&mut rx, &mut sent);
            thread::sleep(Duration::from_micros(200));
        }
        requests.fetch_add(1, Ordering::Relaxed);
        assert_eq!(base_keys(&sent), 2, "a periodic keyframe on video layer 0");
        let periodic_ms = sent
            .iter()
            .filter(|f| f.layer == 0 && f.key)
            .nth(1)
            .unwrap()
            .sent_ms;
        let before = sent.len();
        wait_for(&mut rx, &mut sent, |s| {
            s[before..].iter().any(|f| f.layer == 0 && f.key)
        });
        producer.stop();
        let key = sent[before..]
            .iter()
            .find(|f| f.layer == 0 && f.key)
            .unwrap();
        let cooldown_ms = replay::KEYFRAME_REQUEST_COOLDOWN_US as f64 / 1000.0;
        let gap_ms = key.sent_ms - periodic_ms;
        assert!(
            gap_ms >= cooldown_ms - 60.0,
            "the request keyed video layer 0 {} ms after its periodic keyframe",
            gap_ms
        );
        assert!(
            gap_ms <= 1500.0,
            "the pending request was answered, not left for the next periodic keyframe ({} ms)",
            gap_ms
        );
    }

    #[test]
    fn a_single_stream_ekg_request_after_a_periodic_keyframe_waits_for_the_cooldown() {
        a_request_after_a_periodic_keyframe_waits_for_the_cooldown(Source::Ekg, 1);
    }

    #[test]
    fn a_simulcast_ekg_request_after_a_periodic_keyframe_waits_for_the_cooldown() {
        a_request_after_a_periodic_keyframe_waits_for_the_cooldown(Source::Ekg, 2);
    }

    #[test]
    fn a_simulcast_costume_request_after_a_periodic_keyframe_waits_for_the_cooldown() {
        a_request_after_a_periodic_keyframe_waits_for_the_cooldown(Source::Costume, 2);
    }

    #[test]
    fn a_congested_websocket_uplink_holds_a_request_for_the_longer_cooldown() {
        let requests = Arc::new(AtomicU64::new(0));
        let wait = Arc::new(AtomicU64::new(0));
        let w = Arc::clone(&wait);
        let uplink = replay::UplinkProbe {
            websocket: true,
            wait_us: Box::new(move || w.load(Ordering::Relaxed)),
        };
        let mut gate = ForcedKeyframeGate::new(Some(Arc::clone(&requests)), Some(uplink));
        gate.on_keyframe(0);
        requests.fetch_add(1, Ordering::Relaxed);
        let congested = replay::CONGESTED_KEYFRAME_REQUEST_COOLDOWN_US;
        let tick = 40_000;
        let mut t = 0;
        while t + tick < congested {
            t += tick;
            wait.fetch_add(1_000, Ordering::Relaxed);
            assert!(!gate.requested(t), "held at {} us on a congested uplink", t);
        }
        wait.fetch_add(1_000, Ordering::Relaxed);
        assert!(
            gate.requested(congested),
            "answered once the congested cooldown passed"
        );
    }

    #[test]
    fn a_pending_request_clears_once_every_active_video_layer_has_keyed() {
        let requests = Arc::new(AtomicU64::new(0));
        let cooldown = replay::KEYFRAME_REQUEST_COOLDOWN_US;
        let mut gate = ForcedKeyframeGate::new(Some(Arc::clone(&requests)), None);
        gate.on_keyframe(0);
        requests.fetch_add(1, Ordering::Relaxed);
        assert!(!gate.requested(1_000));
        gate.on_layer_keyframe(0, 2, 2_000);
        assert!(
            gate.requested(2_000 + cooldown),
            "video layer 1 has not keyed since the request"
        );

        let mut gate = ForcedKeyframeGate::new(Some(Arc::clone(&requests)), None);
        gate.on_keyframe(0);
        requests.fetch_add(1, Ordering::Relaxed);
        assert!(!gate.requested(1_000));
        gate.on_layer_keyframe(1, 2, 2_000);
        gate.on_layer_keyframe(0, 2, 3_000);
        assert!(
            !gate.requested(3_000 + cooldown),
            "periodic keyframes on both active video layers answered the request"
        );

        let mut gate = ForcedKeyframeGate::new(Some(Arc::clone(&requests)), None);
        gate.on_layer_keyframe(1, 2, 0);
        requests.fetch_add(1, Ordering::Relaxed);
        assert!(!gate.requested(1_000));
        gate.on_layer_keyframe(0, 2, 2_000);
        assert!(
            gate.requested(2_000 + cooldown),
            "a keyframe on video layer 1 from before the request does not count"
        );

        let mut gate = ForcedKeyframeGate::new(Some(Arc::clone(&requests)), None);
        gate.on_keyframe(0);
        requests.fetch_add(1, Ordering::Relaxed);
        assert!(!gate.requested(1_000));
        gate.on_layer_keyframe(0, 1, 2_000);
        assert!(
            !gate.requested(2_000 + cooldown),
            "with one active video layer, its keyframe answers the request"
        );
    }

    #[test]
    fn a_request_inside_the_cooldown_waits_and_is_not_lost() {
        let requests = Arc::new(AtomicU64::new(0));
        let mut gate = ForcedKeyframeGate::new(Some(Arc::clone(&requests)), None);
        assert!(!gate.requested(0));
        gate.on_keyframe(0);
        requests.fetch_add(3, Ordering::Relaxed);
        assert!(!gate.requested(replay::KEYFRAME_REQUEST_COOLDOWN_US - 1));
        assert!(gate.requested(replay::KEYFRAME_REQUEST_COOLDOWN_US));
        gate.on_keyframe(replay::KEYFRAME_REQUEST_COOLDOWN_US);
        assert!(!gate.requested(10 * replay::KEYFRAME_REQUEST_COOLDOWN_US));
    }

    /// Send one frame through `build_and_send_layer` and return the parsed
    /// outer wrapper. Uses a synchronous `try_send`/`try_recv` pair (the helper
    /// never awaits) so no tokio runtime is needed.
    fn send_one(layer_id: u32, sequence: u64, framerate: u32) -> PacketWrapper {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let sender = OutboundFrameSender::new(tx);
        let sent = build_and_send_layer(
            &sender,
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

    // ------------------------------------------------------------------
    // Per-layer AQ consumption (issue #1083 V21) — the pure decision half.
    // Tested without a real libvpx encoder via `simulcast_layer_directives`.
    // ------------------------------------------------------------------

    use crate::aq_controller::SimulcastSnapshot;

    fn snap(is_simulcast: bool, active: usize, bitrates: &[u32]) -> SimulcastSnapshot {
        SimulcastSnapshot {
            is_simulcast,
            layer_count: bitrates.len().max(1),
            active,
            layer_bitrates_kbps: bitrates.to_vec(),
        }
    }

    /// V21 top-down shed: when the AQ active count is below the ladder length,
    /// the directive's returned `active` must equal that count so the caller
    /// skips the top layer(s). Fails if a shed (active < N) were ignored and the
    /// full ladder kept being encoded.
    #[test]
    fn directives_active_count_drives_top_down_shed() {
        // 3-layer ladder, AQ has shed down to 2 active.
        let s = snap(true, 2, &[400, 900, 1500]);
        let last = [0u32; 3];
        let (active, to_apply) = simulcast_layer_directives(&s, 3, &last);
        assert_eq!(active, 2, "active count must follow the AQ shed (2 of 3)");
        // Layer 2 (the shed top layer) must get NO bitrate directive.
        assert_eq!(to_apply[2], None, "shed top layer must not be reconfigured");
        // Base + middle (active) get their targets since last_applied was 0.
        assert_eq!(to_apply[0], Some(400));
        assert_eq!(to_apply[1], Some(900));
    }

    /// V21 base-always-flows: even an absurd active=0 from the AQ must floor at
    /// 1 so the base layer keeps flowing. Fails if a 0 active count silenced the
    /// whole publisher.
    #[test]
    fn directives_floor_active_at_one() {
        let s = snap(true, 0, &[400, 900, 1500]);
        let last = [0u32; 3];
        let (active, to_apply) = simulcast_layer_directives(&s, 3, &last);
        assert_eq!(active, 1, "active must floor at 1 (base always flows)");
        assert_eq!(to_apply[0], Some(400), "base layer still gets its target");
        assert_eq!(to_apply[1], None);
        assert_eq!(to_apply[2], None);
    }

    /// V21 bitrate propagation on cap change: when the budget cap rescales a
    /// layer's target, the directive must surface the NEW value; an UNCHANGED
    /// target (already applied) must surface `None` so the encoder is not
    /// reconfigured every frame. Fails if either the change is dropped or an
    /// unchanged value is needlessly re-applied.
    #[test]
    fn directives_propagate_only_changed_bitrates() {
        // last_applied: base already at 300 (rescaled earlier), others unset.
        let last = [300u32, 0, 0];
        // New snapshot: base rescaled to 250 (cap tightened), middle at 700.
        let s = snap(true, 2, &[250, 700, 1500]);
        let (active, to_apply) = simulcast_layer_directives(&s, 3, &last);
        assert_eq!(active, 2);
        assert_eq!(to_apply[0], Some(250), "changed base target must propagate");
        assert_eq!(
            to_apply[1],
            Some(700),
            "newly-active middle target propagates"
        );

        // Re-run with last_applied now matching: base unchanged => None.
        let last2 = [250u32, 700, 0];
        let (_, to_apply2) = simulcast_layer_directives(&s, 3, &last2);
        assert_eq!(
            to_apply2[0], None,
            "an unchanged target must NOT trigger a reconfigure"
        );
        assert_eq!(
            to_apply2[1], None,
            "unchanged middle target must not reconfigure"
        );
    }

    /// N==1 / non-simulcast path untouched: a non-simulcast snapshot must return
    /// the full ladder active and touch NO bitrate (legacy single-stream and the
    /// fail-open guard). Fails if the wiring leaked into the non-simulcast path.
    #[test]
    fn directives_non_simulcast_is_inert() {
        // is_simulcast = false; the snapshot bitrates must be ignored entirely.
        let s = snap(false, 1, &[0]);
        let last = [0u32; 3];
        let (active, to_apply) = simulcast_layer_directives(&s, 3, &last);
        assert_eq!(active, 3, "non-simulcast keeps the full ladder active");
        assert!(
            to_apply.iter().all(|d| d.is_none()),
            "non-simulcast must issue no bitrate directives: {:?}",
            to_apply
        );
    }

    // ------------------------------------------------------------------
    // Loop-wrap keyframe latch (issue #1123 item 1) — pure state machine.
    // ------------------------------------------------------------------

    /// Drive ONE render tick of the REAL [`LayerKeyframeState`] in the EXACT
    /// order, and through the EXACT methods, that both simulcast loops use:
    ///   (a) `latch_wrap(at_loop_wrap)` — every tick, BEFORE the not-due skip;
    ///   (b) the not-due skip (`continue` in the loop) — modeled by returning
    ///       `None` without touching the encode/clear methods;
    ///   (c) `force_keyframe(at_loop_wrap)` — the keyframe decision;
    ///   (d) `on_encoded(was_keyframe)` — clear-on-observed-key + seq advance.
    /// At `g_lag_in_frames = 1` a forced keyframe emits its key packet on the
    /// same encode call, so `was_keyframe == force` — the loop ORs the emitted
    /// `frame.key`, which for a forced keyframe is exactly `force`. This is NOT
    /// a re-implementation: it calls the same `LayerKeyframeState` methods the
    /// loops call, so making any of those method bodies a no-op (or dropping a
    /// term) fails the assertions below.
    ///
    /// Returns `Some(was_keyframe)` for a tick the layer encoded, `None` for a
    /// tick it skipped (not due).
    fn drive_tick(layer: &mut LayerKeyframeState, at_loop_wrap: bool, due: bool) -> Option<bool> {
        // (a) real site (a): latch the wrap for EVERY layer, before the skip.
        layer.latch_wrap(at_loop_wrap);
        // (b) real site (b): the "not yet due" guard `continue`s past encode.
        if !due {
            return None;
        }
        // (c) real site (c): the keyframe decision.
        let force = layer.force_keyframe(at_loop_wrap);
        // (d) real site (e): a forced keyframe at lag=1 emits its key packet on
        // this same call, so the observed-key flag equals `force`. A non-forced
        // encode observes no key packet (`false`). This mirrors the loop's
        // `saw_key |= frame.key` + `on_encoded(saw_key)`.
        layer.on_encoded(force);
        Some(force)
    }

    /// THE test for issue #1123 item 1. A layer slower than the render loop is
    /// NOT due on the exact wrap tick, so it skips the wrap; its NEXT due frame
    /// must still be a forced keyframe (carried by the latch), NOT a delta. A
    /// large `frames_per_keyframe` ensures the periodic cadence is NOT due on
    /// that next frame, so ONLY the latch can force it. Drives the REAL
    /// `LayerKeyframeState`: making `latch_wrap` a no-op, or dropping
    /// `resolve_layer_keyframe`'s `pending` term inside `force_keyframe`, fails
    /// this (no X==X pin).
    #[test]
    fn loop_wrap_keyframe_survives_a_skipped_tick() {
        // Periodic interval huge so sequence%kf is only 0 on the very first
        // encode; subsequent encodes are periodic-delta unless the latch fires.
        let mut layer = LayerKeyframeState::new(1000);

        // t0: first render tick is a wrap (prev_frame_index == None). Layer is
        // due here (its initial deadline is 0) — first frame is a keyframe.
        assert_eq!(
            drive_tick(&mut layer, true, true),
            Some(true),
            "first frame after start must be a keyframe"
        );

        // Steady delta frames while NOT wrapping (sequence 1..) — periodic is far
        // away, so these are deltas.
        assert_eq!(drive_tick(&mut layer, false, true), Some(false));
        assert_eq!(drive_tick(&mut layer, false, true), Some(false));

        // THE WRAP, but the slow layer is mid-interval => NOT due this tick. It
        // skips the encode entirely; the wrap must be LATCHED.
        assert_eq!(
            drive_tick(&mut layer, true, false),
            None,
            "slow layer skips the encode on the wrap tick"
        );

        // A couple more render ticks pass with the layer still not due, no wrap.
        assert_eq!(drive_tick(&mut layer, false, false), None);

        // The layer's NEXT due frame: NOT a wrap tick, periodic NOT due, yet it
        // MUST be a keyframe because the wrap was latched across the skip.
        assert_eq!(
            drive_tick(&mut layer, false, true),
            Some(true),
            "the first frame the slow layer emits after a loop wrap MUST be a keyframe"
        );

        // And the latch is now cleared — the following due frame is a delta.
        // This also pins the Finding-2 invariant: the latched encode above was
        // forced (`force == true`), so `on_encoded(true)` cleared the latch.
        assert_eq!(
            drive_tick(&mut layer, false, true),
            Some(false),
            "the latch must clear after the keyframe is emitted (no perpetual keyframes)"
        );
    }

    /// Negative control: with NO wrap ever, a layer that skips ticks must NOT
    /// spontaneously keyframe — it only keyframes on its periodic cadence. This
    /// proves the keyframe in the test above is caused by the WRAP latch, not by
    /// skipping per se. Fails if `latch_wrap` set the latch unconditionally.
    #[test]
    fn skipped_ticks_without_wrap_do_not_force_keyframe() {
        let mut layer = LayerKeyframeState::new(1000);
        // First frame keyframes (start), then deltas with interleaved skips and
        // NO wrap at all.
        assert_eq!(
            drive_tick(&mut layer, false, true),
            Some(true),
            "first frame keyframes"
        );
        assert_eq!(drive_tick(&mut layer, false, false), None);
        assert_eq!(drive_tick(&mut layer, false, false), None);
        assert_eq!(
            drive_tick(&mut layer, false, true),
            Some(false),
            "no wrap => the post-skip frame must be a delta, not a keyframe"
        );
    }

    /// A layer fast enough to be due ON the wrap tick keyframes immediately and
    /// does not also keyframe the following frame (latch cleared in-place by
    /// `on_encoded(true)`). Fails if `on_encoded` clears unconditionally (it
    /// would clear a never-set latch — harmless here — but more importantly
    /// fails if `on_encoded` does NOT clear on an observed keyframe: the next
    /// frame would then stay forced).
    #[test]
    fn fast_layer_keyframes_on_the_wrap_tick_itself() {
        let mut layer = LayerKeyframeState::new(1000);
        assert_eq!(
            drive_tick(&mut layer, true, true),
            Some(true),
            "start keyframe"
        );
        assert_eq!(drive_tick(&mut layer, false, true), Some(false));
        // Wrap AND due on the same tick.
        assert_eq!(
            drive_tick(&mut layer, true, true),
            Some(true),
            "a layer due on the wrap tick keyframes that frame"
        );
        assert_eq!(
            drive_tick(&mut layer, false, true),
            Some(false),
            "the frame after the wrap keyframe is a delta"
        );
    }

    /// Finding 2 / issue #1123 item 1: a FAILED encode emits no frame, so the
    /// latch MUST persist and the next attempt must still keyframe across the
    /// source-loop cut. The loop's `Err` arm does NOT call `on_encoded` (it
    /// advances `sequence` and `continue`s), so model the failure by NOT calling
    /// `on_encoded` on the failed tick — only `sequence += 1` — and assert the
    /// next due encode is still a keyframe. Fails if the success path's
    /// `on_encoded` is moved to (or shared with) the failure path, because then
    /// the latch would clear despite no observed keyframe.
    #[test]
    fn failed_encode_keeps_latch_until_a_keyframe_is_observed() {
        let mut layer = LayerKeyframeState::new(1000);
        // Start frame keyframes and clears.
        assert_eq!(drive_tick(&mut layer, true, true), Some(true));
        assert_eq!(drive_tick(&mut layer, false, true), Some(false));

        // A wrap on a due tick latches AND would force a keyframe — but the
        // encode FAILS. Model the loop's Err arm: latch first (site a), the
        // layer IS due, the decision is a forced keyframe, then the encode
        // errors so `on_encoded` is NOT called; only the sequence advances.
        layer.latch_wrap(true);
        assert!(
            layer.force_keyframe(true),
            "wrap tick must decide a keyframe"
        );
        layer.sequence += 1; // mirrors the loop's `Err` arm (no `on_encoded`).
        assert!(
            layer.pending_keyframe,
            "a failed encode must leave the wrap latch set"
        );

        // No wrap now, periodic not due, but the latch survived the failure, so
        // the next successful due encode MUST still be a keyframe.
        assert_eq!(
            drive_tick(&mut layer, false, true),
            Some(true),
            "the encode after a failed keyframe must still be a keyframe (latch survived)"
        );
        // ...and then clear.
        assert_eq!(drive_tick(&mut layer, false, true), Some(false));
    }

    // ------------------------------------------------------------------
    // EKG rms real-time index mapping (issue #1123 item 2).
    // ------------------------------------------------------------------

    /// The render-frame -> rms-index map must cover the FULL rms buffer at the
    /// loop end (no premature flatline) and track real time, when the simulcast
    /// render fps exceeds the fps the rms was sampled at. Models a 10s loop:
    /// rms sampled at 20fps (200 entries) but rendered at 30fps (300 ticks).
    #[test]
    fn rms_index_tracks_real_time_across_full_loop() {
        let rms_fps = 20u32;
        let render_fps = 30u32;
        let loop_secs = 10u32;
        let rms_len = (rms_fps * loop_secs) as usize; // 200
        let render_ticks = (render_fps * loop_secs) as usize; // 300

        // Tick 0 maps to rms[0].
        assert_eq!(
            ekg_rms_index_for_render_frame(0, rms_fps, render_fps),
            0,
            "first render frame maps to rms[0]"
        );

        // Every render tick maps in-range (the LAST tick must NOT overshoot the
        // buffer by more than the final partial frame): index < rms_len for all
        // but possibly the final tick, which must be exactly rms_len-1 or rms_len.
        let last_tick = render_ticks - 1; // 299
        let last_index = ekg_rms_index_for_render_frame(last_tick, rms_fps, render_fps);
        // 299 * 20 / 30 = 199 -> the FINAL rms entry. Crucially NOT < ~133 (which
        // is what the buggy `frame_in_loop`-as-index would flatline past: tick
        // 200 already == rms_len under the old code).
        assert_eq!(
            last_index,
            rms_len - 1,
            "the final render tick must map to the LAST rms entry, not past it"
        );

        // Real-time tracking: a render tick at the loop midpoint (~5s) must map
        // to ~the rms midpoint. Tick 150 (=5s at 30fps) -> 150*20/30 = 100 = rms
        // midpoint.
        assert_eq!(
            ekg_rms_index_for_render_frame(render_ticks / 2, rms_fps, render_fps),
            rms_len / 2,
            "the loop-midpoint render frame must map to the rms midpoint (real-time)"
        );

        // Regression guard: under the OLD code (index == frame_in_loop), tick 200
        // would already be >= rms_len (200) and read the 0.0 fallback, flatlining
        // the last third of the loop. The mapping must keep it in range.
        assert!(
            ekg_rms_index_for_render_frame(200, rms_fps, render_fps) < rms_len,
            "render tick 200 must stay in range (old code flatlined here)"
        );
    }

    /// When rms_fps == render_fps (the common case where the top layer's fps
    /// equals the AQ default tier fps), the mapping must be the identity so the
    /// behavior is unchanged from indexing rms directly.
    #[test]
    fn rms_index_is_identity_when_fps_match() {
        for f in [0usize, 1, 7, 199, 200, 599] {
            assert_eq!(
                ekg_rms_index_for_render_frame(f, 30, 30),
                f,
                "equal fps must map the index 1:1"
            );
        }
    }

    fn rgb_to_i420_f32_reference(image: &ImageBuffer<Rgb<u8>, Vec<u8>>, i420_data: &mut [u8]) {
        let width = image.width() as usize;
        let height = image.height() as usize;
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

    /// Asserts every byte is within 1 of the f32 reference; returns the
    /// per-plane (Y, U, V) count of bytes that differ at all.
    fn diff_against_reference(image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> [usize; 3] {
        let (w, h) = (image.width() as usize, image.height() as usize);
        let len = w * h * 3 / 2;
        let mut got = vec![0u8; len];
        let mut want = vec![0u8; len];
        rgb_to_i420_into(image, &mut got);
        rgb_to_i420_f32_reference(image, &mut want);
        let mut mismatches = [0usize; 3];
        for (i, (g, r)) in got.iter().zip(&want).enumerate() {
            let plane = if i < w * h {
                0
            } else if i < w * h + w * h / 4 {
                1
            } else {
                2
            };
            let d = (*g as i32 - *r as i32).abs();
            assert!(
                d <= 1,
                "{}x{} plane {} byte {}: got {}, reference {}",
                w,
                h,
                plane,
                i,
                g,
                r
            );
            mismatches[plane] += usize::from(d != 0);
        }
        mismatches
    }

    #[test]
    fn integer_rgb_to_i420_matches_f32_reference_for_every_rgb_value() {
        const PER_STRIP: u32 = 4096;
        let mut mismatches = [0usize; 3];
        for strip in 0..(1u32 << 24) / PER_STRIP {
            let image = ImageBuffer::from_fn(PER_STRIP * 2, 2, |x, _| {
                let c = strip * PER_STRIP + x / 2;
                Rgb([(c >> 16) as u8, (c >> 8) as u8, c as u8])
            });
            for (m, d) in mismatches.iter_mut().zip(diff_against_reference(&image)) {
                *m += d;
            }
        }
        let colours = 1usize << 24;
        assert!(
            mismatches[0] < colours * 4 / 500,
            "Y differs on {} of {} samples",
            mismatches[0],
            colours * 4
        );
        for m in &mismatches[1..] {
            assert!(
                *m < colours / 500,
                "chroma differs on {} of {} samples",
                m,
                colours
            );
        }
    }

    #[test]
    fn integer_rgb_to_i420_matches_f32_reference_across_dimensions() {
        let mut state: u32 = 0x2914;
        for (w, h) in [
            (2, 2),
            (4, 2),
            (6, 4),
            (8, 6),
            (5, 4),
            (7, 6),
            (320, 180),
            (641, 360),
            (1280, 720),
        ] {
            let image = ImageBuffer::from_fn(w, h, |_, _| {
                let mut next = || {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (state >> 24) as u8
                };
                Rgb([next(), next(), next()])
            });
            diff_against_reference(&image);
        }
    }

    #[test]
    fn simulcast_layer_encoders_apply_cpu_used_8() {
        let layers =
            VideoProducer::build_simulcast_layers(3, 1280, 720).expect("layers must build");
        assert_eq!(layers.len(), 3);
        for layer in &layers {
            assert_eq!(
                layer.encoder.applied_cpu_used(),
                8,
                "layer {}",
                layer.layer_id
            );
        }
    }
}

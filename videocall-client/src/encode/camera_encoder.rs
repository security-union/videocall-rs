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

use gloo_timers::future::sleep;
use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use js_sys::JsString;
use js_sys::Reflect;
use log::error;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::HtmlVideoElement;
use web_sys::LatencyMode;
use web_sys::MediaStream;
use web_sys::MediaStreamConstraints;
use web_sys::MediaStreamTrack;
use web_sys::MediaStreamTrackProcessor;
use web_sys::MediaStreamTrackProcessorInit;
use web_sys::ReadableStreamDefaultReader;
use web_sys::VideoEncoder;
use web_sys::VideoEncoderConfig;
use web_sys::VideoEncoderEncodeOptions;
use web_sys::VideoEncoderInit;
use web_sys::VideoFrame;
use web_sys::VideoTrack;

use super::super::client::VideoCallClient;
use super::encoder_state::EncoderState;
use super::transform::transform_video_chunk;

use crate::adaptive_quality_constants::{
    AUDIO_QUALITY_TIERS, BITRATE_CHANGE_THRESHOLD, ENCODER_PLI_COOLDOWN_MS, VIDEO_QUALITY_TIERS,
};
use crate::constants::get_video_codec_string;
use crate::diagnostics::adaptive_quality_manager::TierTransitionRecord;
use crate::diagnostics::EncoderBitrateController;
use crate::health_reporter::ClimbLimiterSnapshot;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;

/// [CameraEncoder] encodes the video from a camera and sends it through a [`VideoCallClient`](crate::VideoCallClient) connection.
///
/// To use this struct, the caller must first create an `HtmlVideoElement` DOM node, to which the
/// camera will be connected.
///
/// See also:
/// * [MicrophoneEncoder](crate::MicrophoneEncoder)
/// * [ScreenEncoder](crate::ScreenEncoder)
///
pub struct CameraEncoder {
    client: VideoCallClient,
    video_elem_id: String,
    state: EncoderState,
    current_bitrate: Rc<AtomicU32>,
    current_fps: Rc<AtomicU32>,
    on_encoder_settings_update: Callback<String>,
    on_error: Option<Callback<String>>,
    /// Tier-controlled max width. The encoding loop checks this and reconfigures
    /// the encoder when it changes. 0 means "use camera native resolution".
    tier_max_width: Rc<AtomicU32>,
    /// Tier-controlled max height.
    tier_max_height: Rc<AtomicU32>,
    /// Tier-controlled keyframe interval (frames).
    tier_keyframe_interval: Rc<AtomicU32>,
    /// When set to `true`, the next encoded frame will be forced as a keyframe.
    /// Used by the PLI (Picture Loss Indication) mechanism: when a remote peer
    /// detects missing frames and sends a KEYFRAME_REQUEST, the VideoCallClient
    /// sets this flag so the encoder produces an immediate keyframe.
    force_keyframe: Arc<AtomicBool>,
    /// When set to `true`, the encoder control loop calls
    /// `force_video_step_down()` on the next iteration. Set by the
    /// `VideoCallClient` when a CONGESTION signal arrives from the server.
    congestion_step_down: Arc<AtomicBool>,
    /// Shared audio tier bitrate (bps). Written by the camera encoder's
    /// quality manager when the audio tier changes. The microphone encoder
    /// reads this to know the current audio bitrate, avoiding a duplicate
    /// `EncoderBitrateController`.
    shared_audio_tier_bitrate: Rc<AtomicU32>,
    /// Shared audio tier FEC flag. Written by the camera encoder's quality
    /// manager alongside `shared_audio_tier_bitrate`.
    shared_audio_tier_fec: Rc<AtomicBool>,
    /// Shared flag indicating whether screen share is active. Written by the
    /// `ScreenEncoder`, read by this camera encoder's diagnostics loop to
    /// coordinate bandwidth (drop camera tier and set ceiling when active).
    screen_sharing_active: Rc<AtomicBool>,
    /// Current video quality tier index (0=full_hd/best, 7=minimal).
    /// Updated whenever the adaptive quality manager changes tiers.
    shared_video_tier_index: Rc<AtomicU32>,
    /// Current audio quality tier index (0=high, 3=emergency).
    shared_audio_tier_index: Rc<AtomicU32>,
    /// Last fps_ratio from the encoder control loop (f32 bits in AtomicU32).
    shared_encoder_fps_ratio: Rc<AtomicU32>,
    /// Worst peer FPS from the encoder control loop (f32 bits in AtomicU32).
    shared_encoder_worst_peer_fps: Rc<AtomicU32>,
    /// Last bitrate_ratio from the encoder control loop (f32 bits in AtomicU32).
    shared_encoder_bitrate_ratio: Rc<AtomicU32>,
    /// PID target bitrate kbps from the encoder control loop (f32 bits in AtomicU32).
    shared_encoder_target_bitrate_kbps: Rc<AtomicU32>,
    /// Tier transition events buffer, drained by health reporter each health packet.
    shared_tier_transitions: Rc<RefCell<Vec<TierTransitionRecord>>>,
    /// Climb-rate limiter snapshot, updated by the encoder each tick, read by health reporter.
    shared_climb_limiter_snapshot: Rc<RefCell<ClimbLimiterSnapshot>>,
    /// Dwell time samples buffer, populated by encoder, drained by health reporter.
    shared_dwell_samples: Rc<RefCell<Vec<(String, f64)>>>,
    /// Re-election completed signal. Set by ConnectionManager, consumed by the
    /// encoder control loop to call `notify_reelection_completed()`.
    reelection_completed_signal: Rc<AtomicBool>,
}

impl CameraEncoder {
    /// Construct a camera encoder, with arguments:
    ///
    /// * `client` - an instance of a [`VideoCallClient`](crate::VideoCallClient).  It does not need to be currently connected.
    ///
    /// * `video_elem_id` - the the ID of an `HtmlVideoElement` to which the camera will be connected.  It does not need to currently exist.
    ///
    /// * `initial_bitrate` - the initial bitrate for the encoder, in kbps.
    ///
    /// * `on_encoder_settings_update` - a callback that will be called when the encoder settings change.
    ///
    /// The encoder is created in a disabled state, [`encoder.set_enabled(true)`](Self::set_enabled) must be called before it can start encoding.
    /// The encoder is created without a camera selected, [`encoder.select(device_id)`](Self::select) must be called before it can start encoding.
    pub fn new(
        client: VideoCallClient,
        video_elem_id: &str,
        initial_bitrate: u32,
        on_encoder_settings_update: Callback<String>,
        on_error: Callback<String>,
    ) -> Self {
        let default_tier = &VIDEO_QUALITY_TIERS[0];
        let default_audio_tier = &AUDIO_QUALITY_TIERS[0];
        Self {
            client,
            video_elem_id: video_elem_id.to_string(),
            state: EncoderState::new(),
            current_bitrate: Rc::new(AtomicU32::new(initial_bitrate)),
            current_fps: Rc::new(AtomicU32::new(0)),
            on_encoder_settings_update,
            on_error: Some(on_error),
            tier_max_width: Rc::new(AtomicU32::new(default_tier.max_width)),
            tier_max_height: Rc::new(AtomicU32::new(default_tier.max_height)),
            tier_keyframe_interval: Rc::new(AtomicU32::new(default_tier.keyframe_interval_frames)),
            force_keyframe: Arc::new(AtomicBool::new(false)),
            congestion_step_down: Arc::new(AtomicBool::new(false)),
            shared_audio_tier_bitrate: Rc::new(AtomicU32::new(
                default_audio_tier.bitrate_kbps * 1000,
            )),
            shared_audio_tier_fec: Rc::new(AtomicBool::new(default_audio_tier.enable_fec)),
            screen_sharing_active: Rc::new(AtomicBool::new(false)),
            shared_video_tier_index: Rc::new(AtomicU32::new(0)),
            shared_audio_tier_index: Rc::new(AtomicU32::new(0)),
            shared_encoder_fps_ratio: Rc::new(AtomicU32::new(0)),
            shared_encoder_worst_peer_fps: Rc::new(AtomicU32::new(0)),
            shared_encoder_bitrate_ratio: Rc::new(AtomicU32::new(0)),
            shared_encoder_target_bitrate_kbps: Rc::new(AtomicU32::new(0)),
            shared_tier_transitions: Rc::new(RefCell::new(Vec::new())),
            shared_climb_limiter_snapshot: Rc::new(RefCell::new(ClimbLimiterSnapshot::default())),
            shared_dwell_samples: Rc::new(RefCell::new(Vec::new())),
            reelection_completed_signal: Rc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_encoder_control(
        &mut self,
        mut diagnostics_receiver: UnboundedReceiver<DiagnosticsPacket>,
    ) {
        let current_bitrate = self.current_bitrate.clone();
        let current_fps = self.current_fps.clone();
        let on_encoder_settings_update = self.on_encoder_settings_update.clone();
        let enabled = self.state.enabled.clone();
        let tier_max_width = self.tier_max_width.clone();
        let tier_max_height = self.tier_max_height.clone();
        let tier_keyframe_interval = self.tier_keyframe_interval.clone();
        let congestion_flag = self.congestion_step_down.clone();
        let shared_audio_bitrate = self.shared_audio_tier_bitrate.clone();
        let shared_audio_fec = self.shared_audio_tier_fec.clone();
        let screen_sharing_active = self.screen_sharing_active.clone();
        let shared_video_tier_idx = self.shared_video_tier_index.clone();
        let shared_audio_tier_idx = self.shared_audio_tier_index.clone();
        let shared_encoder_fps_ratio = self.shared_encoder_fps_ratio.clone();
        let shared_encoder_worst_peer_fps = self.shared_encoder_worst_peer_fps.clone();
        let shared_encoder_bitrate_ratio = self.shared_encoder_bitrate_ratio.clone();
        let shared_encoder_target_bitrate_kbps = self.shared_encoder_target_bitrate_kbps.clone();
        let shared_tier_transitions = self.shared_tier_transitions.clone();
        let shared_climb_limiter_snapshot = self.shared_climb_limiter_snapshot.clone();
        let shared_dwell_samples = self.shared_dwell_samples.clone();
        let reelection_completed_signal = self.reelection_completed_signal.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let mut encoder_control = EncoderBitrateController::new(
                current_bitrate.load(Ordering::Relaxed),
                current_fps.clone(),
            );
            let mut prev_screen_active = false;
            let mut last_ws_drop_snapshot: u64 =
                videocall_transport::websocket::websocket_drop_count();
            let mut ws_drop_window_start_ms: f64 = js_sys::Date::now();
            while let Some(event) = diagnostics_receiver.next().await {
                // Check for screen sharing state transitions and coordinate
                // camera quality to avoid bandwidth contention.
                let screen_active = screen_sharing_active.load(Ordering::Acquire);
                if screen_active != prev_screen_active {
                    prev_screen_active = screen_active;
                    encoder_control.notify_screen_sharing(screen_active);
                    log::info!(
                        "CameraEncoder: screen sharing {} — camera tier coordination applied",
                        if screen_active { "ACTIVE" } else { "INACTIVE" },
                    );
                }

                // Check for server congestion step-down request before
                // processing the diagnostics packet so the forced step-down
                // takes effect immediately.
                if congestion_flag.swap(false, Ordering::AcqRel) {
                    log::warn!(
                        "CameraEncoder: server CONGESTION signal received, forcing video step-down"
                    );
                    encoder_control.force_video_step_down();
                }

                // Client-side WebSocket backpressure detection.
                // When the browser's TCP send buffer is full, outbound packets
                // are dropped locally (see websocket.rs send_binary). If enough
                // drops accumulate within the sliding window, self-trigger an AQ
                // step-down without waiting for the server. For WebTransport
                // users, websocket_drop_count() always returns 0 so this is a
                // no-op.
                {
                    let current_ws_drops = videocall_transport::websocket::websocket_drop_count();
                    let now_ms = js_sys::Date::now();
                    let elapsed_ms = now_ms - ws_drop_window_start_ms;

                    if elapsed_ms >= crate::adaptive_quality_constants::WS_SELF_CONGESTION_WINDOW_MS
                    {
                        let delta = current_ws_drops - last_ws_drop_snapshot;
                        if delta
                            >= crate::adaptive_quality_constants::WS_SELF_CONGESTION_DROP_THRESHOLD
                        {
                            log::warn!(
                                "CameraEncoder: client WS backpressure detected ({} drops in {:.0}ms), \
                                 forcing video step-down",
                                delta,
                                elapsed_ms,
                            );
                            encoder_control.force_video_step_down();
                        }
                        last_ws_drop_snapshot = current_ws_drops;
                        ws_drop_window_start_ms = now_ms;
                    }
                }

                let output_wasted = encoder_control.process_diagnostics_packet(event);

                // Write encoder decision inputs to shared atomics for health reporting.
                shared_encoder_fps_ratio.store(
                    (encoder_control.last_fps_ratio() as f32).to_bits(),
                    Ordering::Relaxed,
                );
                shared_encoder_worst_peer_fps.store(
                    (encoder_control.last_worst_peer_fps() as f32).to_bits(),
                    Ordering::Relaxed,
                );
                shared_encoder_bitrate_ratio.store(
                    (encoder_control.last_bitrate_ratio() as f32).to_bits(),
                    Ordering::Relaxed,
                );
                shared_encoder_target_bitrate_kbps.store(
                    (encoder_control.last_target_bitrate_kbps() as f32).to_bits(),
                    Ordering::Relaxed,
                );

                // Drain tier transitions into shared buffer for health reporting.
                let transitions = encoder_control.drain_tier_transitions();
                if !transitions.is_empty() {
                    shared_tier_transitions.borrow_mut().extend(transitions);
                }

                // Check re-election completed signal. When ConnectionManager
                // completes a re-election, it sets this flag. We consume it
                // here so the quality manager suppresses crash ceiling arming
                // during the server-swap transient.
                if reelection_completed_signal.swap(false, Ordering::AcqRel) {
                    log::info!("CameraEncoder: re-election completed, notifying quality manager");
                    encoder_control.notify_reelection_completed();
                }

                // Update climb-rate limiter snapshot for health reporting.
                if let Some(info) = encoder_control.crash_ceiling_info() {
                    let (ceiling_idx, _label, decay_ms) = info;
                    let mut snap = shared_climb_limiter_snapshot.borrow_mut();
                    snap.crash_ceiling_active = true;
                    snap.crash_ceiling_tier_index = Some(ceiling_idx as u32);
                    snap.crash_ceiling_decay_ms = Some(decay_ms);
                } else {
                    let mut snap = shared_climb_limiter_snapshot.borrow_mut();
                    snap.crash_ceiling_active = false;
                    snap.crash_ceiling_tier_index = None;
                    snap.crash_ceiling_decay_ms = None;
                }
                {
                    let (ceiling, slowdown, screen) = encoder_control.step_up_blocked_counts();
                    let mut snap = shared_climb_limiter_snapshot.borrow_mut();
                    snap.step_up_blocked_ceiling = ceiling;
                    snap.step_up_blocked_slowdown = slowdown;
                    snap.step_up_blocked_screen_share = screen;
                }

                // Drain dwell samples into shared buffer for health reporting.
                let dwells = encoder_control.drain_dwell_samples();
                if !dwells.is_empty() {
                    shared_dwell_samples.borrow_mut().extend(
                        dwells
                            .into_iter()
                            .map(|(label, ms)| (label.to_string(), ms)),
                    );
                }

                if let Some(bitrate) = output_wasted {
                    if enabled.load(Ordering::Acquire) {
                        // Only update if change is greater than threshold
                        let current = current_bitrate.load(Ordering::Relaxed) as f64;
                        let new = bitrate;
                        let percent_change = (new - current).abs() / current;

                        if percent_change > BITRATE_CHANGE_THRESHOLD {
                            on_encoder_settings_update.emit(format!("Bitrate: {bitrate:.2} kbps"));
                            current_bitrate.store(bitrate as u32, Ordering::Relaxed);
                        }
                    } else {
                        on_encoder_settings_update.emit("Disabled".to_string());
                    }
                }

                // Check if the quality manager triggered a tier change
                // (either from regular adaptation OR from the forced congestion
                // step-down above). Update shared atomics so the encoding loop
                // picks up the new resolution and keyframe interval.
                if encoder_control.take_tier_changed() {
                    let tier = encoder_control.current_video_tier();
                    tier_max_width.store(tier.max_width, Ordering::Relaxed);
                    tier_max_height.store(tier.max_height, Ordering::Relaxed);
                    tier_keyframe_interval.store(tier.keyframe_interval_frames, Ordering::Relaxed);
                    shared_video_tier_idx
                        .store(encoder_control.video_tier_index() as u32, Ordering::Relaxed);
                    log::info!(
                        "CameraEncoder: tier changed to '{}' ({}x{}, {}fps, kf={})",
                        tier.label,
                        tier.max_width,
                        tier.max_height,
                        tier.target_fps,
                        tier.keyframe_interval_frames,
                    );

                    // Also update shared audio tier atomics so the microphone
                    // encoder picks up the new audio quality settings without
                    // needing its own EncoderBitrateController.
                    let audio_tier = encoder_control.current_audio_tier();
                    shared_audio_bitrate.store(audio_tier.bitrate_kbps * 1000, Ordering::Relaxed);
                    shared_audio_fec.store(audio_tier.enable_fec, Ordering::Relaxed);
                    shared_audio_tier_idx
                        .store(encoder_control.audio_tier_index() as u32, Ordering::Relaxed);
                    log::info!(
                        "CameraEncoder: audio tier updated to '{}' ({}kbps, fec={})",
                        audio_tier.label,
                        audio_tier.bitrate_kbps,
                        audio_tier.enable_fec,
                    );
                }
            }
        });
    }

    /// Gets the current encoder output frame rate
    pub fn get_current_fps(&self) -> u32 {
        self.current_fps.load(Ordering::Relaxed)
    }

    /// Returns the shared audio tier bitrate atomic (bps).
    ///
    /// The microphone encoder reads this to track the current audio quality
    /// tier without needing its own `EncoderBitrateController`.
    pub fn shared_audio_tier_bitrate(&self) -> Rc<AtomicU32> {
        self.shared_audio_tier_bitrate.clone()
    }

    /// Returns the shared audio tier FEC flag.
    ///
    /// The microphone encoder reads this to decide whether to include
    /// RED-style redundancy in audio packets.
    pub fn shared_audio_tier_fec(&self) -> Rc<AtomicBool> {
        self.shared_audio_tier_fec.clone()
    }

    /// Returns the shared screen-sharing-active flag.
    ///
    /// The `ScreenEncoder` writes this flag when screen capture starts/stops.
    /// The camera encoder's diagnostics loop reads it to coordinate bandwidth.
    pub fn screen_sharing_flag(&self) -> Rc<AtomicBool> {
        self.screen_sharing_active.clone()
    }

    /// Returns the current video quality tier index (0 = best, 7 = minimal).
    pub fn shared_video_tier_index(&self) -> Rc<AtomicU32> {
        self.shared_video_tier_index.clone()
    }

    /// Returns the current audio quality tier index (0 = high, 3 = emergency).
    pub fn shared_audio_tier_index(&self) -> Rc<AtomicU32> {
        self.shared_audio_tier_index.clone()
    }

    /// Returns the encoder output FPS atomic.
    pub fn shared_encoder_output_fps(&self) -> Rc<AtomicU32> {
        self.current_fps.clone()
    }

    /// Returns the encoder fps_ratio atomic (f32 bits).
    pub fn shared_encoder_fps_ratio(&self) -> Rc<AtomicU32> {
        self.shared_encoder_fps_ratio.clone()
    }

    /// Returns the encoder worst peer FPS atomic (f32 bits).
    pub fn shared_encoder_worst_peer_fps(&self) -> Rc<AtomicU32> {
        self.shared_encoder_worst_peer_fps.clone()
    }

    /// Returns the encoder bitrate_ratio atomic (f32 bits).
    pub fn shared_encoder_bitrate_ratio(&self) -> Rc<AtomicU32> {
        self.shared_encoder_bitrate_ratio.clone()
    }

    /// Returns the encoder target bitrate kbps atomic (f32 bits).
    pub fn shared_encoder_target_bitrate_kbps(&self) -> Rc<AtomicU32> {
        self.shared_encoder_target_bitrate_kbps.clone()
    }

    /// Returns the shared tier transitions buffer for health reporting.
    pub fn shared_tier_transitions(&self) -> Rc<RefCell<Vec<TierTransitionRecord>>> {
        self.shared_tier_transitions.clone()
    }

    /// Returns the shared climb-rate limiter snapshot for health reporting.
    pub fn shared_climb_limiter_snapshot(&self) -> Rc<RefCell<ClimbLimiterSnapshot>> {
        self.shared_climb_limiter_snapshot.clone()
    }

    /// Returns the shared dwell samples buffer for health reporting.
    pub fn shared_dwell_samples(&self) -> Rc<RefCell<Vec<(String, f64)>>> {
        self.shared_dwell_samples.clone()
    }

    /// Returns a shared reference to the re-election completed signal.
    ///
    /// The `ConnectionManager` sets this flag to `true` when a re-election
    /// succeeds. The encoder control loop checks and clears it each tick,
    /// calling `notify_reelection_completed()` on the quality manager.
    pub fn reelection_completed_signal(&self) -> Rc<AtomicBool> {
        self.reelection_completed_signal.clone()
    }

    /// Replace the internal re-election completed signal with an externally-owned one.
    pub fn set_reelection_completed_signal(&mut self, signal: Rc<AtomicBool>) {
        self.reelection_completed_signal = signal;
    }

    /// Returns a shared reference to the force-keyframe flag.
    ///
    /// The `VideoCallClient` stores this and sets it to `true` when a
    /// `KEYFRAME_REQUEST` packet arrives from a remote peer. The encoding
    /// loop checks this flag on every frame and forces a keyframe when set.
    pub fn force_keyframe_flag(&self) -> Arc<AtomicBool> {
        self.force_keyframe.clone()
    }

    /// Request the encoder to produce a keyframe on the next frame.
    pub fn request_keyframe(&self) {
        self.force_keyframe.store(true, Ordering::Release);
        log::info!("CameraEncoder: keyframe requested (PLI)");
    }

    /// Replace the internal force-keyframe flag with an externally-owned one.
    ///
    /// Call this after construction to share the flag with `VideoCallClient`,
    /// which sets it when a remote peer sends a KEYFRAME_REQUEST.
    pub fn set_force_keyframe_flag(&mut self, flag: Arc<AtomicBool>) {
        self.force_keyframe = flag;
    }

    /// Replace the internal congestion step-down flag with an externally-owned one.
    ///
    /// Call this after construction to share the flag with `VideoCallClient`,
    /// which sets it when a server CONGESTION signal is received.
    pub fn set_congestion_step_down_flag(&mut self, flag: Arc<AtomicBool>) {
        self.congestion_step_down = flag;
    }

    // The next three methods delegate to self.state

    /// Enables/disables the encoder.   Returns true if the new value is different from the old value.
    ///
    /// The encoder starts disabled, [`encoder.set_enabled(true)`](Self::set_enabled) must be
    /// called prior to starting encoding.
    ///
    /// Disabling encoding after it has started will cause it to stop.
    pub fn set_enabled(&mut self, value: bool) -> bool {
        self.state.set_enabled(value)
    }

    /// Selects a camera:
    ///
    /// * `device_id` - The value of `entry.device_id` for some entry in
    ///   [`media_device_list.video_inputs.devices()`](crate::MediaDeviceList::video_inputs)
    ///
    /// The encoder starts without a camera associated,
    /// [`encoder.selected(device_id)`](Self::select) must be called prior to starting encoding.
    pub fn select(&mut self, device_id: String) -> bool {
        self.state.select(device_id)
    }

    /// Stops encoding after it has been started.
    pub fn stop(&mut self) {
        self.state.stop()
    }

    /// Start encoding and sending the data to the client connection (if it's currently connected).
    ///
    /// This will not do anything if [`encoder.set_enabled(true)`](Self::set_enabled) has not been
    /// called, or if [`encoder.select(device_id)`](Self::select) has not been called.
    pub fn start(&mut self) {
        // 1. Query the first device with a camera and a mic attached.
        // 2. setup WebCodecs, in particular
        // 3. send encoded video frames and raw audio to the server.
        let client = self.client.clone();
        let userid = client.user_id().clone();
        let aes = client.aes();
        let video_elem_id = self.video_elem_id.clone();
        let EncoderState {
            enabled, switching, ..
        } = self.state.clone();
        let current_bitrate = self.current_bitrate.clone();
        let current_fps = self.current_fps.clone();
        let tier_max_width = self.tier_max_width.clone();
        let tier_max_height = self.tier_max_height.clone();
        let tier_keyframe_interval = self.tier_keyframe_interval.clone();
        let force_keyframe = self.force_keyframe.clone();
        let video_output_handler = {
            let mut buffer: Vec<u8> = Vec::with_capacity(100_000);
            let mut sequence_number = 0;
            let mut last_chunk_time = window().performance().unwrap().now();
            let mut chunks_in_last_second = 0;

            Box::new(move |chunk: JsValue| {
                let now = window().performance().unwrap().now();
                let chunk = web_sys::EncodedVideoChunk::from(chunk);

                // Update FPS calculation
                chunks_in_last_second += 1;
                if now - last_chunk_time >= 1000.0 {
                    let fps = chunks_in_last_second;
                    current_fps.store(fps, Ordering::Relaxed);
                    log::debug!("Encoder output FPS: {fps}");
                    chunks_in_last_second = 0;
                    last_chunk_time = now;
                }

                // Ensure the backing buffer is large enough for this chunk
                let byte_length = chunk.byte_length() as usize;
                if buffer.len() < byte_length {
                    buffer.resize(byte_length, 0);
                }

                let packet: PacketWrapper = transform_video_chunk(
                    chunk,
                    sequence_number,
                    buffer.as_mut_slice(),
                    &userid,
                    aes.clone(),
                );
                client.send_media_packet(packet);
                sequence_number += 1;
            })
        };
        let device_id = if let Some(vid) = &self.state.selected {
            vid.to_string()
        } else {
            return;
        };
        let on_error = self.on_error.clone();

        log::info!(
            "CameraEncoder::start(): using video device_id = {}",
            device_id
        );

        wasm_bindgen_futures::spawn_local(async move {
            let navigator = window().navigator();

            // Wait for <video id="{video_elem_id}"> to be mounted in the DOM
            // Yew renders components asynchronously
            let mut attempt = 0;
            let video_element = loop {
                if let Some(doc) = window().document() {
                    if let Some(elem) = doc.get_element_by_id(&video_elem_id) {
                        if let Ok(video_elem) = elem.dyn_into::<HtmlVideoElement>() {
                            log::info!(
                                "CameraEncoder: found <video id='{}'> after {} attempts",
                                video_elem_id,
                                attempt
                            );
                            break video_elem;
                        }
                    }
                }
                // Sleep a bit and retry
                sleep(Duration::from_millis(50)).await;
                attempt += 1;
                if attempt > 20 {
                    let msg = format!(
                        "Camera error: video element '{}' not found in DOM after 1 second",
                        video_elem_id
                    );
                    error!("{msg}");
                    if let Some(cb) = &on_error {
                        cb.emit(msg);
                    }
                    return;
                }
            };

            let media_devices = match navigator.media_devices() {
                Ok(d) => d,
                Err(e) => {
                    let msg = format!("Failed to access media devices: {e:?}");
                    error!("{msg}");
                    if let Some(cb) = &on_error {
                        cb.emit(msg);
                    }
                    return;
                }
            };
            let constraints = MediaStreamConstraints::new();
            let media_info = web_sys::MediaTrackConstraints::new();

            // Force exact deviceId match (avoids partial/ideal matching surprises).
            if device_id.is_empty() {
                log::warn!("Camera device_id is empty, using default constraint");
                constraints.set_video(&JsValue::TRUE);
            } else {
                let exact = js_sys::Object::new();
                js_sys::Reflect::set(
                    &exact,
                    &JsValue::from_str("exact"),
                    &JsValue::from_str(&device_id),
                )
                .unwrap();

                log::debug!("CameraEncoder: deviceId.exact = {}", device_id);
                media_info.set_device_id(&exact.into());
                constraints.set_video(&media_info.into());
            }

            constraints.set_audio(&Boolean::from(false));

            let devices_query = match media_devices.get_user_media_with_constraints(&constraints) {
                Ok(p) => p,
                Err(e) => {
                    let msg = format!("Camera access failed: {e:?}");
                    error!("{msg}");
                    if let Some(cb) = &on_error {
                        cb.emit(msg);
                    }
                    return;
                }
            };

            let device = match JsFuture::from(devices_query).await {
                Ok(s) => s.unchecked_into::<MediaStream>(),
                Err(e) => {
                    let msg = format!("Failed to get camera stream: {e:?}");
                    error!("{msg}");
                    if let Some(cb) = &on_error {
                        cb.emit(msg);
                    }
                    return;
                }
            };

            log::info!(
                "CameraEncoder: getUserMedia OK, stream id={:?}, tracks={}",
                device.id(),
                device.get_tracks().length()
            );
            // Configure the local preview element
            // Muted must be set before calling play() to avoid autoplay restrictions
            video_element.set_muted(true);
            video_element.set_attribute("playsinline", "true").unwrap();
            video_element.set_src_object(None);
            video_element.set_src_object(Some(&device));

            // play() returns a Promise; await it so Safari's rejection doesn't
            // become an unhandled Promise rejection.  If the first attempt fails
            // (e.g. autoplay policy), retry once after a short delay.
            match video_element.play() {
                Ok(promise) => {
                    if let Err(e) = JsFuture::from(promise).await {
                        log::warn!(
                            "VIDEO PLAY promise rejected on '{}': {:?}  — retrying in 200ms",
                            video_elem_id,
                            e
                        );
                        sleep(Duration::from_millis(200)).await;
                        if let Ok(p2) = video_element.play() {
                            if let Err(e2) = JsFuture::from(p2).await {
                                log::warn!(
                                    "VIDEO PLAY retry also rejected on '{}': {:?}",
                                    video_elem_id,
                                    e2
                                );
                            } else {
                                log::info!("VIDEO PLAY retry succeeded on {}", video_elem_id);
                            }
                        }
                    } else {
                        log::info!(
                            "VIDEO PLAY started successfully on element {}",
                            video_elem_id
                        );
                    }
                }
                Err(e) => {
                    error!("VIDEO PLAY method call failed: {:?}", e);
                }
            }

            let video_track = Box::new(
                device
                    .get_video_tracks()
                    .find(&mut |_: JsValue, _: u32, _: Array| true)
                    .unchecked_into::<VideoTrack>(),
            );

            // Setup video encoder
            let video_error_handler = Closure::wrap(Box::new(move |e: JsValue| {
                error!("error_handler error {e:?}");
            }) as Box<dyn FnMut(JsValue)>);

            let video_output_handler =
                Closure::wrap(video_output_handler as Box<dyn FnMut(JsValue)>);

            let video_encoder_init = VideoEncoderInit::new(
                video_error_handler.as_ref().unchecked_ref(),
                video_output_handler.as_ref().unchecked_ref(),
            );

            let video_encoder = match VideoEncoder::new(&video_encoder_init) {
                Ok(enc) => Box::new(enc),
                Err(e) => {
                    let msg = format!("Failed to create video encoder: {e:?}");
                    error!("{msg}");
                    if let Some(cb) = &on_error {
                        cb.emit(msg);
                    }
                    return;
                }
            };

            // Get track settings to get actual width and height
            let media_track = video_track
                .as_ref()
                .clone()
                .unchecked_into::<MediaStreamTrack>();
            let track_settings = media_track.get_settings();

            let width = track_settings.get_width().expect("width is None");
            let height = track_settings.get_height().expect("height is None");

            let video_encoder_config =
                VideoEncoderConfig::new(get_video_codec_string(), height as u32, width as u32);
            video_encoder_config
                .set_bitrate(current_bitrate.load(Ordering::Relaxed) as f64 * 1000.0);
            video_encoder_config.set_latency_mode(LatencyMode::Realtime);

            if let Err(e) = video_encoder.configure(&video_encoder_config) {
                error!("Error configuring video encoder: {e:?}");
            }

            let video_processor =
                MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(
                    &video_track.clone().unchecked_into::<MediaStreamTrack>(),
                ))
                .unwrap();
            let video_reader = video_processor
                .readable()
                .get_reader()
                .unchecked_into::<ReadableStreamDefaultReader>();

            // Start encoding video and audio.
            let mut video_frame_counter: u32 = 0;
            let mut last_pli_keyframe_time: f64 = 0.0;

            // Cache the initial bitrate
            let mut local_bitrate: u32 = current_bitrate.load(Ordering::Relaxed) * 1000;

            // Track current encoder dimensions for dynamic reconfiguration
            let mut current_encoder_width = width as u32;
            let mut current_encoder_height = height as u32;

            // Cache tier-controlled values
            let mut local_keyframe_interval = tier_keyframe_interval.load(Ordering::Relaxed);
            let mut local_tier_max_width = tier_max_width.load(Ordering::Relaxed);
            let mut local_tier_max_height = tier_max_height.load(Ordering::Relaxed);

            loop {
                if !enabled.load(Ordering::Acquire) || switching.load(Ordering::Acquire) {
                    switching.store(false, Ordering::Release);
                    let video_track = video_track.clone().unchecked_into::<MediaStreamTrack>();
                    video_track.stop();
                    log::info!("CameraEncoder: stopped");
                    if let Err(e) = video_encoder.close() {
                        error!("Error closing video encoder: {e:?}");
                    }
                    return;
                }

                // Check for tier-driven dimension changes (adaptive quality).
                // When the tier changes max_width/max_height, we reconfigure
                // the encoder to downscale (WebCodecs handles the scaling).
                let new_tier_w = tier_max_width.load(Ordering::Relaxed);
                let new_tier_h = tier_max_height.load(Ordering::Relaxed);
                let new_kf = tier_keyframe_interval.load(Ordering::Relaxed);

                let tier_dims_changed =
                    new_tier_w != local_tier_max_width || new_tier_h != local_tier_max_height;
                if tier_dims_changed {
                    local_tier_max_width = new_tier_w;
                    local_tier_max_height = new_tier_h;

                    // Constrain current encoder dimensions to the tier max.
                    let constrained_w = current_encoder_width.min(local_tier_max_width);
                    let constrained_h = current_encoder_height.min(local_tier_max_height);

                    log::info!(
                        "CameraEncoder: tier dimension change -> {}x{} (was {}x{})",
                        constrained_w,
                        constrained_h,
                        current_encoder_width,
                        current_encoder_height,
                    );
                    current_encoder_width = constrained_w;
                    current_encoder_height = constrained_h;

                    let new_config = VideoEncoderConfig::new(
                        get_video_codec_string(),
                        current_encoder_height,
                        current_encoder_width,
                    );
                    new_config.set_bitrate(local_bitrate as f64);
                    new_config.set_latency_mode(LatencyMode::Realtime);
                    if let Err(e) = video_encoder.configure(&new_config) {
                        error!("Error reconfiguring camera encoder for tier change: {e:?}");
                    }
                }

                if new_kf != local_keyframe_interval {
                    local_keyframe_interval = new_kf;
                    log::info!(
                        "CameraEncoder: keyframe interval changed to {}",
                        local_keyframe_interval
                    );
                }

                // Update the bitrate if it has changed more than the threshold percentage
                let new_current_bitrate = current_bitrate.load(Ordering::Relaxed) * 1000;
                if new_current_bitrate != local_bitrate && !tier_dims_changed {
                    log::info!("Updating video bitrate to {new_current_bitrate}");
                    local_bitrate = new_current_bitrate;
                    video_encoder_config.set_bitrate(local_bitrate as f64);
                    if let Err(e) = video_encoder.configure(&video_encoder_config) {
                        error!("Error configuring video encoder: {e:?}");
                    }
                } else if new_current_bitrate != local_bitrate {
                    // Bitrate also changed alongside tier dims -- already applied above.
                    local_bitrate = new_current_bitrate;
                }

                match JsFuture::from(video_reader.read()).await {
                    Ok(js_frame) => {
                        let video_frame = Reflect::get(&js_frame, &JsString::from("value"))
                            .unwrap()
                            .unchecked_into::<VideoFrame>();

                        // Check for dimension changes (rotation, camera switch).
                        // Also constrain to current tier max dimensions.
                        let frame_width = video_frame.display_width();
                        let frame_height = video_frame.display_height();
                        let clamped_width = if frame_width > 0 {
                            frame_width.min(local_tier_max_width)
                        } else {
                            frame_width
                        };
                        let clamped_height = if frame_height > 0 {
                            frame_height.min(local_tier_max_height)
                        } else {
                            frame_height
                        };

                        if clamped_width > 0
                            && clamped_height > 0
                            && (clamped_width != current_encoder_width
                                || clamped_height != current_encoder_height)
                        {
                            log::info!("Camera dimensions changed from {current_encoder_width}x{current_encoder_height} to {clamped_width}x{clamped_height}, reconfiguring encoder");

                            current_encoder_width = clamped_width;
                            current_encoder_height = clamped_height;

                            let new_config = VideoEncoderConfig::new(
                                get_video_codec_string(),
                                current_encoder_height,
                                current_encoder_width,
                            );
                            new_config.set_bitrate(local_bitrate as f64);
                            new_config.set_latency_mode(LatencyMode::Realtime);
                            if let Err(e) = video_encoder.configure(&new_config) {
                                error!(
                                    "Error reconfiguring camera encoder with new dimensions: {e:?}"
                                );
                            }
                        }

                        let video_encoder_encode_options = VideoEncoderEncodeOptions::new();
                        // Check if a keyframe was requested via PLI (Picture Loss Indication).
                        // The flag is cleared after producing the keyframe.
                        let pli_requested = force_keyframe.swap(false, Ordering::AcqRel);
                        let now = window()
                            .performance()
                            .expect("Performance API not available")
                            .now();
                        let pli_cooldown_ok =
                            (now - last_pli_keyframe_time) >= ENCODER_PLI_COOLDOWN_MS;
                        let force_pli = pli_requested && pli_cooldown_ok;
                        if force_pli {
                            last_pli_keyframe_time = now;
                        }
                        // Use tier-controlled keyframe interval instead of the
                        // static constant, allowing adaptive quality to adjust it.
                        // Using `%` instead of `.is_multiple_of()` for compatibility
                        // with Rust toolchains older than 1.87.
                        #[allow(clippy::manual_is_multiple_of)]
                        let is_periodic_keyframe = local_keyframe_interval > 0
                            && video_frame_counter % local_keyframe_interval == 0;
                        video_encoder_encode_options
                            .set_key_frame(is_periodic_keyframe || force_pli);
                        if force_pli {
                            log::info!(
                                "CameraEncoder: forcing keyframe at frame {} (PLI)",
                                video_frame_counter
                            );
                        } else if pli_requested {
                            log::info!(
                                "CameraEncoder: PLI keyframe suppressed at frame {} (cooldown: {:.0}ms since last)",
                                video_frame_counter,
                                now - last_pli_keyframe_time,
                            );
                        }
                        if let Err(e) = video_encoder
                            .encode_with_options(&video_frame, &video_encoder_encode_options)
                        {
                            error!("Error encoding video frame: {e:?}");
                        }
                        video_frame.close();
                        video_frame_counter += 1;
                    }
                    Err(e) => {
                        error!("error {e:?}");
                    }
                }
            }
        });
    }
}

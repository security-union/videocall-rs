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
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

// ── Encoder error observability counters (cumulative, since page load) ───────
// These use the same global-static pattern as `keyframe_requests_sent_count` in
// peer_decode_manager.rs: global AtomicU64 + public getter. The health reporter
// reads these each tick and includes them in the protobuf health packet so
// Prometheus/Grafana can derive per-second rates via `rate()`.

static CAMERA_ENCODER_ERRORS_CLOSED_CODEC: AtomicU64 = AtomicU64::new(0);
static CAMERA_ENCODER_ERRORS_VPX_MEM_ALLOC: AtomicU64 = AtomicU64::new(0);
static CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL: AtomicU64 = AtomicU64::new(0);
static CAMERA_ENCODER_ERRORS_GENERIC: AtomicU64 = AtomicU64::new(0);
static CAMERA_ENCODER_FRAMES_SUBMITTED_OK: AtomicU64 = AtomicU64::new(0);

pub fn camera_encoder_errors_closed_codec() -> u64 {
    CAMERA_ENCODER_ERRORS_CLOSED_CODEC.load(Ordering::Relaxed)
}
pub fn camera_encoder_errors_vpx_mem_alloc() -> u64 {
    CAMERA_ENCODER_ERRORS_VPX_MEM_ALLOC.load(Ordering::Relaxed)
}
pub fn camera_encoder_errors_configure_fatal() -> u64 {
    CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL.load(Ordering::Relaxed)
}
pub fn camera_encoder_errors_generic() -> u64 {
    CAMERA_ENCODER_ERRORS_GENERIC.load(Ordering::Relaxed)
}
pub fn camera_encoder_frames_submitted_ok() -> u64 {
    CAMERA_ENCODER_FRAMES_SUBMITTED_OK.load(Ordering::Relaxed)
}
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::CodecState;
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
use super::classify_encode_error::{classify_encode_error, EncodeErrorBucket};
use super::encoder_state::EncoderState;
use super::transform::transform_video_chunk;

use crate::adaptive_quality_constants::{
    AUDIO_QUALITY_TIERS, BITRATE_CHANGE_THRESHOLD, VIDEO_QUALITY_TIERS,
};
use crate::constants::get_video_codec_string;
use crate::diagnostics::EncoderBitrateController;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;

fn is_fatal_encoder_error_message(msg: &str) -> bool {
    msg.contains("closed codec")
        || msg.contains("InvalidStateError")
        || msg.contains("Memory allocation error")
        || msg.contains("Unable to find free frame buffer")
}

fn is_fatal_encoder_error(err: &JsValue) -> bool {
    let msg = format!("{err:?}");
    is_fatal_encoder_error_message(&msg)
}

fn stop_media_stream_tracks(stream: &MediaStream) {
    if let Some(tracks) = stream.get_tracks().dyn_ref::<Array>() {
        for i in 0..tracks.length() {
            if let Ok(track) = tracks.get(i).dyn_into::<MediaStreamTrack>() {
                track.stop();
            }
        }
    }
}

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
        wasm_bindgen_futures::spawn_local(async move {
            let mut encoder_control = EncoderBitrateController::new(
                current_bitrate.load(Ordering::Relaxed),
                current_fps.clone(),
            );
            while let Some(event) = diagnostics_receiver.next().await {
                // Check for server congestion step-down request before
                // processing the diagnostics packet so the forced step-down
                // takes effect immediately.
                if congestion_flag.swap(false, Ordering::AcqRel) {
                    log::warn!(
                        "CameraEncoder: server CONGESTION signal received, forcing video step-down"
                    );
                    encoder_control.force_video_step_down();
                }

                let output_wasted = encoder_control.process_diagnostics_packet(event);
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

            // Sequence number persists across restarts so the receiving side
            // never sees duplicate or regressed sequence numbers.
            let mut sequence_number: u64 = 0;

            let mut restart_count: u32 = 0;
            const MAX_RESTARTS: u32 = 5;

            'restart: loop {
                // Backoff + max-restart guard (skip on first iteration).
                if restart_count > 0 {
                    let delay_ms = 500u64.saturating_mul(restart_count.min(4) as u64);
                    log::warn!(
                        "CameraEncoder: restarting (attempt {}/{}), backoff {}ms",
                        restart_count,
                        MAX_RESTARTS,
                        delay_ms,
                    );
                    sleep(Duration::from_millis(delay_ms)).await;
                    if restart_count >= MAX_RESTARTS {
                        error!("CameraEncoder: max restarts ({MAX_RESTARTS}) reached, giving up");
                        if let Some(cb) = &on_error {
                            cb.emit("Camera encoder failed after repeated restarts".into());
                        }
                        return;
                    }
                }

                // --- getUserMedia ---

                let media_devices = match navigator.media_devices() {
                    Ok(d) => d,
                    Err(e) => {
                        let msg = format!("Failed to access media devices: {e:?}");
                        error!("{msg}");
                        if let Some(cb) = &on_error {
                            cb.emit(msg);
                        }
                        restart_count += 1;
                        continue 'restart;
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

                let devices_query =
                    match media_devices.get_user_media_with_constraints(&constraints) {
                        Ok(p) => p,
                        Err(e) => {
                            let msg = format!("Camera access failed: {e:?}");
                            error!("{msg}");
                            if let Some(cb) = &on_error {
                                cb.emit(msg);
                            }
                            restart_count += 1;
                            continue 'restart;
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
                        restart_count += 1;
                        continue 'restart;
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

                // --- Setup video encoder ---
                // The output handler and error handler closures must be re-created
                // on each restart because Closure::wrap consumes them and the new
                // VideoEncoder needs fresh JS function references.

                let video_output_handler = {
                    let client = client.clone();
                    let userid = userid.clone();
                    let aes = aes.clone();
                    let current_fps = current_fps.clone();
                    let mut buffer: Vec<u8> = Vec::with_capacity(100_000);
                    // Capture the current sequence_number by value; we will read
                    // the updated value back after the encode loop exits.
                    let mut local_seq = sequence_number;
                    let seq_out = Rc::new(std::cell::Cell::new(sequence_number));
                    let seq_out_inner = seq_out.clone();
                    let mut last_chunk_time = window().performance().unwrap().now();
                    let mut chunks_in_last_second = 0;

                    (
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
                                local_seq,
                                buffer.as_mut_slice(),
                                &userid,
                                aes.clone(),
                            );
                            client.send_media_packet(packet);
                            local_seq += 1;
                            seq_out_inner.set(local_seq);
                        }) as Box<dyn FnMut(JsValue)>,
                        seq_out,
                    )
                };

                let (video_output_box, seq_out_cell) = video_output_handler;

                let video_error_handler = Closure::wrap(Box::new(move |e: JsValue| {
                    error!("error_handler error {e:?}");
                })
                    as Box<dyn FnMut(JsValue)>);

                let video_output_closure = Closure::wrap(video_output_box);

                let video_encoder_init = VideoEncoderInit::new(
                    video_error_handler.as_ref().unchecked_ref(),
                    video_output_closure.as_ref().unchecked_ref(),
                );

                let video_encoder = match VideoEncoder::new(&video_encoder_init) {
                    Ok(enc) => Box::new(enc),
                    Err(e) => {
                        let msg = format!("Failed to create video encoder: {e:?}");
                        error!("{msg}");
                        stop_media_stream_tracks(&device);
                        if let Some(cb) = &on_error {
                            cb.emit(msg);
                        }
                        restart_count += 1;
                        continue 'restart;
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
                    CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL.fetch_add(1, Ordering::Relaxed);
                    if is_fatal_encoder_error(&e) {
                        error!("CameraEncoder: fatal configure error before encode loop, restarting: {e:?}");
                        let _ = video_encoder.close();
                        stop_media_stream_tracks(&device);
                        restart_count += 1;
                        continue 'restart;
                    }
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

                // Cache the initial bitrate
                let mut local_bitrate: u32 = current_bitrate.load(Ordering::Relaxed) * 1000;

                // Track current encoder dimensions for dynamic reconfiguration
                let mut current_encoder_width = width as u32;
                let mut current_encoder_height = height as u32;

                // Cache tier-controlled values
                let mut local_keyframe_interval = tier_keyframe_interval.load(Ordering::Relaxed);
                let mut local_tier_max_width = tier_max_width.load(Ordering::Relaxed);
                let mut local_tier_max_height = tier_max_height.load(Ordering::Relaxed);

                // Track whether we have successfully encoded at least one frame
                // in this restart cycle. Used to reset restart_count on success.
                let mut encoded_ok_this_cycle = false;

                'encode: loop {
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

                    // --- Guard: check if the encoder has been closed externally ---
                    // This can happen if the browser closes the codec (e.g. due to
                    // GPU process crash, OOM, or an error callback we didn't intercept).
                    if video_encoder.state() == CodecState::Closed {
                        log::warn!("CameraEncoder: encoder state is Closed, triggering restart");
                        restart_count += 1;
                        break 'encode;
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
                        // Guard: do not configure a closed encoder.
                        if video_encoder.state() == CodecState::Closed {
                            log::warn!("CameraEncoder: encoder closed before tier reconfigure");
                            restart_count += 1;
                            break 'encode;
                        }

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
                            CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL.fetch_add(1, Ordering::Relaxed);
                            if is_fatal_encoder_error(&e) {
                                error!("CameraEncoder: fatal configure error, restarting: {e:?}");
                                restart_count += 1;
                                break 'encode;
                            }
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
                        // Guard: do not configure a closed encoder.
                        if video_encoder.state() == CodecState::Closed {
                            log::warn!("CameraEncoder: encoder closed before bitrate reconfigure");
                            restart_count += 1;
                            break 'encode;
                        }
                        log::info!("Updating video bitrate to {new_current_bitrate}");
                        local_bitrate = new_current_bitrate;
                        video_encoder_config.set_bitrate(local_bitrate as f64);
                        if let Err(e) = video_encoder.configure(&video_encoder_config) {
                            CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL.fetch_add(1, Ordering::Relaxed);
                            if is_fatal_encoder_error(&e) {
                                error!("CameraEncoder: fatal configure error, restarting: {e:?}");
                                restart_count += 1;
                                break 'encode;
                            }
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
                                // Guard: do not configure a closed encoder.
                                if video_encoder.state() == CodecState::Closed {
                                    log::warn!("CameraEncoder: encoder closed before dimension reconfigure");
                                    video_frame.close();
                                    restart_count += 1;
                                    break 'encode;
                                }

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
                                    CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL
                                        .fetch_add(1, Ordering::Relaxed);
                                    if is_fatal_encoder_error(&e) {
                                        error!("CameraEncoder: fatal configure error, restarting: {e:?}");
                                        restart_count += 1;
                                        break 'encode;
                                    }
                                    error!(
                                        "Error reconfiguring camera encoder with new dimensions: {e:?}"
                                    );
                                }
                            }

                            let video_encoder_encode_options = VideoEncoderEncodeOptions::new();
                            // Check if a keyframe was requested via PLI (Picture Loss Indication).
                            // The flag is cleared after producing the keyframe.
                            let pli_requested = force_keyframe.swap(false, Ordering::AcqRel);
                            // Use tier-controlled keyframe interval instead of the
                            // static constant, allowing adaptive quality to adjust it.
                            // Using `%` instead of `.is_multiple_of()` for compatibility
                            // with Rust toolchains older than 1.87.
                            #[allow(clippy::manual_is_multiple_of)]
                            let is_periodic_keyframe = local_keyframe_interval > 0
                                && video_frame_counter % local_keyframe_interval == 0;
                            video_encoder_encode_options
                                .set_key_frame(is_periodic_keyframe || pli_requested);
                            if pli_requested {
                                log::info!(
                                    "CameraEncoder: forcing keyframe at frame {} (PLI)",
                                    video_frame_counter
                                );
                            }
                            match video_encoder
                                .encode_with_options(&video_frame, &video_encoder_encode_options)
                            {
                                Ok(_) => {
                                    CAMERA_ENCODER_FRAMES_SUBMITTED_OK
                                        .fetch_add(1, Ordering::Relaxed);
                                    // First successful encode after a restart resets the
                                    // restart counter so transient errors don't accumulate
                                    // toward MAX_RESTARTS across long-lived sessions.
                                    if !encoded_ok_this_cycle && restart_count > 0 {
                                        log::info!(
                                            "CameraEncoder: encode succeeded after restart, resetting restart counter"
                                        );
                                        restart_count = 0;
                                    }
                                    encoded_ok_this_cycle = true;
                                }
                                Err(e) => {
                                    let msg = format!("{e:?}");
                                    match classify_encode_error(&msg) {
                                        EncodeErrorBucket::ClosedCodec => {
                                            CAMERA_ENCODER_ERRORS_CLOSED_CODEC
                                                .fetch_add(1, Ordering::Relaxed);
                                        }
                                        EncodeErrorBucket::VpxMemAlloc => {
                                            CAMERA_ENCODER_ERRORS_VPX_MEM_ALLOC
                                                .fetch_add(1, Ordering::Relaxed);
                                        }
                                        EncodeErrorBucket::Generic => {
                                            CAMERA_ENCODER_ERRORS_GENERIC
                                                .fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                    if is_fatal_encoder_error(&e) {
                                        error!(
                                            "CameraEncoder: fatal encode error (restart {restart_count}): {e:?}"
                                        );
                                        video_frame.close();
                                        restart_count += 1;
                                        break 'encode;
                                    }
                                    error!("Error encoding video frame: {e:?}");
                                }
                            }
                            video_frame.close();
                            video_frame_counter += 1;
                        }
                        Err(e) => {
                            error!("error {e:?}");
                        }
                    }
                } // end 'encode

                // --- Cleanup before restart ---
                // Persist the sequence number from the output handler so the next
                // restart cycle continues numbering where we left off.
                sequence_number = seq_out_cell.get();

                // Close the encoder (may already be closed; ignore errors).
                let _ = video_encoder.close();

                // Stop the media track to release the camera.
                let vt = video_track.clone().unchecked_into::<MediaStreamTrack>();
                vt.stop();

                log::info!("CameraEncoder: cleaned up encoder and track, looping to restart");
                // Loop back to 'restart for backoff + re-acquisition.
            } // end 'restart
        });
    }
}

#[cfg(test)]
mod tests {
    use super::is_fatal_encoder_error_message;

    #[test]
    fn fatal_encoder_errors_match_closed_codec_signatures() {
        assert!(is_fatal_encoder_error_message(
            "InvalidStateError: closed codec"
        ));
        assert!(is_fatal_encoder_error_message(
            "Memory allocation error (Unable to find free frame buffer)"
        ));
    }

    #[test]
    fn non_fatal_encoder_errors_do_not_trigger_restart() {
        assert!(!is_fatal_encoder_error_message(
            "EncodingError: dropped one frame"
        ));
    }
}

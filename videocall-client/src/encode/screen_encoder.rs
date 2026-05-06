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

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use gloo_timers::future::sleep;
use gloo_utils::window;
use js_sys::Array;
use js_sys::JsString;
use js_sys::Reflect;
use log::error;
use log::info;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::CodecState;
use web_sys::LatencyMode;
use web_sys::MediaStream;
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
use super::classify_encode_error::classify_encode_error;
use super::classify_encode_error::EncodeErrorBucket;
use super::encoder_state::EncoderState;
use super::transform::transform_screen_chunk;
use crate::crypto::aes::Aes128State;

use crate::adaptive_quality_constants::{
    BITRATE_CHANGE_THRESHOLD, DEFAULT_SCREEN_TIER_INDEX, ENCODER_PLI_COOLDOWN_MS,
    SCREEN_QUALITY_TIERS,
};
use crate::constants::get_video_codec_string;
use crate::diagnostics::adaptive_quality_manager::TierTransitionRecord;
use crate::diagnostics::EncoderBitrateController;

// ── Screen encoder error observability counters (cumulative, since page load) ─
// Mirrors the camera encoder pattern. See camera_encoder.rs for design rationale.

static SCREEN_ENCODER_ERRORS_CLOSED_CODEC: AtomicU64 = AtomicU64::new(0);
static SCREEN_ENCODER_ERRORS_VPX_MEM_ALLOC: AtomicU64 = AtomicU64::new(0);
static SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL: AtomicU64 = AtomicU64::new(0);
static SCREEN_ENCODER_ERRORS_GENERIC: AtomicU64 = AtomicU64::new(0);
static SCREEN_ENCODER_FRAMES_SUBMITTED_OK: AtomicU64 = AtomicU64::new(0);

pub fn screen_encoder_errors_closed_codec() -> u64 {
    SCREEN_ENCODER_ERRORS_CLOSED_CODEC.load(Ordering::Relaxed)
}
pub fn screen_encoder_errors_vpx_mem_alloc() -> u64 {
    SCREEN_ENCODER_ERRORS_VPX_MEM_ALLOC.load(Ordering::Relaxed)
}
pub fn screen_encoder_errors_configure_fatal() -> u64 {
    SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL.load(Ordering::Relaxed)
}
pub fn screen_encoder_errors_generic() -> u64 {
    SCREEN_ENCODER_ERRORS_GENERIC.load(Ordering::Relaxed)
}
pub fn screen_encoder_frames_submitted_ok() -> u64 {
    SCREEN_ENCODER_FRAMES_SUBMITTED_OK.load(Ordering::Relaxed)
}

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

fn should_reacquire_screen_capture(media_acquired: bool, restart_count: u32) -> bool {
    !media_acquired || restart_count > 0
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

/// Sets `bitrateMode = "variable"` on a [`VideoEncoderConfig`].
///
/// Variable bitrate lets the encoder burst above the target during high-motion
/// events (scrolling, window switching) and stay below it when content is
/// static, keeping text readable without rate-starving the encoder.
fn set_vbr_mode(config: &VideoEncoderConfig) {
    let _ = Reflect::set(
        config,
        &JsValue::from_str("bitrateMode"),
        &JsValue::from_str("variable"),
    );
}

/// Events emitted by [ScreenEncoder] to notify about screen share state changes.
///
/// This allows the UI to react to screen share lifecycle events without managing
/// the MediaStream directly.
#[derive(Clone, Debug)]
pub enum ScreenShareEvent {
    /// Screen share successfully started and encoding is active, carrying the MediaStream
    Started(MediaStream),
    /// User cancelled the browser picker dialog (no error dialog shown)
    Cancelled,
    /// Screen share ended normally (user clicked browser's "Stop sharing" or stream ended)
    Stopped,
    /// Screen share failed due to an error (shows error dialog)
    Failed(String),
}

/// [ScreenEncoder] encodes the user's screen and sends it through a [`VideoCallClient`](crate::VideoCallClient) connection.
///
/// See also:
/// * [CameraEncoder](crate::CameraEncoder)
/// * [MicrophoneEncoder](crate::MicrophoneEncoder)
///
pub struct ScreenEncoder {
    client: VideoCallClient,
    state: EncoderState,
    current_bitrate: Rc<AtomicU32>,
    current_fps: Rc<AtomicU32>,
    on_encoder_settings_update: Option<Callback<String>>,
    on_state_change: Option<Callback<ScreenShareEvent>>,
    /// Holds the active MediaStream so `stop()` can synchronously kill all tracks.
    /// Only used by the screen encoder -- this is screen-specific state, not generic encoder state.
    /// I do not like this but so far it is reliable.
    screen_stream: Rc<RefCell<Option<MediaStream>>>,
    /// Tier-controlled max width for screen share.
    tier_max_width: Rc<AtomicU32>,
    /// Tier-controlled max height for screen share.
    tier_max_height: Rc<AtomicU32>,
    /// Tier-controlled keyframe interval (frames).
    tier_keyframe_interval: Rc<AtomicU32>,
    /// When set to `true`, the next encoded frame will be forced as a keyframe.
    /// Used by the PLI (Picture Loss Indication) mechanism.
    force_keyframe: Arc<AtomicBool>,
    /// Holds the *original* video track returned by getDisplayMedia so that `stop()` can call
    /// `.stop()` on it directly.  The browser's native screen-share indicator bar (the
    /// "You are sharing" bar with "Stop sharing" / "Hide") is only dismissed when the
    /// original capture track is stopped; stopping a cloned track (e.g. from
    /// `MediaStream::clone()`) does **not** affect the indicator.
    active_video_track: Rc<RefCell<Option<MediaStreamTrack>>>,
    /// Shared flag for cross-stream bandwidth coordination. Set to `true` when
    /// screen capture starts, `false` when it stops. The `CameraEncoder` reads
    /// this to drop its quality tier and prevent bandwidth contention.
    screen_sharing_active: Option<Rc<AtomicBool>>,
    /// Current screen share quality tier index (0=high, 1=medium, 2=low).
    shared_screen_tier_index: Rc<AtomicU32>,
    /// Tier transition events buffer, drained by health reporter.
    shared_tier_transitions: Rc<RefCell<Vec<TierTransitionRecord>>>,
}

impl ScreenEncoder {
    /// Construct a screen encoder:
    ///
    /// * `client` - an instance of a [`VideoCallClient`](crate::VideoCallClient).  It does not need to be currently connected.
    /// * `bitrate_kbps` - initial bitrate in kilobits per second
    /// * `on_encoder_settings_update` - callback for encoder settings updates (e.g., bitrate changes)
    /// * `on_state_change` - callback for screen share state changes (started, cancelled, stopped)
    ///
    /// The encoder is created in a disabled state, [`encoder.set_enabled(true)`](Self::set_enabled) must be called before it can start encoding.
    pub fn new(
        client: VideoCallClient,
        bitrate_kbps: u32,
        on_encoder_settings_update: Callback<String>,
        on_state_change: Callback<ScreenShareEvent>,
    ) -> Self {
        let default_tier = &SCREEN_QUALITY_TIERS[DEFAULT_SCREEN_TIER_INDEX];
        Self {
            client,
            state: EncoderState::new(),
            current_bitrate: Rc::new(AtomicU32::new(bitrate_kbps)),
            current_fps: Rc::new(AtomicU32::new(0)),
            on_encoder_settings_update: Some(on_encoder_settings_update),
            on_state_change: Some(on_state_change),
            screen_stream: Rc::new(RefCell::new(None)),
            tier_max_width: Rc::new(AtomicU32::new(default_tier.max_width)),
            tier_max_height: Rc::new(AtomicU32::new(default_tier.max_height)),
            tier_keyframe_interval: Rc::new(AtomicU32::new(default_tier.keyframe_interval_frames)),
            force_keyframe: Arc::new(AtomicBool::new(false)),
            active_video_track: Rc::new(RefCell::new(None)),
            screen_sharing_active: None,
            shared_screen_tier_index: Rc::new(AtomicU32::new(DEFAULT_SCREEN_TIER_INDEX as u32)),
            shared_tier_transitions: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Set the shared screen-sharing-active flag for cross-stream coordination.
    ///
    /// This flag is read by the `CameraEncoder` to drop its quality tier when
    /// screen share is active, preventing bandwidth contention.
    pub fn set_screen_sharing_flag(&mut self, flag: Rc<AtomicBool>) {
        self.screen_sharing_active = Some(flag);
    }

    /// Returns the current screen share quality tier index (0=high, 1=medium, 2=low).
    pub fn shared_screen_tier_index(&self) -> Rc<AtomicU32> {
        self.shared_screen_tier_index.clone()
    }

    /// Returns the shared tier transitions buffer for health reporting.
    pub fn shared_tier_transitions(&self) -> Rc<RefCell<Vec<TierTransitionRecord>>> {
        self.shared_tier_transitions.clone()
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
        let shared_screen_tier_idx = self.shared_screen_tier_index.clone();
        let shared_tier_transitions = self.shared_tier_transitions.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let mut encoder_control =
                EncoderBitrateController::new_for_screen(current_fps.clone(), SCREEN_QUALITY_TIERS);
            while let Some(event) = diagnostics_receiver.next().await {
                let output_wasted = encoder_control.process_diagnostics_packet(event);
                if let Some(bitrate) = output_wasted {
                    if enabled.load(Ordering::Acquire) {
                        // Only update if change is greater than threshold
                        let current = current_bitrate.load(Ordering::Relaxed) as f64;
                        let new = bitrate;
                        let percent_change = (new - current).abs() / current;

                        if percent_change > BITRATE_CHANGE_THRESHOLD {
                            if let Some(callback) = &on_encoder_settings_update {
                                callback.emit(format!("Bitrate: {bitrate:.2} kbps"));
                            }
                            current_bitrate.store(bitrate as u32, Ordering::Relaxed);
                        }
                    } else if let Some(callback) = &on_encoder_settings_update {
                        callback.emit("Disabled".to_string());
                    }
                }

                // Check for tier changes and update shared atomics.
                if encoder_control.take_tier_changed() {
                    let tier = encoder_control.current_video_tier();
                    tier_max_width.store(tier.max_width, Ordering::Relaxed);
                    tier_max_height.store(tier.max_height, Ordering::Relaxed);
                    tier_keyframe_interval.store(tier.keyframe_interval_frames, Ordering::Relaxed);
                    shared_screen_tier_idx
                        .store(encoder_control.video_tier_index() as u32, Ordering::Relaxed);
                    log::info!(
                        "ScreenEncoder: tier changed to '{}' ({}x{}, {}fps, kf={})",
                        tier.label,
                        tier.max_width,
                        tier.max_height,
                        tier.target_fps,
                        tier.keyframe_interval_frames,
                    );
                }

                // Drain tier transitions, overriding stream to "screen".
                let mut transitions = encoder_control.drain_tier_transitions();
                for t in &mut transitions {
                    t.stream = "screen";
                }
                if !transitions.is_empty() {
                    shared_tier_transitions.borrow_mut().extend(transitions);
                }
            }
        });
    }

    /// Returns a handle to the active screen-share MediaStream.
    /// The inner Option is None when no screen is being shared.
    pub fn screen_stream(&self) -> Rc<RefCell<Option<MediaStream>>> {
        self.screen_stream.clone()
    }

    /// Gets the current encoder output frame rate
    pub fn get_current_fps(&self) -> u32 {
        self.current_fps.load(Ordering::Relaxed)
    }

    /// Returns a shared reference to the force-keyframe flag.
    ///
    /// The `VideoCallClient` stores this and sets it to `true` when a
    /// `KEYFRAME_REQUEST` packet arrives from a remote peer.
    pub fn force_keyframe_flag(&self) -> Arc<AtomicBool> {
        self.force_keyframe.clone()
    }

    /// Request the encoder to produce a keyframe on the next frame.
    pub fn request_keyframe(&self) {
        self.force_keyframe.store(true, Ordering::Release);
        log::info!("ScreenEncoder: keyframe requested (PLI)");
    }

    /// Replace the internal force-keyframe flag with an externally-owned one.
    ///
    /// Call this after construction to share the flag with `VideoCallClient`,
    /// which sets it when a remote peer sends a KEYFRAME_REQUEST.
    pub fn set_force_keyframe_flag(&mut self, flag: Arc<AtomicBool>) {
        self.force_keyframe = flag;
    }

    /// Allows setting a callback to receive encoder settings updates
    pub fn set_encoder_settings_callback(&mut self, callback: Callback<String>) {
        self.on_encoder_settings_update = Some(callback);
    }

    // The next two methods delegate to self.state

    /// Enables/disables the encoder.   Returns true if the new value is different from the old value.
    ///
    /// The encoder starts disabled, [`encoder.set_enabled(true)`](Self::set_enabled) must be
    /// called prior to starting encoding.
    ///
    /// Disabling encoding after it has started will cause it to stop.
    pub fn set_enabled(&mut self, value: bool) -> bool {
        self.state.set_enabled(value)
    }

    /// Stops encoding and MediaStream after it has been started.
    ///
    /// This is the authoritative cleanup path when the UI triggers a stop.
    /// It sets the encoder flags, notifies the client at the protocol level,
    /// and synchronously stops all media tracks.
    pub fn stop(&mut self) {
        // Clear screen-sharing flag so the camera encoder removes its quality ceiling.
        if let Some(ref flag) = self.screen_sharing_active {
            flag.store(false, Ordering::Release);
        }

        // Signal the encoding loop to exit
        self.state.stop();

        // Notify the client that screen sharing is disabled at the protocol level.
        // This must happen here because self.state.stop() sets enabled=false,
        // which causes the encoding loop's end-of-loop cleanup to skip its own
        // set_screen_enabled(false) call (the enabled.swap guard returns false).
        self.client.set_screen_enabled(false);

        // Stop the *original* capture track synchronously so the browser dismisses
        // its native screen-share indicator bar ("Stop sharing" / "Hide") immediately.
        // The stream stored in `screen_stream` is a *clone* of the original stream;
        // its tracks are clones of the original tracks.  Stopping cloned tracks does
        // NOT stop the underlying capture source — the indicator only goes away when
        // the original track is stopped.  The encoding loop also calls
        // `media_track.stop()` during cleanup, but that only happens after the next
        // async read() resolves, which can be one frame-period later (or longer when
        // the shared window is idle).  Stopping here is immediate.
        if let Some(track) = self.active_video_track.borrow_mut().take() {
            log::info!("stop: stopping original capture track to dismiss browser indicator");
            track.stop();
        }

        // Synchronously stop all tracks from the stored (cloned) stream.
        // SAFETY: In WASM's single-threaded environment this lock can never be contended.
        let stream = self.screen_stream.borrow_mut().take();
        log::info!("stop share media stream");
        if let Some(stream) = stream {
            for i in 0..stream.get_tracks().length() {
                let track = stream
                    .get_tracks()
                    .get(i)
                    .unchecked_into::<web_sys::MediaStreamTrack>();
                track.stop();
            }
            // Emit Stopped so the UI layer can clean up (e.g., detach preview srcObject).
            // The encoding loop's end-of-loop cleanup will skip its own Stopped emission
            // because enabled.swap(false) returns false (state.stop() already cleared it).
            // The onended handler may also fire in browsers that dispatch "ended" on
            // programmatic stop() calls (e.g., Chrome); duplicate Stopped events are
            // harmless — the UI handlers are idempotent.
            if let Some(ref callback) = self.on_state_change {
                callback.emit(ScreenShareEvent::Stopped);
            }
        }
    }

    /// Apply the initial quality tier to shared atomics before starting the
    /// encoding loop.  Called by both [`start`](Self::start) and
    /// [`start_with_stream`](Self::start_with_stream).
    fn apply_initial_tier(&mut self, initial_tier: usize) {
        let clamped_tier = initial_tier.min(SCREEN_QUALITY_TIERS.len().saturating_sub(1));
        if clamped_tier != initial_tier {
            log::warn!(
                "ScreenEncoder: initial_tier {} out of bounds, clamped to {}",
                initial_tier,
                clamped_tier
            );
        }

        let tier = &SCREEN_QUALITY_TIERS[clamped_tier];
        self.shared_screen_tier_index
            .store(clamped_tier as u32, Ordering::Relaxed);
        self.tier_max_width.store(tier.max_width, Ordering::Relaxed);
        self.tier_max_height
            .store(tier.max_height, Ordering::Relaxed);
        self.tier_keyframe_interval
            .store(tier.keyframe_interval_frames, Ordering::Relaxed);
        self.current_bitrate
            .store(tier.ideal_bitrate_kbps, Ordering::Relaxed);

        log::info!(
            "ScreenEncoder: initial tier {} '{}' ({}x{}, {}fps, kf={}, bitrate={}kbps)",
            clamped_tier,
            tier.label,
            tier.max_width,
            tier.max_height,
            tier.target_fps,
            tier.keyframe_interval_frames,
            tier.ideal_bitrate_kbps,
        );
    }

    /// Start screen sharing with an already-acquired `MediaStream`.
    ///
    /// Safari requires `getDisplayMedia()` to be called synchronously within a
    /// user-gesture (click) handler.  By obtaining the stream in the UI click
    /// handler and passing it here, the browser's gesture requirement is
    /// satisfied regardless of any async boundaries that follow.
    ///
    /// The stream is consumed: this method takes ownership and will stop its
    /// tracks when encoding ends or `stop()` is called.
    pub fn start_with_stream(&mut self, stream: MediaStream, initial_tier: usize) {
        self.apply_initial_tier(initial_tier);

        let EncoderState {
            enabled, switching, ..
        } = self.state.clone();
        enabled.store(true, Ordering::Release);

        let client = self.client.clone();
        let client_for_onended = client.clone();
        let client_for_state = client.clone();
        let userid = client.user_id().clone();
        let aes = client.aes();
        let current_bitrate = self.current_bitrate.clone();
        let current_fps = self.current_fps.clone();
        let on_state_change = self.on_state_change.clone();
        let screen_stream = self.screen_stream.clone();
        let tier_max_width = self.tier_max_width.clone();
        let tier_max_height = self.tier_max_height.clone();
        let tier_keyframe_interval = self.tier_keyframe_interval.clone();
        let force_keyframe = self.force_keyframe.clone();
        let active_video_track = self.active_video_track.clone();
        let screen_sharing_active = self.screen_sharing_active.clone();

        wasm_bindgen_futures::spawn_local(async move {
            let screen_to_share = stream;

            log::info!("Screen to share (pre-acquired stream): {screen_to_share:?}");

            Self::run_screen_encoding(
                screen_to_share,
                enabled,
                switching,
                client,
                client_for_onended,
                client_for_state,
                userid,
                aes,
                current_bitrate,
                current_fps,
                on_state_change,
                screen_stream,
                tier_max_width,
                tier_max_height,
                tier_keyframe_interval,
                force_keyframe,
                active_video_track,
                screen_sharing_active,
            )
            .await;
        });
    }

    /// Start encoding and sending the data to the client connection (if it's currently connected).
    /// The user is prompted by the browser to select which window or screen to encode.
    ///
    /// # Arguments
    /// * `initial_tier` - Starting tier index into `SCREEN_QUALITY_TIERS` (0=high, 1=medium, 2=low).
    ///   This allows the caller to select a conservative starting tier based on network signals
    ///   (e.g., RTT, camera tier index) at the moment screen sharing starts, giving a readable
    ///   first frame on constrained uplinks without waiting for the PID loop to ramp down.
    ///
    /// This will toggle the enabled state of the encoder.
    ///
    /// NOTE: On Safari, `getDisplayMedia()` must be called synchronously within a
    /// user-gesture handler.  If the call to `start()` is deferred (e.g. via a
    /// timeout or a re-render), Safari will reject the request.  In that case
    /// use [`start_with_stream`](Self::start_with_stream) instead, obtaining the
    /// stream directly in the click handler.
    pub fn start(&mut self, initial_tier: usize) {
        self.apply_initial_tier(initial_tier);

        let EncoderState {
            enabled, switching, ..
        } = self.state.clone();
        // enable the encoder
        enabled.store(true, Ordering::Release);

        let client = self.client.clone();
        let client_for_onended = client.clone();
        let client_for_state = client.clone();
        let userid = client.user_id().clone();
        let aes = client.aes();
        let current_bitrate = self.current_bitrate.clone();
        let current_fps = self.current_fps.clone();
        let on_state_change = self.on_state_change.clone();
        let screen_stream = self.screen_stream.clone();
        let tier_max_width = self.tier_max_width.clone();
        let tier_max_height = self.tier_max_height.clone();
        let tier_keyframe_interval = self.tier_keyframe_interval.clone();
        let force_keyframe = self.force_keyframe.clone();
        let active_video_track = self.active_video_track.clone();
        let screen_sharing_active = self.screen_sharing_active.clone();

        wasm_bindgen_futures::spawn_local(async move {
            let navigator = window().navigator();
            let media_devices = navigator.media_devices().unwrap_or_else(|_| {
                error!("Failed to get media devices - browser may not support screen sharing");
                panic!("MediaDevices not available");
            });

            // Build getDisplayMedia constraints requesting high-resolution capture.
            // This tells the browser to prefer the source's native resolution rather
            // than downscaling, which is critical for readable text and code.
            // Use {ideal: N} dictionaries instead of bare numbers — bare numbers are
            // treated as {exact: N} and will cause the browser to reject capture if
            // the source (e.g. 1440p or 4K monitor) doesn't match exactly.
            let width_constraint = js_sys::Object::new();
            let _ = Reflect::set(
                &width_constraint,
                &JsValue::from_str("ideal"),
                &JsValue::from_f64(1920.0),
            );
            let height_constraint = js_sys::Object::new();
            let _ = Reflect::set(
                &height_constraint,
                &JsValue::from_str("ideal"),
                &JsValue::from_f64(1080.0),
            );
            let framerate_constraint = js_sys::Object::new();
            let _ = Reflect::set(
                &framerate_constraint,
                &JsValue::from_str("ideal"),
                &JsValue::from_f64(10.0),
            );
            let video_constraints = js_sys::Object::new();
            let _ = Reflect::set(
                &video_constraints,
                &JsValue::from_str("width"),
                &width_constraint.into(),
            );
            let _ = Reflect::set(
                &video_constraints,
                &JsValue::from_str("height"),
                &height_constraint.into(),
            );
            let _ = Reflect::set(
                &video_constraints,
                &JsValue::from_str("frameRate"),
                &framerate_constraint.into(),
            );

            let constraints = web_sys::DisplayMediaStreamConstraints::new();
            constraints.set_video(&video_constraints.into());
            constraints.set_audio(&JsValue::FALSE);

            let screen_to_share: MediaStream =
                match media_devices.get_display_media_with_constraints(&constraints) {
                    Ok(promise) => match JsFuture::from(promise).await {
                        Ok(stream) => stream.unchecked_into::<MediaStream>(),
                        Err(e) => {
                            // Check if user cancelled (NotAllowedError = permission denied/cancelled)
                            let is_user_cancel = Reflect::get(&e, &JsString::from("name"))
                                .ok()
                                .and_then(|v| v.as_string())
                                .map(|name| name == "NotAllowedError")
                                .unwrap_or(false);

                            if is_user_cancel {
                                log::info!("User cancelled screen sharing");
                                if let Some(ref callback) = on_state_change {
                                    callback.emit(ScreenShareEvent::Cancelled);
                                }
                            } else {
                                let error_msg = format!("{e:?}");
                                error!("Screen sharing error: {error_msg}");
                                if let Some(ref callback) = on_state_change {
                                    callback.emit(ScreenShareEvent::Failed(error_msg));
                                }
                            }
                            enabled.store(false, Ordering::Release);
                            return;
                        }
                    },
                    Err(e) => {
                        let error_msg = format!("{e:?}");
                        error!("Failed to get display media: {error_msg}");
                        if let Some(ref callback) = on_state_change {
                            callback.emit(ScreenShareEvent::Failed(error_msg));
                        }
                        enabled.store(false, Ordering::Release);
                        return;
                    }
                };

            log::info!("Screen to share: {screen_to_share:?}");

            Self::run_screen_encoding(
                screen_to_share,
                enabled,
                switching,
                client,
                client_for_onended,
                client_for_state,
                userid,
                aes,
                current_bitrate,
                current_fps,
                on_state_change,
                screen_stream,
                tier_max_width,
                tier_max_height,
                tier_keyframe_interval,
                force_keyframe,
                active_video_track,
                screen_sharing_active,
            )
            .await;
        });
    }

    /// Shared async encoding loop used by both [`start`](Self::start) and
    /// [`start_with_stream`](Self::start_with_stream).
    ///
    /// All parameters are pre-cloned values that the encoding loop needs.
    /// The function takes ownership of everything so it can live inside a
    /// `spawn_local` future.
    ///
    /// Contains a `'restart` loop that handles encoder auto-recovery with
    /// exponential backoff when the encoder encounters fatal errors (e.g.,
    /// "closed codec", "InvalidStateError"). On restart, the media stream
    /// is re-acquired via `getDisplayMedia` since the original stream may
    /// have been torn down by the browser.
    #[allow(clippy::too_many_arguments)]
    async fn run_screen_encoding(
        screen_to_share: MediaStream,
        enabled: Arc<AtomicBool>,
        switching: Arc<AtomicBool>,
        client: VideoCallClient,
        client_for_onended: VideoCallClient,
        client_for_state: VideoCallClient,
        userid: String,
        aes: Rc<Aes128State>,
        current_bitrate: Rc<AtomicU32>,
        current_fps: Rc<AtomicU32>,
        on_state_change: Option<Callback<ScreenShareEvent>>,
        screen_stream: Rc<RefCell<Option<MediaStream>>>,
        tier_max_width: Rc<AtomicU32>,
        tier_max_height: Rc<AtomicU32>,
        tier_keyframe_interval: Rc<AtomicU32>,
        force_keyframe: Arc<AtomicBool>,
        active_video_track: Rc<RefCell<Option<MediaStreamTrack>>>,
        screen_sharing_active: Option<Rc<AtomicBool>>,
    ) {
        // Signal camera encoder ASAP after capture is confirmed so it begins
        // stepping down during encoder setup, not after encoding starts.
        if let Some(ref flag) = screen_sharing_active {
            flag.store(true, Ordering::Release);
        } else {
            log::warn!(
                "ScreenEncoder: screen_sharing_active flag not wired — \
                 camera bandwidth coordination will not engage. \
                 Ensure host.rs calls screen.set_screen_sharing_flag(camera.screen_sharing_flag())"
            );
        }

        screen_stream.borrow_mut().replace(screen_to_share.clone());

        // Helper to clean up stream on error - stops all tracks, clears flags, emits Failed event
        let cleanup_on_error = |screen_to_share: &MediaStream,
                                enabled: &Arc<AtomicBool>,
                                on_state_change: &Option<Callback<ScreenShareEvent>>,
                                error_msg: String| {
            // Stop all tracks
            if let Some(tracks) = screen_to_share.get_tracks().dyn_ref::<Array>() {
                for i in 0..tracks.length() {
                    if let Ok(track) = tracks.get(i).dyn_into::<MediaStreamTrack>() {
                        track.stop();
                    }
                }
            }
            // Reset enabled flag
            enabled.store(false, Ordering::Release);
            // Clear screen-sharing flag so camera drops its ceiling
            if let Some(ref flag) = screen_sharing_active {
                flag.store(false, Ordering::Release);
            }
            // Emit Failed event
            if let Some(ref callback) = on_state_change {
                callback.emit(ScreenShareEvent::Failed(error_msg));
            }
        };

        let navigator = window().navigator();
        let media_devices = navigator.media_devices().unwrap_or_else(|_| {
            error!("Failed to get media devices - browser may not support screen sharing");
            panic!("MediaDevices not available");
        });

        let mut restart_count: u32 = 0;
        const MAX_RESTARTS: u32 = 5;
        let mut media_acquired = true; // true because we already have a stream

        // These variables hold the current media state. They are initialized from
        // the stream passed in, and may be re-acquired on restart.
        let mut current_stream: Option<MediaStream> = Some(screen_to_share);
        let mut current_track: Option<MediaStreamTrack> = None;
        let mut width: u32 = 0;
        let mut height: u32 = 0;

        // The onended handler closure must live as long as we use the media track.
        // We store it here so it isn't dropped when the inner loop restarts.
        let mut _onended_handler: Option<Closure<dyn FnMut()>> = None;

        // Setup FPS tracking and screen output handler.
        // These closures are created once and shared across encoder restarts
        // because the VideoEncoderInit callbacks are wired to the same output
        // pipeline regardless of which VideoEncoder instance is active.
        let screen_output_handler = {
            let mut buffer: Vec<u8> = Vec::with_capacity(150_000);
            let mut sequence_number = 0;
            let performance = window()
                .performance()
                .expect("Performance API not available");
            let mut last_chunk_time = performance.now();
            let mut chunks_in_last_second = 0;
            let current_fps = current_fps.clone();
            let userid = userid.clone();
            let aes = aes.clone();
            let client = client.clone();

            Box::new(move |chunk: JsValue| {
                let now = window()
                    .performance()
                    .expect("Performance API not available")
                    .now();
                let chunk = web_sys::EncodedVideoChunk::from(chunk);

                // Update FPS calculation
                chunks_in_last_second += 1;
                if now - last_chunk_time >= 1000.0 {
                    let fps = chunks_in_last_second;
                    current_fps.store(fps, Ordering::Relaxed);
                    chunks_in_last_second = 0;
                    last_chunk_time = now;
                }

                // Ensure buffer is large enough for this chunk
                let byte_length = chunk.byte_length() as usize;
                if buffer.len() < byte_length {
                    buffer.resize(byte_length, 0);
                }

                let packet: PacketWrapper = transform_screen_chunk(
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

        let screen_error_handler = Closure::wrap(Box::new(move |e: JsValue| {
            error!("Screen encoder error: {e:?}");
        }) as Box<dyn FnMut(JsValue)>);

        let screen_output_handler = Closure::wrap(screen_output_handler as Box<dyn FnMut(JsValue)>);

        let screen_encoder_init = VideoEncoderInit::new(
            screen_error_handler.as_ref().unchecked_ref(),
            screen_output_handler.as_ref().unchecked_ref(),
        );

        'restart: loop {
            // --- Backoff + max-restart guard (skip on first iteration) ---
            if restart_count > 0 {
                let delay_ms = 500u64.saturating_mul(restart_count.min(4) as u64);
                log::warn!(
                    "ScreenEncoder: restarting encoder (attempt {restart_count}/{MAX_RESTARTS}), \
                     backoff {delay_ms}ms"
                );
                sleep(Duration::from_millis(delay_ms)).await;
                if restart_count >= MAX_RESTARTS {
                    error!("ScreenEncoder: max restarts ({MAX_RESTARTS}) reached, giving up");
                    if let Some(ref stream) = current_stream {
                        cleanup_on_error(
                            stream,
                            &enabled,
                            &on_state_change,
                            "Screen encoder failed after repeated restarts".to_string(),
                        );
                    }
                    return;
                }
                // Check if stop() was called or track ended during backoff
                if !enabled.load(Ordering::Acquire) {
                    log::info!("ScreenEncoder: disabled during restart backoff, exiting");
                    break 'restart;
                }
            }

            // --- Media acquisition (first iteration uses the passed-in stream,
            //     restarts re-acquire via getDisplayMedia) ---
            if should_reacquire_screen_capture(media_acquired, restart_count) {
                if let Some(track) = current_track.take() {
                    track.set_onended(None);
                    track.stop();
                }
                if let Some(stream) = current_stream.take() {
                    stop_media_stream_tracks(&stream);
                }
                screen_stream.borrow_mut().take();
                active_video_track.borrow_mut().take();
                _onended_handler = None;

                // Build getDisplayMedia constraints requesting high-resolution capture.
                let width_constraint = js_sys::Object::new();
                let _ = Reflect::set(
                    &width_constraint,
                    &JsValue::from_str("ideal"),
                    &JsValue::from_f64(1920.0),
                );
                let height_constraint = js_sys::Object::new();
                let _ = Reflect::set(
                    &height_constraint,
                    &JsValue::from_str("ideal"),
                    &JsValue::from_f64(1080.0),
                );
                let framerate_constraint = js_sys::Object::new();
                let _ = Reflect::set(
                    &framerate_constraint,
                    &JsValue::from_str("ideal"),
                    &JsValue::from_f64(10.0),
                );
                let video_constraints = js_sys::Object::new();
                let _ = Reflect::set(
                    &video_constraints,
                    &JsValue::from_str("width"),
                    &width_constraint.into(),
                );
                let _ = Reflect::set(
                    &video_constraints,
                    &JsValue::from_str("height"),
                    &height_constraint.into(),
                );
                let _ = Reflect::set(
                    &video_constraints,
                    &JsValue::from_str("frameRate"),
                    &framerate_constraint.into(),
                );

                let constraints = web_sys::DisplayMediaStreamConstraints::new();
                constraints.set_video(&video_constraints.into());
                constraints.set_audio(&JsValue::FALSE);

                let acquired_stream: MediaStream =
                    match media_devices.get_display_media_with_constraints(&constraints) {
                        Ok(promise) => match JsFuture::from(promise).await {
                            Ok(stream) => stream.unchecked_into::<MediaStream>(),
                            Err(e) => {
                                // Check if user cancelled (NotAllowedError = permission denied/cancelled)
                                let is_user_cancel = Reflect::get(&e, &JsString::from("name"))
                                    .ok()
                                    .and_then(|v| v.as_string())
                                    .map(|name| name == "NotAllowedError")
                                    .unwrap_or(false);

                                if is_user_cancel {
                                    log::info!("User cancelled screen sharing");
                                    if let Some(ref callback) = on_state_change {
                                        callback.emit(ScreenShareEvent::Cancelled);
                                    }
                                } else {
                                    let error_msg = format!("{e:?}");
                                    error!("Screen sharing error: {error_msg}");
                                    if let Some(ref callback) = on_state_change {
                                        callback.emit(ScreenShareEvent::Failed(error_msg));
                                    }
                                }
                                enabled.store(false, Ordering::Release);
                                return;
                            }
                        },
                        Err(e) => {
                            let error_msg = format!("{e:?}");
                            error!("Failed to get display media: {error_msg}");
                            if let Some(ref callback) = on_state_change {
                                callback.emit(ScreenShareEvent::Failed(error_msg));
                            }
                            enabled.store(false, Ordering::Release);
                            return;
                        }
                    };

                log::info!("Screen to share: {acquired_stream:?}");

                // Signal camera encoder ASAP after capture is confirmed so it begins
                // stepping down during encoder setup, not after encoding starts.
                if let Some(ref flag) = screen_sharing_active {
                    flag.store(true, Ordering::Release);
                }

                screen_stream.borrow_mut().replace(acquired_stream.clone());

                let screen_track = Box::new(
                    acquired_stream
                        .get_video_tracks()
                        .find(&mut |_: JsValue, _: u32, _: Array| true)
                        .unchecked_into::<VideoTrack>(),
                );

                let track = screen_track
                    .as_ref()
                    .clone()
                    .unchecked_into::<MediaStreamTrack>();

                // Set contentHint = 'detail' so the encoder optimizes for sharp text
                let _ = Reflect::set(
                    &track,
                    &JsValue::from_str("contentHint"),
                    &JsValue::from_str("detail"),
                );

                // Store the original track so stop() can stop it synchronously
                active_video_track.borrow_mut().replace(track.clone());

                // Set up onended handler to detect when user clicks browser's "Stop sharing" button
                _onended_handler = {
                    let enabled_clone = enabled.clone();
                    let on_state_change_clone = on_state_change.clone();
                    let screen_sharing_flag_clone = screen_sharing_active.clone();
                    let client_onended = client_for_onended.clone();
                    let handler = Closure::wrap(Box::new(move || {
                        log::info!("Screen share track ended (user stopped sharing)");
                        enabled_clone.store(false, Ordering::Release);
                        if let Some(ref flag) = screen_sharing_flag_clone {
                            flag.store(false, Ordering::Release);
                        }
                        client_onended.set_screen_enabled(false);
                        if let Some(ref callback) = on_state_change_clone {
                            callback.emit(ScreenShareEvent::Stopped);
                        }
                    }) as Box<dyn FnMut()>);
                    track.set_onended(Some(handler.as_ref().unchecked_ref()));
                    Some(handler)
                };

                let track_settings = track.get_settings();
                width = track_settings.get_width().expect("width is None") as u32;
                height = track_settings.get_height().expect("height is None") as u32;

                current_stream = Some(acquired_stream);
                current_track = Some(track);
                media_acquired = true;
            } else if current_track.is_none() {
                // First iteration: extract track from the initially-passed stream
                let stream_ref = current_stream.as_ref().expect("stream must exist");

                let screen_track = Box::new(
                    stream_ref
                        .get_video_tracks()
                        .find(&mut |_: JsValue, _: u32, _: Array| true)
                        .unchecked_into::<VideoTrack>(),
                );

                let track = screen_track
                    .as_ref()
                    .clone()
                    .unchecked_into::<MediaStreamTrack>();

                // Set contentHint = 'detail' so the encoder optimizes for sharp text
                // and edges rather than smooth motion.
                let _ = Reflect::set(
                    &track,
                    &JsValue::from_str("contentHint"),
                    &JsValue::from_str("detail"),
                );

                // Store the original track so stop() can stop it synchronously
                active_video_track.borrow_mut().replace(track.clone());

                // Set up onended handler
                _onended_handler = {
                    let enabled_clone = enabled.clone();
                    let on_state_change_clone = on_state_change.clone();
                    let screen_sharing_flag_clone = screen_sharing_active.clone();
                    let client_onended = client_for_onended.clone();
                    let handler = Closure::wrap(Box::new(move || {
                        log::info!("Screen share track ended (user stopped sharing)");
                        enabled_clone.store(false, Ordering::Release);
                        if let Some(ref flag) = screen_sharing_flag_clone {
                            flag.store(false, Ordering::Release);
                        }
                        client_onended.set_screen_enabled(false);
                        if let Some(ref callback) = on_state_change_clone {
                            callback.emit(ScreenShareEvent::Stopped);
                        }
                    }) as Box<dyn FnMut()>);
                    track.set_onended(Some(handler.as_ref().unchecked_ref()));
                    Some(handler)
                };

                let track_settings = track.get_settings();
                width = track_settings.get_width().expect("width is None") as u32;
                height = track_settings.get_height().expect("height is None") as u32;

                current_track = Some(track);
            }

            // Unwrap the media references — they are guaranteed to be Some after
            // the first iteration sets media_acquired = true.
            let stream_ref = current_stream.as_ref().expect("stream must exist");
            let track_ref = current_track.as_ref().expect("track must exist");

            // --- Create VideoEncoder (re-created on every restart) ---
            let screen_encoder = match VideoEncoder::new(&screen_encoder_init) {
                Ok(encoder) => Box::new(encoder),
                Err(e) => {
                    let msg = format!("Failed to create video encoder: {e:?}");
                    error!("ScreenEncoder: {msg} (restart {restart_count})");
                    restart_count += 1;
                    continue 'restart;
                }
            };

            // --- Initial configure ---
            let mut local_bitrate: u32 = current_bitrate.load(Ordering::Relaxed) * 1000;
            let screen_encoder_config =
                VideoEncoderConfig::new(get_video_codec_string(), height, width);
            screen_encoder_config.set_bitrate(local_bitrate as f64);
            screen_encoder_config.set_latency_mode(LatencyMode::Realtime);
            set_vbr_mode(&screen_encoder_config);
            if let Err(e) = screen_encoder.configure(&screen_encoder_config) {
                SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL.fetch_add(1, Ordering::Relaxed);
                let msg = format!("Error configuring screen encoder: {e:?}");
                error!("ScreenEncoder: {msg} (restart {restart_count})");
                restart_count += 1;
                continue 'restart;
            }

            // --- Create MediaStreamTrackProcessor + reader ---
            // These must be re-created each restart because the previous reader
            // may be in an error state after the encoder died mid-read.
            let screen_processor = match MediaStreamTrackProcessor::new(
                &MediaStreamTrackProcessorInit::new(track_ref),
            ) {
                Ok(processor) => processor,
                Err(e) => {
                    let msg = format!("ScreenEncoder: failed to create track processor: {e:?}");
                    error!("{msg}");
                    let _ = screen_encoder.close();
                    if restart_count > 0 {
                        // On restart, a processor failure means the capture track is dead.
                        // getDisplayMedia can't be re-called without a user gesture -- give up.
                        cleanup_on_error(stream_ref, &enabled, &on_state_change, msg);
                        return;
                    }
                    // On first attempt, treat as a normal init failure.
                    cleanup_on_error(stream_ref, &enabled, &on_state_change, msg);
                    return;
                }
            };

            // Emit Started on every successful acquisition so the preview can
            // bind to the fresh stream after a restart.
            if restart_count == 0 {
                client_for_state.set_screen_enabled(true);
            } else {
                log::info!(
                    "ScreenEncoder: encoder restarted successfully (attempt {restart_count})"
                );
            }
            if let Some(ref callback) = on_state_change {
                callback.emit(ScreenShareEvent::Started(stream_ref.clone()));
            }

            let screen_reader = match screen_processor
                .readable()
                .get_reader()
                .dyn_into::<ReadableStreamDefaultReader>()
            {
                Ok(reader) => reader,
                Err(e) => {
                    let msg = format!(
                        "ScreenEncoder: failed to acquire ReadableStreamDefaultReader: {e:?}"
                    );
                    error!("{msg}");
                    let _ = screen_encoder.close();
                    cleanup_on_error(stream_ref, &enabled, &on_state_change, msg);
                    return;
                }
            };

            let mut screen_frame_counter: u32 = 0;
            let mut last_pli_keyframe_time: f64 = 0.0;
            let mut current_encoder_width = width;
            let mut current_encoder_height = height;

            // Cache tier-controlled values
            let mut local_keyframe_interval = tier_keyframe_interval.load(Ordering::Relaxed);
            let mut local_tier_max_width = tier_max_width.load(Ordering::Relaxed);
            let mut local_tier_max_height = tier_max_height.load(Ordering::Relaxed);

            // Track whether the inner loop exited due to a fatal encode error
            // vs. a stream-read error or shutdown signal.
            let mut fatal_encode_exit = false;

            'encode: loop {
                // Check if we should stop encoding (user called stop() or
                // onended fired). This exits the function entirely — no restart.
                if !enabled.load(Ordering::Acquire) || switching.load(Ordering::Acquire) {
                    switching.store(false, Ordering::Release);
                    track_ref.stop();
                    if let Err(e) = screen_encoder.close() {
                        error!("Error closing screen encoder: {e:?}");
                    }
                    // Break to final cleanup — not a restart.
                    break 'restart;
                }

                // --- Guard: skip reconfigure if encoder is already closed ---
                if screen_encoder.state() == CodecState::Closed {
                    log::warn!("ScreenEncoder: encoder found in closed state, triggering restart");
                    restart_count += 1;
                    break 'encode;
                }

                // Check for tier-driven dimension/keyframe changes.
                let new_tier_w = tier_max_width.load(Ordering::Relaxed);
                let new_tier_h = tier_max_height.load(Ordering::Relaxed);
                let new_kf = tier_keyframe_interval.load(Ordering::Relaxed);

                let tier_dims_changed =
                    new_tier_w != local_tier_max_width || new_tier_h != local_tier_max_height;
                if tier_dims_changed {
                    local_tier_max_width = new_tier_w;
                    local_tier_max_height = new_tier_h;

                    let constrained_w = current_encoder_width.min(local_tier_max_width);
                    let constrained_h = current_encoder_height.min(local_tier_max_height);

                    log::info!(
                        "ScreenEncoder: tier dimension change -> {}x{} (was {}x{})",
                        constrained_w,
                        constrained_h,
                        current_encoder_width,
                        current_encoder_height,
                    );
                    current_encoder_width = constrained_w;
                    current_encoder_height = constrained_h;

                    // Guard: check encoder state before reconfigure
                    if screen_encoder.state() == CodecState::Closed {
                        log::warn!(
                            "ScreenEncoder: encoder closed before tier reconfigure, restarting"
                        );
                        restart_count += 1;
                        break 'encode;
                    }
                    let new_config = VideoEncoderConfig::new(
                        get_video_codec_string(),
                        current_encoder_height,
                        current_encoder_width,
                    );
                    new_config.set_bitrate(local_bitrate as f64);
                    new_config.set_latency_mode(LatencyMode::Realtime);
                    set_vbr_mode(&new_config);
                    if let Err(e) = screen_encoder.configure(&new_config) {
                        SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL.fetch_add(1, Ordering::Relaxed);
                        error!("Error reconfiguring screen encoder for tier change: {e:?}");
                        if is_fatal_encoder_error(&e) {
                            restart_count += 1;
                            break 'encode;
                        }
                    }
                }

                if new_kf != local_keyframe_interval {
                    local_keyframe_interval = new_kf;
                    log::info!(
                        "ScreenEncoder: keyframe interval changed to {}",
                        local_keyframe_interval
                    );
                }

                // Update the bitrate if it has changed from diagnostics system
                let new_bitrate = current_bitrate.load(Ordering::Relaxed) * 1000;
                if new_bitrate != local_bitrate && !tier_dims_changed {
                    info!("Updating screen bitrate to {new_bitrate}");
                    local_bitrate = new_bitrate;
                    // Guard: check encoder state before bitrate reconfigure
                    if screen_encoder.state() == CodecState::Closed {
                        log::warn!(
                            "ScreenEncoder: encoder closed before bitrate reconfigure, restarting"
                        );
                        restart_count += 1;
                        break 'encode;
                    }
                    let new_config = VideoEncoderConfig::new(
                        get_video_codec_string(),
                        current_encoder_height,
                        current_encoder_width,
                    );
                    new_config.set_bitrate(local_bitrate as f64);
                    new_config.set_latency_mode(LatencyMode::Realtime);
                    set_vbr_mode(&new_config);
                    if let Err(e) = screen_encoder.configure(&new_config) {
                        SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL.fetch_add(1, Ordering::Relaxed);
                        error!("Error configuring screen encoder: {e:?}");
                        if is_fatal_encoder_error(&e) {
                            restart_count += 1;
                            break 'encode;
                        }
                    }
                } else if new_bitrate != local_bitrate {
                    local_bitrate = new_bitrate;
                }

                match JsFuture::from(screen_reader.read()).await {
                    Ok(js_frame) => {
                        let value = match Reflect::get(&js_frame, &JsString::from("value")) {
                            Ok(v) => v,
                            Err(e) => {
                                error!("Failed to get frame value: {e:?}");
                                continue;
                            }
                        };

                        if value.is_undefined() {
                            error!("Screen share stream ended");
                            break 'encode;
                        }

                        let video_frame = value.unchecked_into::<VideoFrame>();
                        let frame_width = video_frame.display_width();
                        let frame_height = video_frame.display_height();
                        // Constrain to tier max dimensions.
                        let frame_width = if frame_width > 0 {
                            (frame_width as u32).min(local_tier_max_width)
                        } else {
                            0
                        };
                        let frame_height = if frame_height > 0 {
                            (frame_height as u32).min(local_tier_max_height)
                        } else {
                            0
                        };

                        if frame_width > 0
                            && frame_height > 0
                            && (frame_width != current_encoder_width
                                || frame_height != current_encoder_height)
                        {
                            info!("Frame dimensions changed from {current_encoder_width}x{current_encoder_height} to {frame_width}x{frame_height}, reconfiguring encoder");

                            current_encoder_width = frame_width;
                            current_encoder_height = frame_height;

                            // Guard: check encoder state before dimension reconfigure
                            if screen_encoder.state() == CodecState::Closed {
                                log::warn!(
                                    "ScreenEncoder: encoder closed before dimension reconfigure, restarting"
                                );
                                video_frame.close();
                                fatal_encode_exit = true;
                                restart_count += 1;
                                break 'encode;
                            }
                            let new_config = VideoEncoderConfig::new(
                                get_video_codec_string(),
                                current_encoder_height,
                                current_encoder_width,
                            );
                            new_config.set_bitrate(local_bitrate as f64);
                            new_config.set_latency_mode(LatencyMode::Realtime);
                            set_vbr_mode(&new_config);
                            if let Err(e) = screen_encoder.configure(&new_config) {
                                SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL
                                    .fetch_add(1, Ordering::Relaxed);
                                error!(
                                    "Error reconfiguring screen encoder with new dimensions: {e:?}"
                                );
                                if is_fatal_encoder_error(&e) {
                                    video_frame.close();
                                    fatal_encode_exit = true;
                                    restart_count += 1;
                                    break 'encode;
                                }
                            }
                        }

                        let opts = VideoEncoderEncodeOptions::new();
                        // Check if a keyframe was requested via PLI.
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
                        // Use tier-controlled keyframe interval.
                        // Using `%` instead of `.is_multiple_of()` for compatibility
                        // with Rust toolchains older than 1.87.
                        #[allow(clippy::manual_is_multiple_of)]
                        let is_periodic_keyframe = local_keyframe_interval > 0
                            && screen_frame_counter % local_keyframe_interval == 0;
                        opts.set_key_frame(is_periodic_keyframe || force_pli);
                        if force_pli {
                            log::info!(
                                "ScreenEncoder: forcing keyframe at frame {} (PLI)",
                                screen_frame_counter
                            );
                        } else if pli_requested {
                            log::info!(
                                "ScreenEncoder: PLI keyframe suppressed at frame {} (cooldown: {:.0}ms since last)",
                                screen_frame_counter,
                                now - last_pli_keyframe_time,
                            );
                        }

                        match screen_encoder.encode_with_options(&video_frame, &opts) {
                            Ok(_) => {
                                SCREEN_ENCODER_FRAMES_SUBMITTED_OK.fetch_add(1, Ordering::Relaxed);
                                if restart_count > 0 {
                                    // First successful encode after a restart — reset the
                                    // counter so transient errors don't accumulate toward
                                    // the max-restart limit across unrelated incidents.
                                    log::info!(
                                        "ScreenEncoder: first successful encode after restart, \
                                         resetting restart counter"
                                    );
                                    restart_count = 0;
                                }
                            }
                            Err(e) => {
                                let msg = format!("{e:?}");
                                match classify_encode_error(&msg) {
                                    EncodeErrorBucket::ClosedCodec => {
                                        SCREEN_ENCODER_ERRORS_CLOSED_CODEC
                                            .fetch_add(1, Ordering::Relaxed);
                                    }
                                    EncodeErrorBucket::VpxMemAlloc => {
                                        SCREEN_ENCODER_ERRORS_VPX_MEM_ALLOC
                                            .fetch_add(1, Ordering::Relaxed);
                                    }
                                    EncodeErrorBucket::Generic => {
                                        SCREEN_ENCODER_ERRORS_GENERIC
                                            .fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                if is_fatal_encoder_error(&e) {
                                    error!(
                                        "ScreenEncoder: fatal encode error (restart {restart_count}): {e:?}"
                                    );
                                    video_frame.close();
                                    fatal_encode_exit = true;
                                    restart_count += 1;
                                    break 'encode;
                                }
                                error!("Error encoding screen frame: {e:?}");
                            }
                        }
                        video_frame.close();
                        screen_frame_counter += 1;
                    }
                    Err(e) => {
                        error!("Error reading screen frame: {e:?}");
                        break 'encode;
                    }
                }
            } // end 'encode

            // --- Post-inner-loop: decide restart vs full exit ---
            // Close the dead encoder before restarting (best-effort; it may
            // already be closed).
            let _ = screen_encoder.close();

            if fatal_encode_exit {
                // Fatal encode error: the encoder died but the stream may be
                // alive.  Continue to the next restart iteration.
                continue 'restart;
            }

            log::warn!("ScreenEncoder: restarting with a fresh screen capture stream");
            restart_count += 1;
            continue 'restart;
        } // end 'restart

        // --- Final cleanup (reached on shutdown or unrecoverable failure) ---
        // Clear the active track reference so stop() doesn't try to stop it again.
        active_video_track.borrow_mut().take();

        // Clear the onended handler before dropping the closure to avoid dangling reference
        if let Some(ref track) = current_track {
            track.set_onended(None);
            track.stop();
        }

        if let Some(ref stream) = current_stream {
            if let Some(tracks) = stream.get_tracks().dyn_ref::<Array>() {
                for i in 0..tracks.length() {
                    if let Ok(track) = tracks.get(i).dyn_into::<MediaStreamTrack>() {
                        track.stop();
                    }
                }
            }
        }

        // Clear screen-sharing flag so the camera encoder removes its quality ceiling.
        if let Some(ref flag) = screen_sharing_active {
            flag.store(false, Ordering::Release);
        }

        // Emit Stopped event if we haven't already (onended handler might have already fired)
        // Check enabled flag - if it's still true, onended hasn't fired yet
        if enabled.swap(false, Ordering::AcqRel) {
            client_for_state.set_screen_enabled(false);
            if let Some(ref callback) = on_state_change {
                callback.emit(ScreenShareEvent::Stopped);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_fatal_encoder_error_message;
    use super::should_reacquire_screen_capture;

    #[test]
    fn screen_capture_is_reacquired_after_any_restart() {
        assert!(should_reacquire_screen_capture(false, 0));
        assert!(should_reacquire_screen_capture(true, 1));
        assert!(should_reacquire_screen_capture(true, 4));
    }

    #[test]
    fn screen_encoder_fatal_errors_match_closed_codec_signatures() {
        assert!(is_fatal_encoder_error_message(
            "InvalidStateError: closed codec"
        ));
        assert!(is_fatal_encoder_error_message(
            "Memory allocation error (Unable to find free frame buffer)"
        ));
        assert!(!is_fatal_encoder_error_message(
            "EncodingError: transient frame drop"
        ));
    }
}

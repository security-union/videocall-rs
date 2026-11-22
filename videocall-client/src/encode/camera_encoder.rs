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
use log::error;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use videocall_types::protos::media_packet::VideoCodec;
use videocall_types::Callback;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::ConstrainDomStringParameters;
use web_sys::HtmlVideoElement;
use web_sys::LatencyMode;
use web_sys::MediaStream;
use web_sys::MediaStreamConstraints;
use web_sys::MediaStreamTrack;
use web_sys::MediaStreamTrackProcessor;
use web_sys::MediaStreamTrackProcessorInit;
use web_sys::ReadableStreamDefaultReader;
use web_sys::VideoEncoder;
use web_sys::VideoEncoderBitrateMode;
use web_sys::VideoEncoderConfig;
use web_sys::VideoEncoderEncodeOptions;
use web_sys::VideoEncoderInit;
use web_sys::VideoEncoderSupport;
use web_sys::VideoTrack;

use super::super::client::VideoCallClient;
use super::capture_session::{ActiveCapture, CaptureSession};
use super::encoder_dimensions::FrameSize;
use super::encoder_state::EncoderState;
use super::transform::transform_video_chunk;
use super::FrameRead;

use crate::adaptive_quality_constants::{
    CAMERA_KEYFRAME_INTERVAL_SECS, DIAGNOSTICS_REPORT_INTERVAL_MS,
};
use crate::constants::{get_video_codec, get_video_codec_string};
use crate::diagnostics::frame_rate::{FrameEvent, FrameRateMeter};
use crate::rate_control::camera::{bitrate_change_applies, CameraOutput, HandOff, KeyframeClock};
use crate::rate_control::CameraRateController;
use crate::traffic_shaping::ShapedKind;
use crate::utils::monotonic_now_ms;

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
    current_fps: Rc<AtomicU32>,
    /// Frames the encoder produced, measured like the viewers measure what
    /// they receive.
    output_meter: Rc<RefCell<FrameRateMeter>>,
    on_error: Option<Callback<String>>,
    /// The viewer-driven rate control, owned by the client (which feeds it
    /// the viewers' reports). Asked once per captured frame.
    camera_rate: Rc<RefCell<CameraRateController>>,
    /// When set to `true`, the next encoded frame will be forced as a keyframe.
    /// Used by the PLI (Picture Loss Indication) mechanism: when a remote peer
    /// detects missing frames and sends a KEYFRAME_REQUEST, the VideoCallClient
    /// sets this flag so the encoder produces an immediate keyframe.
    force_keyframe: Arc<AtomicBool>,
    /// The running capture, so stop() releases the camera without waiting for a frame.
    capture: ActiveCapture,
}

/// What the camera's `VideoEncoder` is configured with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CameraEncoderSettings {
    pub(crate) size: FrameSize,
    pub(crate) bitrate_bps: u32,
    pub(crate) framerate: f64,
}

/// Whether to reconfigure the encoder for `wanted`, and whether that frame
/// must be a keyframe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reconfigure {
    pub(crate) configure: bool,
    pub(crate) keyframe: bool,
}

/// What the rate control asks of the encoder for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Wanted {
    pub(crate) settings: CameraEncoderSettings,
    /// The publisher's probe step, for the bitrate threshold.
    pub(crate) probe_fraction: f64,
    /// The publisher is draining a backlog.
    pub(crate) draining: bool,
}

/// Whether to reconfigure the encoder, and whether that frame must be a
/// keyframe. A size or frame-rate change always reconfigures. A bitrate-only
/// change does when [`bitrate_change_applies`]: a probe step or a drain, not
/// RTT jitter.
///
/// A size change forced by the source (first frame, rotation, a new camera
/// mode) gets a keyframe, as before. A size change from the rate control's
/// tier does not on VP9, which rescales its references (a live reconfigure
/// measured on Chromium does not force one); on VP8, whose frame size lives
/// only in keyframes, `keyframe_on_tier_resize` asks for one.
pub(crate) fn plan_reconfigure(
    current: Option<CameraEncoderSettings>,
    wanted: Wanted,
    previous_source: Option<FrameSize>,
    source: FrameSize,
    keyframe_on_tier_resize: bool,
) -> Reconfigure {
    let settings = wanted.settings;
    let unchanged = Reconfigure {
        configure: false,
        keyframe: false,
    };
    if settings.size.is_empty() {
        return unchanged;
    }
    let Some(current) = current else {
        return Reconfigure {
            configure: true,
            keyframe: true,
        };
    };
    let size_changed = current.size != settings.size;
    let shape_changed = size_changed || current.framerate != settings.framerate;
    if !shape_changed
        && !bitrate_change_applies(
            current.bitrate_bps,
            settings.bitrate_bps,
            wanted.probe_fraction,
            wanted.draining,
        )
    {
        return unchanged;
    }
    let source_changed = previous_source != Some(source);
    Reconfigure {
        configure: true,
        keyframe: size_changed && (source_changed || keyframe_on_tier_resize),
    }
}

/// How many failures in a row one capture recovers from (by recreating the
/// encoder) before it gives up and reports the camera as failed.
const MAX_ENCODER_RECOVERIES: u32 = 3;

/// How long the encoder must produce output after a failure before earlier
/// failures are forgotten: one keyframe period, so the recovered encoder
/// has produced a full group of pictures.
const HEALTHY_STRETCH_MS: f64 = CAMERA_KEYFRAME_INTERVAL_SECS * 1000.0;

/// What to do after an encoder failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recovery {
    /// Recreate the encoder and go on.
    Recreate,
    /// Too many failures in a row: report the camera as failed.
    GiveUp,
}

/// Encoder failures in a row within one capture. Failures add up until the
/// encoder has produced two outputs [`HEALTHY_STRETCH_MS`] apart with no
/// failure in between; after that a new failure is the first again, so a
/// transient error hours into a call does not count against the ones at its
/// start. Continuity between those two outputs is not checked: a pause in
/// output (camera paused, frames skipped) is no failure, and any failure
/// restarts the stretch.
#[derive(Debug, Default)]
pub(crate) struct EncoderHealth {
    failures: u32,
    /// The first output since the last failure; the stretch starts here.
    healthy_since_ms: Option<f64>,
}

impl EncoderHealth {
    /// The encoder failed (an error callback, or a configure it refused).
    pub(crate) fn on_failure(&mut self) -> Recovery {
        self.failures += 1;
        self.healthy_since_ms = None;
        if self.failures > MAX_ENCODER_RECOVERIES {
            Recovery::GiveUp
        } else {
            Recovery::Recreate
        }
    }

    /// The encoder produced a chunk.
    pub(crate) fn on_output(&mut self, now_ms: f64) {
        if self.failures == 0 {
            return;
        }
        let since = *self.healthy_since_ms.get_or_insert(now_ms);
        if now_ms - since >= HEALTHY_STRETCH_MS {
            self.failures = 0;
            self.healthy_since_ms = None;
        }
    }

    /// Failures in a row so far.
    pub(crate) fn failures(&self) -> u32 {
        self.failures
    }
}

/// Whether a `configure()` that threw counts as an encoder failure. Per
/// WebCodecs a synchronous throw leaves a configured encoder on its previous
/// config, still encoding: that is no failure, the old settings stay. A
/// (recreated) encoder that was never configured refuses every frame and
/// fires no error callback: that is one.
pub(crate) fn configure_failure_counts(configured_before: bool) -> bool {
    !configured_before
}

/// The camera encoder config: realtime, `framerate` set (without it the
/// encoder under-spends), and constant bitrate mode when the browser
/// supports it (the default variable mode ignores low targets).
fn camera_encoder_config(
    settings: CameraEncoderSettings,
    constant_bitrate: bool,
) -> VideoEncoderConfig {
    // web-sys takes height before width.
    let config = VideoEncoderConfig::new(
        get_video_codec_string(),
        settings.size.height,
        settings.size.width,
    );
    config.set_bitrate_f64(settings.bitrate_bps as f64);
    config.set_framerate(settings.framerate);
    config.set_latency_mode(LatencyMode::Realtime);
    if constant_bitrate {
        config.set_bitrate_mode(VideoEncoderBitrateMode::Constant);
    }
    config
}

/// Configures `encoder` for `settings`. A config the encoder rejects in
/// constant bitrate mode is retried in variable mode, which then sticks.
/// Returns whether the encoder took a config.
fn configure_camera_encoder(
    encoder: &VideoEncoder,
    settings: CameraEncoderSettings,
    constant_bitrate: &mut bool,
) -> bool {
    match encoder.configure(&camera_encoder_config(settings, *constant_bitrate)) {
        Ok(()) => return true,
        Err(e) if *constant_bitrate => {
            log::warn!("CameraEncoder: constant bitrate config rejected ({e:?}), using variable");
            *constant_bitrate = false;
        }
        Err(e) => {
            // The caller decides whether this is a failure.
            log::debug!("Error configuring video encoder: {e:?}");
            return false;
        }
    }
    match encoder.configure(&camera_encoder_config(settings, false)) {
        Ok(()) => true,
        Err(e) => {
            log::debug!("Error configuring video encoder: {e:?}");
            false
        }
    }
}

/// Whether this browser's encoder takes `settings` in constant bitrate mode.
/// Anything but a clear yes (a rejection, an unsupported config) means no,
/// and the encoder keeps the default variable mode.
async fn constant_bitrate_supported(settings: CameraEncoderSettings) -> bool {
    let config = camera_encoder_config(settings, true);
    match JsFuture::from(VideoEncoder::is_config_supported(&config)).await {
        Ok(support) => support
            .unchecked_into::<VideoEncoderSupport>()
            .get_supported()
            .unwrap_or(false),
        Err(e) => {
            log::warn!("CameraEncoder: isConfigSupported failed: {e:?}");
            false
        }
    }
}

impl CameraEncoder {
    /// Construct a camera encoder, with arguments:
    ///
    /// * `client` - an instance of a [`VideoCallClient`](crate::VideoCallClient).  It does not need to be currently connected.
    ///
    /// * `video_elem_id` - the the ID of an `HtmlVideoElement` to which the camera will be connected.  It does not need to currently exist.
    ///
    /// * `max_bitrate_kbps` - the most the camera may send, in kbps. The encoder starts
    ///   here and the viewer-driven rate control never goes above it.
    ///
    /// The encoder is created in a disabled state, [`encoder.set_enabled(true)`](Self::set_enabled) must be called before it can start encoding.
    /// The encoder is created without a camera selected, [`encoder.select(device_id)`](Self::select) must be called before it can start encoding.
    pub fn new(
        client: VideoCallClient,
        video_elem_id: &str,
        max_bitrate_kbps: u32,
        on_error: Callback<String>,
    ) -> Self {
        let camera_rate = client.camera_rate();
        camera_rate
            .borrow_mut()
            .set_max_bitrate(f64::from(max_bitrate_kbps) * 1000.0, monotonic_now_ms());
        Self {
            client,
            video_elem_id: video_elem_id.to_string(),
            state: EncoderState::new(),
            current_fps: Rc::new(AtomicU32::new(0)),
            output_meter: Rc::new(RefCell::new(FrameRateMeter::default())),
            on_error: Some(on_error),
            camera_rate,
            force_keyframe: Arc::new(AtomicBool::new(false)),
            capture: ActiveCapture::default(),
        }
    }

    /// Gets the current encoder output frame rate
    pub fn get_current_fps(&self) -> u32 {
        self.current_fps.load(Ordering::Relaxed)
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
        let switching = self.state.select(device_id);
        if switching {
            // Ending the old capture now; the new loop starts after the switch delay.
            self.capture.cancel();
        }
        switching
    }

    /// Stops encoding after it has been started.
    pub fn stop(&mut self) {
        self.state.stop();
        self.capture.cancel();
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
        let current_fps = self.current_fps.clone();
        let output_meter = self.output_meter.clone();
        let camera_rate = self.camera_rate.clone();
        let force_keyframe = self.force_keyframe.clone();
        let encoder_health = Rc::new(RefCell::new(EncoderHealth::default()));
        let video_output_handler = {
            let encoder_health = encoder_health.clone();
            let camera_rate = camera_rate.clone();
            let mut buffer: Vec<u8> = Vec::with_capacity(100_000);
            // Numbered here, at the hand-off to the transport: a frame the
            // rate control skipped never reaches this callback, and one the
            // send path refuses at once gives its number back. After a
            // refusal the viewers would see no gap, so the stream must
            // restart at a keyframe (see `CameraOutput`).
            let mut output = CameraOutput::default();
            let force_keyframe = force_keyframe.clone();

            Box::new(move |chunk: JsValue| {
                let chunk = web_sys::EncodedVideoChunk::from(chunk);
                encoder_health.borrow_mut().on_output(monotonic_now_ms());

                // One chunk per captured frame, measured like the viewers
                // measure what they receive.
                if let FrameEvent::WindowClosed { fps, .. } =
                    output_meter.borrow_mut().on_frame(js_sys::Date::now())
                {
                    current_fps.store(fps.round() as u32, Ordering::Relaxed);
                    log::debug!("Encoder output FPS: {fps:.1}");
                }

                // Ensure the backing buffer is large enough for this chunk
                let byte_length = chunk.byte_length() as usize;
                if buffer.len() < byte_length {
                    buffer.resize(byte_length, 0);
                }

                let is_keyframe = chunk.type_() == web_sys::EncodedVideoChunkType::Key;
                let now_ms = monotonic_now_ms();
                // Keyframe requests after a refusal are paced by the
                // feedback cycle (report period + RTT).
                let cycle_ms = camera_rate
                    .try_borrow()
                    .map_or(DIAGNOSTICS_REPORT_INTERVAL_MS as f64, |camera_rate| {
                        camera_rate.feedback_cycle_ms()
                    });
                let (outcome, request_keyframe) =
                    output.hand_off(is_keyframe, now_ms, cycle_ms, |seq| {
                        let (packet, is_keyframe) = transform_video_chunk(
                            chunk,
                            seq,
                            buffer.as_mut_slice(),
                            &userid,
                            aes.clone(),
                        );
                        client.send_media_packet(packet, ShapedKind::Camera, is_keyframe)
                    });
                if outcome == HandOff::Sent {
                    // Drives the quiet-viewer signal; skipped and held
                    // frames are not sent, so they do not count.
                    match camera_rate.try_borrow_mut() {
                        Ok(mut camera_rate) => {
                            camera_rate.on_frame_sent(now_ms);
                        }
                        Err(_) => log::debug!("camera rate control busy, sent frame not counted"),
                    }
                }
                if request_keyframe {
                    // The next frame the loop encodes is a keyframe; deltas
                    // already queued in the encoder are held until it.
                    force_keyframe.store(true, Ordering::Release);
                }
            })
        };
        let device_id = if let Some(vid) = &self.state.selected {
            vid.to_string()
        } else {
            return;
        };
        let on_error = self.on_error.clone();
        // Created before any await, so a stop() or switch during setup ends this start().
        let capture = self.capture.clone();
        let session = capture.begin();

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
            if session.is_cancelled() {
                return;
            }

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
                let exact = ConstrainDomStringParameters::new();
                exact.set_exact_str(&device_id);

                log::debug!("CameraEncoder: deviceId.exact = {}", device_id);
                media_info.set_device_id_constrain_dom_string_parameters(&exact);
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
            if session.is_cancelled() {
                stop_tracks(&device);
                return;
            }

            let video_track = Box::new(
                device
                    .get_video_tracks()
                    .find(&mut |_: JsValue, _: u32, _: Array| true)
                    .unchecked_into::<VideoTrack>(),
            );

            // Get track settings to get actual width, height and frame rate
            let media_track = video_track
                .as_ref()
                .clone()
                .unchecked_into::<MediaStreamTrack>();
            let track_settings = media_track.get_settings();
            camera_rate
                .borrow_mut()
                .set_camera_fps(track_settings.get_frame_rate());

            // The settings before the first frame. A first guess: iOS can
            // report the sensor orientation here, and the first frame
            // corrects it.
            let start = camera_rate.borrow().stats(monotonic_now_ms());
            let tier_cap = FrameSize::new(start.tier_width, start.tier_height);
            let start_size = FrameSize::of_settings(&track_settings).fit_within(tier_cap);
            let start_settings = CameraEncoderSettings {
                size: if start_size.is_empty() {
                    tier_cap
                } else {
                    start_size
                },
                bitrate_bps: start.encoder_bitrate_bps,
                framerate: start.tier_fps,
            };
            let constant_bitrate = constant_bitrate_supported(start_settings).await;
            if !constant_bitrate {
                log::warn!(
                    "CameraEncoder: constant bitrate mode not supported, using variable bitrate"
                );
            }
            if session.is_cancelled() {
                stop_tracks(&device);
                return;
            }
            // Tier resizes need a keyframe on VP8 only; see plan_reconfigure.
            let keyframe_on_tier_resize = get_video_codec() == VideoCodec::VP8;

            // Setup video encoder. An error closes the encoder; the loop
            // sees the flag and recreates it (see below).
            let encoder_failed = Rc::new(Cell::new(false));
            let video_error_handler = {
                let encoder_failed = encoder_failed.clone();
                Closure::wrap(Box::new(move |e: JsValue| {
                    log::warn!("CameraEncoder: encoder error: {e:?}");
                    encoder_failed.set(true);
                }) as Box<dyn FnMut(JsValue)>)
            };

            let video_output_handler =
                Closure::wrap(video_output_handler as Box<dyn FnMut(JsValue)>);

            let video_encoder_init = VideoEncoderInit::new(
                video_error_handler.as_ref().unchecked_ref(),
                video_output_handler.as_ref().unchecked_ref(),
            );

            let mut video_encoder = match VideoEncoder::new(&video_encoder_init) {
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

            let mut constant_bitrate = constant_bitrate;
            let mut encoder_settings = None;
            if !start_size.is_empty() {
                let settings = CameraEncoderSettings {
                    size: start_size,
                    ..start_settings
                };
                if configure_camera_encoder(&video_encoder, settings, &mut constant_bitrate) {
                    encoder_settings = Some(settings);
                    camera_rate
                        .borrow_mut()
                        .on_encoder_configured(monotonic_now_ms());
                }
            }

            // Safari and Firefox get this from the polyfill in dioxus-ui/static/capture/.
            let video_processor =
                MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(&media_track))
                    .unwrap();
            session.attach(
                video_processor
                    .readable()
                    .get_reader()
                    .unchecked_into::<ReadableStreamDefaultReader>(),
            );

            let mut keyframes = KeyframeClock::default();
            let mut previous_source = None;
            let mut recovered = false;
            loop {
                if session.is_cancelled()
                    || !enabled.load(Ordering::Acquire)
                    || switching.load(Ordering::Acquire)
                {
                    break;
                }
                let video_frame = match session.read().await {
                    FrameRead::Frame(frame) => frame,
                    FrameRead::Ended => {
                        report_unexpected_end(&session, &on_error, "Camera capture ended".into());
                        break;
                    }
                    FrameRead::Failed(e) => {
                        let message = format!("Camera capture failed: {e:?}");
                        report_unexpected_end(&session, &on_error, message);
                        break;
                    }
                };

                // A failed encoder is closed by the browser and viewers
                // would see a frozen camera. Recreate it in variable bitrate
                // mode (constant mode is the likeliest cause) and start
                // over with a keyframe. Only if that keeps failing is the
                // camera reported as failed, once: the UI turns the camera
                // off on that report.
                if encoder_failed.replace(false) {
                    let mut health = encoder_health.borrow_mut();
                    let failures = health.failures() + 1;
                    if health.on_failure() == Recovery::GiveUp {
                        video_frame.close();
                        let message =
                            format!("Camera encoder failed {failures} times in a row, giving up");
                        report_unexpected_end(&session, &on_error, message);
                        break;
                    }
                    drop(health);
                    log::warn!(
                        "CameraEncoder: recreating the encoder in variable bitrate mode \
                         (recovery {failures} of {MAX_ENCODER_RECOVERIES})"
                    );
                    constant_bitrate = false;
                    match VideoEncoder::new(&video_encoder_init) {
                        Ok(encoder) => *video_encoder = encoder,
                        Err(e) => {
                            video_frame.close();
                            let message = format!("Failed to recreate video encoder: {e:?}");
                            report_unexpected_end(&session, &on_error, message);
                            break;
                        }
                    }
                    encoder_settings = None;
                    recovered = true;
                }

                // The rate control decides per captured frame, on the
                // frame's own capture time (µs). A skipped frame never
                // reaches the encoder, so it takes no sequence number.
                let timestamp_ms = video_frame.timestamp() / 1000.0;
                let plan = camera_rate
                    .borrow_mut()
                    .on_camera_frame(monotonic_now_ms(), timestamp_ms);
                if !plan.encode {
                    video_frame.close();
                    continue;
                }

                let source = FrameSize::of(&video_frame);
                let wanted = Wanted {
                    settings: CameraEncoderSettings {
                        size: source.fit_within(FrameSize::new(plan.width, plan.height)),
                        bitrate_bps: plan.bitrate_bps,
                        framerate: plan.fps,
                    },
                    probe_fraction: plan.probe_fraction,
                    draining: plan.draining,
                };
                let reconfigure = plan_reconfigure(
                    encoder_settings,
                    wanted,
                    previous_source,
                    source,
                    keyframe_on_tier_resize,
                );
                previous_source = Some(source);
                if reconfigure.configure {
                    let settings = wanted.settings;
                    log::debug!(
                        "CameraEncoder: {}x{} @ {} fps, {} bps",
                        settings.size.width,
                        settings.size.height,
                        settings.framerate,
                        settings.bitrate_bps
                    );
                    if configure_camera_encoder(&video_encoder, settings, &mut constant_bitrate) {
                        encoder_settings = Some(settings);
                        camera_rate
                            .borrow_mut()
                            .on_encoder_configured(monotonic_now_ms());
                    } else if configure_failure_counts(encoder_settings.is_some()) {
                        // Never configured (a recreated encoder): it would
                        // refuse every frame and no error callback fires.
                        // The next frame recreates it, or gives up.
                        error!("CameraEncoder: configure failed on an unconfigured encoder");
                        encoder_failed.set(true);
                        video_frame.close();
                        continue;
                    }
                    // Otherwise the encoder keeps its previous config and
                    // goes on encoding; the next frame tries again.
                }

                let pli = force_keyframe.swap(false, Ordering::AcqRel);
                if pli {
                    log::info!("CameraEncoder: forcing keyframe (PLI)");
                }
                let forced = pli || reconfigure.keyframe || std::mem::take(&mut recovered);
                let options = VideoEncoderEncodeOptions::new();
                options.set_key_frame(keyframes.is_keyframe(timestamp_ms, forced));
                if let Err(e) = video_encoder.encode_with_options(&video_frame, &options) {
                    // Also what a closed encoder says until the error
                    // callback has fired: not worth a log line per frame.
                    log::debug!("Error encoding video frame: {e:?}");
                }
                video_frame.close();
            }

            switching.store(false, Ordering::Release);
            session.cancel();
            capture.clear_if(&session);
            media_track.stop();
            log::info!("CameraEncoder: stopped");
            if let Err(e) = video_encoder.close() {
                error!("Error closing video encoder: {e:?}");
            }
        });
    }
}

/// Reports a capture end that neither stop() nor a device switch asked for.
fn report_unexpected_end(
    session: &CaptureSession,
    on_error: &Option<Callback<String>>,
    message: String,
) {
    if !session.end_is_unexpected() {
        return;
    }
    error!("{message}");
    if let Some(callback) = on_error {
        callback.emit(message);
    }
}

fn stop_tracks(stream: &MediaStream) {
    for track in stream.get_tracks().iter() {
        track.unchecked_into::<MediaStreamTrack>().stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: FrameSize = FrameSize::new(1280, 720);
    /// The probe step at 30 fps and a 60 ms round trip: 33.3 / 560.
    const PROBE: f64 = 0.0595;

    fn settings(
        width: u32,
        height: u32,
        bitrate_bps: u32,
        framerate: f64,
    ) -> CameraEncoderSettings {
        CameraEncoderSettings {
            size: FrameSize::new(width, height),
            bitrate_bps,
            framerate,
        }
    }

    fn wanted(settings: CameraEncoderSettings, draining: bool) -> Wanted {
        Wanted {
            settings,
            probe_fraction: PROBE,
            draining,
        }
    }

    fn plan(current: CameraEncoderSettings, next: Wanted) -> Reconfigure {
        plan_reconfigure(Some(current), next, Some(SOURCE), SOURCE, false)
    }

    #[test]
    fn nothing_to_do_when_nothing_changed() {
        let current = settings(1280, 720, 1_000_000, 30.0);
        let result = plan(current, wanted(current, false));
        assert!(!result.configure && !result.keyframe);
    }

    /// A probe step up passes the threshold; a move under half a step (RTT
    /// jitter) does not.
    #[test]
    fn a_probe_step_reconfigures_and_jitter_does_not() {
        let current = settings(854, 480, 600_000, 30.0);
        let probe_up = settings(854, 480, 635_700, 30.0);
        let result = plan(current, wanted(probe_up, false));
        assert!(result.configure && !result.keyframe);
        let jitter = settings(854, 480, 610_000, 30.0);
        assert!(!plan(current, wanted(jitter, false)).configure);
        let small_down = settings(854, 480, 590_000, 30.0);
        assert!(!plan(current, wanted(small_down, false)).configure);
    }

    /// A move down that enters a drain applies at once, however small.
    #[test]
    fn a_drain_applies_at_once() {
        let current = settings(640, 360, 370_000, 30.0);
        let floor = settings(640, 360, 360_000, 30.0);
        assert!(!plan(current, wanted(floor, false)).configure);
        assert!(plan(current, wanted(floor, true)).configure);
    }

    #[test]
    fn a_tier_frame_rate_change_reconfigures_without_a_keyframe() {
        let current = settings(640, 360, 360_000, 30.0);
        let result = plan(current, wanted(settings(640, 360, 360_000, 15.0), false));
        assert!(result.configure);
        assert!(!result.keyframe);
    }

    /// A tier resize on VP9 needs no keyframe; on VP8 it does.
    #[test]
    fn a_tier_resize_forces_a_keyframe_only_on_vp8() {
        let current = settings(1280, 720, 1_000_000, 30.0);
        let next = wanted(settings(854, 480, 990_000, 30.0), false);
        let vp9 = plan_reconfigure(Some(current), next, Some(SOURCE), SOURCE, false);
        assert!(vp9.configure && !vp9.keyframe);
        let vp8 = plan_reconfigure(Some(current), next, Some(SOURCE), SOURCE, true);
        assert!(vp8.configure && vp8.keyframe);
    }

    /// The source changing size (first frame, iPhone rotation) still forces
    /// a keyframe, as before; so does a fresh encoder.
    #[test]
    fn a_source_resize_or_a_new_encoder_forces_a_keyframe() {
        let current = settings(1280, 720, 1_000_000, 30.0);
        let portrait = FrameSize::new(720, 1280);
        let next = wanted(settings(720, 1280, 1_000_000, 30.0), false);
        let rotated = plan_reconfigure(Some(current), next, Some(SOURCE), portrait, false);
        assert!(rotated.configure && rotated.keyframe);
        let first = plan_reconfigure(None, wanted(current, false), Some(SOURCE), SOURCE, false);
        assert!(first.configure && first.keyframe);
    }

    #[test]
    fn an_empty_size_is_never_configured() {
        let result = plan_reconfigure(
            None,
            wanted(settings(0, 0, 1_000_000, 30.0), false),
            None,
            FrameSize::default(),
            false,
        );
        assert!(!result.configure);
    }

    #[test]
    fn three_failures_recreate_and_the_fourth_gives_up() {
        let mut health = EncoderHealth::default();
        for _ in 0..3 {
            assert_eq!(health.on_failure(), Recovery::Recreate);
        }
        assert_eq!(health.on_failure(), Recovery::GiveUp);
    }

    /// Hours apart, with good output in between, failures never add up.
    #[test]
    fn a_healthy_stretch_forgets_old_failures() {
        let mut health = EncoderHealth::default();
        let mut at = 0.0;
        for _ in 0..10 {
            assert_eq!(health.on_failure(), Recovery::Recreate);
            health.on_output(at + 100.0);
            health.on_output(at + 6_000.0);
            at += 3_600_000.0;
        }
    }

    /// Output too soon after a failure, or no output at all, does not count
    /// as healthy.
    #[test]
    fn a_short_stretch_does_not_reset() {
        let mut health = EncoderHealth::default();
        for at in [0.0, 10_000.0, 20_000.0] {
            assert_eq!(health.on_failure(), Recovery::Recreate);
            health.on_output(at + 1_000.0);
        }
        assert_eq!(health.on_failure(), Recovery::GiveUp);
    }

    /// Per WebCodecs a configure that throws leaves a configured encoder on
    /// its previous config, still working: not a failure. A recreated
    /// encoder that was never configured is dead without one: a failure.
    #[test]
    fn a_refused_reconfigure_of_a_working_encoder_is_not_a_failure() {
        assert!(!configure_failure_counts(true));
        assert!(configure_failure_counts(false));
    }
}

/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Pre-join device-preview engine (issue #959).
//!
//! This module owns the *imperative* side of the pre-join preview: acquiring a
//! camera `MediaStream` for the live `<video>` preview, and wiring the selected
//! microphone through a Web Audio `AnalyserNode` to drive a live input-level
//! meter. It is deliberately separate from the RSX in
//! [`super::pre_join_settings_card`] so the UI stays declarative and the media
//! lifecycle (acquire / switch device / teardown) lives in one auditable place.
//!
//! ## Lifecycle contract (critical for a real-time media app)
//!
//! The preview holds *real* hardware: a camera capture and a mic capture. Both
//! MUST be released when the user leaves the pre-join screen or when the meeting
//! actually starts, otherwise the preview contends with the real in-meeting
//! capture (double-capture → black tile / "device in use" errors, and a camera
//! light that never turns off on mobile).
//!
//! [`PreviewEngine::shutdown`] stops every track on both streams and closes the
//! `AudioContext`. Call it:
//!   * from the component's `use_drop` (covers route changes / tab close), and
//!   * explicitly at the moment the meeting starts (the join handler), so the
//!     real encoders own the hardware uncontended.
//!
//! The RMS level itself is computed by the pure, host-testable
//! [`compute_rms_level`] so the meter math is covered by `cargo test` without a
//! DOM.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AnalyserNode, AudioContext, MediaStream, MediaStreamConstraints, MediaStreamTrack,
    MediaTrackConstraints,
};

/// Number of FFT bins requested from the `AnalyserNode`. 256 → 128 time-domain
/// samples per frame, which is plenty for a cheap RMS meter and keeps the
/// per-frame copy small on constrained devices.
const ANALYSER_FFT_SIZE: u32 = 256;

/// Shared cell holding the requestAnimationFrame callback. The loop reschedules
/// itself through this cell, so it must outlive each frame; emptying the cell is
/// how the meter stops.
type RafCell = Rc<RefCell<Option<Closure<dyn FnMut()>>>>;

/// Compute a normalized RMS (root-mean-square) level in `0.0..=1.0` from a
/// buffer of unsigned 8-bit time-domain samples as returned by
/// `AnalyserNode::get_byte_time_domain_data`.
///
/// Web Audio byte time-domain samples are centered at 128 (silence). We shift
/// to a signed `-1.0..=1.0` range, take the RMS, and clamp. This is the pure
/// core of the meter so it is unit-testable without any browser APIs.
///
/// Returns `0.0` for an empty buffer (no samples → no signal).
pub fn compute_rms_level(samples: &[u8]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sum_sq = 0.0f32;
    for &s in samples {
        // Map 0..=255 (center 128) to roughly -1.0..=1.0.
        let centered = (s as f32 - 128.0) / 128.0;
        sum_sq += centered * centered;
    }
    let rms = (sum_sq / samples.len() as f32).sqrt();
    rms.clamp(0.0, 1.0)
}

/// Attack-fast / release-slow peak smoothing for the meter.
///
/// `prev` is the previously displayed level, `target` the new raw RMS. When the
/// signal rises we jump (almost) straight to it so consonant transients are not
/// missed; when it falls we ease down by `release` per frame so the bar does not
/// strobe. Pure + host-tested. Returns the new displayed level in `0.0..=1.0`.
pub fn smooth_meter_level(prev: f32, target: f32, release: f32) -> f32 {
    let prev = prev.clamp(0.0, 1.0);
    let target = target.clamp(0.0, 1.0);
    if target >= prev {
        target
    } else {
        (prev - release).max(target)
    }
}

/// Convert a `0.0..=1.0` level into the percentage width the meter fill uses.
///
/// Applies the display gain (so a normal speaking voice fills a useful range)
/// and clamps to `0.0..=100.0`. Pure + host-tested.
pub fn level_to_pct(level: f32) -> f32 {
    (level * 140.0).clamp(0.0, 100.0)
}

/// Whether enough time has elapsed since the last ARIA update to publish a new
/// one. The visual fill updates every frame (cheap DOM write), but the
/// accessibility value is throttled to ~15fps so screen readers are not
/// flooded. Pure + host-tested.
pub fn should_update_aria(now_ms: f64, last_ms: f64) -> bool {
    const ARIA_MIN_INTERVAL_MS: f64 = 66.0; // ~15fps
    now_ms - last_ms >= ARIA_MIN_INTERVAL_MS
}

/// Human-readable ARIA value text for a given level + mute state. Pure +
/// host-tested.
pub fn meter_value_text(mic_on: bool, level: f32) -> &'static str {
    if !mic_on {
        return "Microphone muted";
    }
    let level = level.clamp(0.0, 1.0);
    if level < 0.02 {
        "No input detected"
    } else if level < 0.25 {
        "Low input level"
    } else if level < 0.6 {
        "Good input level"
    } else {
        "High input level"
    }
}

/// Build the `getUserMedia` video constraints for a specific camera device.
///
/// When `device_id` is empty we fall back to `video: true` (browser default
/// camera) so the very first preview still works before the user has made an
/// explicit choice.
fn video_constraints(device_id: &str) -> MediaStreamConstraints {
    let constraints = MediaStreamConstraints::new();
    if device_id.is_empty() {
        constraints.set_video(&JsValue::TRUE);
    } else {
        let track = MediaTrackConstraints::new();
        // `exact` would hard-fail if the device vanished; the pre-join screen
        // prefers a graceful fallback, so use the plain (ideal) deviceId hint.
        js_sys::Reflect::set(&track, &JsValue::from_str("deviceId"), &device_id.into()).ok();
        constraints.set_video(&track.into());
    }
    constraints
}

/// Build the `getUserMedia` audio constraints for a specific mic device, with
/// the same processing hints the live mic uses (echo cancellation etc.) so the
/// meter reflects what peers will actually hear.
fn audio_constraints(device_id: &str) -> MediaStreamConstraints {
    let constraints = MediaStreamConstraints::new();
    let track = MediaTrackConstraints::new();
    track.set_echo_cancellation(&JsValue::TRUE);
    track.set_noise_suppression(&JsValue::TRUE);
    track.set_auto_gain_control(&JsValue::TRUE);
    if !device_id.is_empty() {
        js_sys::Reflect::set(&track, &JsValue::from_str("deviceId"), &device_id.into()).ok();
    }
    constraints.set_audio(&track.into());
    constraints
}

/// Stop every track on a stream so the browser releases the hardware (camera
/// light / mic indicator turn off). Safe to call on `None`.
fn stop_stream(stream: &Option<MediaStream>) {
    if let Some(stream) = stream {
        for track in stream.get_tracks().iter() {
            let track: MediaStreamTrack = track.unchecked_into();
            track.stop();
        }
    }
}

/// Attach a stream to the preview `<video>` element (by id) and start playback,
/// mirroring the `attach_screen_preview` pattern in `host.rs`.
fn attach_video(video_id: &str, stream: &MediaStream) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(video_id))
    {
        let video: web_sys::HtmlVideoElement = el.unchecked_into();
        // Muted + playsinline so autoplay policy allows play() and mobile does
        // not go fullscreen.
        video.set_muted(true);
        video.set_src_object(Some(stream));
        let video_for_play = video.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match video_for_play.play() {
                Ok(promise) => {
                    if let Err(e) = JsFuture::from(promise).await {
                        log::warn!("Pre-join camera preview play() rejected: {e:?}");
                    }
                }
                Err(e) => log::warn!("Pre-join camera preview play() error: {e:?}"),
            }
        });
    }
}

/// Blank the preview `<video>` element (clear its srcObject).
fn detach_video(video_id: &str) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(video_id))
    {
        let video: web_sys::HtmlVideoElement = el.unchecked_into();
        video.set_src_object(None);
    }
}

/// Current high-resolution timestamp (ms). Falls back to 0.0 when unavailable.
fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}

/// Write `style.width = "<pct>%"` to an element by id (per-frame DOM write).
fn set_element_width_pct(element_id: &str, pct: f32) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(element_id))
    {
        if let Ok(html) = el.dyn_into::<web_sys::HtmlElement>() {
            let _ = html.style().set_property("width", &format!("{pct}%"));
        }
    }
}

/// Update the meter container's ARIA value (throttled by the caller).
fn set_meter_aria(meter_id: &str, pct: f32, level: f32) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(meter_id))
    {
        let _ = el.set_attribute("aria-valuenow", &format!("{}", pct as u32));
        let _ = el.set_attribute("aria-valuetext", meter_value_text(true, level));
    }
}

/// Reset the meter DOM to its muted/idle state (fill collapsed, ARIA muted).
fn reset_meter_dom(meter_id: &str, fill_id: &str) {
    set_element_width_pct(fill_id, 0.0);
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(meter_id))
    {
        let _ = el.set_attribute("aria-valuenow", "0");
        let _ = el.set_attribute("aria-valuetext", meter_value_text(false, 0.0));
    }
}

/// Inner mutable state of the preview engine, shared via `Rc<RefCell<…>>` so the
/// requestAnimationFrame closure and the component callbacks can both touch it.
struct PreviewInner {
    video_element_id: String,
    /// Id of the meter container element (`role="meter"`) — ARIA target.
    meter_element_id: String,
    /// Id of the inner fill element — per-frame `style.width` target.
    meter_fill_element_id: String,
    camera_stream: Option<MediaStream>,
    mic_stream: Option<MediaStream>,
    audio_ctx: Option<AudioContext>,
    analyser: Option<AnalyserNode>,
    /// Kept alive so the browser does not drop the rAF callback. Held as the
    /// shared `Rc<RefCell<Option<Closure>>>` cell the loop reschedules through,
    /// so the closure can re-arm itself each frame (taking the bare `Closure`
    /// out would leave the loop's self-reference `None` after one frame).
    raf_closure: Option<RafCell>,
    raf_handle: Option<i32>,
    /// Monotonic generation counters, one per acquire path. Each `start_*`
    /// captures the current value before awaiting `getUserMedia`; if a
    /// `stop_*` / `shutdown` / re-`start_*` bumped it in the meantime, the
    /// late stream is released instead of stored. This closes the in-flight
    /// gUM race where toggling OFF (or switching device) before the promise
    /// resolves would otherwise leave the camera light / mic indicator on.
    camera_gen: u64,
    mic_gen: u64,
}

/// Imperative engine that owns the camera + mic preview hardware.
///
/// Cheap to `clone()` (shares one `Rc<RefCell<…>>`). The component holds one
/// instance for its lifetime and calls the methods below in response to user
/// actions (toggle camera, switch device) and on teardown.
#[derive(Clone)]
pub struct PreviewEngine {
    inner: Rc<RefCell<PreviewInner>>,
}

impl PreviewEngine {
    /// Create an engine bound to the preview `<video>` element and the meter
    /// container + fill element ids. The meter is driven by direct DOM writes
    /// (no Dioxus signal), so it never re-renders the surrounding card.
    pub fn new(
        video_element_id: impl Into<String>,
        meter_element_id: impl Into<String>,
        meter_fill_element_id: impl Into<String>,
    ) -> Self {
        Self {
            inner: Rc::new(RefCell::new(PreviewInner {
                video_element_id: video_element_id.into(),
                meter_element_id: meter_element_id.into(),
                meter_fill_element_id: meter_fill_element_id.into(),
                camera_stream: None,
                mic_stream: None,
                audio_ctx: None,
                analyser: None,
                raf_closure: None,
                raf_handle: None,
                camera_gen: 0,
                mic_gen: 0,
            })),
        }
    }

    /// Acquire (or re-acquire) the camera with the given device id and show it
    /// in the preview `<video>`. Stops any previously-running camera stream
    /// first so we never leak tracks when switching devices.
    pub fn start_camera(&self, device_id: String) {
        // Stop the old camera and bump the generation so any still-in-flight
        // acquire from a previous call is invalidated.
        let my_gen = {
            let mut inner = self.inner.borrow_mut();
            stop_stream(&inner.camera_stream);
            inner.camera_stream = None;
            inner.camera_gen = inner.camera_gen.wrapping_add(1);
            inner.camera_gen
        };
        let inner_rc = self.inner.clone();
        let constraints = video_constraints(&device_id);
        wasm_bindgen_futures::spawn_local(async move {
            let Some(media_devices) = web_sys::window()
                .map(|w| w.navigator())
                .and_then(|n| n.media_devices().ok())
            else {
                log::warn!("Pre-join: navigator.mediaDevices unavailable");
                return;
            };
            let promise = match media_devices.get_user_media_with_constraints(&constraints) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("Pre-join camera getUserMedia error: {e:?}");
                    return;
                }
            };
            match JsFuture::from(promise).await {
                Ok(value) => {
                    let stream: MediaStream = value.unchecked_into();
                    let mut inner = inner_rc.borrow_mut();
                    // If a stop/shutdown/re-start raced this acquire, the
                    // generation no longer matches — release the late stream so
                    // the camera light does not stay on.
                    if inner.camera_gen != my_gen {
                        drop(inner);
                        stop_stream(&Some(stream));
                        return;
                    }
                    attach_video(&inner.video_element_id, &stream);
                    inner.camera_stream = Some(stream);
                }
                Err(e) => log::warn!("Pre-join camera getUserMedia rejected: {e:?}"),
            }
        });
    }

    /// Stop the camera preview and blank the `<video>` (camera toggled OFF).
    pub fn stop_camera(&self) {
        let mut inner = self.inner.borrow_mut();
        stop_stream(&inner.camera_stream);
        inner.camera_stream = None;
        // Invalidate any in-flight camera acquire.
        inner.camera_gen = inner.camera_gen.wrapping_add(1);
        detach_video(&inner.video_element_id);
    }

    /// Acquire (or re-acquire) the mic with the given device id and start the
    /// live level meter. Stops any previous mic stream + analyser first.
    pub fn start_mic_meter(&self, device_id: String) {
        // Tear down the previous mic path so devices/contexts are not leaked.
        // `stop_mic_meter` bumps `mic_gen`, invalidating any prior in-flight
        // acquire; we then read the fresh generation for THIS acquire.
        self.stop_mic_meter();
        let my_gen = self.inner.borrow().mic_gen;

        // Create AND resume the AudioContext SYNCHRONOUSLY, inside the user
        // gesture that triggered this call (the mic toggle / device select).
        // Strict-autoplay browsers (Chrome) only honor resume() when it runs
        // with an active user-activation; doing it later inside the post-gUM
        // async chain can leave the context suspended → a permanently zero
        // meter. (code-review item 10)
        let ctx = match AudioContext::new() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("Pre-join AudioContext::new failed: {e:?}");
                return;
            }
        };
        // Fire-and-forget resume on the gesture; the await below is a no-op if
        // it already settled.
        let resume_promise = ctx.resume().ok();

        let inner_rc = self.inner.clone();
        let constraints = audio_constraints(&device_id);
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(resume) = resume_promise {
                let _ = JsFuture::from(resume).await;
            }
            let Some(media_devices) = web_sys::window()
                .map(|w| w.navigator())
                .and_then(|n| n.media_devices().ok())
            else {
                let _ = ctx.close();
                return;
            };
            let promise = match media_devices.get_user_media_with_constraints(&constraints) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("Pre-join mic getUserMedia error: {e:?}");
                    let _ = ctx.close();
                    return;
                }
            };
            let stream: MediaStream = match JsFuture::from(promise).await {
                Ok(value) => value.unchecked_into(),
                Err(e) => {
                    log::warn!("Pre-join mic getUserMedia rejected: {e:?}");
                    let _ = ctx.close();
                    return;
                }
            };

            // A stop/shutdown/re-start raced the gUM await — drop the stream
            // and the context before doing any further work so the mic
            // indicator clears.
            if inner_rc.borrow().mic_gen != my_gen {
                let _ = ctx.close();
                stop_stream(&Some(stream));
                return;
            }

            let source = match ctx.create_media_stream_source(&stream) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("Pre-join createMediaStreamSource failed: {e:?}");
                    let _ = ctx.close();
                    stop_stream(&Some(stream));
                    return;
                }
            };
            let analyser = match ctx.create_analyser() {
                Ok(a) => a,
                Err(e) => {
                    log::warn!("Pre-join createAnalyser failed: {e:?}");
                    let _ = ctx.close();
                    stop_stream(&Some(stream));
                    return;
                }
            };
            analyser.set_fft_size(ANALYSER_FFT_SIZE);
            if let Err(e) = source.connect_with_audio_node(&analyser) {
                log::warn!("Pre-join analyser connect failed: {e:?}");
                let _ = ctx.close();
                stop_stream(&Some(stream));
                return;
            }

            {
                let mut inner = inner_rc.borrow_mut();
                // Teardown / re-start raced this acquire (e.g. the resume()
                // await above) — release everything now.
                if inner.mic_gen != my_gen {
                    drop(inner);
                    let _ = ctx.close();
                    stop_stream(&Some(stream));
                    return;
                }
                inner.mic_stream = Some(stream);
                inner.audio_ctx = Some(ctx);
                inner.analyser = Some(analyser);
            }

            // Start the rAF meter loop.
            PreviewEngine::spawn_meter_loop(inner_rc);
        });
    }

    /// Drive the level meter via requestAnimationFrame until the analyser is
    /// torn down. Writes the fill width directly to the DOM every frame (no
    /// Dioxus signal → no per-frame card re-diff) and throttles the ARIA value
    /// to ~15fps. Stores the closure + handle so cleanup can cancel it.
    fn spawn_meter_loop(inner_rc: Rc<RefCell<PreviewInner>>) {
        let (bin_count, fill_id, meter_id) = {
            let inner = inner_rc.borrow();
            let bins = inner
                .analyser
                .as_ref()
                .map(|a| a.frequency_bin_count())
                .unwrap_or(0);
            (
                bins,
                inner.meter_fill_element_id.clone(),
                inner.meter_element_id.clone(),
            )
        };
        if bin_count == 0 {
            return;
        }

        let raf_state = inner_rc.clone();
        // Use a self-referential Rc<RefCell<Option<Closure>>> so the closure can
        // reschedule itself.
        let cb: RafCell = Rc::new(RefCell::new(None));
        let cb_for_loop = cb.clone();

        let mut buffer = vec![0u8; bin_count as usize];
        // Per-loop display state: smoothed level + last ARIA-publish timestamp.
        let mut displayed = 0.0f32;
        let mut last_aria_ms = 0.0f64;
        // Release-per-frame for the attack-fast/release-slow smoothing.
        const METER_RELEASE_PER_FRAME: f32 = 0.06;
        let tick = Closure::wrap(Box::new(move || {
            // Read the analyser (if still alive) and compute the level.
            let still_running = {
                let inner = raf_state.borrow();
                inner.analyser.as_ref().map(|analyser| {
                    analyser.get_byte_time_domain_data(&mut buffer);
                    compute_rms_level(&buffer)
                })
            };
            let Some(raw) = still_running else {
                return;
            };

            // Smooth, then write the fill width straight to the DOM.
            displayed = smooth_meter_level(displayed, raw, METER_RELEASE_PER_FRAME);
            let pct = level_to_pct(displayed);
            set_element_width_pct(&fill_id, pct);

            // Throttle the ARIA value so screen readers are not flooded.
            let now = now_ms();
            if should_update_aria(now, last_aria_ms) {
                last_aria_ms = now;
                set_meter_aria(&meter_id, pct, displayed);
            }

            // Reschedule.
            if let Some(window) = web_sys::window() {
                if let Some(closure) = cb_for_loop.borrow().as_ref() {
                    let handle = window
                        .request_animation_frame(closure.as_ref().unchecked_ref())
                        .unwrap_or(0);
                    raf_state.borrow_mut().raf_handle = Some(handle);
                }
            }
        }) as Box<dyn FnMut()>);

        *cb.borrow_mut() = Some(tick);

        // Kick off the first frame.
        if let Some(window) = web_sys::window() {
            let handle = {
                let borrow = cb.borrow();
                borrow.as_ref().map(|closure| {
                    window
                        .request_animation_frame(closure.as_ref().unchecked_ref())
                        .unwrap_or(0)
                })
            };
            if let Some(handle) = handle {
                inner_rc.borrow_mut().raf_handle = Some(handle);
            }
        }
        // Stash the SHARED closure cell in the engine so it lives as long as the
        // meter does AND the loop's own `cb_for_loop` reference keeps pointing at
        // a live closure for rescheduling.
        inner_rc.borrow_mut().raf_closure = Some(cb);
    }

    /// Stop the mic meter: cancel the rAF loop, drop the analyser, stop the mic
    /// stream, and close the AudioContext.
    pub fn stop_mic_meter(&self) {
        let mut inner = self.inner.borrow_mut();
        // Invalidate any in-flight mic acquire.
        inner.mic_gen = inner.mic_gen.wrapping_add(1);
        if let (Some(handle), Some(window)) = (inner.raf_handle.take(), web_sys::window()) {
            let _ = window.cancel_animation_frame(handle);
        }
        // Break the closure↔cell reference cycle: empty the shared cell so the
        // Closure is dropped, then drop our handle to the cell. (The loop also
        // bails on its next tick because `analyser` is cleared below.)
        if let Some(cell) = inner.raf_closure.take() {
            *cell.borrow_mut() = None;
        }
        inner.analyser = None;
        stop_stream(&inner.mic_stream);
        inner.mic_stream = None;
        if let Some(ctx) = inner.audio_ctx.take() {
            let _ = ctx.close();
        }
        // Collapse the bar and mark the meter muted directly in the DOM (the
        // meter is signal-free, so there is no Dioxus state to reset).
        reset_meter_dom(&inner.meter_element_id, &inner.meter_fill_element_id);
    }

    /// Full teardown: release ALL preview hardware and close the AudioContext.
    ///
    /// Idempotent. Call on component unmount AND at meeting start so the real
    /// in-meeting capture owns the camera/mic uncontended (no double-capture).
    pub fn shutdown(&self) {
        // stop_mic_meter / stop_camera each bump their generation counter, so
        // any acquire still awaiting gUM will release its stream on resume.
        self.stop_mic_meter();
        self.stop_camera();
        log::info!("Pre-join PreviewEngine: shutdown complete (camera + mic released)");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        compute_rms_level, level_to_pct, meter_value_text, should_update_aria, smooth_meter_level,
    };

    #[test]
    fn rms_of_empty_is_zero() {
        assert_eq!(compute_rms_level(&[]), 0.0);
    }

    #[test]
    fn rms_of_silence_is_zero() {
        // Web Audio silence is centered at 128.
        let silence = [128u8; 128];
        assert_eq!(compute_rms_level(&silence), 0.0);
    }

    #[test]
    fn rms_of_full_scale_square_wave_is_near_one() {
        // Alternating 0 / 255. With the byte-centering math, 0 maps to -1.0 and
        // 255 maps to 127/128 ≈ 0.992, so the RMS lands just under 1.0 — the
        // theoretical max for a full-scale 8-bit time-domain signal.
        let mut buf = [0u8; 128];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = if i % 2 == 0 { 0 } else { 255 };
        }
        let level = compute_rms_level(&buf);
        assert!(level > 0.99, "expected near-full-scale, got {level}");
        assert!(level <= 1.0, "must be clamped to 1.0, got {level}");
    }

    #[test]
    fn rms_of_constant_offset_is_proportional() {
        // All samples at 192 → centered 0.5 → RMS = 0.5.
        let buf = [192u8; 64];
        let level = compute_rms_level(&buf);
        assert!((level - 0.5).abs() < 1e-3, "expected ~0.5, got {level}");
    }

    #[test]
    fn rms_is_clamped_to_unit_range() {
        // 255 centered = 127/128 ≈ 0.992, never exceeds 1.0.
        let buf = [255u8; 16];
        let level = compute_rms_level(&buf);
        assert!((0.0..=1.0).contains(&level));
    }

    // ── Meter smoothing / display helpers ──────────────────────────────

    #[test]
    fn smooth_attack_is_instant() {
        // Rising signal jumps straight to the target (attack-fast).
        assert_eq!(smooth_meter_level(0.1, 0.9, 0.06), 0.9);
    }

    #[test]
    fn smooth_release_is_gradual() {
        // Falling signal eases down by at most `release` per frame.
        let next = smooth_meter_level(0.9, 0.0, 0.06);
        assert!((next - 0.84).abs() < 1e-6, "expected 0.84, got {next}");
    }

    #[test]
    fn smooth_release_floors_at_target() {
        // Release never undershoots the target.
        assert_eq!(smooth_meter_level(0.62, 0.6, 0.06), 0.6);
    }

    #[test]
    fn smooth_clamps_inputs() {
        assert_eq!(smooth_meter_level(2.0, 5.0, 0.06), 1.0);
        assert_eq!(smooth_meter_level(-1.0, -1.0, 0.06), 0.0);
    }

    #[test]
    fn level_to_pct_applies_gain_and_clamps() {
        assert_eq!(level_to_pct(0.0), 0.0);
        // 0.5 * 140 = 70.
        assert!((level_to_pct(0.5) - 70.0).abs() < 1e-3);
        // 0.8 * 140 = 112 → clamped to 100.
        assert_eq!(level_to_pct(0.8), 100.0);
        assert_eq!(level_to_pct(1.0), 100.0);
    }

    #[test]
    fn aria_throttle_respects_interval() {
        // ~15fps → 66ms minimum interval.
        assert!(!should_update_aria(1000.0, 1000.0));
        assert!(!should_update_aria(1050.0, 1000.0));
        assert!(should_update_aria(1066.0, 1000.0));
        assert!(should_update_aria(2000.0, 1000.0));
    }

    #[test]
    fn meter_text_reports_muted_when_off() {
        assert_eq!(meter_value_text(false, 0.9), "Microphone muted");
    }

    #[test]
    fn meter_text_buckets_levels_when_on() {
        assert_eq!(meter_value_text(true, 0.0), "No input detected");
        assert_eq!(meter_value_text(true, 0.1), "Low input level");
        assert_eq!(meter_value_text(true, 0.4), "Good input level");
        assert_eq!(meter_value_text(true, 0.9), "High input level");
    }
}

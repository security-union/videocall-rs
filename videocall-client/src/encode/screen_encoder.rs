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

use crate::connection::MediaStreamKey;
use gloo_timers::future::sleep;
use gloo_utils::window;
use js_sys::Array;
use js_sys::JsString;
use js_sys::Reflect;
use log::error;
use log::info;
use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
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
use super::classify_encode_error::{
    classify_encode_error, restart_reason_from_message, EncodeErrorBucket, RestartReason,
};
use super::dimensions::{resolve_capture_dimensions, settings_source_stamp};
use super::encoder_state::{
    keyframe_tick_decision, periodic_keyframe_due, EncoderState, KeyframeTickInput,
};
use super::transform::transform_screen_chunk;
use super::AqControlLoopCancel;
use crate::crypto::aes::Aes128State;

use crate::adaptive_quality_constants::{
    simulcast_screen_layers, DEFAULT_SCREEN_TIER_INDEX, ENCODER_PLI_COOLDOWN_MS,
    SCREEN_MAX_ENCODE_HEIGHT, SCREEN_MAX_ENCODE_WIDTH, SCREEN_MIN_BITRATE_KBPS,
    SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS, SCREEN_QUALITY_TIERS, SCREEN_TARGET_FPS,
};
use crate::constants::get_video_codec_string;
// Reuse the SEND-side simulcast diagnostics types defined alongside the camera
// encoder (issue #1095 observability) so screen + camera share one shape.
use crate::diagnostics::adaptive_quality_manager::TierTransitionRecord;
use crate::diagnostics::EncoderBitrateController;
use crate::encode::camera_encoder::SimulcastSendSnapshot;
use videocall_aq::screen_bitrate::{
    ScreenBaselineKbps, ScreenTargetKbps, ScreenUplinkGovernor, ScreenUplinkSample,
    SCREEN_UPLINK_PRESSURE_MS,
};
use videocall_aq::{
    capture_exceeds_encode_ceiling, fit_within_tier_box, orient_box_to_source,
    screen_encode_box_for_capture, simulcast_layer_target_dims,
};

/// Upper bound on SCREEN layers, tied to the AQ ladder so the two cannot drift.
const SCREEN_SIMULCAST_MAX_SUPPORTED_LAYERS: u32 =
    crate::adaptive_quality_constants::SCREEN_SIMULCAST_MAX_LAYERS as u32;

/// Clamp a requested screen `max_layers` to the supported range. `0` (meaningless
/// — there is always the base layer) becomes 1. Free function so it is
/// unit-testable without a live `ScreenEncoder`.
fn clamp_screen_layer_count(max_layers: u32) -> u32 {
    max_layers.clamp(1, SCREEN_SIMULCAST_MAX_SUPPORTED_LAYERS)
}

/// Capture ceiling requested from `getDisplayMedia` (issue 1973), as both `max`
/// and `ideal`. The same box the encoder fits into, so source dims equal config
/// dims and WebCodecs never software-rescales a frame on the codec queue.
const SCREEN_CAPTURE_MAX_WIDTH: f64 = SCREEN_QUALITY_TIERS[0].max_width as f64;
const SCREEN_CAPTURE_MAX_HEIGHT: f64 = SCREEN_QUALITY_TIERS[0].max_height as f64;

/// Publish the configured geometry and return its pixel-rate budget.
fn publish_screen_encode_geometry(
    encode_w_out: &AtomicU32,
    encode_h_out: &AtomicU32,
    width: u32,
    height: u32,
    fps: u32,
) -> ScreenBaselineKbps {
    encode_w_out.store(width, Ordering::Relaxed);
    encode_h_out.store(height, Ordering::Relaxed);
    ScreenBaselineKbps::for_geometry(width, height, fps)
}

/// What the screen `VideoEncoder` is configured at, kbps. A distinct TYPE whose
/// only inhabitant comes from `screen_effective_bitrate_kbps`.
#[derive(Debug)]
pub(crate) struct ScreenEffectiveBitrate(AtomicU32);

impl ScreenEffectiveBitrate {
    /// Pre-share placeholder, clamped so a caller cannot seed a tier constant.
    fn seed_from_ceiling(kbps: u32) -> Self {
        let ceiling = ScreenBaselineKbps::for_geometry(
            SCREEN_MAX_ENCODE_WIDTH,
            SCREEN_MAX_ENCODE_HEIGHT,
            SCREEN_TARGET_FPS,
        )
        .kbps();
        Self(AtomicU32::new(kbps.clamp(SCREEN_MIN_BITRATE_KBPS, ceiling)))
    }

    fn store(&self, target: ScreenTargetKbps) {
        self.0.store(target.kbps(), Ordering::Relaxed);
    }

    #[cfg(test)]
    fn kbps(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Publish one governed target to BOTH the encoder's cell and the telemetry /
/// wire mirror, so neither can disagree with what the encoder is configured at.
fn publish_screen_effective_bitrate(
    effective_out: &ScreenEffectiveBitrate,
    telemetry_out: &AtomicU32,
    target: ScreenTargetKbps,
) {
    effective_out.store(target);
    telemetry_out.store(target.kbps(), Ordering::Relaxed);
}
/// Preferred capture framerate hint (unchanged; screen share targets ≤10fps).
const SCREEN_CAPTURE_IDEAL_FPS: f64 = 10.0;

/// Ordered `(key, value)` entries applied to each `MediaTrackConstraints`
/// sub-dictionary (`width`, `height`, `frameRate`) of the `getDisplayMedia`
/// video request. Pure + host-testable so the issue-1973 resolution ceiling can
/// be pinned by a unit test without a live browser; the wasm builder
/// [`build_screen_display_constraints`] writes exactly these entries onto the JS
/// constraint object, so the test guards what is actually sent to the browser.
struct ScreenCaptureConstraintSpec {
    width: Vec<(&'static str, f64)>,
    height: Vec<(&'static str, f64)>,
    framerate: Vec<(&'static str, f64)>,
}

/// Build the screen-capture constraint spec.
///
/// `include_ceiling` selects whether the issue-1973 resolution ceiling (`max`)
/// is requested. It is `true` on the normal path; the acquire helper retries
/// with `false` — the pre-issue-1973 `ideal`-only request — once if a browser
/// rejects the ceiling with `OverconstrainedError`.
///
/// # Why `max` (issue 1973)
/// `ideal` is only a hint, so the browser may still deliver native resolution.
/// Per the MediaTrackConstraints spec `max` is a mandatory upper bound in the
/// SelectSettings fitness algorithm (settings above it are excluded from the
/// candidate set), so the browser bounds resolution at CAPTURE time (hardware
/// compositor path) before frames reach the encoder. Aspect ratio is preserved
/// by fitting within the ceiling: a 3840x1600 (21:9) source is delivered at
/// 2560x1066, not letterboxed or cropped. `fit_within_preserving_aspect` is
/// unchanged: it still bounds the encoder CONFIG dims (defensive on engines that
/// under-honor `max`), but only the capture ceiling avoids the per-frame rescale.
fn screen_capture_constraint_spec(include_ceiling: bool) -> ScreenCaptureConstraintSpec {
    let mut width = Vec::with_capacity(2);
    let mut height = Vec::with_capacity(2);
    if include_ceiling {
        width.push(("max", SCREEN_CAPTURE_MAX_WIDTH));
        height.push(("max", SCREEN_CAPTURE_MAX_HEIGHT));
    }
    // `ideal` keeps native-resolution capture for sources already at/under the
    // ceiling (readable text/code), which is the pre-issue-1973 behavior.
    width.push(("ideal", SCREEN_CAPTURE_MAX_WIDTH));
    height.push(("ideal", SCREEN_CAPTURE_MAX_HEIGHT));
    ScreenCaptureConstraintSpec {
        width,
        height,
        framerate: vec![("ideal", SCREEN_CAPTURE_IDEAL_FPS)],
    }
}

/// Decide whether a rejected `getDisplayMedia` attempt should be retried once
/// without the issue-1973 resolution ceiling.
///
/// Only an `OverconstrainedError` on the FIRST attempt qualifies — that is the
/// error a browser raises when it cannot satisfy the `max` ceiling. A second
/// failure (`already_retried == true`) or any other error name (including
/// `NotAllowedError` user-cancel) returns `false`, letting the caller's normal
/// error path run. PUBLIC + host-testable so the encoder's acquire path AND the
/// UI's pre-acquire click handler share one retry policy.
pub fn should_retry_screen_capture_without_ceiling(
    error_name: &str,
    already_retried: bool,
) -> bool {
    !already_retried && error_name == "OverconstrainedError"
}

/// Construct the `getDisplayMedia` constraints from a [`ScreenCaptureConstraintSpec`].
///
/// Wasm-only (touches `js_sys`/`web_sys`); every value it writes comes from the
/// host-testable spec, so [`screen_capture_constraint_spec`]'s unit tests guard
/// exactly what reaches the browser.
fn build_screen_display_constraints(
    spec: &ScreenCaptureConstraintSpec,
) -> web_sys::DisplayMediaStreamConstraints {
    fn dim_object(entries: &[(&'static str, f64)]) -> js_sys::Object {
        let obj = js_sys::Object::new();
        for &(key, value) in entries {
            let _ = Reflect::set(&obj, &JsValue::from_str(key), &JsValue::from_f64(value));
        }
        obj
    }
    let video_constraints = js_sys::Object::new();
    let _ = Reflect::set(
        &video_constraints,
        &JsValue::from_str("width"),
        &dim_object(&spec.width).into(),
    );
    let _ = Reflect::set(
        &video_constraints,
        &JsValue::from_str("height"),
        &dim_object(&spec.height).into(),
    );
    let _ = Reflect::set(
        &video_constraints,
        &JsValue::from_str("frameRate"),
        &dim_object(&spec.framerate).into(),
    );

    let constraints = web_sys::DisplayMediaStreamConstraints::new();
    constraints.set_video(&video_constraints.into());
    constraints.set_audio(&JsValue::FALSE);
    constraints
}

/// Build the `getDisplayMedia` constraints for a screen share with (`true`) or
/// without (`false`) the issue-1973 resolution ceiling.
///
/// PUBLIC single source of truth for screen-capture constraints: the encoder's
/// own `start()` / re-acquire paths AND the UI's Safari-synchronous pre-acquire
/// click handler (which feeds [`ScreenEncoder::start_with_stream`]) all call
/// this, so every capture site requests the identical ceiling and can never
/// drift out of sync. Pass `include_ceiling = false` only for the one-shot
/// [`should_retry_screen_capture_without_ceiling`] fallback.
pub fn screen_capture_display_constraints(
    include_ceiling: bool,
) -> web_sys::DisplayMediaStreamConstraints {
    build_screen_display_constraints(&screen_capture_constraint_spec(include_ceiling))
}

/// Decide whether a screen-capture TRACK should be re-negotiated to the encode
/// ceiling (guarding issue 1973).
///
/// # When this fires
/// Only when the capture was acquired WITHOUT the ceiling being requested — the
/// `OverconstrainedError` fallback, or a stream the UI pre-acquired whose
/// request this encoder cannot inspect. On the normal path `getDisplayMedia`
/// already carries the ceiling as `max`, so the surface arrives pre-capped and
/// the "nothing binds" guard below suppresses the call.
///
/// A surface LARGER than the encode box would otherwise make WebCodecs
/// software-rescale every frame on the codec queue — the configuration that
/// stalled the encoder for 60-142 s in issue 1973. Requesting the box as the
/// track's `max` moves that downscale into the capture pipeline instead.
///
/// # What it requests
/// The tier's own box, capped by the capture ceiling — so this can never widen
/// capture past what `getDisplayMedia` was originally allowed, and at the TOP
/// rung (whose box IS the ceiling) it naturally RELEASES any earlier
/// step-down constraint, letting the surface return to its native size once the
/// network recovers. Without that release a share would stay permanently soft
/// after a single transient congestion episode.
///
/// # Returns
/// `Some((w, h))` — the `max` width/height to request — or `None` when the call
/// would be pointless:
/// - the same box was already requested (`last_w`/`last_h`) → `None`, so a tier
///   change between two rungs that share a box (e.g. `medium` → `low`, both
///   1280x720) does not re-negotiate the track;
/// - neither the new box nor any outstanding one binds the known source → `None`;
///   the capture is already smaller than every box in play, so there is nothing
///   to shrink and nothing to release.
///
/// `last_w`/`last_h` of `(0, 0)` means **no constraint has been requested yet**
/// — the state after an acquisition whose ceiling was DROPPED by the
/// [`should_retry_screen_capture_without_ceiling`] fallback, and the state the
/// UI's own pre-acquired stream arrives in (the encoder cannot know what that
/// call asked for). Treating it as a live 0x0 request would be backwards: it
/// would make the "nothing binds" guard read `0 < source` = "the outgoing
/// request binds" and fire a pointless `applyConstraints` on every small-window
/// share. It is therefore read as "nothing outstanding", so only the INCOMING
/// box decides — which is exactly what lets the first genuine bounding attempt
/// through on a 5K panel whose ceiling was dropped.
///
/// With unknown source dims (`0` on either axis) the source guard is skipped and
/// the request is made — the constraint is a `max`, so it can only ever shrink a
/// source that is genuinely larger and is a no-op otherwise.
///
/// Pure + host-testable: the wasm caller
/// ([`apply_screen_track_resolution_constraint`]) only turns the returned pair
/// into a `MediaTrackConstraints` and fires `applyConstraints`.
#[allow(clippy::too_many_arguments)]
fn screen_track_constraint_for_tier(
    tier_w: u32,
    tier_h: u32,
    ceiling_w: u32,
    ceiling_h: u32,
    source_w: u32,
    source_h: u32,
    last_w: u32,
    last_h: u32,
) -> Option<(u32, u32)> {
    // Never ask for more than getDisplayMedia was allowed to give us.
    let want_w = tier_w.min(ceiling_w);
    let want_h = tier_h.min(ceiling_h);
    if want_w == 0 || want_h == 0 {
        return None;
    }
    if (want_w, want_h) == (last_w, last_h) {
        return None;
    }
    // Neither the new nor any OUTSTANDING request binds a known source: the
    // track is already smaller than both, so re-negotiating changes nothing.
    // `(0, 0)` = nothing outstanding (see the doc above), NOT a live 0x0 box.
    if source_w > 0 && source_h > 0 {
        let want_binds = want_w < source_w || want_h < source_h;
        let last_binds = last_w > 0 && last_h > 0 && (last_w < source_w || last_h < source_h);
        if !want_binds && !last_binds {
            return None;
        }
    }
    Some((want_w, want_h))
}

/// Tolerance (percent) by which a live capture may exceed a requested `max` box
/// before the engine is judged to have IGNORED the constraint.
///
/// Not zero: engines legitimately round to even/macroblock-aligned dimensions,
/// and a surface whose aspect forces a fractional fit can land a pixel or two
/// over. The smallest gap between two adjacent DISTINCT rung widths in
/// `SCREEN_QUALITY_TIERS` is 1920→2560 = +33%, so 5% is far too small to be
/// reached by a genuinely honoured constraint and far too small to hide a
/// genuinely ignored one (which leaves the track a whole rung or more too big).
const SCREEN_CONSTRAINT_TOLERANCE_PCT: u32 = 5;

/// Did the capture engine actually honour the box we asked for? (issue #2179
/// review.)
///
/// `applyConstraints` is specified to resolve only after the new settings are
/// applied, but an engine that silently declines still resolves the promise and
/// leaves the track at its old size. When that happens the encoder is configured
/// smaller than the source and WebCodecs software-rescales every frame on the
/// codec queue — and each further tier step-down DEEPENS that ratio (4K → 720p
/// is a 9x rescale), which is the issue #1973 stall spiral.
///
/// Returns `false` for any unknown dimension: an absent `getSettings()` pair is
/// not evidence of misbehaviour, and a false accusation would needlessly pin the
/// encode box large.
fn screen_constraint_was_ignored(req_w: u32, req_h: u32, live_w: u32, live_h: u32) -> bool {
    if req_w == 0 || req_h == 0 || live_w == 0 || live_h == 0 {
        return false;
    }
    let tol_w = req_w + req_w * SCREEN_CONSTRAINT_TOLERANCE_PCT / 100;
    let tol_h = req_h + req_h * SCREEN_CONSTRAINT_TOLERANCE_PCT / 100;
    live_w > tol_w || live_h > tol_h
}

/// The BASE-layer encode box to use once the engine has been observed to ignore
/// `applyConstraints` (issue #2179 review).
///
/// While the constraint is honoured, source dims track config dims and the tier
/// box is used as-is. Once it is NOT honoured, shrinking the encode config
/// further only grows the per-frame WebCodecs rescale ratio, so the box is
/// floored at the source's own size (itself capped by the capture ceiling, which
/// is the largest frame that can ever arrive). The tier still governs BITRATE
/// and fps, so a step-down keeps shedding bits — it just stops paying for a
/// deeper software rescale it cannot benefit from.
///
/// Unknown source dims, or a honoured constraint, return the tier box unchanged.
fn screen_encode_box_when_constraint_ignored(
    tier_w: u32,
    tier_h: u32,
    source_w: u32,
    source_h: u32,
    ceiling_w: u32,
    ceiling_h: u32,
    ignored: bool,
) -> (u32, u32) {
    if !ignored || source_w == 0 || source_h == 0 {
        return (tier_w, tier_h);
    }
    (
        tier_w.max(source_w.min(ceiling_w)),
        tier_h.max(source_h.min(ceiling_h)),
    )
}

/// The encode box, composed once for the three sites that need it. Widens to the
/// source when the capture engine declined our `applyConstraints` (#1973).
#[allow(clippy::too_many_arguments)]
fn screen_base_encode_box_for_source(
    tier_max_w: u32,
    tier_max_h: u32,
    source_w: u32,
    source_h: u32,
    ceiling_w: u32,
    ceiling_h: u32,
    constraint_ignored: bool,
) -> (u32, u32) {
    screen_encode_box_when_constraint_ignored(
        tier_max_w,
        tier_max_h,
        source_w,
        source_h,
        ceiling_w,
        ceiling_h,
        constraint_ignored,
    )
}

/// Apply a resolution ceiling to a live screen-capture track (issue 2179).
///
/// Fire-and-forget: `applyConstraints` returns a promise, and a browser that
/// rejects or ignores it simply leaves the track at its current size — the
/// encoder still produces correct output via the existing per-frame fit, exactly
/// as it did before this call existed. So this is a pure optimization and can
/// never fail a share.
///
/// # What the SUCCESS arm records (issue #2179 review)
/// Everything that describes "what the track actually is now" is recorded only
/// once the promise RESOLVES, because only then has anything been applied:
/// - `last_constraint` — the box the caller may consider outstanding. Recording
///   it eagerly (as the first cut did) remembered a box a REJECTED call never
///   applied, and the `== last` short-circuit then suppressed the identical
///   retry on the next tier change.
/// - `live_source_w` / `live_source_h` — the LIVE capture size, re-read from
///   `getSettings()`. This is the pair the wire stamp uses; the acquisition
///   pair is deliberately frozen (see `run_screen_encoding`).
/// - `pending_verify` — arms the "did the engine actually honour it?" check the
///   encode loop performs once the source dims have had `SCREEN_DIM_SETTLE_MS`
///   to settle. `getSettings()` alone is not proof: an engine may report the
///   requested settings while still delivering the old frame size, and it is the
///   FRAME size that drives the per-frame rescale cost.
///
/// Wasm-only (touches `web_sys`); the decisions it feeds live in the
/// host-testable [`screen_track_constraint_for_tier`],
/// [`screen_constraint_was_ignored`] and
/// [`screen_encode_box_when_constraint_ignored`].
fn apply_screen_track_resolution_constraint(
    track: &MediaStreamTrack,
    max_w: u32,
    max_h: u32,
    last_constraint: Rc<Cell<(u32, u32)>>,
    source_dims: SourceDims,
    pending_verify: Rc<Cell<Option<(u32, u32, f64)>>>,
) {
    let width = js_sys::Object::new();
    let _ = Reflect::set(
        &width,
        &JsValue::from_str("max"),
        &JsValue::from_f64(max_w as f64),
    );
    let height = js_sys::Object::new();
    let _ = Reflect::set(
        &height,
        &JsValue::from_str("max"),
        &JsValue::from_f64(max_h as f64),
    );
    let constraints = web_sys::MediaTrackConstraints::new();
    let _ = Reflect::set(&constraints, &JsValue::from_str("width"), &width.into());
    let _ = Reflect::set(&constraints, &JsValue::from_str("height"), &height.into());

    match track.apply_constraints_with_constraints(&constraints) {
        Ok(promise) => {
            let track = track.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match JsFuture::from(promise).await {
                    Ok(_) => {
                        // Only NOW is this box genuinely outstanding.
                        last_constraint.set((max_w, max_h));
                        let settings = track.get_settings();
                        let (live_w, live_h) = settings_source_stamp(
                            settings.get_width().map(f64::from),
                            settings.get_height().map(f64::from),
                        );
                        if live_w > 0 && live_h > 0 {
                            // ONLY the live pair moves — see `SourceDims`.
                            source_dims.refresh_live(live_w, live_h);
                        }
                        // Give the pipeline the same settle window the #1922 gate
                        // uses before judging whether the constraint took effect.
                        let due = window().performance().map(|p| p.now()).unwrap_or(0.0)
                            + SCREEN_DIM_SETTLE_MS;
                        pending_verify.set(Some((max_w, max_h, due)));
                    }
                    Err(e) => {
                        log::warn!(
                            "ScreenEncoder: applyConstraints({max_w}x{max_h}) rejected \
                             ({e:?}); the capture stays at its current size and the \
                             encoder falls back to per-frame scaling"
                        );
                    }
                }
            });
        }
        Err(e) => {
            log::warn!("ScreenEncoder: applyConstraints threw synchronously: {e:?}");
        }
    }
}

/// Read the capture SOURCE dimensions from a screen `MediaStream`'s first video
/// track (issue 2179).
///
/// Returns `(0, 0)` — "unknown" — when the stream has no video track or the
/// track's `getSettings()` omits a complete `width`/`height` pair (see
/// [`settings_source_stamp`], whose "never fabricate an aspect" contract this
/// reuses). Callers treat `(0, 0)` as "not known yet" rather than guessing.
fn screen_stream_source_dims(stream: &MediaStream) -> (u32, u32) {
    let tracks = stream.get_video_tracks();
    if tracks.length() == 0 {
        return (0, 0);
    }
    let track = tracks.get(0).unchecked_into::<MediaStreamTrack>();
    let settings = track.get_settings();
    settings_source_stamp(
        settings.get_width().map(f64::from),
        settings.get_height().map(f64::from),
    )
}

/// The TWO source-dimension pairs a screen share tracks (issue #2179 review r3).
///
/// # Why a struct and not four loose atomics
/// The invariant that matters — "acquisition seeds BOTH, a constraint refreshes
/// ONLY the live pair" — used to live in a doc comment and in a unit test that
/// passed literals to a pure function. That test proves the arithmetic but not
/// the WIRING: someone could refresh the frozen pair inside the encode loop and
/// every test would stay green. Routing both pairs through these two methods
/// makes the invariant executable, because `refresh_live` is the only writer a
/// post-constraint path can reach and it provably cannot touch `frozen`.
///
/// # The invariant, and why it is load-bearing
/// `frozen` is written ONLY at acquisition. [`screen_track_constraint_for_tier`]
/// needs it to know how big the surface ORIGINALLY was: on a step back UP, if
/// the source pair had been refreshed to the shrunken post-constraint size then
/// both `want >= source` and `last >= source` would hold, the "neither request
/// binds" guard would return `None`, and the step-down constraint could NEVER be
/// released — a share would stay soft forever after one congestion episode.
///
/// `live` is what every outgoing packet is stamped with, because telling
/// receivers the share is still 4K after a step-down shrank the capture to 720p
/// is simply a lie on the wire.
#[derive(Clone)]
struct SourceDims {
    frozen_w: Arc<AtomicU32>,
    frozen_h: Arc<AtomicU32>,
    live_w: Arc<AtomicU32>,
    live_h: Arc<AtomicU32>,
}

impl SourceDims {
    /// Build over an encoder-owned live pair.
    fn new_with_live(live_w: Arc<AtomicU32>, live_h: Arc<AtomicU32>) -> Self {
        Self {
            frozen_w: Arc::new(AtomicU32::new(0)),
            frozen_h: Arc::new(AtomicU32::new(0)),
            live_w,
            live_h,
        }
    }

    #[cfg(test)]
    fn new() -> Self {
        Self::new_with_live(Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)))
    }

    /// Capture (re)acquired: both pairs describe the same, brand-new surface.
    fn seed_on_acquisition(&self, w: u32, h: u32) {
        self.frozen_w.store(w, Ordering::Relaxed);
        self.frozen_h.store(h, Ordering::Relaxed);
        self.live_w.store(w, Ordering::Relaxed);
        self.live_h.store(h, Ordering::Relaxed);
    }

    /// A constraint was APPLIED: only the live pair moves. Never touches
    /// `frozen` — that is the whole point of the type.
    fn refresh_live(&self, w: u32, h: u32) {
        self.live_w.store(w, Ordering::Relaxed);
        self.live_h.store(h, Ordering::Relaxed);
    }

    /// Acquisition-frozen dims — the CONSTRAINT decision's input.
    fn frozen(&self) -> (u32, u32) {
        (
            self.frozen_w.load(Ordering::Relaxed),
            self.frozen_h.load(Ordering::Relaxed),
        )
    }

    /// Live capture dims — the WIRE STAMP's input.
    #[cfg(test)]
    fn live(&self) -> (u32, u32) {
        (
            self.live_w.load(Ordering::Relaxed),
            self.live_h.load(Ordering::Relaxed),
        )
    }
}

/// Read the DOMException `name` from a rejected-promise `JsValue`, or the empty
/// string if absent. Feeds the host-tested [`should_retry_screen_capture_without_ceiling`].
fn js_error_name(err: &JsValue) -> String {
    Reflect::get(err, &JsString::from("name"))
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default()
}

/// Acquire a screen-capture `MediaStream` via `getDisplayMedia` with the
/// issue-1973 resolution ceiling applied.
///
/// The ceiling is a `max` constraint; per spec a browser that cannot honor it
/// rejects with `OverconstrainedError`. That is not expected — every mainstream
/// engine satisfies `max` by downscaling — but to guarantee the ceiling can
/// never itself kill a share, the FIRST `OverconstrainedError` triggers ONE
/// retry with the ceiling dropped (the pre-issue-1973 `ideal`-only request).
/// Any other error, or a second failure, is returned unchanged so the caller's
/// existing user-cancel (`NotAllowedError`) vs. failure classification runs. The
/// outer `JsValue` from a synchronous-call error is returned the same way.
///
/// NOTE (Safari): the first `getDisplayMedia` is invoked synchronously when this
/// future is first polled — before any `.await` — preserving the user-gesture
/// requirement. The fallback retry runs after an `await` and so may fall outside
/// the gesture on Safari; if it is rejected the share fails exactly as it would
/// have without this helper, so the retry only ever adds a recovery chance.
///
/// # Return
/// `(stream, ceiling_dropped)`. `ceiling_dropped` is `true` only when the
/// fallback path ran, i.e. the capture was acquired with NO `max` — so the
/// track may be arbitrarily larger than the ladder's top rung and the encode
/// loop must not pretend a ceiling was ever requested (issue #2179 review; see
/// the `last_track_constraint` seed).
async fn acquire_screen_capture_stream(
    media_devices: &web_sys::MediaDevices,
) -> Result<(MediaStream, bool), JsValue> {
    let constraints = screen_capture_display_constraints(true);
    match JsFuture::from(media_devices.get_display_media_with_constraints(&constraints)?).await {
        Ok(stream) => Ok((stream.unchecked_into::<MediaStream>(), false)),
        Err(e) => {
            if should_retry_screen_capture_without_ceiling(&js_error_name(&e), false) {
                log::warn!(
                    "issue 1973: getDisplayMedia rejected the capture resolution ceiling \
                     (OverconstrainedError); retrying once without the max constraint"
                );
                let fallback = screen_capture_display_constraints(false);
                let stream =
                    JsFuture::from(media_devices.get_display_media_with_constraints(&fallback)?)
                        .await?;
                Ok((stream.unchecked_into::<MediaStream>(), true))
            } else {
                Err(e)
            }
        }
    }
}

/// One screen simulcast layer's encoder + per-layer mutable encode state
/// (issue #989, Phase 3b). Mirrors the camera's `LayerEncoder`. Local to
/// `run_screen_encoding`. The WebCodecs output/error `Closure`s must outlive the
/// `VideoEncoder` that holds JS references to them, so they are stored here
/// (leading underscore = held only to keep the JS callbacks alive).
struct LayerEncoder {
    /// This layer's WebCodecs `VideoEncoder`.
    encoder: Box<VideoEncoder>,
    /// Reused config object for in-place bitrate/dimension reconfiguration.
    config: VideoEncoderConfig,
    /// Output-handler-owned sequence cell, read back after the encode loop to
    /// persist this layer's sequence across `'restart`.
    seq_out: Rc<std::cell::Cell<u64>>,
    /// This layer's simulcast id, stamped onto every emitted `PacketWrapper`.
    layer_id: u32,
    /// Current encoder width/height for this layer (issue #1196). Seeded at
    /// construction from the capture dims fitted into `tier_w`/`tier_h`, then
    /// re-fitted per frame in the encode loop when the share's source aspect
    /// changes (window-region resize, shared-surface switch), mirroring the
    /// camera's per-layer `LayerEncoder` and the base screen layer.
    current_w: u32,
    current_h: u32,
    /// This layer's tier bounding box (issue #1196). The source frame is fitted
    /// INSIDE this box (aspect-preserving) rather than configured at the raw box
    /// dims, so a non-16:9 capture is not squashed on rungs 1..n.
    tier_w: u32,
    tier_h: u32,
    /// This simulcast rung's fixed `target_fps` (issue #1832), captured at build
    /// time from the rung's `SCREEN_QUALITY_TIERS` tier. Used to set the WebCodecs
    /// `framerate` rate-control hint every time this rung's `config` is (re)built
    /// — construction and the mid-share per-rung dimension re-fit — so the rung's
    /// bitrate is budgeted across its real cadence (screen runs
    /// 5–10 fps), matching the base layer and the camera encoder. The rung's fps
    /// is fixed, so it is stored once and never mutated.
    target_fps: u32,
    /// Cached bitrate (bps) last applied to this layer's encoder.
    local_bitrate: u32,
    /// Kept alive so the JS output callback stays valid.
    _output_closure: Closure<dyn FnMut(JsValue)>,
    /// Kept alive so the JS error callback stays valid.
    _error_closure: Closure<dyn FnMut(JsValue)>,
}

impl LayerEncoder {
    /// Cache a new bitrate into the stored config and re-apply it.
    fn reconfigure_at_bitrate(&mut self, bitrate_bps: u32) -> Result<(), JsValue> {
        self.config.set_bitrate(bitrate_bps as f64);
        let outcome = self.encoder.configure(&self.config);
        if outcome.is_ok() {
            self.local_bitrate = bitrate_bps;
        }
        outcome
    }
}

// ── Screen encoder error observability counters (cumulative, since page load) ─
// Mirrors the camera encoder pattern. See camera_encoder.rs for design rationale.

static SCREEN_ENCODER_ERRORS_CLOSED_CODEC: AtomicU64 = AtomicU64::new(0);
static SCREEN_ENCODER_ERRORS_VPX_MEM_ALLOC: AtomicU64 = AtomicU64::new(0);
static SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL: AtomicU64 = AtomicU64::new(0);
static SCREEN_ENCODER_ERRORS_GENERIC: AtomicU64 = AtomicU64::new(0);
static SCREEN_ENCODER_FRAMES_SUBMITTED_OK: AtomicU64 = AtomicU64::new(0);
// Screen encoder auto-RESTART cycles (issue #527), partitioned by reason. Bumped
// once per `restart_count += 1`, NOT per error event. Exported as
// `videocall_encoder_restart_total{kind="screen", reason}`. Cold start and
// user-initiated stop do NOT bump these. Mirrors the camera counters.
static SCREEN_ENCODER_RESTARTS_CLOSED_CODEC: AtomicU64 = AtomicU64::new(0);
static SCREEN_ENCODER_RESTARTS_MEMORY: AtomicU64 = AtomicU64::new(0);
static SCREEN_ENCODER_RESTARTS_CONFIGURE: AtomicU64 = AtomicU64::new(0);
static SCREEN_ENCODER_RESTARTS_OTHER: AtomicU64 = AtomicU64::new(0);
// Cumulative count of upper-rung `VideoEncoder`s torn down after a sustained
// shed dwell (issue #1230). Bumped once per `extra_layers` rung freed; the base
// screen layer is never torn down. Mirrors the camera counter.
static SCREEN_ENCODER_LAYERS_TORN_DOWN_AFTER_DWELL: AtomicU64 = AtomicU64::new(0);

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

/// Cumulative screen encoder auto-restart cycles classified as a closed/invalid
/// codec (issue #527). See [`record_screen_restart`].
pub fn screen_encoder_restarts_closed_codec() -> u64 {
    SCREEN_ENCODER_RESTARTS_CLOSED_CODEC.load(Ordering::Relaxed)
}
/// Cumulative screen encoder auto-restart cycles classified as a memory fault.
pub fn screen_encoder_restarts_memory() -> u64 {
    SCREEN_ENCODER_RESTARTS_MEMORY.load(Ordering::Relaxed)
}
/// Cumulative screen encoder auto-restart cycles caused by a fatal `configure()`
/// or an encoder found already-closed at a reconfigure/guard point.
pub fn screen_encoder_restarts_configure() -> u64 {
    SCREEN_ENCODER_RESTARTS_CONFIGURE.load(Ordering::Relaxed)
}
/// Cumulative screen encoder auto-restart cycles with no codec/memory/configure
/// cause (capture-acquisition failures and unclassified errors).
pub fn screen_encoder_restarts_other() -> u64 {
    SCREEN_ENCODER_RESTARTS_OTHER.load(Ordering::Relaxed)
}

/// Record one screen encoder auto-restart cycle, partitioned by [`RestartReason`]
/// (issue #527). Call at EACH `restart_count += 1` site. Cold start and
/// user-initiated stop must NOT call this.
fn record_screen_restart(reason: RestartReason) {
    let counter = match reason {
        RestartReason::ClosedCodec => &SCREEN_ENCODER_RESTARTS_CLOSED_CODEC,
        RestartReason::Memory => &SCREEN_ENCODER_RESTARTS_MEMORY,
        RestartReason::Configure => &SCREEN_ENCODER_RESTARTS_CONFIGURE,
        RestartReason::Other => &SCREEN_ENCODER_RESTARTS_OTHER,
    };
    counter.fetch_add(1, Ordering::Relaxed);
    // `trace!` (off by default) so this adds no production noise; it records the
    // exact `reason` label the metric uses (RestartReason::as_label) for local
    // debugging and is NOT a periodic/analyzer-consumed line.
    log::trace!(
        "screen encoder restart recorded (reason={})",
        reason.as_label()
    );
}

// ── Screen-share WebSocket send-side freshness gate (issue #1921) ─────────────
//
// On a WebSocket publisher ALL media shares one ordered TCP stream — unlike
// WebTransport, where screen rides its own reliable QUIC unistream
// (`MediaStreamKey::Screen`). A large screen keyframe (30–150 KB) then queues
// head-of-line behind whatever screen DELTAS are already backed up in the
// socket, so a receiver's PLI→keyframe turnaround was observed at 30–120 s on
// WS vs 1–2 s on WT, with macroblock corruption and staleness up to 27 s that
// survived the entire #1903 receiver-side fix set.
//
// The gate below shortens that queue at the source. When the browser's WS
// `bufferedAmount` shows the stream is genuinely backed up, screen DELTAS are
// dropped (they would arrive seconds stale regardless) while KEYFRAMES are
// ALWAYS sent. Because the sequence number still advances on a drop, the
// receiver's jitter buffer sees a gap — its `find_next_frame_to_decode` only
// releases sequence-CONTIGUOUS frames — so it holds the deltas behind the gap
// and skips to the next keyframe. The receiver FREEZES on the last good frame
// until that next keyframe: ~1.8 s+ when a PLI is served (the receiver's
// freshness deadline + relay/keyframe RTT) and up to the ~3–5 s periodic/floor
// keyframe cadence otherwise — bounded, and strictly better than the up-to-27 s
// staleness and macroblock corruption it replaces (a dropped delta would
// otherwise decode against a wrong reference).
//
// Scope is SCREEN only. Camera deltas are small and continuous — dropping them
// would stutter the primary video for little queue relief, and shortening the
// screen backlog already speeds every keyframe (camera included) sharing the
// pipe. AUDIO and CONTROL never reach this gate. On WebTransport the depth
// accessor returns `None` (no shared queue), so that path never drops. The
// decision reads the sender's own pre-encryption frame type and media kind, so
// it is E2EE-safe: no encrypted payload is inspected.

/// Cumulative screen DELTAS dropped by the WS send-side freshness gate (#1921),
/// since page load. Mirrors the sibling screen-encoder observability counters;
/// WebTransport publishers (depth `None`) never increment it.
static SCREEN_WS_STALE_DELTA_DROPS: AtomicU64 = AtomicU64::new(0);

/// Wall-clock (epoch ms) of the last throttled freshness-drop log line, and the
/// cumulative drop total captured at that line — shared by BOTH screen output
/// handlers (base + per-simulcast-layer) so the aggregated `info!` is emitted at
/// most once per [`SCREEN_WS_STALE_DROP_LOG_THROTTLE_MS`] no matter how many
/// handlers or frames are dropping. wasm is single-threaded, so plain
/// load/store on these atomics needs no CAS.
static SCREEN_WS_STALE_DROP_LOG_LAST_MS: AtomicU64 = AtomicU64::new(0);
static SCREEN_WS_STALE_DROP_LOG_LAST_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Minimum spacing between aggregated freshness-drop `info!` lines. ≥1s so a
/// congested share logs a once-per-second summary (dropped-count in the window)
/// for e2e assertion + field triage, without spamming the console per frame.
const SCREEN_WS_STALE_DROP_LOG_THROTTLE_MS: u64 = 1000;

/// Cumulative count of stale screen DELTAS dropped at the WS send-side freshness
/// gate (issue #1921). See [`screen_ws_send_decision`].
///
/// DIAGNOSTIC-ONLY pending telemetry wiring: this counter is NOT yet surfaced in
/// the health/stats packet. The sibling encoder counters (e.g.
/// `screen_encoder_errors_generic`) are NAMED protobuf fields on the stats
/// packet, so surfacing this one requires a NEW proto field + Docker codegen (a
/// cross-crate change deferred out of this PR). Until then it is observable via
/// the throttled `info!` at the drop site (see [`record_screen_ws_stale_drop`])
/// and drives the #1921 AQ freshness axis
/// ([`screen_ws_stale_drop_step_down_decision`]).
pub fn screen_ws_stale_delta_drops() -> u64 {
    SCREEN_WS_STALE_DELTA_DROPS.load(Ordering::Relaxed)
}

/// Record one #1921 freshness-gate drop: bump the cumulative counter and, at most
/// once per [`SCREEN_WS_STALE_DROP_LOG_THROTTLE_MS`], emit an aggregated `info!`
/// summarizing how many deltas were gated since the last line plus the live
/// backlog/threshold. Shared by both screen output handlers (one throttle for
/// all). wasm-only (reads `js_sys::Date::now()`); the pure send DECISION stays in
/// [`screen_ws_send_decision`].
fn record_screen_ws_stale_drop(buffered_bytes: u64, threshold_bytes: u64) {
    let total = SCREEN_WS_STALE_DELTA_DROPS.fetch_add(1, Ordering::Relaxed) + 1;
    let now_ms = js_sys::Date::now() as u64;
    let last_ms = SCREEN_WS_STALE_DROP_LOG_LAST_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last_ms) >= SCREEN_WS_STALE_DROP_LOG_THROTTLE_MS {
        SCREEN_WS_STALE_DROP_LOG_LAST_MS.store(now_ms, Ordering::Relaxed);
        let prev_total = SCREEN_WS_STALE_DROP_LOG_LAST_TOTAL.swap(total, Ordering::Relaxed);
        let dropped_in_window = total.saturating_sub(prev_total);
        log::info!(
            "ScreenEncoder: dropping stale screen delta(s) under WS backpressure (issue #1921) — \
             dropped={dropped_in_window} in last window, buffered={buffered_bytes}, \
             threshold={threshold_bytes}"
        );
    }
}

/// Acceptable socket-queue delay before a screen DELTA is treated as stale: the
/// freshness threshold is this many milliseconds of the encoder's current screen
/// target bitrate ("half a second of screen video worth of backlog"). 500 ms is
/// the conservative end of the #1921 range — on a healthy link `bufferedAmount`
/// flushes to the kernel and sits near zero, so the gate stays dormant; only a
/// genuine sustained uplink backlog exceeds it.
const SCREEN_WS_FRESHNESS_DELAY_MS: u64 = 500;

/// Never drop for a backlog smaller than this. A few-KB queue drains within a
/// frame or two and delays no keyframe meaningfully, so gating below it would
/// only stutter the share on transient blips.
const SCREEN_WS_MIN_THRESHOLD_BYTES: u64 = 16_384;

/// WS `bufferedAmount` (bytes) above which a screen DELTA is dropped, given the
/// screen bitrate in kbps. Pure so it is host-testable. The result is floored at
/// [`SCREEN_WS_MIN_THRESHOLD_BYTES`].
fn screen_ws_freshness_threshold_bytes(target_bitrate_kbps: u32) -> u64 {
    // bytes = kbps * 1000 / 8 * delay_ms / 1000  ==  kbps * 125 * delay_ms / 1000
    let bytes = (target_bitrate_kbps as u64)
        .saturating_mul(125)
        .saturating_mul(SCREEN_WS_FRESHNESS_DELAY_MS)
        / 1000;
    bytes.max(SCREEN_WS_MIN_THRESHOLD_BYTES)
}

const _: () = assert!(
    SCREEN_UPLINK_PRESSURE_MS < SCREEN_WS_FRESHNESS_DELAY_MS,
    "threshold ordering only; SCREEN_UPLINK_PRESSURE_DWELL_MS means this does \
     NOT imply the governor steps down before the #1921 gate discards"
);

/// Whether `governed` warrants a `configure()` attempt this tick. A target the
/// encoder already rejected is not retried until the governor moves elsewhere,
/// so one non-fatal failure cannot re-fire every SCREEN_STATIC_REENCODE_POLL_MS.
fn should_attempt_screen_reconfigure(
    governed: ScreenTargetKbps,
    local_target: ScreenTargetKbps,
    last_failed: Option<ScreenTargetKbps>,
) -> bool {
    governed != local_target && last_failed != Some(governed)
}

/// Whether this tick's step warrants an `info!` line, and the next latch value.
/// A bare `governed != local_target` re-fires every poll while a rejected target
/// is latched — only an ACCEPTED `configure()` moves `local_target` (#1221-pt1).
/// Re-arming at the applied target keeps a later step to the same value visible.
fn screen_step_log_decision(
    governed: ScreenTargetKbps,
    local_target: ScreenTargetKbps,
    last_logged: Option<ScreenTargetKbps>,
) -> (bool, Option<ScreenTargetKbps>) {
    if governed == local_target {
        (false, None)
    } else if last_logged == Some(governed) {
        (false, last_logged)
    } else {
        (true, Some(governed))
    }
}

/// The #1921 freshness threshold for the CURRENT capture geometry. Takes a
/// [`ScreenBaselineKbps`] — never a governed target — so the gate that SENSES
/// congestion cannot be moved by the governor that ACTS on it.
fn screen_ws_gate_threshold_bytes(baseline: ScreenBaselineKbps) -> u64 {
    screen_ws_freshness_threshold_bytes(baseline.kbps())
}

/// Pick the sample from the ACTIVE transport, never from the `Option`-ness of
/// the depth: `send_queue_depth()` is also `None` with no elected connection or
/// a borrowed controller cell, where the WT counters cannot advance and so read
/// as relief on every tick.
fn screen_uplink_sample(
    active_transport: Option<&str>,
    ws_buffered: Option<u64>,
    gate_drops: u64,
    stream_events: u64,
) -> ScreenUplinkSample {
    match (active_transport, ws_buffered) {
        (Some("webtransport"), _) => ScreenUplinkSample::StreamEvents {
            events: stream_events,
        },
        (Some("websocket"), Some(bytes)) => ScreenUplinkSample::Buffered { bytes, gate_drops },
        _ => ScreenUplinkSample::Unobservable,
    }
}

/// Reconstruct the live baseline from the atomics the geometry publisher writes.
fn screen_baseline_from_published(
    encode_w: &AtomicU32,
    encode_h: &AtomicU32,
    tier_idx: &AtomicU32,
) -> ScreenBaselineKbps {
    ScreenBaselineKbps::for_geometry(
        encode_w.load(Ordering::Relaxed),
        encode_h.load(Ordering::Relaxed),
        active_screen_tier_fps(tier_idx.load(Ordering::Relaxed)),
    )
}

/// Outcome of the screen-share WS send-side freshness gate (issue #1921).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenWsSend {
    /// Transmit the frame normally.
    Send,
    /// Drop this stale screen DELTA — the WS backlog is over the freshness
    /// threshold and the delta would arrive too late to be useful.
    DropStaleDelta,
}

/// Send-side freshness decision for one screen frame on a WebSocket publisher.
///
/// * `buffered_amount` — the WS `bufferedAmount`, or `None` when the active
///   transport is WebTransport (screen has its own QUIC unistream, no shared
///   queue) or no connection is elected yet. `None` ⇒ always [`ScreenWsSend::Send`].
/// * `is_keyframe` — keyframes are ALWAYS sent; the decode chain and the #1908
///   keyframe floor depend on them arriving.
/// * `threshold_bytes` — from [`screen_ws_freshness_threshold_bytes`].
///
/// Pure and stateless: each frame is judged against the live backlog, so the
/// gate cannot wedge (there is no consecutive-success counter to pin healthy)
/// and needs no reset on reconnect — a fresh socket starts at
/// `bufferedAmount == 0`, below any threshold.
fn screen_ws_send_decision(
    buffered_amount: Option<u64>,
    is_keyframe: bool,
    threshold_bytes: u64,
) -> ScreenWsSend {
    match buffered_amount {
        // WebTransport / no elected connection: no shared queue to shorten.
        None => ScreenWsSend::Send,
        // Keyframes are never dropped — the receiver resumes on them.
        Some(_) if is_keyframe => ScreenWsSend::Send,
        // Backed-up socket: drop this stale delta so keyframes queue less.
        Some(buffered) if buffered > threshold_bytes => ScreenWsSend::DropStaleDelta,
        // Queue is shallow enough that the delta will arrive fresh.
        Some(_) => ScreenWsSend::Send,
    }
}

/// Cumulative count of upper-rung simulcast `VideoEncoder`s torn down after a
/// sustained shed dwell (issue #1230). Pure observability hook, mirrors the
/// camera getter: confirms memory is reclaimed on sustained-distress devices and
/// that teardown is not thrashing.
pub fn screen_encoder_layers_torn_down() -> u64 {
    SCREEN_ENCODER_LAYERS_TORN_DOWN_AFTER_DWELL.load(Ordering::Relaxed)
}

// ── Screen encoder tick-starvation stall telemetry (discussion 1960, issue 2) ──
// Monotonic count of detected encoder stall EPISODES (each loop-tick resume whose wall-clock gap
// exceeded SCREEN_ENCODER_STALL_GAP_MS — a JS-main-thread freeze during which receivers saw
// re-encoded stale content), plus the MAX observed stall gap (ms). Same zero-cost global-static +
// non-zero-only export convention as the error/restart counters above; the issue-1972 Grafana work
// folds these getters into the health packet the relay scrapes (this PR provides the surface only).
static SCREEN_ENCODER_STALL_EPISODES: AtomicU64 = AtomicU64::new(0);
/// Cumulative count of capture `applyConstraints` calls whose promise RESOLVED
/// but whose track was still larger than the requested box (issue #2179 review).
///
/// A non-zero value means the engine is silently declining the capture-side
/// downscale, so every frame is being software-rescaled on the WebCodecs queue —
/// the #1973 stall shape. Deliberately a plain cumulative counter alongside the
/// stall counters rather than a new subsystem; it is NOT yet carried on the
/// health packet because that needs a `videocall-types` proto field, so the
/// field signal today is the `[SCREEN_ENCODER] constraint-ignored` warn that
/// accompanies each increment.
static SCREEN_ENCODER_IGNORED_CONSTRAINTS: AtomicU64 = AtomicU64::new(0);
static SCREEN_ENCODER_MAX_STALL_GAP_MS: AtomicU64 = AtomicU64::new(0);

/// Cumulative screen encoder tick-starvation stall episodes since page load (discussion 1960,
/// issue 2). Each increment is one `'encode` loop-tick resume whose wall-clock gap since the previous
/// tick exceeded [`SCREEN_ENCODER_STALL_GAP_MS`] — a main-thread freeze during which the encoder could
/// not sample fresh capture and receivers saw `fps > 0` on minutes-stale re-encoded content.
pub fn screen_encoder_stall_episodes() -> u64 {
    SCREEN_ENCODER_STALL_EPISODES.load(Ordering::Relaxed)
}

/// Cumulative count of IGNORED capture-resolution constraints (issue #2179
/// review). See [`SCREEN_ENCODER_IGNORED_CONSTRAINTS`].
pub fn screen_encoder_ignored_constraints() -> u64 {
    SCREEN_ENCODER_IGNORED_CONSTRAINTS.load(Ordering::Relaxed)
}
/// Largest screen encoder tick-starvation gap observed since page load (ms, rounded) (discussion 1960,
/// issue 2). Surfaces the worst single freeze duration for field/Grafana attribution of the
/// "fps > 0 but content minutes stale" symptom.
pub fn screen_encoder_max_stall_gap_ms() -> u64 {
    SCREEN_ENCODER_MAX_STALL_GAP_MS.load(Ordering::Relaxed)
}

/// **TEST-ONLY** seam for the two stall counters above (issue #2147).
///
/// The health reporter's emission of `screen_encoder_stall_episodes` /
/// `screen_encoder_max_stall_gap_ms` is gated `> 0`, and these statics are only ever
/// incremented by the encode loop's tick-starvation detector inside a `spawn_local`
/// future. Without a setter both `if` blocks are unreachable from a host test, so
/// DELETING the emission would leave every test green — the exact gap this closes.
///
/// Sets absolute values (not increments) so a test can assert the zero and nonzero
/// arms deterministically. `#[cfg(test)]`-gated, so it cannot be reached in
/// production.
///
/// # Isolation contract (issue #2160)
///
/// These statics are PROCESS-GLOBAL and cumulative-since-page-load, and libtest runs
/// `videocall-client`'s plain `#[test]`s on a multi-threaded pool, so a caller holds
/// them at a value that is visible to every CONCURRENT test — not just to later ones.
/// Nothing available here can prevent that: [`screen_encoder_stall_episodes`] and
/// [`screen_encoder_max_stall_gap_ms`] are plain `load(Ordering::Relaxed)` and acquire
/// no lock, and `HealthReporter::create_health_packet` calls them unconditionally, so
/// a concurrent sibling building a health packet WILL observe whatever is stored here.
///
/// Two conventions — both required, neither enforceable by the compiler:
///
/// 1. Hold [`crate::test_serial::lock_screen_encoder_stall_counters`] for as long as
///    you depend on what you stored. This excludes other GUARD-TAKERS only; it is what
///    keeps two injecting tests from interleaving with each other.
/// 2. Restore `(0, 0)` before releasing the guard. This is what bounds the window in
///    which an UNGUARDED sibling can observe the injected value to your own body.
///
/// Callers must take [`crate::test_serial::lock_screen_encoder_stall_counters`] and
/// restore the prior values. A test that asserts on the resulting protobuf fields
/// must take the same guard; the reasoning is recorded at its caller in
/// `health_reporter.rs`.
#[cfg(test)]
pub(crate) fn set_screen_encoder_stall_counters_for_test(episodes: u64, max_gap_ms: u64) {
    SCREEN_ENCODER_STALL_EPISODES.store(episodes, Ordering::Relaxed);
    SCREEN_ENCODER_MAX_STALL_GAP_MS.store(max_gap_ms, Ordering::Relaxed);
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

/// What the encode task does after the inner `'encode` loop breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostEncodeExit {
    /// `break 'restart` — terminate the encode task cleanly. The capture TRACK
    /// ended and cannot be revived by this task.
    Shutdown,
    /// `continue 'restart` — re-enter the restart loop to rebuild the encoder.
    Restart,
}

/// Decide whether a broken `'encode` loop should SHUT DOWN or RESTART.
///
/// The single deciding input is whether the capture **track** ended
/// (`stream_ended`: `reader.read()` resolved with `done` / an `undefined`
/// value — a user stop, the browser's "Stop sharing" button, or an OS/source
/// revoke). A dead capture track is **unrecoverable from inside this task**:
/// the only way back is a fresh `getDisplayMedia()`, which the spec gates on
/// transient user activation this background task does not have. Worse, this
/// `ScreenEncoder` (and its `EncoderState.enabled` `Arc` plus the shared
/// `screen_stream` / `active_video_track` cells) is REUSED for the user's next
/// share, so a post-track-end auto-restart races that next share: it clobbers
/// the new task's shared stream/track cells and its failed non-gesture
/// `getDisplayMedia()` stores `enabled = false`, killing the legitimate new
/// encode task (the stop-tab-then-share-window defect). So a track end MUST
/// short-circuit straight to clean shutdown.
///
/// A NON-track-end exit (a fatal codec fault or a transient read error while
/// the track is still alive) still returns [`PostEncodeExit::Restart`] — that
/// is the genuine encoder auto-recovery the restart loop exists for, and it is
/// deliberately left intact.
fn post_encode_exit_action(stream_ended: bool) -> PostEncodeExit {
    if stream_ended {
        PostEncodeExit::Shutdown
    } else {
        PostEncodeExit::Restart
    }
}

/// Sustained-shed dwell before an upper-rung screen `VideoEncoder` is torn down
/// to reclaim its native VPX/WebCodecs state + ~150KB output buffer (issue
/// #1230). Sibling of the camera const (no shared encode-util module exists, so
/// each loop owns its const, matching the per-file pure-helper style).
///
/// Why 30s: the AQ controller can shed/restore a layer at most once per
/// `MIN_TIER_TRANSITION_INTERVAL_MS` = 1500ms (the `can_transition` floor in
/// `videocall-aq/src/manager.rs`), so 30s is 20× the minimum shed→restore
/// interval — a transient bounce can never accumulate 30s of CONTINUOUS shed and
/// so never trips teardown. Teardown is thrash-free regardless of how soon an
/// earn-up follows: it requires 30s of UNBROKEN shed and the per-frame stamp loop
/// clears a rung's dwell clock the instant it is re-activated, so a
/// teardown→rebuild→teardown cycle is necessarily ≥30s apart. A re-earned rung is
/// rebuilt by the SAME lazy `build_extra_layer` path a publisher already runs at
/// every cold start (only the base is built up front since #1204/#1227), so
/// teardown introduces no new rebuild-stall class. (`MIN_TIER_TRANSITION_INTERVAL_MS`
/// lives in `videocall-aq/src/constants.rs`. NOTE: `CLIMB_COOLDOWN_BASE_MS` is
/// unrelated — it governs the crash-CEILING decay axis, not layer earn-up.)
const SHED_TEARDOWN_DWELL_MS: f64 = 30_000.0;

/// Poll interval (ms) the screen encode loop uses to race `reader.read()` against
/// a timer (issue #1841). A real `getDisplayMedia` track delivers frames ONLY on
/// visual change, so on a STATIC share the encode loop would otherwise park on
/// `read()` indefinitely — and a late joiner's `KEYFRAME_REQUEST` (which merely
/// sets `force_keyframe`) would never be serviced because the decision that reads
/// that flag runs only after a real frame. Racing the read against this timer lets
/// the loop wake on a quiet track and, when a PLI is pending, re-encode the
/// retained frame as a keyframe.
///
/// 150ms: bounds worst-case joiner-visible latency to ~1 tick + encode + relay
/// (well under the <1s bar) while keeping the idle timer branch near-free (two
/// atomic peeks, then loop back). It comfortably exceeds a 60fps frame interval
/// (~16ms) so it never races ahead of a genuinely live track.
const SCREEN_STATIC_REENCODE_POLL_MS: u32 = 150;

/// Minimum wall-clock gap (ms) between the throttled DEBUG lines emitted on a
/// synthetic (retained-frame) re-encode (issue #1841). The FIRST synthetic emit
/// logs once at INFO for field visibility; every subsequent one logs at DEBUG no
/// more than once per this window so even a debug build can't spam it under a
/// sustained joiner churn.
const SCREEN_SYNTHETIC_LOG_THROTTLE_MS: f64 = 1_000.0;

/// Post-quiet budget of static-share keyframe FLOOR emits (issue #1903).
///
/// When a screen share goes STATIC (capture stops emitting frames) the encode loop's timer branch
/// re-encodes the retained frame as a keyframe on a wall-clock FLOOR
/// (`SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS`) so a receiver whose `KEYFRAME_REQUEST` was LOST — WS
/// HOL-blocking, relay suppression, packet loss — still recovers even though its PLI never reached
/// the publisher. The pre-#1903 code only re-encoded the retained frame ON a pending PLI, so a lost
/// PLI meant an indefinite freeze on stale content (the field's "shared content freezes and never
/// refreshes"). Without this floor the periodic GOP keyframe fires ONLY in the real-frame branch, so
/// a paused capture emits no keyframe at all between PLIs.
///
/// The floor is BOUNDED — not a perpetual idle-bandwidth drain — in two independent ways: (1) it
/// fires at most once per `SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS` (the same 3s cadence the
/// moving-content periodic GOP uses, so a share emits keyframes at one uniform cadence whether moving
/// or paused, and never at capture fps); and (2) this budget caps how many CONSECUTIVE floor
/// keyframes go out after the last real frame. Each real captured frame REPLENISHES the budget, so
/// every content change earns a fresh recovery window (covering a change whose delta was lost to some
/// receiver), and a genuinely-idle share (content unchanged for minutes — where every receiver is
/// already showing the correct last frame) stops re-encoding after `BUDGET` cycles instead of
/// spending ~150KB/3s room-wide forever.
///
/// 4: at the 3s floor that is a 12s post-quiet recovery window — comfortably longer than a receiver's
/// own reactive retry/escalation cadence (the codecs jitter buffer re-arms its proactive PLI and
/// escalates within ~6–8s), so a lost PLI has several independent chances to be covered before the
/// floor backs off. A PLI arriving after the budget is spent is still served immediately by the
/// existing on-demand path (that path is ungated by this budget).
const SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET: u32 = 4;
/// Compile-time guard: the floor must permit at least one post-quiet recovery keyframe. A `0` budget
/// would silently disable the #1903 insurance path (the timer branch's `maybe_floor` gate is never
/// true), re-opening the "freeze never refreshes" defect with no test failing.
const _: () = assert!(SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET > 0);

/// Pure decision for the static-share keyframe FLOOR (issue #1903), extracted as a host-testable
/// free function (mirroring `should_teardown_shed_layer` / `periodic_keyframe_due`) so the
/// wall-clock + budget gate is pinned by a native unit test off the wasm-only encode loop.
///
/// Returns `true` iff a floor keyframe is due NOW: the post-quiet `budget_remaining` is not yet
/// exhausted AND at least `floor_ms` has elapsed since the last keyframe of ANY kind
/// (`last_keyframe_emit_ms`, updated by every periodic/forced/floor emit). `None` last-emit ⇒ `false`:
/// on a share that has not emitted a keyframe yet there is no retained frame to re-encode, and the
/// real-frame branch owns the first keyframe. The `>=` boundary is inclusive, matching
/// `periodic_keyframe_due`.
fn static_keyframe_floor_due(
    now_ms: f64,
    last_keyframe_emit_ms: Option<f64>,
    floor_ms: f64,
    budget_remaining: u32,
) -> bool {
    if budget_remaining == 0 {
        return false;
    }
    match last_keyframe_emit_ms {
        Some(last) => now_ms - last >= floor_ms,
        None => false,
    }
}

/// Static-share keyframe-FLOOR budget transition on a FLOOR emit (issue #1903): consume exactly one
/// unit, saturating at 0. Single source of truth for the encode loop's post-emit decrement, extracted
/// so a native test pins it — without this, deleting the loop's decrement would let the floor emit
/// every `SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS` FOREVER on a truly-idle share (unbounded idle
/// bandwidth), and every existing test would still pass.
fn floor_budget_after_emit(budget_remaining: u32) -> u32 {
    budget_remaining.saturating_sub(1)
}

/// Static-share keyframe-FLOOR budget transition on a REAL captured frame (issue #1903): re-arm to the
/// full `SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET` so every content change earns a fresh post-quiet recovery
/// window. Single source of truth for the encode loop's replenish, extracted so a native test pins it —
/// without this, dropping the replenish would cap the floor at `SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET`
/// emits per encoder LIFETIME (a share that goes quiet, recovers, and goes quiet again would never
/// re-issue floor keyframes), and every existing test would still pass.
fn floor_budget_replenished() -> u32 {
    SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET
}

/// Wall-clock anchor (ms, `performance.now()`) of the last screen keyframe that reached **every**
/// currently-published rung — the input the real-frame arm's periodic-GOP backstop measures its
/// `SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS` ceiling from (issue #2328).
///
/// ## The defect this exists to close
/// The screen loop emits keyframes from three arms — the real-frame arm, the static-share
/// PLI/FLOOR timer arm, and the #1922 settled-resize apply — and BEFORE #2328 all three stamped the
/// single `last_keyframe_emit_ms`, which is ALSO what `periodic_keyframe_due` reads as its
/// wall-clock ceiling. Only the real-frame and settle arms fan out to the FULL
/// `local_active_layers`; a **pure-insurance** floor emit is pressure-gated by
/// a pressure gate, so it could re-key a subset of the published rungs — yet it still stamped the
/// shared clock, pushing the full-fan-out periodic keyframe perpetually out of due. A receiver on a
/// rung the floor skipped was left with NO INSURANCE path at all: both unrequested emit paths (the
/// gated floor, and the
/// wall-clock periodic it deferred) skipped its rung, so recovery depended entirely on a reactive
/// PLI surviving the relay's per-(receiver, session) limiter and the publisher's
/// `ENCODER_PLI_COOLDOWN_MS` coalescer. Meanwhile deltas kept arriving, so the freeze was invisible
/// to the relay and to the receiver's arrival-based `LayerAvailability`. The field capture in #2258
/// shows the arithmetic: 67 synthetic (floor) vs 12 real keyframes over a 4m12s share.
///
/// ## The invariant
/// Only an emit whose fan-out covered the full active set re-arms this clock. `last_keyframe_emit_ms`
/// is untouched and keeps doing its own job — the #1287/#1312/#1322 PLI coalescer window and the
/// #1311/#1347 reconnect cooldown reset — so this adds a SECOND, stricter clock rather than
/// re-purposing the existing one. Consequence: a gated floor emit still suppresses redundant PLIs
/// (correct — a keyframe did go out on the low rungs) but no longer defers the ceiling that is the
/// ONLY insurance a top-rung receiver has.
///
/// ## Honesty caveat (inherited, not introduced)
/// The stamp records the fan-out the loop INTENDED, taken at the same point `last_keyframe_emit_ms`
/// is written — before the per-rung `encode_with_options` calls. A non-fatal per-rung encode error
/// is logged and skipped, so a stamp can over-claim by a rung; a FATAL one breaks to `'restart`,
/// which resets both clocks to `None` anyway. This is exactly the existing `last_keyframe_emit_ms`
/// semantics, and the next cadence (≤ `SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS`) re-covers the
/// missed rung.
///
/// Declared INSIDE the encode loop's `'restart` scope alongside `last_keyframe_emit_ms`: a restart
/// rebuilds every encoder, and the first `encode()` after a `configure()` is an implicit keyframe on
/// each rung, so starting from `None` is both honest and self-correcting (`periodic_keyframe_due`
/// treats `None` as "due once a frame has been counted").
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct FullFanoutKeyframeClock {
    last_ms: Option<f64>,
}

impl FullFanoutKeyframeClock {
    /// Cold state: no full-fan-out keyframe emitted yet this encoder session.
    fn new() -> Self {
        Self { last_ms: None }
    }

    /// Record a keyframe emitted at `now_ms` that reached `fanout_layers` rungs while
    /// `active_layers` were published. Re-arms the ceiling ONLY on a full fan-out; a gated
    /// (partial) emit is deliberately ignored so the top rung's backstop keeps running.
    ///
    /// `active_layers` is floored at 1 because the base rung is always published, so a caller that
    /// passes 0 (no simulcast) still records a base-only emit as full coverage.
    ///
    /// With a single published rung every caller passes `fanout_layers == active_layers`, so the
    /// gate is trivially satisfied today. It is RETAINED, not vestigial: it is the #2328 guard, and
    /// publishing a second rung reactivates it.
    fn on_keyframe_emitted(&mut self, now_ms: f64, fanout_layers: usize, active_layers: usize) {
        if fanout_layers >= active_layers.max(1) {
            self.last_ms = Some(now_ms);
        }
    }

    /// The `last_keyframe_emit_ms`-shaped anchor to hand [`periodic_keyframe_due`].
    fn anchor_ms(&self) -> Option<f64> {
        self.last_ms
    }
}

/// The SCREEN real-frame arm's periodic-GOP predicate, with its wall-clock anchor CHOICE baked in
/// (issue #2328).
///
/// ## Why this wrapper exists — it is a mutation guard, not a convenience
/// The encode loop is one enormous `spawn_local` async closure and is not host-testable, so a test
/// can pin [`FullFanoutKeyframeClock`] and [`periodic_keyframe_due`] but cannot reach the LINE that
/// wires them together. That left the highest-probability regression unguarded: swap the anchor
/// argument back to `last_keyframe_emit_ms` at the call site, leave the struct and every stamp in
/// place, and #2258 is fully reintroduced while every unit test still passes.
///
/// Taking [`FullFanoutKeyframeClock`] — a distinct TYPE, not a bare `Option<f64>` — is what closes
/// that: the one-token revert no longer compiles, and the anchor choice now lives inside a function
/// a host test can execute and mutate-check
/// (`the_screen_periodic_backstop_is_anchored_on_the_full_fanout_clock`).
///
/// RESIDUAL, stated so a reviewer does not over-trust this: a deliberate rewrite that deletes this
/// call and inlines `periodic_keyframe_due(.., last_keyframe_emit_ms, ..)` again would still
/// compile and still pass every unit test. Nothing in-crate can catch that, because the call site
/// itself is unreachable from a host test. That case is covered only by the E2E spec.
///
/// `last_keyframe_emit_ms` is deliberately NOT a parameter here. It anchors the PLI coalescer
/// (`ENCODER_PLI_COOLDOWN_MS`) and carries the #1311/#1347 reconnect reset; a gated floor emit
/// legitimately stamps it (a keyframe really did go out, on the low rungs) which is exactly why it
/// is the wrong anchor for a ceiling that must guarantee EVERY rung was re-keyed.
fn screen_periodic_keyframe_due(
    frame_counter: u32,
    keyframe_interval_frames: u32,
    now_ms: f64,
    full_fanout: &FullFanoutKeyframeClock,
    max_interval_ms: f64,
) -> bool {
    periodic_keyframe_due(
        frame_counter,
        keyframe_interval_frames,
        now_ms,
        full_fanout.anchor_ms(),
        max_interval_ms,
    )
}

/// The static-share keyframe-FLOOR accounting that MUST survive a screen-encoder `'restart`
/// (issue #1903, live-stack root cause). Groups the two pieces of floor state a `'restart` previously
/// wiped — the post-quiet emit `budget` and the floor's OWN cadence `clock_ms` — so both are carried
/// across an encoder restart (fatal encode error / closed codec / stream replace) instead of being
/// zeroed.
///
/// ## Why this exists (the live defect the unit tests missed)
/// The first #1903 cut declared the budget INSIDE the `'restart` loop and drove the floor cadence off
/// the encode loop's `last_keyframe_emit_ms`, which is ALSO re-declared to `None` inside `'restart`
/// (its #1311 reset is deliberate). So any `'restart` reset BOTH to their disarmed values (budget 0 /
/// clock `None`), and because ONLY a fresh REAL captured frame restores them, a share that had gone
/// static (parked capture, no new frame) stayed permanently disarmed — the floor never fired on the
/// live stack even though every pure-helper unit test passed. This account is held OUTSIDE `'restart`
/// and carried across restarts by [`ScreenFloorAccount::carry_across_restart`], and it keeps its own
/// cadence clock SEPARATE from `last_keyframe_emit_ms` precisely so it does not inherit that #1311
/// reset.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ScreenFloorAccount {
    /// Post-quiet floor-emit budget; see [`SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET`]. `0` ⇒ backed off.
    budget: u32,
    /// Wall-clock (`performance.now()`, ms) of the last keyframe of ANY kind the floor has observed —
    /// a real-arm periodic/PLI keyframe, or a timer-arm floor/PLI re-encode. Drives the floor's
    /// ≥`floor_ms` cadence. `None` until the first keyframe (no retained frame to re-encode yet).
    clock_ms: Option<f64>,
}

impl ScreenFloorAccount {
    /// Cold state: no budget, no cadence anchor. Matches a share that has produced no frame yet.
    fn idle() -> Self {
        Self {
            budget: 0,
            clock_ms: None,
        }
    }

    /// A real captured frame just published: re-arm the post-quiet budget so every content CHANGE
    /// earns a fresh recovery window.
    fn on_captured_frame(&mut self) {
        self.budget = floor_budget_replenished();
    }

    /// A keyframe of any kind was emitted at `now_ms` (real-arm periodic/PLI, or a timer-arm PLI that
    /// was not floor-driven): stamp the cadence anchor so the floor waits a full `floor_ms` before its
    /// next keyframe. Does NOT touch the budget.
    fn on_keyframe_emitted(&mut self, now_ms: f64) {
        self.clock_ms = Some(now_ms);
    }

    /// A FLOOR keyframe was emitted at `now_ms`: consume one budget unit AND stamp the cadence anchor.
    fn on_floor_emitted(&mut self, now_ms: f64) {
        self.budget = floor_budget_after_emit(self.budget);
        self.clock_ms = Some(now_ms);
    }

    /// Whether a floor keyframe is due now: budget available AND ≥`floor_ms` since the last keyframe.
    fn floor_due(&self, now_ms: f64, floor_ms: f64) -> bool {
        static_keyframe_floor_due(now_ms, self.clock_ms, floor_ms, self.budget)
    }

    /// Carry the account across an encoder `'restart` (issue #1903). Returns `self` UNCHANGED by
    /// design: the budget and cadence clock survive a restart so a static share that restarts still
    /// floors. Written as an explicit transition (rather than relying only on the field being declared
    /// outside `'restart`) so the survival contract is pinned by a native test — reverting this to
    /// `Self::idle()` reproduces the live permanent-disarm bug and fails
    /// `floor_account_survives_restart_and_floors_on_static`.
    fn carry_across_restart(self) -> Self {
        self
    }
}

/// Pure teardown decision (issue #1230, host-testable single source of truth) —
/// sibling of the camera helper. Returns `true` iff `shed_since_ms` is `Some(t)`
/// AND `now_ms - t >= dwell_threshold_ms`; `None` ⇒ `false` (not currently shed,
/// or already torn down). The `>=` makes the boundary inclusive. This is the only
/// place the comparison lives so a host unit test pins it (mutating `>=`→`>`,
/// inverting the comparison, or dropping the `None` guard all fail the test).
fn should_teardown_shed_layer(
    shed_since_ms: Option<f64>,
    now_ms: f64,
    dwell_threshold_ms: f64,
) -> bool {
    match shed_since_ms {
        Some(since) => now_ms - since >= dwell_threshold_ms,
        None => false,
    }
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

/// Close the retained static-share `VideoFrame` (issues #1841/#1903) if one is held, taking it out of
/// `retained`. A held `VideoFrame` owns a native/GPU buffer, so every exit from the encode task must
/// release it. Since #1903 stopped closing the retained frame on each `'restart` (so the static-share
/// keyframe path survives a restart), the `'restart`-internal give-up `return;` paths — which bypass
/// the encode loop's final cleanup — must call this before returning, or the frame leaks. Idempotent:
/// a no-op on `None`, so it is safe on give-up paths that ran before any frame was retained.
fn close_retained_frame(retained: &mut Option<VideoFrame>) {
    if let Some(frame) = retained.take() {
        frame.close();
    }
}

/// Settle window (ms) the source-dimension reconfigure gate (issue #1922) waits for the raw capture
/// dims to hold STEADY before it applies ONE encoder `configure()` + its single implicit keyframe.
///
/// ## Why a settle gate (the field defect)
/// A screen-share `getDisplayMedia` track re-negotiates its native capture dimensions on EVERY step of
/// a window drag-resize, delivering a burst of frames whose `display_width()/display_height()` change
/// continuously (field build ba1a44f1: up to 18 dimension deltas in a single second). The pre-#1922
/// code rebuilt `VideoEncoderConfig` and called `configure()` IMMEDIATELY on each delta. In WebCodecs
/// the first `encode()` after a `configure()` is an IMPLICIT keyframe that does NOT pass through the
/// keyframe cooldown/coalescer (`keyframe_tick_decision` / `ENCODER_PLI_COOLDOWN_MS`), so a drag became
/// a ~140-keyframe storm — pixelation for every receiver and, when a `configure()` fatally errored
/// mid-storm, a re-prompting restart that dropped the whole share (issue #1922).
///
/// ## Why we can safely DEFER the reconfigure
/// WebCodecs SCALES a frame whose native dims differ from the encoder's configured dims and emits valid
/// output — this codebase already relies on that every frame (the #1841 downscaled-tier path and the
/// #1903 restart-carried retained-frame re-encode; see the comment at the encode loop's restart-carry
/// point). So DURING a drag we keep encoding at the current config (frames are scaled, output stays
/// valid) and apply exactly one `configure()` once the source has been stable for `settle_ms`.
///
/// ## Choosing 400ms
/// A drag emits deltas continuously at ~50–100ms gaps (the field's peak 18/sec ≈ 55ms). The window MUST
/// exceed those inter-delta gaps — and a brief mid-drag PAUSE (repositioning the grip, ~100–250ms) —
/// so it does not fire a spurious reconfigure mid-drag; 400ms is ~4–8× the delta gap and comfortably
/// past a repositioning pause. It is also short enough that a genuine one-shot resolution change (window
/// snapped to a size, a display change) reaches its crisp resolution promptly. The static-share poll
/// timer (`SCREEN_STATIC_REENCODE_POLL_MS` = 150ms) bounds the apply latency on a resize-then-STATIC
/// share to ≤ `settle_ms` + one poll.
const SCREEN_DIM_SETTLE_MS: f64 = 400.0;

/// Settle-gate tracker for source-dimension-driven screen-encoder reconfigures (issue #1922). Pure +
/// host-testable (a sibling of [`ScreenFloorAccount`]) so the gate's transitions are pinned by native
/// unit tests off the wasm-only encode loop.
///
/// It observes the RAW source dims of each captured frame and answers "have the source dims held steady
/// long enough to apply a `configure()`?". The two source-dimension reconfigure sites (the base encoder
/// and each active simulcast rung) gate their existing fitted-dim drift check on [`Self::is_settled`],
/// and the static-share timer branch reads [`Self::settled_dims`] to apply the final dims once when a
/// drag ends on content that produces no further frame.
///
/// ## Lifecycle
/// This is transient per-encoder-session state (like `current_encoder_width`), NOT floor accounting: it
/// is declared INSIDE the encode loop's `'restart` scope so a restart / new share session starts it
/// fresh ([`Self::new`]) and a stale pending target from a dead track can never carry into a rebuilt
/// encoder. It deliberately does NOT survive a restart (unlike [`ScreenFloorAccount`], which must).
#[derive(Clone, Copy, Debug, PartialEq)]
struct DimensionSettle {
    /// The last VALID (both axes > 0) raw source dims observed, with the wall-clock ms
    /// (`performance.now()`) at which they last CHANGED to this value. `None` until the first valid
    /// frame. Invalid (0-axis) dims are never stored — see [`Self::observe`].
    seen: Option<DimSample>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DimSample {
    w: u32,
    h: u32,
    /// When `(w, h)` was last set to its current value; the settle window is measured from here.
    changed_at_ms: f64,
}

impl DimensionSettle {
    /// Fresh per encoder session / `'restart`: no source dims observed yet, so nothing is ever settled
    /// until a real frame arrives.
    fn new() -> Self {
        Self { seen: None }
    }

    /// Feed the RAW source dims (`VideoFrame::display_width()/display_height()`) of a just-arrived
    /// frame at `now_ms`.
    ///
    /// - 0/invalid dims are IGNORED — a transient 0×0 frame (a minimized/occluded capture) is NOT a
    ///   settle event and must not re-arm the timer or overwrite the last valid dims (issue #1922
    ///   req 4d). This is what makes a transient invalid frame mid-hold leave an already-settled value
    ///   intact rather than resetting its clock.
    /// - A CHANGE (different from the last valid dims, or the first valid dims) re-arms the settle timer
    ///   by stamping `changed_at_ms = now_ms`.
    /// - An UNCHANGED value leaves `changed_at_ms` in place so the steady interval keeps accumulating.
    fn observe(&mut self, raw_w: u32, raw_h: u32, now_ms: f64) {
        if raw_w == 0 || raw_h == 0 {
            return;
        }
        match self.seen {
            Some(s) if s.w == raw_w && s.h == raw_h => { /* unchanged: keep accumulating steady time */
            }
            _ => {
                self.seen = Some(DimSample {
                    w: raw_w,
                    h: raw_h,
                    changed_at_ms: now_ms,
                });
            }
        }
    }

    /// The settled raw source dims iff the last-observed value has held steady for at least `settle_ms`;
    /// `None` while a drag is still moving the dims, or before any valid frame. The `>=` boundary is
    /// inclusive (matching the sibling keyframe helpers).
    fn settled_dims(&self, now_ms: f64, settle_ms: f64) -> Option<(u32, u32)> {
        match self.seen {
            Some(s) if now_ms - s.changed_at_ms >= settle_ms => Some((s.w, s.h)),
            _ => None,
        }
    }

    /// Bool convenience for the frame-arrival reconfigure gate: are the source dims settled now?
    fn is_settled(&self, now_ms: f64, settle_ms: f64) -> bool {
        self.settled_dims(now_ms, settle_ms).is_some()
    }
}

// ── Sender-side screen encoder stall detection (discussion 1960, issue 2) ──────
// Field evidence (meeting_sync 2026-07-24): the sharer's 3840×1600 capture froze the JS main thread
// for 5 windows of 26–130s. During each freeze the encode loop could not run; on resume it answered
// receiver PLIs by re-encoding the RETAINED (now 60–142s-old) frame, so receivers saw `fps > 0` on
// minutes-stale content. This is the SENDER-side twin of the receiver-side issue-1851 tick-starvation
// pattern in `videocall-codecs::jitter_buffer` (see its `TICK_STARVATION_GAP_MS`).

/// Tick-starvation gap threshold (ms) for the screen encode loop (discussion 1960, issue 2). The
/// loop's `select` re-resolves at least every [`SCREEN_STATIC_REENCODE_POLL_MS`] (150ms) on a static
/// share (the timer arm fires) or FASTER on a moving share (a real frame arrives), so while the JS
/// main thread is alive the loop ticks far under this bound. A gap ABOVE this between two consecutive
/// ticks means the main thread FROZE — it could not even poll its own 150ms timer — i.e. the
/// compositor/GPU stall the field observed on the ultra-wide capture.
///
/// The stall SIGNAL is the loop TICK, deliberately NOT the gap since the last real captured frame: a
/// real `getDisplayMedia` track delivers frames ONLY on visual change (the very reason the timer arm
/// exists — see [`SCREEN_STATIC_REENCODE_POLL_MS`]), so a legitimately STATIC share produces NO real
/// frames for minutes. A real-frame gap would therefore false-positive on every static share; the loop
/// tick cannot, because the timer arm keeps the tick alive at 150ms even when no frame arrives. This
/// mirrors the receiver-side signal choice (issue 1851) for the identical reason — a poll loop's own
/// heartbeat is the only stall signal that survives a legitimately quiet input.
///
/// 2000ms matches the receiver-side `TICK_STARVATION_GAP_MS` exactly: ~13× the 150ms nominal tick, so
/// ordinary jitter, a slow reconfigure, or Chrome's baseline ~1s backgrounded-tab timer clamp never
/// trips it, while any genuine multi-second freeze does.
///
/// One residual FALSE-POSITIVE source is worth naming: Chrome's INTENSIVE throttling clamps background
/// timers to wake at most ~once per 60s after a tab has been hidden ~5 min. Capture-active tabs are
/// USUALLY exempt (an active getDisplayMedia capture keeps the page "playing"), but the exemption is
/// version/platform-dependent and not guaranteed — so a backgrounded STATIC share could see a ~60s
/// timer-arm gap and register a stall it did not truly suffer. The damage is deliberately bounded to
/// observability: a false episode inflates ONLY the two telemetry atomics and emits the rate-limited
/// warn. It CANNOT cause keyframe spam — the one-shot fresh-keyframe latch is consumed ONLY by a REAL
/// captured frame (see the real-frame arm's `take_resume_force`), and a static share produces none, so
/// the latch stays armed and idle until real content next arrives. A backgrounded MOVING share is
/// unaffected: capture-driven frames resolve the `select`'s read future rather than a clamped timer, so
/// as long as frames flow the tick stays fast and no false stall registers.
///
/// Consumer note (issue 1972): when folding the stall exporters into the health packet, disambiguate a
/// background-throttle gap from a real compositor freeze — e.g. correlate the episode with
/// `document.hidden` / page-visibility at emit time — so a merely-hidden static share is not charted as
/// a freeze.
const SCREEN_ENCODER_STALL_GAP_MS: f64 = 2000.0;
/// Compile-time guard: the stall gap MUST sit well above the loop's own nominal poll cadence, or a
/// routine static-share tick would be misread as a stall. Mirrors the issue-1851 ordering assert.
const _: () = assert!(SCREEN_ENCODER_STALL_GAP_MS > SCREEN_STATIC_REENCODE_POLL_MS as f64);

/// Retained-frame staleness ceiling (ms) for the PLI-answer honesty warn (discussion 1960, issue 2).
/// When the timer arm answers a receiver PLI by re-encoding the RETAINED frame (the issue-1841
/// on-demand path) and that frame is older than this, the "fps > 0 but content minutes stale" freeze
/// is in progress: the receiver gets a keyframe, but of pre-stall content. We LOG this (rate-limited)
/// so field analysis can attribute the symptom directly — but do NOT refuse the re-encode: a stale
/// frame still beats a black frame (the receiver keeps the last good content instead of losing it), and
/// the real recovery is the fresh capture-path keyframe forced on stall resume, not withholding this
/// one.
///
/// 10_000ms sits well past any legitimate coalescing/floor window (the floor cadence is 3s and its
/// post-quiet budget spans ~12s), so a normal share re-encoding its current retained frame stays
/// quiet. The one benign case this can still log is a genuinely static share whose late-joiner PLI is
/// answered with an old-but-CORRECT frame — which is why the emit is rate-limited and the
/// episode/gap telemetry (not this line alone) is the authoritative stall signal.
const SCREEN_RETAINED_STALE_MS: f64 = 10_000.0;
/// Minimum wall-clock gap (ms) between the rate-limited retained-stale warn lines (~1 per 5s).
const SCREEN_RETAINED_STALE_LOG_THROTTLE_MS: f64 = 5_000.0;

/// Sender-side encoder tick-starvation monitor (discussion 1960, issue 2) — the pure, host-testable
/// core of the stall detector, mirroring the receiver-side issue-1851 tick-gap decision. Holds the
/// previous loop-tick wall-clock and a one-shot "force a fresh keyframe on the next real frame" latch.
///
/// The wasm encode loop calls [`Self::tick`] once per `'encode` iteration — the loop's heartbeat,
/// which on a static share is the 150ms timer arm and on a moving share is the frame arm, so
/// consecutive ticks are ≤150ms apart while the main thread runs. When a real captured frame next
/// arrives, the loop folds [`Self::take_resume_force`] into the keyframe decision so the FRESH frame
/// (never the retained one) is emitted as a keyframe. Declared INSIDE the encoder `'restart` scope so a
/// restart resets the heartbeat: a restart is a deliberate encoder rebuild, not a main-thread freeze,
/// and its first post-restart tick must not be misread as a stall resume.
#[derive(Debug, Clone, Copy, PartialEq)]
struct EncoderStallMonitor {
    /// `performance.now()` of the previous loop tick; `None` before the first tick (cold start / first
    /// tick after a restart), where there is no prior tick to measure a gap against.
    last_tick_ms: Option<f64>,
    /// One-shot latch: `true` once a stall resume has been detected, cleared when the next real frame
    /// consumes it via [`Self::take_resume_force`]. Not a counter — it forces AT MOST one fresh
    /// keyframe per resume and cannot wedge (on a share that goes truly static right after a resume it
    /// simply forces a keyframe on whatever real frame arrives next, which is harmless).
    resume_force_keyframe: bool,
}

impl EncoderStallMonitor {
    fn new() -> Self {
        Self {
            last_tick_ms: None,
            resume_force_keyframe: false,
        }
    }

    /// Register one loop tick at `now_ms`. Returns `Some(gap_ms)` when this tick RESUMES from a stall
    /// (a prior tick exists AND the gap since it exceeds `gap_ms`), else `None`. Always advances the
    /// tick anchor; on a stall resume it ARMS the one-shot fresh-keyframe latch. A `None` prior tick is
    /// never a stall (nothing to measure against), so a cold start / first post-restart tick cannot
    /// false-positive. The `>` boundary matches the receiver-side issue-1851 gate.
    fn tick(&mut self, now_ms: f64, gap_ms: f64) -> Option<f64> {
        let resumed = match self.last_tick_ms {
            Some(last) if now_ms - last > gap_ms => Some(now_ms - last),
            _ => None,
        };
        self.last_tick_ms = Some(now_ms);
        if resumed.is_some() {
            self.resume_force_keyframe = true;
        }
        resumed
    }

    /// Consume the one-shot fresh-keyframe latch: returns `true` at most once per armed resume, then
    /// disarms. The next real captured frame folds this into the periodic-keyframe input so it emits a
    /// FRESH capture-path keyframe (never the retained frame), bypassing the PLI cooldown exactly once.
    fn take_resume_force(&mut self) -> bool {
        let armed = self.resume_force_keyframe;
        self.resume_force_keyframe = false;
        armed
    }
}

/// Pure decision for the retained-frame staleness warn (discussion 1960, issue 2). Returns `true` iff
/// a PLI is being answered by a retained frame whose age exceeds `stale_ms` AND the rate-limit window
/// (`throttle_ms` since `last_warn_ms`) has elapsed. Extracted so the age gate AND the rate limit are
/// pinned together by a native test. It decides only whether to LOG — never whether to serve the
/// re-encode (the caller always serves it; a stale frame beats a black frame). The `>` age boundary is
/// strict (an age exactly at the ceiling is not yet "stale"); the throttle boundary is `>=` inclusive,
/// matching the file's other throttle gates.
fn retained_stale_warn_due(
    retained_age_ms: f64,
    stale_ms: f64,
    now_ms: f64,
    last_warn_ms: Option<f64>,
    throttle_ms: f64,
) -> bool {
    if retained_age_ms <= stale_ms {
        return false;
    }
    match last_warn_ms {
        None => true,
        Some(last) => now_ms - last >= throttle_ms,
    }
}

/// Apply the state changes a screen-share STOP must make to the shared atoms
/// (issue #2147).
///
/// Extracted from the track `onended` closure — the path the browser's own "Stop
/// sharing" button takes — because that closure lives inside a `spawn_local`
/// future and is therefore unreachable from a unit test. Everything in it that
/// touches shared state lives here so the transition is pinned.
///
/// The `current_fps` reset is the #2147 addition and the reason this exists: that
/// atom is now exported as `screen_encoder_output_fps` → the deliberately
/// ungated `videocall_screen_encoder_output_fps` gauge. `stop()`, `start()` and
/// `start_with_stream()` already reset it, but this path did not, and it could not
/// rely on the AQ loop's `SCREEN_ENCODER_FPS_IDLE_DECAY_MS` backstop because that
/// loop exits once its liveness token drops (Host unmount) — leaving the gauge
/// holding a stale NONZERO that asserts a live screen encoder which had stopped.
fn apply_screen_share_stopped(enabled: &AtomicBool, sharing: &AtomicBool, current_fps: &AtomicU32) {
    enabled.store(false, Ordering::Release);
    sharing.store(false, Ordering::Release);
    crate::encode::reset_output_fps(current_fps);
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

/// Sets the WebCodecs `framerate` rate-control hint on a [`VideoEncoderConfig`]
/// (issue #1832).
///
/// This is a rate-control BUDGETING hint: it tells the encoder's rate controller
/// how many frames per second the configured target bitrate is meant to be
/// spread across, so it budgets bitrate-per-frame for the screen share's slow
/// 5–10 fps cadence instead of assuming a fast (~30/60 fps) default. It does NOT
/// change the bitrate CAP ([`VideoEncoderConfig::set_bitrate`]) or any adaptation
/// path — but it is NOT byte-neutral: the encoder now SPENDS more of the existing
/// budget instead of leaving it unspent, so static screen keyframes grow toward
/// the provisioned target (measured ~52KB → ~158KB, ~3×) and shared text/code
/// stops looking soft. Without the hint the rate controller budgets blind to the
/// cadence and starves each frame of bits even when budget is available.
///
/// SIBLING-PATH NOTE (camera↔screen): the camera encoder ALREADY sets this hint
/// from each layer's `target_fps` (see `camera_encoder.rs`, `Reflect::set(...,
/// "framerate", ...)`). Setting it here brings the screen encoder to PARITY, so
/// this introduces no camera↔screen divergence — it CLOSES one.
///
/// web-sys' `VideoEncoderConfig` exposes no `framerate` setter, so it is set via
/// `Reflect`, the same established pattern as [`set_vbr_mode`]'s `bitrateMode`.
fn set_framerate_hint(config: &VideoEncoderConfig, fps: u32) {
    let _ = Reflect::set(
        config,
        &JsValue::from_str("framerate"),
        &JsValue::from_f64(fps as f64),
    );
}

/// Resolve the ACTIVE screen tier's `target_fps` (issue #1832) for the BASE
/// (single-stream / layer-0) screen encoder from the live
/// `shared_screen_tier_index`.
///
/// The rung's own `target_fps`; the index is clamped into the ladder.
fn active_screen_tier_fps(tier_index: u32) -> u32 {
    let idx = (tier_index as usize).min(SCREEN_QUALITY_TIERS.len().saturating_sub(1));
    SCREEN_QUALITY_TIERS[idx].target_fps
}

/// One AQ tick of the screen share's WebTransport uplink-DROP self-congestion
/// axis (#1199). Given the cumulative `unistream_drop_count()` reading, the
/// window snapshot, and elapsed window time, return the
/// [`SelfCongestionDecision`] under the WebTransport DROP window/threshold
/// (`WT_SELF_CONGESTION_WINDOW_MS` / `WT_SELF_CONGESTION_DROP_THRESHOLD`).
///
/// Extracted from the wasm-only AQ loop (which depends on `js_sys::Date::now()`)
/// so the encoder's choice of signal + constants is pinned by a NATIVE
/// `#[test]`, mirroring the camera encoder. The screen share is frequently the
/// heaviest egress in a call, so this axis matters at least as much here. The
/// loop calls this with
/// `videocall_transport::webtransport::unistream_drop_count()` as `current`.
#[inline]
fn wt_drop_step_down_decision(
    current_drops: u64,
    snapshot_drops: u64,
    elapsed_ms: f64,
) -> videocall_aq::constants::SelfCongestionDecision {
    use crate::adaptive_quality_constants::{
        evaluate_self_congestion, WT_SELF_CONGESTION_DROP_THRESHOLD, WT_SELF_CONGESTION_WINDOW_MS,
    };
    evaluate_self_congestion(
        current_drops,
        snapshot_drops,
        elapsed_ms,
        WT_SELF_CONGESTION_WINDOW_MS,
        WT_SELF_CONGESTION_DROP_THRESHOLD,
    )
}

/// One AQ tick of the screen share's WebTransport uplink-SATURATION axis (#1219
/// prerequisite). Mirrors [`wt_drop_step_down_decision`] but applies the
/// SATURATION window/threshold (`WT_SATURATION_WINDOW_MS` /
/// `WT_SATURATION_STALL_THRESHOLD`) over the slow-`ready()` counter. The loop
/// calls this with
/// `videocall_transport::webtransport::unistream_ready_stall_count()`.
#[inline]
fn wt_saturation_step_down_decision(
    current_stalls: u64,
    snapshot_stalls: u64,
    elapsed_ms: f64,
) -> videocall_aq::constants::SelfCongestionDecision {
    use crate::adaptive_quality_constants::{
        evaluate_self_congestion, WT_SATURATION_STALL_THRESHOLD, WT_SATURATION_WINDOW_MS,
    };
    evaluate_self_congestion(
        current_stalls,
        snapshot_stalls,
        elapsed_ms,
        WT_SATURATION_WINDOW_MS,
        WT_SATURATION_STALL_THRESHOLD,
    )
}

/// One AQ tick of the screen share's #1921 WS FRESHNESS-GATE self-congestion
/// axis (#5). Given the cumulative `screen_ws_stale_delta_drops()` reading, the
/// window snapshot, and elapsed window time, returns the
/// [`SelfCongestionDecision`] under the screen freshness-drop window/threshold
/// (`SCREEN_WS_STALE_DROP_WINDOW_MS` / `SCREEN_WS_STALE_DROP_THRESHOLD`).
///
/// Closes the gap the #1921 send-side gate opened: the gate converges the WS
/// backlog BELOW the 1MB cap that feeds `websocket_drop_count()` (axis #2's
/// signal), so under sustained high-motion congestion axis #2 never fires and
/// the tier stays pinned high while the encoder wastes CPU on discarded deltas.
/// A SUSTAINED cluster of freshness-gate drops signals the encoder to back off
/// toward the achievable rate. Extracted from the wasm-only AQ loop so the
/// signal + constants are pinned by a native `#[test]`, mirroring the WT axes.
#[inline]
fn screen_ws_stale_drop_step_down_decision(
    current_drops: u64,
    snapshot_drops: u64,
    elapsed_ms: f64,
) -> videocall_aq::constants::SelfCongestionDecision {
    use crate::adaptive_quality_constants::{
        evaluate_self_congestion, SCREEN_WS_STALE_DROP_THRESHOLD, SCREEN_WS_STALE_DROP_WINDOW_MS,
    };
    evaluate_self_congestion(
        current_drops,
        snapshot_drops,
        elapsed_ms,
        SCREEN_WS_STALE_DROP_WINDOW_MS,
        SCREEN_WS_STALE_DROP_THRESHOLD,
    )
}

/// User-configurable adaptive-quality tier bounds for SCREEN SHARE (issue #961
/// follow-up), shared from the UI into the running screen encoder control loop.
///
/// QUALITY IS THE INVERSE OF INDEX: `best` = a FLOOR on the index, `worst` = a
/// CAP on it, `None` = "Auto". One screen rung, so both bounds resolve to it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScreenQualityTierBounds {
    /// Floor on the tier index (user MAX quality). `None` = Auto.
    pub best: Option<usize>,
    /// Cap on the tier index (user MIN quality). `None` = Auto.
    pub worst: Option<usize>,
}

/// Shared, mutable screen quality-bounds preference plus a "dirty" generation
/// counter. Same live-reconfig pattern as the camera encoder's
/// `SharedQualityBounds`: the UI writes via
/// `ScreenEncoder::set_quality_tier_bounds` (updating `bounds` + bumping
/// `generation`); the screen encoder control loop reads `generation` each tick
/// and applies `bounds` to the live `EncoderBitrateController` when it advanced.
/// Because the control loop is spawned once and outlives individual share
/// sessions, stored bounds are also (re)applied to the controller whenever the
/// next share starts — the loop just sees the controller's persistent tier and
/// clamps it.
#[derive(Debug, Default)]
struct SharedScreenQualityBounds {
    bounds: ScreenQualityTierBounds,
    /// Monotonic counter bumped on every write so the loop detects changes
    /// without comparing every field.
    generation: u64,
}

/// A real-time snapshot of the SCREEN encoder's current adaptive-quality state,
/// sized for the UI VU meter needle (issue #961 follow-up).
///
/// Video-only — screen share carries no audio. All fields are resolved from the
/// live shared atomics + `SCREEN_QUALITY_TIERS` at call time, indices clamped, so
/// the call is panic-safe and cheap enough to poll each render tick. The UI gets
/// `None` (not this struct) from [`ScreenEncoder::live_screen_snapshot`] while
/// not sharing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenQualitySnapshot {
    /// The width (px) the encoder is CONFIGURED at. `0` before the first configure.
    pub width: u32,
    /// The height (px) the encoder is configured at. See [`Self::width`].
    pub height: u32,
    /// The encoder's framerate hint.
    pub fps: u32,
    /// Live encoder target bitrate (kbps) — the real-time needle value.
    pub target_bitrate_kbps: u32,
    /// The captured surface is LARGER than the encode ceiling, so `width`/`height`
    /// are the ceiling-fitted size.
    pub capture_capped: bool,
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
    current_bitrate: Rc<ScreenEffectiveBitrate>,
    current_fps: Arc<AtomicU32>,
    last_layer0_chunk_ms: Arc<AtomicU64>,
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
    /// When set to `true`, the screen AQ control loop calls
    /// `force_congestion_cut()` on its next tick. Set by the `VideoCallClient`
    /// when a server CONGESTION signal targeting us arrives (issue #1199).
    ///
    /// This MIRRORS the camera's `congestion_step_down` flag (see
    /// `CameraEncoder::set_congestion_step_down_flag`), but is a SEPARATE atom
    /// per encoder — exactly like the split `force_camera_keyframe` /
    /// `force_screen_keyframe` flags. A single shared flag would be a bug: each
    /// AQ loop consumes its flag with `swap(false)`, so two loops sharing one
    /// flag would race and only one would ever observe a given CONGESTION
    /// signal. With one flag per encoder the client sets BOTH, and the camera
    /// and screen loops each clear their own — every live publisher steps down.
    congestion_step_down: Arc<AtomicBool>,
    /// Holds the *original* video track returned by getDisplayMedia so that `stop()` can call
    /// `.stop()` on it directly.  The browser's native screen-share indicator bar (the
    /// "You are sharing" bar with "Stop sharing" / "Hide") is only dismissed when the
    /// original capture track is stopped; stopping a cloned track (e.g. from
    /// `MediaStream::clone()`) does **not** affect the indicator.
    active_video_track: Rc<RefCell<Option<MediaStreamTrack>>>,
    /// Shared flag for cross-stream bandwidth coordination. Set to `true` when
    /// screen capture starts, `false` when it stops. The `CameraEncoder` reads
    /// this to drop ITS OWN quality tier and prevent bandwidth contention.
    screen_sharing_active: Rc<AtomicBool>,
    /// Signal set by ConnectionManager when a server re-election completes.
    /// Consumed by the screen encoder control loop to suppress false crash
    /// ceiling arming during the transient.
    reelection_completed_signal: Rc<AtomicBool>,
    /// Forced-keyframe cooldown reset (issue #1311, SCREEN half — camera was done
    /// in #1348). A one-shot edge that tells the ENCODE loop to clear its
    /// `last_keyframe_emit_ms` cooldown clock so the FIRST post-reconnect /
    /// post-re-election PLI emits a forced keyframe immediately, regardless of how
    /// recently a keyframe went out pre-transition.
    ///
    /// Why a SEPARATE atom rather than reusing `reelection_completed_signal`: the
    /// re-election signal is consumed by the QUALITY task (`.swap(false)` at the
    /// `notify_reelection_completed()` site), and that signal is SHARED with the
    /// CAMERA encoder's quality task (both call
    /// `set_reelection_completed_signal(client.reelection_completed_signal())` in
    /// the host), so whichever quality task swaps first wins the edge. The screen
    /// `last_keyframe_emit_ms` lives in a DIFFERENT `spawn_local` ENCODE task.
    /// Having the encode loop ALSO `.swap` that shared signal would add a third
    /// racing consumer that loses the edge unpredictably. This dedicated atom is
    /// consumed only by the screen encode loop and ARMED from two complementary
    /// sources:
    ///
    /// * RECONNECT **and** RE-ELECTION (primary, race-free): the client's
    ///   `Connected` lifecycle callback unconditionally stores `true` via
    ///   [`Self::keyframe_cooldown_reset`]. Both a full reconnect and a re-election
    ///   re-emit `ConnectionState::Connected`, so this single client-side arm covers
    ///   BOTH transitions. A full reconnect does NOT drive
    ///   `reelection_completed_signal` (it runs `reset_and_start_election`, clearing
    ///   `old_active_connection`), so keying off that signal alone would miss
    ///   reconnects. Wired beside the camera reset arm so both encoders reset
    ///   together on the same `Connected` transition.
    /// * RE-ELECTION (secondary, no plumbing): the screen quality task also arms it
    ///   where it consumes `reelection_completed_signal`. Redundant with the client
    ///   arm on a winning swap, and harmless when it loses (the client arm still
    ///   fires); kept because it is the zero-plumbing in-encoder path and
    ///   self-documents the coupling at the re-election consume site.
    ///
    /// The encode loop `.swap(false)`-consumes this each frame; a duplicate arm is
    /// idempotent and only matters when a PLI is pending. It NEVER forces an
    /// unrequested keyframe — it only un-gates an already-pending PLI, and the
    /// periodic GOP is unaffected.
    keyframe_cooldown_reset: Rc<AtomicBool>,
    /// Index into [`SCREEN_QUALITY_TIERS`], which holds a single rung.
    shared_screen_tier_index: Rc<AtomicU32>,
    /// Tier transition events buffer, drained by health reporter.
    shared_tier_transitions: Rc<RefCell<Vec<TierTransitionRecord>>>,
    /// The encoder's live target bitrate (kbps), stamped on every `VideoMetadata`.
    shared_screen_encoder_target_bitrate_kbps: Rc<AtomicU32>,
    /// The geometry the encode loop last CONFIGURED an encoder at; `0` before that.
    shared_encode_width: Rc<AtomicU32>,
    /// See [`Self::shared_encode_width`].
    shared_encode_height: Rc<AtomicU32>,
    /// The live CAPTURE size reported by the track; `Arc` so `SourceDims` shares it.
    shared_capture_width: Arc<AtomicU32>,
    /// See [`Self::shared_capture_width`].
    shared_capture_height: Arc<AtomicU32>,
    /// User-configurable screen-share quality tier bounds (issue #961 follow-up),
    /// which resolve to the single rung. Written by the UI via
    /// [`Self::set_quality_tier_bounds`], read by the screen encoder control
    /// loop. See [`SharedScreenQualityBounds`] for the
    /// apply mechanism and [`ScreenQualityTierBounds`] for the index↔quality
    /// inversion.
    quality_bounds: Rc<RefCell<SharedScreenQualityBounds>>,
    /// The layer ceiling the UI asked for; [`clamp_screen_layer_count`] clamps it.
    max_layers: u32,
    /// Number of screen layers currently active. Always 1; reset on every (re)share.
    shared_active_layer_count: Rc<AtomicU32>,
    /// Per-layer target bitrate (bps). Empty; the one layer uses `current_bitrate`.
    shared_layer_bitrates_bps: Rc<RefCell<Vec<Rc<AtomicU32>>>>,
    /// Sender-side screen encoder backpressure (issue #1108, Phase B): the max
    /// `VideoEncoder::encode_queue_size()` of the screen encoder, written by the
    /// encode loop each frame and read
    /// by the screen AQ control loop to feed
    /// [`EncoderBitrateController::observe_encoder_queue_depth`]. Borrow-safe
    /// bridge between the encode task (owns the encoders) and the control task
    /// (owns the controller). **Stage 1: stored-only on the controller side, so
    /// it is observability with no behavior change.**
    shared_encoder_queue_depth: Rc<AtomicU32>,
    /// Relay layer-union hint for this publisher's SCREEN ladder (issue #1108,
    /// Stage 3). Mirror of `CameraEncoder::shared_union_requested_layer` for the
    /// SCREEN media-kind: the relay delivers the MAX simulcast layer ANY receiver
    /// wants for this (publisher, SCREEN) on the publisher's own self-subject via
    /// a `LAYER_HINT` packet, `VideoCallClient`'s dispatch arm writes it here, and
    /// the screen AQ control loop reads it each tick and feeds
    /// [`EncoderBitrateController::observe_union_requested_layer`] to cap the
    /// published screen layer.
    ///
    /// **Initialized to [`u32::MAX`] = fail-open (no cap)** and reset to
    /// `u32::MAX` on reconnect so a stale cap from the old relay cannot suppress
    /// against a new session.
    shared_union_requested_layer: Rc<AtomicU32>,
    /// User SEND layer-ceiling for this publisher's SCREEN ladder (perf-panel
    /// "layers published" thumb). Mirror of
    /// `CameraEncoder::shared_user_layer_ceiling` for the SCREEN media-kind: the
    /// performance panel writes the user-selected layer COUNT here (via
    /// [`Self::set_user_layer_ceiling`]) and the screen AQ control loop reads it
    /// each tick and feeds [`EncoderBitrateController::observe_user_layer_ceiling`]
    /// to cap the published screen layer as a further `min` alongside the union
    /// hint.
    ///
    /// **Initialized to [`u32::MAX`] = fail-open (Auto / no user cap).** The base
    /// layer is always published (the AQ side floors the cap at 1).
    shared_user_layer_ceiling: Rc<AtomicU32>,
    /// Screen video-at-floor flag (issue #1611): `true` when the governed screen
    /// bitrate can no longer decrease. Written only by the ENCODE loop — cleared
    /// at share start, then stored after every `uplink_governor.observe`.
    ///
    /// `Arc` (not `Rc`) because it crosses encoder boundaries into the mic.
    screen_at_floor_flag: Arc<AtomicBool>,
    /// Issue #2179: "the AQ controller must adopt the tier the encoder just
    /// resolved from the captured source". ARMED by [`apply_initial_tier_to`]
    /// (i.e. by every `start` / `start_with_stream`), CONSUMED by the AQ control
    /// loop on its next tick while sharing. See the field of the same name on
    /// [`InitialTierTargets`] for why this is a flag and not a rising edge.
    initial_tier_pending: Arc<AtomicBool>,
    /// Screen-sharing-active flag mirrored as `Arc<AtomicBool>` (issue #1611).
    /// Written at the same points as the `Rc<AtomicBool>` `screen_sharing_active`:
    /// share start (`run_screen_encoding` → `true`), normal stop (`stop()` → `false`),
    /// failure teardown (`cleanup_on_error` → `false`), and final cleanup (end of
    /// `run_screen_encoding` → `false`). Exists solely because the mic encoder
    /// trait requires `Arc` and the primary flag is `Rc`. On wasm32 the distinction
    /// is academic (single-threaded), but the type system enforces it. The host
    /// passes this to the mic via [`MicrophoneEncoder::set_screen_sharing_active_signal`].
    screen_sharing_active_arc: Arc<AtomicBool>,
    /// Liveness token bounding the AQ control-loop `spawn_local` future (issue
    /// #1108). The encoder holds the only strong reference; `set_encoder_control`
    /// captures a [`Weak`] and breaks its 1 Hz `tick` loop once `upgrade()`
    /// returns `None`. The control loop runs on
    /// `wasm_bindgen_futures::spawn_local` (NOT scope-bound), so without this it
    /// would tick forever and leak one loop per remount. Bounds the CONTROL loop
    /// only — the encode loop (`run_screen_encoding`) already exits on
    /// `enabled == false`.
    control_loop_liveness: Rc<()>,
    /// Scope-owned AQ-loop cancellation `Host` trips on unmount (issue #2458).
    control_loop_cancel: AqControlLoopCancel,
}

/// Clear both screen-sharing flags (Rc + Arc mirror) atomically (issue #1611).
///
/// Called from all teardown paths: `stop()`, `cleanup_on_error` closure, and
/// final `run_screen_encoding` cleanup. Exists as a module-scope helper so the
/// regression test can invoke the SAME dual-store the production code uses —
/// test-only hand-written stores would be tautological (the anti-pattern named
/// in CLAUDE.md adversarial check 2).
fn clear_screen_sharing_flags(rc: &Rc<AtomicBool>, arc: &Arc<AtomicBool>) {
    rc.store(false, Ordering::Release);
    arc.store(false, Ordering::Release);
}

/// Seed the mic-facing screen-floor signal at share start, before any sample.
fn seed_screen_floor_signal(flag: &Arc<AtomicBool>) {
    flag.store(false, Ordering::Release);
}

/// SOLE producer of `screen_at_floor_flag`, read by the mic's backstop gate.
fn publish_screen_floor_signal(
    flag: &Arc<AtomicBool>,
    governor: &ScreenUplinkGovernor,
    baseline: ScreenBaselineKbps,
) {
    flag.store(governor.at_floor(baseline), Ordering::Release);
}

/// Clear the sharing flags AND zero the output-fps atom (issue #2147).
///
/// Every "the share is over" path must land here rather than on
/// [`clear_screen_sharing_flags`] alone. `current_fps` is exported as
/// `screen_encoder_output_fps` → the deliberately ungated
/// `videocall_screen_encoder_output_fps` gauge, so a path that clears the flags but
/// leaves the atom nonzero makes the gauge assert a live screen encoder for a share
/// that has ended.
///
/// The AQ loop's `SCREEN_ENCODER_FPS_IDLE_DECAY_MS` (5 s) is only a backstop, and it
/// stops running once that loop's liveness token drops — while the `HealthReporter`
/// holds an `Arc` clone of this atom and keeps publishing. Covers the paths
/// `apply_screen_share_stopped` does not: `cleanup_on_error`, the MAX_RESTARTS
/// give-up, and the encode loop's final cleanup — the last of which is the
/// `stream_ended` route taken when a track dies WITHOUT `onended` firing (OS/source
/// revoke, monitor unplug, Wayland portal revoke).
fn clear_screen_sharing_state(rc: &Rc<AtomicBool>, arc: &Arc<AtomicBool>, current_fps: &AtomicU32) {
    clear_screen_sharing_flags(rc, arc);
    crate::encode::reset_output_fps(current_fps);
}

impl ScreenEncoder {
    /// Construct a screen encoder:
    ///
    /// * `client` - an instance of a [`VideoCallClient`](crate::VideoCallClient).  It does not need to be currently connected.
    /// * `bitrate_kbps` - initial bitrate in kilobits per second
    /// * `on_encoder_settings_update` - callback for encoder settings updates (e.g., bitrate changes)
    /// * `on_state_change` - callback for screen share state changes (started, cancelled, stopped)
    /// * `screen_sharing_active` - shared coordination flag; obtain from [`CameraEncoder::screen_sharing_flag()`](crate::CameraEncoder::screen_sharing_flag)
    /// * `max_layers` - the layer ceiling the UI asks for, clamped to
    ///   [`SCREEN_SIMULCAST_MAX_SUPPORTED_LAYERS`].
    ///
    /// The encoder is created in a disabled state, [`encoder.set_enabled(true)`](Self::set_enabled) must be called before it can start encoding.
    pub fn new(
        client: VideoCallClient,
        bitrate_kbps: u32,
        on_encoder_settings_update: Callback<String>,
        on_state_change: Callback<ScreenShareEvent>,
        screen_sharing_active: Rc<AtomicBool>,
        max_layers: u32,
    ) -> Self {
        let default_tier = &SCREEN_QUALITY_TIERS[DEFAULT_SCREEN_TIER_INDEX];
        Self {
            client,
            state: EncoderState::new(),
            current_bitrate: Rc::new(ScreenEffectiveBitrate::seed_from_ceiling(bitrate_kbps)),
            current_fps: Arc::new(AtomicU32::new(0)),
            last_layer0_chunk_ms: Arc::new(AtomicU64::new(0)),
            on_encoder_settings_update: Some(on_encoder_settings_update),
            on_state_change: Some(on_state_change),
            screen_stream: Rc::new(RefCell::new(None)),
            tier_max_width: Rc::new(AtomicU32::new(default_tier.max_width)),
            tier_max_height: Rc::new(AtomicU32::new(default_tier.max_height)),
            tier_keyframe_interval: Rc::new(AtomicU32::new(default_tier.keyframe_interval_frames)),
            force_keyframe: Arc::new(AtomicBool::new(false)),
            // Server-CONGESTION step-down flag (issue #1199). Owned per-encoder
            // and shared to the client via `set_congestion_step_down_flag`, like
            // the camera's. Starts cleared.
            congestion_step_down: Arc::new(AtomicBool::new(false)),
            active_video_track: Rc::new(RefCell::new(None)),
            screen_sharing_active,
            reelection_completed_signal: Rc::new(AtomicBool::new(false)),
            // Issue #1311: no reset pending at construction; armed by a re-election
            // (quality task) or a reconnect (client `Connected` callback).
            keyframe_cooldown_reset: Rc::new(AtomicBool::new(false)),
            shared_screen_tier_index: Rc::new(AtomicU32::new(DEFAULT_SCREEN_TIER_INDEX as u32)),
            shared_tier_transitions: Rc::new(RefCell::new(Vec::new())),
            shared_screen_encoder_target_bitrate_kbps: Rc::new(AtomicU32::new(0)),
            shared_encode_width: Rc::new(AtomicU32::new(0)),
            shared_encode_height: Rc::new(AtomicU32::new(0)),
            shared_capture_width: Arc::new(AtomicU32::new(0)),
            shared_capture_height: Arc::new(AtomicU32::new(0)),
            quality_bounds: Rc::new(RefCell::new(SharedScreenQualityBounds::default())),
            max_layers,
            shared_active_layer_count: Rc::new(AtomicU32::new(clamp_screen_layer_count(
                max_layers,
            ))),
            shared_layer_bitrates_bps: Rc::new(RefCell::new(Vec::new())),
            // Sender encoder backpressure (issue #1108, Phase B). Starts at 0
            // (no frames queued); the encode loop publishes the live depth.
            shared_encoder_queue_depth: Rc::new(AtomicU32::new(0)),
            // Relay layer-union hint (issue #1108, Stage 3). Starts at u32::MAX
            // (fail-open / no cap); reset to u32::MAX on reconnect.
            shared_union_requested_layer: Rc::new(AtomicU32::new(u32::MAX)),
            // User SEND layer-ceiling (perf-panel). Fail-open: u32::MAX = Auto /
            // no user cap until the panel writes a layer count.
            shared_user_layer_ceiling: Rc::new(AtomicU32::new(u32::MAX)),
            screen_at_floor_flag: Arc::new(AtomicBool::new(false)),
            // Issue #2179: no tier seed pending at construction — nothing has
            // started a share yet, so there is no captured source to match.
            initial_tier_pending: Arc::new(AtomicBool::new(false)),
            // Issue #1611: Arc mirror of screen_sharing_active for mic encoder.
            screen_sharing_active_arc: Arc::new(AtomicBool::new(false)),
            // AQ control-loop liveness token (issue #1108). Sole strong owner;
            // the self-tick loop holds a Weak and exits when this drops.
            control_loop_liveness: Rc::new(()),
            control_loop_cancel: AqControlLoopCancel::new(),
        }
    }

    pub fn control_loop_cancel_token(&self) -> AqControlLoopCancel {
        self.control_loop_cancel.clone()
    }

    /// Effective number of screen simulcast layers to encode this session.
    /// Clamps the caller-supplied `max_layers` to `[1, MAX]`. Default 1.
    fn effective_layer_count(&self) -> u32 {
        clamp_screen_layer_count(self.max_layers)
    }

    /// Replace the internal re-election completed signal with an externally-owned one.
    pub fn set_reelection_completed_signal(&mut self, signal: Rc<AtomicBool>) {
        self.reelection_completed_signal = signal;
    }

    /// Returns a shared reference to the forced-keyframe cooldown reset (issue
    /// #1311, SCREEN half).
    ///
    /// The atom is OWNED by this `ScreenEncoder` (not the client) — same ownership
    /// direction as [`Self::shared_union_requested_layer`]. The host hands this
    /// clone to the `VideoCallClient`, which SETS it on each `Connected` lifecycle
    /// event (i.e. every reconnect) so the encode loop clears its forced-keyframe
    /// cooldown clock and the first post-reconnect PLI is not coalesced away. The
    /// re-election path SETS the same atom directly from the quality task (no
    /// plumbing) at its `reelection_completed_signal` consume site, so the two
    /// transitions converge on one consumer in the encode loop.
    pub fn keyframe_cooldown_reset(&self) -> Rc<AtomicBool> {
        self.keyframe_cooldown_reset.clone()
    }

    /// Returns the current screen share quality tier index (0 = best, last = worst).
    pub fn shared_screen_tier_index(&self) -> Rc<AtomicU32> {
        self.shared_screen_tier_index.clone()
    }

    /// Returns the relay layer-union hint atomic for this SCREEN ladder (issue
    /// #1108, Stage 3).
    ///
    /// `VideoCallClient` stores this clone (via
    /// [`VideoCallClient::set_screen_union_requested_layer`](crate::VideoCallClient::set_screen_union_requested_layer))
    /// and writes the MAX-requested-layer carried by an inbound `LAYER_HINT`
    /// packet's SCREEN entry into it. The screen AQ control loop reads it each
    /// tick to cap the published ladder. The value is a max-layer **id**
    /// (`u32::MAX` = fail-open / no cap).
    pub fn shared_union_requested_layer(&self) -> Rc<AtomicU32> {
        self.shared_union_requested_layer.clone()
    }

    /// Shared into the [`MicrophoneEncoder`] via
    /// [`MicrophoneEncoder::set_screen_video_exhausted_signal`], so its backstop
    /// gate opens only once screen video can no longer shed.
    pub fn screen_at_floor_flag(&self) -> Arc<AtomicBool> {
        self.screen_at_floor_flag.clone()
    }

    /// Returns the `Arc<AtomicBool>` mirror of `screen_sharing_active` for the
    /// mic encoder (issue #1611). Written at the same points as the `Rc`
    /// version; exists solely because the mic trait requires `Arc`.
    pub fn screen_sharing_active_arc(&self) -> Arc<AtomicBool> {
        self.screen_sharing_active_arc.clone()
    }

    /// Returns the shared tier transitions buffer for health reporting.
    pub fn shared_tier_transitions(&self) -> Rc<RefCell<Vec<TierTransitionRecord>>> {
        self.shared_tier_transitions.clone()
    }

    /// Returns the ACTIVE screen simulcast layer-count atomic (#1561).
    pub fn shared_active_layer_count(&self) -> Rc<AtomicU32> {
        self.shared_active_layer_count.clone()
    }

    /// Returns the effective screen simulcast layer count (#1561).
    pub fn effective_screen_layer_count(&self) -> u32 {
        self.effective_layer_count()
    }

    /// Set user-configurable SCREEN-SHARE quality tier bounds (issue #961
    /// follow-up). This is the public API the Dioxus "Screen Share Thresholds"
    /// slider calls. The arguments are **tier indices** into
    /// `SCREEN_QUALITY_TIERS`, which since issue 2343 holds a single rung.
    ///
    /// **QUALITY IS THE INVERSE OF INDEX — index 0 is the BEST tier.** So:
    /// - `best` = the user's **max quality** = the *best* tier allowed = a
    ///   **FLOOR on the index** (adaptation never steps UP past it).
    /// - `worst` = the user's **min quality** = the *worst* tier allowed = a
    ///   **CAP on the index** (adaptation never steps DOWN past it).
    /// - `None` on any end = "Auto"; passing both `None` restores fully-automatic
    ///   behaviour. When `best == worst` the tier is pinned to that single index.
    ///
    /// Screen share has no audio, so there is no audio bound here. The camera's
    /// [`CameraEncoder::set_quality_tier_bounds`](crate::CameraEncoder::set_quality_tier_bounds)
    /// is a separate setter on a separate encoder object — this one is screen-only.
    ///
    /// Bounds apply live to a running screen encoder at the next diagnostics tick
    /// (≤1s) AND are stored so they are re-applied when the screen encoder
    /// (re)starts on the next share, so the call is valid whether or not screen
    /// sharing is currently active. Out-of-range / inverted ranges are
    /// clamped/normalized inside the AQ manager.
    pub fn set_quality_tier_bounds(&mut self, best: Option<usize>, worst: Option<usize>) {
        let mut shared = self.quality_bounds.borrow_mut();
        shared.bounds = ScreenQualityTierBounds { best, worst };
        shared.generation = shared.generation.wrapping_add(1);
    }

    /// Returns the current user-configured screen quality tier bounds.
    pub fn quality_tier_bounds(&self) -> ScreenQualityTierBounds {
        self.quality_bounds.borrow().bounds
    }

    /// Set the user's SEND layer-ceiling for SCREEN from the performance panel —
    /// the "layers published" control.
    ///
    /// `ceiling` is the maximum number of SCREEN simulcast layers the user wants
    /// this publisher to emit, as a layer COUNT (1 = base only, up to the screen
    /// device ceiling). `None` = Auto / no user cap. Applied LIVE: the screen AQ
    /// control loop reads this atomic each tick (≤1s) and caps the published
    /// screen set as a further `min` alongside the relay union hint; AQ shedding
    /// stays authoritative on the down side and the base layer (layer 0) is always
    /// published (the AQ side floors the cap at 1).
    ///
    /// Valid whether or not screen sharing is currently active; the value persists
    /// in the shared atomic and is re-read by the control loop on every (re)start
    /// of the screen encoder, so it survives a restart / reconnect / re-share with
    /// no re-arming.
    pub fn set_user_layer_ceiling(&self, ceiling: Option<u32>) {
        self.shared_user_layer_ceiling
            .store(ceiling.unwrap_or(u32::MAX), Ordering::Relaxed);
    }

    /// The current user SEND layer-ceiling for SCREEN (layer COUNT), or `None`
    /// for Auto / no user cap. For the UI to render its current selection.
    pub fn user_layer_ceiling(&self) -> Option<u32> {
        match self.shared_user_layer_ceiling.load(Ordering::Relaxed) {
            u32::MAX => None,
            n => Some(n),
        }
    }

    /// Real-time screen adaptive-quality snapshot for the UI VU meter needle
    /// (issue #961 follow-up).
    ///
    /// Returns `None` when screen sharing is NOT active (so the UI can render a
    /// "Not sharing" empty state), and `Some(snapshot)` while sharing.
    ///
    /// `width`/`height` are the last CONFIGURED encoder geometry.
    pub fn live_screen_snapshot(&self) -> Option<ScreenQualitySnapshot> {
        if !self.screen_sharing_active.load(Ordering::Acquire) {
            return None;
        }
        let tier = &SCREEN_QUALITY_TIERS[DEFAULT_SCREEN_TIER_INDEX];
        let target_bitrate_kbps = self
            .shared_screen_encoder_target_bitrate_kbps
            .load(Ordering::Relaxed);
        let (live_w, live_h) = (
            self.shared_capture_width.load(Ordering::Relaxed),
            self.shared_capture_height.load(Ordering::Relaxed),
        );
        let (mut encode_w, mut encode_h) = (
            self.shared_encode_width.load(Ordering::Relaxed),
            self.shared_encode_height.load(Ordering::Relaxed),
        );
        if encode_w == 0 || encode_h == 0 {
            let (w, h) = screen_encode_box_for_capture(live_w, live_h);
            encode_w = w;
            encode_h = h;
        }
        Some(ScreenQualitySnapshot {
            width: encode_w,
            height: encode_h,
            fps: tier.target_fps,
            target_bitrate_kbps,
            capture_capped: capture_exceeds_encode_ceiling(live_w, live_h),
        })
    }

    /// Live SEND-side diagnostics for the screen share (issue #1095). `None` while
    /// not sharing; always single-stream with an empty rung list.
    pub fn live_simulcast_snapshot(&self) -> Option<SimulcastSendSnapshot> {
        if !self.screen_sharing_active.load(Ordering::Acquire) {
            return None;
        }
        Some(SimulcastSendSnapshot {
            simulcast_active: false,
            effective_layers: 1,
            active_layers: 1,
            layers: Vec::new(),
        })
    }

    /// Spawn the screen-encoder AQ control loop (issue #1108: now a self-timer).
    ///
    /// Mirrors `CameraEncoder::set_encoder_control`: receiver FPS no longer
    /// drives the sender AQ, so this ticks at `AQ_TICK_INTERVAL_MS` off the
    /// screen encoder's own backpressure (`shared_encoder_queue_depth`) plus the
    /// re-election signal, instead of consuming a diagnostics channel.
    pub fn set_encoder_control(&mut self) {
        let current_fps = self.current_fps.clone();
        let last_layer0_chunk_ms = self.last_layer0_chunk_ms.clone();
        let on_encoder_settings_update = self.on_encoder_settings_update.clone();
        let enabled = self.state.enabled.clone();
        let shared_screen_tier_idx = self.shared_screen_tier_index.clone();
        let shared_tier_transitions = self.shared_tier_transitions.clone();
        let reelection_completed_signal = self.reelection_completed_signal.clone();
        // Issue #1311: the QUALITY task ARMS this when it consumes a re-election
        // (below, at the `notify_reelection_completed` site); the ENCODE task
        // CONSUMES it per frame to clear `last_keyframe_emit_ms`. Both spawn_local
        // tasks share this same `ScreenEncoder`-owned atom. Mirrors the camera.
        let keyframe_cooldown_reset_quality = self.keyframe_cooldown_reset.clone();
        // Server-CONGESTION step-down flag (issue #1199): the screen AQ loop
        // consumes this with `swap(false)` each tick, mirroring the camera.
        let congestion_flag = self.congestion_step_down.clone();
        // #961 (send quality bounds) + #1082 (screen simulcast) both feed the
        // screen encoder control loop — clone both sides' shared state.
        let quality_bounds = self.quality_bounds.clone();
        let shared_active_layer_count = self.shared_active_layer_count.clone();
        // Issue #1229: the AQ loop must observe share start/stop edges so it can
        // (a) NOT drift the layer ramp up while idle and (b) re-arm cold start
        // on every (re)share. The control loop is spawned once and outlives
        // individual share sessions, so without this it would keep ramping the
        // active layer count up against a clear idle queue and a re-share would
        // start above the base rung (violating the #1200 first-frame contract).
        let screen_sharing_active = self.screen_sharing_active.clone();
        let shared_layer_bitrates_bps = self.shared_layer_bitrates_bps.clone();
        // Sender encoder backpressure (issue #1108, Phase B): the control loop
        // READS the depth the encode loop published and forwards it to the
        // controller on each self-timer tick.
        let shared_encoder_queue_depth = self.shared_encoder_queue_depth.clone();
        // Relay layer-union hint (issue #1108, Stage 3): the control loop READS
        // the max-layer the client wrote (from a LAYER_HINT packet) and forwards
        // it to the controller's union cap each tick.
        let shared_union_requested_layer = self.shared_union_requested_layer.clone();
        // User SEND layer-ceiling (perf-panel): the control loop READS the layer
        // count the UI wrote and forwards it to the controller's user cap each
        // tick, composed as a further `min` alongside the union cap and the ramp.
        let shared_user_layer_ceiling = self.shared_user_layer_ceiling.clone();
        // Issue #2179: the QUALITY task CONSUMES this to adopt the tier the
        // encoder resolved from the captured source.
        let initial_tier_pending = self.initial_tier_pending.clone();
        let control_loop_liveness = Rc::downgrade(&self.control_loop_liveness);
        let control_loop_cancel = self.control_loop_cancel.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let mut encoder_control =
                EncoderBitrateController::new_for_screen(current_fps.clone(), SCREEN_QUALITY_TIERS);

            // Apply any user screen-quality bounds set before the loop started,
            // and track the generation we last applied so we only re-apply when
            // the UI actually changes them (issue #961 follow-up). The clamp
            // logic is generic over whatever tier table it is given.
            let mut applied_bounds_generation = {
                let shared = quality_bounds.borrow();
                encoder_control.set_video_quality_bounds(shared.bounds.best, shared.bounds.worst);
                shared.generation
            };

            // Client-side uplink-backpressure self-trigger windows (issue #1199,
            // mirroring the camera AQ loop). The WS send-buffer drop counter and
            // the WT unistream drop counter are TRANSPORT-GLOBAL statics
            // (`websocket::websocket_drop_count()` /
            // `webtransport::unistream_drop_count()`), shared by the camera,
            // screen, and microphone egress on the SAME connection.
            //
            // DROP-COUNTER ATTRIBUTION DECISION (issue #1199, requirement 3):
            // each controller keeps its OWN baseline snapshot + sliding window
            // against the aggregate counters. The transport also exposes
            // per-stream attribution, but camera and screen intentionally read
            // aggregate distress because a single browser TCP send buffer / QUIC
            // connection is the shared bottleneck. A drop burst is therefore
            // observed independently by BOTH loops, and BOTH may shed a layer.
            // The baselines are SEPARATE only so the loops' sliding windows roll
            // on their own cadence and neither clears the other's accounting;
            // they are not a partition of the drops.
            let mut last_ws_drop_snapshot: u64 =
                videocall_transport::websocket::websocket_drop_count();
            let mut ws_drop_window_start_ms: f64 = js_sys::Date::now();
            let mut last_wt_drop_snapshot: u64 =
                videocall_transport::webtransport::unistream_drop_count();
            let mut wt_drop_window_start_ms: f64 = js_sys::Date::now();
            // Independent sliding window for the WebTransport uplink-SATURATION
            // self-trigger (#1219 prerequisite); SEPARATE from the WT drop window
            // above (drops = teardown; stalls = slow-but-alive uplink). Per the
            // attribution note above, this is the screen loop's OWN baseline
            // against the shared global stall counter — the camera loop has its
            // own. WS users hold the counter flat at 0 → no-op.
            let mut last_wt_stall_snapshot: u64 =
                videocall_transport::webtransport::unistream_ready_stall_count();
            let mut wt_stall_window_start_ms: f64 = js_sys::Date::now();
            // Issue #1921: independent sliding window for the WS FRESHNESS-GATE
            // self-trigger (axis #5). SEPARATE from the WS overflow window above
            // (overflow = 1MB `bufferedAmount` cap → `websocket_drop_count()`;
            // freshness = the send-side screen-delta gate → the screen-local
            // `screen_ws_stale_delta_drops()`). The gate converges the backlog
            // BELOW the 1MB cap, so the overflow axis stays flat under moderate
            // sustained congestion and this axis is the one that then steps the
            // tier down. This counter is screen-encoder-owned (not shared with
            // the camera loop), so unlike the transport-global counters above it
            // needs no cross-loop attribution note.
            let mut last_screen_ws_stale_drop_snapshot: u64 = screen_ws_stale_delta_drops();
            let mut screen_ws_stale_drop_window_start_ms: f64 = js_sys::Date::now();
            // Issue #1229: previous sharing state, used to detect the rising edge
            // of a (re)share inside the loop. Seeded from the CURRENT value at
            // spawn time. `set_encoder_control` is called during encoder setup —
            // BEFORE any share starts — and the first `start()`/`start_with_stream()`
            // flips `screen_sharing_active` to `true` inside `run_screen_encoding`
            // (a SEPARATE future spawned after this one), so this seed is `false`
            // and the FIRST share is a genuine `false -> true` rising edge that
            // re-arms cold start. That re-arm is idempotent on a cold first share
            // (the controller is already at the base rung), so seeding from the
            // live value is correct without special-casing the first share.
            let mut was_sharing = screen_sharing_active.load(Ordering::Acquire);
            // Self-timer AQ loop (issue #1108): tick at AQ_TICK_INTERVAL_MS
            // instead of waiting on receiver diagnostics. Runs for the lifetime
            // of the owning ScreenEncoder.
            loop {
                gloo_timers::future::sleep(std::time::Duration::from_millis(
                    crate::adaptive_quality_constants::AQ_TICK_INTERVAL_MS,
                ))
                .await;
                if crate::encode::aq_control_loop_should_exit(
                    &control_loop_liveness,
                    &control_loop_cancel,
                ) {
                    log::debug!("ScreenEncoder: AQ control loop exiting (dropped or cancelled)");
                    break;
                }
                let now = js_sys::Date::now();
                // #2060 idle-decay: `last_layer0_chunk_ms` is stamped in the base-layer callback
                // with performance().now() (monotonic), so the staleness check MUST use a fresh
                // performance().now() here — NOT the loop's `now` above (js_sys::Date::now(),
                // wall-clock, a DIFFERENT epoch). Do not "simplify" by reusing `now`: a
                // cross-clock comparison silently breaks the decay and no unit test catches it.
                let perf_now = window()
                    .performance()
                    .expect("Performance API not available")
                    .now();
                if let Some(value) = crate::encode::fps_after_idle_decay(
                    current_fps.load(Ordering::Relaxed),
                    perf_now,
                    last_layer0_chunk_ms.load(Ordering::Relaxed) as f64,
                    crate::adaptive_quality_constants::SCREEN_ENCODER_FPS_IDLE_DECAY_MS,
                ) {
                    current_fps.store(value, Ordering::Relaxed);
                }
                // ── Issue #1229: share start/stop edge handling ───────────────
                // Track the previous sharing state so we can (a) re-arm cold
                // start on the RISING edge of a (re)share and (b) avoid drifting
                // the layer ramp UP while idle (no share running). The control
                // loop is spawned once and ticks for the encoder's whole life, so
                // it MUST observe these edges itself — `stop()` does not break it.
                let now_sharing = screen_sharing_active.load(Ordering::Acquire);
                // Compute BOTH edges from the OLD `was_sharing` before reassigning
                // it. A single tick can be at most one of these (a rising and a
                // falling edge are mutually exclusive), so the two are never both
                // true on the same iteration.
                //
                // Sub-tick stop->start: a `stop()` followed by a `start()` fully
                // contained within one AQ tick interval leaves `now_sharing` true
                // at both samples, so `was_sharing` stayed true and NO rising edge
                // is detected — the controller is not re-armed that tick. This is
                // benign: (a) idle drift only accrues over idle TICKS and a sub-tick
                // blip accrues none, and (b) `apply_initial_tier` (called by every
                // `start`/`start_with_stream`) synchronously seeds the encode loop's
                // `shared_active_layer_count` to the base rung regardless, so the
                // re-share still starts at base even with no detected edge here.
                let share_started = !was_sharing && now_sharing;
                let share_stopped = was_sharing && !now_sharing;
                was_sharing = now_sharing;
                // ── Issue #2179: adopt the encoder's source-resolved tier ────
                // `start` / `start_with_stream` resolve the starting tier from
                // the CAPTURED SOURCE size, write it to `shared_screen_tier_idx`
                // and ARM `initial_tier_pending` — all before
                // `run_screen_encoding` flips `screen_sharing_active`. The
                // controller, however, was constructed once (long before any
                // capture existed) at `DEFAULT_SCREEN_TIER_INDEX`, or is holding
                // the previous share's last tier. Without adopting the resolved
                // tier here, the controller's FIRST transition — in either
                // direction — overwrites the encoder's source-matched dims with
                // the first step-up tick and then has to climb back, costing a
                // reconfigure + keyframe each way.
                //
                // Consumed on the first tick where sharing is ACTIVE rather than
                // on the rising edge, because a `stop()`+`start()` pair contained
                // within one tick interval produces no observable edge (see the
                // sub-tick note in the `share_started` block below) yet still
                // needs the seed. `swap` makes it exactly-once per share.
                //
                // Runs BEFORE the `share_started` drain below so the
                // "coordination" record this pushes is discarded along with the
                // rest of the pre-share tail — it is a seed, not an adaptation
                // event, and must not appear in the share's telemetry. The
                // explicit drain here covers the non-rising-edge case.
                if now_sharing && initial_tier_pending.swap(false, Ordering::AcqRel) {
                    let resolved_tier = shared_screen_tier_idx.load(Ordering::Acquire) as usize;
                    let _ = encoder_control.set_initial_video_tier(resolved_tier);
                    let _ = encoder_control.drain_tier_transitions();
                }
                if share_started {
                    // Issue #1229 (telemetry purity): drain and DISCARD any pending
                    // controller tier-transitions so the NEW share's
                    // `shared_tier_transitions` starts from an EMPTY buffer. This is
                    // the real guarantee that the new share's telemetry is clean,
                    // because the user-quality-bounds apply block below runs on EVERY
                    // idle tick (it is before the `now_sharing` gate) and a bounds
                    // change can enqueue a `trigger: "coordination"` record at any
                    // point during the idle gap — INCLUDING after the falling-edge
                    // drain. Any such pre-share record belongs to the idle period,
                    // not this share, so it is discarded here rather than later
                    // drained inside the `now_sharing` block and mis-tagged to this
                    // share's `shared_tier_transitions`. Runs for ALL share starts
                    // (not gated on `n_layers > 1`): the "coordination" record is
                    // pushed even in single-stream mode.
                    let _ = encoder_control.drain_tier_transitions();
                }
                // Issue #1229 (perf polish): on the FALLING edge of sharing, drain
                // and DISCARD any pending controller tier-transitions so the ended
                // share does not extend its trailing records into the next share's
                // telemetry. NOTE: this alone does NOT keep the buffer empty across
                // the whole idle gap — the user-quality-bounds apply block below
                // runs on every idle tick and a bounds change can re-populate the
                // buffer AFTER this drain. The authoritative guarantee that the next
                // share starts with a clean buffer is the RISING-edge drain above;
                // this falling-edge drain just clears the ended share's tail
                // promptly rather than waiting for the next share to start.
                if share_stopped {
                    let _ = encoder_control.drain_tier_transitions();
                    // Issue #2179 review: the ceiling describes THIS share's
                    // source/device/stream-count facts, so it must not outlive
                    // it. The next share installs its own on its first sharing
                    // tick; clearing here keeps an idle controller unbound.
                    encoder_control.set_source_tier_ceiling(None);
                }
                // Apply user screen-quality bounds if the UI changed them since
                // we last applied. Cheap generation check; the controller snaps
                // the current tier into range and surfaces it via
                // take_tier_changed() below.
                {
                    let shared = quality_bounds.borrow();
                    if shared.generation != applied_bounds_generation {
                        applied_bounds_generation = shared.generation;
                        let b = shared.bounds;
                        drop(shared);
                        encoder_control.set_video_quality_bounds(b.best, b.worst);
                        log::info!(
                            "ScreenEncoder: applied user quality bounds (best={:?}, worst={:?})",
                            b.best,
                            b.worst,
                        );
                    }
                }

                // ── Network-congestion signal consumers (issue #1199) ─────────
                // Mirror the camera AQ loop's three signal consumers so the
                // SCREEN publisher responds to network distress instead of being
                // blind to it. The screen share is frequently the heaviest egress
                // in the call, so reacting here is at least as important as on the
                // camera. These blocks run BEFORE the gradual backpressure/tick
                // below so a forced cut takes effect on this same tick.

                // 1) Server-authored CONGESTION → aggressive congestion cut.
                // The relay is actively dropping our packets; cut hard (multi-tier
                // + shed the top active layer). `swap(false)` consumes our OWN
                // per-encoder flag, so this never races the camera's flag.
                //
                // Issue #1229: ALWAYS consume the flag (the `swap(false)`) so a
                // stale CONGESTION signal set during an idle gap does not leak
                // into the next share, but only ACT on it (`force_congestion_cut`)
                // while actually sharing — acting on an idle controller is
                // pointless (the next re-share re-arms it to base anyway).
                if congestion_flag.swap(false, Ordering::AcqRel) && now_sharing {
                    log::warn!(
                        "ScreenEncoder: server CONGESTION signal received, forcing aggressive congestion cut"
                    );
                    encoder_control.force_congestion_cut();
                }

                // 2) Client-side WebSocket send-buffer backpressure → step down.
                // When the browser's TCP send buffer is full, outbound packets are
                // dropped locally (websocket.rs send_binary) and the global
                // `websocket_drop_count()` increments. A sustained cluster within
                // the window self-triggers an AQ step-down without waiting for the
                // server. For WebTransport users this counter stays flat at 0, so
                // the block is a true no-op. (See the attribution note above: this
                // window is the screen loop's OWN baseline against the shared
                // global counter — the camera loop has its own.)
                {
                    let current_ws_drops = videocall_transport::websocket::websocket_drop_count();
                    let elapsed_ms = now - ws_drop_window_start_ms;
                    if elapsed_ms >= crate::adaptive_quality_constants::WS_SELF_CONGESTION_WINDOW_MS
                    {
                        let delta = current_ws_drops.saturating_sub(last_ws_drop_snapshot);
                        // Issue #1229: roll the window/snapshot ALWAYS (so the
                        // baseline isn't stale across an idle gap), but only act
                        // (`force_video_step_down`) while sharing.
                        if delta
                            >= crate::adaptive_quality_constants::WS_SELF_CONGESTION_DROP_THRESHOLD
                            && now_sharing
                        {
                            log::warn!(
                                "ScreenEncoder: client WS backpressure detected ({} drops in {:.0}ms), \
                                 forcing video step-down",
                                delta,
                                elapsed_ms,
                            );
                            encoder_control.force_video_step_down();
                        }
                        last_ws_drop_snapshot = current_ws_drops;
                        ws_drop_window_start_ms = now;
                    }
                }

                // 3) Client-side WebTransport unistream backpressure → step down
                // (issue #1178 self-trigger). On WebTransport, media frames ride
                // persistent unidirectional QUIC streams; a failed media-frame
                // write increments `unistream_drop_count()` — the WT analogue of
                // the WS send-buffer drop. A sustained cluster self-sheds a layer
                // without waiting for the slower server CONGESTION signal. The
                // window/snapshot are independent of the WS window and the
                // congestion flag, and each axis sheds at most one layer per
                // ITS OWN window. (Note: distinct axes are NOT cross-gated within
                // a single tick — a co-occurring server CONGESTION and a WS/WT
                // drop-burst can each shed a layer in the same tick, because a
                // floor-case `force_congestion_cut` does not stamp the shared
                // min-interval guard. Collapsing toward base under correlated
                // severe distress is acceptable; this matches the camera loop.)
                // For WebSocket users this counter stays flat at 0 (no-op).
                {
                    let current_wt_drops =
                        videocall_transport::webtransport::unistream_drop_count();
                    let elapsed_ms = now - wt_drop_window_start_ms;
                    // Decision + WT-drop constants live in the host-testable
                    // `wt_drop_step_down_decision` helper so a mutation to the
                    // signal/constants is caught by a native test (#509 item #2).
                    let decision = wt_drop_step_down_decision(
                        current_wt_drops,
                        last_wt_drop_snapshot,
                        elapsed_ms,
                    );
                    // Issue #1229: roll the window/snapshot ALWAYS (baseline not
                    // stale across an idle gap), but only act while sharing.
                    if decision.step_down && now_sharing {
                        log::warn!(
                            "ScreenEncoder: client WT uplink backpressure detected ({} unistream \
                             media-frame drops in {:.0}ms), forcing video step-down",
                            current_wt_drops.saturating_sub(last_wt_drop_snapshot),
                            elapsed_ms,
                        );
                        encoder_control.force_video_step_down();
                    }
                    if decision.roll_window {
                        last_wt_drop_snapshot = decision.new_snapshot;
                        wt_drop_window_start_ms = now;
                    }
                }

                // 4) Client-side WebTransport uplink-SATURATION → step down
                // (#1219 prerequisite). The WT DROP block above (3) only fires on
                // stream teardown and is FLAT on a slow-but-alive uplink, because
                // a WritableStream signals backpressure by leaving
                // `writer.ready()` PENDING (the `.await`-blocking media send path
                // never sees a write rejection). The transport exposes
                // `unistream_ready_stall_count()` — incremented once per slow
                // `writer.ready().await` on the established media path — so a
                // SUSTAINED cluster of slow readys self-sheds a layer here. We use
                // the gentle single-rung `force_video_step_down` (NOT
                // `force_congestion_cut`): this is the publisher's own gradual
                // uplink adaptation; the hard cut stays reserved for the
                // server-authored CONGESTION path. Window/snapshot independent of
                // all other axes; one rung per its OWN window. WS users hold the
                // counter flat at 0 → no-op. Screen is frequently the heaviest
                // egress, so detecting its own uplink saturation here is at least
                // as important as on the camera.
                {
                    let current_wt_stalls =
                        videocall_transport::webtransport::unistream_ready_stall_count();
                    let elapsed_ms = now - wt_stall_window_start_ms;
                    // Decision + WT-saturation constants live in the host-testable
                    // `wt_saturation_step_down_decision` helper (#509 item #2).
                    let decision = wt_saturation_step_down_decision(
                        current_wt_stalls,
                        last_wt_stall_snapshot,
                        elapsed_ms,
                    );
                    // Issue #1229: roll the window/snapshot ALWAYS (baseline not
                    // stale across an idle gap), but only act while sharing.
                    if decision.step_down && now_sharing {
                        log::warn!(
                            "ScreenEncoder: client WT uplink saturation detected ({} slow ready() \
                             events in {:.0}ms), forcing video step-down",
                            current_wt_stalls.saturating_sub(last_wt_stall_snapshot),
                            elapsed_ms,
                        );
                        encoder_control.force_video_step_down();
                    }
                    if decision.roll_window {
                        last_wt_stall_snapshot = decision.new_snapshot;
                        wt_stall_window_start_ms = now;
                    }
                }

                // 5) Client-side WS FRESHNESS-GATE backpressure → step down
                // (issue #1921). The #1921 send-side gate DROPS stale screen
                // deltas once the WS `bufferedAmount` backlog exceeds ~half a
                // second of screen bitrate, converging the socket queue to
                // ~156KB — BELOW the 1MB cap that increments
                // `websocket_drop_count()` (axis #2's signal). So under sustained
                // high-motion congestion in the moderate band, axis #2 stays flat
                // and the tier would otherwise stay pinned high while the encoder
                // wastes CPU on deltas the gate discards. A SUSTAINED cluster of
                // gate drops (`screen_ws_stale_delta_drops()`, screen-local, flat
                // at 0 on WebTransport and on healthy WS → no-op) self-sheds a
                // rung so the encoder output converges toward the achievable rate
                // and keyframes shrink. The lower tier tightens the gate's own
                // threshold too — a beneficial coupling that speeds the drain.
                // Window/snapshot independent of all other axes; at most one rung
                // per its OWN (wider, 2s) window; decision + constants live in the
                // host-testable `screen_ws_stale_drop_step_down_decision` helper.
                {
                    let current_stale_drops = screen_ws_stale_delta_drops();
                    let elapsed_ms = now - screen_ws_stale_drop_window_start_ms;
                    let decision = screen_ws_stale_drop_step_down_decision(
                        current_stale_drops,
                        last_screen_ws_stale_drop_snapshot,
                        elapsed_ms,
                    );
                    // Issue #1229: roll the window/snapshot ALWAYS (baseline not
                    // stale across an idle gap), but only act while sharing.
                    if decision.step_down && now_sharing {
                        log::warn!(
                            "ScreenEncoder: sustained WS freshness-gate backpressure detected ({} \
                             stale screen deltas dropped in {:.0}ms), forcing video step-down",
                            current_stale_drops.saturating_sub(last_screen_ws_stale_drop_snapshot),
                            elapsed_ms,
                        );
                        encoder_control.force_video_step_down();
                    }
                    if decision.roll_window {
                        last_screen_ws_stale_drop_snapshot = decision.new_snapshot;
                        screen_ws_stale_drop_window_start_ms = now;
                    }
                }

                // ── Issue #1229: gradual AQ runs ONLY while sharing ───────────
                // The observe → tick → simulcast-write → #903 refresh →
                // tier-change → transitions sequence is the headroom-probe RAMP
                // driver and the active-count writer. While idle (no share) we
                // skip ALL of it so (a) `encoder_control.tick()` cannot advance
                // the ramp against a clear queue and (b) `shared_active_layer_count`
                // is NOT written — the two hard "no drift while idle" requirements
                // of #1229. On the next (re)share the rising-edge block above has
                // already re-armed the controller to the base rung before this
                // tick, so the ramp resumes from base. The drop/stall WINDOW
                // snapshots above are deliberately kept ROLLING every iteration
                // (their counter reads + `*_window_start_ms` updates) so a
                // baseline isn't stale across an idle gap; only the controller
                // ACTIONS (`force_*`) are gated on `now_sharing`.
                //
                // SIDE EFFECT (intentional): the pre-change loop emitted
                // `on_encoder_settings_update("Disabled")` on the first post-stop
                // tick (the `else` branch of the `enabled` check, now inside this
                // block). Moving that block under `now_sharing` means the label no
                // longer flips to "Disabled" on stop. This is inconsequential: the
                // screen encoder's `on_encoder_settings_update` is wired (host.rs)
                // to a handler that flows to a no-op closure (attendants.rs), and
                // the Diagnostics "Encoder Settings" panel renders an
                // `encoder_settings` signal that is never updated — so neither
                // "Bitrate: N kbps" nor "Disabled" is ever shown to the user. The
                // emit is therefore deliberately dropped while idle rather than
                // preserved on the falling edge.
                if now_sharing {
                    // Sender encoder backpressure (issue #1108). Feed the depth the
                    // encode loop published into the screen controller, then advance
                    // the AQ one tick. This is the SOLE gradual quality axis now:
                    // receiver FPS no longer reaches the sender AQ.
                    encoder_control.observe_encoder_queue_depth(
                        shared_encoder_queue_depth.load(Ordering::Relaxed),
                    );
                    // Relay layer-union hint (issue #1108, Stage 3): feed the latest
                    // max-requested-layer the client wrote for SCREEN (u32::MAX =
                    // fail-open / no cap) so the controller caps the published
                    // layer to what some receiver wants. Applied right before `tick`
                    // so it composes with the just-observed backpressure decision.
                    encoder_control.observe_union_requested_layer(
                        shared_union_requested_layer.load(Ordering::Relaxed),
                    );
                    // User SEND layer-ceiling (perf-panel): feed the latest user-
                    // selected layer COUNT for SCREEN (u32::MAX = Auto / no cap →
                    // usize::MAX fail-open). Applied right before `tick` so the cap
                    // composes with the union hint and backpressure as a further
                    // `min`. The base layer is always published (AQ floors at 1).
                    encoder_control.observe_user_layer_ceiling(
                        crate::encode::camera_encoder::layer_ceiling_to_count(
                            shared_user_layer_ceiling.load(Ordering::Relaxed),
                        ),
                    );
                    encoder_control.tick(now);

                    // Screen simulcast (issue #989, Phase 3b): publish the active
                    // layer count + per-layer target bitrates to the encode loop
                    // every tick. Skipped entirely in single-stream mode, so the
                    // legacy behavior is byte-identical.
                    if encoder_control.is_simulcast() {
                        let active = encoder_control.active_layer_count() as u32;
                        let prev_active = shared_active_layer_count.swap(active, Ordering::Relaxed);
                        if prev_active != active {
                            log::info!(
                                "ScreenEncoder: active layers {} -> {}",
                                prev_active,
                                active
                            );
                        }
                        let per_layer = encoder_control.layer_target_bitrates_kbps();
                        let atomics = shared_layer_bitrates_bps.borrow();
                        for (i, atomic) in atomics.iter().enumerate() {
                            if let Some(&kbps) = per_layer.get(i) {
                                atomic.store((kbps * 1000.0) as u32, Ordering::Relaxed);
                            }
                        }
                    }
                    if !enabled.load(Ordering::Acquire) {
                        if let Some(callback) = &on_encoder_settings_update {
                            callback.emit("Disabled".to_string());
                        }
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

                // Issue #1229: the re-election consume runs ALWAYS (even while
                // idle) so a re-election that completes during an idle gap is
                // CONSUMED here and does not leak its signal into the next share.
                // `notify_reelection_completed` on an idle, soon-to-be-re-armed
                // controller is harmless: the rising-edge re-arm on the next share
                // start supersedes any state it touches.
                if reelection_completed_signal.swap(false, Ordering::AcqRel) {
                    log::info!("ScreenEncoder: re-election completed, notifying quality manager");
                    encoder_control.notify_reelection_completed();
                    // Issue #1311: arm the forced-keyframe cooldown reset so the
                    // FIRST post-re-election PLI emits immediately. The encode loop
                    // (a separate spawn_local task) consumes the dedicated atom and
                    // clears `last_keyframe_emit_ms`. We ARM here, piggybacking on the
                    // existing re-election consume, rather than having the encode loop
                    // ALSO `.swap` `reelection_completed_signal`: that atom is swap-
                    // consumed here AND is SHARED with the camera encoder's quality
                    // task (both wired from `client.reelection_completed_signal()`), so
                    // adding a THIRD swap consumer (the encode loop) would race the
                    // existing two and lose the edge unpredictably. Storing into this
                    // separate single-consumer atom avoids that race. The client's
                    // `Connected` callback also arms it (covering RECONNECT, which never
                    // drives this signal) — a duplicate arm is idempotent.
                    keyframe_cooldown_reset_quality.store(true, Ordering::Release);
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

    /// Returns the SCREEN encoder output-FPS atomic (issue #2147).
    ///
    /// Mirrors `CameraEncoder::shared_encoder_output_fps`. Written by the base
    /// layer's (`layer_id == 0`) chunk callback once per second, and decayed to 0
    /// by the AQ control loop after `SCREEN_ENCODER_FPS_IDLE_DECAY_MS` (5000 ms —
    /// deliberately longer than the camera's 2000 ms, because a static share
    /// legitimately produces no frames).
    ///
    /// Cloned into the health reporter, which reads it each packet and emits it as
    /// `screen_encoder_output_fps`. Before #2147 this value was log-only, leaving
    /// the screen encoder — the one implicated in #1899 / #1574 / the #2143 freeze
    /// — with no publisher-side fps signal at all.
    pub fn shared_encoder_output_fps(&self) -> Arc<AtomicU32> {
        self.current_fps.clone()
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

    /// Replace the internal congestion step-down flag with an externally-owned
    /// one (issue #1199).
    ///
    /// Call this after construction to share the flag with `VideoCallClient`,
    /// which sets it when a server CONGESTION signal targeting us is received.
    /// This is the SCREEN analogue of
    /// [`CameraEncoder::set_congestion_step_down_flag`](crate::CameraEncoder::set_congestion_step_down_flag):
    /// the client hands each encoder its OWN flag so both step down on the same
    /// signal without racing over a shared `swap`.
    pub fn set_congestion_step_down_flag(&mut self, flag: Arc<AtomicBool>) {
        self.congestion_step_down = flag;
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
        crate::encode::reset_output_fps(&self.current_fps);

        // Clear screen-sharing flags (Rc + Arc) atomically (issue #1611)
        clear_screen_sharing_flags(&self.screen_sharing_active, &self.screen_sharing_active_arc);

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

    /// Bundle the shared handles [`apply_initial_tier_to`] writes.
    ///
    /// Exists so the tier can ALSO be applied from inside the `start()`
    /// `spawn_local` future — after `getDisplayMedia` resolves and the capture
    /// size is finally known — where `&mut self` is not available.
    fn initial_tier_targets(&self) -> InitialTierTargets {
        InitialTierTargets {
            shared_screen_tier_index: self.shared_screen_tier_index.clone(),
            initial_tier_pending: self.initial_tier_pending.clone(),
            tier_max_width: self.tier_max_width.clone(),
            tier_max_height: self.tier_max_height.clone(),
            tier_keyframe_interval: self.tier_keyframe_interval.clone(),
            shared_active_layer_count: self.shared_active_layer_count.clone(),
            capture_width: self.shared_capture_width.clone(),
            capture_height: self.shared_capture_height.clone(),
            encode_width: self.shared_encode_width.clone(),
            encode_height: self.shared_encode_height.clone(),
        }
    }

    /// Apply the initial quality tier to shared atomics before starting the
    /// encoding loop.  Called by both [`start`](Self::start) and
    /// [`start_with_stream`](Self::start_with_stream).
    fn apply_initial_tier(&mut self) {
        apply_initial_tier_to(&self.initial_tier_targets());
    }
}

/// The shared state [`apply_initial_tier_to`] writes. Every field is an
/// encoder-owned `Rc` handle, so writing through this bundle is identical to
/// writing through `&mut ScreenEncoder`.
struct InitialTierTargets {
    shared_screen_tier_index: Rc<AtomicU32>,
    /// Issue #2179: ARMED by every tier apply, CONSUMED (`swap(false)`) by the AQ
    /// control loop on its next tick while sharing, which then seeds the
    /// controller from `shared_screen_tier_index`. A dedicated flag rather than
    /// the `screen_sharing_active` rising edge because a `stop()`+`start()` pair
    /// contained within one `AQ_TICK_INTERVAL_MS` (1 s) produces NO observable
    /// rising edge — the loop samples `true` at both ends — and the re-share
    /// would then run with a controller still holding the previous session's
    /// tier, whose first transition would overwrite the new source-matched dims.
    initial_tier_pending: Arc<AtomicBool>,
    tier_max_width: Rc<AtomicU32>,
    tier_max_height: Rc<AtomicU32>,
    tier_keyframe_interval: Rc<AtomicU32>,
    shared_active_layer_count: Rc<AtomicU32>,
    /// See `ScreenEncoder::shared_capture_width`; republished on (re)acquisition.
    capture_width: Arc<AtomicU32>,
    /// See `ScreenEncoder::shared_capture_height`.
    capture_height: Arc<AtomicU32>,
    /// See `ScreenEncoder::shared_encode_width`, written at every configure.
    encode_width: Rc<AtomicU32>,
    /// See `ScreenEncoder::shared_encode_height`.
    encode_height: Rc<AtomicU32>,
}

/// Free-function body of [`ScreenEncoder::apply_initial_tier`].
fn apply_initial_tier_to(t: &InitialTierTargets) {
    {
        let tier = &SCREEN_QUALITY_TIERS[DEFAULT_SCREEN_TIER_INDEX];
        t.shared_screen_tier_index
            .store(DEFAULT_SCREEN_TIER_INDEX as u32, Ordering::Relaxed);
        // Arm the AQ control loop to adopt this tier (issue #2179). Stored with
        // Release AFTER the tier index so the loop's Acquire consume cannot
        // observe the flag without the index it is meant to read.
        t.initial_tier_pending.store(true, Ordering::Release);
        t.tier_max_width.store(tier.max_width, Ordering::Relaxed);
        t.tier_max_height.store(tier.max_height, Ordering::Relaxed);
        t.tier_keyframe_interval
            .store(tier.keyframe_interval_frames, Ordering::Relaxed);

        t.shared_active_layer_count.store(1, Ordering::Relaxed);

        log::info!(
            "ScreenEncoder: encode ceiling {}x{} at {}fps (kf={})",
            tier.max_width,
            tier.max_height,
            tier.target_fps,
            tier.keyframe_interval_frames,
        );
    }
}

impl ScreenEncoder {
    /// Start screen sharing with an already-acquired `MediaStream`.
    ///
    /// Safari requires `getDisplayMedia()` to be called synchronously within a
    /// user-gesture (click) handler.  By obtaining the stream in the UI click
    /// handler and passing it here, the browser's gesture requirement is
    /// satisfied regardless of any async boundaries that follow.
    ///
    /// The stream is consumed: this method takes ownership and will stop its
    /// tracks when encoding ends or `stop()` is called.
    ///
    pub fn start_with_stream(&mut self, stream: MediaStream) {
        crate::encode::reset_output_fps(&self.current_fps);
        let tier_targets = self.initial_tier_targets();
        let (src_w, src_h) = screen_stream_source_dims(&stream);
        self.shared_capture_width.store(src_w, Ordering::Relaxed);
        self.shared_capture_height.store(src_h, Ordering::Relaxed);
        self.shared_encode_width.store(0, Ordering::Relaxed);
        self.shared_encode_height.store(0, Ordering::Relaxed);
        self.apply_initial_tier();

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
        let last_layer0_chunk_ms = self.last_layer0_chunk_ms.clone();
        let on_state_change = self.on_state_change.clone();
        let screen_stream = self.screen_stream.clone();
        let tier_max_width = self.tier_max_width.clone();
        let tier_max_height = self.tier_max_height.clone();
        let tier_keyframe_interval = self.tier_keyframe_interval.clone();
        // Issue #1832: hand the encode loop the active screen tier index so its
        // base-encoder config (re)builds can set the framerate rate-control hint.
        let shared_screen_tier_index = self.shared_screen_tier_index.clone();
        let force_keyframe = self.force_keyframe.clone();
        // Issue #1311: hand the encode loop its own clone of the cooldown-reset atom.
        let keyframe_cooldown_reset = self.keyframe_cooldown_reset.clone();
        let active_video_track = self.active_video_track.clone();
        let screen_sharing_active = self.screen_sharing_active.clone();
        let screen_sharing_active_arc = self.screen_sharing_active_arc.clone();
        let screen_at_floor_flag = self.screen_at_floor_flag.clone();
        let shared_target_bitrate = self.shared_screen_encoder_target_bitrate_kbps.clone();
        let n_layers = self.effective_layer_count() as usize;
        let shared_active_layer_count = self.shared_active_layer_count.clone();
        let shared_layer_bitrates_bps = self.shared_layer_bitrates_bps.clone();
        // Sender encoder backpressure (issue #1108, Phase B): forwarded into the
        // shared encode loop, which WRITES the max active-layer queue depth.
        let shared_encoder_queue_depth = self.shared_encoder_queue_depth.clone();

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
                last_layer0_chunk_ms,
                on_state_change,
                screen_stream,
                tier_max_width,
                tier_max_height,
                tier_keyframe_interval,
                shared_screen_tier_index,
                force_keyframe,
                keyframe_cooldown_reset,
                active_video_track,
                screen_sharing_active,
                screen_sharing_active_arc,
                screen_at_floor_flag,
                shared_target_bitrate,
                n_layers,
                shared_active_layer_count,
                shared_layer_bitrates_bps,
                shared_encoder_queue_depth,
                // The UI acquired this stream in its own click handler, so the
                // encoder cannot know whether the #1973 ceiling was requested or
                // dropped. Treat it as unknown (issue #2179 review): the cost of
                // being wrong is at most one extra `applyConstraints` on the
                // first tier change, while the cost of assuming a ceiling that
                // was never applied is a permanent per-frame rescale.
                true,
                tier_targets,
            )
            .await;
        });
    }

    /// Start encoding and sending the data to the client connection (if it's currently connected).
    /// The user is prompted by the browser to select which window or screen to encode.
    ///
    /// This will toggle the enabled state of the encoder.
    ///
    /// NOTE: On Safari, `getDisplayMedia()` must be called synchronously within a
    /// user-gesture handler.  If the call to `start()` is deferred (e.g. via a
    /// timeout or a re-render), Safari will reject the request.  In that case
    /// use [`start_with_stream`](Self::start_with_stream) instead, obtaining the
    /// stream directly in the click handler.
    pub fn start(&mut self) {
        crate::encode::reset_output_fps(&self.current_fps);
        self.shared_capture_width.store(0, Ordering::Relaxed);
        self.shared_capture_height.store(0, Ordering::Relaxed);
        self.shared_encode_width.store(0, Ordering::Relaxed);
        self.shared_encode_height.store(0, Ordering::Relaxed);
        self.apply_initial_tier();
        let tier_targets = self.initial_tier_targets();

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
        let last_layer0_chunk_ms = self.last_layer0_chunk_ms.clone();
        let on_state_change = self.on_state_change.clone();
        let screen_stream = self.screen_stream.clone();
        let tier_max_width = self.tier_max_width.clone();
        let tier_max_height = self.tier_max_height.clone();
        let tier_keyframe_interval = self.tier_keyframe_interval.clone();
        // Issue #1832: hand the encode loop the active screen tier index so its
        // base-encoder config (re)builds can set the framerate rate-control hint.
        let shared_screen_tier_index = self.shared_screen_tier_index.clone();
        let force_keyframe = self.force_keyframe.clone();
        // Issue #1311: hand the encode loop its own clone of the cooldown-reset atom.
        let keyframe_cooldown_reset = self.keyframe_cooldown_reset.clone();
        let active_video_track = self.active_video_track.clone();
        let screen_sharing_active = self.screen_sharing_active.clone();
        let screen_sharing_active_arc = self.screen_sharing_active_arc.clone();
        let screen_at_floor_flag = self.screen_at_floor_flag.clone();
        let shared_target_bitrate = self.shared_screen_encoder_target_bitrate_kbps.clone();
        let n_layers = self.effective_layer_count() as usize;
        let shared_active_layer_count = self.shared_active_layer_count.clone();
        let shared_layer_bitrates_bps = self.shared_layer_bitrates_bps.clone();
        // Sender encoder backpressure (issue #1108, Phase B): forwarded into the
        // shared encode loop, which WRITES the max active-layer queue depth.
        let shared_encoder_queue_depth = self.shared_encoder_queue_depth.clone();

        wasm_bindgen_futures::spawn_local(async move {
            let navigator = window().navigator();
            let media_devices = navigator.media_devices().unwrap_or_else(|_| {
                error!("Failed to get media devices - browser may not support screen sharing");
                panic!("MediaDevices not available");
            });

            // Acquire the capture stream with the issue-1973 resolution ceiling
            // (max 1920x1080) so a native 4K / ultra-wide source is bounded at
            // capture time, before the per-frame main-thread downscale sees it.
            // The helper transparently retries once without the ceiling if a
            // browser rejects it with OverconstrainedError.
            let (screen_to_share, capture_ceiling_dropped): (MediaStream, bool) =
                match acquire_screen_capture_stream(&media_devices).await {
                    Ok(acquired) => acquired,
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
                };

            log::info!("Screen to share: {screen_to_share:?}");

            // Issue 2179: NOW the capture size is known. Re-apply the tier from
            // the source resolution composed with the network floor. This runs
            // BEFORE `run_screen_encoding` flips `screen_sharing_active`, so the
            // AQ control loop's rising-edge seed (which reads
            // `shared_screen_tier_index`) observes the resolved value, not the
            // pre-capture placeholder.
            let (src_w, src_h) = screen_stream_source_dims(&screen_to_share);
            tier_targets.capture_width.store(src_w, Ordering::Relaxed);
            tier_targets.capture_height.store(src_h, Ordering::Relaxed);
            apply_initial_tier_to(&tier_targets);

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
                last_layer0_chunk_ms,
                on_state_change,
                screen_stream,
                tier_max_width,
                tier_max_height,
                tier_keyframe_interval,
                shared_screen_tier_index,
                force_keyframe,
                keyframe_cooldown_reset,
                active_video_track,
                screen_sharing_active,
                screen_sharing_active_arc,
                screen_at_floor_flag,
                shared_target_bitrate,
                n_layers,
                shared_active_layer_count,
                shared_layer_bitrates_bps,
                shared_encoder_queue_depth,
                capture_ceiling_dropped,
                tier_targets,
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
        current_bitrate: Rc<ScreenEffectiveBitrate>,
        current_fps: Arc<AtomicU32>,
        last_layer0_chunk_ms: Arc<AtomicU64>,
        on_state_change: Option<Callback<ScreenShareEvent>>,
        screen_stream: Rc<RefCell<Option<MediaStream>>>,
        tier_max_width: Rc<AtomicU32>,
        tier_max_height: Rc<AtomicU32>,
        tier_keyframe_interval: Rc<AtomicU32>,
        // Issue #1832: the active adaptive SCREEN_QUALITY_TIERS index. Read at
        // each BASE (single-stream / layer-0) `VideoEncoderConfig` (re)build to
        // resolve that tier's `target_fps` for the framerate rate-control hint,
        // so the base encoder budgets bitrate across its real 5–10 fps cadence.
        // Already updated on every tier change (kept in lockstep with the active
        // tier index this loop consumes); higher rungs use their own fps.
        shared_screen_tier_index: Rc<AtomicU32>,
        force_keyframe: Arc<AtomicBool>,
        // Issue #1311: forced-keyframe cooldown reset. The encode loop CONSUMES this
        // each frame (`.swap(false)`) and clears `last_keyframe_emit_ms` when set, so
        // the first PLI after a reconnect/re-election is not coalesced away by a stale
        // pre-transition cooldown timestamp. ARMED by the quality task (re-election)
        // and the client's `Connected` callback (reconnect).
        keyframe_cooldown_reset: Rc<AtomicBool>,
        active_video_track: Rc<RefCell<Option<MediaStreamTrack>>>,
        screen_sharing_active: Rc<AtomicBool>,
        // Issue #1611: Arc mirror of screen_sharing_active for the mic encoder.
        screen_sharing_active_arc: Arc<AtomicBool>,
        screen_at_floor_flag: Arc<AtomicBool>,
        shared_target_bitrate: Rc<AtomicU32>,
        // Screen simulcast (issue #989, Phase 3b). `n_layers == 1` → single
        // encoder, byte-identical to the legacy path. `n_layers > 1` → one
        // VideoEncoder per layer at its fixed SCREEN-ladder resolution, with the
        // AQ controller shedding the top active layer under sender congestion.
        n_layers: usize,
        shared_active_layer_count: Rc<AtomicU32>,
        shared_layer_bitrates_bps: Rc<RefCell<Vec<Rc<AtomicU32>>>>,
        // Sender encoder backpressure (issue #1108, Phase B): the encode loop
        // WRITES the max active-layer `encode_queue_size()` here each frame for
        // the screen AQ control loop. Stored-only on the controller side in
        // Stage 1 (no behavior change).
        shared_encoder_queue_depth: Rc<AtomicU32>,
        // Issue #2179 review: `true` when the capture was acquired WITHOUT the
        // resolution ceiling — either because `getDisplayMedia` rejected the
        // ceiling (the OverconstrainedError fallback) or because the stream was
        // pre-acquired by the UI, whose request this encoder cannot inspect. It
        // seeds `last_track_constraint` to "nothing outstanding" so the first
        // tier change genuinely negotiates instead of being short-circuited by a
        // ceiling that was never actually requested.
        capture_ceiling_dropped: bool,
        tier_targets: InitialTierTargets,
    ) {
        let capture_width = tier_targets.capture_width.clone();
        let capture_height = tier_targets.capture_height.clone();
        let encode_width_out = tier_targets.encode_width.clone();
        let encode_height_out = tier_targets.encode_height.clone();
        let simulcast = n_layers > 1;
        let mut capture_ceiling_dropped = capture_ceiling_dropped;
        // #2147: clones for the two "share is over" cleanup paths below
        // (`cleanup_on_error` and the encode loop's final cleanup), which must zero
        // the output-fps atom via `clear_screen_sharing_state` so the ungated
        // `videocall_screen_encoder_output_fps` gauge cannot keep asserting a live
        // encoder. Bound here because `cleanup_on_error` is a move-closure.
        let current_fps_cleanup = current_fps.clone();
        let current_fps_final = current_fps.clone();
        // Per-layer sequence numbers persist across restarts so a receiver
        // decoding one screen layer sees a dense 0,1,2,… stream (no phantom
        // loss). N=1 is a single-element Vec behaving like the old scalar.
        let mut sequence_numbers: Vec<u64> = vec![0; n_layers];
        // Signal camera encoder ASAP after capture is confirmed so it begins
        // stepping down during encoder setup, not after encoding starts.
        screen_sharing_active.store(true, Ordering::Release);
        // Issue #1611: mirror to the Arc copy the mic encoder reads.
        screen_sharing_active_arc.store(true, Ordering::Release);
        seed_screen_floor_signal(&screen_at_floor_flag);

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
            // Clear screen-sharing flags (Rc + Arc) atomically (issue #1611)
            clear_screen_sharing_state(
                &screen_sharing_active,
                &screen_sharing_active_arc,
                &current_fps_cleanup,
            );
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
        // Maximum restart attempts before surfacing on_error. Sized for the
        // narrow fatal signatures matched by is_fatal_encoder_error_message:
        // the closed-codec InvalidStateError and the VPX allocation failure.
        // Those usually clear within 1-2 retries; 5 gives headroom for a
        // short cascade without spinning forever if the browser is wedged.
        // Revisit this cap if the fatal-error classifier is broadened.
        const MAX_RESTARTS: u32 = 5;

        // Per-rung "continuously shed since" wall-clock (ms, `performance.now()`),
        // indexed by `layer_id` (issue #1230). `Some(t)` once a higher rung drops
        // out of the active set; cleared to `None` when active again or after
        // teardown. Declared OUTSIDE `'restart` (like the camera's
        // `prev_active_layers`; screen has no such persistent var, so add one) so a
        // mid-dwell encoder restart does not reset the clock. The encode loop STAMPS
        // this every frame from the same `local_active_layers` it tears down
        // against, so the dwell clock advances (not a dead timer). Sized `n_layers`;
        // slot 0 (the base `screen_encoder`) is never used — the base is never shed.
        let mut shed_since_ms: Vec<Option<f64>> = vec![None; n_layers];

        let mut media_acquired = true; // true because we already have a stream

        // These variables hold the current media state. They are initialized from
        // the stream passed in, and may be re-acquired on restart.
        let mut current_stream: Option<MediaStream> = Some(screen_to_share);
        let mut current_track: Option<MediaStreamTrack> = None;
        let mut width: u32 = 0;
        let mut height: u32 = 0;

        // Shared atomics carrying the publisher's source track dimensions
        // (resolved from `MediaStreamTrack.getSettings()`, or a safe default
        // pair if the track has not reported them yet). Seeded on acquisition
        // and re-seeded on each `'restart` re-acquire. The output-chunk handler
        // outlives the `'restart:` loop and cannot capture `width` / `height`
        // directly (they get reassigned on restart), so atomics let the
        // per-chunk closure read the most recent source dims at frame-stamping
        // time without locking. `0` means "unknown" and triggers the proto3
        // default-skip, so older publishers / pre-capture frames stay
        // backward-compatible.
        //
        // # The FREEZE invariant (issue #2179 review) — do not "fix" this
        // This pair is written ONLY at acquisition (and re-acquisition), never
        // after an `applyConstraints`. [`screen_track_constraint_for_tier`]
        // decides against how big the surface ORIGINALLY was; refreshing this to
        // the shrunken post-constraint size would make its "neither request
        // binds" guard read true and suppress every later request.
        let source_dims = SourceDims::new_with_live(capture_width, capture_height);
        // LIVE capture dimensions (issue #2179 review): seeded identically at
        // acquisition, then REFRESHED from `getSettings()` after every successful
        // `applyConstraints`. This is the pair stamped onto every outgoing packet
        // — the frozen pair above would tell receivers the share is still 4K
        // after a step-down shrank the capture to 720p, which is simply a lie on
        // the wire. Two pairs rather than one because the two consumers want
        // genuinely different facts: the constraint decision wants the ORIGINAL
        // surface, the wire stamp wants what is being captured RIGHT NOW.
        let live_source_width_atomic = source_dims.live_w.clone();
        let live_source_height_atomic = source_dims.live_h.clone();

        // The onended handler closure must live as long as we use the media track.
        // We store it here so it isn't dropped when the inner loop restarts.
        let mut _onended_handler: Option<Closure<dyn FnMut()>> = None;

        // Retained last-encoded VideoFrame for the static-share keyframe path (issue
        // #1841). Declared OUTSIDE `'restart` so it is in scope for the final cleanup
        // (which closes it on user-stop / unrecoverable exit) AND — post-#1903 — so it
        // SURVIVES an encoder `'restart`: a static share that restarts (fatal encode
        // error / closed codec / stream replace) has no fresh frame to re-seed it, so
        // closing it on restart (as the pre-#1903 code did) left the static-share
        // keyframe path — both the #1841 on-demand PLI re-encode AND the #1903 floor —
        // with nothing to encode and thus permanently disarmed. Set only inside the
        // `'encode` loop, right after a successful encode, and holds EXACTLY ONE open
        // frame at a time (close-prior-on-replace, so it never leaks along the encode
        // path). The `'restart`-internal give-up `return;` paths bypass the final
        // cleanup, so each closes it via `close_retained_frame` before returning.
        //
        // Re-encoding a retained frame whose native dims no longer match a rebuilt
        // encoder (only possible after a DIMENSION-changing restart) is SAFE: Chromium's
        // WebCodecs `VideoEncoder` SCALES the input frame to the configured dims and
        // emits a valid keyframe — the exact behavior the #1841 path already relies on
        // when it encodes a raw 1080p capture frame into downscaled simulcast-tier
        // encoders every frame. So the recovery keyframe still goes out (scaled), and a
        // genuine codec fault would surface asynchronously via the error callback, not a
        // synchronous `encode_with_options` `Err`. Either way it self-heals on the next
        // real frame — strictly better than the pre-#1903 permanent freeze.
        let mut last_encoded_frame: Option<VideoFrame> = None;
        // Static-share keyframe-FLOOR accounting (issue #1903). Declared OUTSIDE
        // `'restart` (alongside `last_encoded_frame`) so the budget and the floor's
        // cadence clock SURVIVE an encoder restart — the live root cause was that the
        // pre-fix budget/clock lived inside `'restart` and were zeroed on every restart,
        // permanently disarming the floor on a static share. Carried across each restart
        // by `ScreenFloorAccount::carry_across_restart` (a no-op by design).
        let mut floor_account = ScreenFloorAccount::idle();
        // One-time INFO latch: `true` once the retained-frame path has served a
        // keyframe request, so the first engagement of the #1841 static path is a
        // single clear signal in field logs rather than a per-emit stream.
        let mut served_synthetic_once = false;
        // `performance.now()` of the last throttled synthetic-re-encode DEBUG line
        // (issue #1841), so the DEBUG path can't spam under sustained joiner churn.
        let mut last_synthetic_log_ms: f64 = 0.0;
        // `performance.now()` of the last #1903 static-tick floor-state DEBUG line, throttled
        // separately from the emit log so a QUIET (non-emitting) tick can still surface the floor's
        // decision inputs once per window for e2e/field diagnosis without spamming every 150ms tick.
        let mut last_floor_debug_ms: f64 = 0.0;
        // discussion 1960 (issue 2): wall-clock (`performance.now()`, ms) the RETAINED frame was
        // captured, so the timer arm can measure its age when answering a PLI (the staleness-honesty
        // warn). Declared OUTSIDE `'restart` alongside `last_encoded_frame` — whose lifetime it
        // describes — so it survives an encoder restart together with the frame it timestamps.
        let mut last_encoded_frame_ms: Option<f64> = None;
        // discussion 1960 (issue 2): rate-limit anchor for the retained-frame staleness warn (~1/5s).
        // `None` until the first warn.
        let mut last_retained_stale_warn_ms: Option<f64> = None;

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
            let last_layer0_chunk_ms = last_layer0_chunk_ms.clone();
            let userid = userid.clone();
            let aes = aes.clone();
            let client = client.clone();
            // Issue #2179 review: the wire stamp must describe the LIVE capture,
            // not the frozen acquisition size (see the atomics' declaration).
            let source_width_for_handler = live_source_width_atomic.clone();
            let source_height_for_handler = live_source_height_atomic.clone();
            let target_bitrate_for_handler = shared_target_bitrate.clone();
            let encode_w_for_handler = encode_width_out.clone();
            let encode_h_for_handler = encode_height_out.clone();
            let tier_idx_for_handler = shared_screen_tier_index.clone();

            Box::new(move |chunk: JsValue| {
                let now = window()
                    .performance()
                    .expect("Performance API not available")
                    .now();
                let chunk = web_sys::EncodedVideoChunk::from(chunk);

                // Update FPS calculation
                last_layer0_chunk_ms.store(now as u64, Ordering::Relaxed);
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

                // Read the latest source dimensions snapshot. The encoder
                // loop seeds the atomics whenever the track is (re)acquired,
                // from `get_settings()` (or a safe default if absent).
                // `Ordering::Relaxed` is sufficient — these values are
                // descriptive metadata, not synchronization signals.
                let source_width_now = source_width_for_handler.load(Ordering::Relaxed);
                let source_height_now = source_height_for_handler.load(Ordering::Relaxed);
                // Issue #903: stamped on the wire for the receiver's Cause line.
                let target_bitrate_now = target_bitrate_for_handler.load(Ordering::Relaxed);
                // Issue #1921: WS send-side freshness gate. Read the frame type
                // BEFORE `chunk` is moved into `transform_screen_chunk`. On a
                // backed-up WebSocket socket a stale screen DELTA is dropped
                // (keyframes are always sent) so keyframes stop queuing
                // head-of-line behind stale deltas on the single shared TCP
                // stream. A `None` depth — WebTransport, or no elected
                // connection — always sends, so it never drops. The
                // sequence number still advances on a drop, so the receiver's
                // jitter buffer sees a gap and freezes to the next keyframe
                // instead of decoding a wrong-reference delta into corruption.
                let is_keyframe = matches!(chunk.type_(), web_sys::EncodedVideoChunkType::Key);
                let ws_threshold = screen_ws_gate_threshold_bytes(screen_baseline_from_published(
                    &encode_w_for_handler,
                    &encode_h_for_handler,
                    &tier_idx_for_handler,
                ));
                let ws_buffered = client.send_queue_depth();
                match screen_ws_send_decision(ws_buffered, is_keyframe, ws_threshold) {
                    ScreenWsSend::Send => {
                        let packet: PacketWrapper = transform_screen_chunk(
                            chunk,
                            sequence_number,
                            buffer.as_mut_slice(),
                            &userid,
                            aes.clone(),
                            source_width_now,
                            source_height_now,
                            target_bitrate_now,
                            0,
                        );
                        // Phase 2 of WT freeze fix: route screen-share video on
                        // its own persistent QUIC stream, isolated from the
                        // camera and audio streams.
                        client.send_media_packet(packet, MediaStreamKey::Screen);
                    }
                    ScreenWsSend::DropStaleDelta => {
                        // `ws_buffered` is `Some` here (a `None` depth decides
                        // Send), so `unwrap_or(0)` reports the real backlog.
                        record_screen_ws_stale_drop(ws_buffered.unwrap_or(0), ws_threshold);
                    }
                }
                // Advance the sequence number whether the frame was sent or
                // dropped: the gap lets the receiver detect the loss (its jitter
                // buffer releases only sequence-contiguous frames) and resume on
                // the next keyframe rather than decode a wrong-reference delta.
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

        let mut uplink_governor = ScreenUplinkGovernor::new();

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
                    // #1903: this give-up return bypasses the encode loop's final cleanup, so release
                    // the retained static-share frame here (it now survives `'restart`, so it may be held).
                    close_retained_frame(&mut last_encoded_frame);
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

                // Re-acquire with the same issue-1973 resolution ceiling as the
                // initial share (max 1920x1080) — a missed ceiling here would
                // reintroduce native-resolution frames on exactly the recovery
                // path. The helper retries once without the ceiling on
                // OverconstrainedError.
                let acquired_stream: MediaStream =
                    match acquire_screen_capture_stream(&media_devices).await {
                        Ok((stream, ceiling_dropped)) => {
                            capture_ceiling_dropped = ceiling_dropped;
                            stream
                        }
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
                            // #1903: release the retained static-share frame before this give-up
                            // return bypasses the encode loop's final cleanup (it survives `'restart`).
                            close_retained_frame(&mut last_encoded_frame);
                            return;
                        }
                    };

                log::info!("Screen to share: {acquired_stream:?}");

                // Signal camera encoder ASAP after capture is confirmed so it begins
                // stepping down during encoder setup, not after encoding starts.
                screen_sharing_active.store(true, Ordering::Release);
                // Issue #1611: mirror to the Arc copy the mic encoder reads.
                screen_sharing_active_arc.store(true, Ordering::Release);

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
                    // Issue #2147: this path must ALSO zero the output-fps atom.
                    // `stop()` / `start()` / `start_with_stream()` all call
                    // `reset_output_fps`, but the BROWSER's own "Stop sharing"
                    // button lands here instead, and this atom is now exported as
                    // `screen_encoder_output_fps` → the ungated
                    // `videocall_screen_encoder_output_fps` gauge. Leaving it alone
                    // relied on the AQ loop's 5s idle decay, which stops running once
                    // the loop's liveness token drops (Host unmount) — so the gauge
                    // could hold a stale NONZERO and assert a live screen encoder
                    // that had stopped. The error/give-up/final-cleanup paths get the
                    // same treatment via `clear_screen_sharing_state`, so every
                    // share-over route zeroes the atom.
                    let current_fps_onended = current_fps.clone();
                    let handler = Closure::wrap(Box::new(move || {
                        log::info!("Screen share track ended (user stopped sharing)");
                        apply_screen_share_stopped(
                            &enabled_clone,
                            &screen_sharing_flag_clone,
                            &current_fps_onended,
                        );
                        client_onended.set_screen_enabled(false);
                        if let Some(ref callback) = on_state_change_clone {
                            callback.emit(ScreenShareEvent::Stopped);
                        }
                    }) as Box<dyn FnMut()>);
                    track.set_onended(Some(handler.as_ref().unchecked_ref()));
                    Some(handler)
                };

                let track_settings = track.get_settings();
                let settings_w = track_settings.get_width().map(f64::from);
                let settings_h = track_settings.get_height().map(f64::from);
                (width, height) = resolve_capture_dimensions(settings_w, settings_h, 0, 0);

                // Publish the source dims to the per-chunk stamper. Read by the
                // screen_output_handler closure on every encoded frame.
                // resolve_capture_dimensions returns a safe non-zero pair (the
                // 640x480 fallback) so the encoder ladder can build, but the
                // #1196 STAMP must not fabricate an aspect: publish 0 = "unknown"
                // (proto3 default-skip) when getSettings() omits a complete pair.
                // Screen seeds-only (no per-frame correction), so an honest 0
                // beats a wrong constant a receiver would read as a real aspect.
                let (stamp_w, stamp_h) = settings_source_stamp(settings_w, settings_h);
                source_dims.seed_on_acquisition(stamp_w, stamp_h);
                // Issue #2179 review r3: a mid-share re-acquire re-prompts the
                tier_targets.capture_width.store(stamp_w, Ordering::Relaxed);
                tier_targets
                    .capture_height
                    .store(stamp_h, Ordering::Relaxed);
                apply_initial_tier_to(&tier_targets);
                log::info!("ScreenEncoder: re-acquired capture {stamp_w}x{stamp_h}");

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
                    // Issue #2147: this path must ALSO zero the output-fps atom.
                    // `stop()` / `start()` / `start_with_stream()` all call
                    // `reset_output_fps`, but the BROWSER's own "Stop sharing"
                    // button lands here instead, and this atom is now exported as
                    // `screen_encoder_output_fps` → the ungated
                    // `videocall_screen_encoder_output_fps` gauge. Leaving it alone
                    // relied on the AQ loop's 5s idle decay, which stops running once
                    // the loop's liveness token drops (Host unmount) — so the gauge
                    // could hold a stale NONZERO and assert a live screen encoder
                    // that had stopped. The error/give-up/final-cleanup paths get the
                    // same treatment via `clear_screen_sharing_state`, so every
                    // share-over route zeroes the atom.
                    let current_fps_onended = current_fps.clone();
                    let handler = Closure::wrap(Box::new(move || {
                        log::info!("Screen share track ended (user stopped sharing)");
                        apply_screen_share_stopped(
                            &enabled_clone,
                            &screen_sharing_flag_clone,
                            &current_fps_onended,
                        );
                        client_onended.set_screen_enabled(false);
                        if let Some(ref callback) = on_state_change_clone {
                            callback.emit(ScreenShareEvent::Stopped);
                        }
                    }) as Box<dyn FnMut()>);
                    track.set_onended(Some(handler.as_ref().unchecked_ref()));
                    Some(handler)
                };

                let track_settings = track.get_settings();
                let settings_w = track_settings.get_width().map(f64::from);
                let settings_h = track_settings.get_height().map(f64::from);
                (width, height) = resolve_capture_dimensions(settings_w, settings_h, 0, 0);

                // Publish the source dims to the per-chunk stamper (see the
                // matching `.store()` in the restart-acquire branch above). Stamp
                // 0 = "unknown" when settings omit a complete pair (rationale there).
                let (stamp_w, stamp_h) = settings_source_stamp(settings_w, settings_h);
                source_dims.seed_on_acquisition(stamp_w, stamp_h);

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
                    // #527: classify by the create error message (memory/other).
                    record_screen_restart(restart_reason_from_message(&msg));
                    restart_count += 1;
                    continue 'restart;
                }
            };

            // --- Initial configure ---
            let mut baseline = publish_screen_encode_geometry(
                &encode_width_out,
                &encode_height_out,
                width,
                height,
                active_screen_tier_fps(shared_screen_tier_index.load(Ordering::Relaxed)),
            );
            let mut local_target: ScreenTargetKbps = uplink_governor.target_for(baseline);
            let mut last_failed_target: Option<ScreenTargetKbps> = None;
            let mut last_logged_governed: Option<ScreenTargetKbps> = None;
            publish_screen_effective_bitrate(
                &current_bitrate,
                &shared_target_bitrate,
                local_target,
            );
            let screen_encoder_config =
                VideoEncoderConfig::new(get_video_codec_string(), height, width);
            screen_encoder_config.set_bitrate(local_target.kbps() as f64 * 1000.0);
            screen_encoder_config.set_latency_mode(LatencyMode::Realtime);
            set_vbr_mode(&screen_encoder_config);
            // Framerate rate-control hint (issue #1832): the base encoder follows
            // the active SCREEN_QUALITY_TIERS tier, so budget its bitrate across
            // that tier's target fps (10/8/5) rather than a fast (~30/60 fps)
            // default. Camera parity — see `set_framerate_hint`.
            set_framerate_hint(
                &screen_encoder_config,
                active_screen_tier_fps(shared_screen_tier_index.load(Ordering::Relaxed)),
            );
            if let Err(e) = screen_encoder.configure(&screen_encoder_config) {
                SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL.fetch_add(1, Ordering::Relaxed);
                let msg = format!("Error configuring screen encoder: {e:?}");
                error!("ScreenEncoder: {msg} (restart {restart_count})");
                record_screen_restart(RestartReason::Configure);
                restart_count += 1;
                continue 'restart;
            }

            // --- Screen simulcast: build the HIGHER layers (issue #989, P3b) ---
            // The existing `screen_encoder` above IS the base layer (layer 0),
            // driven unchanged by the loop below so the N=1 path is byte-
            // identical. For simulcast we additionally build layers 1..n, each
            // its own VideoEncoder at its FIXED SCREEN-ladder resolution + its
            // own per-layer output handler (own seq + layer_id stamp). The
            // encode loop feeds the same captured frame to every active layer
            // and reconfigures each layer's bitrate from the AQ control loop.
            // Layers >= active_layer_count are skipped (shed) — no encode CPU,
            // no egress.
            //
            // LAZY CONSTRUCTION (issue #1204): an upper rung's VideoEncoder is
            // built only on its FIRST ACTIVATION rather than all of layers 1..n
            // at setup. `build_extra_layer` constructs ONE higher rung; at setup we
            // build only the rungs already active (`1..initial_active`), and the
            // encode loop builds the rest when the AQ ramp/restore raises the
            // active count. OUTPUT is unchanged — the encode loop already encodes
            // only `layer_id < active`.
            //
            // TEARDOWN-AFTER-SHED (issue #1230): a shed upper rung is retained (its
            // encoder + ~150KB output buffer) so a brief shed→restore bounce reuses
            // it with no rebuild stall. But on a device under SUSTAINED distress the
            // rung's native VPX/WebCodecs state would leak for the share's lifetime,
            // so once a rung has been continuously shed for `SHED_TEARDOWN_DWELL_MS`
            // (30s) the encode loop pops+closes its `LayerEncoder` from the END of
            // `extra_layers` (top-down shed keeps it a contiguous prefix) to reclaim
            // the memory; this same lazy path rebuilds it (seeded from its persisted
            // sequence) if it is ever earned back. The base screen layer (id 0,
            // the standalone `screen_encoder`) is NEVER torn down. See
            // `should_teardown_shed_layer` + the per-frame dwell tracking below.
            let build_extra_layer = |layer_idx: usize,
                                     initial_seq: u64|
             -> Result<LayerEncoder, ()> {
                let layer_id = layer_idx as u32;
                let screen_tiers = simulcast_screen_layers(n_layers);
                let Some(tier) = screen_tiers.get(layer_idx) else {
                    return Err(());
                };
                // Treat the tier as a BOUNDING BOX, not a fixed output size
                // (issue #1196): fit the actual capture dims inside the
                // layer's rung, aspect-preserving. This is a construction
                // SEED — the first GOP is aspect-correct — and the per-frame
                // encode loop re-fits each rung against this same tier box
                // (`tier_w`/`tier_h` recorded below) when the share's source
                // aspect changes mid-share, exactly like the base screen
                // layer's per-frame reconfigure and the camera's per-layer
                // path. `width` / `height` are the acquisition dims resolved
                // from `getSettings()` above (or a safe default pair if the
                // track had not reported them yet), so a non-16:9 display
                // (16:10, ultrawide, portrait) is not per-axis-squashed into
                // the 16:9 tier dims on rungs 1..n.
                // Issue #2179 review: orient the rung's authored-landscape box to
                // the source so a rotated panel is not bounded by the box's short
                // edge on its long axis (pixel-budget neutral — see
                // `orient_box_to_source`).
                let (layer_w, layer_h) =
                    fit_within_tier_box(width, height, tier.max_width, tier.max_height);
                let init_bitrate_bps = tier.ideal_bitrate_kbps as f64 * 1000.0;

                // Per-layer output handler: own seq cell + #903 metadata
                // (shared, stream-level) + layer_id stamp.
                let (output_box, seq_out) = {
                    let client = client.clone();
                    let userid = userid.clone();
                    let aes = aes.clone();
                    let mut buffer: Vec<u8> = Vec::with_capacity(150_000);
                    let mut local_seq = initial_seq;
                    let seq_out = Rc::new(std::cell::Cell::new(initial_seq));
                    let seq_out_inner = seq_out.clone();
                    // Issue #2179 review: LIVE capture dims for the wire stamp.
                    let source_w = live_source_width_atomic.clone();
                    let source_h = live_source_height_atomic.clone();
                    let target_bitrate = shared_target_bitrate.clone();
                    let encode_w_for_layer = encode_width_out.clone();
                    let encode_h_for_layer = encode_height_out.clone();
                    let tier_idx_for_layer = shared_screen_tier_index.clone();
                    (
                        Box::new(move |chunk: JsValue| {
                            let chunk = web_sys::EncodedVideoChunk::from(chunk);
                            // NOTE: higher layers do NOT update current_fps
                            // (only the base layer does), so the AQ setpoint
                            // is not inflated N×.
                            let byte_length = chunk.byte_length() as usize;
                            if buffer.len() < byte_length {
                                buffer.resize(byte_length, 0);
                            }
                            // Issue #1921: same WS send-side freshness gate as
                            // the base layer. Read frame type before `chunk` is
                            // moved; drop stale deltas on a backed-up WS socket
                            // (keyframes always sent), advancing `local_seq`
                            // either way so the receiver detects the gap.
                            let target_bitrate_now = target_bitrate.load(Ordering::Relaxed);
                            let is_keyframe =
                                matches!(chunk.type_(), web_sys::EncodedVideoChunkType::Key);
                            let ws_threshold =
                                screen_ws_gate_threshold_bytes(screen_baseline_from_published(
                                    &encode_w_for_layer,
                                    &encode_h_for_layer,
                                    &tier_idx_for_layer,
                                ));
                            let ws_buffered = client.send_queue_depth();
                            match screen_ws_send_decision(ws_buffered, is_keyframe, ws_threshold) {
                                ScreenWsSend::Send => {
                                    let packet: PacketWrapper = transform_screen_chunk(
                                        chunk,
                                        local_seq,
                                        buffer.as_mut_slice(),
                                        &userid,
                                        aes.clone(),
                                        source_w.load(Ordering::Relaxed),
                                        source_h.load(Ordering::Relaxed),
                                        target_bitrate_now,
                                        layer_id,
                                    );
                                    client.send_media_packet(packet, MediaStreamKey::Screen);
                                }
                                ScreenWsSend::DropStaleDelta => {
                                    // Shared throttle with the base handler; `ws_buffered`
                                    // is `Some` on the drop path.
                                    record_screen_ws_stale_drop(
                                        ws_buffered.unwrap_or(0),
                                        ws_threshold,
                                    );
                                }
                            }
                            local_seq += 1;
                            seq_out_inner.set(local_seq);
                        }) as Box<dyn FnMut(JsValue)>,
                        seq_out,
                    )
                };
                let error_closure = Closure::wrap(Box::new(move |e: JsValue| {
                    error!("Screen encoder error (layer {layer_id}): {e:?}");
                }) as Box<dyn FnMut(JsValue)>);
                let output_closure = Closure::wrap(output_box);
                let init = VideoEncoderInit::new(
                    error_closure.as_ref().unchecked_ref(),
                    output_closure.as_ref().unchecked_ref(),
                );
                let encoder = match VideoEncoder::new(&init) {
                    Ok(enc) => Box::new(enc),
                    Err(e) => {
                        error!("Failed to create screen encoder (layer {layer_id}): {e:?}");
                        return Err(());
                    }
                };
                let config = VideoEncoderConfig::new(get_video_codec_string(), layer_h, layer_w);
                config.set_bitrate(init_bitrate_bps);
                config.set_latency_mode(LatencyMode::Realtime);
                set_vbr_mode(&config);
                // Framerate rate-control hint (issue #1832): budget this rung's
                // bitrate across its FIXED tier cadence (this rung's `target_fps`,
                // 5–10 fps) instead of a fast default. Camera parity — the camera's
                // per-layer path sets the same hint from `layer_fps`.
                set_framerate_hint(&config, tier.target_fps);
                if let Err(e) = encoder.configure(&config) {
                    SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL.fetch_add(1, Ordering::Relaxed);
                    error!("Error configuring screen encoder (layer {layer_id}): {e:?}");
                    if is_fatal_encoder_error(&e) {
                        let _ = encoder.close();
                        return Err(());
                    }
                }
                Ok(LayerEncoder {
                    encoder,
                    config,
                    seq_out,
                    layer_id,
                    current_w: layer_w,
                    current_h: layer_h,
                    tier_w: tier.max_width,
                    tier_h: tier.max_height,
                    // Issue #1832: retain this rung's fixed tier fps for the
                    // framerate hint on every future per-rung config rebuild.
                    target_fps: tier.target_fps,
                    local_bitrate: init_bitrate_bps as u32,
                    _output_closure: output_closure,
                    _error_closure: error_closure,
                })
            };

            let mut extra_layers: Vec<LayerEncoder> = Vec::new();
            if simulcast {
                // Build only the higher rungs that are ACTIVE right now.
                // `shared_active_layer_count` is the active count INCLUDING the
                // base layer 0, so the active HIGHER rungs are indices
                // `1..initial_active`. Upper rungs are built lazily on first
                // activation in the encode loop below.
                let initial_active =
                    (shared_active_layer_count.load(Ordering::Relaxed) as usize).clamp(1, n_layers);
                // Skip the base (rung 0); enumerate the active higher rungs.
                for (offset, &initial_seq) in sequence_numbers[1..initial_active].iter().enumerate()
                {
                    let layer_idx = 1 + offset;
                    match build_extra_layer(layer_idx, initial_seq) {
                        Ok(le) => extra_layers.push(le),
                        Err(()) => {
                            for built in &extra_layers {
                                let _ = built.encoder.close();
                            }
                            let _ = screen_encoder.close();
                            // #527: build_extra_layer drops the specific error;
                            // the failure is a create-or-fatal-configure at the
                            // build stage, so attribute it to `configure`.
                            record_screen_restart(RestartReason::Configure);
                            restart_count += 1;
                            continue 'restart;
                        }
                    }
                }
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
                    // #1903: release the retained static-share frame before either give-up return below
                    // bypasses the encode loop's final cleanup (it survives `'restart`). On the restart
                    // path a frame from the prior session may be held; on the first attempt it is `None`
                    // and this is a harmless no-op.
                    close_retained_frame(&mut last_encoded_frame);
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
                    // #1903: release the retained static-share frame before this give-up return bypasses
                    // the encode loop's final cleanup (it survives `'restart`).
                    close_retained_frame(&mut last_encoded_frame);
                    cleanup_on_error(stream_ref, &enabled, &on_state_change, msg);
                    return;
                }
            };

            // The single outstanding read on this reader (issue #1841). The encode
            // loop races THIS future against a timer each iteration; on a timer tick
            // (quiet track) the still-pending read is preserved and re-selected, so
            // exactly one read is ever in flight — issuing a second `read()` on the
            // same `ReadableStreamDefaultReader` while one is pending is an error.
            // Re-armed in the loop the moment a frame (or read error) resolves.
            let mut read_fut = Box::pin(JsFuture::from(screen_reader.read()));

            let mut screen_frame_counter: u32 = 0;
            // Wall-clock (`performance.now()`, ms) of the last keyframe this screen
            // publisher emitted — periodic OR PLI-forced. Drives the forced-keyframe
            // emit coalescer (issues #1287/#1312/#1322): PLIs landing within
            // ENCODER_PLI_COOLDOWN_MS of the last keyframe are held pending, not
            // re-emitted. `None` until the first keyframe goes out.
            //
            // Declared INSIDE `'restart`: the per-`'restart` reset to `None` is
            // INTENTIONAL — a `'restart` is fatal-encoder-error recovery (the codec
            // was rebuilt and receivers need a fresh keyframe immediately), so the
            // cooldown clock must start clean. A reconnect/re-election does NOT take
            // this `'restart` path (the encode loop runs uninterrupted), so it gets
            // its own reset via `keyframe_cooldown_reset` in the decision below (issue
            // #1311) — mirroring the camera encoder.
            let mut last_keyframe_emit_ms: Option<f64> = None;
            // Issue #2328: the SECOND, stricter keyframe clock — the last emit that reached EVERY
            // published rung. The periodic-GOP wall-clock backstop below is anchored on THIS, not on
            // `last_keyframe_emit_ms`, so an emit reaching only some rungs cannot defer the one a
            // top-rung receiver depends on. See [`FullFanoutKeyframeClock`] for the defect trace.
            let mut full_fanout_keyframe = FullFanoutKeyframeClock::new();
            // NOTE (issue #1903): the static-share keyframe-FLOOR budget and cadence clock are NOT
            // declared here. They live in `floor_account` OUTSIDE `'restart` so they survive an encoder
            // restart — declaring them here (as the first #1903 cut did, alongside `last_keyframe_emit_ms`)
            // is exactly the live defect: a restart zeroed them and a static capture never restored them,
            // permanently disarming the floor. `last_keyframe_emit_ms` still resets here on purpose (its
            // #1311 cooldown must start clean per restart); the floor deliberately does NOT reuse it.
            let mut current_encoder_width = width;
            let mut current_encoder_height = height;
            // Issue #1922: per-encoder-session settle gate for source-dimension reconfigures. Declared
            // INSIDE `'restart` (like `current_encoder_*`) so a restart / new share session starts it
            // fresh and no stale pending-resize target from a dead track carries into the rebuilt
            // encoder. Unlike `floor_account` (which MUST survive a restart), this is transient.
            let mut dim_settle = DimensionSettle::new();
            // discussion 1960 (issue 2): sender-side tick-starvation monitor. Declared INSIDE `'restart`
            // (like `dim_settle`) so a restart resets the heartbeat — a restart is a deliberate encoder
            // rebuild (whose own recovery keyframe already fires), not a JS-main-thread freeze, so its
            // first post-restart tick must not read as a stall resume. Unlike `floor_account`, this is
            // transient by design.
            let mut stall_monitor = EncoderStallMonitor::new();

            // Cache tier-controlled values
            let mut local_keyframe_interval = tier_keyframe_interval.load(Ordering::Relaxed);
            let local_tier_max_width = tier_max_width.load(Ordering::Relaxed);
            let local_tier_max_height = tier_max_height.load(Ordering::Relaxed);
            // Issue #2179: the resolution ceiling most recently REQUESTED of the
            // capture track, and APPLIED — it is written in the `applyConstraints`
            // success arm, so a rejected call is not remembered as outstanding
            // (which would short-circuit the identical retry). `Rc<Cell<_>>`
            // because that success arm is a separate future.
            //
            // Seeded with the `getDisplayMedia` ceiling ONLY when we know the
            // ceiling was actually requested. When it was dropped — the
            // OverconstrainedError fallback, or a UI-pre-acquired stream whose
            // request this encoder cannot inspect — the seed is `(0, 0)` =
            // "nothing outstanding" (issue #2179 review). Seeding the ceiling
            // unconditionally suppressed the FIRST genuine bounding attempt on,
            // say, a 5K panel: the tier's own box equalled the never-requested
            // seed, the `== last` short-circuit fired, and the capture stayed at
            // 5120x2880 while the encoder ran at 3840x2160 — a per-frame rescale
            // for the life of the share.
            //
            // Declared INSIDE `'restart` (like `current_encoder_*`): a restart
            // re-acquires the stream, so a constraint requested on the dead track
            // must not be remembered.
            let last_track_constraint: Rc<Cell<(u32, u32)>> =
                Rc::new(Cell::new(if capture_ceiling_dropped {
                    (0, 0)
                } else {
                    (
                        SCREEN_CAPTURE_MAX_WIDTH as u32,
                        SCREEN_CAPTURE_MAX_HEIGHT as u32,
                    )
                }));
            // Issue #2179 review: `Some((req_w, req_h, due_ms))` while an applied
            // constraint is awaiting the "did the engine honour it?" check, which
            // the frame arm performs once the source dims have had
            // `SCREEN_DIM_SETTLE_MS` to settle. A share that delivers no frames
            // (the static-capture case) simply never resolves it — harmless, since
            // the cost the check guards against is per-FRAME rescale work that a
            // frameless share is not doing.
            let constraint_pending_verify: Rc<Cell<Option<(u32, u32, f64)>>> =
                Rc::new(Cell::new(None));
            // `true` once an engine has been caught resolving `applyConstraints`
            // without applying it. While set, the base encode box is floored at
            // the source size so further tier step-downs cannot DEEPEN the
            // per-frame WebCodecs rescale (4K -> 720p is 9x — the #1973 spiral).
            let constraint_ignored: Rc<Cell<bool>> = Rc::new(Cell::new(false));

            // With nothing outstanding, make the one bounding attempt the
            // `tier_dims_changed` branch below would otherwise wait for. On a
            // surface that already fits inside the tier box this is suppressed by
            // the decision fn's "nothing binds" guard, so an ordinary share pays
            // nothing.
            if last_track_constraint.get() == (0, 0) {
                let (frozen_w, frozen_h) = source_dims.frozen();
                let (seed_w, seed_h) = orient_box_to_source(
                    local_tier_max_width,
                    local_tier_max_height,
                    frozen_w,
                    frozen_h,
                );
                if let Some((max_w, max_h)) = screen_track_constraint_for_tier(
                    seed_w,
                    seed_h,
                    SCREEN_CAPTURE_MAX_WIDTH as u32,
                    SCREEN_CAPTURE_MAX_HEIGHT as u32,
                    frozen_w,
                    frozen_h,
                    0,
                    0,
                ) {
                    log::info!(
                        "ScreenEncoder: capture acquired without a known ceiling; requesting \
                         {max_w}x{max_h} so the compositor (not the codec queue) does the downscale"
                    );
                    apply_screen_track_resolution_constraint(
                        track_ref,
                        max_w,
                        max_h,
                        last_track_constraint.clone(),
                        source_dims.clone(),
                        constraint_pending_verify.clone(),
                    );
                }
            }

            // Kept off `local_target` so the per-layer AQ atom cannot write the
            // governed actuator local.
            let mut local_layer0_bitrate_bps: u32 = local_target.kbps() * 1000;

            // Track whether the inner loop exited due to a fatal encode error
            // vs. a stream-read error or shutdown signal.
            let mut fatal_encode_exit = false;
            // Set when the capture TRACK ends (reader.read() -> done). Routes the
            // post-loop decision to a clean shutdown instead of an auto-restart:
            // a dead track cannot be re-acquired without a user gesture, and
            // restarting races the shared EncoderState with the user's next share
            // (stop-then-share-again defect). See `post_encode_exit_action`.
            let mut stream_ended = false;

            'encode: loop {
                // discussion 1960 (issue 2): sender-side tick-starvation heartbeat. This runs EVERY
                // `'encode` iteration. The loop's only blocking point is the `select` below, which
                // resolves within SCREEN_STATIC_REENCODE_POLL_MS (timer arm) or sooner (frame arm)
                // while the main thread is alive — so a gap above SCREEN_ENCODER_STALL_GAP_MS between
                // two ticks means the main thread FROZE (the 3840×1600 compositor stall the field saw).
                // On resume we record the episode + max gap and arm a one-shot fresh-keyframe latch that
                // the next REAL captured frame consumes, so receivers recover on FRESH content instead
                // of another re-encode of the minutes-old retained frame. The tick — not the gap since
                // the last real captured frame — is the signal precisely because a static share
                // legitimately delivers no real frames for minutes (see SCREEN_STATIC_REENCODE_POLL_MS)
                // yet keeps ticking at 150ms, so only a true freeze crosses the threshold.
                let tick_now = window()
                    .performance()
                    .expect("Performance API not available")
                    .now();
                if let Some(gap_ms) = stall_monitor.tick(tick_now, SCREEN_ENCODER_STALL_GAP_MS) {
                    SCREEN_ENCODER_STALL_EPISODES.fetch_add(1, Ordering::Relaxed);
                    SCREEN_ENCODER_MAX_STALL_GAP_MS
                        .fetch_max(gap_ms.round() as u64, Ordering::Relaxed);
                    log::warn!(
                        "[SCREEN_ENCODER] stall: encoder tick starved {gap_ms:.0}ms — forcing a fresh capture keyframe on resume (discussion 1960)"
                    );
                }

                // Check if we should stop encoding (user called stop() or
                // onended fired). This exits the function entirely — no restart.
                if !enabled.load(Ordering::Acquire) || switching.load(Ordering::Acquire) {
                    switching.store(false, Ordering::Release);
                    track_ref.stop();
                    if let Err(e) = screen_encoder.close() {
                        error!("Error closing screen encoder: {e:?}");
                    }
                    // Close higher simulcast layers too (no-op when N=1).
                    for layer in &extra_layers {
                        let _ = layer.encoder.close();
                    }
                    // Break to final cleanup — not a restart.
                    break 'restart;
                }

                // --- Guard: skip reconfigure if encoder is already closed ---
                if screen_encoder.state() == CodecState::Closed {
                    log::warn!("ScreenEncoder: encoder found in closed state, triggering restart");
                    record_screen_restart(RestartReason::ClosedCodec);
                    fatal_encode_exit = true;
                    restart_count += 1;
                    break 'encode;
                }

                let new_kf = tier_keyframe_interval.load(Ordering::Relaxed);

                if new_kf != local_keyframe_interval {
                    local_keyframe_interval = new_kf;
                    log::info!(
                        "ScreenEncoder: keyframe interval changed to {}",
                        local_keyframe_interval
                    );
                }

                let screen_key = MediaStreamKey::Screen.as_u8();
                let (active_transport, ws_buffered) = client.uplink_sensors();
                let sample = screen_uplink_sample(
                    active_transport,
                    ws_buffered,
                    screen_ws_stale_delta_drops(),
                    videocall_transport::webtransport::unistream_drop_count_for_stream(screen_key)
                        .saturating_add(
                        videocall_transport::webtransport::unistream_ready_stall_count_for_stream(
                            screen_key,
                        ),
                    ),
                );
                let governed = uplink_governor.observe(js_sys::Date::now(), sample, baseline);
                publish_screen_floor_signal(&screen_at_floor_flag, &uplink_governor, baseline);
                let (log_step, next_logged) =
                    screen_step_log_decision(governed, local_target, last_logged_governed);
                last_logged_governed = next_logged;
                if log_step {
                    info!(
                        "ScreenEncoder: uplink backoff step={} target={}kbps baseline={}kbps",
                        uplink_governor.step().raw(),
                        governed.kbps(),
                        baseline.kbps()
                    );
                }
                if should_attempt_screen_reconfigure(governed, local_target, last_failed_target) {
                    // Guard: check encoder state before bitrate reconfigure
                    if screen_encoder.state() == CodecState::Closed {
                        log::warn!(
                            "ScreenEncoder: encoder closed before bitrate reconfigure, restarting"
                        );
                        record_screen_restart(RestartReason::ClosedCodec);
                        fatal_encode_exit = true;
                        restart_count += 1;
                        break 'encode;
                    }
                    let new_config = VideoEncoderConfig::new(
                        get_video_codec_string(),
                        current_encoder_height,
                        current_encoder_width,
                    );
                    new_config.set_bitrate(governed.kbps() as f64 * 1000.0);
                    new_config.set_latency_mode(LatencyMode::Realtime);
                    set_vbr_mode(&new_config);
                    // Framerate hint (issue #1832): re-apply on every rebuilt base
                    // config so the per-frame bitrate budget tracks the active
                    // tier's fps (a bitrate change alone keeps the tier's fps).
                    set_framerate_hint(
                        &new_config,
                        active_screen_tier_fps(shared_screen_tier_index.load(Ordering::Relaxed)),
                    );
                    if let Err(e) = screen_encoder.configure(&new_config) {
                        SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL.fetch_add(1, Ordering::Relaxed);
                        error!("Error configuring screen encoder: {e:?}");
                        if is_fatal_encoder_error(&e) {
                            record_screen_restart(RestartReason::Configure);
                            fatal_encode_exit = true;
                            restart_count += 1;
                            break 'encode;
                        }
                        // Non-fatal: latch the rejected target. Without this the
                        // gate above stays true and rebuilds+reconfigures+logs
                        // every SCREEN_STATIC_REENCODE_POLL_MS tick (#1221-pt1).
                        // A different governed value still retries.
                        last_failed_target = Some(governed);
                    } else {
                        // Only on a configure the encoder ACCEPTED, so nothing
                        // published can claim a rate the encoder is not at.
                        last_failed_target = None;
                        local_target = governed;
                        publish_screen_effective_bitrate(
                            &current_bitrate,
                            &shared_target_bitrate,
                            governed,
                        );
                    }
                }

                // --- Screen simulcast per-layer bitrate reconfigure (#989 P3b) ---
                // In simulcast mode, drive the BASE layer (layer 0) bitrate from
                // its per-layer atomic (budget-capped by the AQ controller) and
                // reconfigure each ACTIVE higher layer's bitrate from its atomic.
                // Layers >= active are shed (skipped). No-op when N=1.
                let local_active_layers = if simulcast {
                    shared_active_layer_count.load(Ordering::Relaxed) as usize
                } else {
                    1
                };

                // Lazy per-rung construction (issue #1204). If the AQ ramp /
                // restore raised the active count past the higher rungs we have
                // built so far, construct the newly-activated rung(s) NOW, before
                // the bitrate-reconfigure + encode passes below index
                // `extra_layers`. `extra_layers` holds rungs 1..(len+1), so the
                // next index to build is `extra_layers.len() + 1`; the target
                // higher-rung count is `local_active_layers - 1` (minus the base).
                // The clamp keeps indices in-bounds; no-op when N=1 (not
                // simulcast) or when nothing new became active. Each rung is
                // seeded from its PERSISTED sequence so a receiver picking up the
                // freshly-earned rung sees a dense stream.
                if simulcast {
                    let want_extra = local_active_layers.min(n_layers).saturating_sub(1);
                    if extra_layers.len() < want_extra {
                        let mut build_failed = false;
                        // Higher-rung layer_idx == extra-index + 1 (skip base 0).
                        // Enumerate the not-yet-built rung slice to satisfy
                        // needless_range_loop while keeping the absolute index.
                        let next_rung = extra_layers.len() + 1;
                        for (offset, &initial_seq) in sequence_numbers[next_rung..(want_extra + 1)]
                            .iter()
                            .enumerate()
                        {
                            let layer_idx = next_rung + offset;
                            // #1230 rebuild-latency: time the construct+configure
                            // cost so it is field-measurable on real devices/bots.
                            // This delta is the build CALL cost; configure→first-
                            // emitted-keyframe latency can be derived in the field by
                            // correlating this log with the first chunk emitted for
                            // `layer_idx`. This is the "documented rebuild-latency
                            // measurement" that #1204 gated teardown on — now enabled.
                            let build_started_ms = window().performance().unwrap().now();
                            match build_extra_layer(layer_idx, initial_seq) {
                                Ok(le) => {
                                    let build_ms =
                                        window().performance().unwrap().now() - build_started_ms;
                                    info!(
                                        "ScreenEncoder: lazily (re)built simulcast rung {} on activation in {:.1}ms (#1204/#1230 rebuild-latency)",
                                        layer_idx,
                                        build_ms
                                    );
                                    extra_layers.push(le);
                                }
                                Err(()) => {
                                    error!(
                                        "ScreenEncoder: failed to lazily construct simulcast rung {}, restarting",
                                        layer_idx
                                    );
                                    build_failed = true;
                                    break;
                                }
                            }
                        }
                        if build_failed {
                            // #527: build_extra_layer drops the specific error; a
                            // lazy rung build failure is a create-or-fatal-configure
                            // at the build stage → attribute to `configure`.
                            record_screen_restart(RestartReason::Configure);
                            fatal_encode_exit = true;
                            restart_count += 1;
                            break 'encode;
                        }
                    }
                }

                // ── Sustained-shed teardown (issue #1230) ──────────────────────
                // SIMULCAST-ONLY. In single-stream mode (`n_layers == 1`,
                // `simulcast == false`) this whole block is skipped, so the legacy
                // single-encoder path is byte-identical. Operates on `extra_layers`
                // (rungs 1..n); the base screen layer (id 0, the standalone
                // `screen_encoder`) is NEVER torn down. Runs in the SAME loop that
                // reads `local_active_layers` and would rebuild a rung.
                if simulcast {
                    let now_ms = window().performance().unwrap().now();
                    // 1) STAMP per-rung shed-since each frame from the active count
                    // we just read. An extra rung is "shed" iff its id >= active.
                    // Arm on the shed edge; clear when active again. This is what
                    // makes the dwell clock advance (updated every frame here, not
                    // in a side task).
                    for layer in extra_layers.iter() {
                        let id = layer.layer_id as usize;
                        if id >= local_active_layers {
                            if shed_since_ms[id].is_none() {
                                shed_since_ms[id] = Some(now_ms);
                            }
                        } else {
                            shed_since_ms[id] = None;
                        }
                    }

                    // 2) TEAR DOWN the top extra rung(s) whose shed dwell exceeded
                    // the threshold. Pop ONLY from the END so `extra_layers` stays a
                    // contiguous prefix of rungs 1.. (the lazy-build path above
                    // rebuilds `next_rung..` and assumes
                    // `extra_layers[i].layer_id == i + 1`). Screen shed is strictly
                    // top-down, so the shed set is exactly the tail. The base layer
                    // is never in `extra_layers`, so it can never be freed here.
                    // Guard `extra_layers.len() + 1 > local_active_layers` so an
                    // ACTIVE rung is never freed: the top extra rung's id is
                    // `extra_layers.len()` (ids run 1..=len), and it is shed iff
                    // `len >= local_active_layers` — which for integers is exactly
                    // `len + 1 > local_active_layers`. So the guard holds iff that
                    // top rung is shed.
                    while !extra_layers.is_empty()
                        && extra_layers.len() + 1 > local_active_layers
                        && should_teardown_shed_layer(
                            shed_since_ms[extra_layers.len()],
                            now_ms,
                            SHED_TEARDOWN_DWELL_MS,
                        )
                    {
                        // `shed_since_ms[extra_layers.len()]` indexes the top extra
                        // rung's id (id == index + 1; the last extra rung is at
                        // vec index len-1 → id len).
                        if let Some(top) = extra_layers.pop() {
                            let id = top.layer_id as usize;
                            let dwell_s = shed_since_ms[id]
                                .map(|t| (now_ms - t) / 1000.0)
                                .unwrap_or(0.0);
                            // CRITICAL: persist this rung's sequence back into
                            // `sequence_numbers[id]` BEFORE dropping, exactly like
                            // the post-loop writeback
                            // (`sequence_numbers[layer.layer_id] = layer.seq_out.get()`),
                            // so a future lazy rebuild seeds from the continued
                            // sequence and a receiver re-acquiring the rung never
                            // sees a duplicate seq.
                            sequence_numbers[id] = top.seq_out.get();
                            let _ = top.encoder.close();
                            drop(top);
                            shed_since_ms[id] = None;
                            SCREEN_ENCODER_LAYERS_TORN_DOWN_AFTER_DWELL
                                .fetch_add(1, Ordering::Relaxed);
                            info!(
                                "ScreenEncoder: tore down shed simulcast rung {} after {:.1}s sustained shed dwell, reclaiming encoder+buffer (#1230); lazy path rebuilds it if earned back",
                                id,
                                dwell_s
                            );
                        }
                    }
                }

                if simulcast {
                    let atomics = shared_layer_bitrates_bps.borrow();
                    // Base layer (0): apply its per-layer target to screen_encoder.
                    if let Some(a) = atomics.first() {
                        let want = a.load(Ordering::Relaxed);
                        if want > 0
                            && want != local_layer0_bitrate_bps
                            && screen_encoder.state() != CodecState::Closed
                        {
                            let cfg = VideoEncoderConfig::new(
                                get_video_codec_string(),
                                current_encoder_height,
                                current_encoder_width,
                            );
                            cfg.set_bitrate(want as f64);
                            cfg.set_latency_mode(LatencyMode::Realtime);
                            set_vbr_mode(&cfg);
                            // Framerate hint (issue #1832): the base layer (0)
                            // follows the active tier fps; re-apply on this rebuilt
                            // config (simulcast per-layer bitrate path).
                            set_framerate_hint(
                                &cfg,
                                active_screen_tier_fps(
                                    shared_screen_tier_index.load(Ordering::Relaxed),
                                ),
                            );
                            if let Err(e) = screen_encoder.configure(&cfg) {
                                SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL
                                    .fetch_add(1, Ordering::Relaxed);
                                error!("Error reconfiguring base screen layer bitrate: {e:?}");
                                if is_fatal_encoder_error(&e) {
                                    record_screen_restart(RestartReason::Configure);
                                    fatal_encode_exit = true;
                                    restart_count += 1;
                                    break 'encode;
                                }
                            } else {
                                local_layer0_bitrate_bps = want;
                            }
                        }
                    }
                    // Higher layers: per-layer bitrate. Resolution for each rung
                    // is aspect-fitted in the per-frame encode loop (issue #1196),
                    // not here; this pass only adapts the bitrate in place on
                    // `layer.config`, preserving whatever dims that config holds.
                    for layer in extra_layers.iter_mut() {
                        if (layer.layer_id as usize) >= local_active_layers {
                            continue; // shed
                        }
                        let want = atomics
                            .get(layer.layer_id as usize)
                            .map(|a| a.load(Ordering::Relaxed))
                            .unwrap_or(0);
                        if want > 0
                            && want != layer.local_bitrate
                            && layer.encoder.state() != CodecState::Closed
                        {
                            if let Err(e) = layer.reconfigure_at_bitrate(want) {
                                SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL
                                    .fetch_add(1, Ordering::Relaxed);
                                error!(
                                    "Error reconfiguring screen layer {} bitrate: {e:?}",
                                    layer.layer_id
                                );
                            }
                        }
                    }
                }

                // Race the outstanding read against a poll timer (issue #1841) so a
                // STATIC screen share (a real getDisplayMedia track that has stopped
                // emitting frames) does not park this loop and strand late-joiner
                // keyframe requests. `Box::pin` the timer so both arms are `Unpin` as
                // `select` requires; `read_fut` is already a pinned box.
                let timer = Box::pin(gloo_timers::future::TimeoutFuture::new(
                    SCREEN_STATIC_REENCODE_POLL_MS,
                ));
                let read_outcome = match futures::future::select(read_fut, timer).await {
                    futures::future::Either::Left((read_result, _timer)) => {
                        // A real track frame (or a read error) resolved first. Re-arm
                        // the read IMMEDIATELY so exactly one read is ever outstanding
                        // on this reader; the resolved result is processed below.
                        read_fut = Box::pin(JsFuture::from(screen_reader.read()));
                        read_result
                    }
                    futures::future::Either::Right((_elapsed, pending_read)) => {
                        // Timer won: no damage frame arrived within
                        // SCREEN_STATIC_REENCODE_POLL_MS — the static-share case. Keep
                        // the still-pending read alive (moving it back into `read_fut`)
                        // so we never start a second read on the same reader.
                        read_fut = pending_read;

                        // Re-encode the retained frame as a keyframe when EITHER a receiver's
                        // KEYFRAME_REQUEST is pending (on-demand, issue #1841) OR the static-share
                        // wall-clock FLOOR is due (issue #1903).
                        //
                        // The floor is the insurance path the pre-#1903 "on-demand only" gate lacked:
                        // a paused capture never runs the real-frame arm, so the periodic GOP keyframe
                        // (which lives there) never fires, and a receiver whose PLI was LOST — WS
                        // HOL-blocking, relay suppression, packet loss — would hold stale content
                        // forever ("shared content freezes and never refreshes"). The floor emits at
                        // the SAME 3s cadence as the moving-content periodic GOP
                        // (`SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS`), so a share emits keyframes at
                        // one uniform cadence whether moving or paused (never at capture fps), and is
                        // bounded by `floor_account`'s budget so a truly-idle share backs off after a
                        // few cycles instead of spending idle bandwidth room-wide forever. `floor_account`
                        // lives OUTSIDE `'restart` (issue #1903 live fix) so this survives an encoder
                        // restart on a static share.
                        let pli_pending = force_keyframe.load(Ordering::Acquire);
                        let now = window()
                            .performance()
                            .expect("Performance API not available")
                            .now();
                        let floor_due =
                            floor_account.floor_due(now, SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS);
                        // #1903 live-instrumentation: one throttled DEBUG line of the floor's decision
                        // inputs on EVERY static tick — logged BEFORE the emit gate (and independent of
                        // budget / retained-frame presence) so an e2e/field pass can see exactly WHY a
                        // tick did or didn't floor, including the disarmed states (budget 0, clock None,
                        // no retained frame) this fix targets.
                        if now - last_floor_debug_ms >= SCREEN_SYNTHETIC_LOG_THROTTLE_MS {
                            last_floor_debug_ms = now;
                            log::debug!(
                                "ScreenEncoder: static tick — floor_due={floor_due} budget={} clock_ms={:?} retained={} pli_pending={pli_pending}",
                                floor_account.budget,
                                floor_account.clock_ms,
                                last_encoded_frame.is_some(),
                            );
                        }

                        // Issue #1922: apply a SETTLED source-dimension reconfigure on a static share.
                        // During a drag the frame-arrival gate deferred every per-delta reconfigure
                        // (WebCodecs scaled the frames at the stale config). When the drag ends on
                        // content that produces no further frame, NO real-frame arm runs to apply the
                        // final dims — so the timer branch applies them here ONCE: reconfigure the base +
                        // active rungs to the settled fit and re-encode the retained frame as a single
                        // keyframe, so receivers reach the crisp resolution within ~SCREEN_DIM_SETTLE_MS +
                        // one poll. Bounded to one emit — after it applies, `current_encoder_*` equals the
                        // settled fit so the drift check is false until the source moves again.
                        //
                        // This path NEVER restarts the encoder on a `configure()` error (unlike the
                        // frame-arm reconfigure): a restart would re-prompt getDisplayMedia (the #1922
                        // share-drop), and a settled-resize re-key is a quality refinement, not a
                        // correctness necessity. A transient error just aborts this tick's apply and is
                        // retried next tick (dims still settled → still drift); a genuinely closed codec
                        // is caught by the top-of-loop guard on the next `'encode` iteration.
                        let mut settle_emitted = false;
                        if let Some((settled_raw_w, settled_raw_h)) =
                            dim_settle.settled_dims(now, SCREEN_DIM_SETTLE_MS)
                        {
                            // The rungs reconfigured just below already do the equivalent via their
                            // own `tier_w`/`tier_h`.
                            let (settle_box_w, settle_box_h) = screen_base_encode_box_for_source(
                                local_tier_max_width,
                                local_tier_max_height,
                                settled_raw_w,
                                settled_raw_h,
                                SCREEN_CAPTURE_MAX_WIDTH as u32,
                                SCREEN_CAPTURE_MAX_HEIGHT as u32,
                                constraint_ignored.get(),
                            );
                            let (fit_w, fit_h) = fit_within_tier_box(
                                settled_raw_w,
                                settled_raw_h,
                                settle_box_w,
                                settle_box_h,
                            );
                            if fit_w > 0
                                && fit_h > 0
                                && (fit_w != current_encoder_width
                                    || fit_h != current_encoder_height)
                                && screen_encoder.state() != CodecState::Closed
                            {
                                if let Some(retained) = last_encoded_frame.as_ref() {
                                    // Reconfigure the base encoder to the settled fit.
                                    let settled_baseline = ScreenBaselineKbps::for_geometry(
                                        fit_w,
                                        fit_h,
                                        active_screen_tier_fps(
                                            shared_screen_tier_index.load(Ordering::Relaxed),
                                        ),
                                    );
                                    let settled_target =
                                        uplink_governor.target_for(settled_baseline);
                                    let new_config = VideoEncoderConfig::new(
                                        get_video_codec_string(),
                                        fit_h,
                                        fit_w,
                                    );
                                    new_config.set_bitrate(settled_target.kbps() as f64 * 1000.0);
                                    new_config.set_latency_mode(LatencyMode::Realtime);
                                    set_vbr_mode(&new_config);
                                    set_framerate_hint(
                                        &new_config,
                                        active_screen_tier_fps(
                                            shared_screen_tier_index.load(Ordering::Relaxed),
                                        ),
                                    );
                                    if let Err(e) = screen_encoder.configure(&new_config) {
                                        SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL
                                            .fetch_add(1, Ordering::Relaxed);
                                        error!("ScreenEncoder: settled-resize base reconfigure failed (timer, #1922): {e:?}");
                                    } else {
                                        current_encoder_width = fit_w;
                                        current_encoder_height = fit_h;
                                        baseline = publish_screen_encode_geometry(
                                            &encode_width_out,
                                            &encode_height_out,
                                            fit_w,
                                            fit_h,
                                            active_screen_tier_fps(
                                                shared_screen_tier_index.load(Ordering::Relaxed),
                                            ),
                                        );
                                        local_target = settled_target;
                                        publish_screen_effective_bitrate(
                                            &current_bitrate,
                                            &shared_target_bitrate,
                                            local_target,
                                        );
                                        // Reconfigure each ACTIVE rung to its settled fit so the whole
                                        // published set is keyframe-synchronized at the new dims. A
                                        // settled resize is a genuine source-dim change on every rung, so
                                        // (unlike the pure-insurance floor) it fans out to the FULL active
                                        // set — a one-time burst per resize, matching what a real frame at
                                        // the new dims would do.
                                        for layer in extra_layers.iter_mut() {
                                            if (layer.layer_id as usize) >= local_active_layers {
                                                continue;
                                            }
                                            // Issue #2179 review: orient this
                                            // rung's box to the source (see
                                            // `orient_box_to_source`).
                                            let (rung_w, rung_h) = orient_box_to_source(
                                                layer.tier_w,
                                                layer.tier_h,
                                                settled_raw_w,
                                                settled_raw_h,
                                            );
                                            let d = simulcast_layer_target_dims(
                                                settled_raw_w,
                                                settled_raw_h,
                                                rung_w,
                                                rung_h,
                                                layer.current_w,
                                                layer.current_h,
                                            );
                                            if d.needs_reconfigure
                                                && layer.encoder.state() != CodecState::Closed
                                            {
                                                layer.current_w = d.target_w;
                                                layer.current_h = d.target_h;
                                                layer.config = VideoEncoderConfig::new(
                                                    get_video_codec_string(),
                                                    layer.current_h,
                                                    layer.current_w,
                                                );
                                                layer
                                                    .config
                                                    .set_bitrate(layer.local_bitrate as f64);
                                                layer
                                                    .config
                                                    .set_latency_mode(LatencyMode::Realtime);
                                                set_vbr_mode(&layer.config);
                                                set_framerate_hint(&layer.config, layer.target_fps);
                                                if let Err(e) =
                                                    layer.encoder.configure(&layer.config)
                                                {
                                                    SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL
                                                        .fetch_add(1, Ordering::Relaxed);
                                                    error!("ScreenEncoder: settled-resize rung reconfigure failed (timer, layer {}, #1922): {e:?}", layer.layer_id);
                                                }
                                            }
                                        }
                                        // Re-encode the retained frame as ONE keyframe (implicit after
                                        // the base `configure()`; set explicit too) on the base + active
                                        // rungs so every receiver gets the crisp-resolution keyframe.
                                        let opts = VideoEncoderEncodeOptions::new();
                                        opts.set_key_frame(true);
                                        if let Err(e) =
                                            screen_encoder.encode_with_options(retained, &opts)
                                        {
                                            error!("ScreenEncoder: settled-resize re-encode failed (base, #1922): {e:?}");
                                        }
                                        for layer in extra_layers.iter_mut() {
                                            if (layer.layer_id as usize) < local_active_layers {
                                                if let Err(e) = layer
                                                    .encoder
                                                    .encode_with_options(retained, &opts)
                                                {
                                                    error!("ScreenEncoder: settled-resize re-encode failed (layer {}, #1922): {e:?}", layer.layer_id);
                                                }
                                            }
                                        }
                                        settle_emitted = true;
                                        // A room-wide keyframe just went out: stamp the periodic + floor
                                        // cadence clocks (so the next periodic/floor keyframe waits a full
                                        // interval) and satisfy any pending PLI.
                                        last_keyframe_emit_ms = Some(now);
                                        // Issue #2328: the settled-resize re-encode above fans out to
                                        // the FULL active set (base + every rung < local_active_layers,
                                        // see the loop directly above), so it legitimately re-arms the
                                        // periodic-GOP ceiling too.
                                        full_fanout_keyframe.on_keyframe_emitted(
                                            now,
                                            local_active_layers,
                                            local_active_layers,
                                        );
                                        floor_account.on_keyframe_emitted(now);
                                        force_keyframe.store(false, Ordering::Release);
                                        log::info!(
                                            "ScreenEncoder: applied settled resize -> {fit_w}x{fit_h} and re-encoded retained frame as a keyframe (issue #1922)"
                                        );
                                    }
                                }
                            }
                        }

                        // The settled-resize apply above already emitted a room-wide keyframe this tick,
                        // so skip the PLI/floor re-encode (mutually exclusive) — the settle keyframe
                        // satisfies any pending PLI and stamps the floor cadence.
                        if !settle_emitted && (pli_pending || floor_due) {
                            if let Some(retained) = last_encoded_frame.as_ref() {
                                // Consume the #1311 cooldown-reset edge ONLY when actually
                                // servicing a PLI. On a static share the real-frame arm never
                                // runs, so the timer branch is the only consumer that can un-gate
                                // the first post-reconnect PLI — but it must NOT consume the edge
                                // on a quiet no-PLI tick. A pure FLOOR emit does not need the edge
                                // (a periodic/floor keyframe is ungated by the PLI cooldown), so we
                                // leave it un-swapped for a later joiner PLI. Unlike the real arm
                                // (which folds the reset into `last_keyframe_emit_ms` every frame
                                // via the decision), the timer arm has no per-tick store, so
                                // consuming it early would discard the reset. Mutually exclusive
                                // with the real arm per select tick, so not a new racing consumer.
                                let cooldown_reset = if pli_pending {
                                    keyframe_cooldown_reset.swap(false, Ordering::AcqRel)
                                } else {
                                    false
                                };
                                // Reuse the SAME shared keyframe decision the real arm calls, so
                                // the screen PLI coalescer (ENCODER_PLI_COOLDOWN_MS = 2000ms)
                                // collapses a late-joiner WAVE into a single re-encode, and
                                // `is_periodic: floor_due` makes a floor emit ungated by that
                                // cooldown exactly like the moving-content periodic GOP.
                                let decision = keyframe_tick_decision(KeyframeTickInput {
                                    now_ms: now,
                                    pli_pending,
                                    is_periodic: floor_due,
                                    cooldown_reset,
                                    last_keyframe_emit_ms,
                                    cooldown_ms: ENCODER_PLI_COOLDOWN_MS,
                                    // The screen AQ has no tier-change keyframe arm.
                                    tier_change_pending: false,
                                });
                                last_keyframe_emit_ms = decision.last_keyframe_emit_ms;
                                if decision.want_keyframe {
                                    // discussion 1960 (issue 2): PLI-answer staleness honesty. If a
                                    // receiver PLI is being answered by re-encoding the RETAINED frame
                                    // and that frame is older than SCREEN_RETAINED_STALE_MS, the
                                    // "fps > 0 but content minutes stale" freeze is happening —
                                    // receivers get a keyframe of pre-stall content. Warn (rate-limited)
                                    // so field analysis can attribute the symptom directly; do NOT
                                    // refuse the re-encode (a stale frame beats a black frame — the
                                    // receiver keeps the last good content), the fresh capture keyframe
                                    // forced on stall resume is the real fix. Only PLI answers are
                                    // checked; a pure floor emit re-encoding a genuinely-static (and
                                    // therefore CORRECT) frame is not the stall symptom.
                                    if pli_pending {
                                        if let Some(captured_ms) = last_encoded_frame_ms {
                                            let retained_age_ms = now - captured_ms;
                                            if retained_stale_warn_due(
                                                retained_age_ms,
                                                SCREEN_RETAINED_STALE_MS,
                                                now,
                                                last_retained_stale_warn_ms,
                                                SCREEN_RETAINED_STALE_LOG_THROTTLE_MS,
                                            ) {
                                                last_retained_stale_warn_ms = Some(now);
                                                log::warn!(
                                                    "[SCREEN_ENCODER] stall: PLI answered with retained frame age={retained_age_ms:.0}ms (discussion 1960)"
                                                );
                                            }
                                        }
                                    }
                                    let opts = VideoEncoderEncodeOptions::new();
                                    opts.set_key_frame(true);
                                    // Screen publishes one rung, so every emit reaches all of
                                    // it; the #1531/#1903 pressure gate that used to cap a
                                    // multi-rung burst had nothing left to cap.
                                    let fanout_layers = local_active_layers;
                                    // Re-encode the retained frame on the base encoder AND every
                                    // fanned-out higher layer, mirroring the real-frame fan-out so
                                    // the published rungs stay keyframe-synchronized. Encoding a
                                    // retained frame is valid: the real path already encodes ONE
                                    // frame up to N times (base + layers) before its single close,
                                    // so encode() does not consume the frame.
                                    if let Err(e) =
                                        screen_encoder.encode_with_options(retained, &opts)
                                    {
                                        error!("ScreenEncoder: static-share synthetic re-encode failed (base): {e:?}");
                                    }
                                    for layer in extra_layers.iter_mut() {
                                        if (layer.layer_id as usize) < fanout_layers {
                                            if let Err(e) =
                                                layer.encoder.encode_with_options(retained, &opts)
                                            {
                                                error!("ScreenEncoder: static-share synthetic re-encode failed (layer {}): {e:?}", layer.layer_id);
                                            }
                                        }
                                    }
                                    // Issue #2328: re-arm the periodic-GOP ceiling ONLY when this
                                    // emit actually reached every published rung. A PLI answer does
                                    // (`fanout_layers == local_active_layers` above); a
                                    // pressure-gated pure-insurance floor emit does NOT, and stamping
                                    // it here — which is what the shared `last_keyframe_emit_ms` did
                                    // before this fix — is precisely what stranded a top-rung receiver
                                    // in permanent keyframe-less hold.
                                    full_fanout_keyframe.on_keyframe_emitted(
                                        now,
                                        fanout_layers,
                                        local_active_layers,
                                    );
                                    if decision.clear_force_keyframe {
                                        force_keyframe.store(false, Ordering::Release);
                                    }
                                    // Issue #1903: a FLOOR-driven emit consumes one budget unit AND
                                    // stamps the floor cadence clock, so a truly-static share backs
                                    // off after SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET cycles. A PLI-only
                                    // emit (floor not yet due) leaves the budget untouched but still
                                    // stamps the clock — a keyframe just went out room-wide, so the
                                    // floor must wait a full cadence before its next one.
                                    if floor_due {
                                        floor_account.on_floor_emitted(now);
                                    } else {
                                        floor_account.on_keyframe_emitted(now);
                                    }
                                    if !served_synthetic_once {
                                        served_synthetic_once = true;
                                        log::info!(
                                                "ScreenEncoder: static screen share — re-encoded the retained frame as a keyframe (on-demand PLI #1841 / wall-clock floor #1903)"
                                            );
                                    } else if now - last_synthetic_log_ms
                                        >= SCREEN_SYNTHETIC_LOG_THROTTLE_MS
                                    {
                                        last_synthetic_log_ms = now;
                                        log::debug!(
                                                "ScreenEncoder: synthetic keyframe re-encode of retained frame (static share; pli_pending={pli_pending}, floor_due={floor_due})"
                                            );
                                    }
                                }
                            }
                        }
                        // Loop back and re-select the preserved read against a fresh
                        // timer. The top-of-loop stop/reconfigure checks run again, so
                        // stop() stays responsive within one poll interval even while
                        // the track is quiet.
                        continue 'encode;
                    }
                };
                match read_outcome {
                    Ok(js_frame) => {
                        let value = match Reflect::get(&js_frame, &JsString::from("value")) {
                            Ok(v) => v,
                            Err(e) => {
                                error!("Failed to get frame value: {e:?}");
                                continue;
                            }
                        };

                        if value.is_undefined() {
                            // The capture TRACK ended (user stop, browser "Stop
                            // sharing", or OS/source revoke). Flag it so the
                            // post-loop decision shuts down cleanly instead of
                            // entering the auto-restart path — a dead track is
                            // unrecoverable without a user gesture and restarting
                            // races the next share's shared state. See
                            // `post_encode_exit_action`.
                            error!("Screen share stream ended");
                            stream_ended = true;
                            break 'encode;
                        }

                        let video_frame = value.unchecked_into::<VideoFrame>();
                        let raw_frame_width = video_frame.display_width();
                        let raw_frame_height = video_frame.display_height();
                        // Issue #1922: timestamp this frame ONCE (reused by the settle gate below and
                        // the keyframe decision further down) and feed the RAW source dims to the settle
                        // tracker. A window drag-resize changes these dims every frame; the tracker
                        // arms/holds its settle timer so the two source-dimension reconfigure sites can
                        // DEFER until the source stops moving instead of reconfiguring per delta.
                        let now = window()
                            .performance()
                            .expect("Performance API not available")
                            .now();
                        dim_settle.observe(raw_frame_width, raw_frame_height, now);
                        // Issue #2179 review: verify a previously-APPLIED capture
                        // constraint against the dims actually arriving. An engine
                        // that resolves `applyConstraints` without applying it
                        // leaves the encoder rescaling every frame, and each
                        // further tier step-down deepens that ratio. Gated on BOTH
                        // the settle window (so an honoured constraint has had time
                        // to take effect) and the deadline stamped at apply time
                        // (so an IGNORED constraint — whose dims never change, and
                        // which is therefore "settled" from the first frame — is
                        // not judged before the engine had a chance).
                        if let Some((req_w, req_h, due_ms)) = constraint_pending_verify.get() {
                            if now >= due_ms && dim_settle.is_settled(now, SCREEN_DIM_SETTLE_MS) {
                                constraint_pending_verify.set(None);
                                let ignored = screen_constraint_was_ignored(
                                    req_w,
                                    req_h,
                                    raw_frame_width,
                                    raw_frame_height,
                                );
                                constraint_ignored.set(ignored);
                                if ignored {
                                    SCREEN_ENCODER_IGNORED_CONSTRAINTS
                                        .fetch_add(1, Ordering::Relaxed);
                                    log::warn!(
                                        "[SCREEN_ENCODER] constraint-ignored: requested a \
                                         {req_w}x{req_h} capture but frames are still \
                                         {raw_frame_width}x{raw_frame_height}; pinning the encode \
                                         box at the source so further step-downs cannot deepen \
                                         the per-frame rescale (issue 2179 / 1973)"
                                    );
                                }
                            }
                        }
                        // Constrain to tier max dimensions while preserving the
                        // capture's native aspect ratio (issue #1037).
                        // `display_width()` / `display_height()` are the raw
                        // native VideoFrame dims (the true source aspect); a
                        // per-axis `.min()` against the 16:9 tier ceiling would
                        // stretch/squash non-16:9 captures (16:10, ultrawide,
                        // portrait). 0 dims fall through as 0 so the
                        // change-detection below skips reconfigure.
                        let (frame_width, frame_height) =
                            if raw_frame_width > 0 && raw_frame_height > 0 {
                                // Issue #2189: this is the GROUND-TRUTH site — the one that decides
                                // what the base encoder is actually configured to. In simulcast the
                                // base rung is bounded by its own ladder box so its geometry matches
                                // the rung-0 bitrate it is being given; single-stream is unchanged.
                                let (box_w, box_h) = screen_base_encode_box_for_source(
                                    local_tier_max_width,
                                    local_tier_max_height,
                                    raw_frame_width,
                                    raw_frame_height,
                                    SCREEN_CAPTURE_MAX_WIDTH as u32,
                                    SCREEN_CAPTURE_MAX_HEIGHT as u32,
                                    constraint_ignored.get(),
                                );
                                fit_within_tier_box(raw_frame_width, raw_frame_height, box_w, box_h)
                            } else {
                                (0, 0)
                            };

                        // Issue #1922: gate the source-dimension reconfigure on the settle tracker. The
                        // fitted-dim drift check alone fired once PER drag delta (~140 `configure()`
                        // calls in one window drag — each an implicit keyframe bypassing the cooldown).
                        // We reconfigure ONLY once the raw source dims have held steady for
                        // SCREEN_DIM_SETTLE_MS; mid-drag this branch is skipped and the frame is encoded
                        // at the current config (WebCodecs scales it, output stays valid). Skipping
                        // leaves `current_encoder_*` untouched so the drift check still fires the single
                        // time once the source settles.
                        if frame_width > 0
                            && frame_height > 0
                            && (frame_width != current_encoder_width
                                || frame_height != current_encoder_height)
                            && dim_settle.is_settled(now, SCREEN_DIM_SETTLE_MS)
                        {
                            info!("Frame dimensions changed from {current_encoder_width}x{current_encoder_height} to {frame_width}x{frame_height}, reconfiguring encoder (settled #1922)");

                            current_encoder_width = frame_width;
                            current_encoder_height = frame_height;
                            baseline = publish_screen_encode_geometry(
                                &encode_width_out,
                                &encode_height_out,
                                frame_width,
                                frame_height,
                                active_screen_tier_fps(
                                    shared_screen_tier_index.load(Ordering::Relaxed),
                                ),
                            );
                            let resize_target = uplink_governor.target_for(baseline);

                            // Guard: check encoder state before dimension reconfigure
                            if screen_encoder.state() == CodecState::Closed {
                                log::warn!(
                                    "ScreenEncoder: encoder closed before dimension reconfigure, restarting"
                                );
                                video_frame.close();
                                record_screen_restart(RestartReason::ClosedCodec);
                                fatal_encode_exit = true;
                                restart_count += 1;
                                break 'encode;
                            }
                            let new_config = VideoEncoderConfig::new(
                                get_video_codec_string(),
                                current_encoder_height,
                                current_encoder_width,
                            );
                            new_config.set_bitrate(resize_target.kbps() as f64 * 1000.0);
                            new_config.set_latency_mode(LatencyMode::Realtime);
                            set_vbr_mode(&new_config);
                            // Framerate hint (issue #1832): re-apply on the
                            // source-dimension-driven rebuild so the base encoder's
                            // per-frame budget still reflects the active tier fps.
                            set_framerate_hint(
                                &new_config,
                                active_screen_tier_fps(
                                    shared_screen_tier_index.load(Ordering::Relaxed),
                                ),
                            );
                            if let Err(e) = screen_encoder.configure(&new_config) {
                                SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL
                                    .fetch_add(1, Ordering::Relaxed);
                                error!(
                                    "Error reconfiguring screen encoder with new dimensions: {e:?}"
                                );
                                if is_fatal_encoder_error(&e) {
                                    video_frame.close();
                                    record_screen_restart(RestartReason::Configure);
                                    fatal_encode_exit = true;
                                    restart_count += 1;
                                    break 'encode;
                                }
                            } else {
                                // Only on a configure the encoder ACCEPTED; a
                                // non-fatal failure retries next iteration.
                                local_target = resize_target;
                                publish_screen_effective_bitrate(
                                    &current_bitrate,
                                    &shared_target_bitrate,
                                    local_target,
                                );
                            }
                        }

                        let opts = VideoEncoderEncodeOptions::new();
                        // `now` was captured at the top of this frame arm (issue #1922 settle gate) and
                        // is reused here for the keyframe cadence decision.
                        // discussion 1960 (issue 2): consume the one-shot stall-resume latch. When a
                        // tick-starvation resume was just detected (top-of-loop), the FIRST real frame
                        // after the freeze must be a keyframe so receivers recover on FRESH capture
                        // content — not another re-encode of the minutes-old retained frame. We fold it
                        // in as a periodic-keyframe input (ungated by the PLI cooldown, exactly like the
                        // moving-content GOP) so it emits even when no PLI is pending and the cadence is
                        // not otherwise due. One-shot: `take_resume_force` disarms after this frame.
                        let stall_resume_keyframe = stall_monitor.take_resume_force();
                        // Issue #2328: the wall-clock half of this backstop is anchored on the
                        // FULL-FAN-OUT clock, not on `last_keyframe_emit_ms`. This arm is the only
                        // one that unconditionally re-keys every rung `< local_active_layers` (see
                        // the simulcast fan-out loop below, which reuses this same `opts`), so it is
                        // the only guaranteed recovery path for a receiver sitting on a rung the
                        // pressure-gated floor skips. Anchoring on the shared clock let a gated floor
                        // emit push this out of due forever; anchoring here makes the ceiling mean
                        // what its name says — every rung is re-keyed within
                        // SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS of the last time every rung was.
                        // The anchor CHOICE lives inside `screen_periodic_keyframe_due` (which takes
                        // the clock by TYPE, so handing it `last_keyframe_emit_ms` will not compile)
                        // precisely so a host test can pin this decision — see that fn's doc for what
                        // that guard does and does not cover.
                        let is_periodic_keyframe = screen_periodic_keyframe_due(
                            screen_frame_counter,
                            local_keyframe_interval,
                            now,
                            &full_fanout_keyframe,
                            SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS,
                        ) || stall_resume_keyframe;
                        if stall_resume_keyframe {
                            log::info!(
                                "[SCREEN_ENCODER] stall: forcing a fresh capture-path keyframe on the first frame after a tick-starvation resume (discussion 1960)"
                            );
                        }
                        // Resolve the keyframe decision via the shared single source of
                        // truth (issue #1347 item 2: the screen AND camera loops call
                        // the same pure `keyframe_tick_decision`, which the host tests
                        // pin). It folds:
                        //  * #1311 cooldown reset (SCREEN half — camera was #1348) — a
                        //    reconnect or re-election just happened (the
                        //    `keyframe_cooldown_reset` one-shot edge, `.swap(false)`-
                        //    consumed here so a single transition resets exactly once);
                        //    the decision clears the stale cooldown clock so the FIRST
                        //    post-transition PLI emits immediately instead of being
                        //    coalesced away (up to ENCODER_PLI_COOLDOWN_MS = 2000ms of
                        //    suppressed recovery). It only un-gates an ALREADY-pending
                        //    PLI — never forces an unrequested keyframe.
                        //  * #1287/#1312/#1322 PLI coalescer — PEEK the request flag
                        //    (`load`, not `swap`) so a PLI landing mid-window stays
                        //    PENDING (flag cleared only on an actual emit) and is honored
                        //    the instant the window expires rather than dropped. Screen
                        //    uses a longer cooldown than camera (screen content tolerates
                        //    more aggressive coalescing).
                        //  * periodic GOP — never gated by the cooldown.
                        let decision = keyframe_tick_decision(KeyframeTickInput {
                            now_ms: now,
                            pli_pending: force_keyframe.load(Ordering::Acquire),
                            is_periodic: is_periodic_keyframe,
                            cooldown_reset: keyframe_cooldown_reset.swap(false, Ordering::AcqRel),
                            last_keyframe_emit_ms,
                            cooldown_ms: ENCODER_PLI_COOLDOWN_MS,
                            tier_change_pending: false,
                        });
                        let want_keyframe = decision.want_keyframe;
                        last_keyframe_emit_ms = decision.last_keyframe_emit_ms;
                        // Issue #1903: stamp the floor's cadence clock on every real-arm keyframe
                        // (periodic GOP or PLI) so the floor measures its ≥cadence gap from the last
                        // keyframe of ANY kind. When capture then parks, the clock is frozen at this
                        // keyframe and the floor fires exactly one cadence later.
                        if want_keyframe {
                            floor_account.on_keyframe_emitted(now);
                            // Issue #2328: this arm's `opts` is reused verbatim by the simulcast
                            // fan-out loop below for every rung `< local_active_layers`, so a
                            // real-frame keyframe IS a full fan-out and re-arms the ceiling.
                            full_fanout_keyframe.on_keyframe_emitted(
                                now,
                                local_active_layers,
                                local_active_layers,
                            );
                        }
                        if decision.clear_force_keyframe {
                            // ANY keyframe (periodic or forced) is broadcast to the whole
                            // room and satisfies every pending PLI, so clear the request
                            // flag. Clearing only on an actual emit is what lets a
                            // mid-cooldown request survive to be honored at window expiry.
                            force_keyframe.store(false, Ordering::Release);
                        }
                        opts.set_key_frame(want_keyframe);
                        // Log ONLY on emit, matching camera (issue #1347). Under the
                        // peek (`load`) pattern the request flag stays set across the
                        // whole hold window, so an `else if pli_pending` branch here
                        // would fire on EVERY frame of the hold (string-allocating,
                        // unbounded under sustained bursts) rather than once. A held
                        // PLI is observable via the eventual "forcing keyframe" log at
                        // window expiry; a per-window counter (not a per-frame log) is
                        // the right tool if hold visibility is later needed.
                        if let Some(cause) = decision.forced_cause {
                            log::info!(
                                "ScreenEncoder: forcing keyframe at frame {} ({})",
                                screen_frame_counter,
                                cause.label()
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
                                    // #527: reuse the same message classification as
                                    // the error counter just bumped above so the
                                    // restart reason agrees (closed_codec vs memory).
                                    record_screen_restart(restart_reason_from_message(&msg));
                                    fatal_encode_exit = true;
                                    restart_count += 1;
                                    break 'encode;
                                }
                                error!("Error encoding screen frame: {e:?}");
                            }
                        }

                        // --- Screen simulcast: feed the SAME frame to active
                        // higher layers (issue #989, P3b) ---
                        // Reuse the same `opts` so every layer's keyframes are
                        // synchronized. Higher layers downscale the frame to
                        // their fixed tier resolution automatically. Shed layers
                        // (layer_id >= active) are skipped — zero CPU/egress.
                        // A non-fatal per-layer encode error is logged and the
                        // base layer continues; the base layer alone governs the
                        // restart counter (every receiver can decode the base).
                        for layer in extra_layers.iter_mut() {
                            if (layer.layer_id as usize) >= local_active_layers {
                                continue;
                            }

                            // Per-rung aspect re-fit (issue #1196). The base
                            // layer re-fits its dims on every source-aspect change
                            // (above); mirror that for each higher rung so a
                            // mid-share aspect change (window-region resize,
                            // shared-surface switch) does not reintroduce the
                            // per-axis squash on rungs 1..n. Fit the RAW source
                            // frame dims into THIS rung's tier box and reconfigure
                            // only when the fitted dims drift. The fresh config
                            // carries the rung's cached bitrate and is stored back
                            // into `layer.config`, so the dims change never
                            // clobbers the per-layer adaptive bitrate (the
                            // pre-frame bitrate pass mutates this same config in
                            // place next tick).
                            // Issue #2179 review: orient this rung's box to the
                            // source so a portrait share is not bounded by the
                            // authored-landscape short edge on its long axis.
                            let (rung_w, rung_h) = orient_box_to_source(
                                layer.tier_w,
                                layer.tier_h,
                                raw_frame_width,
                                raw_frame_height,
                            );
                            let decision = simulcast_layer_target_dims(
                                raw_frame_width,
                                raw_frame_height,
                                rung_w,
                                rung_h,
                                layer.current_w,
                                layer.current_h,
                            );
                            // Issue #1922: gate the per-rung source-dimension reconfigure on the SAME
                            // settle tracker as the base path, so a drag-resize does not storm the
                            // rungs' `configure()` either (the field's "rung dimension change" bursts,
                            // logged below). Mid-drag the rung encodes the frame at its current config
                            // (WebCodecs scales); the rung's dims are applied once the source settles.
                            if decision.needs_reconfigure
                                && dim_settle.is_settled(now, SCREEN_DIM_SETTLE_MS)
                            {
                                // Guard: do not configure a closed encoder.
                                if layer.encoder.state() == CodecState::Closed {
                                    log::warn!(
                                        "ScreenEncoder: encoder closed before per-rung dimension reconfigure (layer {}), restarting",
                                        layer.layer_id
                                    );
                                    video_frame.close();
                                    record_screen_restart(RestartReason::ClosedCodec);
                                    fatal_encode_exit = true;
                                    restart_count += 1;
                                    break 'encode;
                                }
                                info!(
                                    "ScreenEncoder: rung dimension change -> {}x{} (was {}x{}) within tier {}x{} (layer {})",
                                    decision.target_w,
                                    decision.target_h,
                                    layer.current_w,
                                    layer.current_h,
                                    layer.tier_w,
                                    layer.tier_h,
                                    layer.layer_id,
                                );
                                layer.current_w = decision.target_w;
                                layer.current_h = decision.target_h;
                                layer.config = VideoEncoderConfig::new(
                                    get_video_codec_string(),
                                    layer.current_h,
                                    layer.current_w,
                                );
                                layer.config.set_bitrate(layer.local_bitrate as f64);
                                layer.config.set_latency_mode(LatencyMode::Realtime);
                                set_vbr_mode(&layer.config);
                                // Framerate hint (issue #1832): this rung's fixed
                                // tier cadence, re-applied on the rebuilt config so
                                // a mid-share per-rung re-fit does not drop it.
                                set_framerate_hint(&layer.config, layer.target_fps);
                                if let Err(e) = layer.encoder.configure(&layer.config) {
                                    SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL
                                        .fetch_add(1, Ordering::Relaxed);
                                    if is_fatal_encoder_error(&e) {
                                        error!(
                                            "ScreenEncoder: fatal configure error on rung dimension reconfigure (layer {}), restarting: {e:?}",
                                            layer.layer_id
                                        );
                                        video_frame.close();
                                        record_screen_restart(RestartReason::Configure);
                                        fatal_encode_exit = true;
                                        restart_count += 1;
                                        break 'encode;
                                    }
                                    error!(
                                        "Error reconfiguring screen rung for dimension change (layer {}): {e:?}",
                                        layer.layer_id
                                    );
                                }
                            }

                            match layer.encoder.encode_with_options(&video_frame, &opts) {
                                Ok(_) => {
                                    SCREEN_ENCODER_FRAMES_SUBMITTED_OK
                                        .fetch_add(1, Ordering::Relaxed);
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
                                    error!(
                                        "Error encoding screen frame (layer {}): {e:?}",
                                        layer.layer_id
                                    );
                                }
                            }
                        }

                        // Retain the just-encoded frame instead of closing it, so the
                        // static-share timer branch can re-encode it as a keyframe for
                        // a late joiner with no content change (issue #1841). Close the
                        // PREVIOUS retained frame first: exactly one VideoFrame is ever
                        // held open. The frame is not used past this point in the
                        // iteration (the queue-depth sample below reads encoder state,
                        // not the frame).
                        if let Some(prev) = last_encoded_frame.replace(video_frame) {
                            prev.close();
                        }
                        // discussion 1960 (issue 2): stamp when this retained frame was captured, so a
                        // later timer-arm PLI answer can measure its age for the staleness-honesty warn.
                        // `now` is this frame's capture wall-clock (issue #1922 settle timestamp).
                        last_encoded_frame_ms = Some(now);

                        // Sender encoder backpressure (issue #1108, Phase B).
                        // After submitting this frame to the base encoder and
                        // every ACTIVE higher layer, sample the max
                        // `encode_queue_size()` across them and publish it for the
                        // screen AQ control loop. The base `screen_encoder` is
                        // always layer 0 (active); higher layers mirror the encode
                        // gate above (skip `>= local_active_layers`) so a shed
                        // layer's stale queue can't keep the signal hot. For N==1
                        // `extra_layers` is empty, so this is just the base
                        // encoder's depth. Stage 1: stored-only on the controller
                        // side, so this is observability with no behavior change.
                        let max_active_queue_depth = extra_layers
                            .iter()
                            .filter(|l| (l.layer_id as usize) < local_active_layers)
                            .map(|l| l.encoder.encode_queue_size())
                            .max()
                            .unwrap_or(0)
                            .max(screen_encoder.encode_queue_size());
                        shared_encoder_queue_depth.store(max_active_queue_depth, Ordering::Relaxed);

                        screen_frame_counter += 1;
                        // Issue #1903: a real captured frame just published, so re-arm the
                        // static-share keyframe-FLOOR budget. This is what makes every content
                        // CHANGE earn a fresh post-quiet recovery window (so a change whose delta was
                        // lost to some receiver is re-broadcast as a floor keyframe once capture goes
                        // quiet), while a share that stays truly static drains the budget and stops.
                        floor_account.on_captured_frame();
                    }
                    Err(e) => {
                        error!("Error reading screen frame: {e:?}");
                        break 'encode;
                    }
                }
            } // end 'encode

            // --- Post-inner-loop: decide restart vs full exit ---
            // Persist each higher layer's sequence so the next restart cycle
            // continues numbering where we left off (dense per-layer stream).
            for layer in &extra_layers {
                sequence_numbers[layer.layer_id as usize] = layer.seq_out.get();
            }
            // Close the dead encoder(s) before restarting (best-effort; they may
            // already be closed).
            let _ = screen_encoder.close();
            for layer in &extra_layers {
                let _ = layer.encoder.close();
            }
            // Issue #1903 (live root-cause fix): do NOT close `last_encoded_frame` here. The pre-#1903
            // code took/closed it on every restart so a next real frame would re-seed it — but on a
            // share that has gone STATIC the restart is not followed by any new frame, so closing it
            // left the static-share keyframe path (both the #1841 on-demand PLI re-encode and the #1903
            // floor) with nothing to encode and permanently frozen. Carrying it across the restart lets
            // recovery re-encode it as a keyframe on the rebuilt encoder. After a dimension-changing
            // restart the retained frame's native dims can differ from the rebuilt encoder's config;
            // that is SAFE — WebCodecs scales the frame to the configured dims and emits a valid keyframe
            // (the same scaling the #1841 downscaled-tier path relies on every frame). The frame does
            // not leak: `.replace()` closes the prior frame on the next real frame, and every give-up
            // `return;` inside `'restart` now closes it via `close_retained_frame` (those paths bypass
            // the final cleanup); it is otherwise closed on the final (user-stop / unrecoverable) exit
            // path below.
            //
            // Carry the static-share FLOOR accounting across the restart too (issue #1903). This is a
            // no-op by design — `floor_account` lives OUTSIDE `'restart` so its budget and cadence clock
            // already persist — but making the transition explicit pins the survival contract (a
            // `carry_across_restart` that reset the account would reproduce the live disarm bug).
            floor_account = floor_account.carry_across_restart();
            // Drop the higher layers (and their closures) before the next
            // 'restart iteration rebuilds them.
            drop(extra_layers);

            // A track end short-circuits straight to clean shutdown: it can never
            // be auto-recovered without a user gesture, and re-entering the
            // restart loop would race the shared EncoderState (enabled flag +
            // stream/track cells) with the user's NEXT share, clobbering it (the
            // stop-then-share-again defect). Every OTHER exit (fatal codec fault
            // or transient read error, track still alive) keeps the genuine
            // auto-recovery path intact.
            if post_encode_exit_action(stream_ended) == PostEncodeExit::Shutdown {
                break 'restart;
            }

            if fatal_encode_exit {
                // Fatal encode error: the encoder died but the stream may be
                // alive.  Continue to the next restart iteration.
                continue 'restart;
            }

            log::warn!("ScreenEncoder: restarting with a fresh screen capture stream");
            // #527: this fallthrough is the non-fatal-encode restart path — the
            // 'encode loop exited via a stream-level break (e.g. read error /
            // "stream ended") rather than a codec/memory/configure fault, so the
            // reason is `other`. In-loop codec/configure restart sites set
            // `fatal_encode_exit` after recording their specific reason, so one
            // restart cycle is never split across a specific label plus `other`.
            record_screen_restart(RestartReason::Other);
            restart_count += 1;
            continue 'restart;
        } // end 'restart

        // --- Final cleanup (reached on shutdown or unrecoverable failure) ---
        // Close any retained static-share frame (issue #1841) so a held VideoFrame
        // never outlives the encoder — a leaked frame stalls the capture pipeline.
        // Reached via `break 'restart` (user stop), which skips the per-restart
        // cleanup above.
        if let Some(frame) = last_encoded_frame.take() {
            frame.close();
        }

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

        // Clear screen-sharing flags (Rc + Arc) atomically (issue #1611)
        clear_screen_sharing_state(
            &screen_sharing_active,
            &screen_sharing_active_arc,
            &current_fps_final,
        );

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
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::active_screen_tier_fps;
    use super::clamp_screen_layer_count;
    use super::clear_screen_sharing_flags;
    use super::floor_budget_after_emit;
    use super::floor_budget_replenished;
    use super::is_fatal_encoder_error_message;
    use super::keyframe_tick_decision;
    use super::periodic_keyframe_due;
    use super::post_encode_exit_action;
    use super::publish_screen_effective_bitrate;
    use super::publish_screen_encode_geometry;
    use super::publish_screen_floor_signal;
    use super::record_screen_restart;
    use super::retained_stale_warn_due;
    use super::screen_baseline_from_published;
    use super::screen_capture_constraint_spec;
    use super::screen_constraint_was_ignored;
    use super::screen_encode_box_when_constraint_ignored;
    use super::screen_encoder_restarts_closed_codec;
    use super::screen_encoder_restarts_configure;
    use super::screen_encoder_restarts_memory;
    use super::screen_encoder_restarts_other;
    use super::screen_periodic_keyframe_due;
    use super::screen_step_log_decision;
    use super::screen_track_constraint_for_tier;
    use super::screen_uplink_sample;
    use super::screen_ws_freshness_threshold_bytes;
    use super::screen_ws_gate_threshold_bytes;
    use super::screen_ws_send_decision;
    use super::screen_ws_stale_drop_step_down_decision;
    use super::seed_screen_floor_signal;
    use super::should_attempt_screen_reconfigure;
    use super::should_reacquire_screen_capture;
    use super::should_retry_screen_capture_without_ceiling;
    use super::should_teardown_shed_layer;
    use super::static_keyframe_floor_due;
    use super::wt_drop_step_down_decision;
    use super::wt_saturation_step_down_decision;
    use super::DimensionSettle;
    use super::EncoderStallMonitor;
    use super::FullFanoutKeyframeClock;
    use super::KeyframeTickInput;
    use super::PostEncodeExit;
    use super::RestartReason;
    use super::ScreenEffectiveBitrate;
    use super::ScreenEncoder;
    use super::ScreenFloorAccount;
    use super::ScreenWsSend;
    use super::SourceDims;
    use super::SCREEN_DIM_SETTLE_MS;
    use super::SCREEN_ENCODER_STALL_GAP_MS;
    use super::SCREEN_RETAINED_STALE_LOG_THROTTLE_MS;
    use super::SCREEN_RETAINED_STALE_MS;
    use super::SCREEN_SIMULCAST_MAX_SUPPORTED_LAYERS;
    use super::SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET;
    use super::SCREEN_STATIC_REENCODE_POLL_MS;
    use super::SCREEN_WS_FRESHNESS_DELAY_MS;
    use super::SCREEN_WS_MIN_THRESHOLD_BYTES;
    use super::SHED_TEARDOWN_DWELL_MS;
    use crate::adaptive_quality_constants::ENCODER_PLI_COOLDOWN_MS;
    use crate::adaptive_quality_constants::SCREEN_MIN_BITRATE_KBPS;
    use crate::adaptive_quality_constants::SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS;
    use crate::adaptive_quality_constants::SCREEN_QUALITY_TIERS;
    use crate::adaptive_quality_constants::{
        SCREEN_WS_STALE_DROP_THRESHOLD, SCREEN_WS_STALE_DROP_WINDOW_MS,
        WS_SELF_CONGESTION_WINDOW_MS, WT_SATURATION_STALL_THRESHOLD, WT_SATURATION_WINDOW_MS,
        WT_SELF_CONGESTION_DROP_THRESHOLD, WT_SELF_CONGESTION_WINDOW_MS,
    };
    use crate::encode::encoder_state::ForcedKeyframeCause;
    use crate::{Callback, ScreenShareEvent, VideoCallClient, VideoCallClientOptions};
    use std::sync::atomic::AtomicU32;
    use videocall_aq::screen_bitrate::{
        screen_effective_bitrate_kbps, ScreenPressureStep, ScreenUplinkGovernor,
        ScreenUplinkSample, SCREEN_BACKOFF_MAX_STEP, SCREEN_BACKOFF_PROBE_INTERVAL_MS,
        SCREEN_UPLINK_PRESSURE_MS,
    };
    use videocall_aq::{queued_ms_for, ScreenBaselineKbps};

    // ── Issue 1973: getDisplayMedia capture resolution ceiling ───────────────
    // These pin the pure builder + retry policy that the wasm acquire path
    // (`acquire_screen_capture_stream` via `build_screen_display_constraints`)
    // writes onto the JS constraint object and consults on rejection — not a
    // re-implemented copy — so mutating the ceiling or the retry rule fails here.

    /// The normal request carries a `max` ceiling of exactly 3840x2160 on BOTH
    /// width and height (matching the top screen tier, raised by issue #2179 —
    /// the pre-#2179 1920x1080 ceiling is what forced a DPR-2 Retina window
    /// through a resample before the encoder ever saw it), plus the `ideal`
    /// hint. Dropping either `max` push in `screen_capture_constraint_spec`
    /// fails this, and so does reverting the top tier to 1080p.
    #[test]
    fn screen_capture_spec_requests_the_encode_ceiling() {
        let spec = screen_capture_constraint_spec(true);
        let width_max = spec
            .width
            .iter()
            .find(|&&(k, _)| k == "max")
            .map(|&(_, v)| v as u32);
        let height_max = spec
            .height
            .iter()
            .find(|&&(k, _)| k == "max")
            .map(|&(_, v)| v as u32);
        let (want_w, want_h) = (
            SCREEN_QUALITY_TIERS[0].max_width,
            SCREEN_QUALITY_TIERS[0].max_height,
        );
        assert_eq!(width_max, Some(want_w), "width must request the ceiling");
        assert_eq!(height_max, Some(want_h), "height must request the ceiling");
        // `ideal` matches the ceiling so a source at/under it captures NATIVE
        // rather than being biased down to a fixed size.
        assert!(spec
            .width
            .iter()
            .any(|&(k, v)| k == "ideal" && v as u32 == want_w));
        assert!(spec
            .height
            .iter()
            .any(|&(k, v)| k == "ideal" && v as u32 == want_h));
        assert!(spec
            .framerate
            .iter()
            .any(|&(k, v)| k == "ideal" && v as u32 == 10));
    }

    /// The OverconstrainedError fallback spec drops the ceiling (no `max`) while
    /// keeping the `ideal` hint — the exact pre-issue-1973 request. Mutating the
    /// `include_ceiling` guard so `false` still pushes `max` fails this.
    #[test]
    fn screen_capture_fallback_spec_drops_ceiling() {
        let spec = screen_capture_constraint_spec(false);
        assert!(
            !spec.width.iter().any(|&(k, _)| k == "max"),
            "fallback width must not carry a max ceiling"
        );
        assert!(
            !spec.height.iter().any(|&(k, _)| k == "max"),
            "fallback height must not carry a max ceiling"
        );
        assert!(spec
            .width
            .iter()
            .any(|&(k, v)| k == "ideal" && v as u32 == SCREEN_QUALITY_TIERS[0].max_width));
        assert!(spec
            .height
            .iter()
            .any(|&(k, v)| k == "ideal" && v as u32 == SCREEN_QUALITY_TIERS[0].max_height));
    }

    /// Issue #2179, guarding the issue #1973 regression: once the tier steps
    /// DOWN the encode loop must re-request the CAPTURE size so the compositor
    /// (not the WebCodecs queue) does the downscale. These cases pin the policy
    /// on the production decision function.
    ///
    /// MUTATION, per assertion: drop the `min(box, ceiling)` cap and a request
    /// exceeds what `getDisplayMedia` allowed; drop the `== last` short-circuit
    /// and an unchanged box re-negotiates the track; drop the release branch and
    /// a bound box is never let go; drop the "neither request binds" guard and a
    /// small-window share fires a pointless `applyConstraints`.
    #[test]
    fn screen_track_constraint_for_tier_tracks_the_tier_box() {
        let (cw, ch) = (3840u32, 2160u32);

        // Step DOWN on a 4K source: request the tier's box so the source shrinks
        // with the config instead of being software-rescaled per frame.
        assert_eq!(
            screen_track_constraint_for_tier(1280, 720, cw, ch, 3840, 2160, cw, ch),
            Some((1280, 720))
        );
        // Step back UP to the top rung: the tier box IS the ceiling, so this
        // releases the earlier constraint and the surface can go native again.
        assert_eq!(
            screen_track_constraint_for_tier(cw, ch, cw, ch, 3840, 2160, 1280, 720),
            Some((cw, ch))
        );
        // Partial widening after a 720p constraint.
        assert_eq!(
            screen_track_constraint_for_tier(2560, 1440, cw, ch, 3840, 2160, 1280, 720),
            Some((2560, 1440))
        );

        // Same box as last time (medium -> low, both 1280x720): no re-negotiation.
        assert_eq!(
            screen_track_constraint_for_tier(1280, 720, cw, ch, 3840, 2160, 1280, 720),
            None
        );

        // A tier box larger than the ceiling is capped at the ceiling — the
        // request can never widen capture past what getDisplayMedia allowed.
        assert_eq!(
            screen_track_constraint_for_tier(7680, 4320, cw, ch, 3840, 2160, 1280, 720),
            Some((cw, ch))
        );

        // Small source: neither the outgoing (ceiling) nor the incoming (1080p)
        // request binds a 1280x720 capture, so there is nothing to do.
        assert_eq!(
            screen_track_constraint_for_tier(1920, 1080, cw, ch, 1280, 720, cw, ch),
            None
        );
        // …but once a request WOULD bind that small source, it is made.
        assert_eq!(
            screen_track_constraint_for_tier(1280, 720, cw, ch, 1600, 900, cw, ch),
            Some((1280, 720))
        );

        // Unknown source dims: the guard is skipped and the request is made; a
        // `max` can only shrink a genuinely larger source.
        assert_eq!(
            screen_track_constraint_for_tier(1280, 720, cw, ch, 0, 0, cw, ch),
            Some((1280, 720))
        );

        // Degenerate tier box never produces a zero-dimension request.
        assert_eq!(
            screen_track_constraint_for_tier(0, 720, cw, ch, 3840, 2160, cw, ch),
            None
        );

        // ── The FREEZE invariant, made executable (issue #2179 review) ───────
        // The step-up release case above passes source=3840x2160 together with
        // last=1280x720. That combination is ONLY reachable because the source
        // pair is frozen at ACQUISITION and never refreshed after a constraint
        // shrinks the capture. Feed the same call the dims a REFRESHED source
        // pair would report and the release returns None — i.e. a share stays
        // soft forever after one transient congestion episode. This is why
        // `run_screen_encoding` keeps a SEPARATE live pair for the wire stamp
        // instead of refreshing this one.
        assert_eq!(
            screen_track_constraint_for_tier(cw, ch, cw, ch, 1280, 720, 1280, 720),
            None,
            "a REFRESHED source pair would make the step-up release a no-op — the \
             acquisition freeze is load-bearing, not incidental"
        );
    }

    /// Issue #2179 review: `(0, 0)` last-constraint means "nothing requested yet"
    /// (ceiling dropped by the Overconstrained fallback, or a UI-pre-acquired
    /// stream), so only the INCOMING box decides.
    ///
    /// Mutation guards:
    /// - treat `(0, 0)` as a live 0x0 request (the pre-review arithmetic) and the
    ///   small-source case returns `Some` — a pointless `applyConstraints` on
    ///   every small-window share;
    /// - drop the `want_binds` term and the 5K case returns `None`, leaving the
    ///   first genuine bounding attempt un-made.
    #[test]
    fn screen_track_constraint_treats_zero_last_as_nothing_outstanding() {
        let (cw, ch) = (3840u32, 2160u32);

        // 5K panel whose capture ceiling was DROPPED: nothing was ever requested,
        // so the first tier change must genuinely negotiate the top rung's box.
        assert_eq!(
            screen_track_constraint_for_tier(cw, ch, cw, ch, 5120, 2880, 0, 0),
            Some((cw, ch))
        );
        // …and a smaller tier likewise.
        assert_eq!(
            screen_track_constraint_for_tier(1280, 720, cw, ch, 5120, 2880, 0, 0),
            Some((1280, 720))
        );

        // Small window, nothing outstanding: the incoming box does not bind the
        // source, so there is nothing to negotiate.
        assert_eq!(
            screen_track_constraint_for_tier(1920, 1080, cw, ch, 1280, 720, 0, 0),
            None
        );
    }

    /// Issue #2179 review (PERF): detect an engine that resolves
    /// `applyConstraints` without applying it, and stop deepening the per-frame
    /// rescale when it does.
    ///
    /// Mutation guards:
    /// - make `screen_constraint_was_ignored` return `false` always and the
    ///   "ignored" assertion fails;
    /// - drop the source floor in `screen_encode_box_when_constraint_ignored`
    ///   and the 4K-source case reports the 720p tier box, i.e. the 9x rescale
    ///   the guard exists to prevent.
    #[test]
    fn ignored_capture_constraint_is_detected_and_stops_deepening_the_rescale() {
        // Honoured exactly, and within the rounding tolerance.
        assert!(!screen_constraint_was_ignored(1280, 720, 1280, 720));
        assert!(!screen_constraint_was_ignored(1280, 720, 1282, 720));
        // Ignored: the track is still the full 4K surface.
        assert!(screen_constraint_was_ignored(1280, 720, 3840, 2160));
        // Only one axis over is still ignored.
        assert!(screen_constraint_was_ignored(1280, 720, 1280, 2160));
        // Unknown dims never accuse the engine.
        assert!(!screen_constraint_was_ignored(0, 720, 3840, 2160));
        assert!(!screen_constraint_was_ignored(1280, 720, 0, 0));

        // While honoured, the tier box is used as-is.
        assert_eq!(
            screen_encode_box_when_constraint_ignored(1280, 720, 3840, 2160, 3840, 2160, false),
            (1280, 720)
        );
        // Once ignored, the encode box is floored at the source so the rescale
        // ratio cannot grow with each further step-down.
        assert_eq!(
            screen_encode_box_when_constraint_ignored(1280, 720, 3840, 2160, 3840, 2160, true),
            (3840, 2160)
        );
        // …but never past the capture ceiling (the largest frame that can arrive).
        assert_eq!(
            screen_encode_box_when_constraint_ignored(1280, 720, 5120, 2880, 3840, 2160, true),
            (3840, 2160)
        );
        // Unknown source dims fall back to the tier box.
        assert_eq!(
            screen_encode_box_when_constraint_ignored(1280, 720, 0, 0, 3840, 2160, true),
            (1280, 720)
        );
    }

    /// Only the FIRST OverconstrainedError retries without the ceiling; a second
    /// failure, a non-overconstrained error, and user-cancel all decline.
    #[test]
    fn retry_without_ceiling_only_on_first_overconstrained() {
        assert!(should_retry_screen_capture_without_ceiling(
            "OverconstrainedError",
            false
        ));
        assert!(!should_retry_screen_capture_without_ceiling(
            "OverconstrainedError",
            true
        ));
        assert!(!should_retry_screen_capture_without_ceiling(
            "NotAllowedError",
            false
        ));
        assert!(!should_retry_screen_capture_without_ceiling(
            "NotReadableError",
            false
        ));
        assert!(!should_retry_screen_capture_without_ceiling("", false));
    }

    #[test]
    fn screen_reconfigure_is_not_retried_for_a_target_the_encoder_rejected() {
        use videocall_aq::screen_bitrate::{screen_effective_bitrate_kbps, ScreenPressureStep};
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        let healthy = screen_effective_bitrate_kbps(baseline, ScreenPressureStep::from_raw(0));
        let stepped = screen_effective_bitrate_kbps(baseline, ScreenPressureStep::from_raw(2));
        let other = screen_effective_bitrate_kbps(baseline, ScreenPressureStep::from_raw(4));
        assert_ne!(
            stepped, healthy,
            "precondition: the step must move the target"
        );

        assert!(
            should_attempt_screen_reconfigure(stepped, healthy, None),
            "a new governed target must be attempted"
        );
        assert!(
            !should_attempt_screen_reconfigure(stepped, healthy, Some(stepped)),
            "a target the encoder already rejected must NOT be retried; retrying rebuilds \
             the config and logs on every SCREEN_STATIC_REENCODE_POLL_MS tick (#1221-pt1)"
        );
        assert!(
            should_attempt_screen_reconfigure(stepped, healthy, Some(other)),
            "a DIFFERENT earlier failure must not suppress the current target"
        );
        assert!(
            !should_attempt_screen_reconfigure(healthy, healthy, None),
            "nothing to do when the encoder is already at the governed target"
        );
    }

    /// Reverting to the bare `governed != local_target` difference test fails the
    /// "must not re-log" assertion — one `info!` per poll while the wedge lasts.
    #[test]
    fn screen_step_log_fires_once_per_distinct_step_while_the_reconfigure_is_latched() {
        use videocall_aq::screen_bitrate::{screen_effective_bitrate_kbps, ScreenPressureStep};
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        let healthy = screen_effective_bitrate_kbps(baseline, ScreenPressureStep::from_raw(0));
        let stepped = screen_effective_bitrate_kbps(baseline, ScreenPressureStep::from_raw(2));
        let deeper = screen_effective_bitrate_kbps(baseline, ScreenPressureStep::from_raw(4));
        assert_ne!(
            stepped, healthy,
            "precondition: the step must move the target"
        );
        assert_ne!(deeper, stepped, "precondition: the steps must be distinct");

        let (log, latch) = screen_step_log_decision(stepped, healthy, None);
        assert!(log, "a new step must be visible");
        assert_eq!(latch, Some(stepped));

        let (log, latch) = screen_step_log_decision(stepped, healthy, latch);
        assert!(
            !log,
            "the SAME governed target must not re-log while latched"
        );
        assert_eq!(latch, Some(stepped));

        let (log, latch) = screen_step_log_decision(deeper, healthy, latch);
        assert!(log, "a DIFFERENT step must still be visible");
        assert_eq!(latch, Some(deeper));

        let (log, latch) = screen_step_log_decision(healthy, healthy, latch);
        assert!(!log, "nothing to report at the applied target");
        assert_eq!(latch, None, "back at the applied target the latch re-arms");

        let (log, _) = screen_step_log_decision(deeper, healthy, latch);
        assert!(
            log,
            "a later step to the same value must log again after re-arming"
        );
    }

    // ── Issue #1921: WS send-side screen freshness gate ──────────────────────
    // These drive the REAL production decision (`screen_ws_send_decision`) and
    // threshold (`screen_ws_freshness_threshold_bytes`) the encode output
    // handler calls — not a re-implemented copy — so mutating either policy
    // breaks an assertion here.

    /// A KEYFRAME is ALWAYS sent, even when the WS backlog dwarfs the threshold.
    /// Mutating the `is_keyframe ⇒ Send` arm to drop would fail this.
    #[test]
    fn screen_ws_keyframe_always_sent_even_when_backlog_huge() {
        // buffered 10 MB, threshold a mere 100 B, but it's a keyframe.
        assert_eq!(
            screen_ws_send_decision(Some(10_000_000), true, 100),
            ScreenWsSend::Send
        );
    }

    /// A stale DELTA over the threshold is dropped. Mutating the comparison
    /// away (or the arm to `Send`) would fail this.
    #[test]
    fn screen_ws_delta_dropped_when_backlog_over_threshold() {
        assert_eq!(
            screen_ws_send_decision(Some(200_000), false, 125_000),
            ScreenWsSend::DropStaleDelta
        );
    }

    /// The boundary is strict `>`: a DELTA at EXACTLY the threshold is sent, one
    /// byte above is dropped, one below is sent. Pins `>` against a `>=`
    /// mutation and an off-by-one.
    #[test]
    fn screen_ws_delta_threshold_boundary_is_strict_greater_than() {
        assert_eq!(
            screen_ws_send_decision(Some(125_000), false, 125_000),
            ScreenWsSend::Send,
            "at exactly the threshold the delta must still be sent"
        );
        assert_eq!(
            screen_ws_send_decision(Some(125_001), false, 125_000),
            ScreenWsSend::DropStaleDelta,
            "one byte over the threshold must drop"
        );
        assert_eq!(
            screen_ws_send_decision(Some(124_999), false, 125_000),
            ScreenWsSend::Send,
            "below the threshold must send"
        );
    }

    /// On WebTransport (or before election) the depth is `None` — screen has its
    /// own QUIC unistream, so nothing is ever dropped here even for a delta with
    /// a threshold of 1. Mutating the `None ⇒ Send` arm would fail this and is
    /// the guard that keeps the WT path byte-for-byte unchanged.
    #[test]
    fn screen_ws_none_depth_never_drops() {
        assert_eq!(screen_ws_send_decision(None, false, 1), ScreenWsSend::Send);
        assert_eq!(screen_ws_send_decision(None, true, 1), ScreenWsSend::Send);
    }

    /// The threshold is 500 ms of the current screen target bitrate. Exact
    /// values pin the formula (`kbps * 125 * 500 / 1000`); mutating the delay
    /// constant or the arithmetic shifts these.
    #[test]
    fn screen_ws_threshold_scales_with_bitrate() {
        // 2500 kbps → 312.5 KB/s → 500 ms ≈ 156_250 bytes.
        assert_eq!(screen_ws_freshness_threshold_bytes(2500), 156_250);
        // 2000 kbps → 250 KB/s → 500 ms = 125_000 bytes.
        assert_eq!(screen_ws_freshness_threshold_bytes(2000), 125_000);
    }

    /// A low tier's raw 500 ms figure falls below the floor and is clamped, so
    /// the gate never fires on a trivially small backlog. Mutating away the
    /// `.max(floor)` would fail this.
    #[test]
    fn screen_ws_threshold_floored_for_low_bitrate() {
        // 100 kbps → raw 100*125*500/1000 = 6_250 bytes, below the 16 KB floor,
        // so the production fn must clamp it up to exactly the floor.
        assert_eq!(
            screen_ws_freshness_threshold_bytes(100),
            SCREEN_WS_MIN_THRESHOLD_BYTES
        );
    }
    /// Issue #1922: the source-dimension SETTLE gate. Drives the REAL production `DimensionSettle`
    /// (the tracker the encode loop's two source-dim reconfigure sites gate on) through a window
    /// drag-resize: a burst of dimension deltas at drag cadence, then the source holding steady.
    ///
    /// It models the loop's frame-arrival gate faithfully — `observe(raw)` then reconfigure iff the
    /// (here unclamped) fitted dims drift from the applied dims AND `is_settled(now, SCREEN_DIM_SETTLE_MS)`
    /// — and counts how many reconfigures fire. The pre-#1922 code had NO settle term, so it
    /// reconfigured once per delta (a ~140-`configure()` storm, each an implicit keyframe). With the
    /// gate, ZERO reconfigures fire during the drag and EXACTLY ONE fires once the source has held
    /// steady for the settle window.
    ///
    /// MUTATION RECEIPT (verified by reverting): dropping the `&& is_settled(..)` term at the call
    /// site — or making `settled_dims`/`is_settled` ignore the elapsed-time check (return settled on
    /// the first observation), or making `observe` not re-stamp `changed_at_ms` on a change — makes the
    /// gate fire once per delta, so `reconfigures` overruns 1 and the "exactly one" / "zero during
    /// drag" assertions fail.
    #[test]
    fn dim_settle_defers_reconfigure_until_source_settles() {
        let settle = SCREEN_DIM_SETTLE_MS;
        let mut s = DimensionSettle::new();
        // Applied encoder dims (mirror of `current_encoder_*`), seeded to the pre-drag size.
        let mut applied = (1280u32, 720u32);
        let mut reconfigures = 0u32;

        // A drag emits deltas continuously at ~55ms (the field's peak 18/sec). Each frame carries
        // DIFFERENT source dims. The settle window must exceed this gap so no delta reads as settled.
        let delta_gap = 55.0f64;
        let mut now = 1_000.0f64;
        let mut w = 1280u32;
        let deltas = 20u32; // ~1.1s of dragging; the field saw up to 18 deltas in ONE second
        for _ in 0..deltas {
            w += 8; // window growing as the user drags
            let (raw_w, raw_h) = (w, 720u32);
            s.observe(raw_w, raw_h, now);
            // Loop gate (unclamped fit == raw here): drift AND settled.
            if (raw_w != applied.0 || raw_h != applied.1) && s.is_settled(now, settle) {
                applied = (raw_w, raw_h);
                reconfigures += 1;
            }
            now += delta_gap;
        }
        assert_eq!(
            reconfigures, 0,
            "no reconfigure may fire WHILE the drag is still moving the source dims"
        );

        // Drag ends: the source holds at its final size. Advance to exactly the inclusive settle
        // boundary since the LAST delta (whose `observe` was at `now - delta_gap`), then observe the
        // final dims once more (live-content case). The first steady observation at/after the window
        // reconfigures exactly once.
        let final_dims = (w, 720u32);
        now += settle - delta_gap;
        s.observe(final_dims.0, final_dims.1, now);
        if (final_dims.0 != applied.0 || final_dims.1 != applied.1) && s.is_settled(now, settle) {
            applied = final_dims;
            reconfigures += 1;
        }
        assert_eq!(
            reconfigures, 1,
            "exactly ONE reconfigure fires once the source has held steady for the settle window"
        );
        assert_eq!(
            applied, final_dims,
            "the applied dims are the SETTLED final size"
        );

        // Further steady observations do NOT re-fire (drift is now false).
        for _ in 0..5 {
            now += 200.0;
            s.observe(final_dims.0, final_dims.1, now);
            if (final_dims.0 != applied.0 || final_dims.1 != applied.1) && s.is_settled(now, settle)
            {
                reconfigures += 1;
            }
        }
        assert_eq!(
            reconfigures, 1,
            "a stable source never re-triggers a reconfigure"
        );
    }

    /// Issue #1922: a genuine ONE-SHOT resolution change (window snapped to a size, a display change)
    /// must STILL reconfigure — after exactly one settle window, never wedged at the old dims. Drives
    /// the production `settled_dims` at the inclusive boundary.
    #[test]
    fn dim_settle_applies_single_legit_resolution_change() {
        let settle = SCREEN_DIM_SETTLE_MS;
        let mut s = DimensionSettle::new();
        let t0 = 5_000.0f64;
        s.observe(1920, 1080, t0);
        assert!(
            s.settled_dims(t0 + settle - 1.0, settle).is_none(),
            "inside the settle window a one-shot change is not yet applied"
        );
        assert_eq!(
            s.settled_dims(t0 + settle, settle),
            Some((1920, 1080)),
            "a one-shot resolution change is applied after exactly one settle window"
        );
    }

    /// Issue #1922: the settle state is per-encoder-session. A fresh tracker (new share session or an
    /// encoder `'restart`, where the loop re-declares it via `DimensionSettle::new()`) carries NO
    /// pending resize target from the prior session.
    #[test]
    fn dim_settle_resets_across_sessions() {
        let settle = SCREEN_DIM_SETTLE_MS;
        // Session A settles at one size.
        let mut a = DimensionSettle::new();
        a.observe(1600, 900, 0.0);
        assert_eq!(a.settled_dims(settle, settle), Some((1600, 900)));

        // A NEW session starts empty — nothing is settled until its own first frame, even long after
        // wall-clock has advanced.
        let b = DimensionSettle::new();
        assert_eq!(
            b,
            DimensionSettle::new(),
            "a new session's settle tracker is the cold state"
        );
        assert!(
            b.settled_dims(1_000_000.0, settle).is_none(),
            "a fresh session has no settled dims until it observes its own frame"
        );
    }

    /// Issue #1922 req 4d: 0/invalid transient dims (a minimized/occluded capture) never produce a
    /// reconfigure decision, and a transient invalid frame mid-hold must not disturb an already-settled
    /// value — it must neither re-arm the settle clock nor overwrite the last valid dims.
    #[test]
    fn dim_settle_ignores_zero_and_invalid_transient_dims() {
        let settle = SCREEN_DIM_SETTLE_MS;

        // Zero/invalid dims alone never settle.
        let mut z = DimensionSettle::new();
        z.observe(0, 1080, 0.0);
        z.observe(1920, 0, 10.0);
        z.observe(0, 0, 20.0);
        assert!(
            z.settled_dims(1_000_000.0, settle).is_none(),
            "invalid (0-axis) dims are never stored, so nothing ever settles"
        );

        // A valid value that then sees a transient 0x0 mid-hold stays settled at the valid value: the
        // invalid frame neither re-arms the clock nor overwrites the dims.
        let mut s = DimensionSettle::new();
        s.observe(1920, 1080, 100.0);
        s.observe(0, 0, 100.0 + settle / 2.0);
        assert_eq!(
            s.settled_dims(100.0 + settle, settle),
            Some((1920, 1080)),
            "a transient 0x0 mid-hold must not reset the settle clock or overwrite the last valid dims"
        );
    }

    /// Issue #1903: the static-share keyframe FLOOR gate. Drives the REAL production predicate
    /// (`static_keyframe_floor_due`, the fn the encode loop's timer branch calls) so a mutation to
    /// any of its three rules breaks an assertion here. The floor cadence under test is the exact
    /// production constant the loop passes in (`SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS`), and the
    /// budget is the real `SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET` — not a re-declared copy.
    #[test]
    fn static_keyframe_floor_due_respects_cadence_budget_and_first_frame() {
        let floor = SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS;

        // No keyframe emitted yet (None): nothing to re-encode, so never due — the real-frame arm
        // owns the first keyframe. (Deleting the `None => false` guard flips this.)
        assert!(
            !static_keyframe_floor_due(10_000.0, None, floor, SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET),
            "with no prior keyframe the floor is never due"
        );

        // Budget exhausted: a truly-idle share that already spent its window must back off even long
        // after the last keyframe. (Deleting the `budget_remaining == 0` guard flips this.)
        assert!(
            !static_keyframe_floor_due(1_000_000.0, Some(0.0), floor, 0),
            "an exhausted floor budget suppresses further floor keyframes"
        );

        // Within the floor window: not yet due. (Mutating `>=`→`<=`/inverting the compare flips this.)
        assert!(
            !static_keyframe_floor_due(
                1_000.0 + floor - 1.0,
                Some(1_000.0),
                floor,
                SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET
            ),
            "inside the floor cadence window the floor is not yet due"
        );

        // Exactly at the inclusive boundary WITH budget: due. (Mutating `>=`→`>` flips this.)
        assert!(
            static_keyframe_floor_due(
                1_000.0 + floor,
                Some(1_000.0),
                floor,
                SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET
            ),
            "at exactly floor_ms since the last keyframe (budget available) the floor fires"
        );

        // Past the window with a single remaining budget unit: still due (the last covered cycle).
        assert!(
            static_keyframe_floor_due(1_000.0 + floor + 500.0, Some(1_000.0), floor, 1),
            "past the window with budget remaining the floor is due"
        );
    }

    /// Issue #1903 — the static-share FLOOR budget ACCOUNTING (the loop side effects the gate test
    /// above does not exercise). Simulates the encode loop's quiet-tick budget bookkeeping through the
    /// SAME production helpers the loop calls (`static_keyframe_floor_due`, `floor_budget_after_emit`,
    /// `floor_budget_replenished`), pinning two behaviors that were previously untested loop side
    /// effects. First: after capture goes quiet the floor emits EXACTLY
    /// `SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET` keyframes, then backs off (does not re-encode room-wide
    /// forever). Second: a real captured frame re-arms the budget so a later quiet period earns a fresh
    /// window.
    ///
    /// MUTATION RECEIPTS (verified by reverting each in turn). Making `floor_budget_after_emit(b)`
    /// return `b` (decrement removed) leaves the budget undrained, so `static_keyframe_floor_due` stays
    /// true every tick and the emit count overruns `SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET` — the "emits
    /// exactly BUDGET" assertion fails. Making `floor_budget_replenished()` return `0` (replenish
    /// removed) leaves the budget at 0 after a captured frame, so the re-arm and resume assertions fail.
    #[test]
    fn static_floor_budget_stops_after_full_budget_and_rearms_on_captured_frame() {
        let floor = SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS;

        // A captured frame re-arms to the FULL budget.
        let mut budget = floor_budget_replenished();
        assert_eq!(
            budget, SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET,
            "a real captured frame must re-arm the floor budget to the full value"
        );

        // (a) Drive many floor-interval ticks with NO intervening real frame (no replenish). Count how
        // many floor keyframes actually emit; it must be EXACTLY the budget, then stop.
        let mut last_kf = 1_000.0f64;
        let mut now = last_kf;
        let mut emits = 0u32;
        for _ in 0..(SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET + 5) {
            now += floor;
            if static_keyframe_floor_due(now, Some(last_kf), floor, budget) {
                emits += 1;
                budget = floor_budget_after_emit(budget);
                last_kf = now;
            }
        }
        assert_eq!(
            emits, SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET,
            "a quiet static share must emit exactly SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET floor keyframes, then back off"
        );
        assert_eq!(
            budget, 0,
            "the floor budget must be fully drained after its window"
        );
        now += floor;
        assert!(
            !static_keyframe_floor_due(now, Some(last_kf), floor, budget),
            "with the budget drained the floor must stay off (no perpetual idle-bandwidth re-encode)"
        );

        // (b) A real captured frame re-arms the budget → the floor resumes for another full window.
        budget = floor_budget_replenished();
        assert!(
            static_keyframe_floor_due(now + floor, Some(last_kf), floor, budget),
            "after a captured frame re-arms the budget, the floor must be due again"
        );
        let mut emits_after_rearm = 0u32;
        for _ in 0..(SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET + 5) {
            now += floor;
            if static_keyframe_floor_due(now, Some(last_kf), floor, budget) {
                emits_after_rearm += 1;
                budget = floor_budget_after_emit(budget);
                last_kf = now;
            }
        }
        assert_eq!(
            emits_after_rearm, SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET,
            "after re-arming, the floor must again emit exactly SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET keyframes"
        );
    }

    /// Issue #1903 (LIVE-STACK root cause) — the static-share FLOOR accounting MUST survive an encoder
    /// `'restart`. The first #1903 cut kept the budget and cadence clock INSIDE the encode loop's
    /// `'restart` scope (and drove the floor off the `'restart`-reset `last_keyframe_emit_ms`), so an
    /// encoder restart (fatal encode error / closed codec / stream replace) zeroed them; on a share that
    /// had gone STATIC no fresh frame followed to restore them, so the floor NEVER fired again on the
    /// live docker stack even though every pure-helper test passed. This drives the real
    /// `ScreenFloorAccount` transitions the encode loop calls — including `carry_across_restart` at the
    /// restart boundary — and asserts the floor is still due after a restart-then-static.
    ///
    /// FAILS ON THE BROKEN SEMANTIC (verified): making `carry_across_restart(self) -> Self` return
    /// `Self::idle()` (reset budget/clock on restart — the pre-fix behavior) drops the budget to 0 and
    /// the clock to `None`, so the post-restart `floor_due` assertion fails. This is the receipt the
    /// live bug demanded: it pins the loop-wiring/lifecycle contract, not just the pure helpers.
    #[test]
    fn floor_account_survives_restart_and_floors_on_static() {
        let floor = SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS;

        // Active sharing: a captured frame arms the budget; a keyframe stamps the floor cadence clock.
        let mut acct = ScreenFloorAccount::idle();
        acct.on_captured_frame();
        let t_kf = 10_000.0f64;
        acct.on_keyframe_emitted(t_kf);
        assert_eq!(
            acct.budget, SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET,
            "a captured frame arms the full floor budget"
        );

        // An encoder restart fires while the share is ALREADY static — no fresh frame follows. The
        // account is carried across the restart exactly as the encode loop does at the restart boundary.
        acct = acct.carry_across_restart();

        // Not due within the cadence window...
        assert!(
            !acct.floor_due(t_kf + floor - 1.0, floor),
            "the floor is not due before one full cadence has elapsed"
        );
        // ...but DUE one cadence after the last keyframe — PROVING the budget and cadence clock survived
        // the restart. On the pre-fix (reset-on-restart) semantic this is false forever (clock None,
        // budget 0), which is exactly the live freeze.
        assert!(
            acct.floor_due(t_kf + floor, floor),
            "after a restart-then-static the floor must still become due — budget+clock survive the restart (#1903)"
        );

        // And it still drains correctly post-restart: exactly BUDGET floor emits, then backs off.
        let mut now = t_kf;
        let mut emits = 0u32;
        for _ in 0..(SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET + 5) {
            now += floor;
            if acct.floor_due(now, floor) {
                emits += 1;
                acct.on_floor_emitted(now);
            }
        }
        assert_eq!(
            emits, SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET,
            "after a restart the floor still emits exactly SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET keyframes then backs off"
        );
    }

    /// Issue #2328 (root cause of #2258): a PRESSURE-GATED static-share floor emit must NOT defer
    /// the full-fan-out periodic keyframe.
    ///
    /// This composes the PRODUCTION functions exactly as the wasm-only encode loop does —
    /// `FullFanoutKeyframeClock::on_keyframe_emitted` decides whether an emit re-arms the ceiling,
    /// and `periodic_keyframe_due` reads `anchor_ms()` — so there is no re-implemented copy of the
    /// policy here. The partial fan-out is supplied directly: issue 2343 left one screen rung, so
    /// no production caller can produce one any more. `frame_counter` is held
    /// at a NON-multiple of the interval throughout so the frame-count half of
    /// `periodic_keyframe_due` is provably out of the way and only the wall-clock half is under test
    /// (a static-ish share advances the counter far too slowly for the frame-count path to cover the
    /// gap — that is why the wall-clock ceiling is the top rung's only insurance).
    ///
    /// ## Why it fails on the un-fixed code (the arithmetic)
    /// Before the fix the floor arm and the real arm shared ONE clock, so the floor's `Some(now)`
    /// was the anchor. Here: 3 active rungs with a fan-out of 2, so the floor re-keys rungs 0-1 and
    /// never rung 2. Emitting at t=0, 3000, 6000, 9000 (the
    /// SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS cadence) and probing at t=9000+interval-1 leaves
    /// `now - shared_anchor` = interval-1 < interval on EVERY probe — the pre-fix code therefore
    /// answers `false` forever and rung 2 is never re-keyed. With the fix the gated emits leave the
    /// anchor at `None`, so `periodic_keyframe_due` answers `true` (its documented mid-session
    /// `None` ⇒ due-once-a-frame-has-been-counted branch) and the real arm emits a FULL fan-out.
    ///
    /// ## Why the wall-clock half is the ONLY guarantee (severity)
    /// The `medium` screen tier is `target_fps: 8, keyframe_interval_frames: 24`, and its constant
    /// documents "~3s at 8fps (text readability); wall-clock cap guarantees ≤3s". But #2259 measured
    /// the screen encoder's ACTUAL output during a real share at ~1.8 fps average (max 4), not the
    /// nominal 8 — so the frame-count path needs 24 / 1.8 ≈ 13.3 SECONDS, not 3. The wall-clock
    /// backstop is therefore the only thing delivering the advertised ≤3s cap, and a gated floor
    /// emit was resetting it on that very same 3000ms cadence. Net: on the shipped code the interval
    /// between FULL-fan-out keyframes was UNBOUNDED, which is why #2258's victim stayed frozen for
    /// the whole 4m12s share — and that tier constant's "guarantees ≤3s" comment was itself false
    /// for the gated-floor case. This test asserts the contract in those terms (elapsed ≥
    /// `SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS` since the last FULL fan-out ⇒ due) rather than in
    /// terms of an implementation detail, so it stays meaningful if the constant is retuned.
    ///
    /// The second half pins the converse so the fix cannot be "never stamp": a floor emit that
    /// answers a PLI fans out to the full active set (`pli_pending` branch in the loop) and MUST
    /// still defer the ceiling — otherwise every static tick would re-key every rung.
    ///
    /// ## What this test does NOT protect
    /// It pins the CLOCK's semantics, composed here by the test. It does not reach the encode
    /// loop's call site. Pinning the anchor CHOICE is the separate job of
    /// [`the_screen_periodic_backstop_is_anchored_on_the_full_fanout_clock`] below, via the
    /// `screen_periodic_keyframe_due` wrapper; read that fn's doc for the residual the two tests
    /// together still cannot cover.
    #[test]
    fn a_gated_floor_emit_does_not_defer_the_full_fanout_periodic_keyframe() {
        let interval = SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS;
        let active = 3usize;
        let gated_fanout = 2usize;
        assert!(gated_fanout < active);

        // --- Gated floor emits at the full 3s cadence must leave the ceiling armed. ---
        let mut clock = FullFanoutKeyframeClock::new();
        // Non-multiple of the interval so `frame_counter % keyframe_interval_frames != 0`
        // (7 % 300 == 7): the frame-count half of the predicate is off for every probe below.
        let frame_counter = 7u32;
        let keyframe_interval_frames = 300u32;
        assert_ne!(
            frame_counter % keyframe_interval_frames,
            0,
            "guard: the frame-count half of periodic_keyframe_due must be inert in this test"
        );
        for i in 0..4 {
            let t = i as f64 * interval;
            clock.on_keyframe_emitted(t, gated_fanout, active);
        }
        assert_eq!(
            clock.anchor_ms(),
            None,
            "four gated floor emits must not have re-armed the full-fan-out ceiling"
        );
        let probe = 3.0 * interval + interval - 1.0;
        assert!(
            periodic_keyframe_due(
                frame_counter,
                keyframe_interval_frames,
                probe,
                clock.anchor_ms(),
                interval,
            ),
            "after gated floor emits the real arm's periodic keyframe must be DUE; on the un-fixed \
             shared-clock code the anchor would be {last:?} and now - last = {gap} < {interval}, \
             i.e. NOT due — the permanent keyframe-less hold of #2258",
            last = Some(3.0 * interval),
            gap = probe - 3.0 * interval,
        );

        // --- The converse: a FULL fan-out emit (the PLI-answer branch) DOES defer it. ---
        let mut clock = FullFanoutKeyframeClock::new();
        clock.on_keyframe_emitted(0.0, active, active);
        assert_eq!(
            clock.anchor_ms(),
            Some(0.0),
            "a full fan-out emit re-arms the ceiling"
        );
        assert!(
            !periodic_keyframe_due(
                frame_counter,
                keyframe_interval_frames,
                interval - 1.0,
                clock.anchor_ms(),
                interval,
            ),
            "within the interval of a FULL fan-out keyframe the periodic must not be due"
        );
        assert!(
            periodic_keyframe_due(
                frame_counter,
                keyframe_interval_frames,
                interval,
                clock.anchor_ms(),
                interval,
            ),
            "at exactly the interval boundary the periodic is due again (inclusive `>=`)"
        );

        // A base-only publisher (active == 1, or the defensive 0) still counts as full coverage.
        let mut clock = FullFanoutKeyframeClock::new();
        clock.on_keyframe_emitted(42.0, 1, 1);
        assert_eq!(
            clock.anchor_ms(),
            Some(42.0),
            "single-rung share is covered"
        );
        let mut clock = FullFanoutKeyframeClock::new();
        clock.on_keyframe_emitted(42.0, 1, 0);
        assert_eq!(
            clock.anchor_ms(),
            Some(42.0),
            "a 0 active count is floored at 1, so a base emit still re-arms the ceiling"
        );
    }

    /// Issue #2328 — the WIRING guard for the sibling test above.
    ///
    /// The sibling pins the clock's semantics but composes them itself, so the single most likely
    /// regression — swapping the encode loop's anchor argument back to `last_keyframe_emit_ms`
    /// while leaving the struct and every stamp intact — would reintroduce #2258 in full and leave
    /// it green. This test pins the anchor CHOICE instead, by executing the production
    /// `screen_periodic_keyframe_due`, which is the function the loop now calls.
    ///
    /// The decoy is the whole point: `last_keyframe_emit_ms` is set to a value that would make the
    /// predicate answer FALSE, and the full-fan-out clock to one that makes it answer TRUE. The
    /// production wrapper takes the clock by TYPE, so it cannot even be handed the decoy — which is
    /// exactly the guard: the one-token revert is now a compile error rather than a silent
    /// regression. Any reimplementation of the wrapper's body that reads a cooldown-shaped anchor
    /// flips this assertion.
    ///
    /// Arithmetic: `SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS` (3000ms) is the SAME constant that
    /// drives the floor's cadence, so on the un-fixed code a gated floor emit landed exactly as the
    /// ceiling came due and reset it — the full fan-out was not delayed, it was structurally unable
    /// to fire. Here the decoy stands at t=9000 (the last gated floor emit) and the true full
    /// fan-out at t=0; probing at t=3000, the shared-clock answer is `3000-9000 < 0` ⇒ false, and
    /// the correct answer is `3000-0 >= 3000` ⇒ true.
    ///
    /// RESIDUAL (stated so no reviewer over-trusts this pair): neither test can reach the encode
    /// loop's line itself, so a DELIBERATE rewrite that deletes the wrapper call and re-inlines
    /// `periodic_keyframe_due(.., last_keyframe_emit_ms, ..)` would still compile and still pass.
    /// That case is covered only by the E2E spec.
    #[test]
    fn the_screen_periodic_backstop_is_anchored_on_the_full_fanout_clock() {
        let interval = SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS;
        // Frame-count half held inert (7 % 300 != 0) so only the wall-clock anchor is under test.
        let frame_counter = 7u32;
        let keyframe_interval_frames = 300u32;

        // The DECOY: what `last_keyframe_emit_ms` would read after gated floor emits on the 3000ms
        // cadence. Any implementation anchored on it answers `false` here.
        let pli_cooldown_decoy_ms = Some(3.0 * interval);
        // The truth: the last emit that actually reached every rung.
        let mut full_fanout = FullFanoutKeyframeClock::new();
        full_fanout.on_keyframe_emitted(0.0, 3, 3);

        let now = interval; // exactly one ceiling since the last FULL fan-out
        assert!(
            !periodic_keyframe_due(
                frame_counter,
                keyframe_interval_frames,
                now,
                pli_cooldown_decoy_ms,
                interval,
            ),
            "guard: the decoy anchor must produce the OPPOSITE answer, or this test proves nothing"
        );
        assert!(
            screen_periodic_keyframe_due(
                frame_counter,
                keyframe_interval_frames,
                now,
                &full_fanout,
                interval,
            ),
            "the periodic backstop must be anchored on the FULL-FAN-OUT clock: one ceiling has \
             elapsed since every rung was last re-keyed, so a keyframe is due — even though the \
             shared PLI-cooldown clock was reset by a gated floor emit moments ago"
        );

        // And it must still DEFER while that ceiling has not elapsed, so the guard is not just
        // "always true".
        assert!(
            !screen_periodic_keyframe_due(
                frame_counter,
                keyframe_interval_frames,
                interval - 1.0,
                &full_fanout,
                interval,
            ),
            "inside the ceiling of the last full fan-out the periodic must not be due"
        );
    }

    /// MUTATION: change `active_screen_tier_fps` or retune the rung's cadence —
    /// the literal 10 here is not a re-read of the table.
    #[test]
    fn active_screen_tier_fps_maps_each_tier_and_clamps() {
        assert_eq!(
            active_screen_tier_fps(0),
            10,
            "the single native rung budgets at 10 fps"
        );
        assert_eq!(active_screen_tier_fps(99), 10);
        assert_eq!(active_screen_tier_fps(u32::MAX), 10);
    }

    #[test]
    fn record_screen_restart_increments_each_reason_counter() {
        let before_closed = screen_encoder_restarts_closed_codec();
        let before_memory = screen_encoder_restarts_memory();
        let before_configure = screen_encoder_restarts_configure();
        let before_other = screen_encoder_restarts_other();

        record_screen_restart(RestartReason::ClosedCodec);
        record_screen_restart(RestartReason::Memory);
        record_screen_restart(RestartReason::Configure);
        record_screen_restart(RestartReason::Other);

        assert!(screen_encoder_restarts_closed_codec() > before_closed);
        assert!(screen_encoder_restarts_memory() > before_memory);
        assert!(screen_encoder_restarts_configure() > before_configure);
        assert!(screen_encoder_restarts_other() > before_other);
    }

    fn build_test_client() -> VideoCallClient {
        VideoCallClient::new(VideoCallClientOptions {
            enable_e2ee: false,
            enable_webtransport: false,
            max_received_layer: None,
            skip_canvas_paint: false,
            on_peer_added: Callback::noop(),
            on_peer_first_frame: Callback::noop(),
            on_peer_removed: None,
            on_peers_removed_batch: None,
            refresh_room_token_callback: None,
            get_peer_video_canvas_id: Callback::from(|id| id),
            get_peer_screen_canvas_id: Callback::from(|id| id),
            user_id: "test-user".to_string(),
            display_name: "test".to_string(),
            meeting_id: "test-meeting".to_string(),
            websocket_urls: Vec::new(),
            webtransport_urls: Vec::new(),
            on_connected: Callback::noop(),
            on_connection_lost: Callback::noop(),
            enable_diagnostics: false,
            diagnostics_update_interval_ms: None,
            enable_health_reporting: false,
            health_reporting_interval_ms: None,
            on_encoder_settings_update: None,
            rtt_testing_period_ms: 2000,
            rtt_probe_interval_ms: None,
            on_meeting_info: None,
            on_meeting_ended: None,
            on_speaking_changed: None,
            on_audio_level_changed: None,
            vad_threshold: None,
            on_meeting_activated: None,
            on_participant_admitted: None,
            on_participant_rejected: None,
            on_waiting_room_updated: None,
            on_meeting_settings_updated: None,
            on_peer_left: None,
            on_peer_joined: None,
            on_reaction: None,
            on_raise_hand: None,
            on_meeting_timer: None,
            on_display_name_changed: None,
            on_host_mute: None,
            on_host_disable_video: None,
            on_participant_kicked: None,
            on_host_granted: None,
            on_host_revoked: None,
            on_peer_event: None,
            decode_media: true,
            is_guest: false,
            allow_post_rebase_retry: true,
        })
    }

    #[test]
    fn screen_capture_is_reacquired_after_any_restart() {
        assert!(should_reacquire_screen_capture(false, 0));
        assert!(should_reacquire_screen_capture(true, 1));
        assert!(should_reacquire_screen_capture(true, 4));
    }

    /// Regression: a capture TRACK end (`reader.read()` -> done, i.e. the user
    /// stopped the share, clicked the browser's "Stop sharing" button, or the OS
    /// revoked capture) MUST terminate the encode task, NOT re-enter the restart
    /// loop.
    ///
    /// The bug this pins: on a track end the loop used to fall through to the
    /// auto-restart path (`continue 'restart`) unconditionally. Because a single
    /// `ScreenEncoder` (and its shared `EncoderState.enabled` `Arc` +
    /// `screen_stream` / `active_video_track` cells) is REUSED for the user's
    /// next share, that zombie restart raced the next share: it clobbered the new
    /// task's shared stream/track cells and its non-gesture `getDisplayMedia()`
    /// rejected, storing `enabled = false` and killing the legitimate new encode
    /// task. Symptom: after "stop share, immediately share again" the peer stayed
    /// frozen on the last frame and the sharer saw "No peers received the shared
    /// content within 10 seconds".
    ///
    /// Mutation sensitivity: reverting the fix — i.e. making the track-end case
    /// restart like every other exit (`post_encode_exit_action` always returning
    /// `Restart`, the pre-fix behavior) — flips the first assertion and FAILS.
    /// The `false` arm pins that genuine auto-recovery (fatal codec fault /
    /// transient read error, track still alive) is preserved, so the fix is a
    /// true behavioral difference and not a blanket "never restart".
    #[test]
    fn track_end_shuts_down_and_never_auto_restarts() {
        assert_eq!(
            post_encode_exit_action(true),
            PostEncodeExit::Shutdown,
            "a capture-track end must shut the encode task down, never auto-restart \
             (auto-restart races the reused ScreenEncoder's shared state with the next share)"
        );
        assert_eq!(
            post_encode_exit_action(false),
            PostEncodeExit::Restart,
            "a non-track-end exit (fatal codec fault / transient read error, track still \
             alive) must still re-enter the restart loop — genuine auto-recovery is preserved"
        );
    }

    #[test]
    fn clamp_screen_layer_count_treats_zero_and_one_as_one() {
        // Explicit 0/1 inputs → single layer (feature-off / legacy screen path).
        assert_eq!(clamp_screen_layer_count(0), 1);
        assert_eq!(clamp_screen_layer_count(1), 1);
    }

    /// MUTATION: untie `SCREEN_SIMULCAST_MAX_SUPPORTED_LAYERS` from the AQ ladder
    /// and a request for 2 or 3 rungs survives the clamp.
    #[test]
    fn clamp_screen_layer_count_passes_through_and_caps() {
        for requested in [2, 3, 99] {
            assert_eq!(
                clamp_screen_layer_count(requested),
                SCREEN_SIMULCAST_MAX_SUPPORTED_LAYERS
            );
        }
        assert_eq!(
            SCREEN_SIMULCAST_MAX_SUPPORTED_LAYERS, 1,
            "raising this activates the un-latched bitrate arms (local_layer0_bitrate_bps and LayerEncoder::reconfigure_at_bitrate) - see issue 2550",
        );
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

    /// #2458. MUTATION: return a fresh handle from `control_loop_cancel_token()`.
    #[test]
    fn control_loop_cancel_token_exits_loop_without_dropping_encoder_2458() {
        let encoder = ScreenEncoder::new(
            build_test_client(),
            500,
            Callback::from(|_: String| {}),
            Callback::from(|_: ScreenShareEvent| {}),
            Rc::new(AtomicBool::new(false)),
            1,
        );
        let liveness = Rc::downgrade(&encoder.control_loop_liveness);
        let token = encoder.control_loop_cancel_token();

        assert!(!crate::encode::aq_control_loop_should_exit(
            &liveness,
            &encoder.control_loop_cancel
        ));

        token.cancel();

        assert!(
            liveness.upgrade().is_some(),
            "fixture must keep the encoder alive, else this pins drop and not cancellation"
        );
        assert!(crate::encode::aq_control_loop_should_exit(
            &liveness,
            &encoder.control_loop_cancel
        ));
    }

    /// Issue #1199: the screen encoder must read an EXTERNALLY-owned congestion
    /// step-down flag (the client hands it the screen-specific atom). This pins
    /// that `set_congestion_step_down_flag` rewires the screen encoder's internal
    /// flag to the shared atom — the same indirection as the re-election signal —
    /// so a server CONGESTION signal set by the client reaches the screen AQ
    /// loop. It also pins the SEPARATE-flag design: the screen flag is a distinct
    /// atom from the camera's, so the two AQ loops' `swap(false)` consumers never
    /// race over one shared flag.
    #[test]
    fn screen_encoder_reads_externally_owned_congestion_flag() {
        let client = build_test_client();
        let mut encoder = ScreenEncoder::new(
            client,
            500,
            Callback::from(|_: String| {}),
            Callback::from(|_: ScreenShareEvent| {}),
            Rc::new(AtomicBool::new(false)),
            1, // max_layers (single layer)
        );

        // Two distinct flags stand in for the client's camera vs screen atoms.
        // The congestion flag is an `Arc` (shared with the client, like the
        // keyframe flags), distinct from the `Rc` re-election signal.
        let camera_flag = std::sync::Arc::new(AtomicBool::new(false));
        let screen_flag = std::sync::Arc::new(AtomicBool::new(false));
        encoder.set_congestion_step_down_flag(screen_flag.clone());

        // The client sets the SCREEN flag (the CONGESTION dispatch sets both).
        screen_flag.store(true, Ordering::Release);
        assert!(
            encoder.congestion_step_down.swap(false, Ordering::AcqRel),
            "screen encoder must observe the externally-owned SCREEN congestion flag"
        );
        // The camera's flag is independent: setting it must NOT appear on the
        // screen encoder's flag (separate atoms — no swap race).
        camera_flag.store(true, Ordering::Release);
        assert!(
            !encoder.congestion_step_down.load(Ordering::Acquire),
            "the screen congestion flag must be SEPARATE from the camera's"
        );
    }

    /// #2147: the BROWSER's own "Stop sharing" button (the track `onended` path)
    /// must zero the output-fps atom, not just clear the enabled/sharing flags.
    ///
    /// That atom is exported as `screen_encoder_output_fps` → the deliberately
    /// ungated `videocall_screen_encoder_output_fps` gauge, so a leftover nonzero
    /// makes the gauge assert a live screen encoder for a share that has stopped —
    /// the exact stale-value class as #2145. It cannot rely on the AQ loop's
    /// `SCREEN_ENCODER_FPS_IDLE_DECAY_MS` backstop, because that loop exits when its
    /// liveness token drops (Host unmount) while the health reporter keeps an `Arc`
    /// clone of the atom and keeps reporting.
    ///
    /// #2147: the ERROR / give-up / final-cleanup teardown paths must zero the
    /// output-fps atom too, not just the `onended` path.
    ///
    /// The last of these is the `stream_ended` route — taken when a capture track
    /// dies WITHOUT `onended` firing (OS/source revoke, monitor unplug, Wayland
    /// portal revoke). All three route through `clear_screen_sharing_state`, so this
    /// pins that chokepoint. Without it the ungated
    /// `videocall_screen_encoder_output_fps` gauge keeps asserting a live screen
    /// encoder for a share that has ended.
    ///
    /// MUTATION: change `clear_screen_sharing_state` back to calling only
    /// `clear_screen_sharing_flags` (drop the `reset_output_fps`) and the fps
    /// assertion fails (stays 17).
    #[test]
    fn clear_screen_sharing_state_zeroes_output_fps_and_both_flags() {
        use super::clear_screen_sharing_state;
        use std::sync::atomic::AtomicU32;
        use std::sync::Arc;

        let rc = Rc::new(AtomicBool::new(true));
        let arc = Arc::new(AtomicBool::new(true));
        let current_fps = AtomicU32::new(17);

        clear_screen_sharing_state(&rc, &arc, &current_fps);

        assert_eq!(
            current_fps.load(Ordering::Relaxed),
            0,
            "#2147: an error/revoke teardown must report an honest 0, not a stale nonzero"
        );
        // The pre-existing #1611 dual-flag behaviour must be preserved.
        assert!(
            !rc.load(Ordering::Acquire),
            "the Rc sharing flag must clear"
        );
        assert!(
            !arc.load(Ordering::Acquire),
            "the Arc sharing flag must clear (mic-side #1611)"
        );
    }

    /// MUTATION: delete the `reset_output_fps(current_fps)` line from
    /// `apply_screen_share_stopped` and the fps assertion fails (stays 24).
    #[test]
    fn screen_share_stopped_zeroes_output_fps_and_flags() {
        use super::apply_screen_share_stopped;
        use std::sync::atomic::AtomicU32;

        let enabled = AtomicBool::new(true);
        let sharing = AtomicBool::new(true);
        let current_fps = AtomicU32::new(24);

        apply_screen_share_stopped(&enabled, &sharing, &current_fps);

        assert_eq!(
            current_fps.load(Ordering::Relaxed),
            0,
            "#2147: a stopped share must report an honest 0, never a stale nonzero"
        );
        // The pre-existing flag behaviour must be preserved by the extraction.
        assert!(
            !enabled.load(Ordering::Acquire),
            "the enabled flag must be cleared"
        );
        assert!(
            !sharing.load(Ordering::Acquire),
            "the screen-sharing flag must be cleared"
        );
    }

    /// #2060: `stop()` must reset the shared `current_fps` atomic to 0 so a
    /// re-enable re-warms honestly (no stale-nonzero republish downstream). This
    /// pins the stop()->`reset_output_fps` CALL-SITE — the pure helper itself is
    /// unit-tested in `encode/mod.rs`; this guards that `stop()` actually calls
    /// it. MUTATION: deleting `reset_output_fps(&self.current_fps)` from `stop()`
    /// leaves `current_fps` at 30 and fails this assertion.
    #[test]
    fn stop_resets_current_fps_to_zero() {
        let client = build_test_client();
        let mut encoder = ScreenEncoder::new(
            client,
            500,
            Callback::from(|_: String| {}),
            Callback::from(|_: ScreenShareEvent| {}),
            Rc::new(AtomicBool::new(false)),
            1, // max_layers (single layer)
        );
        // Simulate a live encoder that has produced layer-0 output.
        encoder.current_fps.store(30, Ordering::Relaxed);
        assert_eq!(encoder.get_current_fps(), 30);
        encoder.stop();
        assert_eq!(
            encoder.get_current_fps(),
            0,
            "#2060: stop() must reset current_fps to 0"
        );
    }

    #[test]
    fn screen_encoder_uses_shared_reelection_signal() {
        let client = build_test_client();
        let mut encoder = ScreenEncoder::new(
            client,
            500,
            Callback::from(|_: String| {}),
            Callback::from(|_: ScreenShareEvent| {}),
            Rc::new(AtomicBool::new(false)),
            1, // max_layers (single layer)
        );
        let shared_signal = Rc::new(AtomicBool::new(false));
        encoder.set_reelection_completed_signal(shared_signal.clone());

        shared_signal.store(true, Ordering::Release);
        assert!(
            encoder
                .reelection_completed_signal
                .swap(false, Ordering::AcqRel),
            "screen encoder should read the externally owned re-election signal"
        );
    }

    /// Issue #2179: every tier apply must ARM `initial_tier_pending`, which is
    /// what makes the AQ control loop adopt the encoder's source-resolved tier
    /// instead of overwriting it with its own construction default on the first
    /// transition.
    ///
    /// Mutation guard: deleting the `initial_tier_pending.store(true, ...)` in
    /// `apply_initial_tier_to` leaves the flag `false` and this fails. The flag
    /// starts `false` at construction (asserted first), so the test cannot pass
    /// on a stuck-`true` default either.
    #[test]
    fn apply_initial_tier_arms_the_aq_controller_seed() {
        let client = build_test_client();
        let mut encoder = ScreenEncoder::new(
            client,
            500,
            Callback::from(|_: String| {}),
            Callback::from(|_: ScreenShareEvent| {}),
            Rc::new(AtomicBool::new(false)),
            1, // max_layers (single layer)
        );
        assert!(
            !encoder.initial_tier_pending.load(Ordering::Acquire),
            "no seed may be pending before a share starts"
        );

        encoder.apply_initial_tier();
        assert!(
            encoder.initial_tier_pending.load(Ordering::Acquire),
            "applying the initial tier must arm the AQ controller seed"
        );
        assert_eq!(
            encoder.shared_screen_tier_index.load(Ordering::Relaxed),
            crate::adaptive_quality_constants::DEFAULT_SCREEN_TIER_INDEX as u32,
            "the armed seed must be readable from the shared tier index"
        );

        // The AQ loop's consume is a `swap(false)`: exactly once per apply.
        assert!(encoder.initial_tier_pending.swap(false, Ordering::AcqRel));
        assert!(!encoder.initial_tier_pending.load(Ordering::Acquire));
    }

    /// Drive a real governor to its floor with unbroken uplink pressure.
    fn governor_at_floor(baseline: ScreenBaselineKbps) -> ScreenUplinkGovernor {
        let mut g = ScreenUplinkGovernor::new();
        let mut t = 0.0;
        while g.step().raw() < SCREEN_BACKOFF_MAX_STEP {
            assert!(t < 60_000.0, "governor never reached its floor");
            g.observe(
                t,
                ScreenUplinkSample::Buffered {
                    bytes: 400_000,
                    gate_drops: 0,
                },
                baseline,
            );
            t += 150.0;
        }
        g
    }

    #[test]
    fn the_mic_facing_screen_floor_signal_tracks_the_uplink_governor() {
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        let flag = std::sync::Arc::new(AtomicBool::new(true));

        seed_screen_floor_signal(&flag);
        assert!(!flag.load(Ordering::Acquire), "share start seeds shut");

        let mut g = ScreenUplinkGovernor::new();
        publish_screen_floor_signal(&flag, &g, baseline);
        assert!(
            !flag.load(Ordering::Acquire),
            "a healthy share is not at floor"
        );

        g = governor_at_floor(baseline);
        publish_screen_floor_signal(&flag, &g, baseline);
        assert!(
            flag.load(Ordering::Acquire),
            "a bottomed-out share is at floor"
        );

        // Past `governor_at_floor`'s 60s bound, so relief ticks read forwards.
        let mut t = 60_000.0;
        while g.step().raw() > 0 {
            g.observe(
                t,
                ScreenUplinkSample::Buffered {
                    bytes: 0,
                    gate_drops: 0,
                },
                baseline,
            );
            t += 150.0;
        }
        publish_screen_floor_signal(&flag, &g, baseline);
        assert!(
            !flag.load(Ordering::Acquire),
            "recovery must retract the signal, not latch it"
        );
    }

    #[test]
    fn the_mic_facing_signal_holds_through_a_probe_under_sustained_congestion() {
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let mut g = governor_at_floor(baseline);

        let mut t = 60_000.0;
        let deadline = t + 4.0 * SCREEN_BACKOFF_PROBE_INTERVAL_MS as f64;
        while t <= deadline {
            g.observe(
                t,
                ScreenUplinkSample::Buffered {
                    bytes: 400_000,
                    gate_drops: 0,
                },
                baseline,
            );
            publish_screen_floor_signal(&flag, &g, baseline);
            assert!(
                flag.load(Ordering::Acquire),
                "retracted at +{}ms on step {}",
                t - 60_000.0,
                g.step().raw()
            );
            t += 150.0;
        }
        assert!(g.probes_fired() >= 2, "no probe window crossed");
    }

    /// Double integer truncation costs up to 1ms, so the round-trip lands on
    /// 499 or 500 — asserting `== 500` would fail on correct code.
    #[test]
    fn screen_uplink_pressure_sits_below_the_ws_freshness_gate() {
        for (w, h) in [(2560, 1440), (1920, 1080), (1400, 700), (1280, 720)] {
            let baseline = ScreenBaselineKbps::for_geometry(w, h, 10);
            let threshold = screen_ws_freshness_threshold_bytes(baseline.kbps());
            let round_trip = queued_ms_for(threshold, baseline);
            assert!(
                (SCREEN_WS_FRESHNESS_DELAY_MS - 1..=SCREEN_WS_FRESHNESS_DELAY_MS)
                    .contains(&round_trip),
                "{w}x{h}: gate threshold {threshold}B is {round_trip}ms of video, not ~{SCREEN_WS_FRESHNESS_DELAY_MS}ms"
            );
            assert!(
                SCREEN_UPLINK_PRESSURE_MS < round_trip,
                "{w}x{h}: the governor must act before the gate drops ({SCREEN_UPLINK_PRESSURE_MS} >= {round_trip})"
            );
        }
    }

    #[test]
    fn the_geometry_publisher_cannot_write_the_encoder_target() {
        let w = AtomicU32::new(0);
        let h = AtomicU32::new(0);

        let native = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        let reduced = screen_effective_bitrate_kbps(native, ScreenPressureStep::from_raw(3));
        assert_eq!(reduced.kbps(), 1214);
        let cell = ScreenEffectiveBitrate::seed_from_ceiling(0);
        cell.store(reduced);

        let baseline = publish_screen_encode_geometry(&w, &h, 2560, 1440, 10);
        assert_eq!(baseline.kbps(), 4423);
        assert_eq!(w.load(Ordering::Relaxed), 2560);
        assert_eq!(h.load(Ordering::Relaxed), 1440);
        assert_eq!(
            cell.kbps(),
            1214,
            "the geometry publisher must not touch the encoder's governed target"
        );

        let baseline = publish_screen_encode_geometry(&w, &h, 1920, 1080, 10);
        assert_eq!(baseline.kbps(), 2488);
        assert_eq!(w.load(Ordering::Relaxed), 1920);
        assert_eq!(
            cell.kbps(),
            1214,
            "a resize must not wipe an active uplink reduction"
        );
    }

    #[test]
    fn the_governor_sample_is_selected_by_transport_not_by_a_missing_depth() {
        assert_eq!(
            screen_uplink_sample(Some("webtransport"), None, 9, 4),
            ScreenUplinkSample::StreamEvents { events: 4 }
        );
        assert_eq!(
            screen_uplink_sample(Some("websocket"), Some(70_000), 9, 4),
            ScreenUplinkSample::Buffered {
                bytes: 70_000,
                gate_drops: 9
            }
        );
        for unreportable in [
            screen_uplink_sample(Some("websocket"), None, 9, 4),
            screen_uplink_sample(None, None, 9, 4),
            screen_uplink_sample(None, Some(70_000), 9, 4),
        ] {
            assert_eq!(
                unreportable,
                ScreenUplinkSample::Unobservable,
                "a depth this client cannot read is not a quiet WebTransport stream"
            );
        }
    }

    #[test]
    fn the_ws_gate_threshold_tracks_geometry_not_the_governed_target() {
        let native = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        assert_eq!(screen_ws_gate_threshold_bytes(native), 276_437);

        let governor = governor_at_floor(native);
        assert_eq!(governor.target_for(native).kbps(), 512);
        assert_eq!(
            screen_ws_gate_threshold_bytes(native),
            276_437,
            "the gate must not move with the actuator"
        );

        let w = AtomicU32::new(2560);
        let h = AtomicU32::new(1440);
        let tier = AtomicU32::new(0);
        assert_eq!(screen_baseline_from_published(&w, &h, &tier), native);

        w.store(0, Ordering::Relaxed);
        h.store(0, Ordering::Relaxed);
        assert_eq!(
            screen_baseline_from_published(&w, &h, &tier).kbps(),
            SCREEN_MIN_BITRATE_KBPS,
            "an unpublished geometry must floor, not divide by zero"
        );
    }

    #[test]
    fn a_governed_target_reaches_both_the_encoder_cell_and_the_telemetry_mirror() {
        let cell = ScreenEffectiveBitrate::seed_from_ceiling(0);
        let telemetry = AtomicU32::new(4423);

        let governed = screen_effective_bitrate_kbps(
            ScreenBaselineKbps::for_geometry(2560, 1440, 10),
            ScreenPressureStep::from_raw(3),
        );
        publish_screen_effective_bitrate(&cell, &telemetry, governed);

        assert_eq!(governed.kbps(), 1214);
        assert_eq!(cell.kbps(), 1214);
        assert_eq!(telemetry.load(Ordering::Relaxed), 1214);
    }

    /// Issue #2179 review r3: the freeze invariant is WIRING-enforced, not just
    /// documented. [`SourceDims::refresh_live`] — the only writer reachable from
    /// the `applyConstraints` success path — must never move the frozen pair.
    ///
    /// The earlier pin passed literals to
    /// [`screen_track_constraint_for_tier`], which proves the arithmetic but NOT
    /// the wiring: refreshing the frozen pair inside the encode loop would have
    /// left it green. This one fails.
    ///
    /// Mutation guard: add `self.frozen_w.store(w, ..)` /
    /// `self.frozen_h.store(h, ..)` to `refresh_live` and the divergence
    /// assertion fails — and with it the step-up RELEASE, which is what pins a
    /// share soft forever after one congestion episode.
    #[test]
    fn source_dims_refresh_live_never_moves_the_frozen_pair() {
        let dims = SourceDims::new();

        // Acquisition seeds BOTH: at that instant they describe one surface.
        dims.seed_on_acquisition(3840, 2160);
        assert_eq!(dims.frozen(), (3840, 2160));
        assert_eq!(dims.live(), (3840, 2160));

        // A step-down constraint is applied and the capture really shrinks.
        dims.refresh_live(1280, 720);
        assert_eq!(
            dims.live(),
            (1280, 720),
            "the wire stamp must follow the LIVE capture"
        );
        assert_eq!(
            dims.frozen(),
            (3840, 2160),
            "the constraint decision must still see the ORIGINAL surface"
        );

        // …and that divergence is exactly what keeps the step-up RELEASE alive.
        // With the frozen pair (3840x2160) the release is made; with the live
        // pair it would be suppressed and the share would stay soft forever.
        let (cw, ch) = (3840u32, 2160u32);
        assert_eq!(
            screen_track_constraint_for_tier(
                cw,
                ch,
                cw,
                ch,
                dims.frozen().0,
                dims.frozen().1,
                1280,
                720
            ),
            Some((cw, ch)),
            "release must fire against the frozen pair"
        );
        assert_eq!(
            screen_track_constraint_for_tier(
                cw,
                ch,
                cw,
                ch,
                dims.live().0,
                dims.live().1,
                1280,
                720
            ),
            None,
            "…and would be suppressed against the live pair — the wedge this \
             invariant exists to prevent"
        );

        // A re-acquire re-seeds BOTH, so a new surface is not judged against the
        // dead one.
        dims.seed_on_acquisition(1920, 1080);
        assert_eq!(dims.frozen(), (1920, 1080));
        assert_eq!(dims.live(), (1920, 1080));
    }

    #[test]
    fn sustained_shed_teardown_decision_fires_only_past_dwell() {
        // Issue #1230: pins the screen SINGLE SOURCE OF TRUTH for teardown
        // (`should_teardown_shed_layer`). The encode loop frees a shed extra rung
        // iff this returns true, so pinning it here pins the behavior off-wasm (the
        // counter is bumped in the live loop and is not host-runnable).
        //
        // Mutations these assertions CATCH:
        //  * dropping the `None` guard (a non-shed rung would tear down) — first case
        //  * inverting the comparison (`>=`→`<`) — every Some case flips and FAILS
        //  * swapping `>=`→`>` — the exact-boundary case flips and FAILS
        let dwell = SHED_TEARDOWN_DWELL_MS; // 30_000.0

        assert!(
            !should_teardown_shed_layer(None, 100_000.0, dwell),
            "a rung that is not shed (None) must never be torn down"
        );
        assert!(
            !should_teardown_shed_layer(Some(0.0), 29_999.0, dwell),
            "29.999s < 30s dwell must retain the rung"
        );
        assert!(
            should_teardown_shed_layer(Some(0.0), 30_000.0, dwell),
            "exactly 30s dwell must tear down (>= is inclusive)"
        );
        assert!(
            should_teardown_shed_layer(Some(10_000.0), 45_000.0, dwell),
            "35s dwell must tear down"
        );
        assert!(
            !should_teardown_shed_layer(Some(10_000.0), 20_000.0, dwell),
            "10s dwell must retain"
        );

        // FREED-COUNT SEMANTICS via the REAL decision path (NOT X==X). Screen's
        // `extra_layers` are rungs 1.. (base id 0 is the standalone encoder, never
        // in this array). Index the per-rung shed-since array by id; id 0 (base) is
        // never shed. now = 40_000ms:
        //   id1: armed t=0    (40s dwell >= 30s) → tear down
        //   id2: armed t=20s  (20s dwell <  30s) → retain
        let now_ms = 40_000.0;
        let shed_since: [Option<f64>; 3] = [None, Some(0.0), Some(20_000.0)];
        let freed = shed_since
            .iter()
            .filter(|s| should_teardown_shed_layer(**s, now_ms, dwell))
            .count();
        assert_eq!(
            freed, 1,
            "exactly the extra rungs whose dwell exceeded the threshold are freed"
        );
    }

    /// Pairs with the simulcast test above: in SINGLE-STREAM mode (`max_layers == 1`)
    /// `apply_initial_tier` must NOT force `shared_active_layer_count` — the screen
    /// path stays byte-identical to its pre-#1229 behavior. The construction seed
    /// for `max_layers == 1` is `clamp_screen_layer_count(1) == 1`; the new branch
    /// is gated on `effective_layer_count() > 1`, so it is skipped here and the
    /// value the encode loop reads is whatever was there. We store a SENTINEL (7)
    /// before the call and assert it survives unchanged, which proves the branch did
    /// not execute (a regression that dropped the `effective_layer_count() > 1` guard
    /// would clobber it to 1 and fail).
    #[test]
    fn apply_initial_tier_resets_the_active_layer_count_to_the_single_rung() {
        let client = build_test_client();
        let mut encoder = ScreenEncoder::new(
            client,
            500,
            Callback::from(|_: String| {}),
            Callback::from(|_: ScreenShareEvent| {}),
            Rc::new(AtomicBool::new(false)),
            1, // max_layers (single-stream — the new branch must be skipped)
        );

        // Construction seed for single-stream is the base rung.
        assert_eq!(
            encoder.shared_active_layer_count.load(Ordering::Relaxed),
            1,
            "precondition: single-stream construction seeds the base rung"
        );
        encoder
            .shared_active_layer_count
            .store(7, Ordering::Relaxed);

        encoder.apply_initial_tier();

        assert_eq!(
            encoder.shared_active_layer_count.load(Ordering::Relaxed),
            1,
            "issue 2343: every (re)share resets the active count to the single \
             published layer, whatever a prior session left behind"
        );
    }

    /// Issue #1322 / #1347 item 2: a PLI that lands mid-cooldown must be HELD pending
    /// and honored at window expiry, NOT dropped. This drives the REAL per-frame
    /// decision the screen encode loop calls (`keyframe_tick_decision`) AND replays
    /// the loop's exact atomic interaction with the `force_keyframe` request flag:
    /// PEEK it (`load`), and `store(false)` ONLY when the decision says
    /// `clear_force_keyframe` (i.e. an actual emit). The production loop calls this
    /// same fn, so a mutation to the real decision breaks this test off-wasm (the
    /// live loop is not host-runnable).
    ///
    /// Mutations this catches: reverting the held-PLI fix to `swap(false)` (so the
    /// flag is cleared every tick) is equivalent in the pure fn to making
    /// `clear_force_keyframe` true unconditionally — the `still pending` assertions
    /// below flip and the held PLI is dropped instead of firing at window expiry.
    #[test]
    fn screen_mid_cooldown_pli_is_held_then_fired_not_dropped() {
        use super::ENCODER_PLI_COOLDOWN_MS;

        let force_keyframe = AtomicBool::new(false);
        let cd = ENCODER_PLI_COOLDOWN_MS; // 2000.0
        let mut last_keyframe_emit_ms: Option<f64> = None;

        // One encode-loop tick, byte-for-byte the loop's atomic interaction around the
        // shared decision: PEEK the request (`load`), call the REAL
        // `keyframe_tick_decision`, write back the cooldown clock, and `store(false)`
        // ONLY when the decision says to clear. No reconnect in this slice.
        // Returns whether a keyframe is emitted this tick.
        let mut tick = |now: f64, is_periodic: bool| -> bool {
            let decision = keyframe_tick_decision(KeyframeTickInput {
                now_ms: now,
                pli_pending: force_keyframe.load(Ordering::Acquire),
                is_periodic,
                cooldown_reset: false,
                last_keyframe_emit_ms,
                cooldown_ms: cd,
                tier_change_pending: false,
            });
            last_keyframe_emit_ms = decision.last_keyframe_emit_ms;
            if decision.clear_force_keyframe {
                force_keyframe.store(false, Ordering::Release);
            }
            decision.want_keyframe
        };

        // t=0: a periodic keyframe emits and starts the cooldown window.
        assert!(tick(0.0, true), "periodic keyframe at t=0 must emit");

        // t=500: a PLI arrives well within the 2000ms cooldown.
        force_keyframe.store(true, Ordering::Release);
        assert!(
            !tick(500.0, false),
            "a PLI 500ms into a 2000ms cooldown must NOT force a keyframe yet"
        );
        // #1322 core guard: the request must remain PENDING, not be cleared/dropped.
        assert!(
            force_keyframe.load(Ordering::Acquire),
            "a mid-cooldown PLI must stay pending (held), not be dropped"
        );

        // t=1500: still inside the window — still held, still pending.
        assert!(!tick(1500.0, false), "still within the cooldown window");
        assert!(
            force_keyframe.load(Ordering::Acquire),
            "the PLI must still be pending deeper into the window"
        );

        // t=2000: the window expires (>= cooldown) → the held PLI fires immediately.
        assert!(
            tick(2000.0, false),
            "a held PLI must fire the instant the cooldown window expires"
        );
        // The emit clears the flag so it does not re-fire next tick.
        assert!(
            !force_keyframe.load(Ordering::Acquire),
            "emitting the keyframe must clear the request flag"
        );
    }

    /// Issue #1312 parity / #1347 item 2: under a saturated PLI burst (every frame
    /// requests a keyframe, the N-receivers-hammering-one-publisher worst case) the
    /// screen coalescer must collapse the burst to at most one forced keyframe per
    /// ENCODER_PLI_COOLDOWN_MS window — not one per frame. Drives the REAL
    /// `keyframe_tick_decision` (the fn the production loop calls) with the real
    /// clear-on-emit state update (no periodic keyframes in this slice). Removing the
    /// cooldown gate from the decision makes every frame force a keyframe, failing the
    /// `== 3` assertion.
    ///
    /// A 300ms inter-frame spacing is used deliberately: 2000ms is NOT an integer
    /// multiple of it (2000/300 ≈ 6.67), so every window boundary falls strictly
    /// between two frames, keeping the count robust to float rounding (the boundary
    /// is pinned separately and exactly by `pli_keyframe_allowed_pins_cooldown_boundary`).
    #[test]
    fn screen_saturated_pli_burst_coalesces_to_one_per_window() {
        use super::ENCODER_PLI_COOLDOWN_MS;

        let cd = ENCODER_PLI_COOLDOWN_MS; // 2000.0
        let frame_interval_ms = 300.0;
        let mut last_keyframe_emit_ms: Option<f64> = None;
        let mut forced = 0u32;
        let mut now = 0.0_f64;
        // ~6s of saturated PLI: a PLI is pending every frame; no periodic GOP in this
        // slice, so every emit is PLI-forced. Emissions land at the first frame at/after
        // each window: t=0 (None guard), t=2100 (frame 7), t=4200 (frame 14) ⇒ 3.
        for _ in 0..20 {
            let decision = keyframe_tick_decision(KeyframeTickInput {
                now_ms: now,
                pli_pending: true,
                is_periodic: false,
                cooldown_reset: false,
                last_keyframe_emit_ms,
                cooldown_ms: cd,
                tier_change_pending: false,
            });
            if decision.want_keyframe {
                assert_eq!(
                    decision.forced_cause,
                    Some(ForcedKeyframeCause::Pli),
                    "with no periodic GOP, every emit in this slice is PLI-forced"
                );
                forced += 1;
            }
            last_keyframe_emit_ms = decision.last_keyframe_emit_ms;
            now += frame_interval_ms;
        }
        assert_eq!(
            forced, 3,
            "a saturated PLI burst at a 2000ms cooldown must coalesce to 3 forced keyframes, \
             not one per frame"
        );
    }

    // -----------------------------------------------------------------------
    // WebTransport backpressure wiring (#509 parity audit, item #2).
    //
    // Mirror of the camera-encoder pins: the screen AQ loop is wasm-only, so the
    // per-axis decision (counter → `evaluate_self_congestion` → WT constants) is
    // extracted into `wt_drop_step_down_decision` /
    // `wt_saturation_step_down_decision`, which the loop calls with the live WT
    // counters. Screen is frequently the heaviest egress, so its WT self-shed
    // matters at least as much as the camera's. A mutation pointing an axis at
    // the wrong constants is caught here; the transport-side increment is pinned
    // by the `videocall-transport` `record_*` tests.
    // -----------------------------------------------------------------------

    #[test]
    fn screen_wt_drop_axis_fires_on_sustained_drops() {
        let decision = wt_drop_step_down_decision(
            WT_SELF_CONGESTION_DROP_THRESHOLD,
            0,
            WT_SELF_CONGESTION_WINDOW_MS,
        );
        assert!(
            decision.step_down,
            "a WT-drop delta == WT threshold over a closed WT window must step down"
        );
    }

    #[test]
    fn screen_wt_drop_axis_does_not_fire_below_threshold() {
        let decision = wt_drop_step_down_decision(
            WT_SELF_CONGESTION_DROP_THRESHOLD - 1,
            0,
            WT_SELF_CONGESTION_WINDOW_MS,
        );
        assert!(
            !decision.step_down,
            "a WT-drop delta below the WT threshold must NOT step down"
        );
    }

    /// Anti-misweave pin: the drop axis must use the WT window, not the WS
    /// window. At an elapsed past the (narrower) WS window but before the WT
    /// window closes, the WT axis must still treat the window as OPEN. The
    /// premise (WT window wider than WS) is pinned at COMPILE TIME below so it
    /// is not a runtime `assert!` on constants (clippy `assertions_on_constants`).
    #[test]
    fn screen_wt_drop_axis_uses_wt_window_not_ws() {
        const _: () = assert!(
            WT_SELF_CONGESTION_WINDOW_MS > WS_SELF_CONGESTION_WINDOW_MS,
            "test premise: WT drop window must be wider than WS window"
        );
        let elapsed = WS_SELF_CONGESTION_WINDOW_MS + 1.0;
        let decision = wt_drop_step_down_decision(WT_SELF_CONGESTION_DROP_THRESHOLD, 0, elapsed);
        assert!(
            !decision.step_down,
            "WT-drop axis must treat the WT window as open at WS-window elapsed (proves WT \
             constants, not WS)"
        );
        assert!(!decision.roll_window, "an open WT window must not roll");
    }

    #[test]
    fn screen_wt_saturation_axis_fires_on_sustained_stalls() {
        let decision = wt_saturation_step_down_decision(
            WT_SATURATION_STALL_THRESHOLD,
            0,
            WT_SATURATION_WINDOW_MS,
        );
        assert!(
            decision.step_down,
            "a saturation delta == saturation threshold over a closed window must step down"
        );
    }

    #[test]
    fn screen_wt_saturation_axis_never_fires_when_flat() {
        let decision = wt_saturation_step_down_decision(0, 0, WT_SATURATION_WINDOW_MS);
        assert!(
            !decision.step_down,
            "a flat-at-0 saturation counter must never step down (WS users / healthy WT)"
        );
    }

    // ── Issue #1921: WS freshness-gate self-congestion axis (#5) ─────────────
    // The screen AQ loop is wasm-only, so axis #5's decision (the freshness-drop
    // counter → `evaluate_self_congestion` → screen freshness constants) is
    // extracted into `screen_ws_stale_drop_step_down_decision`, pinned here.
    // These drive the REAL helper the loop calls (not a copy), so removing the
    // axis or zeroing its threshold breaks an assertion.

    /// A SUSTAINED cluster (delta ≥ threshold over a closed window) steps down.
    /// Zeroing `SCREEN_WS_STALE_DROP_THRESHOLD` would make delta 0 also fire, but
    /// this exact-threshold fire is the boundary the axis must keep.
    #[test]
    fn screen_ws_stale_axis_fires_on_sustained_drops() {
        let decision = screen_ws_stale_drop_step_down_decision(
            SCREEN_WS_STALE_DROP_THRESHOLD,
            0,
            SCREEN_WS_STALE_DROP_WINDOW_MS,
        );
        assert!(
            decision.step_down,
            "a freshness-drop delta == threshold over a closed window must step down"
        );
    }

    /// A blip (delta below threshold) must NOT step down — this is the "sustained,
    /// not a spike" guarantee. If the threshold were lowered to the WS axis's 3,
    /// a `THRESHOLD - 1` blip could exceed it; pinning `THRESHOLD - 1` → no-fire
    /// guards that the sustained bar is preserved.
    #[test]
    fn screen_ws_stale_axis_does_not_fire_on_a_blip() {
        let decision = screen_ws_stale_drop_step_down_decision(
            SCREEN_WS_STALE_DROP_THRESHOLD - 1,
            0,
            SCREEN_WS_STALE_DROP_WINDOW_MS,
        );
        assert!(
            !decision.step_down,
            "a freshness-drop delta below threshold (a transient gating blip) must NOT step down"
        );
    }

    /// A flat-at-0 counter (WebTransport publisher, or healthy WS where the gate
    /// never fires) must never step down, so the axis is a true no-op off the
    /// congested-WS path. Mutating the signal to something non-flat would fail.
    #[test]
    fn screen_ws_stale_axis_never_fires_when_flat() {
        let decision =
            screen_ws_stale_drop_step_down_decision(0, 0, SCREEN_WS_STALE_DROP_WINDOW_MS);
        assert!(
            !decision.step_down,
            "a flat-at-0 freshness-drop counter must never step down (WT / healthy WS)"
        );
    }

    /// Anti-misweave + sustained-window pin: the axis must use its OWN 2s window,
    /// not the WS overflow axis's 1s window. At an elapsed past the (narrower) WS
    /// window but before the freshness window closes, the axis must still treat
    /// the window as OPEN (no fire, no roll) even with a threshold-meeting delta.
    /// The premise (freshness window wider than WS) is pinned at COMPILE TIME so
    /// it is not a runtime `assert!` on constants.
    #[test]
    fn screen_ws_stale_axis_uses_its_own_wide_window_not_ws() {
        const _: () = assert!(
            SCREEN_WS_STALE_DROP_WINDOW_MS > WS_SELF_CONGESTION_WINDOW_MS,
            "test premise: freshness-drop window must be wider than the WS overflow window"
        );
        let elapsed = WS_SELF_CONGESTION_WINDOW_MS + 1.0;
        let decision =
            screen_ws_stale_drop_step_down_decision(SCREEN_WS_STALE_DROP_THRESHOLD, 0, elapsed);
        assert!(
            !decision.step_down,
            "freshness axis must treat its window as open at WS-window elapsed (proves the wider \
             sustained window, not the WS window)"
        );
        assert!(
            !decision.roll_window,
            "an open freshness window must not roll"
        );
    }

    /// Issue #1311 (SCREEN half): after a reconnect/re-election the screen encode
    /// loop keeps running (it is NOT torn down — only the connection is rebuilt / the
    /// re-election atomic flips), so `last_keyframe_emit_ms` carries a STALE
    /// pre-transition timestamp. Without a reset, a recovery PLI on the first
    /// post-transition frame would be coalesced away for up to ENCODER_PLI_COOLDOWN_MS
    /// (2000ms — far longer than camera's 250ms, so the screen freeze is worse). The
    /// fix arms a one-shot reset (`keyframe_cooldown_reset`) that the encode loop
    /// `.swap(false)`-consumes each frame and passes into `keyframe_tick_decision` as
    /// `cooldown_reset`, which clears the stale clock so the PLI emits immediately.
    ///
    /// Drives the REAL `keyframe_tick_decision` (the fn the screen production loop
    /// calls) at the SCREEN cooldown value. Mutation-proof: the CONTROL arm pins the
    /// cooldown genuinely WOULD suppress (so the assert is not vacuous), and the RESET
    /// arm fails if the `cooldown_reset` clear is removed from the decision.
    #[test]
    fn screen_keyframe_cooldown_reset_unblocks_first_post_reconnect_pli() {
        use super::ENCODER_PLI_COOLDOWN_MS;

        let cd = ENCODER_PLI_COOLDOWN_MS; // 2000.0

        // A keyframe was emitted just before the transition.
        let pre_reconnect_emit_ms = 10_000.0;
        // The first post-transition frame arrives only 100ms later — deep INSIDE the
        // 2000ms window, with a PLI pending (a receiver requesting recovery).
        let first_frame_after_ms = pre_reconnect_emit_ms + 100.0;

        // CONTROL: reset NOT armed. The stale timestamp must SUPPRESS the PLI.
        let control = keyframe_tick_decision(KeyframeTickInput {
            now_ms: first_frame_after_ms,
            pli_pending: true,
            is_periodic: false,
            cooldown_reset: false,
            last_keyframe_emit_ms: Some(pre_reconnect_emit_ms),
            cooldown_ms: cd,
            tier_change_pending: false,
        });
        assert!(
            !control.want_keyframe,
            "control: a screen PLI {}ms after the last keyframe must be coalesced when no \
             reconnect reset is armed",
            first_frame_after_ms - pre_reconnect_emit_ms
        );

        // RESET ARM: a reconnect/re-election armed the reset (the loop
        // `.swap(false)`-consumed it → `cooldown_reset: true`). The SAME PLI on the
        // SAME early frame now EMITS. Removing the `cooldown_reset` clear from the
        // decision makes `want_keyframe` false and FAILS.
        let reset = keyframe_tick_decision(KeyframeTickInput {
            now_ms: first_frame_after_ms,
            pli_pending: true,
            is_periodic: false,
            cooldown_reset: true,
            last_keyframe_emit_ms: Some(pre_reconnect_emit_ms),
            cooldown_ms: cd,
            tier_change_pending: false,
        });
        assert!(
            reset.want_keyframe,
            "after a reconnect/re-election reset, the first screen PLI must emit a forced \
             keyframe immediately even {}ms < cooldown ({}ms) since the last keyframe",
            first_frame_after_ms - pre_reconnect_emit_ms,
            cd
        );
        assert_eq!(
            reset.forced_cause,
            Some(ForcedKeyframeCause::Pli),
            "the un-gated screen emit is PLI-forced"
        );

        // One-shot: the reset is a per-frame edge (the loop already consumed it via
        // `.swap`), so a SUBSEQUENT early frame — still inside the cooldown of the
        // keyframe we just emitted, reset NOT re-armed — is coalesced again.
        let next = keyframe_tick_decision(KeyframeTickInput {
            now_ms: first_frame_after_ms + 100.0,
            pli_pending: true,
            is_periodic: false,
            cooldown_reset: false,
            last_keyframe_emit_ms: reset.last_keyframe_emit_ms,
            cooldown_ms: cd,
            tier_change_pending: false,
        });
        assert!(
            !next.want_keyframe,
            "after the one-shot reset is consumed, the screen coalescer resumes \
             suppressing PLIs inside the cooldown window"
        );
    }

    /// Issue #1611 regression test — `clear_screen_sharing_flags` must store
    /// BOTH flags (Rc + Arc). Without the Arc store, the mic's audio-after-video
    /// detector sees a stale-true screen-active signal on failure teardown.
    ///
    /// This test invokes the PRODUCTION helper (`clear_screen_sharing_flags`)
    /// used by `stop()`, `cleanup_on_error`, and final cleanup — NOT hand-written
    /// stores (which would be tautological per CLAUDE.md adversarial check 2).
    /// Removing the `arc.store` line from the helper **provably breaks this test**.
    #[test]
    fn screen_sharing_active_arc_mirrors_rc_on_all_paths() {
        use std::sync::Arc;

        let rc_flag = Rc::new(AtomicBool::new(false));
        let arc_flag = Arc::new(AtomicBool::new(false));

        // Simulate teardown paths (stop / cleanup_on_error / final cleanup):
        // set both flags true (share was active), then invoke the production
        // helper that MUST clear both atomically.
        rc_flag.store(true, Ordering::Release);
        arc_flag.store(true, Ordering::Release);

        // Invoke the PRODUCTION helper (same one stop/cleanup_on_error/final use)
        clear_screen_sharing_flags(&rc_flag, &arc_flag);

        // Both flags MUST now be false. If the helper's `arc.store(false)` is
        // removed (or never added), this assertion FAILS — proving the test guards
        // the production code, not its own inline copy.
        assert!(
            !rc_flag.load(Ordering::Acquire),
            "Rc must be false after clear_screen_sharing_flags"
        );
        assert!(
            !arc_flag.load(Ordering::Acquire),
            "Arc MUST be false after clear_screen_sharing_flags — stale-true would \
             wedge the audio-after-video detector. REGRESSION: if this fails, the \
             helper is missing the arc.store(false) line at screen_encoder.rs:~643"
        );
    }

    // ── discussion 1960 (issue 2): sender-side encoder stall detection ────────────
    // These drive the REAL production types the encode loop uses — `EncoderStallMonitor`,
    // `retained_stale_warn_due`, and (for the forced-keyframe wiring) `keyframe_tick_decision` — not
    // re-implemented copies, so a mutation to the detector, the one-shot latch, the age gate, or the
    // rate limit breaks an assertion here.

    /// The tick-starvation detector fires only on a gap ABOVE the threshold, ignores the cold-start /
    /// first-tick case, and arms a ONE-SHOT fresh-keyframe latch (consumed exactly once).
    ///
    /// Mutation guards: flipping `tick`'s `>` to always-false, or dropping the arm, flips the resume
    /// assertions; making `take_resume_force` sticky (not disarming) flips the one-shot assertion;
    /// making it always-`false` flips the "armed" assertion.
    #[test]
    fn encoder_stall_monitor_detects_gap_and_arms_one_shot() {
        let gap = SCREEN_ENCODER_STALL_GAP_MS;
        let mut mon = EncoderStallMonitor::new();

        // Cold start: no prior tick → never a stall, never armed.
        assert!(
            mon.tick(1_000.0, gap).is_none(),
            "the first tick has no prior tick to measure a gap against"
        );
        assert!(
            !mon.take_resume_force(),
            "a cold-start tick must not arm the fresh-keyframe latch"
        );

        // A normal 150ms poll gap is under the threshold → no stall.
        assert!(
            mon.tick(1_150.0, gap).is_none(),
            "a routine 150ms poll gap is far under the stall threshold"
        );
        assert!(!mon.take_resume_force());

        // An 80s freeze then resume: gap far over threshold → stall detected, latch armed.
        let resumed = mon.tick(1_150.0 + 80_000.0, gap);
        assert!(
            resumed.is_some(),
            "an 80s tick gap must report as a stall resume"
        );
        assert!(
            resumed.unwrap() >= 79_000.0,
            "the reported gap is the full stall duration, not a truncated value"
        );

        // One-shot: the first consumer sees the arm, the second does not.
        assert!(
            mon.take_resume_force(),
            "the resume arms exactly one fresh-keyframe request"
        );
        assert!(
            !mon.take_resume_force(),
            "the fresh-keyframe latch is one-shot — it disarms after one take"
        );
    }

    /// LOAD-BEARING signal-choice test (discussion 1960, issue 2). A legitimately STATIC share delivers
    /// NO real captured frames for minutes (a `getDisplayMedia` track emits only on visual change — see
    /// SCREEN_STATIC_REENCODE_POLL_MS), yet the encode loop keeps TICKING at the 150ms poll cadence.
    /// Driving `EncoderStallMonitor` at that TICK cadence must NEVER read as a stall.
    ///
    /// This pins WHY the signal is the loop tick and not the gap since the last real frame: at the tick
    /// cadence no gap ever crosses the threshold (the loop below), whereas a monitor ticked only on real
    /// frames would see minutes-long gaps and FALSE-POSITIVE on every static share (the contrast
    /// assertion). If a future change re-based the detector on real-frame arrivals, a static share would
    /// begin tripping it and this test's static-cadence loop would start returning `Some`.
    #[test]
    fn encoder_stall_monitor_no_false_positive_on_static_share_tick_cadence() {
        let gap = SCREEN_ENCODER_STALL_GAP_MS;
        let poll = SCREEN_STATIC_REENCODE_POLL_MS as f64;

        let mut mon = EncoderStallMonitor::new();
        let mut t = 1_000.0;
        mon.tick(t, gap); // seed the anchor

        // ~150s of a static share: the timer arm ticks every 150ms with NO real frames arriving.
        for _ in 0..1_000 {
            t += poll;
            assert!(
                mon.tick(t, gap).is_none(),
                "a static-share poll tick (150ms apart) must never read as a stall"
            );
        }
        assert!(
            !mon.take_resume_force(),
            "a static share (frequent ticks, no frames) must never arm the fresh-keyframe latch"
        );

        // CONTRAST: this is exactly what the WRONG signal (gap since the last real captured frame)
        // would produce — on a static share real frames are minutes apart, so a monitor ticked only on
        // real frames sees a huge gap and trips. Proving the tick cadence, not the frame cadence, is the
        // correct signal.
        let mut wrong = EncoderStallMonitor::new();
        wrong.tick(1_000.0, gap);
        assert!(
            wrong.tick(1_000.0 + 120_000.0, gap).is_some(),
            "a 120s gap (real-frame cadence on a static share) DOES trip — which is why the detector is \
             driven by the 150ms loop tick, not by real-frame arrivals"
        );
    }

    /// The retained-frame staleness warn fires only when the frame age exceeds the ceiling AND the
    /// rate-limit window has elapsed. Drives the production `retained_stale_warn_due`.
    ///
    /// Mutation guards: flipping the age gate `<=` (returning false for an over-ceiling age) flips the
    /// "stale ⇒ warn" assertion; dropping the `None ⇒ true` first-warn arm flips the "first warn fires"
    /// assertion; weakening the throttle comparison flips the "within-window suppressed" /
    /// "after-window re-fires" assertions.
    #[test]
    fn retained_stale_warn_due_gates_on_age_and_rate_limit() {
        let stale = SCREEN_RETAINED_STALE_MS;
        let throttle = SCREEN_RETAINED_STALE_LOG_THROTTLE_MS;
        let now = 100_000.0;

        // Fresh-enough retained frame: never warns, regardless of the rate-limit anchor.
        assert!(
            !retained_stale_warn_due(stale - 1.0, stale, now, None, throttle),
            "an age under the staleness ceiling is not the stall symptom — no warn"
        );
        assert!(
            !retained_stale_warn_due(stale, stale, now, None, throttle),
            "an age exactly at the ceiling is not yet stale (strict > boundary)"
        );

        // Stale frame, never warned before: warn.
        assert!(
            retained_stale_warn_due(stale + 1.0, stale, now, None, throttle),
            "a retained frame older than the ceiling, with no prior warn, must warn"
        );

        // Stale frame, but a warn fired within the throttle window: suppressed.
        assert!(
            !retained_stale_warn_due(60_000.0, stale, now, Some(now - (throttle - 1.0)), throttle),
            "a second stale answer inside the rate-limit window is suppressed"
        );

        // Stale frame, throttle window elapsed: warn again (>= boundary inclusive).
        assert!(
            retained_stale_warn_due(60_000.0, stale, now, Some(now - throttle), throttle),
            "once the rate-limit window has elapsed the warn re-fires"
        );
    }

    /// The stall-resume path forces EXACTLY ONE fresh keyframe, ungated by the PLI cooldown, on the
    /// first real frame after the freeze — and not on the next frame. Wires the real production types
    /// together: `EncoderStallMonitor` arms the latch, and the loop folds `take_resume_force()` into
    /// the `keyframe_tick_decision` `is_periodic` input exactly as the encode loop does.
    ///
    /// Mutation guards: if `take_resume_force` never armed / always returned false, frame 1 would not
    /// force a keyframe (first assertion fails); if it were sticky, frame 2 would also force one (last
    /// assertion fails). `last_keyframe_emit_ms` is set so a *PLI* would be cooldown-gated, proving the
    /// resume force bypasses the cooldown via the periodic input, not via a PLI.
    #[test]
    fn stall_resume_forces_exactly_one_fresh_keyframe_via_decision() {
        let gap = SCREEN_ENCODER_STALL_GAP_MS;
        let mut mon = EncoderStallMonitor::new();
        mon.tick(1_000.0, gap);
        assert!(
            mon.tick(1_000.0 + 90_000.0, gap).is_some(),
            "a 90s tick gap is a stall resume that arms the latch"
        );

        // Frame 1 after resume: no periodic boundary, no PLI, and a keyframe went out 500ms ago (so a
        // PLI would be cooldown-gated). The resume latch alone must still force a keyframe. `is_periodic`
        // here models the production `periodic_keyframe_due(..) || stall_resume_keyframe` with the
        // periodic term false.
        let resume1 = mon.take_resume_force();
        assert!(
            resume1,
            "the first real frame after a resume consumes the armed latch"
        );
        let d1 = keyframe_tick_decision(KeyframeTickInput {
            now_ms: 91_500.0,
            pli_pending: false,
            is_periodic: resume1,
            cooldown_reset: false,
            last_keyframe_emit_ms: Some(91_000.0),
            cooldown_ms: ENCODER_PLI_COOLDOWN_MS,
            tier_change_pending: false,
        });
        assert!(
            d1.want_keyframe,
            "the stall-resume latch forces a fresh keyframe ungated by the PLI cooldown"
        );

        // Frame 2: the one-shot latch is spent → no periodic, no PLI ⇒ no keyframe.
        let resume2 = mon.take_resume_force();
        assert!(
            !resume2,
            "the resume latch is one-shot — the second frame does not re-force"
        );
        let d2 = keyframe_tick_decision(KeyframeTickInput {
            now_ms: 91_600.0,
            pli_pending: false,
            is_periodic: resume2,
            cooldown_reset: false,
            last_keyframe_emit_ms: Some(91_500.0),
            cooldown_ms: ENCODER_PLI_COOLDOWN_MS,
            tier_change_pending: false,
        });
        assert!(
            !d2.want_keyframe,
            "with the one-shot latch spent, a non-periodic no-PLI frame emits no keyframe"
        );
    }
}

#[cfg(test)]
mod wasm_tests {
    use super::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    fn built_screen_layer(width: u32, height: u32, bitrate_bps: u32) -> LayerEncoder {
        let error_closure = Closure::wrap(Box::new(|_e: JsValue| {}) as Box<dyn FnMut(JsValue)>);
        let output_closure = Closure::wrap(Box::new(|_c: JsValue| {}) as Box<dyn FnMut(JsValue)>);
        let init = VideoEncoderInit::new(
            error_closure.as_ref().unchecked_ref(),
            output_closure.as_ref().unchecked_ref(),
        );
        let encoder = Box::new(VideoEncoder::new(&init).expect("create VideoEncoder"));
        let config = VideoEncoderConfig::new(get_video_codec_string(), height, width);
        config.set_bitrate(bitrate_bps as f64);
        config.set_latency_mode(LatencyMode::Realtime);
        set_vbr_mode(&config);
        set_framerate_hint(&config, 10);
        encoder.configure(&config).expect("cold-start configure");
        LayerEncoder {
            encoder,
            config,
            seq_out: Rc::new(Cell::new(0)),
            layer_id: 1,
            current_w: width,
            current_h: height,
            tier_w: width,
            tier_h: height,
            target_fps: 10,
            local_bitrate: bitrate_bps,
            _output_closure: output_closure,
            _error_closure: error_closure,
        }
    }

    #[wasm_bindgen_test]
    fn a_rejected_rung_bitrate_reconfigure_leaves_the_cache_at_the_applied_rate_2550() {
        let mut layer = built_screen_layer(640, 360, 700_000);

        assert!(
            Reflect::set(
                &layer.config,
                &JsValue::from_str("width"),
                &JsValue::from_f64(0.0),
            )
            .expect("set width on the stored config"),
            "Reflect::set refused the write, so the fixture proves nothing",
        );
        let err = layer
            .reconfigure_at_bitrate(400_000)
            .expect_err("a zero-width stored config must be rejected by configure()");
        assert!(
            !is_fatal_encoder_error(&err),
            "fixture must exercise the NON-fatal branch the encode loop continues past: {err:?}",
        );

        assert_eq!(
            layer.local_bitrate, 700_000,
            "the cache claimed a rate configure() rejected (#2550)",
        );
    }

    #[wasm_bindgen_test]
    fn an_accepted_rung_bitrate_reconfigure_advances_the_cache() {
        let mut layer = built_screen_layer(640, 360, 700_000);

        layer
            .reconfigure_at_bitrate(400_000)
            .expect("rung bitrate reconfigure");

        assert_eq!(layer.local_bitrate, 400_000);
        assert_eq!(
            Reflect::get(&layer.config, &JsValue::from_str("bitrate"))
                .ok()
                .and_then(|v| v.as_f64()),
            Some(400_000.0),
        );
    }
}

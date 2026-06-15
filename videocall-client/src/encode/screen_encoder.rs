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
use super::encoder_state::{keyframe_tick_decision, EncoderState, KeyframeTickInput};
use super::transform::transform_screen_chunk;
use crate::crypto::aes::Aes128State;

use crate::adaptive_quality_constants::{
    simulcast_screen_layers, BITRATE_CHANGE_THRESHOLD, DEFAULT_SCREEN_TIER_INDEX,
    ENCODER_PLI_COOLDOWN_MS, SCREEN_QUALITY_TIERS,
};
use crate::constants::get_video_codec_string;
// Reuse the SEND-side simulcast diagnostics types defined alongside the camera
// encoder (issue #1095 observability) so screen + camera share one shape.
use crate::diagnostics::adaptive_quality_manager::TierTransitionRecord;
use crate::diagnostics::EncoderBitrateController;
use crate::encode::camera_encoder::{build_simulcast_layers, SimulcastSendSnapshot};
use videocall_aq::{fit_within_preserving_aspect, simulcast_layer_target_dims};

/// Upper bound on SCREEN simulcast layers regardless of what the caller
/// requests (issue #989, Phase 3b). Matches the 3-tier screen ladder the AQ
/// crate defines (`simulcast_screen_layers`). The caller passes 1 by default
/// (feature off → single layer, byte-identical to the pre-simulcast path).
const SCREEN_SIMULCAST_MAX_SUPPORTED_LAYERS: u32 = 3;

/// Clamp a requested screen `max_layers` to the supported range. `0` (meaningless
/// — there is always the base layer) becomes 1. Free function so it is
/// unit-testable without a live `ScreenEncoder`.
fn clamp_screen_layer_count(max_layers: u32) -> u32 {
    max_layers.clamp(1, SCREEN_SIMULCAST_MAX_SUPPORTED_LAYERS)
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
    /// Cached bitrate (bps) last applied to this layer's encoder.
    local_bitrate: u32,
    /// Kept alive so the JS output callback stays valid.
    _output_closure: Closure<dyn FnMut(JsValue)>,
    /// Kept alive so the JS error callback stays valid.
    _error_closure: Closure<dyn FnMut(JsValue)>,
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
/// Cumulative count of upper-rung simulcast `VideoEncoder`s torn down after a
/// sustained shed dwell (issue #1230). Pure observability hook, mirrors the
/// camera getter: confirms memory is reclaimed on sustained-distress devices and
/// that teardown is not thrashing.
pub fn screen_encoder_layers_torn_down() -> u64 {
    SCREEN_ENCODER_LAYERS_TORN_DOWN_AFTER_DWELL.load(Ordering::Relaxed)
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

/// Translate a `TierTransitionRecord::trigger` value into the publisher-side
/// `cause_hint` string carried on `VideoMetadata` (issue #903).
///
/// Trigger taxonomy comes from `videocall-aq` (see `TierTransitionRecord`):
/// `"fps"`, `"bitrate"`, `"congestion"`, `"coordination"`. The receiver
/// renders the hint verbatim, so the mapping is the wire format and must
/// stay in sync with `build_screen_cause_line` in `dioxus-ui/components/
/// signal_quality.rs`. Unknown triggers fall back to `""` (no hint) rather
/// than a guess — proto3 default-empty makes the consumer omit the line.
fn cause_hint_from_trigger(trigger: &str) -> &'static str {
    match trigger {
        "bitrate" => "bitrate-limited",
        "fps" => "cpu-pressure",
        "congestion" => "network-rtt",
        "coordination" => "manual-cap",
        _ => "",
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

/// User-configurable adaptive-quality tier bounds for SCREEN SHARE (issue #961
/// follow-up), shared from the UI into the running screen encoder control loop.
///
/// QUALITY IS THE INVERSE OF INDEX over the 3-tier `SCREEN_QUALITY_TIERS` ladder:
/// index 0 = BEST (1080p), index 2 = WORST (low). So `best` is the user's MAX
/// quality = a FLOOR on the index (adaptation never steps UP past it), and
/// `worst` is the user's MIN quality = a CAP on the index (never steps DOWN past
/// it). `None` on either end = "Auto" (no user bound). Screen has no audio, so
/// only video-style bounds exist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScreenQualityTierBounds {
    /// Best/floor screen tier index (user MAX quality). `None` = Auto.
    pub best: Option<usize>,
    /// Worst/cap screen tier index (user MIN quality). `None` = Auto.
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
    /// Current screen tier index (0 = best / 1080p, 2 = worst / low).
    pub tier_index: usize,
    /// Current screen tier max width (px).
    pub width: u32,
    /// Current screen tier max height (px).
    pub height: u32,
    /// Current screen tier target fps.
    pub fps: u32,
    /// Current screen tier ideal bitrate (kbps).
    pub ideal_kbps: u32,
    /// Live encoder target bitrate (kbps) — the real-time needle value.
    pub target_bitrate_kbps: u32,
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
    current_fps: Arc<AtomicU32>,
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
    /// this to drop its quality tier and prevent bandwidth contention.
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
    /// Current screen share quality tier index (0=high, 1=medium, 2=low).
    shared_screen_tier_index: Rc<AtomicU32>,
    /// Tier transition events buffer, drained by health reporter.
    shared_tier_transitions: Rc<RefCell<Vec<TierTransitionRecord>>>,
    /// Issue #903: encoder state stamped on every screen-share `VideoMetadata`
    /// so the receiver can render a `Cause:` line below the Screen row in the
    /// signal-quality tooltip. All three are seeded in `apply_initial_tier`
    /// and updated by the `set_encoder_control` loop whenever AQ acts on the
    /// encoder; the output-chunk closure reads them at frame stamping time.
    /// `0` / empty strings are the proto3 defaults, treated by consumers as
    /// "no data" — so older publishers and the unconstrained-tier path both
    /// suppress the Cause line naturally.
    ///
    /// Latest encoder *target* bitrate (kbps) — what the encoder is currently
    /// trying to produce, sourced from
    /// `EncoderBitrateController::last_target_bitrate_kbps()`.
    shared_screen_encoder_target_bitrate_kbps: Rc<AtomicU32>,
    /// Tier label currently constraining the encoder, e.g. `"high"`,
    /// `"medium"`, `"low"`. Empty when AQ isn't engaged (top tier).
    shared_screen_adaptive_tier: Rc<RefCell<String>>,
    /// Publisher-classified cause hint, e.g. `"bitrate-limited"`,
    /// `"cpu-pressure"`, `"network-rtt"`, `"network-loss"`, `"manual-cap"`.
    /// Empty when AQ is unconstrained or no transition has happened yet.
    shared_screen_cause_hint: Rc<RefCell<String>>,
    /// User-configurable screen-share quality tier bounds (issue #961 follow-up).
    /// Written by the UI via [`Self::set_quality_tier_bounds`], read by the
    /// screen encoder control loop (which applies them live to the
    /// `EncoderBitrateController`). See [`SharedScreenQualityBounds`] for the
    /// apply mechanism and [`ScreenQualityTierBounds`] for the index↔quality
    /// inversion.
    quality_bounds: Rc<RefCell<SharedScreenQualityBounds>>,
    /// Maximum number of SCREEN simulcast layers to emit (issue #989, Phase 3b).
    /// Computed in the UI as `min(experimentalSimulcastMaxLayers, capability
    /// ceiling)`, exactly like the camera. Defaults to 1 (feature off →
    /// single-layer, byte-identical to the pre-simulcast screen path).
    max_layers: u32,
    /// Number of screen layers currently active (encoded + sent), written by the
    /// screen AQ control loop and read by the encode loop. 1 in single-stream
    /// mode (gates nothing). The encode loop encodes only layers with
    /// `layer_id < active_layer_count`, so a shed top layer costs no egress or
    /// sender encode CPU.
    shared_active_layer_count: Rc<AtomicU32>,
    /// Per-layer target bitrate (bps), one atomic per screen ladder layer
    /// (lowest first, index == `layer_id`). Written by the screen AQ control
    /// loop in simulcast mode; read by the encode loop. Empty in single-stream
    /// mode (the legacy `current_bitrate` atomic is used instead).
    shared_layer_bitrates_bps: Rc<RefCell<Vec<Rc<AtomicU32>>>>,
    /// Sender-side screen encoder backpressure (issue #1108, Phase B): the max
    /// `VideoEncoder::encode_queue_size()` across the base `screen_encoder` and
    /// the ACTIVE `extra_layers`, written by the encode loop each frame and read
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
    /// published screen ladder.
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
    /// to cap the published screen ladder as a further `min` alongside the union
    /// hint.
    ///
    /// **Initialized to [`u32::MAX`] = fail-open (Auto / no user cap).** The base
    /// layer is always published (the AQ side floors the cap at 1).
    shared_user_layer_ceiling: Rc<AtomicU32>,
    /// Liveness token bounding the AQ control-loop `spawn_local` future (issue
    /// #1108). The encoder holds the only strong reference; `set_encoder_control`
    /// captures a [`Weak`] and breaks its 1 Hz `tick` loop once `upgrade()`
    /// returns `None` (encoder dropped on Host unmount). The control loop runs on
    /// `wasm_bindgen_futures::spawn_local` (NOT scope-bound), so without this it
    /// would tick forever and leak one loop per remount. Bounds the CONTROL loop
    /// only — the encode loop (`run_screen_encoding`) already exits on
    /// `enabled == false`.
    control_loop_liveness: Rc<()>,
}

impl ScreenEncoder {
    /// Construct a screen encoder:
    ///
    /// * `client` - an instance of a [`VideoCallClient`](crate::VideoCallClient).  It does not need to be currently connected.
    /// * `bitrate_kbps` - initial bitrate in kilobits per second
    /// * `on_encoder_settings_update` - callback for encoder settings updates (e.g., bitrate changes)
    /// * `on_state_change` - callback for screen share state changes (started, cancelled, stopped)
    /// * `screen_sharing_active` - shared coordination flag; obtain from [`CameraEncoder::screen_sharing_flag()`](crate::CameraEncoder::screen_sharing_flag)
    /// * `max_layers` - maximum number of SCREEN simulcast layers to emit (issue
    ///   #989, Phase 3b). The UI passes `min(experimentalSimulcastMaxLayers,
    ///   capability ceiling)`, exactly like the camera. Clamped to
    ///   [`SCREEN_SIMULCAST_MAX_SUPPORTED_LAYERS`]; `0`/`1` yield a single layer
    ///   (byte-identical to the legacy screen path).
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
            current_bitrate: Rc::new(AtomicU32::new(bitrate_kbps)),
            current_fps: Arc::new(AtomicU32::new(0)),
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
            shared_screen_adaptive_tier: Rc::new(RefCell::new(String::new())),
            shared_screen_cause_hint: Rc::new(RefCell::new(String::new())),
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
            // AQ control-loop liveness token (issue #1108). Sole strong owner;
            // the self-tick loop holds a Weak and exits when this drops.
            control_loop_liveness: Rc::new(()),
        }
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

    /// Returns the current screen share quality tier index (0=high, 1=medium, 2=low).
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

    /// Returns the shared tier transitions buffer for health reporting.
    pub fn shared_tier_transitions(&self) -> Rc<RefCell<Vec<TierTransitionRecord>>> {
        self.shared_tier_transitions.clone()
    }

    /// Set user-configurable SCREEN-SHARE quality tier bounds (issue #961
    /// follow-up). This is the public API the Dioxus "Screen Share Thresholds"
    /// slider calls. The arguments are **tier indices** into
    /// `SCREEN_QUALITY_TIERS` (the 3-tier ladder: 0 = high/1080p, 1 =
    /// medium/720p, 2 = low).
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
    /// "Not sharing" empty state), and `Some(snapshot)` while sharing. The
    /// snapshot resolves the live shared atomics (`shared_screen_tier_index`,
    /// `shared_screen_encoder_target_bitrate_kbps`) against `SCREEN_QUALITY_TIERS`
    /// with the index clamped, so it never panics mid-transition and is cheap
    /// enough to poll each render tick.
    ///
    /// Note on `target_bitrate_kbps`: the shared target atomic reads `0` at tier
    /// 0 by the issue #903 "omit-on-unconstrained" wire contract. To give the VU
    /// needle a meaningful value, this falls back to the current tier's
    /// `ideal_bitrate_kbps` when the live target reads `0`.
    pub fn live_screen_snapshot(&self) -> Option<ScreenQualitySnapshot> {
        if !self.screen_sharing_active.load(Ordering::Acquire) {
            return None;
        }
        let idx = (self.shared_screen_tier_index.load(Ordering::Relaxed) as usize)
            .min(SCREEN_QUALITY_TIERS.len().saturating_sub(1));
        let tier = &SCREEN_QUALITY_TIERS[idx];
        let live_target = self
            .shared_screen_encoder_target_bitrate_kbps
            .load(Ordering::Relaxed);
        let target_bitrate_kbps = if live_target > 0 {
            live_target
        } else {
            tier.ideal_bitrate_kbps
        };
        Some(ScreenQualitySnapshot {
            tier_index: idx,
            width: tier.max_width,
            height: tier.max_height,
            fps: tier.target_fps,
            ideal_kbps: tier.ideal_bitrate_kbps,
            target_bitrate_kbps,
        })
    }

    /// Live SEND-side simulcast diagnostics for the screen share (issue #1095
    /// observability). `None` while not sharing (mirrors
    /// [`Self::live_screen_snapshot`]). Reads the active-layer count + per-layer
    /// target-bitrate atomics and resolves EVERY effective layer's fixed
    /// resolution from the SCREEN simulcast ladder. Panic-safe; cheap to poll.
    ///
    /// Emits one rung per EFFECTIVE layer (not just active), so a SHED layer
    /// (`bitrate_kbps == 0`) stays visible instead of the ladder shrinking —
    /// same shed-aware logic as the camera (issue #1095), via the shared
    /// [`build_simulcast_layers`] helper.
    ///
    /// In single-stream mode (effective layers == 1) the returned snapshot has
    /// `simulcast_active = false` and an empty `layers` Vec.
    pub fn live_simulcast_snapshot(&self) -> Option<SimulcastSendSnapshot> {
        if !self.screen_sharing_active.load(Ordering::Acquire) {
            return None;
        }
        let effective = self.effective_layer_count();
        if effective <= 1 {
            return Some(SimulcastSendSnapshot {
                simulcast_active: false,
                effective_layers: effective,
                active_layers: 1,
                layers: Vec::new(),
            });
        }
        // Full SCREEN ladder resolutions (every effective layer, shed included).
        let resolutions: Vec<(u32, u32)> = simulcast_screen_layers(effective as usize)
            .iter()
            .map(|t| (t.max_width, t.max_height))
            .collect();
        let active = (self.shared_active_layer_count.load(Ordering::Relaxed))
            .min(effective)
            .max(1);
        let active_bitrates_kbps: Vec<u32> = {
            let bitrate_atomics = self.shared_layer_bitrates_bps.borrow();
            (0..active)
                .map(|layer_id| {
                    bitrate_atomics
                        .get(layer_id as usize)
                        .map(|a| a.load(Ordering::Relaxed) / 1000)
                        .unwrap_or(0)
                })
                .collect()
        };
        let layers = build_simulcast_layers(effective, active, &resolutions, &active_bitrates_kbps);
        Some(SimulcastSendSnapshot {
            simulcast_active: true,
            effective_layers: effective,
            active_layers: active,
            layers,
        })
    }

    /// Spawn the screen-encoder AQ control loop (issue #1108: now a self-timer).
    ///
    /// Mirrors `CameraEncoder::set_encoder_control`: receiver FPS no longer
    /// drives the sender AQ, so this ticks at `AQ_TICK_INTERVAL_MS` off the
    /// screen encoder's own backpressure (`shared_encoder_queue_depth`) plus the
    /// re-election signal, instead of consuming a diagnostics channel.
    pub fn set_encoder_control(&mut self) {
        let current_bitrate = self.current_bitrate.clone();
        let current_fps = self.current_fps.clone();
        let on_encoder_settings_update = self.on_encoder_settings_update.clone();
        let enabled = self.state.enabled.clone();
        let tier_max_width = self.tier_max_width.clone();
        let tier_max_height = self.tier_max_height.clone();
        let tier_keyframe_interval = self.tier_keyframe_interval.clone();
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
        let shared_target_bitrate = self.shared_screen_encoder_target_bitrate_kbps.clone();
        let shared_adaptive_tier = self.shared_screen_adaptive_tier.clone();
        let shared_cause_hint = self.shared_screen_cause_hint.clone();
        // #961 (send quality bounds) + #1082 (screen simulcast) both feed the
        // screen encoder control loop — clone both sides' shared state.
        let quality_bounds = self.quality_bounds.clone();
        let n_layers = self.effective_layer_count() as usize;
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
        // Liveness sentinel (issue #1108): a Weak to the encoder-owned token. The
        // loop breaks once this fails to upgrade (ScreenEncoder dropped on Host
        // unmount), so the immortal `spawn_local` future doesn't leak per remount.
        let control_loop_liveness = Rc::downgrade(&self.control_loop_liveness);
        wasm_bindgen_futures::spawn_local(async move {
            let mut encoder_control =
                EncoderBitrateController::new_for_screen(current_fps.clone(), SCREEN_QUALITY_TIERS);

            // Apply any user screen-quality bounds set before the loop started,
            // and track the generation we last applied so we only re-apply when
            // the UI actually changes them (issue #961 follow-up). The screen
            // controller's clamp logic is generic over its 3-tier ladder.
            let mut applied_bounds_generation = {
                let shared = quality_bounds.borrow();
                encoder_control.set_video_quality_bounds(shared.bounds.best, shared.bounds.worst);
                shared.generation
            };

            // Enable simulcast on the screen controller when >1 layer. The
            // controller is `is_screen`, so its per-layer PIDs use the SCREEN
            // ladder. n_layers == 1 leaves it single-stream (byte-identical).
            //
            // EARN-UP COLD START (issue #1200): use
            // `set_simulcast_ceiling_start_at_base`, NOT `set_simulcast_layers`.
            // The latter seeds `active_layer_count == n` (all rungs hot from
            // frame one — ~4.2 Mbps / 3 encodes immediately), which is what the
            // screen path used to do. Mirroring the camera (#1140/#1141), we now
            // configure the device CEILING to `n_layers` but START the active
            // count at the BASE rung (1); the headroom-probe ramp in
            // `EncoderBitrateController::tick` earns the upper rungs up to the
            // ceiling only when backpressure + uplink budget allow. The
            // receiver-side LayerChooser already handles an upper layer that
            // appears late, so a late-earned 1080p rung is delivered correctly.
            if n_layers > 1 {
                encoder_control.set_simulcast_ceiling_start_at_base(n_layers);
                let mut atomics = shared_layer_bitrates_bps.borrow_mut();
                if atomics.len() != n_layers {
                    *atomics = (0..n_layers).map(|_| Rc::new(AtomicU32::new(0))).collect();
                }
            }
            // Client-side uplink-backpressure self-trigger windows (issue #1199,
            // mirroring the camera AQ loop). The WS send-buffer drop counter and
            // the WT unistream drop counter are TRANSPORT-GLOBAL statics
            // (`websocket::websocket_drop_count()` /
            // `webtransport::unistream_drop_count()`), shared by the camera,
            // screen, and microphone egress on the SAME connection.
            //
            // DROP-COUNTER ATTRIBUTION DECISION (issue #1199, requirement 3):
            // each controller keeps its OWN baseline snapshot + sliding window
            // against the shared global counters. We deliberately do NOT attempt
            // to attribute drops to a specific media-kind (the transport does not
            // tag drops by stream, and a single browser TCP send buffer / QUIC
            // connection is the shared bottleneck). So a drop burst is observed
            // independently by BOTH the camera and the screen loop, and BOTH may
            // shed a layer. That is the CORRECT behavior: the uplink is shared,
            // so when it is distressed every live egress should back off. The
            // baselines are SEPARATE (not shared) only so the two loops' sliding
            // windows roll on their own cadence and neither clears the other's
            // accounting — they are NOT a partition of the drops.
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
            // of the owning ScreenEncoder. This `spawn_local` future is NOT bound
            // to the Dioxus component scope, so it must break itself when the
            // encoder is torn down (Host unmount) via the liveness Weak —
            // otherwise it ticks forever and leaks one loop per remount.
            loop {
                gloo_timers::future::sleep(std::time::Duration::from_millis(
                    crate::adaptive_quality_constants::AQ_TICK_INTERVAL_MS,
                ))
                .await;
                // Encoder torn down? Stop ticking and release the captured Rc graph.
                if control_loop_liveness.upgrade().is_none() {
                    log::debug!("ScreenEncoder: AQ control loop exiting (encoder dropped)");
                    break;
                }
                let now = js_sys::Date::now();
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
                // RISING EDGE: a fresh share just began. Re-arm the controller's
                // cold start so `active_layer_count` resets to the base rung (1),
                // undoing any idle drift from a prior session. Done BEFORE this
                // tick's `tick()`/write block so the freshly-armed base count is
                // what gets written this iteration. Guarded by `n_layers > 1` to
                // match the construction-time `set_simulcast_ceiling_start_at_base`
                // guard — single-stream mode stays byte-identical.
                if share_started {
                    if n_layers > 1 {
                        encoder_control.set_simulcast_ceiling_start_at_base(n_layers);
                        log::info!(
                            "ScreenEncoder: (re)share started — re-armed AQ cold start at base rung (ceiling {n_layers})"
                        );
                    }
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
                    // fail-open / no cap) so the controller caps the screen ladder to
                    // what some receiver actually wants. Applied right before `tick`
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
                    let output_wasted = Some(encoder_control.last_target_bitrate_kbps());

                    // Screen simulcast (issue #989, Phase 3b): publish the active
                    // layer count + per-layer target bitrates to the encode loop
                    // every tick. Skipped entirely in single-stream mode, so the
                    // legacy behavior is byte-identical.
                    if encoder_control.is_simulcast() {
                        let active = encoder_control.active_layer_count() as u32;
                        shared_active_layer_count.store(active, Ordering::Relaxed);
                        let per_layer = encoder_control.layer_target_bitrates_kbps();
                        let atomics = shared_layer_bitrates_bps.borrow();
                        for (i, atomic) in atomics.iter().enumerate() {
                            if let Some(&kbps) = per_layer.get(i) {
                                atomic.store((kbps * 1000.0) as u32, Ordering::Relaxed);
                            }
                        }
                    }
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

                    // Issue #903: refresh the encoder's *target* bitrate every
                    // tick so the consumer's Cause line reflects what the encoder
                    // is currently trying to produce (not just the last
                    // negotiated step). This runs whether or not a tier change
                    // fired — PID-driven adjustments within a tier still change
                    // the target.
                    //
                    // Contract: at tier 0 the encoder is "unconstrained" and the
                    // entire Cause line must be omitted by the receiver. The
                    // tier-change branch below clears tier + hint at index 0;
                    // for symmetry the target bitrate must read `0` too,
                    // otherwise the receiver renders a partial `Cause: <N>kbps`
                    // line (the renderer keys off ANY non-default Cause field).
                    if encoder_control.video_tier_index() == 0 {
                        shared_target_bitrate.store(0, Ordering::Relaxed);
                    } else {
                        let last_target =
                            encoder_control.last_target_bitrate_kbps().round().max(0.0) as u32;
                        if last_target > 0 {
                            shared_target_bitrate.store(last_target, Ordering::Relaxed);
                        }
                    }

                    // Check for tier changes and update shared atomics.
                    if encoder_control.take_tier_changed() {
                        let tier = encoder_control.current_video_tier();
                        tier_max_width.store(tier.max_width, Ordering::Relaxed);
                        tier_max_height.store(tier.max_height, Ordering::Relaxed);
                        tier_keyframe_interval
                            .store(tier.keyframe_interval_frames, Ordering::Relaxed);
                        let tier_index = encoder_control.video_tier_index();
                        shared_screen_tier_idx.store(tier_index as u32, Ordering::Relaxed);
                        log::info!(
                            "ScreenEncoder: tier changed to '{}' ({}x{}, {}fps, kf={})",
                            tier.label,
                            tier.max_width,
                            tier.max_height,
                            tier.target_fps,
                            tier.keyframe_interval_frames,
                        );
                        // Issue #903: refresh the tier label exposed on the wire.
                        // Tier 0 (highest) is treated as "unconstrained" and
                        // clears the label so the receiver omits the Cause line.
                        // Target bitrate must also be cleared here for the
                        // same omit-on-unconstrained contract — the per-tick
                        // refresh above already guards on `tier_index == 0`,
                        // but a tier-change tick that arrives without a
                        // subsequent diagnostics packet would otherwise leave
                        // the previous tier's target bitrate stale.
                        if tier_index == 0 {
                            shared_target_bitrate.store(0, Ordering::Relaxed);
                            shared_adaptive_tier.borrow_mut().clear();
                            shared_cause_hint.borrow_mut().clear();
                        } else {
                            *shared_adaptive_tier.borrow_mut() = tier.label.to_string();
                        }
                    }

                    // Drain tier transitions, overriding stream to "screen".
                    let mut transitions = encoder_control.drain_tier_transitions();
                    for t in &mut transitions {
                        t.stream = "screen";
                    }
                    // Issue #903: capture the *most recent* transition's trigger
                    // as the publisher's cause classification. We only refresh
                    // the hint when AQ is actually constraining the encoder
                    // (tier index > 0); at the top tier the encoder is
                    // unconstrained and the receiver should not show a Cause
                    // line.
                    if !transitions.is_empty() && encoder_control.video_tier_index() > 0 {
                        if let Some(last) = transitions.last() {
                            let hint = cause_hint_from_trigger(last.trigger);
                            if !hint.is_empty() {
                                *shared_cause_hint.borrow_mut() = hint.to_string();
                            }
                        }
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
        // Clear screen-sharing flag so the camera encoder removes its quality ceiling.
        self.screen_sharing_active.store(false, Ordering::Release);

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

        // Issue #1229: on every (re)share, synchronously reset the active screen
        // layer count to the BASE rung when simulcast is active, so the encode
        // loop (a SEPARATE spawn_local future that reads `shared_active_layer_count`
        // at setup ~run_screen_encoding and per-frame) starts at base from frame
        // one — not the construction-time FULL count (`clamp_screen_layer_count(max_layers)`)
        // nor a value drifted up by a prior session's AQ loop. The AQ control loop
        // independently re-arms the controller on the sharing rising edge (see
        // `set_encoder_control`); this write closes the cross-future race where the
        // encode loop could read the stale-high count before the AQ loop's first
        // post-edge tick writes the fresh base value. No-op semantics in
        // single-stream mode: the encode loop ignores `shared_active_layer_count`
        // unless `simulcast` (n_layers > 1), so writing 1 here is byte-identical.
        if self.effective_layer_count() > 1 {
            self.shared_active_layer_count.store(1, Ordering::Relaxed);
        }

        // Issue #903: seed the publisher-side encoder-state metadata so the
        // very first frames carry meaningful Cause data. The screen-share
        // ladder defaults to the *medium* tier (not the top); that is a
        // bandwidth-conservative choice and the receiver should be able to
        // explain the resulting downscale immediately rather than waiting
        // for the first PID-driven tier transition.
        //
        // Contract (mirrored in the consumer at
        // `dioxus-ui/components/signal_quality.rs::build_screen_cause_line`
        // and on `SignalSample::screen_encoder_target_bitrate_kbps`'s
        // doc-comment): tier 0 is "unconstrained" and ALL three Cause-line
        // fields — target bitrate, tier label, and cause hint — must read
        // their proto3 defaults (`0` / empty). If we leak the high-tier
        // ideal bitrate here, the receiver renders a partial
        // `Cause: <N>kbps` line that violates the omit-on-unconstrained
        // contract (regression caught by HCL e2e iter2: cause-hint test
        // observed `Cause: 2500kbps` at tier 0 from cold-start RTT/camera
        // signals).
        if clamped_tier == 0 {
            self.shared_screen_encoder_target_bitrate_kbps
                .store(0, Ordering::Relaxed);
            self.shared_screen_adaptive_tier.borrow_mut().clear();
            self.shared_screen_cause_hint.borrow_mut().clear();
        } else {
            self.shared_screen_encoder_target_bitrate_kbps
                .store(tier.ideal_bitrate_kbps, Ordering::Relaxed);
            *self.shared_screen_adaptive_tier.borrow_mut() = tier.label.to_string();
            // Default cause for the initial constrained tier: the encoder
            // started below the top of the ladder because the screen
            // encoder seeds itself there to avoid ramp-up bandwidth
            // contention. AQ will revise this once a real transition
            // fires.
            *self.shared_screen_cause_hint.borrow_mut() = "bitrate-limited".to_string();
        }

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
        // Issue #1311: hand the encode loop its own clone of the cooldown-reset atom.
        let keyframe_cooldown_reset = self.keyframe_cooldown_reset.clone();
        let active_video_track = self.active_video_track.clone();
        let screen_sharing_active = self.screen_sharing_active.clone();
        let shared_target_bitrate = self.shared_screen_encoder_target_bitrate_kbps.clone();
        let shared_adaptive_tier = self.shared_screen_adaptive_tier.clone();
        let shared_cause_hint = self.shared_screen_cause_hint.clone();
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
                on_state_change,
                screen_stream,
                tier_max_width,
                tier_max_height,
                tier_keyframe_interval,
                force_keyframe,
                keyframe_cooldown_reset,
                active_video_track,
                screen_sharing_active,
                shared_target_bitrate,
                shared_adaptive_tier,
                shared_cause_hint,
                n_layers,
                shared_active_layer_count,
                shared_layer_bitrates_bps,
                shared_encoder_queue_depth,
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
        // Issue #1311: hand the encode loop its own clone of the cooldown-reset atom.
        let keyframe_cooldown_reset = self.keyframe_cooldown_reset.clone();
        let active_video_track = self.active_video_track.clone();
        let screen_sharing_active = self.screen_sharing_active.clone();
        let shared_target_bitrate = self.shared_screen_encoder_target_bitrate_kbps.clone();
        let shared_adaptive_tier = self.shared_screen_adaptive_tier.clone();
        let shared_cause_hint = self.shared_screen_cause_hint.clone();
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
                keyframe_cooldown_reset,
                active_video_track,
                screen_sharing_active,
                shared_target_bitrate,
                shared_adaptive_tier,
                shared_cause_hint,
                n_layers,
                shared_active_layer_count,
                shared_layer_bitrates_bps,
                shared_encoder_queue_depth,
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
        current_fps: Arc<AtomicU32>,
        on_state_change: Option<Callback<ScreenShareEvent>>,
        screen_stream: Rc<RefCell<Option<MediaStream>>>,
        tier_max_width: Rc<AtomicU32>,
        tier_max_height: Rc<AtomicU32>,
        tier_keyframe_interval: Rc<AtomicU32>,
        force_keyframe: Arc<AtomicBool>,
        // Issue #1311: forced-keyframe cooldown reset. The encode loop CONSUMES this
        // each frame (`.swap(false)`) and clears `last_keyframe_emit_ms` when set, so
        // the first PLI after a reconnect/re-election is not coalesced away by a stale
        // pre-transition cooldown timestamp. ARMED by the quality task (re-election)
        // and the client's `Connected` callback (reconnect).
        keyframe_cooldown_reset: Rc<AtomicBool>,
        active_video_track: Rc<RefCell<Option<MediaStreamTrack>>>,
        screen_sharing_active: Rc<AtomicBool>,
        // Issue #903: publisher-side encoder state read at frame-stamping
        // time and stamped onto every `VideoMetadata`. The values are
        // updated by `set_encoder_control` whenever AQ acts; the output
        // handler below treats `0` / empty as "no data" so the receiver
        // omits the Cause line for unconstrained streams.
        shared_target_bitrate: Rc<AtomicU32>,
        shared_adaptive_tier: Rc<RefCell<String>>,
        shared_cause_hint: Rc<RefCell<String>>,
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
    ) {
        let simulcast = n_layers > 1;
        // Per-layer sequence numbers persist across restarts so a receiver
        // decoding one screen layer sees a dense 0,1,2,… stream (no phantom
        // loss). N=1 is a single-element Vec behaving like the old scalar.
        let mut sequence_numbers: Vec<u64> = vec![0; n_layers];
        // Signal camera encoder ASAP after capture is confirmed so it begins
        // stepping down during encoder setup, not after encoding starts.
        screen_sharing_active.store(true, Ordering::Release);

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
            screen_sharing_active.store(false, Ordering::Release);
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

        // Shared atomics carrying the publisher's *source* track dimensions
        // (from `MediaStreamTrack.getSettings()`). The output-chunk handler
        // below is created once and outlives the `'restart:` loop, so it can
        // not capture `width` / `height` directly — they get reassigned on
        // restart. Atomics let the per-chunk closure read the most recent
        // source dims at frame-stamping time without locking. `0` means
        // "unknown" and triggers the proto3 default-skip, so older publishers
        // / pre-capture frames stay backward-compatible.
        let source_width_atomic = Arc::new(AtomicU32::new(0));
        let source_height_atomic = Arc::new(AtomicU32::new(0));

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
            let source_width_for_handler = source_width_atomic.clone();
            let source_height_for_handler = source_height_atomic.clone();
            // Issue #903: per-chunk handles to the encoder-state shared
            // values. Same indirection pattern as the source dimensions —
            // the controller loop writes, the output handler reads, both
            // outlive any individual encoder restart.
            let target_bitrate_for_handler = shared_target_bitrate.clone();
            let adaptive_tier_for_handler = shared_adaptive_tier.clone();
            let cause_hint_for_handler = shared_cause_hint.clone();

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

                // Read the latest source dimensions snapshot. The encoder
                // loop updates the atomics whenever the track is (re)acquired
                // and reports its native capture size via `get_settings()`.
                // `Ordering::Relaxed` is sufficient — these values are
                // descriptive metadata, not synchronization signals.
                let source_width_now = source_width_for_handler.load(Ordering::Relaxed);
                let source_height_now = source_height_for_handler.load(Ordering::Relaxed);
                // Issue #903: snapshot encoder state for the receiver's
                // Cause line. Cheap: the bitrate is an atomic load and the
                // two strings are short labels we clone once per frame.
                let target_bitrate_now = target_bitrate_for_handler.load(Ordering::Relaxed);
                let adaptive_tier_now = adaptive_tier_for_handler.borrow().clone();
                let cause_hint_now = cause_hint_for_handler.borrow().clone();
                let packet: PacketWrapper = transform_screen_chunk(
                    chunk,
                    sequence_number,
                    buffer.as_mut_slice(),
                    &userid,
                    aes.clone(),
                    source_width_now,
                    source_height_now,
                    target_bitrate_now,
                    adaptive_tier_now,
                    cause_hint_now,
                    // N=1 single-layer path: layer 0 (wire-absent), byte-identical
                    // to the pre-simulcast screen publisher.
                    0,
                );
                // Phase 2 of WT freeze fix: route screen-share video on its
                // own persistent QUIC stream, isolated from the camera and
                // audio streams.
                client.send_media_packet(packet, MediaStreamKey::Screen);
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
                screen_sharing_active.store(true, Ordering::Release);

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
                        screen_sharing_flag_clone.store(false, Ordering::Release);
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

                // Publish the source dims to the per-chunk stamper. Read by
                // the screen_output_handler closure on every encoded frame.
                source_width_atomic.store(width, Ordering::Relaxed);
                source_height_atomic.store(height, Ordering::Relaxed);

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
                        screen_sharing_flag_clone.store(false, Ordering::Release);
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

                // Publish the source dims to the per-chunk stamper (see the
                // matching `.store()` in the restart-acquire branch above).
                source_width_atomic.store(width, Ordering::Relaxed);
                source_height_atomic.store(height, Ordering::Relaxed);

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
            // LAZY CONSTRUCTION (issue #1204): now that the screen ladder earns
            // up from the base rung (#1200), an upper rung's VideoEncoder is built
            // only on its FIRST ACTIVATION rather than all of layers 1..n at
            // setup. `build_extra_layer` constructs ONE higher rung; at setup we
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
                let tier = &screen_tiers[layer_idx];
                // Treat the tier as a BOUNDING BOX, not a fixed output size
                // (issue #1196): fit the actual capture dims inside the
                // layer's rung, aspect-preserving. This is a construction
                // SEED — the first GOP is aspect-correct — and the per-frame
                // encode loop re-fits each rung against this same tier box
                // (`tier_w`/`tier_h` recorded below) when the share's source
                // aspect changes mid-share, exactly like the base screen
                // layer's per-frame reconfigure and the camera's per-layer
                // path. `width` / `height` are the real capture dims read
                // from `getSettings()` above, so a non-16:9 display (16:10,
                // ultrawide, portrait) is never per-axis-squashed into the
                // 16:9 tier dims on rungs 1..n.
                let (layer_w, layer_h) =
                    fit_within_preserving_aspect(width, height, tier.max_width, tier.max_height);
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
                    let source_w = source_width_atomic.clone();
                    let source_h = source_height_atomic.clone();
                    let target_bitrate = shared_target_bitrate.clone();
                    let adaptive_tier = shared_adaptive_tier.clone();
                    let cause_hint = shared_cause_hint.clone();
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
                            let packet: PacketWrapper = transform_screen_chunk(
                                chunk,
                                local_seq,
                                buffer.as_mut_slice(),
                                &userid,
                                aes.clone(),
                                source_w.load(Ordering::Relaxed),
                                source_h.load(Ordering::Relaxed),
                                target_bitrate.load(Ordering::Relaxed),
                                adaptive_tier.borrow().clone(),
                                cause_hint.borrow().clone(),
                                layer_id,
                            );
                            client.send_media_packet(packet, MediaStreamKey::Screen);
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
                    local_bitrate: init_bitrate_bps as u32,
                    _output_closure: output_closure,
                    _error_closure: error_closure,
                })
            };

            let mut extra_layers: Vec<LayerEncoder> = Vec::new();
            if simulcast {
                // Build only the higher rungs that are ACTIVE right now (cold
                // start: none, since the screen ladder earns up from the base).
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
            let mut current_encoder_width = width;
            let mut current_encoder_height = height;

            // Cache tier-controlled values
            let mut local_keyframe_interval = tier_keyframe_interval.load(Ordering::Relaxed);
            let mut local_tier_max_width = tier_max_width.load(Ordering::Relaxed);
            let mut local_tier_max_height = tier_max_height.load(Ordering::Relaxed);

            // Log-on-change guard for the "Updating screen bitrate" line
            // (issue #1221-pt1). The bitrate reconfigure below is gated on
            // `new_bitrate != local_bitrate`, but `local_bitrate` is also
            // mutated each tick by the simulcast base-layer per-layer bitrate
            // pass (it tracks the LAST APPLIED bitrate, not the last LOGGED
            // one), so the single-stream `info!` could re-fire on essentially
            // every frame as the two sources interleave — 16,878 lines in one
            // 46-minute meeting. We log ONLY when the applied bitrate differs
            // from the value we last LOGGED (seeded to the initial config
            // bitrate so the first genuine change logs, but a steady-state
            // bitrate never does). This is logging-only: the reconfigure
            // decision and `local_bitrate` bookkeeping are untouched.
            let mut last_logged_bitrate = local_bitrate;

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

                // Check for tier-driven dimension/keyframe changes.
                let new_tier_w = tier_max_width.load(Ordering::Relaxed);
                let new_tier_h = tier_max_height.load(Ordering::Relaxed);
                let new_kf = tier_keyframe_interval.load(Ordering::Relaxed);

                let tier_dims_changed =
                    new_tier_w != local_tier_max_width || new_tier_h != local_tier_max_height;
                if tier_dims_changed {
                    local_tier_max_width = new_tier_w;
                    local_tier_max_height = new_tier_h;

                    // Constrain to the tier max while preserving the capture
                    // source aspect ratio (issue #1037). `current_encoder_*` is
                    // seeded from the screen track's native getSettings() dims
                    // and only ever updated via this uniform fit or a raw
                    // VideoFrame's native dims, so it always carries the source
                    // aspect. getDisplayMedia requests 16:9 (ideal 1920x1080)
                    // but the actual capture can be 16:10, ultrawide, portrait,
                    // etc.; a per-axis `.min()` against the 16:9 tier ceiling
                    // would stretch/squash those sources.
                    let (constrained_w, constrained_h) = fit_within_preserving_aspect(
                        current_encoder_width,
                        current_encoder_height,
                        local_tier_max_width,
                        local_tier_max_height,
                    );

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
                    new_config.set_bitrate(local_bitrate as f64);
                    new_config.set_latency_mode(LatencyMode::Realtime);
                    set_vbr_mode(&new_config);
                    if let Err(e) = screen_encoder.configure(&new_config) {
                        SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL.fetch_add(1, Ordering::Relaxed);
                        error!("Error reconfiguring screen encoder for tier change: {e:?}");
                        if is_fatal_encoder_error(&e) {
                            record_screen_restart(RestartReason::Configure);
                            fatal_encode_exit = true;
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
                    // Log-on-change only (issue #1221-pt1): suppress the line
                    // unless this differs from the bitrate we last logged.
                    if new_bitrate != last_logged_bitrate {
                        info!("Updating screen bitrate to {new_bitrate}");
                        last_logged_bitrate = new_bitrate;
                    }
                    local_bitrate = new_bitrate;
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
                    new_config.set_bitrate(local_bitrate as f64);
                    new_config.set_latency_mode(LatencyMode::Realtime);
                    set_vbr_mode(&new_config);
                    if let Err(e) = screen_encoder.configure(&new_config) {
                        SCREEN_ENCODER_ERRORS_CONFIGURE_FATAL.fetch_add(1, Ordering::Relaxed);
                        error!("Error configuring screen encoder: {e:?}");
                        if is_fatal_encoder_error(&e) {
                            record_screen_restart(RestartReason::Configure);
                            fatal_encode_exit = true;
                            restart_count += 1;
                            break 'encode;
                        }
                    }
                } else if new_bitrate != local_bitrate {
                    local_bitrate = new_bitrate;
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
                            && want != local_bitrate
                            && screen_encoder.state() != CodecState::Closed
                        {
                            local_bitrate = want;
                            let cfg = VideoEncoderConfig::new(
                                get_video_codec_string(),
                                current_encoder_height,
                                current_encoder_width,
                            );
                            cfg.set_bitrate(local_bitrate as f64);
                            cfg.set_latency_mode(LatencyMode::Realtime);
                            set_vbr_mode(&cfg);
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
                            layer.local_bitrate = want;
                            layer.config.set_bitrate(layer.local_bitrate as f64);
                            if let Err(e) = layer.encoder.configure(&layer.config) {
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
                        let raw_frame_width = video_frame.display_width();
                        let raw_frame_height = video_frame.display_height();
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
                                fit_within_preserving_aspect(
                                    raw_frame_width,
                                    raw_frame_height,
                                    local_tier_max_width,
                                    local_tier_max_height,
                                )
                            } else {
                                (0, 0)
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
                                    record_screen_restart(RestartReason::Configure);
                                    fatal_encode_exit = true;
                                    restart_count += 1;
                                    break 'encode;
                                }
                            }
                        }

                        let opts = VideoEncoderEncodeOptions::new();
                        let now = window()
                            .performance()
                            .expect("Performance API not available")
                            .now();
                        // Use tier-controlled keyframe interval.
                        // Using `%` instead of `.is_multiple_of()` for compatibility
                        // with Rust toolchains older than 1.87.
                        #[allow(clippy::manual_is_multiple_of)]
                        let is_periodic_keyframe = local_keyframe_interval > 0
                            && screen_frame_counter % local_keyframe_interval == 0;
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
                        });
                        let want_keyframe = decision.want_keyframe;
                        last_keyframe_emit_ms = decision.last_keyframe_emit_ms;
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
                        if decision.pli_forced {
                            log::info!(
                                "ScreenEncoder: forcing keyframe at frame {} (PLI)",
                                screen_frame_counter
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
                            let decision = simulcast_layer_target_dims(
                                raw_frame_width,
                                raw_frame_height,
                                layer.tier_w,
                                layer.tier_h,
                                layer.current_w,
                                layer.current_h,
                            );
                            if decision.needs_reconfigure {
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

                        video_frame.close();

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
            // Drop the higher layers (and their closures) before the next
            // 'restart iteration rebuilds them.
            drop(extra_layers);

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
        screen_sharing_active.store(false, Ordering::Release);

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

    use super::cause_hint_from_trigger;
    use super::clamp_screen_layer_count;
    use super::is_fatal_encoder_error_message;
    use super::keyframe_tick_decision;
    use super::record_screen_restart;
    use super::screen_encoder_restarts_closed_codec;
    use super::screen_encoder_restarts_configure;
    use super::screen_encoder_restarts_memory;
    use super::screen_encoder_restarts_other;
    use super::should_reacquire_screen_capture;
    use super::should_teardown_shed_layer;
    use super::wt_drop_step_down_decision;
    use super::wt_saturation_step_down_decision;
    use super::KeyframeTickInput;
    use super::RestartReason;
    use super::ScreenEncoder;
    use super::SCREEN_SIMULCAST_MAX_SUPPORTED_LAYERS;
    use super::SHED_TEARDOWN_DWELL_MS;
    use crate::adaptive_quality_constants::{
        WS_SELF_CONGESTION_WINDOW_MS, WT_SATURATION_STALL_THRESHOLD, WT_SATURATION_WINDOW_MS,
        WT_SELF_CONGESTION_DROP_THRESHOLD, WT_SELF_CONGESTION_WINDOW_MS,
    };
    use crate::{Callback, ScreenShareEvent, VideoCallClient, VideoCallClientOptions};

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

    #[test]
    fn clamp_screen_layer_count_treats_zero_and_one_as_one() {
        // 0 and 1 → single layer (feature off / byte-identical screen path).
        assert_eq!(clamp_screen_layer_count(0), 1);
        assert_eq!(clamp_screen_layer_count(1), 1);
    }

    #[test]
    fn clamp_screen_layer_count_passes_through_and_caps() {
        assert_eq!(clamp_screen_layer_count(2), 2);
        assert_eq!(clamp_screen_layer_count(3), 3);
        assert_eq!(
            clamp_screen_layer_count(99),
            SCREEN_SIMULCAST_MAX_SUPPORTED_LAYERS
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

    /// Issue #903: the trigger -> cause_hint mapping is the publisher-side
    /// wire format consumed by `build_screen_cause_line` in the dioxus UI.
    /// If a `videocall-aq` trigger string is ever renamed without updating
    /// this match arm, the function silently falls back to `""`, the Cause
    /// line vanishes from the Signal Quality tooltip, and no other test
    /// fails. This guards each known mapping and the unknown-trigger
    /// fallback so a stale arm is caught at compile time.
    #[test]
    fn cause_hint_from_trigger_maps_each_known_trigger_and_falls_back_for_unknown() {
        assert_eq!(cause_hint_from_trigger("bitrate"), "bitrate-limited");
        assert_eq!(cause_hint_from_trigger("fps"), "cpu-pressure");
        assert_eq!(cause_hint_from_trigger("congestion"), "network-rtt");
        assert_eq!(cause_hint_from_trigger("coordination"), "manual-cap");
        assert_eq!(cause_hint_from_trigger("nonsense_unknown_trigger"), "");
        assert_eq!(cause_hint_from_trigger(""), "");
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

    /// Issue #982 (follow-up to PR #973 iter2): tier 0 is "unconstrained" and
    /// ALL three Cause-line fields — target bitrate, tier label, cause hint —
    /// MUST read their proto3 defaults (`0` / empty) so the receiver omits
    /// the Cause line entirely.
    ///
    /// This contract is documented in three places that must stay in sync:
    ///   * `ScreenEncoder::apply_initial_tier` (the cold-start branch this
    ///     test exercises),
    ///   * the per-tick / tier-change branches in `set_encoder_control`,
    ///   * `SignalSample::screen_encoder_target_bitrate_kbps`'s doc-comment
    ///     and `build_screen_cause_line` in the dioxus UI (the consumer).
    ///
    /// The regression caught by HCL e2e iter2 of PR #973 was that the
    /// receiver saw `Cause: 2500kbps` at tier 0 — the publisher had leaked
    /// the high tier's `ideal_bitrate_kbps` into the shared atomic. The
    /// renderer keys off ANY non-default Cause field, so a partial line
    /// (just bitrate, with no tier label or hint) is enough to violate the
    /// omit-on-unconstrained contract. This unit test pins the publisher
    /// side directly so a future revert of the tier-0 guards is caught at
    /// `cargo test`, not at e2e time.
    #[test]
    fn tier_zero_zeroes_all_three_cause_line_fields() {
        let client = build_test_client();
        let mut encoder = ScreenEncoder::new(
            client,
            500,
            Callback::from(|_: String| {}),
            Callback::from(|_: ScreenShareEvent| {}),
            Rc::new(AtomicBool::new(false)),
            1, // max_layers (single layer)
        );

        encoder.apply_initial_tier(0);

        assert_eq!(
            encoder
                .shared_screen_encoder_target_bitrate_kbps
                .load(Ordering::Relaxed),
            0,
            "tier 0 must zero shared target bitrate so the receiver omits the Cause line"
        );
        assert!(
            encoder.shared_screen_adaptive_tier.borrow().is_empty(),
            "tier 0 must clear adaptive-tier label so the receiver omits the Cause line"
        );
        assert!(
            encoder.shared_screen_cause_hint.borrow().is_empty(),
            "tier 0 must clear cause hint so the receiver omits the Cause line"
        );
    }

    /// Pairs with `tier_zero_zeroes_all_three_cause_line_fields`: at any
    /// constrained tier the three Cause-line fields must carry non-default
    /// values, otherwise the receiver would also (incorrectly) omit the
    /// Cause line. This guards against an over-eager future refactor that
    /// zeroes the fields at every tier instead of only tier 0.
    #[test]
    fn tier_one_emits_non_default_cause_line_fields() {
        let client = build_test_client();
        let mut encoder = ScreenEncoder::new(
            client,
            500,
            Callback::from(|_: String| {}),
            Callback::from(|_: ScreenShareEvent| {}),
            Rc::new(AtomicBool::new(false)),
            1, // max_layers (single layer)
        );

        encoder.apply_initial_tier(1);

        assert!(
            encoder
                .shared_screen_encoder_target_bitrate_kbps
                .load(Ordering::Relaxed)
                > 0,
            "constrained tier must seed shared target bitrate"
        );
        assert!(
            !encoder.shared_screen_adaptive_tier.borrow().is_empty(),
            "constrained tier must seed adaptive-tier label"
        );
        assert!(
            !encoder.shared_screen_cause_hint.borrow().is_empty(),
            "constrained tier must seed cause hint"
        );
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

    /// Issue #1229: on every (re)share, `apply_initial_tier` must synchronously
    /// reset `shared_active_layer_count` to the BASE rung (1) when simulcast is
    /// active (`max_layers > 1`). This closes the cross-future race where the
    /// encode loop (a separate `spawn_local`) could read a stale-high active count
    /// — either the construction-time FULL seed (`clamp_screen_layer_count(max_layers)`)
    /// or a value drifted up by a prior session's AQ ramp — before the AQ control
    /// loop's first post-rising-edge tick writes the fresh base value.
    ///
    /// This pins the screen-side seed directly (the `videocall-aq` test only
    /// exercises the controller's `set_simulcast_ceiling_start_at_base`). Mutation
    /// guard: deleting the `shared_active_layer_count.store(1, ...)` in
    /// `apply_initial_tier` leaves the stored-high value (3) in place and fails the
    /// assert. We store a HIGH value (3) first to stand in for the drifted/full
    /// count, then prove the seed forces it back to base.
    #[test]
    fn apply_initial_tier_seeds_active_layer_count_to_base_when_simulcast() {
        let client = build_test_client();
        let mut encoder = ScreenEncoder::new(
            client,
            500,
            Callback::from(|_: String| {}),
            Callback::from(|_: ScreenShareEvent| {}),
            Rc::new(AtomicBool::new(false)),
            3, // max_layers (simulcast — exercises the new cold-start seed branch)
        );

        // Construction already seeds the FULL count for simulcast; assert that so
        // the test is meaningful (the seed must move it DOWN, not be a no-op).
        assert_eq!(
            encoder.shared_active_layer_count.load(Ordering::Relaxed),
            3,
            "precondition: simulcast construction seeds the full active layer count"
        );
        // Stand in for a count drifted up by a prior session's AQ ramp.
        encoder
            .shared_active_layer_count
            .store(3, Ordering::Relaxed);

        encoder.apply_initial_tier(0);

        assert_eq!(
            encoder.shared_active_layer_count.load(Ordering::Relaxed),
            1,
            "simulcast (re)share must cold-start the active layer count at the base rung (#1229)"
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
    fn apply_initial_tier_leaves_active_layer_count_untouched_in_single_stream() {
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
        // Sentinel that the byte-identical single-stream path must not touch.
        encoder
            .shared_active_layer_count
            .store(7, Ordering::Relaxed);

        encoder.apply_initial_tier(0);

        assert_eq!(
            encoder.shared_active_layer_count.load(Ordering::Relaxed),
            7,
            "single-stream apply_initial_tier must NOT force the active layer count \
             (byte-identical legacy behavior; the #1229 seed is gated on simulcast)"
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
            });
            if decision.want_keyframe {
                assert!(
                    decision.pli_forced,
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
        });
        assert!(
            reset.want_keyframe,
            "after a reconnect/re-election reset, the first screen PLI must emit a forced \
             keyframe immediately even {}ms < cooldown ({}ms) since the last keyframe",
            first_frame_after_ms - pre_reconnect_emit_ms,
            cd
        );
        assert!(reset.pli_forced, "the un-gated screen emit is PLI-forced");

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
        });
        assert!(
            !next.want_keyframe,
            "after the one-shot reset is consumed, the screen coalescer resumes \
             suppressing PLIs inside the cooldown window"
        );
    }
}

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
use crate::connection::MediaStreamKey;
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
    simulcast_layers, AUDIO_QUALITY_TIERS, BITRATE_CHANGE_THRESHOLD, VIDEO_QUALITY_TIERS,
};
use crate::constants::get_video_codec_string;
use crate::diagnostics::adaptive_quality_manager::TierTransitionRecord;
use crate::diagnostics::EncoderBitrateController;
use crate::health_reporter::ClimbLimiterSnapshot;
use videocall_aq::fit_within_preserving_aspect;

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

/// User-configurable adaptive-quality tier bounds (issue #961), shared from the
/// UI into the running encoder control loop.
///
/// QUALITY IS THE INVERSE OF INDEX: tier index 0 = BEST quality. So each
/// `*_best` field is the user's MAX quality = a FLOOR on the index (never step UP
/// past it), and each `*_worst` field is the user's MIN quality = a CAP on the
/// index (never step DOWN past it). `None` on any end means "Auto" (no user
/// bound). Indices are tier indices into `VIDEO_QUALITY_TIERS` /
/// `AUDIO_QUALITY_TIERS`. The UI maps resolution/bitrate labels to indices.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QualityTierBounds {
    /// Video best/floor index (user MAX quality). `None` = Auto.
    pub video_best: Option<usize>,
    /// Video worst/cap index (user MIN quality). `None` = Auto.
    pub video_worst: Option<usize>,
    /// Audio best/floor index (user MAX quality). `None` = Auto.
    pub audio_best: Option<usize>,
    /// Audio worst/cap index (user MIN quality). `None` = Auto.
    pub audio_worst: Option<usize>,
}

/// Shared, mutable quality-bounds preference plus a "dirty" generation counter.
///
/// The UI writes new bounds via `CameraEncoder::set_quality_tier_bounds`, which
/// updates `bounds` and bumps `generation`. The encoder control loop reads
/// `generation` each tick and, when it advanced, applies `bounds` to the live
/// `EncoderBitrateController`. This is the same live-reconfig pattern used by the
/// congestion / re-election shared flags — the loop never blocks and bounds are
/// applied at the next diagnostics tick (≤1s). The preference is also stored so
/// it can be (re)applied whenever the encoder (re)starts.
#[derive(Debug, Default)]
struct SharedQualityBounds {
    bounds: QualityTierBounds,
    /// Monotonic counter bumped on every write so the loop detects changes
    /// without comparing every field.
    generation: u64,
}

/// A real-time snapshot of the encoder's current adaptive-quality state, sized
/// for the UI VU meter (issue #961).
///
/// All fields are resolved from the live shared atomics + the AQ tier tables at
/// call time, so the UI never needs to index `VIDEO_QUALITY_TIERS` /
/// `AUDIO_QUALITY_TIERS` itself (avoiding out-of-bounds risk). Tier indices are
/// included so the UI can also render the index↔quality relationship.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveQualitySnapshot {
    /// Current video tier index (0 = best / 1080p, 7 = worst / 240p).
    pub video_tier_index: usize,
    /// Current video tier max width (px).
    pub video_width: u32,
    /// Current video tier max height (px).
    pub video_height: u32,
    /// Current video tier target fps.
    pub video_fps: u32,
    /// Current video tier ideal bitrate (kbps).
    pub video_ideal_kbps: u32,
    /// Current audio tier index (0 = best / 50kbps, 3 = worst / 16kbps).
    pub audio_tier_index: usize,
    /// Current audio tier bitrate (kbps).
    pub audio_kbps: u32,
    /// Live PID target bitrate (kbps) the encoder is actually aiming at — the
    /// real-time "needle" value for the VU meter (distinct from the tier ideal).
    pub target_bitrate_kbps: f32,
}

/// One active simulcast layer's live diagnostics: its layer id, the bitrate the
/// AQ controller is currently targeting for it, and its fixed tier resolution
/// (issue #1095 observability). Used by [`SimulcastSendSnapshot`].
///
/// Resolution comes from the per-layer SIMULCAST ladder rung (the layer's tier
/// is fixed; only the bitrate adapts), so it is stable and panic-safely resolved
/// at snapshot time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulcastLayerInfo {
    /// This layer's simulcast id (0 = base / lowest quality).
    pub layer_id: u32,
    /// The bitrate (kbps) the AQ controller is currently targeting for this
    /// layer. `0` until the control loop has published a value.
    pub bitrate_kbps: u32,
    /// Fixed tier width (px) for this layer.
    pub width: u32,
    /// Fixed tier height (px) for this layer.
    pub height: u32,
}

/// A real-time snapshot of the SEND-side simulcast state for one media kind
/// (issue #1095 observability — additive, no AQ behavior change).
///
/// Read from the live shared encoder atomics + the SIMULCAST ladder at call
/// time, so the panel never indexes the AQ tables itself. In single-stream mode
/// (`effective_layers == 1`) `simulcast_active` is `false` and `layers` is empty
/// — the panel then just shows the single adaptive tier from
/// [`LiveQualitySnapshot`]. Cheap enough to poll at the needle cadence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulcastSendSnapshot {
    /// `true` when this encoder is publishing more than one layer.
    pub simulcast_active: bool,
    /// The effective layer count this session may emit
    /// (`min(flag, capability)` ladder size). `1` in single-stream mode.
    pub effective_layers: u32,
    /// How many layers are CURRENTLY active (encoded + sent). The AQ controller
    /// sheds the top layer(s) under congestion, so this can be `< effective_layers`.
    pub active_layers: u32,
    /// Per-EFFECTIVE-layer breakdown (lowest layer first). Empty in single-stream
    /// mode; otherwise length == `effective_layers`. The top
    /// `effective_layers - active_layers` entries are SHED layers, carried with
    /// `bitrate_kbps == 0` so the UI can render them ghosted/dashed (the
    /// `layer_id < active_layers` boundary distinguishes active vs shed). Issue
    /// #1095: shed rungs must stay visible rather than the ladder silently
    /// shrinking when the AQ drops the top layer under congestion.
    pub layers: Vec<SimulcastLayerInfo>,
}

/// Build the per-EFFECTIVE-layer simulcast breakdown (issue #1095), lowest layer
/// first. One [`SimulcastLayerInfo`] per layer in `0..effective`:
///   * resolution from `resolutions[layer_id]` (resolvable for ALL effective
///     layers, shed included, since each layer's tier is fixed),
///   * `bitrate_kbps` = the live value from `active_bitrates_kbps[layer_id]` for
///     ACTIVE layers (`layer_id < active`), or **`0` for SHED layers**
///     (`layer_id >= active`) — the UI lights up the dashed shed styling off the
///     `layer_id < active` boundary, so a zero bitrate is the shed marker.
///
/// Pure (no atomics / clock) so the shed-vs-active boundary is host-testable
/// without a live encoder. `active` is clamped to `[0, effective]` defensively.
///
/// `pub(crate)` so the screen encoder reuses the exact same shed logic.
pub(crate) fn build_simulcast_layers(
    effective: u32,
    active: u32,
    resolutions: &[(u32, u32)],
    active_bitrates_kbps: &[u32],
) -> Vec<SimulcastLayerInfo> {
    let active = active.min(effective);
    (0..effective)
        .map(|layer_id| {
            let (width, height) = resolutions
                .get(layer_id as usize)
                .copied()
                .unwrap_or((0, 0));
            // Active layers report their real targeted bitrate; shed layers
            // (layer_id >= active) report 0 — the shed marker for the UI.
            let bitrate_kbps = if layer_id < active {
                active_bitrates_kbps
                    .get(layer_id as usize)
                    .copied()
                    .unwrap_or(0)
            } else {
                0
            };
            SimulcastLayerInfo {
                layer_id,
                bitrate_kbps,
                width,
                height,
            }
        })
        .collect()
}

/// One simulcast layer's encoder and its per-layer mutable encode state
/// (issue #989). Local to `CameraEncoder::start`'s encode task.
///
/// **Closure ownership note:** the WebCodecs output and error callbacks are
/// `Closure`s whose backing `Box<dyn FnMut>` must outlive the `VideoEncoder`
/// that holds a JS reference to them. In the pre-simulcast code these were
/// loop-local bindings (`video_output_closure` / `video_error_handler`) kept
/// alive incidentally by staying in scope until the end of the `'restart`
/// iteration. With N layers we must store them per-layer, so they are moved
/// into `_output_closure` / `_error_closure` here; the leading underscore
/// documents that they are held purely to keep the JS callbacks alive (never
/// invoked directly from Rust). Dropping a `LayerEncoder` drops its encoder
/// first, then its closures — the correct teardown order.
struct LayerEncoder {
    /// This layer's WebCodecs `VideoEncoder`.
    encoder: Box<VideoEncoder>,
    /// The config object reused for in-place bitrate reconfiguration (mirrors
    /// the legacy single-encoder `video_encoder_config`).
    config: VideoEncoderConfig,
    /// Output-handler-owned sequence cell, read back after the encode loop to
    /// persist this layer's sequence across `'restart`.
    seq_out: Rc<std::cell::Cell<u64>>,
    /// This layer's simulcast id, stamped onto every emitted `PacketWrapper`.
    layer_id: u32,
    /// Current encoder width/height for this layer (dynamic reconfigure).
    current_w: u32,
    current_h: u32,
    /// Cached bitrate (bps) last applied to this layer's encoder.
    local_bitrate: u32,
    /// Kept alive so the JS output callback stays valid (see struct doc).
    _output_closure: Closure<dyn FnMut(JsValue)>,
    /// Kept alive so the JS error callback stays valid (see struct doc).
    _error_closure: Closure<dyn FnMut(JsValue)>,
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
    current_fps: Arc<AtomicU32>,
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
    shared_encoder_p75_peer_fps: Rc<AtomicU32>,
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
    /// User-configurable adaptive-quality tier bounds (issue #961). Written by
    /// the UI via [`Self::set_quality_tier_bounds`], read by the encoder control
    /// loop (which applies them live to the `EncoderBitrateController`) and on
    /// every encoder (re)start. See [`SharedQualityBounds`] for the apply
    /// mechanism and [`QualityTierBounds`] for the index↔quality inversion.
    quality_bounds: Rc<RefCell<SharedQualityBounds>>,
    /// Maximum number of simulcast layers this publisher is allowed to emit
    /// (issue #989). Computed in the UI from device capability + the
    /// `experimentalSimulcastMaxLayers` runtime flag and passed into the constructor.
    /// Clamped to [`SIMULCAST_MAX_SUPPORTED_LAYERS`] by [`effective_layer_count`].
    ///
    /// **PR A always passes 1**, so [`effective_layer_count`] returns 1 and the
    /// encode path is byte-identical to the pre-simulcast single-encoder path.
    /// N>1 wiring (per-layer tiers, AQ layer-drop) lands in PR B.
    max_layers: u32,
    /// Number of simulcast layers currently active (encoded + sent), written by
    /// the AQ control loop and read by the encode loop (issue #989, PR B).
    /// In single-stream mode (effective layers == 1) this stays 1 and the
    /// encode loop's per-layer gating is a no-op. The encode loop encodes only
    /// layers with `layer_id < active_layer_count`, so dropping the top layer
    /// cuts both egress and sender encode CPU.
    shared_active_layer_count: Rc<AtomicU32>,
    /// Per-layer target bitrate (bps), one atomic per ladder layer (lowest
    /// first, index == `layer_id`). Written by the AQ control loop in simulcast
    /// mode; read by the encode loop to reconfigure each layer's encoder. Empty
    /// in single-stream mode (the legacy `current_bitrate` atomic is used
    /// instead). Sized to `SIMULCAST_MAX_SUPPORTED_LAYERS` lazily on first use.
    shared_layer_bitrates_bps: Rc<RefCell<Vec<Rc<AtomicU32>>>>,
    /// Sender-side encoder backpressure (issue #1108, Phase B): the max
    /// `VideoEncoder::encode_queue_size()` across the ACTIVE layers, written by
    /// the encode loop each frame and read by the AQ control loop to feed
    /// [`EncoderBitrateController::observe_encoder_queue_depth`]. The encode loop
    /// owns the `VideoEncoder`s and the control loop owns the controller, so this
    /// atomic is the borrow-safe bridge between the two tasks — neither borrows
    /// the other's state. **Stage 1: the controller only stores this value (no
    /// shed/tier effect), so it is observability-only.**
    shared_encoder_queue_depth: Rc<AtomicU32>,
    /// Relay layer-union hint for this publisher's VIDEO ladder (issue #1108,
    /// Stage 3). The relay tracks the MAX simulcast layer ANY receiver currently
    /// wants for this (publisher, VIDEO) and delivers it on the publisher's own
    /// self-subject via a `LAYER_HINT` packet. `VideoCallClient`'s dispatch arm
    /// writes the received max-layer id here; the AQ control loop reads it each
    /// tick and feeds it to
    /// [`EncoderBitrateController::observe_union_requested_layer`], which caps the
    /// published ladder so the encoder stops spending CPU/uplink on a top layer
    /// no receiver wants.
    ///
    /// **Initialized to [`u32::MAX`] = fail-open (no cap):** until a hint arrives,
    /// the controller keeps its full backpressure-governed ladder. Reset back to
    /// `u32::MAX` on reconnect so a stale cap from the old relay cannot suppress
    /// against a freshly-allocated session on a new relay. Borrow-safe bridge
    /// (atomic) between the inbound-packet task and the control-loop task, exactly
    /// like `shared_encoder_queue_depth`.
    shared_union_requested_layer: Rc<AtomicU32>,
    /// Liveness token whose sole purpose is to bound the lifetime of the AQ
    /// control-loop `spawn_local` future (issue #1108). The encoder holds the
    /// only strong reference; `set_encoder_control` captures a [`Weak`] and
    /// breaks its 1 Hz `tick` loop as soon as `upgrade()` returns `None`. Because
    /// the control loop runs on `wasm_bindgen_futures::spawn_local` (NOT bound to
    /// the Dioxus component scope), it would otherwise run forever and pin its
    /// cloned `Rc` graph + `on_encoder_settings_update` callback across `Host`
    /// remounts. When this `CameraEncoder` is dropped on unmount, the strong
    /// count hits 0 and the loop exits cleanly — restoring the pre-#1108 lifetime
    /// where the loop ended when the diagnostics channel closed.
    control_loop_liveness: Rc<()>,
}

/// Upper bound on simulcast layers regardless of what the caller requests.
///
/// Tied directly to the AQ crate's `SIMULCAST_MAX_LAYERS` (the owner of the
/// simulcast tier ladder) so the encoder cap and the ladder size can never
/// silently diverge (issue #1077). Bumping the ladder size in `videocall-aq`
/// automatically raises this cap — no second edit, no doc-comment-only sync.
const SIMULCAST_MAX_SUPPORTED_LAYERS: u32 = videocall_aq::constants::SIMULCAST_MAX_LAYERS as u32;

/// Clamp a requested `max_layers` to the supported range.
///
/// `0` (meaningless — there is always at least the base layer) becomes 1, and
/// anything above [`SIMULCAST_MAX_SUPPORTED_LAYERS`] is capped. Free function so
/// it can be unit-tested without constructing a full `CameraEncoder` (which
/// needs a live `VideoCallClient`).
fn clamp_layer_count(max_layers: u32) -> u32 {
    max_layers.clamp(1, SIMULCAST_MAX_SUPPORTED_LAYERS)
}

/// Decide whether a just-encoded frame counts as "healthy" for the purpose of
/// resetting the encoder restart counter.
///
/// Health is anchored to the **base layer** (`layer_id == 0`), not to "any
/// layer encoded". Every receiver currently decodes only the base layer
/// (receiver default `selected_video_layer = 0`) and the relay does not yet
/// filter layers, so a frame in which the base layer failed — even if a higher
/// simulcast layer succeeded — is broken video for every viewer and must NOT
/// reset the restart counter (otherwise the encoder sits forever on a
/// non-fatally-failing base layer with no restart path; issue #989 review).
///
/// For N==1 the sole layer IS the base layer, so `base_ok` equals the old
/// "any layer ok" condition and this is a no-op vs. the pre-fix behavior.
///
/// Pure function so it can be unit-tested without a live camera / encoder.
#[inline]
fn frame_is_healthy(base_ok: bool) -> bool {
    base_ok
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
    /// * `max_layers` - the maximum number of simulcast layers to emit (issue
    ///   #989). The UI computes this from device capability and the
    ///   `experimentalSimulcastMaxLayers` runtime flag. Clamped to
    ///   [`SIMULCAST_MAX_SUPPORTED_LAYERS`]; `0` is treated as `1`. **PR A
    ///   always passes 1** (single layer, byte-identical to the legacy path).
    ///
    /// The encoder is created in a disabled state, [`encoder.set_enabled(true)`](Self::set_enabled) must be called before it can start encoding.
    /// The encoder is created without a camera selected, [`encoder.select(device_id)`](Self::select) must be called before it can start encoding.
    pub fn new(
        client: VideoCallClient,
        video_elem_id: &str,
        initial_bitrate: u32,
        on_encoder_settings_update: Callback<String>,
        on_error: Callback<String>,
        max_layers: u32,
    ) -> Self {
        let default_tier = &VIDEO_QUALITY_TIERS[0];
        let default_audio_tier = &AUDIO_QUALITY_TIERS[0];
        Self {
            client,
            video_elem_id: video_elem_id.to_string(),
            state: EncoderState::new(),
            current_bitrate: Rc::new(AtomicU32::new(initial_bitrate)),
            current_fps: Arc::new(AtomicU32::new(0)),
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
            shared_encoder_p75_peer_fps: Rc::new(AtomicU32::new(0)),
            shared_encoder_bitrate_ratio: Rc::new(AtomicU32::new(0)),
            shared_encoder_target_bitrate_kbps: Rc::new(AtomicU32::new(0)),
            shared_tier_transitions: Rc::new(RefCell::new(Vec::new())),
            shared_climb_limiter_snapshot: Rc::new(RefCell::new(ClimbLimiterSnapshot::default())),
            shared_dwell_samples: Rc::new(RefCell::new(Vec::new())),
            reelection_completed_signal: Rc::new(AtomicBool::new(false)),
            quality_bounds: Rc::new(RefCell::new(SharedQualityBounds::default())),
            max_layers,
            // Simulcast active-layer state (issue #989, PR B). Initialized to the
            // effective layer count so the encode loop knows how many layers to
            // build; the control loop adjusts it down/up under congestion.
            shared_active_layer_count: Rc::new(AtomicU32::new(clamp_layer_count(max_layers))),
            shared_layer_bitrates_bps: Rc::new(RefCell::new(Vec::new())),
            // Sender encoder backpressure (issue #1108, Phase B). Starts at 0
            // (no frames queued); the encode loop publishes the live depth.
            shared_encoder_queue_depth: Rc::new(AtomicU32::new(0)),
            // Relay layer-union hint (issue #1108, Stage 3). Starts at u32::MAX
            // (fail-open / no cap): the controller keeps its full ladder until a
            // LAYER_HINT arrives. Reset to u32::MAX on reconnect.
            shared_union_requested_layer: Rc::new(AtomicU32::new(u32::MAX)),
            // AQ control-loop liveness token (issue #1108). The encoder is the
            // sole strong owner; the self-tick loop holds a Weak and exits when
            // this drops (encoder torn down on Host unmount).
            control_loop_liveness: Rc::new(()),
        }
    }

    /// Effective number of simulcast layers to encode this session.
    ///
    /// Clamps the caller-supplied `max_layers` to at least 1 (a `0` request is
    /// meaningless — there is always at least the base layer) and at most
    /// [`SIMULCAST_MAX_SUPPORTED_LAYERS`]. In PR A the caller always passes 1,
    /// so this returns 1 and the encode loop runs a single layer exactly as
    /// before.
    fn effective_layer_count(&self) -> u32 {
        clamp_layer_count(self.max_layers)
    }

    /// Spawn the encoder AQ control loop (issue #1108: now a self-timer).
    ///
    /// Receiver-reported FPS no longer drives the sender AQ, so this loop is no
    /// longer fed by a diagnostics channel. It ticks at `AQ_TICK_INTERVAL_MS`,
    /// reading the sender's own encoder-queue backpressure (published by the
    /// encode loop into `shared_encoder_queue_depth`) plus the server-CONGESTION
    /// and WS-send-buffer signals, and applies tier/layer/bitrate decisions.
    pub fn set_encoder_control(&mut self) {
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
        let shared_encoder_p75_peer_fps = self.shared_encoder_p75_peer_fps.clone();
        let shared_encoder_bitrate_ratio = self.shared_encoder_bitrate_ratio.clone();
        let shared_encoder_target_bitrate_kbps = self.shared_encoder_target_bitrate_kbps.clone();
        let shared_tier_transitions = self.shared_tier_transitions.clone();
        let shared_climb_limiter_snapshot = self.shared_climb_limiter_snapshot.clone();
        let shared_dwell_samples = self.shared_dwell_samples.clone();
        let reelection_completed_signal = self.reelection_completed_signal.clone();
        // #961 (send quality bounds) + #1082 (simulcast layers) both feed the
        // encoder control loop — clone both sides' shared state.
        let quality_bounds = self.quality_bounds.clone();
        let n_layers = self.effective_layer_count() as usize;
        let shared_active_layer_count = self.shared_active_layer_count.clone();
        let shared_layer_bitrates_bps = self.shared_layer_bitrates_bps.clone();
        // Sender encoder backpressure (issue #1108, Phase B): the control loop
        // READS the depth the encode loop published and forwards it to the
        // controller on each self-timer tick.
        let shared_encoder_queue_depth = self.shared_encoder_queue_depth.clone();
        // Relay layer-union hint (issue #1108, Stage 3): the control loop READS
        // the max-layer the client wrote (from a LAYER_HINT packet) and forwards
        // it to the controller's union cap each tick.
        let shared_union_requested_layer = self.shared_union_requested_layer.clone();
        // Liveness sentinel (issue #1108): a Weak to the encoder-owned token.
        // The loop below breaks as soon as this fails to upgrade, i.e. when the
        // CameraEncoder is dropped (Host unmount). Without this, the
        // `spawn_local` future is immortal and leaks per remount.
        let control_loop_liveness = Rc::downgrade(&self.control_loop_liveness);
        wasm_bindgen_futures::spawn_local(async move {
            let mut encoder_control = EncoderBitrateController::new(
                current_bitrate.load(Ordering::Relaxed),
                current_fps.clone(),
            );
            // Apply any user quality bounds set before the encoder started, and
            // track the generation we last applied so the loop only re-applies
            // when the UI actually changes them (issue #961).
            let mut applied_bounds_generation = {
                let shared = quality_bounds.borrow();
                encoder_control
                    .set_video_quality_bounds(shared.bounds.video_best, shared.bounds.video_worst);
                encoder_control
                    .set_audio_quality_bounds(shared.bounds.audio_best, shared.bounds.audio_worst);
                shared.generation
            };

            // Enable simulcast on the controller when the effective layer count
            // is > 1 (issue #989, PR B). n_layers == 1 leaves the controller in
            // single-stream mode (no-op) — byte-identical to the legacy path.
            if n_layers > 1 {
                encoder_control.set_simulcast_layers(n_layers);
                // Pre-size the per-layer bitrate atomics (lowest layer first).
                let mut atomics = shared_layer_bitrates_bps.borrow_mut();
                if atomics.len() != n_layers {
                    *atomics = (0..n_layers).map(|_| Rc::new(AtomicU32::new(0))).collect();
                }
            }
            let mut prev_screen_active = false;
            let mut last_ws_drop_snapshot: u64 =
                videocall_transport::websocket::websocket_drop_count();
            let mut ws_drop_window_start_ms: f64 = js_sys::Date::now();
            // Self-timer AQ loop (issue #1108): tick at AQ_TICK_INTERVAL_MS
            // instead of waiting on receiver diagnostics. Runs for the lifetime
            // of the owning CameraEncoder: this `spawn_local` future is NOT bound
            // to the Dioxus component scope, so it must break itself when the
            // encoder is torn down (Host unmount) — otherwise it would tick
            // forever, pinning its cloned Rc graph and firing into a stale
            // `on_encoder_settings_update` callback, leaking one loop per remount.
            // The `control_loop_liveness` Weak fails to upgrade once the encoder
            // (sole strong owner of the token) is dropped, which is our exit. The
            // `enabled` flag does NOT terminate the loop — it only gates the
            // bitrate-vs-"Disabled" emit below, so a muted-then-unmuted camera
            // keeps adapting without re-arming.
            loop {
                gloo_timers::future::sleep(std::time::Duration::from_millis(
                    crate::adaptive_quality_constants::AQ_TICK_INTERVAL_MS,
                ))
                .await;
                // Encoder torn down? Stop ticking and let the future complete so
                // its captured Rc graph is released.
                if control_loop_liveness.upgrade().is_none() {
                    log::debug!("CameraEncoder: AQ control loop exiting (encoder dropped)");
                    break;
                }
                let now = js_sys::Date::now();
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

                // Apply user-configurable quality bounds if the UI changed them
                // since we last applied (issue #961). Cheap generation check
                // avoids touching the controller every tick; the actual snap-
                // into-range happens inside the controller and surfaces via
                // take_tier_changed() below.
                {
                    let shared = quality_bounds.borrow();
                    if shared.generation != applied_bounds_generation {
                        applied_bounds_generation = shared.generation;
                        let b = shared.bounds;
                        drop(shared);
                        encoder_control.set_video_quality_bounds(b.video_best, b.video_worst);
                        encoder_control.set_audio_quality_bounds(b.audio_best, b.audio_worst);
                        log::info!(
                            "CameraEncoder: applied user quality bounds video(best={:?},worst={:?}) \
                             audio(best={:?},worst={:?})",
                            b.video_best,
                            b.video_worst,
                            b.audio_best,
                            b.audio_worst,
                        );
                    }
                }

                // Check for server congestion step-down request before
                // processing the diagnostics packet so the forced step-down
                // takes effect immediately.
                if congestion_flag.swap(false, Ordering::AcqRel) {
                    log::warn!(
                        "CameraEncoder: server CONGESTION signal received, forcing aggressive congestion cut"
                    );
                    encoder_control.force_congestion_cut();
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
                    let elapsed_ms = now - ws_drop_window_start_ms;

                    if elapsed_ms >= crate::adaptive_quality_constants::WS_SELF_CONGESTION_WINDOW_MS
                    {
                        let delta = current_ws_drops.saturating_sub(last_ws_drop_snapshot);
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
                        ws_drop_window_start_ms = now;
                    }
                }

                // Sender encoder backpressure (issue #1108). Feed the depth the
                // encode loop published into the controller, then advance the AQ
                // one tick. This is the SOLE gradual quality axis now: receiver
                // FPS no longer reaches the sender AQ. The native bot feeds 0 in
                // its own loop; here we forward the live WebCodecs queue depth.
                encoder_control.observe_encoder_queue_depth(
                    shared_encoder_queue_depth.load(Ordering::Relaxed),
                );
                // Relay layer-union hint (issue #1108, Stage 3): feed the latest
                // max-requested-layer the client wrote (u32::MAX = fail-open / no
                // cap) so the controller caps the published ladder to what some
                // receiver actually wants. Applied right before `tick` so the cap
                // composes with the just-observed backpressure decision.
                encoder_control.observe_union_requested_layer(
                    shared_union_requested_layer.load(Ordering::Relaxed),
                );
                encoder_control.tick(now);
                let output_wasted = Some(encoder_control.last_target_bitrate_kbps());

                // Write encoder decision inputs to shared atomics for health
                // reporting. Issue #1108: the receiver-FPS-derived signals are
                // gone — `last_fps_ratio()` / `last_bitrate_ratio()` now return
                // NaN (the health reporter's is_finite() guard drops those proto
                // fields), and `last_p75_peer_fps()` is REPOINTED to carry the new
                // sender backpressure signal (encoder queue depth) so the existing
                // host telemetry channel surfaces it with no proto/Grafana churn.
                shared_encoder_fps_ratio.store(
                    (encoder_control.last_fps_ratio() as f32).to_bits(),
                    Ordering::Relaxed,
                );
                shared_encoder_p75_peer_fps.store(
                    (encoder_control.last_p75_peer_fps() as f32).to_bits(),
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
                    shared_audio_tier_idx
                        .store(encoder_control.audio_tier_index() as u32, Ordering::Relaxed);
                    shared_audio_bitrate.store(audio_tier.bitrate_kbps * 1000, Ordering::Relaxed);
                    shared_audio_fec.store(audio_tier.enable_fec, Ordering::Relaxed);
                    log::info!(
                        "CameraEncoder: audio tier updated to '{}' ({}kbps, fec={})",
                        audio_tier.label,
                        audio_tier.bitrate_kbps,
                        audio_tier.enable_fec,
                    );
                }

                // Simulcast (issue #989, PR B): publish the active-layer count
                // and per-layer target bitrates to the encode loop every tick.
                // In single-stream mode (is_simulcast() == false) this is
                // skipped entirely, so behavior is byte-identical to before.
                if encoder_control.is_simulcast() {
                    let active = encoder_control.active_layer_count() as u32;
                    shared_active_layer_count.store(active, Ordering::Relaxed);
                    let per_layer = encoder_control.layer_target_bitrates_kbps();
                    let atomics = shared_layer_bitrates_bps.borrow();
                    for (i, atomic) in atomics.iter().enumerate() {
                        if let Some(&kbps) = per_layer.get(i) {
                            // kbps (f64) -> bps (u32), matching the encoder's
                            // set_bitrate(bps) expectation elsewhere.
                            atomic.store((kbps * 1000.0) as u32, Ordering::Relaxed);
                        }
                    }
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
    pub fn shared_encoder_output_fps(&self) -> Arc<AtomicU32> {
        self.current_fps.clone()
    }

    /// Real-time adaptive-quality snapshot for the UI VU meter (issue #961).
    ///
    /// Resolves the live shared atomics (`shared_video_tier_index`,
    /// `shared_audio_tier_index`, `shared_encoder_target_bitrate_kbps`) against
    /// the AQ tier tables and returns a [`LiveQualitySnapshot`] with the current
    /// video resolution / fps / ideal-kbps, audio kbps, and the live PID target
    /// bitrate. Indices are clamped to valid table bounds, so this never panics
    /// even mid-transition. Call it on the UI's render/poll tick.
    pub fn live_quality_snapshot(&self) -> LiveQualitySnapshot {
        let v_idx = (self.shared_video_tier_index.load(Ordering::Relaxed) as usize)
            .min(VIDEO_QUALITY_TIERS.len().saturating_sub(1));
        let a_idx = (self.shared_audio_tier_index.load(Ordering::Relaxed) as usize)
            .min(AUDIO_QUALITY_TIERS.len().saturating_sub(1));
        let v = &VIDEO_QUALITY_TIERS[v_idx];
        let a = &AUDIO_QUALITY_TIERS[a_idx];
        let target_bitrate_kbps = f32::from_bits(
            self.shared_encoder_target_bitrate_kbps
                .load(Ordering::Relaxed),
        );
        LiveQualitySnapshot {
            video_tier_index: v_idx,
            video_width: v.max_width,
            video_height: v.max_height,
            video_fps: v.target_fps,
            video_ideal_kbps: v.ideal_bitrate_kbps,
            audio_tier_index: a_idx,
            audio_kbps: a.bitrate_kbps,
            target_bitrate_kbps,
        }
    }

    /// Live SEND-side simulcast diagnostics for the camera (issue #1095
    /// observability). Reads the active-layer count + per-layer target-bitrate
    /// atomics published by the AQ control loop, and resolves EVERY effective
    /// layer's fixed resolution from the SIMULCAST ladder. Panic-safe (indices
    /// clamped); cheap to poll at the needle cadence.
    ///
    /// Emits one rung per EFFECTIVE layer (the configured ladder depth), not just
    /// the active ones, so a layer the AQ has SHED under congestion stays visible
    /// (with `bitrate_kbps == 0`) instead of the ladder silently shrinking. The
    /// `active_layers` field is the active-vs-shed boundary.
    ///
    /// In single-stream mode (effective layers == 1) this returns
    /// `simulcast_active = false` with an empty `layers` Vec.
    pub fn live_simulcast_snapshot(&self) -> SimulcastSendSnapshot {
        let effective = self.effective_layer_count();
        if effective <= 1 {
            return SimulcastSendSnapshot {
                simulcast_active: false,
                effective_layers: effective,
                active_layers: 1,
                layers: Vec::new(),
            };
        }
        // Fixed per-layer resolutions for the FULL ladder (lowest layer first) —
        // resolvable for every effective layer, shed included.
        let resolutions: Vec<(u32, u32)> = simulcast_layers(effective as usize)
            .iter()
            .map(|t| (t.max_width, t.max_height))
            .collect();
        // Active layer count is shed-aware (the AQ loop drops the top layer under
        // congestion); clamp it to the ladder size defensively.
        let active = (self.shared_active_layer_count.load(Ordering::Relaxed))
            .min(effective)
            .max(1);
        // Live targeted bitrates (kbps) for the active layers, from the atomics.
        let active_bitrates_kbps: Vec<u32> = {
            let bitrate_atomics = self.shared_layer_bitrates_bps.borrow();
            (0..active)
                .map(|layer_id| {
                    bitrate_atomics
                        .get(layer_id as usize)
                        .map(|a| a.load(Ordering::Relaxed) / 1000) // bps -> kbps
                        .unwrap_or(0)
                })
                .collect()
        };
        let layers = build_simulcast_layers(effective, active, &resolutions, &active_bitrates_kbps);
        SimulcastSendSnapshot {
            simulcast_active: true,
            effective_layers: effective,
            active_layers: active,
            layers,
        }
    }

    /// Returns the encoder fps_ratio atomic (f32 bits).
    pub fn shared_encoder_fps_ratio(&self) -> Rc<AtomicU32> {
        self.shared_encoder_fps_ratio.clone()
    }

    /// Returns the encoder worst peer FPS atomic (f32 bits).
    pub fn shared_encoder_p75_peer_fps(&self) -> Rc<AtomicU32> {
        self.shared_encoder_p75_peer_fps.clone()
    }

    /// Returns the encoder bitrate_ratio atomic (f32 bits).
    pub fn shared_encoder_bitrate_ratio(&self) -> Rc<AtomicU32> {
        self.shared_encoder_bitrate_ratio.clone()
    }

    /// Returns the encoder target bitrate kbps atomic (f32 bits).
    pub fn shared_encoder_target_bitrate_kbps(&self) -> Rc<AtomicU32> {
        self.shared_encoder_target_bitrate_kbps.clone()
    }

    /// Returns the relay layer-union hint atomic for this VIDEO ladder (issue
    /// #1108, Stage 3).
    ///
    /// `VideoCallClient` stores this clone (via
    /// [`VideoCallClient::set_camera_union_requested_layer`](crate::VideoCallClient::set_camera_union_requested_layer))
    /// and writes the MAX-requested-layer carried by an inbound `LAYER_HINT`
    /// packet into it. The encoder's AQ control loop reads it each tick to cap the
    /// published ladder. The value is a max-layer **id** (`u32::MAX` = fail-open /
    /// no cap); the controller converts it to an active-layer count.
    pub fn shared_union_requested_layer(&self) -> Rc<AtomicU32> {
        self.shared_union_requested_layer.clone()
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

    /// Set user-configurable adaptive-quality tier bounds (issue #961).
    ///
    /// This is the public API the Dioxus Performance settings panel calls. The
    /// arguments are **tier indices** into `VIDEO_QUALITY_TIERS` /
    /// `AUDIO_QUALITY_TIERS`.
    ///
    /// **QUALITY IS THE INVERSE OF INDEX — index 0 is the BEST tier.** So:
    /// - `video_best` / `audio_best` = the user's **max quality** = the *best*
    ///   tier allowed = a **FLOOR on the index** (adaptation never steps UP past
    ///   it, i.e. never picks a smaller index / higher quality).
    /// - `video_worst` / `audio_worst` = the user's **min quality** = the *worst*
    ///   tier allowed = a **CAP on the index** (adaptation never steps DOWN past
    ///   it, i.e. never picks a larger index / lower quality).
    /// - `None` on any end = "Auto" (no user bound on that end). Passing all
    ///   `None` restores fully-automatic behaviour.
    /// - When `best == worst` the tier is pinned to that single index.
    ///
    /// The UI is responsible for mapping its resolution / bitrate labels to
    /// indices (e.g. "1080p" → 0, "240p" → 7 for video).
    ///
    /// Bounds are applied live to the running encoder at the next diagnostics
    /// tick (≤1s) AND stored so they are re-applied on every encoder (re)start,
    /// so the call is valid whether or not the encoder is currently running. Out-
    /// of-range or inverted ranges are clamped/normalized inside the AQ manager.
    pub fn set_quality_tier_bounds(
        &mut self,
        video_best: Option<usize>,
        video_worst: Option<usize>,
        audio_best: Option<usize>,
        audio_worst: Option<usize>,
    ) {
        let mut shared = self.quality_bounds.borrow_mut();
        shared.bounds = QualityTierBounds {
            video_best,
            video_worst,
            audio_best,
            audio_worst,
        };
        shared.generation = shared.generation.wrapping_add(1);
    }

    /// Returns the current user-configured quality tier bounds (issue #961).
    pub fn quality_tier_bounds(&self) -> QualityTierBounds {
        self.quality_bounds.borrow().bounds
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
        // Number of simulcast layers to encode this session (issue #989).
        // n_layers == 1 → single encoder, byte-identical to the legacy path
        // (adaptive single-stream resolution preserved). n_layers > 1 → each
        // layer encodes at a FIXED SIMULCAST_LAYER_TIERS resolution + adaptive
        // per-layer bitrate, and the AQ controller sheds the top active layer
        // under congestion.
        let n_layers = self.effective_layer_count() as usize;
        let simulcast = n_layers > 1;
        let shared_active_layer_count = self.shared_active_layer_count.clone();
        let shared_layer_bitrates_bps = self.shared_layer_bitrates_bps.clone();
        // Sender encoder backpressure (issue #1108, Phase B): the encode loop
        // WRITES the max active-layer encode_queue_size() here each frame.
        let shared_encoder_queue_depth = self.shared_encoder_queue_depth.clone();
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

            // Per-layer sequence numbers persist across restarts so the
            // receiving side never sees duplicate or regressed sequence numbers.
            // Each simulcast layer (issue #989) carries its own monotonic
            // counter: a receiver decoding only one layer must see a dense
            // 0,1,2,… stream or its `SequenceTracker` reports phantom loss
            // (~(N-1)/N) and storms PLIs. PR A has n_layers == 1 so this is a
            // single-element Vec behaving exactly like the old scalar.
            let mut sequence_numbers: Vec<u64> = vec![0; n_layers];

            let mut restart_count: u32 = 0;
            // Maximum restart attempts before surfacing on_error. Sized for the
            // narrow fatal signatures matched by is_fatal_encoder_error_message:
            // the closed-codec InvalidStateError and the VPX allocation failure.
            // Those usually clear within 1-2 retries; 5 gives headroom for a
            // short cascade without spinning forever if the browser is wedged.
            // Revisit this cap if the fatal-error classifier is broadened.
            const MAX_RESTARTS: u32 = 5;

            // Last active-layer count we logged a transition for. Declared
            // OUTSIDE `'restart` so it persists across encoder restart cycles:
            // seeding it per-restart from `n_layers` would fake a phantom
            // `n_layers->current` transition on the first frame after any
            // restart that happens once the controller has already shed layers.
            // Seeded from the shared atomic's CURRENT value so the first frame
            // logs only a real change. The encode loop refreshes and compares it
            // each frame, emitting ONE line only when the count actually changes.
            let mut prev_active_layers: usize =
                shared_active_layer_count.load(Ordering::Relaxed) as usize;

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

                // Get track settings to get actual width and height up front so
                // every layer can be constructed with the native dimensions
                // (PR A) — per-layer downscaled tiers land in PR B.
                let media_track = video_track
                    .as_ref()
                    .clone()
                    .unchecked_into::<MediaStreamTrack>();
                let track_settings = media_track.get_settings();

                let width = track_settings.get_width().expect("width is None");
                let height = track_settings.get_height().expect("height is None");

                // --- Setup video encoders (one per simulcast layer) ---
                // The output and error handler closures must be re-created on
                // each restart because Closure::wrap consumes them and the new
                // VideoEncoder needs fresh JS function references. Each layer
                // owns its own output closure (own seq counter + reused buffer),
                // its own error closure, and its own config object. The closures
                // are stored in the LayerEncoder so they outlive the encoder.
                //
                // PR A: n_layers == 1, so this loop runs once and produces a
                // single layer at the native resolution with layer_id 0 —
                // byte-identical to the legacy single-encoder path.
                let mut layers: Vec<LayerEncoder> = Vec::with_capacity(n_layers);
                // `sequence_numbers` has exactly `n_layers` elements (vec![0;
                // n_layers]), so enumerating it yields one (idx, persisted-seq)
                // pair per layer.
                for (layer_idx, &initial_seq) in sequence_numbers.iter().enumerate() {
                    let layer_id = layer_idx as u32;

                    let (video_output_box, seq_out) = {
                        let client = client.clone();
                        let userid = userid.clone();
                        let aes = aes.clone();
                        let current_fps = current_fps.clone();
                        let mut buffer: Vec<u8> = Vec::with_capacity(100_000);
                        // Capture this layer's current sequence by value; we read
                        // the updated value back after the encode loop exits.
                        let mut local_seq = initial_seq;
                        let seq_out = Rc::new(std::cell::Cell::new(initial_seq));
                        let seq_out_inner = seq_out.clone();
                        let mut last_chunk_time = window().performance().unwrap().now();
                        let mut chunks_in_last_second = 0;

                        (
                            Box::new(move |chunk: JsValue| {
                                let now = window().performance().unwrap().now();
                                let chunk = web_sys::EncodedVideoChunk::from(chunk);

                                // FPS calculation: ONLY layer 0 updates the
                                // shared `current_fps`. That atomic is the AQ
                                // controller's setpoint (encoder output fps);
                                // summing N layers would inflate it N× and
                                // corrupt every PID/tier decision. Higher layers
                                // still encode and send, they just don't touch
                                // the setpoint.
                                if layer_id == 0 {
                                    chunks_in_last_second += 1;
                                    if now - last_chunk_time >= 1000.0 {
                                        let fps = chunks_in_last_second;
                                        current_fps.store(fps, Ordering::Relaxed);
                                        // PER-TICK telemetry: fires ~1 Hz while
                                        // encoding (layer 0). Demoted debug!->trace!
                                        // so it stays off even when console-log
                                        // collection bumps to Debug (#1100 follow-up).
                                        log::trace!("Encoder output FPS: {fps}");
                                        chunks_in_last_second = 0;
                                        last_chunk_time = now;
                                    }
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
                                    layer_id,
                                );
                                // Phase 2 of WT freeze fix: route camera video on
                                // its dedicated persistent QUIC stream so a stall
                                // on a video keyframe never blocks audio.
                                client.send_media_packet(packet, MediaStreamKey::Video);
                                local_seq += 1;
                                seq_out_inner.set(local_seq);
                            }) as Box<dyn FnMut(JsValue)>,
                            seq_out,
                        )
                    };

                    let error_closure = Closure::wrap(Box::new(move |e: JsValue| {
                        error!("error_handler error (layer {layer_id}) {e:?}");
                    })
                        as Box<dyn FnMut(JsValue)>);

                    let output_closure = Closure::wrap(video_output_box);

                    let video_encoder_init = VideoEncoderInit::new(
                        error_closure.as_ref().unchecked_ref(),
                        output_closure.as_ref().unchecked_ref(),
                    );

                    let video_encoder = match VideoEncoder::new(&video_encoder_init) {
                        Ok(enc) => Box::new(enc),
                        Err(e) => {
                            let msg =
                                format!("Failed to create video encoder (layer {layer_id}): {e:?}");
                            error!("{msg}");
                            // Close any already-built layer encoders before retry.
                            for built in &layers {
                                let _ = built.encoder.close();
                            }
                            stop_media_stream_tracks(&device);
                            if let Some(cb) = &on_error {
                                cb.emit(msg);
                            }
                            restart_count += 1;
                            continue 'restart;
                        }
                    };

                    // Resolution + initial bitrate per layer:
                    //  - single-stream (n_layers == 1): native camera resolution
                    //    and the shared adaptive bitrate — the legacy path, with
                    //    tier-resolution stepping preserved (see encode loop).
                    //  - simulcast (n_layers > 1): each layer encodes at its
                    //    FIXED SIMULCAST_LAYER_TIERS resolution (issue #989, PR B)
                    //    and its own initial bitrate (tier ideal). Resolution is
                    //    fixed for simulcast layers; only the bitrate adapts.
                    let (layer_w, layer_h, init_bitrate_bps) = if simulcast {
                        let tiers = simulcast_layers(n_layers);
                        let tier = &tiers[layer_idx];
                        (
                            tier.max_width,
                            tier.max_height,
                            tier.ideal_bitrate_kbps as f64 * 1000.0,
                        )
                    } else {
                        (
                            width as u32,
                            height as u32,
                            current_bitrate.load(Ordering::Relaxed) as f64 * 1000.0,
                        )
                    };
                    let config =
                        VideoEncoderConfig::new(get_video_codec_string(), layer_h, layer_w);
                    config.set_bitrate(init_bitrate_bps);
                    config.set_latency_mode(LatencyMode::Realtime);

                    if let Err(e) = video_encoder.configure(&config) {
                        CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL.fetch_add(1, Ordering::Relaxed);
                        if is_fatal_encoder_error(&e) {
                            error!("CameraEncoder: fatal configure error before encode loop (layer {layer_id}), restarting: {e:?}");
                            let _ = video_encoder.close();
                            for built in &layers {
                                let _ = built.encoder.close();
                            }
                            stop_media_stream_tracks(&device);
                            restart_count += 1;
                            continue 'restart;
                        }
                        error!("Error configuring video encoder (layer {layer_id}): {e:?}");
                    }

                    layers.push(LayerEncoder {
                        encoder: video_encoder,
                        config,
                        seq_out,
                        layer_id,
                        current_w: layer_w,
                        current_h: layer_h,
                        local_bitrate: init_bitrate_bps as u32,
                        _output_closure: output_closure,
                        _error_closure: error_closure,
                    });
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

                // Per-encoder bitrate and dimensions now live in each
                // `LayerEncoder` (local_bitrate / current_w / current_h) so each
                // layer reconfigures independently. The tier-controlled caches
                // below are shared across layers because they are driven by the
                // single shared tier atomics (the AQ controller is per-publisher
                // in PR A; per-layer AQ lands in PR B).

                // Cache tier-controlled values (shared across layers).
                let mut local_keyframe_interval = tier_keyframe_interval.load(Ordering::Relaxed);
                let mut local_tier_max_width = tier_max_width.load(Ordering::Relaxed);
                let mut local_tier_max_height = tier_max_height.load(Ordering::Relaxed);
                // Simulcast: how many layers are currently active (encoded+sent).
                // In single-stream mode this is always 1 and gates nothing.
                // Refreshed from the shared atomic at the top of every frame.
                let mut local_active_layers: usize;

                // Track whether we have successfully encoded at least one frame
                // in this restart cycle. Used to reset restart_count on success.
                let mut encoded_ok_this_cycle = false;

                'encode: loop {
                    if !enabled.load(Ordering::Acquire) || switching.load(Ordering::Acquire) {
                        switching.store(false, Ordering::Release);
                        let video_track = video_track.clone().unchecked_into::<MediaStreamTrack>();
                        video_track.stop();
                        log::info!("CameraEncoder: stopped");
                        // Close every layer's encoder.
                        for layer in &layers {
                            if let Err(e) = layer.encoder.close() {
                                error!(
                                    "Error closing video encoder (layer {}): {e:?}",
                                    layer.layer_id
                                );
                            }
                        }
                        return;
                    }

                    // --- Guard: check if any encoder has been closed externally ---
                    // This can happen if the browser closes the codec (e.g. due to
                    // GPU process crash, OOM, or an error callback we didn't intercept).
                    // If ANY layer's encoder is closed we restart all layers (per-layer
                    // restart is a future optimization, see plan PR B).
                    if layers
                        .iter()
                        .any(|l| l.encoder.state() == CodecState::Closed)
                    {
                        log::warn!("CameraEncoder: an encoder state is Closed, triggering restart");
                        restart_count += 1;
                        break 'encode;
                    }

                    // Read the shared tier/keyframe atomics ONCE per frame. The
                    // keyframe interval is shared across all layers in both modes.
                    let new_kf = tier_keyframe_interval.load(Ordering::Relaxed);
                    if new_kf != local_keyframe_interval {
                        local_keyframe_interval = new_kf;
                        log::info!(
                            "CameraEncoder: keyframe interval changed to {}",
                            local_keyframe_interval
                        );
                    }
                    // Refresh the active-layer count each frame (simulcast only).
                    // The event-driven transition log is emitted AFTER the
                    // per-layer reconfigure pass below, so the reported
                    // `local_bitrate` reflects the bitrate just applied this tick.
                    local_active_layers =
                        shared_active_layer_count.load(Ordering::Relaxed) as usize;

                    // Single-stream tier dims + shared bitrate (only meaningful
                    // when NOT simulcast — the adaptive single-stream resolution
                    // path is preserved verbatim for n_layers == 1).
                    let new_tier_w = tier_max_width.load(Ordering::Relaxed);
                    let new_tier_h = tier_max_height.load(Ordering::Relaxed);
                    let new_current_bitrate = current_bitrate.load(Ordering::Relaxed) * 1000;
                    let tier_dims_changed = !simulcast
                        && (new_tier_w != local_tier_max_width
                            || new_tier_h != local_tier_max_height);
                    if tier_dims_changed {
                        local_tier_max_width = new_tier_w;
                        local_tier_max_height = new_tier_h;
                    }

                    // Per-layer reconfiguration.
                    //
                    //  - Single-stream (n_layers == 1): the legacy logic —
                    //    tier-resolution stepping (tier dims) + shared adaptive
                    //    bitrate, applied verbatim. N=1 behavior is unchanged.
                    //  - Simulcast (n_layers > 1, issue #989 PR B): RESOLUTION IS
                    //    FIXED per layer (set at construction from
                    //    SIMULCAST_LAYER_TIERS), so tier-resolution stepping is
                    //    retired here; only the per-layer adaptive bitrate is
                    //    reconfigured. Layers with layer_id >= active count are
                    //    skipped entirely (not reconfigured, not encoded) so a
                    //    dropped top layer costs no encode CPU.
                    let mut fatal_reconfigure = false;
                    for layer in layers.iter_mut() {
                        // Simulcast: skip inactive (shed) top layers entirely.
                        if simulcast && (layer.layer_id as usize) >= local_active_layers {
                            continue;
                        }

                        if simulcast {
                            // Per-layer adaptive bitrate (fixed resolution).
                            let new_layer_bitrate = {
                                let atomics = shared_layer_bitrates_bps.borrow();
                                atomics
                                    .get(layer.layer_id as usize)
                                    .map(|a| a.load(Ordering::Relaxed))
                                    .unwrap_or(0)
                            };
                            if new_layer_bitrate > 0 && new_layer_bitrate != layer.local_bitrate {
                                if layer.encoder.state() == CodecState::Closed {
                                    log::warn!("CameraEncoder: encoder closed before per-layer bitrate reconfigure (layer {})", layer.layer_id);
                                    fatal_reconfigure = true;
                                    break;
                                }
                                layer.local_bitrate = new_layer_bitrate;
                                layer.config.set_bitrate(layer.local_bitrate as f64);
                                if let Err(e) = layer.encoder.configure(&layer.config) {
                                    CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL
                                        .fetch_add(1, Ordering::Relaxed);
                                    if is_fatal_encoder_error(&e) {
                                        error!("CameraEncoder: fatal configure error (layer {}), restarting: {e:?}", layer.layer_id);
                                        fatal_reconfigure = true;
                                        break;
                                    }
                                    error!(
                                        "Error configuring video encoder (layer {}): {e:?}",
                                        layer.layer_id
                                    );
                                }
                            }
                            continue;
                        }

                        // --- Single-stream legacy path (n_layers == 1) ---
                        if tier_dims_changed {
                            // Guard: do not configure a closed encoder.
                            if layer.encoder.state() == CodecState::Closed {
                                log::warn!("CameraEncoder: encoder closed before tier reconfigure (layer {})", layer.layer_id);
                                fatal_reconfigure = true;
                                break;
                            }

                            // Constrain current encoder dimensions to the tier
                            // max, preserving the source aspect ratio (#1037).
                            //
                            // `layer.current_w/current_h` carry the source
                            // aspect (seeded from native track dims and only
                            // ever updated via this same uniform fit or from a
                            // raw VideoFrame's native dims on the per-frame path
                            // below), so using them as the "source" keeps the
                            // ratio intact while the tier ceiling tightens. A
                            // per-axis `.min()` here would stretch/squash
                            // whenever the source aspect (e.g. a 4:3 webcam)
                            // differs from the 16:9 tier ceiling. For N==1 this
                            // produces the same dims the legacy single encoder
                            // computed via `fit_within_preserving_aspect`.
                            let (constrained_w, constrained_h) = fit_within_preserving_aspect(
                                layer.current_w,
                                layer.current_h,
                                local_tier_max_width,
                                local_tier_max_height,
                            );

                            log::info!(
                                "CameraEncoder: tier dimension change -> {}x{} (was {}x{}) (layer {})",
                                constrained_w,
                                constrained_h,
                                layer.current_w,
                                layer.current_h,
                                layer.layer_id,
                            );
                            layer.current_w = constrained_w;
                            layer.current_h = constrained_h;

                            let new_config = VideoEncoderConfig::new(
                                get_video_codec_string(),
                                layer.current_h,
                                layer.current_w,
                            );
                            new_config.set_bitrate(layer.local_bitrate as f64);
                            new_config.set_latency_mode(LatencyMode::Realtime);
                            if let Err(e) = layer.encoder.configure(&new_config) {
                                CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL
                                    .fetch_add(1, Ordering::Relaxed);
                                if is_fatal_encoder_error(&e) {
                                    error!("CameraEncoder: fatal configure error (layer {}), restarting: {e:?}", layer.layer_id);
                                    fatal_reconfigure = true;
                                    break;
                                }
                                error!("Error reconfiguring camera encoder for tier change (layer {}): {e:?}", layer.layer_id);
                            }
                        }

                        // Update the bitrate if it changed (and dims did not also
                        // change above — a dim change already applied the new
                        // bitrate via the fresh config).
                        if new_current_bitrate != layer.local_bitrate && !tier_dims_changed {
                            // Guard: do not configure a closed encoder.
                            if layer.encoder.state() == CodecState::Closed {
                                log::warn!("CameraEncoder: encoder closed before bitrate reconfigure (layer {})", layer.layer_id);
                                fatal_reconfigure = true;
                                break;
                            }
                            log::info!(
                                "Updating video bitrate to {new_current_bitrate} (layer {})",
                                layer.layer_id
                            );
                            layer.local_bitrate = new_current_bitrate;
                            layer.config.set_bitrate(layer.local_bitrate as f64);
                            if let Err(e) = layer.encoder.configure(&layer.config) {
                                CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL
                                    .fetch_add(1, Ordering::Relaxed);
                                if is_fatal_encoder_error(&e) {
                                    error!("CameraEncoder: fatal configure error (layer {}), restarting: {e:?}", layer.layer_id);
                                    fatal_reconfigure = true;
                                    break;
                                }
                                error!(
                                    "Error configuring video encoder (layer {}): {e:?}",
                                    layer.layer_id
                                );
                            }
                        } else if new_current_bitrate != layer.local_bitrate {
                            // Bitrate also changed alongside tier dims -- already applied above.
                            layer.local_bitrate = new_current_bitrate;
                        }
                    }
                    if fatal_reconfigure {
                        restart_count += 1;
                        break 'encode;
                    }

                    // Event-driven simulcast layer-transition log (issue #989).
                    //
                    // Fires ONCE per change in the active-layer count — a layer
                    // being shed (count drops) or restored (count rises). NOT a
                    // periodic heartbeat: layer changes are heavily damped by the
                    // AQ controller's hysteresis (≈1.5s to shed, ≈5s to restore),
                    // so a per-tick snapshot would be near-silent noise. We log
                    // the TRANSITION, not the steady state.
                    //
                    // Emitted HERE, after the per-layer reconfigure pass above,
                    // so each layer's `local_bitrate` already reflects the
                    // bitrate applied on this same tick — when AQ changes the
                    // active-layer count and the per-layer bitrate together, the
                    // log shows the new bitrate, not the previous tick's value.
                    //
                    // Gated on `simulcast` (n_layers > 1) so single-stream
                    // sessions — where this count is pinned at 1 — never emit it.
                    // `info!` is safe here: this is per-transition, several
                    // seconds apart at most, never per-frame or per-packet.
                    if simulcast && local_active_layers != prev_active_layers {
                        // Directional reason only. The richer cause (server
                        // CONGESTION vs WS-backpressure force-cut vs gradual /
                        // floor-saturated degrade vs recovery) lives in the
                        // `EncoderBitrateController` in the separate diagnostics
                        // loop and reaches the encode loop solely through the
                        // `shared_active_layer_count` atomic — no reason flag is
                        // plumbed across. We avoid adding heavy plumbing just for
                        // this string.
                        // TODO(#989): surface the precise shed reason
                        // (congestion / degrade / recover) from the controller
                        // via a small shared enum atomic so this can read e.g.
                        // reason=congestion instead of the directional fallback.
                        let reason = if local_active_layers < prev_active_layers {
                            "shed-under-load"
                        } else {
                            "restore"
                        };
                        let detail = layers
                            .iter()
                            .map(|l| {
                                if (l.layer_id as usize) < local_active_layers {
                                    format!(
                                        "[{}] {}x{} ~{}kbps ACTIVE",
                                        l.layer_id,
                                        l.current_w,
                                        l.current_h,
                                        l.local_bitrate / 1000
                                    )
                                } else {
                                    format!("[{}] {}x{} SHED", l.layer_id, l.current_w, l.current_h)
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(" | ");
                        log::info!(
                            "Simulcast layer change: active {}->{} (reason={}) | {}",
                            prev_active_layers,
                            local_active_layers,
                            reason,
                            detail
                        );
                        prev_active_layers = local_active_layers;
                    }

                    match JsFuture::from(video_reader.read()).await {
                        Ok(js_frame) => {
                            // Read the VideoFrame ONCE. It is fed to every layer's
                            // encoder synchronously below (WebCodecs copies the
                            // frame data on `encode`), then closed EXACTLY ONCE
                            // after all layers have encoded — see the single
                            // `video_frame.close()` at the end of this arm.
                            let video_frame = Reflect::get(&js_frame, &JsString::from("value"))
                                .unwrap()
                                .unchecked_into::<VideoFrame>();

                            // Read-and-clear the PLI keyframe request ONCE per
                            // frame and apply the SAME keyframe flag to every
                            // layer. Reading it per-layer would let only the
                            // first layer see the request (the swap clears it),
                            // desynchronizing keyframes across layers.
                            let pli_requested = force_keyframe.swap(false, Ordering::AcqRel);
                            // Use tier-controlled keyframe interval instead of the
                            // static constant, allowing adaptive quality to adjust it.
                            // Using `%` instead of `.is_multiple_of()` for compatibility
                            // with Rust toolchains older than 1.87.
                            #[allow(clippy::manual_is_multiple_of)]
                            let is_periodic_keyframe = local_keyframe_interval > 0
                                && video_frame_counter % local_keyframe_interval == 0;
                            let want_keyframe = is_periodic_keyframe || pli_requested;
                            if pli_requested {
                                log::info!(
                                    "CameraEncoder: forcing keyframe at frame {} (PLI)",
                                    video_frame_counter
                                );
                            }

                            // Frame display dimensions, read once; each layer
                            // clamps to its own current dims + the shared tier max.
                            let frame_width = video_frame.display_width();
                            let frame_height = video_frame.display_height();

                            let mut fatal_encode = false;
                            // Health is anchored to the BASE layer (layer_id == 0),
                            // NOT "≥1 layer encoded". Every receiver currently decodes
                            // only layer 0 (receiver default `selected_video_layer = 0`),
                            // so a broken base layer means broken video for everyone —
                            // even if a higher layer succeeds. Tracking `any_ok` here
                            // would reset `restart_count` every frame and strand the
                            // encoder forever on a non-fatally-failing base layer with
                            // no restart path. (Fatal errors on ANY layer still force a
                            // restart via `fatal_encode` below; this only governs the
                            // non-fatal restart-counter reset.) For N==1 the sole layer
                            // IS layer 0, so `base_ok` ≡ the old `any_ok` — no behavior
                            // change in the PR-A single-layer path.
                            let mut base_ok = false;
                            for layer in layers.iter_mut() {
                                // Simulcast: skip inactive (shed) top layers — no
                                // encode, no send, so a dropped layer costs zero
                                // CPU/egress.
                                if simulcast && (layer.layer_id as usize) >= local_active_layers {
                                    continue;
                                }

                                // Dimension-change handling (rotation, camera
                                // switch). SIMULCAST: each layer's resolution is
                                // FIXED by its tier — the encoder downscales the
                                // source frame automatically — so we do NOT track
                                // frame dimensions or reconfigure on frame-size
                                // change (the `!simulcast` gate below). SINGLE-
                                // STREAM: keep the legacy behavior of following
                                // the frame size, constrained to the current tier
                                // max while preserving the frame's native aspect
                                // ratio (#1037).
                                //
                                // `frame_width` / `frame_height` are the raw
                                // native VideoFrame dimensions (the true source
                                // aspect). Fitting them uniformly (rather than a
                                // per-axis `.min()`) prevents the encoder from
                                // baking a stretch/squash into the stream when the
                                // source (e.g. a 4:3 webcam) does not match the
                                // 16:9 tier ceiling. For N==1 this matches the
                                // legacy single encoder; the computed values are
                                // only consumed on the `!simulcast` reconfigure
                                // path below.
                                let (clamped_width, clamped_height) =
                                    if frame_width > 0 && frame_height > 0 {
                                        fit_within_preserving_aspect(
                                            frame_width,
                                            frame_height,
                                            local_tier_max_width,
                                            local_tier_max_height,
                                        )
                                    } else {
                                        // Degenerate frame dims: leave as-is so
                                        // the `> 0` change-detection below skips
                                        // the reconfigure.
                                        (frame_width, frame_height)
                                    };

                                if !simulcast
                                    && clamped_width > 0
                                    && clamped_height > 0
                                    && (clamped_width != layer.current_w
                                        || clamped_height != layer.current_h)
                                {
                                    // Guard: do not configure a closed encoder.
                                    if layer.encoder.state() == CodecState::Closed {
                                        log::warn!("CameraEncoder: encoder closed before dimension reconfigure (layer {})", layer.layer_id);
                                        fatal_encode = true;
                                        break;
                                    }

                                    log::info!("Camera dimensions changed from {}x{} to {clamped_width}x{clamped_height}, reconfiguring encoder (layer {})", layer.current_w, layer.current_h, layer.layer_id);

                                    layer.current_w = clamped_width;
                                    layer.current_h = clamped_height;

                                    let new_config = VideoEncoderConfig::new(
                                        get_video_codec_string(),
                                        layer.current_h,
                                        layer.current_w,
                                    );
                                    new_config.set_bitrate(layer.local_bitrate as f64);
                                    new_config.set_latency_mode(LatencyMode::Realtime);
                                    if let Err(e) = layer.encoder.configure(&new_config) {
                                        CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL
                                            .fetch_add(1, Ordering::Relaxed);
                                        if is_fatal_encoder_error(&e) {
                                            error!("CameraEncoder: fatal configure error (layer {}), restarting: {e:?}", layer.layer_id);
                                            fatal_encode = true;
                                            break;
                                        }
                                        error!("Error reconfiguring camera encoder with new dimensions (layer {}): {e:?}", layer.layer_id);
                                    }
                                }

                                let video_encoder_encode_options = VideoEncoderEncodeOptions::new();
                                video_encoder_encode_options.set_key_frame(want_keyframe);

                                match layer.encoder.encode_with_options(
                                    &video_frame,
                                    &video_encoder_encode_options,
                                ) {
                                    Ok(_) => {
                                        // Raw per-layer submission counter. NOTE: for
                                        // N>1 this aggregates ALL layers, so it can
                                        // overcount relative to delivered/usable frames
                                        // (only the base layer is decoded today — see
                                        // the `experimental_simulcast_max_layers` knob
                                        // docs). Left as-is intentionally; it is a
                                        // submission counter, not a health gate.
                                        CAMERA_ENCODER_FRAMES_SUBMITTED_OK
                                            .fetch_add(1, Ordering::Relaxed);
                                        if layer.layer_id == 0 {
                                            base_ok = true;
                                        }
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
                                            error!("CameraEncoder: fatal encode error (layer {}, restart {restart_count}): {e:?}", layer.layer_id);
                                            fatal_encode = true;
                                            break;
                                        }
                                        error!(
                                            "Error encoding video frame (layer {}): {e:?}",
                                            layer.layer_id
                                        );
                                    }
                                }
                            }

                            // Close the frame EXACTLY ONCE, after every layer has
                            // encoded (or we hit a fatal error). Encoders copy
                            // synchronously so all layers have already consumed
                            // the frame data by here. Never double-close: the
                            // fatal path below also reaches this single close.
                            video_frame.close();

                            // Sender encoder backpressure (issue #1108, Phase B).
                            // After submitting this frame to every ACTIVE layer,
                            // sample the max `encode_queue_size()` across those
                            // layers and publish it for the AQ control loop. We
                            // mirror the encode gate above (skip layers
                            // `>= local_active_layers` in simulcast mode) so a
                            // shed layer's stale queue can't keep the signal hot.
                            // For N==1 this is just the sole base layer's depth.
                            // Stage 1: stored-only on the controller side, so this
                            // is observability with no behavior change.
                            let max_active_queue_depth = layers
                                .iter()
                                .filter(|l| {
                                    !simulcast || (l.layer_id as usize) < local_active_layers
                                })
                                .map(|l| l.encoder.encode_queue_size())
                                .max()
                                .unwrap_or(0);
                            shared_encoder_queue_depth
                                .store(max_active_queue_depth, Ordering::Relaxed);

                            // First healthy frame after a restart resets the restart
                            // counter so transient errors don't accumulate toward
                            // MAX_RESTARTS across long-lived sessions. "Healthy" is
                            // base-layer success (see `frame_is_healthy`): a frame in
                            // which only a higher layer encoded is NOT healthy, because
                            // receivers decode the base layer only.
                            let frame_healthy = frame_is_healthy(base_ok);
                            if frame_healthy && !encoded_ok_this_cycle && restart_count > 0 {
                                log::info!(
                                    "CameraEncoder: base-layer encode succeeded after restart, resetting restart counter"
                                );
                                restart_count = 0;
                            }
                            if frame_healthy {
                                encoded_ok_this_cycle = true;
                            }

                            if fatal_encode {
                                restart_count += 1;
                                break 'encode;
                            }

                            video_frame_counter += 1;
                        }
                        Err(e) => {
                            error!("error {e:?}");
                        }
                    }
                } // end 'encode

                // --- Cleanup before restart ---
                // Persist each layer's sequence number from its output handler so
                // the next restart cycle continues numbering where we left off.
                for layer in &layers {
                    sequence_numbers[layer.layer_id as usize] = layer.seq_out.get();
                }

                // Close every layer's encoder (may already be closed; ignore errors).
                for layer in &layers {
                    let _ = layer.encoder.close();
                }
                // Drop the layers (and their owned closures) before the next
                // 'restart iteration rebuilds them.
                drop(layers);

                // Stop the media track to release the camera.
                let vt = video_track.clone().unchecked_into::<MediaStreamTrack>();
                vt.stop();

                log::info!("CameraEncoder: cleaned up encoders and track, looping to restart");
                // Loop back to 'restart for backoff + re-acquisition.
            } // end 'restart
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_simulcast_layers, clamp_layer_count, frame_is_healthy,
        is_fatal_encoder_error_message, SimulcastLayerInfo, SIMULCAST_MAX_SUPPORTED_LAYERS,
    };

    #[test]
    fn simulcast_layers_emit_shed_rungs_with_zero_bitrate() {
        // Issue #1095: the ladder must carry one rung per EFFECTIVE layer, with
        // the top (effective - active) shed rungs at bitrate 0, so the UI can draw
        // the ghosted/dashed shed rungs instead of the ladder silently shrinking.
        let resolutions = [(640, 360), (960, 540), (1280, 720)];
        let active_bitrates = [400u32, 900]; // only 2 active; L2 is shed
        let layers = build_simulcast_layers(3, 2, &resolutions, &active_bitrates);

        // All 3 effective rungs present (not just the 2 active).
        assert_eq!(layers.len(), 3, "must emit one rung per EFFECTIVE layer");
        assert_eq!(
            layers,
            vec![
                SimulcastLayerInfo {
                    layer_id: 0,
                    bitrate_kbps: 400,
                    width: 640,
                    height: 360,
                },
                SimulcastLayerInfo {
                    layer_id: 1,
                    bitrate_kbps: 900,
                    width: 960,
                    height: 540,
                },
                // SHED layer: resolution still resolved, bitrate 0 = shed marker.
                SimulcastLayerInfo {
                    layer_id: 2,
                    bitrate_kbps: 0,
                    width: 1280,
                    height: 720,
                },
            ]
        );
        // The shed boundary: layers[i].bitrate_kbps == 0 exactly for i >= active.
        for (i, l) in layers.iter().enumerate() {
            if (i as u32) >= 2 {
                assert_eq!(l.bitrate_kbps, 0, "layer {i} is shed → bitrate 0");
            } else {
                assert!(l.bitrate_kbps > 0, "layer {i} is active → real bitrate");
            }
        }
    }

    #[test]
    fn simulcast_layers_all_active_have_no_shed() {
        // active == effective → every rung carries a real bitrate, none shed.
        let resolutions = [(640, 360), (960, 540), (1280, 720)];
        let active_bitrates = [400u32, 900, 1500];
        let layers = build_simulcast_layers(3, 3, &resolutions, &active_bitrates);
        assert_eq!(layers.len(), 3);
        assert!(layers.iter().all(|l| l.bitrate_kbps > 0));
        assert_eq!(layers[2].bitrate_kbps, 1500);
    }

    #[test]
    fn simulcast_layers_clamp_active_and_missing_inputs() {
        // active > effective is clamped (no panic), and missing resolution /
        // bitrate slots default to 0 rather than indexing out of bounds.
        let layers = build_simulcast_layers(2, 99, &[(640, 360)], &[]);
        assert_eq!(layers.len(), 2);
        // Resolution for L1 is missing → (0,0); bitrate atomics empty → 0.
        assert_eq!(
            layers[0],
            SimulcastLayerInfo {
                layer_id: 0,
                bitrate_kbps: 0,
                width: 640,
                height: 360,
            }
        );
        assert_eq!(
            layers[1],
            SimulcastLayerInfo {
                layer_id: 1,
                bitrate_kbps: 0,
                width: 0,
                height: 0,
            }
        );
    }

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

    #[test]
    fn clamp_layer_count_treats_zero_as_one() {
        // 0 is meaningless (there is always the base layer) → 1. This is also
        // the PR-A invariant: max_layers == 1 (or 0) yields exactly one layer,
        // whose only layer_id is 0 — byte-identical to the legacy path.
        assert_eq!(clamp_layer_count(0), 1);
        assert_eq!(clamp_layer_count(1), 1);
    }

    #[test]
    fn clamp_layer_count_passes_through_in_range() {
        assert_eq!(clamp_layer_count(2), 2);
        assert_eq!(clamp_layer_count(3), 3);
    }

    #[test]
    fn clamp_layer_count_caps_at_max_supported() {
        assert_eq!(clamp_layer_count(4), SIMULCAST_MAX_SUPPORTED_LAYERS);
        assert_eq!(clamp_layer_count(99), SIMULCAST_MAX_SUPPORTED_LAYERS);
    }

    #[test]
    fn base_layer_failure_is_unhealthy_even_when_higher_layer_succeeds() {
        // Models the per-frame health decision in the encode loop. `base_ok` is
        // set true ONLY when the layer with `layer_id == 0` encodes Ok. A higher
        // layer succeeding while the base layer fails must NOT mark the frame
        // healthy — otherwise the restart counter resets every frame and the
        // encoder is stranded on a broken base layer with no restart path (#989).

        // Simulate "base layer failed, layer 1 succeeded": base_ok stays false.
        let base_ok_when_only_higher_layer_ok = false;
        assert!(
            !frame_is_healthy(base_ok_when_only_higher_layer_ok),
            "a frame where only a higher layer encoded must be UNHEALTHY"
        );

        // Simulate "base layer encoded Ok": healthy regardless of higher layers.
        assert!(
            frame_is_healthy(true),
            "a frame where the base layer encoded must be HEALTHY"
        );

        // N==1 invariant: the sole layer IS layer 0, so its success == base_ok ==
        // frame healthy — byte-identical to the pre-fix `any_ok` behavior.
        let sole_layer_ok = true; // single-layer encode succeeded
        assert_eq!(frame_is_healthy(sole_layer_ok), sole_layer_ok);
    }

    #[test]
    fn single_layer_emits_layer_id_zero() {
        // The build loop assigns layer_id = layer_idx for idx in 0..n_layers.
        // For n_layers == 1 (PR A) the only id is 0. This pins that invariant
        // without needing a live camera/VideoEncoder.
        let n_layers = clamp_layer_count(1) as usize;
        let ids: Vec<u32> = (0..n_layers).map(|i| i as u32).collect();
        assert_eq!(ids, vec![0]);
    }
}

/// Browser-only tests: exercise the real `transform_video_chunk` layer-id
/// wiring with a genuine `EncodedVideoChunk`. This is the closest faithful
/// proxy for "the N=1 encoder emits `simulcast_layer_id == 0`" that does not
/// require camera permission / `getUserMedia` — the output handler's only
/// simulcast-relevant behaviour is the `layer_id` it threads into
/// `transform_video_chunk`.
#[cfg(test)]
mod wasm_tests {
    use super::*;
    use crate::crypto::aes::Aes128State;
    use protobuf::Message;
    use videocall_types::protos::packet_wrapper::PacketWrapper;
    use wasm_bindgen_test::*;
    use web_sys::{EncodedVideoChunkInit, EncodedVideoChunkType};

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_chunk() -> web_sys::EncodedVideoChunk {
        let data = js_sys::Uint8Array::new_with_length(8);
        let init = EncodedVideoChunkInit::new(&data, 0.0, EncodedVideoChunkType::Key);
        web_sys::EncodedVideoChunk::new(&init).unwrap()
    }

    #[wasm_bindgen_test]
    fn transform_video_chunk_layer_zero_omits_field() {
        let aes = Rc::new(Aes128State::new(false));
        let mut buf = vec![0u8; 100_000];
        let wrapper =
            super::transform_video_chunk(make_chunk(), 0, buf.as_mut_slice(), "alice", aes, 0);
        // Layer 0 round-trips to 0 and (proto3 tag-5-when-nonzero) is wire-absent.
        let bytes = wrapper.write_to_bytes().unwrap();
        let parsed = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        assert_eq!(parsed.simulcast_layer_id, 0);
    }

    #[wasm_bindgen_test]
    fn transform_video_chunk_layer_two_round_trips() {
        let aes = Rc::new(Aes128State::new(false));
        let mut buf = vec![0u8; 100_000];
        let wrapper =
            super::transform_video_chunk(make_chunk(), 0, buf.as_mut_slice(), "alice", aes, 2);
        let bytes = wrapper.write_to_bytes().unwrap();
        let parsed = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        assert_eq!(parsed.simulcast_layer_id, 2);
    }
}

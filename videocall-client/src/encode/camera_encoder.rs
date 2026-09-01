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
use videocall_transport::webtransport::{FrameDropMeta, ReadyStallThresholdOwner};

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
// Encoder auto-RESTART cycles (issue #527), partitioned by reason. Bumped once
// per `restart_count += 1` (the loop re-enters `'restart`), NOT per error event:
// distinct from the *_ERRORS_* counters above, which count fault occurrences
// (one restart may follow several classified errors, and a restart may have no
// classified error, e.g. a getUserMedia failure). Exported as
// `videocall_encoder_restart_total{kind="camera", reason}`. Cold start (the
// first `'restart` iteration with `restart_count == 0`) and user-initiated
// `stop()`/supersede returns do NOT bump these.
static CAMERA_ENCODER_RESTARTS_CLOSED_CODEC: AtomicU64 = AtomicU64::new(0);
static CAMERA_ENCODER_RESTARTS_MEMORY: AtomicU64 = AtomicU64::new(0);
static CAMERA_ENCODER_RESTARTS_CONFIGURE: AtomicU64 = AtomicU64::new(0);
static CAMERA_ENCODER_RESTARTS_OTHER: AtomicU64 = AtomicU64::new(0);
// Cumulative count of upper-rung `VideoEncoder`s torn down after a sustained
// shed dwell (issue #1230). Bumped once per rung freed in the encode loop; the
// freed encoder + its ~100KB output buffer are reclaimed and the existing lazy
// path (`encoders_to_build`) rebuilds the rung if it is ever earned back.
static CAMERA_ENCODER_LAYERS_TORN_DOWN_AFTER_DWELL: AtomicU64 = AtomicU64::new(0);

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

/// Cumulative camera encoder auto-restart cycles classified as a closed/invalid
/// codec (issue #527). See [`record_camera_restart`].
pub fn camera_encoder_restarts_closed_codec() -> u64 {
    CAMERA_ENCODER_RESTARTS_CLOSED_CODEC.load(Ordering::Relaxed)
}
/// Cumulative camera encoder auto-restart cycles classified as a memory fault.
pub fn camera_encoder_restarts_memory() -> u64 {
    CAMERA_ENCODER_RESTARTS_MEMORY.load(Ordering::Relaxed)
}
/// Cumulative camera encoder auto-restart cycles caused by a fatal `configure()`
/// or an encoder found already-closed at a reconfigure/guard point.
pub fn camera_encoder_restarts_configure() -> u64 {
    CAMERA_ENCODER_RESTARTS_CONFIGURE.load(Ordering::Relaxed)
}
/// Cumulative camera encoder auto-restart cycles with no codec/memory/configure
/// cause (media-acquisition failures and unclassified errors).
pub fn camera_encoder_restarts_other() -> u64 {
    CAMERA_ENCODER_RESTARTS_OTHER.load(Ordering::Relaxed)
}

/// Record one camera encoder auto-restart cycle, partitioned by [`RestartReason`]
/// (issue #527). Call this at EACH `restart_count += 1` site, with the reason
/// that triggered the restart. Cold start and user-initiated stop must NOT call
/// this (they do not bump `restart_count`).
fn record_camera_restart(reason: RestartReason) {
    let counter = match reason {
        RestartReason::ClosedCodec => &CAMERA_ENCODER_RESTARTS_CLOSED_CODEC,
        RestartReason::Memory => &CAMERA_ENCODER_RESTARTS_MEMORY,
        RestartReason::Configure => &CAMERA_ENCODER_RESTARTS_CONFIGURE,
        RestartReason::Other => &CAMERA_ENCODER_RESTARTS_OTHER,
    };
    counter.fetch_add(1, Ordering::Relaxed);
    // `trace!` (off by default) so this adds no production noise; it records the
    // exact `reason` label the metric uses (RestartReason::as_label) for local
    // debugging and is NOT a periodic/analyzer-consumed line.
    log::trace!(
        "camera encoder restart recorded (reason={})",
        reason.as_label()
    );
}
/// Cumulative count of upper-rung simulcast `VideoEncoder`s torn down after a
/// sustained shed dwell (issue #1230). A pure observability hook: it lets
/// Prometheus/Grafana confirm memory is actually being reclaimed on devices
/// stuck in distress, and that teardown is NOT thrashing (a rapidly climbing
/// counter alongside frequent rebuilds would mean the dwell is mistuned).
pub fn camera_encoder_layers_torn_down() -> u64 {
    CAMERA_ENCODER_LAYERS_TORN_DOWN_AFTER_DWELL.load(Ordering::Relaxed)
}
use crate::connection::MediaStreamKey;
use crate::media_devices::classify_get_user_media_error;
use crate::MediaPermissionsErrorState;
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
use super::classify_encode_error::{
    classify_encode_error, restart_reason_from_message, EncodeErrorBucket, RestartReason,
};
use super::dimensions::{corrected_source_dims, resolve_capture_dimensions};
use super::encoder_state::{
    keyframe_tick_decision, periodic_keyframe_due, EncoderState, KeyframeTickInput,
};
use super::transform::transform_video_chunk;
use super::AqControlLoopCancel;

use crate::adaptive_quality_constants::{
    camera_periodic_keyframe_max_interval_ms, simulcast_layers, AUDIO_QUALITY_TIERS,
    BITRATE_CHANGE_THRESHOLD, SIMULCAST_LAYER_FPS_THROTTLE_SLACK, VIDEO_QUALITY_TIERS,
};
use crate::constants::get_video_codec_string;
use crate::diagnostics::adaptive_quality_manager::TierTransitionRecord;
use crate::diagnostics::EncoderBitrateController;
use crate::health_reporter::ClimbLimiterSnapshot;
use videocall_aq::{fit_within_preserving_aspect, simulcast_layer_target_dims};

// Issue #2199: only this crate sees both the mirror and the real constant.
const _: () = assert!(
    videocall_aq::constants::RECEIVER_MAX_KEYFRAME_LESS_HOLD_MS
        == videocall_codecs::jitter_buffer::MAX_KEYFRAME_LESS_HOLD_MS,
    "videocall-aq's mirrored receiver keyframe-less-hold ceiling drifted from videocall-codecs (#2199)"
);

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
///
/// **`video_width`/`video_height` are a MEASUREMENT; every other field is a TIER
/// TARGET** (issue #2170). The dims carry the geometry the encode loop last
/// requested for its top active layer, with `(0, 0)` as the NOT-YET-PUBLISHED
/// sentinel. `video_fps`/`video_ideal_kbps` remain AQ tier ideals and stay
/// meaningful with no frame published. Do not read the two kinds of field as
/// having the same provenance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveQualitySnapshot {
    /// Current video tier index (0 = best / 1080p, 7 = worst / 240p).
    pub video_tier_index: usize,
    /// Encode width (px) of the top ACTIVE layer — the geometry the encode loop
    /// last REQUESTED, not the AQ tier's bounding box (issue #2170).
    ///
    /// `0` is the NOT-YET-PUBLISHED sentinel: before the first frame, and after
    /// [`CameraEncoder::stop`]. Consumers must render it as unknown rather than as
    /// `0x0` — see `format_video_readout`, which emits an em-dash.
    ///
    /// **"last REQUESTED", not "is encoding at".** On the single-stream path the
    /// encoder can revert to its build-time native geometry behind this value,
    /// because that path reconfigures without storing the new `VideoEncoderConfig`
    /// (issue #2176, pre-existing and not fixed here). The value is still strictly
    /// better than the tier box it replaces — a box the encoder never encoded at in
    /// any state — but it is not a guarantee about the live encoder.
    pub video_width: u32,
    /// Encode height (px) of the top ACTIVE layer. Same sentinel and the same
    /// last-REQUESTED caveat as [`Self::video_width`].
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

/// Pack a fitted `(width, height)` pair into one `u32` for the shared per-layer
/// dimension slots (issue #2170).
///
/// A SINGLE atomic per layer rather than two, so width and height are always read
/// as a coherent PAIR: two separate atomics could tear across an encoder
/// reconfigure, letting a reader observe a width from the new geometry with a
/// height from the old (e.g. `640×720`), a resolution that never existed.
///
/// To be precise about the guarantee, since it is easy to over-claim: that tear is
/// NOT reachable in this runtime. `wasm32-unknown-unknown` has no shared-memory
/// threads, and every reader borrows the cell and releases it synchronously inside
/// a single JS task, so no reader can interleave between two adjacent stores.
/// Packing is free and keeps the invariant true by construction if this ever runs
/// under wasm threads — it is not fixing an observed tear today.
///
/// Both axes are clamped to `u16::MAX`, which no real capture or tier approaches
/// (the largest shipped tier is 1920×1080), so the clamp is a
/// never-emit-a-corrupt-value guard rather than a functional limit.
pub(crate) fn pack_layer_dims(width: u32, height: u32) -> u32 {
    (width.min(u16::MAX as u32) << 16) | height.min(u16::MAX as u32)
}

/// Inverse of [`pack_layer_dims`]. `0` unpacks to `(0, 0)`, which callers treat as
/// "the encode loop has not published dims for this layer yet".
pub(crate) fn unpack_layer_dims(packed: u32) -> (u32, u32) {
    (packed >> 16, packed & 0xFFFF)
}

/// May THIS encode-loop generation write the SHARED per-layer geometry (issue
/// #2170)?
///
/// `true` only for the current generation. Two writers ask this — the publish
/// inside [`LayerEncoder::set_encode_dims`] and the `'restart`-head
/// [`clear_layer_dims`] — and both must answer it the same way, so the comparison
/// lives here rather than being spelled out twice.
///
/// # Why this is NOT [`loop_is_superseded`]
///
/// That predicate ORs in `!enabled`, which is right for "should I stop looping?"
/// and WRONG here: a still-current but disabled loop legitimately clears its own
/// geometry (that is `stop()`'s contract). The only question this asks is
/// ownership: is the shared state mine to write?
///
/// # What a host test on this DOES and does NOT buy
///
/// It pins the DECISION. It does not pin the two CALL SITES, which need a real
/// `VideoEncoder` (or a live frame read) to reach — deleting either fence leaves
/// the host suite green. Extraction makes the rule legible and testable; it is not
/// call-site coverage. See the disclosure at `shared_layer_dims`' clone in
/// [`CameraEncoder::start`].
pub(crate) fn loop_owns_shared_geometry(loop_epoch: u64, my_epoch: u64) -> bool {
    loop_epoch == my_epoch
}

/// May this generation PUBLISH geometry right now (issue #2170)?
///
/// Strictly stronger than [`loop_owns_shared_geometry`]: a publish additionally
/// requires the encoder to still be ENABLED. The two differ because the two writers
/// answer different questions.
///
/// # Why the epoch alone is not enough for a publish
///
/// `CameraEncoder::stop()` clears the slots and sets `enabled = false`, but it does
/// **not** bump `loop_epoch` — `EncoderState::stop` only touches `enabled` and
/// `switching`. So a loop of the CURRENT generation that is parked in an await keeps
/// `loop_epoch == my_epoch` across a `stop()`. The acquire-phase
/// `loop_is_superseded` check does read `enabled`, but it runs BEFORE
/// `video_element.play().await` (and its 200 ms autoplay-retry sleep), with no
/// re-check between there and `build_layer`'s publish. A `stop()` inside that window
/// would therefore clear, and then the parked loop would publish non-zero dims over
/// the `(0, 0)` sentinel — leaving a stopped camera reporting a live resolution.
/// Unrepairable, like every other stale-write on this channel: nothing republishes,
/// because every other writer is gated on a dimension CHANGE that a clear does not
/// move.
///
/// # Why the CLEAR must NOT require `enabled`
///
/// The `'restart`-head clear deliberately uses the epoch-only predicate: a
/// still-current loop that has just been DISABLED *should* clear its geometry — that
/// is exactly `stop()`'s contract. Requiring `enabled` there would skip the clear
/// that makes a stopped camera read as unknown. Publishes tighten, clears do not.
pub(crate) fn loop_may_publish_geometry(enabled: bool, loop_epoch: u64, my_epoch: u64) -> bool {
    enabled && loop_owns_shared_geometry(loop_epoch, my_epoch)
}

/// Publish one layer's fitted dims into the shared per-layer slots (issue #2170).
///
/// Split out of [`LayerEncoder::set_encode_dims`] so the WRITE ITSELF is testable
/// off-browser. It was not, and that mattered: deleting this entire body left the
/// whole suite green, because every seeding helper writes the vec directly and
/// bypasses the writer.
///
/// **Scope of that coverage, stated precisely — extraction is NOT call-site
/// coverage.** The host tests pin this FUNCTION. They do NOT pin the
/// `publish_layer_dims(...)` CALL inside [`LayerEncoder::set_encode_dims`]:
/// deleting that call leaves the host suite green, because `LayerEncoder` owns a
/// real `VideoEncoder` and cannot be constructed off-browser. See
/// [`CameraEncoder::start`]'s `shared_layer_dims` handling for the full list of
/// browser-only call sites and how they are covered.
///
/// Self-sizing rather than pre-sized at ladder build, so the vec cannot fall out of
/// sync with a ladder whose depth changed on a camera restart, and there is no
/// second site to keep in step. `try_borrow_mut` (not `borrow_mut`) because the
/// encode loop and the snapshot readers both touch this cell and a diagnostics
/// publish must never panic a live encode on a transient conflict.
///
/// On the skip path the update is LOST, not deferred: there is no periodic
/// re-publish, and every writer is gated on a dimension CHANGE, so for a steady
/// camera the next publish may never come. That is acceptable only because the skip
/// is unreachable in this runtime — `wasm32-unknown-unknown` has no shared-memory
/// threads and every reader borrows and releases synchronously within one JS task,
/// so no reader holds this borrow across a yield. `try_borrow_mut` is defence for a
/// future where that stops being true, not a recovery mechanism.
pub(crate) fn publish_layer_dims(
    shared_layer_dims: &RefCell<Vec<Rc<AtomicU32>>>,
    layer_id: u32,
    width: u32,
    height: u32,
) {
    if let Ok(mut dims) = shared_layer_dims.try_borrow_mut() {
        if dims.len() <= layer_id as usize {
            dims.resize_with(layer_id as usize + 1, || Rc::new(AtomicU32::new(0)));
        }
        dims[layer_id as usize].store(pack_layer_dims(width, height), Ordering::Relaxed);
    }
}

/// Zero every published per-layer dim slot (issue #2170).
///
/// `0` is the "not yet published" sentinel every reader keys off, so clearing is
/// how a publisher says "I am not encoding anything right now" — as opposed to
/// leaving the last-known geometry standing, which reads downstream as a live
/// encoder. Mirrors the `reset_output_fps` convention already applied to the fps
/// diagnostic on [`CameraEncoder::stop`].
pub(crate) fn clear_layer_dims(shared_layer_dims: &RefCell<Vec<Rc<AtomicU32>>>) {
    // `try_borrow` for the same reason as the publish path: a diagnostics reset
    // must never panic a live encode on a transient borrow conflict.
    if let Ok(dims) = shared_layer_dims.try_borrow() {
        for slot in dims.iter() {
            slot.store(0, Ordering::Relaxed);
        }
    }
}

/// Clear ONE layer's published geometry — the shed-teardown counterpart to
/// [`clear_layer_dims`].
///
/// Used when the AQ's dwell timer expires and a shed rung's encoder is actually
/// closed and freed: that rung is emitting nothing, so leaving its last-known dims
/// standing makes the consumer report a live encoder that no longer exists.
///
/// A no-op for an out-of-range `layer_id`, and (like every other writer here) a
/// no-op on a borrow conflict — a diagnostics reset must never panic a live encode.
///
/// Extracted rather than left inline for the same reason as [`publish_layer_dims`],
/// and with the same limit: extraction makes the HELPER host-testable, it does NOT
/// cover the CALL in the shed-teardown loop, which needs a real `VideoEncoder` to
/// reach.
pub(crate) fn clear_layer_dim(shared_layer_dims: &RefCell<Vec<Rc<AtomicU32>>>, layer_id: usize) {
    if let Ok(dims) = shared_layer_dims.try_borrow() {
        if let Some(slot) = dims.get(layer_id) {
            slot.store(0, Ordering::Relaxed);
        }
    }
}

/// One reportable camera rung's live fitted geometry and measured output rate.
pub(crate) type CameraLayerMetric = (u32, u32, u32, Option<u32>);

/// Opaque live source for publisher-side per-layer camera metrics (issue #2170).
///
/// Every read applies two independent filters. The active-count bound excludes a
/// built-then-shed rung whose published dims remain nonzero until dwell teardown;
/// the nonzero-dims filter excludes a not-yet-built rung inside that active range.
/// The `.min(dims.len())` guard is load-bearing because the active count can rise
/// before `build_layer` grows the vec.
///
/// `try_borrow` is deliberate. A health report must skip one tick instead of
/// panicking the UI if it contends with the encode loop. FPS is absent until its
/// first measurement window closes. Unlike geometry it is not republished on an
/// OFF-to-ON transition; it re-warms from fresh output within about one second.
#[derive(Clone, Debug)]
pub struct CameraLayerMetricSource {
    dims: Rc<RefCell<Vec<Rc<AtomicU32>>>>,
    output_fps: Rc<RefCell<Vec<Rc<AtomicU32>>>>,
    output_fps_ready: Rc<RefCell<Vec<Rc<AtomicBool>>>>,
    last_chunk_ms: Rc<RefCell<Vec<Rc<AtomicU64>>>>,
    active_layer_count: Rc<AtomicU32>,
}

impl Default for CameraLayerMetricSource {
    fn default() -> Self {
        Self {
            dims: Rc::new(RefCell::new(Vec::new())),
            output_fps: Rc::new(RefCell::new(Vec::new())),
            output_fps_ready: Rc::new(RefCell::new(Vec::new())),
            last_chunk_ms: Rc::new(RefCell::new(Vec::new())),
            active_layer_count: Rc::new(AtomicU32::new(0)),
        }
    }
}

impl CameraLayerMetricSource {
    fn reportable_layers_at(&self, now_ms: f64) -> Vec<CameraLayerMetric> {
        let Ok(dims) = self.dims.try_borrow() else {
            return Vec::new();
        };
        let Ok(output_fps) = self.output_fps.try_borrow() else {
            return Vec::new();
        };
        let Ok(output_fps_ready) = self.output_fps_ready.try_borrow() else {
            return Vec::new();
        };
        let Ok(last_chunk_ms) = self.last_chunk_ms.try_borrow() else {
            return Vec::new();
        };

        let active = self.active_layer_count.load(Ordering::Relaxed).max(1) as usize;
        let upto = active.min(dims.len());
        dims[..upto]
            .iter()
            .enumerate()
            .filter_map(|(layer_id, packed)| {
                let (width, height) = unpack_layer_dims(packed.load(Ordering::Relaxed));
                if width == 0 || height == 0 {
                    return None;
                }

                let fps = output_fps_ready
                    .get(layer_id)
                    .filter(|ready| ready.load(Ordering::Relaxed))
                    .and_then(|_| output_fps.get(layer_id).zip(last_chunk_ms.get(layer_id)))
                    .map(|(fps, last_chunk)| {
                        if crate::encode::fps_after_idle_decay(
                            fps.load(Ordering::Relaxed),
                            now_ms,
                            last_chunk.load(Ordering::Relaxed) as f64,
                            crate::adaptive_quality_constants::ENCODER_FPS_IDLE_DECAY_MS,
                        )
                        .is_some()
                        {
                            0
                        } else {
                            fps.load(Ordering::Relaxed)
                        }
                    });
                Some((layer_id as u32, width, height, fps))
            })
            .collect()
    }

    pub(crate) fn reportable_layers(&self) -> Vec<CameraLayerMetric> {
        let Some(performance) = window().performance() else {
            return Vec::new();
        };
        self.reportable_layers_at(performance.now())
    }
}

fn layer_output_metric_slots(
    output_fps: &RefCell<Vec<Rc<AtomicU32>>>,
    output_fps_ready: &RefCell<Vec<Rc<AtomicBool>>>,
    last_chunk_ms: &RefCell<Vec<Rc<AtomicU64>>>,
    layer_id: usize,
) -> Option<(Rc<AtomicU32>, Rc<AtomicBool>, Rc<AtomicU64>)> {
    let mut fps = output_fps.try_borrow_mut().ok()?;
    let mut ready = output_fps_ready.try_borrow_mut().ok()?;
    let mut last_chunk = last_chunk_ms.try_borrow_mut().ok()?;
    fps.resize_with(layer_id + 1, || Rc::new(AtomicU32::new(0)));
    ready.resize_with(layer_id + 1, || Rc::new(AtomicBool::new(false)));
    last_chunk.resize_with(layer_id + 1, || Rc::new(AtomicU64::new(0)));
    Some((
        fps[layer_id].clone(),
        ready[layer_id].clone(),
        last_chunk[layer_id].clone(),
    ))
}

/// Normalize a chunk count into fps over the window it was actually measured across
/// (issue #2170).
///
/// The window closes on the first chunk at or after 1000 ms and can be FAR longer (a
/// stall, a backgrounded tab), so the raw count over-reports by `elapsed / 1000`.
/// Rounds to nearest so a steady 30 fps still reads 30, not 29.
pub(crate) fn normalize_window_fps(chunks: u32, elapsed_ms: f64) -> u32 {
    if elapsed_ms <= 0.0 {
        return chunks;
    }
    (f64::from(chunks) * 1000.0 / elapsed_ms).round() as u32
}

/// Publish one layer's measured rate, and — layer 0 ONLY — the AQ `current_fps`
/// setpoint.
///
/// `measured_fps` (normalized) is exported; `raw_window_count` goes to `current_fps`
/// deliberately, because that is a CONTROL input and issue #2170 is observability-only.
/// Its raw-count over-report after a stall is pre-existing and stays untouched.
fn publish_layer_output_fps(
    layer_id: u32,
    measured_fps: u32,
    raw_window_count: u32,
    layer_fps: &AtomicU32,
    layer_fps_ready: &AtomicBool,
    current_fps: &AtomicU32,
) {
    layer_fps.store(measured_fps, Ordering::Relaxed);
    layer_fps_ready.store(true, Ordering::Relaxed);
    if layer_id == 0 {
        current_fps.store(raw_window_count, Ordering::Relaxed);
    }
}

fn clear_layer_output_metrics(
    output_fps: &RefCell<Vec<Rc<AtomicU32>>>,
    output_fps_ready: &RefCell<Vec<Rc<AtomicBool>>>,
    last_chunk_ms: &RefCell<Vec<Rc<AtomicU64>>>,
) {
    let (Ok(fps), Ok(ready), Ok(last_chunk)) = (
        output_fps.try_borrow(),
        output_fps_ready.try_borrow(),
        last_chunk_ms.try_borrow(),
    ) else {
        return;
    };
    for slot in fps.iter() {
        slot.store(0, Ordering::Relaxed);
    }
    for slot in ready.iter() {
        slot.store(false, Ordering::Relaxed);
    }
    for slot in last_chunk.iter() {
        slot.store(0, Ordering::Relaxed);
    }
}

fn clear_layer_output_metric(
    output_fps: &RefCell<Vec<Rc<AtomicU32>>>,
    output_fps_ready: &RefCell<Vec<Rc<AtomicBool>>>,
    last_chunk_ms: &RefCell<Vec<Rc<AtomicU64>>>,
    layer_id: usize,
) {
    let (Ok(fps), Ok(ready), Ok(last_chunk)) = (
        output_fps.try_borrow(),
        output_fps_ready.try_borrow(),
        last_chunk_ms.try_borrow(),
    ) else {
        return;
    };
    if let Some(slot) = fps.get(layer_id) {
        slot.store(0, Ordering::Relaxed);
    }
    if let Some(slot) = ready.get(layer_id) {
        slot.store(false, Ordering::Relaxed);
    }
    if let Some(slot) = last_chunk.get(layer_id) {
        slot.store(0, Ordering::Relaxed);
    }
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

/// Resolve one simulcast rung's ENCODE parameters from the camera ladder:
/// `(layer_w, layer_h, tier_w, tier_h, init_bitrate_bps, layer_fps)`.
///
/// **This is the read that determines what is ACTUALLY ENCODED**, extracted from
/// the encode loop's `build_layer` closure so it is testable off-browser.
///
/// `src_w`/`src_h` are the NATIVE capture dims. **A rung's dims are a BOUNDING BOX,
/// not an output size** (issue #1196): the source is fitted inside it
/// aspect-preserving and NEVER upscaled, so a capture below the box passes through
/// at its own size. `tier_w`/`tier_h` are returned unfitted for the per-frame
/// re-fit. Pinned by `encode_path_fits_a_non_16_9_source_inside_the_rung_box`.
///
/// `layer_idx` is clamped into the ladder, so an out-of-range rung degrades to the
/// top rung rather than panicking a live call.
fn simulcast_layer_encode_params(
    n_layers: usize,
    layer_idx: usize,
    src_w: u32,
    src_h: u32,
) -> (u32, u32, u32, u32, f64, Option<u32>) {
    let tiers = simulcast_layers(n_layers);
    let idx = layer_idx.min(tiers.len().saturating_sub(1));
    let tier = &tiers[idx];
    // Seed the first GOP at the aspect-fitted dims, not the raw 16:9 tier dims.
    let (fit_w, fit_h) =
        fit_within_preserving_aspect(src_w, src_h, tier.max_width, tier.max_height);
    (
        fit_w,
        fit_h,
        tier.max_width,
        tier.max_height,
        tier.ideal_bitrate_kbps as f64 * 1000.0,
        Some(tier.target_fps),
    )
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
    /// This layer's tier bounding box (issue #1196). For simulcast layers this
    /// is the layer's own `SIMULCAST_LAYER_TIERS` rung — the source frame is
    /// fitted INSIDE this box (aspect-preserving) rather than configured at the
    /// raw box dims, so a non-16:9 capture is not squashed. For the
    /// single-stream layer the box is the shared adaptive tier max and these
    /// fields are unused (the single-stream path fits against the shared
    /// `local_tier_max_*` instead).
    tier_w: u32,
    tier_h: u32,
    /// Cached bitrate (bps) last applied to this layer's encoder.
    local_bitrate: u32,
    /// Bitrate (bps) whose `configure()` the encoder rejected (see `should_attempt_bitrate`).
    last_failed_bitrate: Option<u32>,
    /// Minimum wall-clock gap (ms) between two encodes for this layer, derived
    /// from the layer's simulcast rung `target_fps` (issue #1768). `0.0` on the
    /// single-stream path (no per-layer fps cap — the stream follows the
    /// capture/adaptive cadence). Simulcast layers use it to DROP frames that
    /// arrive faster than the rung's fps (real-time over smoothness — never
    /// queued); keyframes bypass the cap so every layer's GOP stays coherent.
    min_encode_interval_ms: f64,
    /// Wall-clock time (ms, `performance.now()` domain) of this layer's last
    /// encode. Seeded to `NEG_INFINITY` so the first frame after (re)build always
    /// encodes. Drives the [`min_encode_interval_ms`] frame-drop throttle.
    last_encode_ms: f64,
    /// Kept alive so the JS output callback stays valid (see struct doc).
    _output_closure: Closure<dyn FnMut(JsValue)>,
    /// Kept alive so the JS error callback stays valid (see struct doc).
    _error_closure: Closure<dyn FnMut(JsValue)>,
}

impl LayerEncoder {
    /// THE chokepoint for this layer's fitted encode dimensions (issue #2170).
    ///
    /// Sets `current_w`/`current_h` **and** publishes the pair into
    /// `shared_layer_dims`, so the out-of-loop consumer — the self-view readout —
    /// sees the geometry the loop requested rather than re-deriving the AQ tier's
    /// bounding box from the tables.
    ///
    /// # Why a method rather than four assignments
    ///
    /// `current_w`/`current_h` change at four distinct sites — initial build, tier
    /// dimension change, per-frame simulcast re-fit, and single-stream re-fit. Any
    /// site that assigns the fields directly without publishing leaves the shared
    /// dims STALE, which is silent: the readout keeps rendering the previous
    /// geometry and no test fails. Routing every mutation through one method makes
    /// that drift require deliberately bypassing this function, and gives a single
    /// place to audit. **A new assignment site must call this, not assign.**
    ///
    /// The publish itself lives in [`publish_layer_dims`] so it can be tested
    /// without a browser. It GROWS the vec on demand, so there is no out-of-range
    /// case to guard; the only skip path is a failed `try_borrow_mut`.
    ///
    /// This publishes what the loop is ABOUT TO REQUEST, not what `configure()`
    /// accepted — the three reconfigure sites build their config from the fields
    /// this sets, so the publish necessarily precedes the `configure()` call. See
    /// issue #2177 for the refusal path, deliberately out of scope here.
    ///
    /// # The publish is EPOCH-FENCED; `current_w`/`current_h` are not
    ///
    /// `current_w`/`current_h` are this loop's OWN state and are always updated —
    /// a superseded loop's encoder really is at that geometry until it exits, and
    /// its change gates must keep working.
    ///
    /// The PUBLISH is different: it writes into state SHARED with whatever loop is
    /// current, so a stale loop must not touch it. Fencing it is not theoretical
    /// (issue #2170 review, found independently twice). The acquire-phase
    /// `loop_is_superseded` check does NOT make this safe, because there are awaits
    /// between it and the first publish — `video_element.play().await`, plus a 200 ms
    /// sleep and a second await on the autoplay-retry path — and further awaits
    /// (`video_reader.read().await`) before every per-frame publish. So: loop A
    /// passes its supersede check, parks in one of those awaits, the epoch is bumped,
    /// loop B completes and publishes, then A resumes and would overwrite B's slots
    /// with A's geometry.
    ///
    /// That corruption is UNREPAIRABLE without this fence, which is why it is a fence
    /// and not a nicety: A then exits at its next epoch check WITHOUT clearing (the
    /// `'restart`-head clear is itself epoch-guarded, so A skips it), and every other
    /// writer is gated on a change that A's write does not move — so the readout
    /// would report A's dead geometry for the rest of the session behind a healthy
    /// encoder B.
    fn set_encode_dims(
        &mut self,
        width: u32,
        height: u32,
        shared_layer_dims: &RefCell<Vec<Rc<AtomicU32>>>,
        enabled: &AtomicBool,
        loop_epoch: &AtomicU64,
        my_epoch: u64,
    ) {
        self.current_w = width;
        self.current_h = height;
        self.last_failed_bitrate = None;
        // Same ordering and convention as the `'restart`-head clear and the
        // canary/bound-id clears at both supersede sites. `enabled` is read here too
        // — see `loop_may_publish_geometry` for why a publish needs it and the clear
        // must not.
        if loop_may_publish_geometry(
            enabled.load(Ordering::Acquire),
            loop_epoch.load(Ordering::Acquire),
            my_epoch,
        ) {
            publish_layer_dims(shared_layer_dims, self.layer_id, width, height);
        }
    }

    /// Rebuild this layer's config at its current fitted dims + cached bitrate,
    /// STORE IT BACK into `self.config`, and apply it. The store is what stops
    /// [`Self::reconfigure_at_bitrate`] — which mutates that same field in place
    /// — from re-applying the pre-change dims (issue #2176).
    fn reconfigure_at_current_dims(&mut self) -> Result<(), JsValue> {
        self.config =
            VideoEncoderConfig::new(get_video_codec_string(), self.current_h, self.current_w);
        self.config.set_bitrate(self.local_bitrate as f64);
        self.config.set_latency_mode(LatencyMode::Realtime);
        if self.min_encode_interval_ms > 0.0 {
            let _ = Reflect::set(
                &self.config,
                &JsValue::from_str("framerate"),
                &JsValue::from_f64(1000.0 / self.min_encode_interval_ms),
            );
        }
        self.encoder.configure(&self.config)
    }

    /// `bitrate_bps` must be visible BEFORE the call: the config
    /// [`Self::reconfigure_at_current_dims`] rebuilds is built from `self.local_bitrate`.
    fn reconfigure_at_current_dims_and_bitrate(&mut self, bitrate_bps: u32) -> Result<(), JsValue> {
        let applied = self.local_bitrate;
        self.local_bitrate = bitrate_bps;
        let outcome = self.reconfigure_at_current_dims();
        if outcome.is_err() {
            self.local_bitrate = applied;
            self.last_failed_bitrate = Some(bitrate_bps);
        } else {
            self.last_failed_bitrate = None;
        }
        outcome
    }

    fn reconfigure_at_bitrate(&mut self, bitrate_bps: u32) -> Result<(), JsValue> {
        self.config.set_bitrate(bitrate_bps as f64);
        let outcome = self.encoder.configure(&self.config);
        if outcome.is_ok() {
            self.local_bitrate = bitrate_bps;
            self.last_failed_bitrate = None;
        } else {
            self.last_failed_bitrate = Some(bitrate_bps);
        }
        outcome
    }

    fn should_attempt_bitrate(&mut self, want: u32) -> bool {
        if want == self.local_bitrate {
            self.last_failed_bitrate = None;
        }
        bitrate_attempt_allowed(want, self.local_bitrate, self.last_failed_bitrate)
    }
}

/// A target back at the applied rate needs no attempt; a latched target is withheld.
#[inline]
fn bitrate_attempt_allowed(want: u32, applied_bps: u32, last_failed_bps: Option<u32>) -> bool {
    want != applied_bps && last_failed_bps != Some(want)
}

struct CameraTierChangeTargets<'a> {
    tier_max_width: &'a AtomicU32,
    tier_max_height: &'a AtomicU32,
    tier_keyframe_interval: &'a AtomicU32,
    shared_video_tier_index: &'a AtomicU32,
    tier_change_keyframe: &'a AtomicBool,
    keyframe_cooldown_reset: &'a AtomicBool,
}

fn apply_camera_tier_change(
    targets: CameraTierChangeTargets<'_>,
    tier: &videocall_aq::constants::VideoQualityTier,
    tier_index: usize,
) {
    targets
        .tier_max_width
        .store(tier.max_width, Ordering::Relaxed);
    targets
        .tier_max_height
        .store(tier.max_height, Ordering::Relaxed);
    targets
        .tier_keyframe_interval
        .store(tier.keyframe_interval_frames, Ordering::Relaxed);
    targets
        .shared_video_tier_index
        .store(tier_index as u32, Ordering::Relaxed);
    targets.tier_change_keyframe.store(true, Ordering::Release);
    targets
        .keyframe_cooldown_reset
        .store(true, Ordering::Release);
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
    last_layer0_chunk_ms: Arc<AtomicU64>,
    on_encoder_settings_update: Callback<String>,
    on_error: Option<Callback<String>>,
    /// Classified callback fired ONLY at the real `getUserMedia` rejection site,
    /// carrying a [`MediaPermissionsErrorState`] (e.g. `DeviceInUse`) so the UI
    /// can show a specific reason and drive auto-retry. At that one site it fires
    /// INSTEAD OF `on_error` (the UI raises a dedicated modal for the classified
    /// error, so emitting both would stack two modals). `on_error` still carries
    /// the generic string for every OTHER error site in `start`.
    /// `None` until wired via [`Self::set_permission_error_callback`].
    on_permission_error: Option<Callback<MediaPermissionsErrorState>>,
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
    /// The encoder control loop's *reported* queue-depth value (f32 bits in
    /// AtomicU32) plumbed out to the health reporter for Prometheus telemetry.
    /// Distinct from the internal `shared_encoder_queue_depth` AQ bridge — this
    /// atom carries the telemetry copy, not the raw encode-loop→control-loop signal.
    shared_encoder_queue_depth_report: Rc<AtomicU32>,
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
    /// Forced-keyframe cooldown reset (issue #1311). A one-shot edge that tells
    /// the ENCODE loop to clear its `last_keyframe_emit_ms` cooldown clock so the
    /// FIRST post-reconnect/post-re-election PLI emits a forced keyframe
    /// immediately, regardless of how recently a keyframe went out pre-transition.
    ///
    /// Why a SEPARATE atom rather than reusing `reelection_completed_signal`: the
    /// re-election signal is consumed by the QUALITY task (`.swap(false)` at the
    /// `notify_reelection_completed()` site — and it is SHARED with the screen
    /// encoder's quality task, which swap-consumes its own copy), while
    /// `last_keyframe_emit_ms` lives in a DIFFERENT `spawn_local` ENCODE task.
    /// Having the encode loop ALSO `.swap` that signal would add a third racing
    /// consumer that loses the edge unpredictably. This dedicated atom is consumed
    /// only by the encode loop and ARMED from two complementary sources:
    ///
    /// * RECONNECT **and** RE-ELECTION (primary, race-free): the client's
    ///   `Connected` lifecycle callback unconditionally stores `true` via
    ///   [`Self::keyframe_cooldown_reset`]. Both a full reconnect and a re-election
    ///   re-emit `ConnectionState::Connected` (both run an election that ends in
    ///   `report_state()`), so this single client-side arm covers BOTH transitions.
    ///   A full reconnect does NOT drive `reelection_completed_signal` at all — it
    ///   runs `reset_and_start_election`, which clears `old_active_connection`, so
    ///   the post-reconnect election's "Elected" branch skips the re-election store
    ///   — which is exactly why keying off that signal alone would miss reconnects.
    /// * RE-ELECTION (secondary, no plumbing): the quality task also arms it where
    ///   it consumes `reelection_completed_signal`. Redundant with the client arm
    ///   on a winning swap, and harmless when it loses (the client arm still fires);
    ///   kept because it is the zero-plumbing in-encoder path and self-documents the
    ///   coupling at the re-election consume site.
    ///
    /// The encode loop `.swap(false)`-consumes this each frame; a duplicate arm is
    /// idempotent and only matters when a PLI is pending.
    keyframe_cooldown_reset: Rc<AtomicBool>,
    /// Pending keyframe request from an AQ tier change (issue #2567), armed by the
    /// QUALITY task and consumed by the ENCODE task. Separate from `force_keyframe`
    /// so that atom keeps meaning "a receiver asked for repair".
    tier_change_keyframe: Rc<AtomicBool>,
    /// Camera video-at-floor flag (issue #1611): `true` when the camera AQ's
    /// video quality is fully exhausted — tier at the user-capped step-down floor
    /// AND active simulcast layers at 1. Stored unconditionally by the camera AQ
    /// control loop AFTER every `tick()` so it is always current, and shared into
    /// the [`MicrophoneEncoder`] so the mic-side uplink-distress detector's
    /// backstop gate can open even with the camera on (the "camera-on but video
    /// can't shed further → audio may shed" path).
    ///
    /// `Arc` (not `Rc`) because it crosses from the camera encoder into the mic
    /// encoder, matching the camera-enabled-flag wiring pattern.
    video_at_floor_flag: Arc<AtomicBool>,
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
    /// The UI passes `min(runtime flag, sniffed capability)`. The flag defaults
    /// to 3 (#1082), while the capability check can still clamp weak devices to
    /// 1 or 2. An explicit ceiling of 1 preserves the pre-simulcast
    /// single-encoder path; larger ceilings enable per-layer tiers and AQ shed.
    max_layers: u32,
    /// Number of simulcast layers currently active (encoded + sent), written by
    /// the AQ control loop and read by the encode loop (issue #989, PR B).
    /// In single-stream mode (effective layers == 1) this stays 1 and the
    /// encode loop's per-layer gating is a no-op. The encode loop encodes only
    /// layers with `layer_id < active_layer_count`, so dropping the top layer
    /// cuts both egress and sender encode CPU.
    shared_active_layer_count: Rc<AtomicU32>,
    /// Number of simulcast layers this publisher is currently configured to
    /// encode/send — the EFFECTIVE ladder depth (#1143 observability). Published
    /// as a shared atomic (not derived from `max_layers` on the fly) so the
    /// health reporter can read the live value without holding the encoder, and
    /// so it tracks dynamic changes: today it is written once at construction to
    /// [`effective_layer_count`]; PR #1135/#1136 retunes WHEN it is >1 and will
    /// update this atomic when the effective count changes mid-call. Distinct
    /// from `shared_active_layer_count`, which is the shed-aware count of layers
    /// presently active (<= effective).
    shared_effective_layer_count: Rc<AtomicU32>,
    /// Per-layer target bitrate (bps), one atomic per ladder layer (lowest
    /// first, index == `layer_id`). Written by the AQ control loop in simulcast
    /// mode; read by the encode loop to reconfigure each layer's encoder. Empty
    /// in single-stream mode (the legacy `current_bitrate` atomic is used
    /// instead). Sized to `SIMULCAST_MAX_SUPPORTED_LAYERS` lazily on first use.
    shared_layer_bitrates_bps: Rc<RefCell<Vec<Rc<AtomicU32>>>>,
    /// Per-layer fitted encode dimensions, packed `(w << 16) | h` by
    /// [`pack_layer_dims`], one atomic per layer (lowest first, index ==
    /// `layer_id`). Issue #2170.
    ///
    /// Written by the encode loop through [`LayerEncoder::set_encode_dims`] — the
    /// single chokepoint for `current_w`/`current_h` — and read by
    /// [`CameraEncoder::top_published_layer_dims`], whose only consumer today is
    /// [`CameraEncoder::live_quality_snapshot`] (the self-view readout).
    ///
    /// **Why this exists:** before #2170 the fitted dims lived ONLY inside the
    /// encode loop's `LayerEncoder`, so the self-view readout re-derived geometry
    /// from the AQ tier table and reported that tier's bounding box instead of what
    /// the loop had configured. A rung/tier's `max_width`/`max_height` is a BOUNDING
    /// BOX: `fit_within_preserving_aspect` never upscales, so a 4:3 640×480 webcam
    /// on tier `medium` encodes 640×480 while the table says 854×480. This is the
    /// publication point that gives the readout the real value.
    ///
    /// Unlike `shared_layer_bitrates_bps` this is sized for SINGLE-STREAM mode too
    /// (one entry), because the self-view resolution readout needs fitted dims in
    /// both modes. `0` means "not yet published"; consumers must render it as
    /// unknown rather than as `0x0`.
    ///
    /// NOTE the asymmetry with the sibling atomics: the per-generation reset at the
    /// `'restart` loop head clears THIS vec but not `shared_layer_bitrates_bps` or
    /// `shared_active_layer_count`, which the AQ control loop owns. So during a codec
    /// rebuild (backoff + re-acquire, ~0.5-3s) the readout reads unknown while the
    /// AQ's tier targets still render. Deliberate: the AQ's intent (how many rungs it
    /// wants active, at what bitrate) genuinely survives a codec restart, whereas the
    /// geometry does not exist until the new generation publishes it.
    ///
    /// **`live_simulcast_snapshot` deliberately does NOT read this** — the
    /// diagnostics ladder still resolves rung bounding boxes from the ladder table,
    /// the same defect one surface over. Routing it here needs the sentinel rendered
    /// in the drawer (em-dash copy, a11y labels, shed-rung CSS) and is issue #2170's
    /// second half, not a loose end of this change.
    shared_layer_dims: Rc<RefCell<Vec<Rc<AtomicU32>>>>,
    /// Measured output fps per camera rung. These observability-only atomics are
    /// written by each layer's output callback and are never read by AQ control.
    shared_layer_output_fps: Rc<RefCell<Vec<Rc<AtomicU32>>>>,
    /// Presence companion for `shared_layer_output_fps`: false until the first
    /// measurement window closes, then true even when an idle decay reports 0.
    shared_layer_output_fps_ready: Rc<RefCell<Vec<Rc<AtomicBool>>>>,
    /// Last output-chunk time per rung, used only by the reporter to turn a stale
    /// measured rate into a present 0 without touching the AQ setpoint.
    shared_layer_last_chunk_ms: Rc<RefCell<Vec<Rc<AtomicU64>>>>,
    /// Monotonic count of whole-vec geometry CLEARS (issue #2170).
    ///
    /// Bumped by every `clear_layer_dims` caller that is NOT itself about to
    /// republish — today that is `CameraEncoder::stop()`. The encode loop compares
    /// the value it last saw against the live one each frame and republishes its
    /// layers' geometry when they differ.
    ///
    /// # Why a COUNTER and not an enabled-edge
    ///
    /// The hazard this exists for is a camera OFF→ON pair that happens entirely
    /// while the loop is parked in `video_reader.read().await` (OS camera shutter,
    /// another app holding the device). A tick-observed `enabled` edge CANNOT see
    /// that: the loop never samples the disabled state, so on resume it reads
    /// `(prev = true, now = true)`, no edge fires — while `start()`'s same-device
    /// guard has already early-returned, so no new generation republishes either. A
    /// counter is edge-independent: the clear is recorded whether or not any frame
    /// boundary observed it. (An earlier fix here used the edge and missed exactly
    /// the scenario it was written for.)
    shared_layer_dims_cleared: Rc<AtomicU64>,
    /// Sender-side encoder backpressure (issue #1108, Phase B): the max
    /// `VideoEncoder::encode_queue_size()` across the ACTIVE layers, written by
    /// the encode loop each frame and read by the AQ control loop to feed
    /// [`EncoderBitrateController::observe_encoder_queue_depth`]. The encode loop
    /// owns the `VideoEncoder`s and the control loop owns the controller, so this
    /// atomic is the borrow-safe bridge between the two tasks — neither borrows
    /// the other's state. **This is NOT observability-only (issue #1108, Phase
    /// B):** the AQ control loop maps the sampled depth through the controller's
    /// `backpressure_decision` hysteresis (the HIGH/CLEAR sustain/stabilization
    /// timers) into a gradual video tier step-down/up — i.e. a simulcast
    /// top-layer shed/restore under sustained encode backpressure.
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
    /// User SEND layer-ceiling for this publisher's VIDEO ladder (perf-panel
    /// "layers published" thumb). The performance panel lets the user explicitly
    /// bound how many simulcast layers this publisher emits; the UI writes the
    /// chosen layer COUNT here (via [`Self::set_user_layer_ceiling`]), and the AQ
    /// control loop reads it each tick and feeds it to
    /// [`EncoderBitrateController::observe_user_layer_ceiling`], which caps the
    /// published ladder as a further `min` alongside the relay union hint and the
    /// runtime ramp.
    ///
    /// **Initialized to [`u32::MAX`] = fail-open (Auto / no user cap):** until the
    /// user drags the thumb below full, the controller keeps its full
    /// backpressure-governed ladder. Borrow-safe bridge (atomic) between the UI's
    /// setter and the control-loop task, exactly like `shared_union_requested_layer`.
    /// The base layer is ALWAYS published — the AQ side floors this cap at 1, so
    /// this never touches the floor / base-present invariant.
    shared_user_layer_ceiling: Rc<AtomicU32>,
    /// Liveness token whose sole purpose is to bound the lifetime of the AQ
    /// control-loop `spawn_local` future (issue #1108). The encoder holds the
    /// only strong reference; `set_encoder_control` captures a [`Weak`] and
    /// breaks its 1 Hz `tick` loop as soon as `upgrade()` returns `None`. Because
    /// the control loop runs on `wasm_bindgen_futures::spawn_local` (NOT bound to
    /// the Dioxus component scope), it would otherwise run forever and pin its
    /// cloned `Rc` graph + `on_encoder_settings_update` callback across `Host`
    /// remounts.
    control_loop_liveness: Rc<()>,
    control_loop_cancel: AqControlLoopCancel,
    /// Single-layer "pin to the `low` rung" gate (issue #1136, hysteretic per
    /// #1156). `true` when this publisher is in single-stream mode
    /// (`effective_layers == 1`) AND the call has **more than 3 other peers** —
    /// the flooding regime where one adaptive (medium-tier) stream is heavy on
    /// every receiver's decoder. When set, the single-stream encode path caps
    /// resolution/bitrate to the `low` rung (640×360 + low ideal) instead of the
    /// adaptive tier; when clear it keeps the existing adaptive behavior.
    ///
    /// **Hysteresis (#1156):** the gate ENGAGES at `> 3` others and RELEASES at
    /// `< 3` others, HOLDING its prior value at exactly 3. The AQ loop feeds this
    /// atomic's current value back into the decision so a participant count
    /// oscillating 3 ↔ 4 cannot flip the pin every tick — each flip would change
    /// the effective tier dims and force a keyframe-emitting reconfigure (up to
    /// 1/sec on a weak uplink).
    ///
    /// Borrow-safe bridge (atomic) between the AQ control-loop task — which reads
    /// the LIVE peer count each tick and owns the gate decision — and the
    /// encode-loop task, which reads it per frame and applies it. The decision is
    /// re-evaluated every tick, so the pin engages/releases as peers join or leave
    /// mid-call (it is NOT latched at cold start). In simulcast mode
    /// (`effective_layers > 1`) this stays `false` and the gate is a no-op.
    single_layer_low_pin: Rc<AtomicBool>,
    /// Per-encoder live-screen threshold state shared by the AQ loop and `Drop`.
    /// Its RAII owner updates the process-global threshold max and releases
    /// immediately on screen stop or Host teardown.
    screen_ready_stall_tracker: Rc<RefCell<ScreenReadyStallThresholdTracker>>,
    /// "Loop already running" canary (issue #1295). Mirrors the mic encoder's
    /// `codecs[0].is_instantiated()` canary, which the camera lacks. Set
    /// synchronously in `start()` right before `spawn_local`; cleared by the
    /// spawned task on EVERY task-exit path. `start()`'s guard reads it to refuse
    /// a duplicate loop spawn (and to tear down the old loop on a real switch),
    /// so at most one acquire/`set_src_object` is ever in flight for the selected
    /// device — closing the intermittent wrong-device race.
    loop_running: Arc<AtomicBool>,
    /// Device id the currently-running loop is bound to (issue #1295). Recorded
    /// synchronously in `start()` alongside `loop_running`, cleared on every
    /// task-exit path (epoch-guarded). The guard compares it to the freshly-selected
    /// device: same id => the live loop is already correct (true duplicate, no
    /// respawn); different id with no `switching` raised (the OFF→switch→ON
    /// path) => a real device change the `select()`-while-disabled missed, so
    /// stop() the stale loop and respawn on the new device. `Rc<RefCell<…>>`
    /// (not an atomic) matches this file's shared-mutable-across-`spawn_local`
    /// idiom and is safe on the single-threaded wasm executor.
    loop_device_id: Rc<RefCell<Option<String>>>,
    /// Loop-generation epoch (issue #1295). `start()` bumps it synchronously
    /// before spawning and each task captures its own value. On EVERY task-exit
    /// path a loop clears `loop_running`/`loop_device_id` ONLY if this epoch
    /// still equals its captured value — i.e. "I am still the latest loop". This
    /// closes the supersede race: when a different-device start() tears down a
    /// stale loop and spawns a new one, the new loop has set the canary/bound-id
    /// synchronously; the stale loop then exits LATER and must NOT clobber them.
    /// The epoch mismatch makes the stale loop's clear a no-op, so exactly the
    /// latest loop owns the canary. The acquire-phase and per-frame exit checks
    /// also read it, so a superseded loop self-terminates before binding / on its
    /// next frame even if `enabled` was flipped back true for the newer loop.
    /// Epoch is the authoritative supersede signal — the supersede guards do NOT
    /// consult `switching`, which only flags that a switch was *requested* (the
    /// newest loop is that request's response and must not abort on it).
    loop_epoch: Arc<AtomicU64>,
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

/// Per-layer framerate-cap decision for the simulcast encode throttle (issue
/// #1768). Returns `true` if the layer should encode THIS frame, `false` to
/// DROP it (real-time over smoothness — a dropped frame is never queued, so the
/// layer always encodes the newest eligible frame rather than a backlog).
///
/// A frame is encoded when ANY of:
///   * `want_keyframe` — a periodic GOP or PLI keyframe must reach EVERY layer
///     so each layer's GOP stays coherent; keyframes are never dropped. Because
///     the keyframe decision is shared across layers, all layers keyframe on the
///     same source frame and the shared ~5s cadence is preserved.
///   * `min_interval_ms <= 0.0` — no cap (the single-stream path passes 0.0).
///   * at least `(1 - SIMULCAST_LAYER_FPS_THROTTLE_SLACK) * min_interval_ms` has
///     elapsed since `last_encode_ms` — the slack lets a frame arriving slightly
///     early still count, so a rung fed by a faster capture lands near its
///     target fps instead of quantizing down (see the constant's doc).
///
/// `last_encode_ms == f64::NEG_INFINITY` (the post-(re)build seed) makes the
/// elapsed term `+∞`, so the first frame after a build/restart always encodes.
/// Pure (no clock / WebCodecs) so it is host-unit-testable off-wasm.
fn should_encode_layer_frame(
    now_ms: f64,
    last_encode_ms: f64,
    min_interval_ms: f64,
    want_keyframe: bool,
) -> bool {
    if want_keyframe || min_interval_ms <= 0.0 {
        return true;
    }
    let threshold = min_interval_ms * (1.0 - SIMULCAST_LAYER_FPS_THROTTLE_SLACK);
    now_ms - last_encode_ms >= threshold
}

/// Convert the user SEND layer-ceiling atomic (a `u32` layer COUNT, with
/// [`u32::MAX`] as the Auto / no-cap sentinel) into the `usize` count the AQ
/// controller's [`observe_user_layer_ceiling`] expects.
///
/// [`u32::MAX`] maps to [`usize::MAX`] (fail-open) explicitly rather than via
/// `as usize`, which is target-dependent: on 64-bit (native test target)
/// `u32::MAX as usize` is `2^32 - 1`, NOT `usize::MAX`, so an explicit check
/// keeps the fail-open mapping identical on wasm32 and native. Any other value
/// widens losslessly (usize is ≥ 32 bits on every supported target). The AQ side
/// re-clamps to `[1, device_ceiling]`, so an out-of-range count is harmless here.
///
/// Free function so it is unit-testable without a live `CameraEncoder`; shared
/// with the screen encoder (same atomic→count contract).
pub(crate) fn layer_ceiling_to_count(ceiling: u32) -> usize {
    if ceiling == u32::MAX {
        usize::MAX
    } else {
        ceiling as usize
    }
}

/// The number of simulcast layers a camera publisher starts ACTIVE at, before
/// the runtime ramp earns more (issue #1140 / #1141).
///
/// Every camera publisher cold-starts at the BASE layer only (1 active layer =
/// the legacy single-stream path), regardless of how many layers the device's
/// CEILING permits. The `videocall-aq` `EncoderBitrateController` then *earns*
/// additional layers up to that ceiling at runtime, gated on observed
/// encoder-queue backpressure headroom + uplink budget. The cold CPU benchmark
/// no longer gates the active layer count at startup — it sets the ceiling only.
///
/// `const fn` so it is a compile-time constant; free function so it can be
/// unit-tested without a live `CameraEncoder`.
const fn initial_active_layer_count() -> u32 {
    1
}

/// How many per-layer `VideoEncoder`s to have CONSTRUCTED given the current
/// active-layer count and the ladder ceiling (issue #1204, lazy construction).
///
/// This is the single source of truth shared by the cold-start build loop
/// (`0..n`) and the in-loop lazy-build trigger (`built_len < n` ⇒ build the
/// missing `built_len..n`). Returns `active.clamp(1, ceiling)`: at cold start
/// `active == 1` so only the BASE encoder is built (NOT the ceiling), and an
/// upper rung's encoder is built only once the AQ ramp/restore raises `active`
/// past it. Floored at 1 (the base layer is always present) and capped at the
/// ceiling (never build more encoders than the ladder has rungs).
///
/// Free `const fn` so the lazy-vs-eager boundary is host-testable without a live
/// `CameraEncoder` / `VideoEncoder` (which need `getUserMedia` + WebCodecs).
const fn encoders_to_build(active: usize, ceiling: usize) -> usize {
    if active < 1 {
        1
    } else if active > ceiling {
        ceiling
    } else {
        active
    }
}

/// Sustained-shed dwell before an upper-rung `VideoEncoder` is torn down to
/// reclaim its native VPX/WebCodecs state + ~100KB output buffer (issue #1230).
///
/// Why 30s: the AQ controller can shed/restore a layer at most once per
/// `MIN_TIER_TRANSITION_INTERVAL_MS` = 1500ms (the `can_transition` floor in
/// `videocall-aq/src/manager.rs`), so 30s is 20× the minimum shed→restore
/// interval — a transient bounce can never accumulate 30s of CONTINUOUS shed and
/// so never trips teardown. Teardown is also thrash-free regardless of how soon
/// an earn-up follows: it requires 30s of UNBROKEN shed, and the per-frame stamp
/// loop clears a rung's dwell clock the instant it is re-activated, so a
/// teardown→rebuild→teardown cycle is necessarily ≥30s apart, not a tight loop.
/// And a re-earned rung is rebuilt by the SAME lazy `encoders_to_build` path a
/// publisher already runs at every cold start (only the base is built up front
/// since #1204/#1227) — teardown introduces no new rebuild-stall class, only
/// defers an already-existing one. (`MIN_TIER_TRANSITION_INTERVAL_MS` lives in
/// `videocall-aq/src/constants.rs`. NOTE: `CLIMB_COOLDOWN_BASE_MS` is unrelated
/// here — it governs the crash-CEILING decay axis, not layer earn-up.)
const SHED_TEARDOWN_DWELL_MS: f64 = 30_000.0;

/// Pure teardown decision (issue #1230, host-testable single source of truth).
///
/// Given when a rung first became continuously shed (`shed_since_ms`, `None`
/// once it is active or already torn down), the current clock (`now_ms`), and
/// the dwell threshold, return `true` iff the rung has been shed for at least
/// `dwell_threshold_ms`. `None` ⇒ `false` (not currently shed → never tear
/// down). The `>=` makes the boundary inclusive: exactly `dwell_threshold_ms`
/// of dwell tears down.
///
/// This is the ONLY place the comparison lives so a host unit test pins it
/// (mutating `>=`→`>`, inverting the comparison, or dropping the `None` guard
/// all make the test fail). Both encode loops call it.
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

/// Minimum interval between PLI-driven *forced* keyframes a publisher will emit
/// (issue #1287 emit coalescer). The periodic GOP keyframe is NOT gated by this.
///
/// A publisher has one encoder per layer feeding a single outbound stream that is
/// broadcast to ALL receivers; a forced keyframe is ~5-10x a delta frame, x the
/// active simulcast layers, and fans out to every receiver's downlink — not just
/// the requester. With N receivers each allowed ~3 inbound KEYFRAME_REQUESTs/sec
/// by the relay, an uncoalesced publisher can be driven toward frame-rate forced
/// keyframes, amplifying egress by xN x layers. Because a single keyframe is
/// broadcast, ONE emission already satisfies every pending requester, so
/// collapsing a burst of PLIs into one forced keyframe per window cuts that
/// worst-case amplification without harming recovery: a request that lands
/// mid-window is honored the instant the window expires, so added recovery
/// latency is bounded by this value.
///
/// 250ms (at most ~4 forced keyframes/sec at 30fps, vs up to frame-rate before)
/// is a conservative starting point within the issue's 250-500ms range — longer
/// coalesces more but adds recovery latency. Tune/validate via performance-reviewer.
const FORCED_KEYFRAME_COOLDOWN_MS: f64 = 250.0;

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

/// Whether a spawned encode loop has been superseded and must abandon its work
/// WITHOUT binding the shared `<video>` element (issue #1295).
///
/// A loop is superseded iff it has been disabled (`!enabled`) OR a newer
/// `start()` bumped the loop epoch past the value this loop captured at spawn
/// (`loop_epoch != my_epoch`). The epoch is the SOLE supersede authority.
///
/// `switching` (the "a switch was requested" flag set by `EncoderState::select()`
/// while enabled) is deliberately NOT a parameter and NOT consulted: the newest
/// loop IS that request's response and must OWN the switch, not self-abort on it.
/// On a real switch, `start()`'s running-guard tears the old loop down and bumps
/// the epoch, so the stale loop is already caught by `loop_epoch != my_epoch`
/// here; the newest loop sees its own epoch and proceeds to bind. Reading
/// `switching` in this predicate was the initial-join dark-square bug — the loop
/// that should bind killed itself because a post-permission devicechange had
/// raised `switching`. Taking no `switching` arg makes that regression
/// impossible to reintroduce without changing this signature.
///
/// Pure function so it can be unit-tested without a live camera / encoder; the
/// acquire-phase and per-frame supersede guards both call it.
#[inline]
fn loop_is_superseded(enabled: bool, loop_epoch: u64, my_epoch: u64) -> bool {
    !enabled || loop_epoch != my_epoch
}

/// A minimal, decoder-free view of one simulcast layer for formatting the
/// event-driven `Simulcast layer change:` log line (issue #1106).
///
/// Decouples the log-string builder from the live `LayerEncoder` (which owns a
/// `VideoEncoder` and JS closures and cannot be constructed off-wasm) so the
/// formatting is host-testable. The encode loop projects each live
/// `LayerEncoder` into one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LayerView {
    /// Simulcast layer id (== ladder position, lowest layer first).
    id: u32,
    /// Current encoder width for this layer.
    w: u32,
    /// Current encoder height for this layer.
    h: u32,
    /// Last bitrate (bps) applied to this layer's encoder. Displayed in kbps
    /// for ACTIVE layers; ignored for SHED layers.
    bitrate_bps: u32,
}

/// Classify the direction of an active-layer-count transition for the
/// `Simulcast layer change:` log (issue #1106).
///
/// Returns `"shed-under-load"` when the active count fell (`cur < prev`) and
/// `"restore"` otherwise (it rose). The caller only emits the log when
/// `prev != cur`, so the `prev == cur` case never reaches this in practice; it
/// is folded into `"restore"` so the function is total. Pure free function so
/// the directional reason is host-testable and a mutation of the comparison
/// fails a unit test.
fn shed_reason(prev: usize, cur: usize) -> &'static str {
    if cur < prev {
        "shed-under-load"
    } else {
        "restore"
    }
}

/// Build the human-readable message for the event-driven
/// `Simulcast layer change:` log line (issue #1106).
///
/// Renders one ` | `-joined rung per layer in `layers`. A layer is ACTIVE when
/// its `id` is below `cur` (the new active-layer count) and SHED otherwise —
/// the same `layer_id < active` boundary the receiver/UI use. ACTIVE rungs show
/// the live bitrate in kbps (`bitrate_bps / 1000`); SHED rungs show only the
/// resolution (their bitrate is the zero shed marker and is not meaningful).
///
/// The returned string is the full log message, e.g.
/// `Simulcast layer change: active 3->2 (reason=shed-under-load) | [0] 320x180 ~100kbps ACTIVE | ...`.
///
/// Pure function (no atomics / DOM / `LayerEncoder`) so the exact emitted text
/// is host-testable and byte-stable against the encode loop. `prev`/`cur` are
/// the previous and new active-layer counts.
fn format_layer_transition(prev: usize, cur: usize, layers: &[LayerView]) -> String {
    let reason = shed_reason(prev, cur);
    let detail = layers
        .iter()
        .map(|l| {
            if (l.id as usize) < cur {
                format!(
                    "[{}] {}x{} ~{}kbps ACTIVE",
                    l.id,
                    l.w,
                    l.h,
                    l.bitrate_bps / 1000
                )
            } else {
                format!("[{}] {}x{} SHED", l.id, l.w, l.h)
            }
        })
        .collect::<Vec<_>>()
        .join(" | ");
    format!("Simulcast layer change: active {prev}->{cur} (reason={reason}) | {detail}")
}

/// Peer-count threshold at/above which the single-layer low-rung pin ENGAGES
/// (issue #1136). `> 3 others` means the local publisher plus more than 3 remote
/// participants — i.e. a call of 5+ where one adaptive medium-tier stream from
/// each single-layer publisher is heavy on every receiver's decoder.
const SINGLE_LAYER_LOW_PIN_ENGAGE_THRESHOLD: usize = 3;

/// Peer-count threshold below which the single-layer low-rung pin RELEASES
/// (issue #1156 hysteresis). The pin engages at `> ENGAGE` (≥ 4 others) and only
/// releases at `< RELEASE` (≤ 2 others); at exactly 3 it HOLDS its current state.
///
/// Equal to the engage threshold, which yields a one-peer-wide dead-band at
/// exactly 3 others. Without it the gate flips every AQ tick (1 Hz) when the
/// participant count oscillates 3 ↔ 4 — each flip changes the effective
/// `new_tier_w/h`, trips `tier_dims_changed`, and forces a full
/// `VideoEncoder.configure()` + keyframe, so sustained boundary churn could emit
/// up to one keyframe burst PER SECOND on a weak uplink (#1156). The band makes
/// exact-boundary oscillation a no-op for the pin.
const SINGLE_LAYER_LOW_PIN_RELEASE_THRESHOLD: usize = 3;

/// Decide whether a single-layer publisher should pin its lone stream to the
/// `low` rung (issue #1136), with engage/release hysteresis (issue #1156).
///
/// Returns the NEXT pin state given the CURRENT state and live inputs. Pins iff
/// the publisher is in single-stream mode (`effective_layers == 1`) — a simulcast
/// publisher already lets each receiver pull the cheapest rung its downlink can
/// sustain, so the pin would be redundant and is suppressed — AND the peer count
/// crosses the hysteresis band:
///
/// * `other_peer_count > SINGLE_LAYER_LOW_PIN_ENGAGE_THRESHOLD` (≥ 4) → ENGAGE.
/// * `other_peer_count < SINGLE_LAYER_LOW_PIN_RELEASE_THRESHOLD` (≤ 2) → RELEASE.
/// * in between (exactly 3) → HOLD `currently_pinned` (the dead-band that stops
///   3 ↔ 4 oscillation from flipping the pin every tick — issue #1156).
///
/// `other_peer_count` is the number of REMOTE peers (the relay never echoes the
/// sender's own packets, so the local publisher is NOT counted) — exactly the
/// ">3 others" engage semantics #1136 wants. In simulcast mode the result is
/// always `false` regardless of `currently_pinned`, so a publisher that gains
/// layers releases the pin cleanly.
///
/// Pure function (no atomics / DOM) so the band is host-testable and a mutation
/// of either threshold, the comparison direction, or the layer gate fails a unit
/// test.
#[inline]
fn should_pin_single_layer_low(
    effective_layers: u32,
    other_peer_count: usize,
    currently_pinned: bool,
) -> bool {
    if effective_layers != 1 {
        // Simulcast publisher: the pin never applies — release unconditionally.
        return false;
    }
    if other_peer_count > SINGLE_LAYER_LOW_PIN_ENGAGE_THRESHOLD {
        true
    } else if other_peer_count < SINGLE_LAYER_LOW_PIN_RELEASE_THRESHOLD {
        false
    } else {
        // Dead-band (exactly at the boundary): hold the prior decision so a
        // count oscillating across the boundary cannot flip the pin per-tick.
        currently_pinned
    }
}

/// Compute the next single-layer-low pin value for one AQ tick, given the peer
/// count reading.
///
/// Issue #1172: `peer_count()` returns `None` on a momentarily-busy `inner`
/// borrow. A `None` reading is NOT "0 peers" — treating it as 0 would fall below
/// the release threshold and drop the pin mid-call, forcing a tier reconfigure +
/// keyframe on a spurious read. On `None` this returns the unchanged
/// `currently_pinned` value so the caller's atomic holds its prior state until a
/// real count arrives. On `Some(count)` it defers to [`should_pin_single_layer_low`].
///
/// Pure (no atomics / DOM) so the borrow-fail-hold behavior is host-testable and
/// a mutation that maps `None` onto 0 peers fails a unit test.
#[inline]
fn next_single_layer_pin(
    effective_layers: u32,
    other_peer_count: Option<usize>,
    currently_pinned: bool,
) -> bool {
    match other_peer_count {
        Some(count) => should_pin_single_layer_low(effective_layers, count, currently_pinned),
        // Borrow-fail tick: hold prior state, do not treat as 0 peers.
        None => currently_pinned,
    }
}

/// What one AQ tick asks of the lone single-stream (`n_layers == 1`) encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SingleStreamReconfigure {
    Nothing,
    Dims { bitrate_bps: u32 },
    Bitrate { bitrate_bps: u32 },
}

/// Decide the single-stream reconfigure for this tick (issue #2476).
///
/// `Dims` carries `new_bitrate_bps`, never the cached rate: a tier change
/// is the only `configure()` that tick, so a same-tick bitrate change has to
/// ride it or the encoder keeps the old rate while `local_bitrate` reports the
/// new one — and both bitrate gates key off that cached value.
#[inline]
fn plan_single_stream_reconfigure(
    tier_dims_changed: bool,
    new_bitrate_bps: u32,
    bitrate_attempt_allowed: bool,
) -> SingleStreamReconfigure {
    if tier_dims_changed {
        SingleStreamReconfigure::Dims {
            bitrate_bps: new_bitrate_bps,
        }
    } else if bitrate_attempt_allowed {
        SingleStreamReconfigure::Bitrate {
            bitrate_bps: new_bitrate_bps,
        }
    } else {
        SingleStreamReconfigure::Nothing
    }
}

/// One AQ tick of the camera's WebTransport uplink-DROP self-congestion axis
/// (#1104). Given the cumulative `unistream_drop_count()` reading, the window
/// snapshot, and how long the window has been open, return the
/// [`SelfCongestionDecision`] — applying the WebTransport DROP window/threshold
/// (`WT_SELF_CONGESTION_WINDOW_MS` / `WT_SELF_CONGESTION_DROP_THRESHOLD`), NOT
/// the WebSocket or saturation constants.
///
/// Extracted from the wasm-only AQ loop so the encoder's CHOICE OF SIGNAL — the
/// drop counter wired through `evaluate_self_congestion` with the WT-drop
/// constants — is pinned by a NATIVE `#[test]` (the loop itself depends on
/// `js_sys::Date::now()` and cannot run on host). A mutation that fed the WS
/// constants, the saturation constants, or inverted the comparison changes the
/// returned decision and fails the test. The wasm loop calls this with
/// `videocall_transport::webtransport::unistream_drop_count()` as `current`, so
/// the counter the test reasons about is the one the encoder consults.
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

/// One AQ tick of the camera's WebTransport uplink-SATURATION self-congestion
/// axis (#1219 prerequisite). Mirrors [`wt_drop_step_down_decision`] but applies
/// the SATURATION window/threshold (`WT_SATURATION_WINDOW_MS` /
/// `WT_SATURATION_STALL_THRESHOLD`) over the slow-`ready()` counter. The wasm
/// loop calls this with
/// `videocall_transport::webtransport::unistream_ready_stall_count()` as
/// `current`, so a mutation that fed the drop counter / drop constants here is
/// caught by the native test (the saturation boundary differs).
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

const SCREEN_READY_STALL_INTERVAL_MULTIPLIER: f64 = 8.0;

/// Return the dual-stream WT slow-`ready()` threshold update for this AQ tick.
///
/// The threshold must be refreshed both when sharing starts and whenever the
/// live screen tier changes while sharing remains active. An invalid tier index
/// resolves to the slowest tier, which is the conservative choice: a stale or
/// malformed value must not shorten the threshold and create false saturation.
#[inline]
fn screen_ready_stall_threshold_update(
    was_active: bool,
    is_active: bool,
    previous_tier_index: u32,
    tier_index: u32,
) -> Option<(f64, f64)> {
    if !is_active || (was_active && previous_tier_index == tier_index) {
        return None;
    }

    let tiers = crate::adaptive_quality_constants::SCREEN_QUALITY_TIERS;
    let resolved_index = (tier_index as usize).min(tiers.len().saturating_sub(1));
    let screen_ifi_ms = 1000.0 / f64::from(tiers[resolved_index].target_fps);
    Some((
        SCREEN_READY_STALL_INTERVAL_MULTIPLIER * screen_ifi_ms,
        screen_ifi_ms,
    ))
}

struct ScreenReadyStallThresholdTracker {
    was_active: bool,
    previous_tier_index: u32,
    owner: Option<ReadyStallThresholdOwner>,
}

impl ScreenReadyStallThresholdTracker {
    fn new(initial_tier_index: u32) -> Self {
        Self {
            was_active: false,
            previous_tier_index: initial_tier_index,
            owner: None,
        }
    }

    /// Apply one production AQ tick to the per-encoder threshold owner.
    ///
    /// Returning the arithmetic update lets the caller log the selected tier and
    /// frame interval without duplicating the transition decision.
    fn tick(&mut self, is_active: bool, tier_index: u32) -> Option<(f64, f64)> {
        let update = screen_ready_stall_threshold_update(
            self.was_active,
            is_active,
            self.previous_tier_index,
            tier_index,
        );

        if let Some((threshold_ms, _)) = update {
            if let Some(owner) = self.owner.as_ref() {
                owner.set_threshold_ms(threshold_ms);
            } else {
                self.owner = Some(ReadyStallThresholdOwner::new(threshold_ms));
            }
        }
        if !is_active {
            self.owner.take();
        }

        self.was_active = is_active;
        self.previous_tier_index = tier_index;
        update
    }

    fn release(&mut self) {
        self.owner.take();
        self.was_active = false;
    }
}

/// One AQ tick of the camera's WebTransport stale-DELTA self-congestion axis
/// (#1737 Phase 1). The wasm loop calls this with
/// `videocall_transport::webtransport::unistream_stale_delta_drop_count()`.
#[inline]
fn camera_wt_stale_drop_step_down_decision(
    current_drops: u64,
    snapshot_drops: u64,
    elapsed_ms: f64,
) -> videocall_aq::constants::SelfCongestionDecision {
    use crate::adaptive_quality_constants::{
        evaluate_self_congestion, CAMERA_WT_STALE_DROP_THRESHOLD, CAMERA_WT_STALE_DROP_WINDOW_MS,
    };
    evaluate_self_congestion(
        current_drops,
        snapshot_drops,
        elapsed_ms,
        CAMERA_WT_STALE_DROP_WINDOW_MS,
        CAMERA_WT_STALE_DROP_THRESHOLD,
    )
}

/// Should the encode loop REPUBLISH its layers' geometry on this frame (issue
/// #2170)?
///
/// `true` when a whole-vec CLEAR has happened that this loop has not yet reconciled
/// AND the encoder is enabled. Extracted as a pure fn so the decision is
/// host-testable, mirroring [`video_at_floor_on_tick`]'s shape — the encode loop's
/// own republish call site needs a live `VideoEncoder` and stays browser-only
/// (disclosed with the other unguarded sites).
///
/// # Why a clear GENERATION and not an `enabled` edge
///
/// The first cut of this compared `prev_enabled`/`now_enabled` per frame and missed
/// the case it existed for: the hazard is a camera OFF→ON pair that completes while
/// the loop is parked in `video_reader.read().await`, so the loop never samples the
/// disabled state and on resume sees `(true, true)` — no edge fires, while
/// `start()`'s same-device guard has already early-returned. Comparing counters is
/// edge-independent: `stop()` records its clear whether or not a frame boundary
/// observed it.
///
/// # Why gated on `enabled`, and why "publish whenever cleared" is wrong
///
/// A clear observed while STOPPED must not be undone — that sentinel is `stop()`'s
/// contract. And republishing unconditionally every frame would defeat the
/// change-gating that keeps this channel cheap (the design publishes only on a
/// dimension CHANGE); the caller advances its seen-generation only after a republish,
/// so a clear seen while disabled is repaired on the next enabled frame instead.
fn republish_geometry_on_tick(seen_cleared: u64, live_cleared: u64, enabled: bool) -> bool {
    live_cleared != seen_cleared && enabled
}

/// Compute the camera's `video_at_floor` flag value for one AQ tick.
///
/// On the camera-ENABLE rising edge (`!prev_enabled && now_enabled`) this force-
/// clears to `false`: the mic encoder's audio-after-video backstop gate samples
/// `video_at_floor_flag` on the SAME 1 Hz detector tick (wired via
/// `microphone.set_camera_video_exhausted_signal(camera.video_at_floor_flag())`),
/// and `EncoderBitrateController::video_at_floor()` does NOT read the camera
/// `enabled` flag — so a camera that was disabled while at the floor would leave
/// a STALE `true` that opens the audio backstop prematurely on the first tick
/// after re-enable. Clearing on the rising edge closes that leak; the live
/// detector re-asserts on the next tick if video is genuinely still at floor.
/// Otherwise (steady enabled, steady disabled, or the falling edge) the detector
/// value passes through unchanged. This is the SINGLE source of truth for the
/// per-tick flag value — the AQ loop has exactly one writer of
/// `video_at_floor_flag`, and it stores this fn's result.
#[inline]
fn video_at_floor_on_tick(prev_enabled: bool, now_enabled: bool, detector_at_floor: bool) -> bool {
    if !prev_enabled && now_enabled {
        // Rising edge: force-clear so a stale at-floor reading from the disabled
        // period cannot open the mic backstop on the same tick the camera returns.
        false
    } else {
        detector_at_floor
    }
}

/// Synchronously clear `video_at_floor_flag` on the camera ENABLE rising edge
/// (issue #1678, pre-submit follow-up). Stores `false` IFF this `set_enabled`
/// call actually flipped the flag (`changed`) to enabled (`now_enabled`) — i.e.
/// a real disabled -> enabled transition.
///
/// This is the SYNCHRONOUS counterpart to the in-loop rising-edge clear in
/// [`video_at_floor_on_tick`]: the mic backstop detector runs on its own ~1 Hz
/// loop and can sample `video_at_floor_flag` in the window between the Host
/// flipping `enabled` true and the camera AQ loop's next tick. Clearing here, at
/// the same synchronous point the `enabled` atom flips, closes that cross-loop
/// window so a stale `true` from a prior distress episode cannot open the audio
/// backstop on the re-enable. Extracted as a free fn taking the flag directly so
/// the store can be unit-tested on the native host (the full `CameraEncoder` is
/// wasm-bound).
#[inline]
fn clear_video_at_floor_on_enable_edge(
    video_at_floor_flag: &Arc<AtomicBool>,
    changed: bool,
    now_enabled: bool,
) {
    if changed && now_enabled {
        video_at_floor_flag.store(false, Ordering::Release);
    }
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
    ///   [`SIMULCAST_MAX_SUPPORTED_LAYERS`]; `0` is treated as `1`. The runtime
    ///   flag defaults to 3 (#1082), but the device-capability ceiling may
    ///   reduce the value. Passing 1 explicitly preserves the legacy path.
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
        // Reset the WT uplink-saturation threshold to floor (250ms) to prevent
        // leaking a raised threshold from a prior CameraEncoder that was dropped
        // while screen-sharing (issue #1618 suggested fix). Ensures a clean
        // single-stream baseline for every new encoder instance.
        //
        // GUARDED variant (issue #1670): the reset is SKIPPED when a live encoder
        // still holds a raise. A `Host` re-mount constructs a fresh CameraEncoder
        // while the PRIOR encoder's AQ loop may still be running (its liveness
        // token has not dropped yet) and still holding a raised threshold from an
        // active screen share; an UNCONDITIONAL reset here would clobber that live
        // raise back to the floor and make the still-running dual-stream loop
        // shed video on spurious saturation. The screen-STOP edge and encoder
        // `Drop` release that encoder's registry entry; the registry preserves
        // any surviving owner's maximum and floors after the final release, so a
        // dropped-while-raised encoder cannot leak its threshold (#1667).
        videocall_transport::webtransport::reset_ready_stall_threshold_on_construction();

        let default_tier = &VIDEO_QUALITY_TIERS[0];
        let default_audio_tier = &AUDIO_QUALITY_TIERS[0];
        Self {
            client,
            video_elem_id: video_elem_id.to_string(),
            state: EncoderState::new(),
            current_bitrate: Rc::new(AtomicU32::new(initial_bitrate)),
            current_fps: Arc::new(AtomicU32::new(0)),
            last_layer0_chunk_ms: Arc::new(AtomicU64::new(0)),
            on_encoder_settings_update,
            on_error: Some(on_error),
            on_permission_error: None,
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
            shared_encoder_queue_depth_report: Rc::new(AtomicU32::new(0)),
            shared_encoder_target_bitrate_kbps: Rc::new(AtomicU32::new(0)),
            shared_tier_transitions: Rc::new(RefCell::new(Vec::new())),
            shared_climb_limiter_snapshot: Rc::new(RefCell::new(ClimbLimiterSnapshot::default())),
            shared_dwell_samples: Rc::new(RefCell::new(Vec::new())),
            reelection_completed_signal: Rc::new(AtomicBool::new(false)),
            // Issue #1311: no reset pending at construction; armed by a re-election
            // (quality task) or a reconnect (client `Connected` callback).
            keyframe_cooldown_reset: Rc::new(AtomicBool::new(false)),
            tier_change_keyframe: Rc::new(AtomicBool::new(false)),
            // Issue #1611: camera video NOT exhausted at construction (fresh encoder
            // starts at the default tier, which is above the floor).
            video_at_floor_flag: Arc::new(AtomicBool::new(false)),
            quality_bounds: Rc::new(RefCell::new(SharedQualityBounds::default())),
            max_layers,
            // Simulcast ACTIVE-layer state (issue #989 / #1140 / #1141). Cold-start
            // at the BASE layer only (`initial_active_layer_count()` == 1), NOT the
            // device ceiling: the publisher's ENCODE/EGRESS OUTPUT is byte-identical
            // to the legacy single-stream path at startup, and the AQ control loop
            // *earns* more layers up to `max_layers` at runtime from observed
            // backpressure headroom + uplink budget. Per-layer VideoEncoders are
            // now constructed LAZILY on first activation (issue #1204): cold start
            // allocates only the base-layer encoder, and the encode loop builds an
            // upper rung's encoder the first time the ramp/restore raises the
            // active count past it — so this is byte-identical OUTPUT *and* no
            // longer pre-allocates encoders for un-earned rungs.
            shared_active_layer_count: Rc::new(AtomicU32::new(initial_active_layer_count())),
            // EFFECTIVE ladder depth = the configured device CEILING (#1143
            // observability), distinct from the active count above. Stays at
            // `clamp_layer_count(max_layers)`: #1140/#1141 changed where the
            // *operating point* (active) starts, NOT the configured ceiling the
            // encoder builds its ladder to. The health reporter reads this as
            // "layers this publisher is configured to encode"; the active count is
            // "layers presently being sent" (active <= effective, the gap = the
            // runtime ramp not-yet-earned + AQ shed).
            shared_effective_layer_count: Rc::new(AtomicU32::new(clamp_layer_count(max_layers))),
            shared_layer_bitrates_bps: Rc::new(RefCell::new(Vec::new())),
            // Fitted per-layer encode dims (issue #2170). Empty until the encode
            // loop sizes and publishes; the readout reads unknown while it is.
            shared_layer_dims: Rc::new(RefCell::new(Vec::new())),
            shared_layer_output_fps: Rc::new(RefCell::new(Vec::new())),
            shared_layer_output_fps_ready: Rc::new(RefCell::new(Vec::new())),
            shared_layer_last_chunk_ms: Rc::new(RefCell::new(Vec::new())),
            // Geometry clear-generation (issue #2170). Starts at 0; the encode loop
            // seeds its local copy from this at spawn.
            shared_layer_dims_cleared: Rc::new(AtomicU64::new(0)),
            // Sender encoder backpressure (issue #1108, Phase B). Starts at 0
            // (no frames queued); the encode loop publishes the live depth.
            shared_encoder_queue_depth: Rc::new(AtomicU32::new(0)),
            // Relay layer-union hint (issue #1108, Stage 3). Starts at u32::MAX
            // (fail-open / no cap): the controller keeps its full ladder until a
            // LAYER_HINT arrives. Reset to u32::MAX on reconnect.
            shared_union_requested_layer: Rc::new(AtomicU32::new(u32::MAX)),
            // User SEND layer-ceiling (perf-panel). Fail-open: u32::MAX = Auto /
            // no user cap until the panel writes a layer count.
            shared_user_layer_ceiling: Rc::new(AtomicU32::new(u32::MAX)),
            control_loop_liveness: Rc::new(()),
            control_loop_cancel: AqControlLoopCancel::new(),
            // Single-layer low-rung pin (issue #1136). Starts cleared; the AQ
            // control loop sets it once it observes single-stream mode + >3
            // peers. No effect in simulcast mode.
            single_layer_low_pin: Rc::new(AtomicBool::new(false)),
            screen_ready_stall_tracker: Rc::new(RefCell::new(
                ScreenReadyStallThresholdTracker::new(
                    crate::adaptive_quality_constants::DEFAULT_SCREEN_TIER_INDEX as u32,
                ),
            )),
            loop_running: Arc::new(AtomicBool::new(false)),
            loop_device_id: Rc::new(RefCell::new(None)),
            loop_epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Effective number of simulcast layers to encode this session.
    ///
    /// Clamps the caller-supplied `max_layers` to at least 1 (a `0` request is
    /// meaningless — there is always at least the base layer) and at most
    /// [`SIMULCAST_MAX_SUPPORTED_LAYERS`]. An explicit input of 1 makes the
    /// encode loop run a single layer exactly as before; larger values permit
    /// the runtime AQ ramp to activate upper rungs.
    fn effective_layer_count(&self) -> u32 {
        clamp_layer_count(self.max_layers)
    }

    pub fn control_loop_cancel_token(&self) -> AqControlLoopCancel {
        self.control_loop_cancel.clone()
    }

    /// Spawn the encoder AQ control loop (issue #1108: now a self-timer).
    ///
    /// Receiver-reported FPS no longer drives the sender AQ, so this loop is no
    /// longer fed by a diagnostics channel. It ticks at `AQ_TICK_INTERVAL_MS`,
    /// reading the sender's own encoder-queue backpressure (published by the
    /// encode loop into `shared_encoder_queue_depth`) plus the server-CONGESTION
    /// and WS-send-buffer signals, and applies tier/layer/bitrate decisions.
    ///
    /// `shared_screen_tier_index` must be the live atom owned by the paired
    /// `ScreenEncoder`. Requiring it here makes the threshold wiring part of loop
    /// construction instead of a setter whose ordering could silently freeze the
    /// loop on a stale default atom.
    pub fn set_encoder_control(&mut self, shared_screen_tier_index: Rc<AtomicU32>) {
        let current_bitrate = self.current_bitrate.clone();
        let current_fps = self.current_fps.clone();
        let last_layer0_chunk_ms = self.last_layer0_chunk_ms.clone();
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
        let shared_encoder_queue_depth_report = self.shared_encoder_queue_depth_report.clone();
        let shared_encoder_target_bitrate_kbps = self.shared_encoder_target_bitrate_kbps.clone();
        let shared_tier_transitions = self.shared_tier_transitions.clone();
        let shared_climb_limiter_snapshot = self.shared_climb_limiter_snapshot.clone();
        let shared_dwell_samples = self.shared_dwell_samples.clone();
        let reelection_completed_signal = self.reelection_completed_signal.clone();
        // Issue #1311: the QUALITY task ARMS this when it consumes a re-election
        // (below, at the `notify_reelection_completed` site); the ENCODE task
        // CONSUMES it per frame to clear `last_keyframe_emit_ms`. Both spawn_local
        // tasks share this same `CameraEncoder`-owned atom.
        let keyframe_cooldown_reset_quality = self.keyframe_cooldown_reset.clone();
        let tier_change_keyframe_quality = self.tier_change_keyframe.clone();
        // Issue #1611: the QUALITY task stores this each tick AFTER `tick()`.
        let video_at_floor_flag = self.video_at_floor_flag.clone();
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
        // User SEND layer-ceiling (perf-panel): the control loop READS the layer
        // count the UI wrote and forwards it to the controller's user cap each
        // tick, composed as a further `min` alongside the union cap and the ramp.
        let shared_user_layer_ceiling = self.shared_user_layer_ceiling.clone();
        let control_loop_liveness = Rc::downgrade(&self.control_loop_liveness);
        let control_loop_cancel = self.control_loop_cancel.clone();
        // Single-layer low-rung pin gate (issue #1136, hysteretic per #1156). The
        // loop reads the LIVE peer count from the client each tick (single-stream
        // mode only) and writes the gate decision into this atomic for the encode
        // loop. We clone the client (its inner is an `Rc<RefCell<…>>`, and the
        // encode loop already holds a strong clone for `send_media_packet`, so
        // this is the established lifetime pattern). `peer_count()` is a
        // non-blocking `try_borrow` read that counts off the cached key Rc WITHOUT
        // cloning it (#1156) — if the inner is momentarily busy it returns `None`
        // (NOT `0`). A `None` reading is treated as "no fresh count this tick", so
        // the gate HOLDS its prior pin value rather than releasing it (#1172): a
        // spurious `0` would otherwise fall below the release threshold and flip a
        // pinned publisher off for one tick, emitting a needless keyframe. The next
        // tick (≤1s later) re-reads the real count. In practice the borrow never
        // fails here: all `inner` borrows are short and non-blocking and none is
        // held across an `.await`, so on single-threaded wasm no borrow is active
        // mid-tick — the `None` arm is a correctness fail-safe, not a hot path.
        let peer_count_client = self.client.clone();
        let single_layer_low_pin = self.single_layer_low_pin.clone();
        let screen_ready_stall_tracker = self.screen_ready_stall_tracker.clone();
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
            // is > 1 (issue #989 / #1140 / #1141). Configure the device CEILING to
            // `n_layers` but START the active count at the BASE layer (1): the
            // publisher emits a single legacy-equivalent stream at startup and the
            // headroom-probe ramp in `EncoderBitrateController::tick` earns layers
            // up to the ceiling only when backpressure + uplink budget allow. The
            // controller's `is_simulcast()` keys off the CEILING (not the active
            // count), so the ramp logic runs even while active == 1. n_layers == 1
            // leaves the controller in single-stream mode (no-op) — byte-identical
            // to the legacy path.
            if n_layers > 1 {
                encoder_control.set_simulcast_ceiling_start_at_base(n_layers);
                // Pre-size the per-layer bitrate atomics (lowest layer first).
                let mut atomics = shared_layer_bitrates_bps.borrow_mut();
                if atomics.len() != n_layers {
                    *atomics = (0..n_layers).map(|_| Rc::new(AtomicU32::new(0))).collect();
                }
            }
            let mut prev_screen_active = false;
            // Issue #1678: track the previous camera-enabled state so the AQ loop
            // can detect the camera-ENABLE rising edge itself and force-clear the
            // `video_at_floor_flag` for that tick (see `video_at_floor_on_tick`).
            // Seed from the LIVE value (mirroring the screen loop's `was_sharing`
            // seed) so a loop that begins life with the camera already enabled
            // does NOT treat its first tick as a spurious rising edge. Acquire to
            // match the screen-side load ordering.
            let mut was_enabled = enabled.load(Ordering::Acquire);
            let mut last_ws_drop_snapshot: u64 =
                videocall_transport::websocket::websocket_drop_count();
            let mut ws_drop_window_start_ms: f64 = js_sys::Date::now();
            // Independent sliding window for the WebTransport uplink-backpressure
            // self-trigger (#1104). Kept SEPARATE from the WS window above so the
            // two transports' signals never interfere; on a WS connection the WT
            // counter stays flat at 0 (no unistream sends) and this block is a
            // no-op, symmetric to how the WS block is a no-op under WebTransport.
            let mut last_wt_drop_snapshot: u64 =
                videocall_transport::webtransport::unistream_drop_count();
            let mut wt_drop_window_start_ms: f64 = js_sys::Date::now();
            // Independent sliding window for the WebTransport uplink-SATURATION
            // self-trigger (#1219 prerequisite). SEPARATE from the WT drop window
            // above: the drop counter only moves on stream teardown, whereas this
            // counts slow `writer.ready()` events (a slow-but-alive uplink). Both
            // are WT-only and flat at 0 on WebSocket, so this is a no-op there.
            let mut last_wt_stall_snapshot: u64 =
                videocall_transport::webtransport::unistream_ready_stall_count();
            let mut wt_stall_window_start_ms: f64 = js_sys::Date::now();
            let mut last_wt_stale_drop_snapshot: u64 =
                videocall_transport::webtransport::unistream_stale_delta_drop_count();
            let mut wt_stale_drop_window_start_ms: f64 = js_sys::Date::now();
            // Self-timer AQ loop (issue #1108): tick at AQ_TICK_INTERVAL_MS
            // instead of waiting on receiver diagnostics. Runs for the lifetime
            // of the owning CameraEncoder.
            // The `enabled` flag does NOT terminate the loop — it only gates the
            // bitrate-vs-"Disabled" emit below, so a muted-then-unmuted camera
            // keeps adapting without re-arming.
            loop {
                gloo_timers::future::sleep(std::time::Duration::from_millis(
                    crate::adaptive_quality_constants::AQ_TICK_INTERVAL_MS,
                ))
                .await;
                if crate::encode::aq_control_loop_should_exit(
                    &control_loop_liveness,
                    &control_loop_cancel,
                ) {
                    log::debug!("CameraEncoder: AQ control loop exiting (dropped or cancelled)");
                    break;
                }
                let now = js_sys::Date::now();
                // #2060 idle-decay: `last_layer0_chunk_ms` is stamped in the layer-0 callback
                // with performance().now() (monotonic), so the staleness check MUST use a fresh
                // performance().now() here — NOT the loop's `now` above (js_sys::Date::now(),
                // wall-clock, a DIFFERENT epoch). Do not "simplify" by reusing `now`: a
                // cross-clock comparison silently breaks the decay and no unit test catches it
                // (the wiring is native-untestable; only this contract guards it).
                let perf_now = window()
                    .performance()
                    .expect("Performance API not available")
                    .now();
                if let Some(value) = crate::encode::fps_after_idle_decay(
                    current_fps.load(Ordering::Relaxed),
                    perf_now,
                    last_layer0_chunk_ms.load(Ordering::Relaxed) as f64,
                    crate::adaptive_quality_constants::ENCODER_FPS_IDLE_DECAY_MS,
                ) {
                    current_fps.store(value, Ordering::Relaxed);
                }

                // Single-layer low-rung pin gate (issue #1136 + #1156 hysteresis).
                // ONLY meaningful in single-stream mode (n_layers == 1): a lone
                // adaptive (medium-tier) stream is heavy on every receiver's
                // decoder, so once the call has more than 3 OTHER peers we pin this
                // publisher's single stream to the `low` rung (640×360) instead.
                // `peer_count()` returns `Some(count)` of REMOTE peers only (the
                // relay never echoes our own packets back, and session_id 0 /
                // self is never inserted into the peer decode manager) — exactly
                // the ">3 others" engage semantics #1136 wants (a 5+-participant
                // call). It reads `.len()` off the cached `Rc<Vec<String>>`
                // WITHOUT cloning it, so this 1 Hz hot loop no longer allocates a
                // `Vec<String>` per tick just to count peers (#1156). It returns
                // `None` on a busy borrow — see the skip below (#1172).
                //
                // The decision is hysteretic (#1156): the pin engages at > 3 and
                // releases at < 3, holding its prior state at exactly 3. Feeding
                // the CURRENT atomic value as `currently_pinned` gives the dead-band
                // its memory, so a participant count oscillating 3 ↔ 4 cannot flip
                // the pin every tick (each flip would change `new_tier_w/h`, trip
                // `tier_dims_changed`, and force a keyframe-emitting reconfigure —
                // up to 1 keyframe/sec on a weak uplink). In simulcast mode
                // (n_layers > 1) the pin stays cleared — the receiver-driven layer
                // chooser already sheds cost there.
                if n_layers == 1 {
                    // Issue #1172: a momentarily-busy `inner` borrow returns
                    // `None`, which is NOT "0 peers". Feeding 0 here would fall
                    // below the release threshold and drop the pin mid-call,
                    // forcing a tier reconfigure + keyframe on a spurious read.
                    // `next_single_layer_pin` HOLDS the prior pin on `None` so the
                    // atomic keeps its value until a real count arrives.
                    let other_peers = peer_count_client.peer_count();
                    let currently_pinned = single_layer_low_pin.load(Ordering::Relaxed);
                    single_layer_low_pin.store(
                        next_single_layer_pin(n_layers as u32, other_peers, currently_pinned),
                        Ordering::Relaxed,
                    );
                }

                // Check for screen sharing state transitions and coordinate
                // camera quality to avoid bandwidth contention.
                let screen_active = screen_sharing_active.load(Ordering::Acquire);
                let screen_tier_index = shared_screen_tier_index.load(Ordering::Acquire);
                if let Some((threshold, screen_ifi_ms)) = screen_ready_stall_tracker
                    .borrow_mut()
                    .tick(screen_active, screen_tier_index)
                {
                    log::info!(
                        "CameraEncoder: WT stall threshold set to {:.0}ms (dual-stream, \
                         screen tier={}, IFI={:.0}ms)",
                        threshold,
                        screen_tier_index,
                        screen_ifi_ms,
                    );
                }
                if screen_active != prev_screen_active {
                    prev_screen_active = screen_active;
                    encoder_control.notify_screen_sharing(screen_active);

                    // Frame-rate-aware WT uplink-saturation threshold (issues
                    // #1618 and #1669). The update above tracks both the sharing
                    // start edge and live screen-tier changes. A lower screen fps
                    // has a longer frame interval and therefore needs a higher
                    // threshold to avoid K-amplification false positives. WS
                    // publishers execute the write but never read the value (the
                    // WT stall counter remains flat on WS).
                    // The tracker owns one registry entry while this screen is
                    // active. On stop it drops that owner and the transport
                    // atomically republishes the maximum value still required by
                    // any overlapping Host encoder, or the floor when none remain.

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

                // Client-side WebTransport uplink-backpressure detection
                // (issue #1104, 2026-06-09 meeting_sync analysis).
                //
                // The WS block above is a no-op for WebTransport users
                // (websocket_drop_count() is always 0 on WT). On WebTransport,
                // audio/video/screen ride PERSISTENT unidirectional QUIC
                // streams; when a media-frame write fails (stream reset / fatal
                // backpressure) the frame is dropped and unistream_drop_count()
                // increments. That is the true client-side WT analogue of the
                // WS send-buffer drop — a real media frame that did not leave
                // the uplink. (Datagrams carry only heartbeats/RTT probes, so
                // datagram_drop_count() is NOT used here.) When a SUSTAINED
                // cluster of drops accumulates within the window we self-shed a
                // layer without waiting for the slower, indirect server
                // CONGESTION signal. The window/snapshot are independent of the
                // WS window and the server-congestion flag, and each axis sheds
                // at most one layer per window, so the paths cannot compound
                // into a runaway double step-down. For WebSocket users this
                // counter stays flat at 0, so the block is a true no-op.
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
                    if decision.step_down {
                        log::warn!(
                            "CameraEncoder: client WT uplink backpressure detected ({} unistream \
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

                // Client-side WebTransport uplink-SATURATION detection
                // (#1219 prerequisite). The WT DROP block above only fires on
                // stream/connection TEARDOWN (STOP_SENDING / RESET_STREAM /
                // close); it stays FLAT on a slow-but-alive uplink because the WT
                // media send path is `.await`-blocking and a WritableStream
                // signals backpressure by leaving `writer.ready()` PENDING, not
                // by rejecting the write. So a genuine bandwidth cliff (link slow,
                // ACKs flowing, no reset) would NEVER self-shed on the drop
                // counter. The transport therefore also exposes
                // `unistream_ready_stall_count()`, incremented once per slow
                // `writer.ready().await` (> producer-side READY_STALL_THRESHOLD_MS)
                // on the established media path. A SUSTAINED cluster of those
                // within the window means the uplink is saturated, so we self-shed
                // a layer — the same gentle, single-rung `force_video_step_down`
                // the drop/WS blocks use (NOT `force_congestion_cut`): this is the
                // publisher's OWN gradual uplink adaptation, where one rung per
                // window is the right granularity; the hard multi-tier cut is
                // reserved for the server-authored CONGESTION path, which is a
                // stronger, externally-corroborated signal. Window/snapshot are
                // INDEPENDENT of the WT drop, WS, and server-congestion paths;
                // each axis sheds at most one layer per its own window, so they
                // cannot compound into a runaway double step-down. WS users hold
                // this counter flat at 0 → true no-op. This is the signal that
                // lets the relay's room-wide sender-keyed CONGESTION (bug #1219)
                // be removed: a WT publisher now sees its own uplink saturation
                // directly.
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
                    if decision.step_down {
                        log::warn!(
                            "CameraEncoder: client WT uplink saturation detected ({} slow ready() \
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

                // Client-side WebTransport camera stale-DELTA detection
                // (#1737 Phase 1). The WT send path deliberately skips old
                // camera deltas after `writer.ready()` resolves, while always
                // sending keyframes. A sustained cluster means the encoder is
                // still producing above what the uplink can deliver, so this
                // independent axis steps the camera down instead of silently
                // eating frames at full encode rate.
                {
                    let current_wt_stale_drops =
                        videocall_transport::webtransport::unistream_stale_delta_drop_count();
                    let elapsed_ms = now - wt_stale_drop_window_start_ms;
                    let decision = camera_wt_stale_drop_step_down_decision(
                        current_wt_stale_drops,
                        last_wt_stale_drop_snapshot,
                        elapsed_ms,
                    );
                    if decision.step_down {
                        log::warn!(
                            "CameraEncoder: client WT stale-delta backpressure detected ({} \
                             camera deltas dropped in {:.0}ms), forcing video step-down",
                            current_wt_stale_drops.saturating_sub(last_wt_stale_drop_snapshot),
                            elapsed_ms,
                        );
                        encoder_control.force_video_step_down();
                    }
                    if decision.roll_window {
                        last_wt_stale_drop_snapshot = decision.new_snapshot;
                        wt_stale_drop_window_start_ms = now;
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
                // User SEND layer-ceiling (perf-panel): feed the latest user-
                // selected layer COUNT (u32::MAX = Auto / no cap → usize::MAX
                // fail-open). Applied right before `tick` so the cap composes with
                // the union hint and backpressure as a further `min`. The base
                // layer is always published (the AQ side floors at 1).
                encoder_control.observe_user_layer_ceiling(layer_ceiling_to_count(
                    shared_user_layer_ceiling.load(Ordering::Relaxed),
                ));
                encoder_control.tick(now);
                let output_wasted = Some(encoder_control.last_target_bitrate_kbps());

                // Issue #1611 / #1678: store whether camera video is exhausted (tier at
                // user-capped floor AND active layers at 1) for the mic encoder's
                // audio-after-video backstop gate, which reads this on the same 1 Hz tick.
                // The per-tick value flows through `video_at_floor_on_tick` — the SINGLE
                // writer of `video_at_floor_flag` in this loop — which passes the detector
                // value through in steady state but force-clears on the camera-ENABLE rising
                // edge so a stale `true` from a disabled-while-at-floor period cannot open the
                // backstop prematurely (#1678, mirroring the screen rising-edge clear).
                let now_enabled = enabled.load(Ordering::Acquire);
                let at_floor = video_at_floor_on_tick(
                    was_enabled,
                    now_enabled,
                    encoder_control.video_at_floor(),
                );
                was_enabled = now_enabled;
                video_at_floor_flag.store(at_floor, Ordering::Release);

                // Write encoder decision inputs to shared atomics for health
                // reporting. Issue #1184: the dead receiver-FPS-derived ratios
                // (`encoder_fps_ratio` / `encoder_bitrate_ratio`) and their
                // shared atomics have been removed; `encoder_queue_depth()`
                // carries the sender backpressure signal (encoder queue depth)
                // through the existing host telemetry channel.
                shared_encoder_queue_depth_report.store(
                    (encoder_control.encoder_queue_depth() as f32).to_bits(),
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
                    // Issue #1311: arm the forced-keyframe cooldown reset so the
                    // FIRST post-re-election PLI emits immediately. The encode loop
                    // (a separate spawn_local task) consumes the dedicated atom and
                    // clears `last_keyframe_emit_ms`. We ARM here, piggybacking on
                    // the existing re-election consume, rather than having the encode
                    // loop ALSO `.swap` `reelection_completed_signal`: that atom is
                    // already swap-consumed in exactly one place per encoder (this
                    // line) — and is shared with the screen encoder's quality task,
                    // which swap-consumes its own copy — so adding a THIRD swap
                    // consumer (the encode loop) would race the existing two and lose
                    // the edge half the time. Storing into a separate single-consumer
                    // atom avoids that race entirely.
                    keyframe_cooldown_reset_quality.store(true, Ordering::Release);
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
                    let tier_index = encoder_control.video_tier_index();
                    apply_camera_tier_change(
                        CameraTierChangeTargets {
                            tier_max_width: &tier_max_width,
                            tier_max_height: &tier_max_height,
                            tier_keyframe_interval: &tier_keyframe_interval,
                            shared_video_tier_index: &shared_video_tier_idx,
                            tier_change_keyframe: &tier_change_keyframe_quality,
                            keyframe_cooldown_reset: &keyframe_cooldown_reset_quality,
                        },
                        tier,
                        tier_index,
                    );
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

    /// Returns the camera ENABLED flag (`Arc<AtomicBool>`): the camera-on/off
    /// signal (issue #1398).
    ///
    /// This is the SAME `EncoderState::enabled` atom that [`Self::set_enabled`]
    /// writes (`set_enabled(true)` → camera ON, `set_enabled(false)` / `stop()`
    /// → camera OFF). The `Host` toggles it directly: it calls
    /// `camera.set_enabled(true)` when video is on and `camera.set_enabled(false)`
    /// (plus `stop()`) when video is off (audio-only). So `false` is an
    /// UNAMBIGUOUS, always-current "camera off / audio-only" indication — unlike
    /// the shared audio-tier atoms, whose values (top-tier 50 kbps / index 0) are
    /// indistinguishable between "camera off" and "camera on and healthy".
    ///
    /// Shared into the [`MicrophoneEncoder`] (via
    /// [`MicrophoneEncoder::set_camera_active_signal`]) so the mic-side
    /// single-layer uplink-distress detector (#1398) can GATE itself to
    /// camera-off: the mic bitrate-floor lever and the camera AQ loop's audio
    /// downshift are then mutually exclusive (no compounding). `Arc` (not `Rc`)
    /// because the atom is `EncoderState`'s `Arc<AtomicBool>` and it crosses into
    /// the mic encoder, matching the congestion-flag wiring.
    pub fn camera_enabled_flag(&self) -> Arc<AtomicBool> {
        self.state.enabled.clone()
    }

    /// Returns the camera video-at-floor flag (`Arc<AtomicBool>`): `true` when
    /// the camera AQ's video quality is fully exhausted (tier at user-capped
    /// floor AND active simulcast layers at 1) (issue #1611).
    ///
    /// Shared into the [`MicrophoneEncoder`] (via
    /// [`MicrophoneEncoder::set_camera_video_exhausted_signal`]) so the mic-side
    /// uplink-distress detector's backstop gate can open even with the camera on
    /// when video can't shed further — the "camera-on but video exhausted →
    /// audio may shed" path. Updated unconditionally by the camera AQ control
    /// loop on every tick (AFTER `encoder_control.tick()`).
    pub fn video_at_floor_flag(&self) -> Arc<AtomicBool> {
        self.video_at_floor_flag.clone()
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
    ///
    /// Since #2170 the RESOLUTION is not resolved from the tier table — see the
    /// `video_width` comment inside, and [`LiveQualitySnapshot::video_width`] for
    /// the field contract.
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
        // Issue #2170: report the geometry the encode loop REQUESTED, not the AQ
        // tier's bounding box.
        //
        // `VIDEO_QUALITY_TIERS[v_idx]` is a ceiling the capture is fitted inside
        // (never upscaled), so on a 4:3 640×480 webcam this read `854x480` for tier
        // `medium` while every layer encoded 640×480 or below — a resolution the user
        // was never sending. The self-view readout is exactly the surface someone
        // checks to answer "what am I publishing right now", so it must carry the
        // fitted value.
        //
        // The TOP ACTIVE layer is the right one to show: it is the best stream any
        // receiver can pull, and in single-stream mode it is the only one.
        //
        // Reports the `(0, 0)` NOT-YET-PUBLISHED sentinel — not the tier box — when
        // nothing has published. Substituting the box here would be the same
        // defect one state over: it renders a ceiling in a format that means
        // "measured", and it is unfalsifiable because a tier box is never 0. Two
        // consequences of that substitution, both real: `host.rs`'s
        // `self_metrics_overlay` gates its resolution on
        // `video_width > 0 && video_height > 0`, so the self tile could NEVER show
        // "unknown"; and after `stop()` clears the atomics the readout would
        // silently revert to the box for a camera that is off. The consumer decides
        // how to present the sentinel — see `format_video_readout`, which renders an
        // em-dash.
        let (fitted_w, fitted_h) = self.top_published_layer_dims().unwrap_or((0, 0));
        LiveQualitySnapshot {
            video_tier_index: v_idx,
            video_width: fitted_w,
            video_height: fitted_h,
            video_fps: v.target_fps,
            video_ideal_kbps: v.ideal_bitrate_kbps,
            audio_tier_index: a_idx,
            audio_kbps: a.bitrate_kbps,
            target_bitrate_kbps,
        }
    }

    /// The fitted dims of the highest ACTIVE layer that has published geometry, or
    /// `None` if none has (issue #2170).
    ///
    /// Two separate mechanisms, easy to conflate:
    /// - The `[..active]` bound EXCLUDES shed rungs. A built-then-shed rung keeps
    ///   NON-zero published dims (only `clear_layer_dim` at dwell-teardown zeroes
    ///   it), so it would otherwise be reported as what this publisher is sending
    ///   when the AQ has stopped sending it — the over-report this bound exists to
    ///   prevent.
    /// - The downward scan then skips rungs with no published dims at all, so a
    ///   not-yet-built rung inside the active range still yields the best live
    ///   geometry rather than `None`.
    ///
    /// The bound handles shed rungs; the scan handles unbuilt ones. Neither
    /// substitutes for the other.
    pub(crate) fn top_published_layer_dims(&self) -> Option<(u32, u32)> {
        // `.max(1)` is DEFENSIVE, not load-bearing: a count of 0 is unreachable in
        // production. `CameraEncoder::new` seeds this to `initial_active_layer_count()
        // == 1`, and the only runtime writer is the AQ loop, whose source
        // `AdaptiveQualityManager::active_layer_count` is floored at 1
        // (`drop_top_layer` returns early at `<= 1`). Only a test can seed 0. Kept so
        // a future writer that does not floor cannot silently blank the readout.
        let active = self
            .shared_active_layer_count
            .load(Ordering::Relaxed)
            .max(1) as usize;
        let dims = self.shared_layer_dims.try_borrow().ok()?;
        // `.min(dims.len())` IS load-bearing, and it is a PANIC GUARD — not part of
        // the shed bound, which is `active` itself. Do not "simplify" it away as
        // redundant: `active > dims.len()` is a REAL state, because
        // `shared_layer_dims` starts empty and `publish_layer_dims` grows it on
        // demand, so the whole window between the AQ raising the active count and
        // `build_layer` publishing has more active rungs than slots. Dropping this
        // does not mis-report — it panics on the slice index (`range end index 1 out
        // of range for slice of length 0`, RUN), on a `pub(crate)` path reached from
        // the UI render tick. Two tests catch it:
        // `quality_snapshot_reports_the_sentinel_before_any_publish` and
        // `top_published_layer_dims_picks_the_highest_published_active_rung`.
        let upto = active.min(dims.len());
        dims[..upto]
            .iter()
            .rev()
            .map(|a| unpack_layer_dims(a.load(Ordering::Relaxed)))
            .find(|(w, h)| *w > 0 && *h > 0)
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

    /// Returns the reported encoder-queue-depth telemetry atomic (f32 bits).
    pub fn shared_encoder_queue_depth_report(&self) -> Rc<AtomicU32> {
        self.shared_encoder_queue_depth_report.clone()
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

    /// Returns the EFFECTIVE simulcast layer-count atomic (#1143): how many
    /// layers this publisher is currently configured to encode/send. Cloned into
    /// the health reporter, which reads the current atomic value each packet.
    /// (At this tip the atomic is written once at construction — see the field
    /// doc on `shared_effective_layer_count`; #1135/#1136 will update it.)
    pub fn shared_effective_layer_count(&self) -> Rc<AtomicU32> {
        self.shared_effective_layer_count.clone()
    }

    /// Returns the ACTIVE simulcast layer-count atomic (#1143): how many of the
    /// effective layers are presently active (encoded + sent). The AQ control
    /// loop writes this; it is `<=` the effective count, the gap being shed
    /// layers. Cloned into the health reporter for the active-layers metric.
    pub fn shared_active_layer_count(&self) -> Rc<AtomicU32> {
        self.shared_active_layer_count.clone()
    }

    /// Returns an opaque source that reads the encode loop's live per-layer
    /// fitted geometry and measured output fps on each health-report tick.
    pub fn camera_layer_metric_source(&self) -> CameraLayerMetricSource {
        CameraLayerMetricSource {
            dims: self.shared_layer_dims.clone(),
            output_fps: self.shared_layer_output_fps.clone(),
            output_fps_ready: self.shared_layer_output_fps_ready.clone(),
            last_chunk_ms: self.shared_layer_last_chunk_ms.clone(),
            active_layer_count: self.shared_active_layer_count.clone(),
        }
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

    /// Returns a shared reference to the forced-keyframe cooldown reset (issue #1311).
    ///
    /// The atom is OWNED by this `CameraEncoder` (not the client) — same ownership
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

    /// Wire the classified permission-error callback. Fired only at the real
    /// `getUserMedia` rejection site (see [`Self::start`]) with the specific
    /// [`MediaPermissionsErrorState`], INSTEAD OF the generic `on_error` at that
    /// site (so the UI shows exactly one modal, not two).
    pub fn set_permission_error_callback(
        &mut self,
        on_permission_error: Callback<MediaPermissionsErrorState>,
    ) {
        self.on_permission_error = Some(on_permission_error);
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

    /// Set the user's SEND layer-ceiling from the performance panel — the
    /// "layers published" control.
    ///
    /// `ceiling` is the maximum number of simulcast layers the user wants this
    /// camera publisher to emit, as a layer COUNT (1 = base only, 2 = base + one,
    /// up to the device ceiling). `None` = Auto / no user cap (the full
    /// backpressure-governed ladder). Applied LIVE: the AQ control loop reads this
    /// atomic each tick (≤1s) and caps the published set as a further `min`
    /// alongside the relay union hint and the runtime ramp; AQ shedding stays
    /// authoritative on the down side and the base layer (layer 0) is always
    /// published (the AQ side floors the cap at 1).
    ///
    /// Valid whether or not the encoder is currently running; the value persists
    /// in the shared atomic and is re-read by the control loop on every
    /// (re)start, so it survives an encoder restart / reconnect with no re-arming
    /// (the atomic is owned by the `CameraEncoder`, which the Host re-applies its
    /// stored preference to after re-init).
    pub fn set_user_layer_ceiling(&self, ceiling: Option<u32>) {
        // None (Auto) → the u32::MAX fail-open sentinel; otherwise the layer
        // count. `layer_ceiling_to_count` maps the sentinel back on the read side.
        self.shared_user_layer_ceiling
            .store(ceiling.unwrap_or(u32::MAX), Ordering::Relaxed);
    }

    /// The current user SEND layer-ceiling (layer COUNT), or `None` for Auto /
    /// no user cap. For the UI to render its current selection.
    pub fn user_layer_ceiling(&self) -> Option<u32> {
        match self.shared_user_layer_ceiling.load(Ordering::Relaxed) {
            u32::MAX => None,
            n => Some(n),
        }
    }

    // The next three methods delegate to self.state

    /// Enables/disables the encoder.   Returns true if the new value is different from the old value.
    ///
    /// The encoder starts disabled, [`encoder.set_enabled(true)`](Self::set_enabled) must be
    /// called prior to starting encoding.
    ///
    /// Disabling encoding after it has started will cause it to stop.
    pub fn set_enabled(&mut self, value: bool) -> bool {
        let changed = self.state.set_enabled(value);
        // Issue #1678: on the camera disable -> re-enable RISING edge, clear the
        // `video_at_floor_flag` SYNCHRONOUSLY here — not only on the next camera
        // AQ tick (~1 Hz later). The mic backstop detector runs on its OWN
        // independent loop and reads (`camera_active`, `camera_video_exhausted`)
        // = (this same `EncoderState::enabled` atom, `video_at_floor_flag`). The
        // Host flips `enabled` true here; if the mic detector's interval fires in
        // the window before the camera AQ tick re-evaluates, it would observe a
        // STALE `true` from a prior distress episode together with the now-true
        // `camera_active` and open the audio backstop on the very re-enable tick
        // this fix targets (caught in pre-submit). Clearing on the rising edge
        // closes that cross-loop window; the live detector re-asserts on the next
        // camera AQ tick if video is genuinely still at floor. (`video_at_floor_on_tick`
        // applies the same rising-edge clear in-loop for re-enables that route
        // through this method. The one path that bypasses BOTH — `start()`'s raw
        // `state.set_enabled(true)` during a device switch, where the loop also
        // seeds `was_enabled = true` and sees no in-loop edge — is benign: a
        // device switch happens while the camera is RUNNING, so the flag carries
        // a LIVE at-floor reading, not the stale-from-disabled value this guards.)
        clear_video_at_floor_on_enable_edge(&self.video_at_floor_flag, changed, value);
        changed
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
        crate::encode::reset_output_fps(&self.current_fps);
        clear_layer_output_metrics(
            &self.shared_layer_output_fps,
            &self.shared_layer_output_fps_ready,
            &self.shared_layer_last_chunk_ms,
        );
        // Issue #2170: published geometry must stop reading as live the moment the
        // encoder does. Without this, turning the camera off after it has been on
        // once leaves the last dims standing for the rest of the session —
        // `CameraEncoder` is constructed once per Host mount and `start`/`stop`
        // merely toggle it, so nothing else would ever reset them. Same contract,
        // same place, as the fps reset above.
        clear_layer_dims(&self.shared_layer_dims);
        // Record the clear so a LIVE encode loop republishes even if it never
        // observes the disable — see `shared_layer_dims_cleared`. `stop()` does not
        // bump `loop_epoch`, and a camera OFF→ON on the same device makes `start()`
        // early-return, so without this a stalled loop's geometry stays blank for the
        // rest of the session behind a working camera.
        self.shared_layer_dims_cleared
            .fetch_add(1, Ordering::Release);
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
        let last_layer0_chunk_ms = self.last_layer0_chunk_ms.clone();
        let tier_max_width = self.tier_max_width.clone();
        let tier_max_height = self.tier_max_height.clone();
        let tier_keyframe_interval = self.tier_keyframe_interval.clone();
        // Issue #1531: the live camera AQ tier index. The encode loop READS it per
        // frame to resolve the tier-aware periodic-keyframe wall-clock ceiling
        // (`camera_periodic_keyframe_max_interval_ms`); the two lowest tiers relax
        // the #1510 cap so a scarce-bandwidth publisher spends fewer forced I-frames.
        let shared_video_tier_index = self.shared_video_tier_index.clone();
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
        // Fitted per-layer encode dims (issue #2170). The encode loop publishes
        // configuration changes through `LayerEncoder::set_encode_dims`; it also
        // owns the generation clear, shed clear, and OFF→ON republish below.
        // `stop()` owns the synchronous whole-vec clear.
        //
        // THE CALL SITES IN THIS FUTURE ARE HOST-UNGUARDED — TEN of them (sites 8-10
        // are the per-layer fps writes added by issue #2170, documented at
        // `shared_layer_output_fps`' clone below), all needing a real `VideoEncoder`
        // (or a live frame read) to reach, so no host test can cover them and
        // reverting any ONE leaves the whole client suite green. Enumerated, because
        // a count alone hides which seams are open:
        //   1-4. the FOUR `LayerEncoder::set_encode_dims` call sites — `build_layer`'s
        //        initial publish, the single-stream tier-change re-fit, the per-frame
        //        simulcast re-fit, and the per-frame single-stream re-fit. Replacing
        //        ANY of them with a direct `current_w`/`current_h` assignment (i.e.
        //        bypassing the chokepoint, which is exactly the drift the chokepoint
        //        exists to prevent) was RUN and left 807/807 passing.
        //   5.   the `clear_layer_dim` call in the shed-teardown loop.
        //   6.   the `clear_layer_dims` call at the `'restart` head, including its
        //        epoch guard — an inline atomic load, so deleting or inverting the
        //        `if` is also invisible to the host suite.
        //   7.   the OFF→ON republish loop that reconciles `seen_dims_cleared`. Its
        //        DECISION is host-tested via `republish_geometry_on_tick` (including
        //        the unobserved-transition case an earlier edge-based fix got wrong),
        //        but deleting the `for layer in &layers { publish_layer_dims(..) }`
        //        body leaves the suite green — the same callee-vs-call-site split as
        //        1-4. Reaching it needs a stalled `video_reader.read().await` across a
        //        same-device OFF→ON cycle, which is a device-level stall no host test
        //        can stage.
        //
        // WHAT WOULD CLOSE THESE, and why it is not in this PR. They need a live
        // `VideoEncoder` fed real camera frames, inside the spawned encode task.
        // `videocall-client` does have a wasm test target (`tests/
        // capability_probe_wasm.rs` + the `wasm-bindgen-test` dev-dep), so "no
        // harness exists" would be a FALSE deferral — but that target only exercises
        // pure fns over synthetic `JsValue`s, has no way to construct a
        // `CameraEncoder`, and is named in NO CI workflow, so it runs nowhere today.
        // Covering these ten needs a new browser harness that drives a real capture
        // through the encode loop AND a CI step to run it: peer-sized work, not a
        // loose end of this change. What IS covered end-to-end instead is the
        // OBSERVABLE RESULT of sites 1-4 — `e2e/tests/performance-settings.spec.ts`
        // asserts the rendered readout and the self-tile overlay both report fitted
        // geometry from a live encoder, and both were mutation-verified by reverting
        // `live_quality_snapshot` and rebuilding the stack wasm. That does not pin
        // the individual call sites, which is why they stay disclosed here.
        //
        // The extracted helpers (`publish_layer_dims`, `clear_layer_dim(s)`) DO have
        // mutation-verified host tests. Those pin the HELPERS and not these CALLS —
        // the distinction matters, because a test on a callee leaves every caller
        // revertible-green. An earlier version of this comment said "three", counting
        // the four `set_encode_dims` callers as the single publish inside the callee;
        // running the call-site mutations disproved that. Closing these needs
        // `#[wasm_bindgen_test]`.
        let shared_layer_dims = self.shared_layer_dims.clone();
        // Per-layer measured output fps mirrors the geometry lifecycle's clearing
        // sites. It deliberately has no OFF-to-ON republish: measurements re-warm
        // from fresh encoded output.
        //
        // Sites 8-10, unguarded for the same reason as 1-7, all added by issue #2170:
        //   8.  `publish_layer_output_fps` in the output closure — the chokepoint
        //       holding the `layer_id == 0` gate on `current_fps` (the AQ setpoint).
        //       Keep the store behind the helper: bypassing it inflates the setpoint N×.
        //   9.  `clear_layer_output_metrics` at the `'restart` head (beside site 6).
        //   10. `clear_layer_output_metric` in shed teardown (beside site 5).
        // Tests pin the HELPERS, not these CALLS. Mutation results: PR for issue #2170.
        let shared_layer_output_fps = self.shared_layer_output_fps.clone();
        let shared_layer_output_fps_ready = self.shared_layer_output_fps_ready.clone();
        let shared_layer_last_chunk_ms = self.shared_layer_last_chunk_ms.clone();
        // Geometry clear-generation (issue #2170): the loop republishes when this
        // moves, which is how a `stop()` it never observed still gets repaired.
        let shared_layer_dims_cleared = self.shared_layer_dims_cleared.clone();
        // Sender encoder backpressure (issue #1108, Phase B): the encode loop
        // WRITES the max active-layer encode_queue_size() here each frame.
        let shared_encoder_queue_depth = self.shared_encoder_queue_depth.clone();
        // Single-layer low-rung pin (issue #1136): the AQ control loop WRITES
        // this gate (single-stream + >3 peers); the single-stream encode path
        // READS it per frame to cap resolution/bitrate to the `low` rung. Always
        // `false` in simulcast mode, so the read is a no-op there.
        let single_layer_low_pin = self.single_layer_low_pin.clone();
        // Issue #1311: the ENCODE loop CONSUMES this each frame (`.swap(false)`)
        // and clears `last_keyframe_emit_ms` when set, so the first PLI after a
        // reconnect/re-election is not coalesced away by a stale pre-transition
        // cooldown timestamp. ARMED by the quality task (re-election) and the
        // client's `Connected` callback (reconnect).
        let keyframe_cooldown_reset = self.keyframe_cooldown_reset.clone();
        let tier_change_keyframe = self.tier_change_keyframe.clone();
        let device_id = if let Some(vid) = &self.state.selected {
            vid.to_string()
        } else {
            return;
        };

        // No-op if not enabled (mirrors MicrophoneEncoder::start():396). This is
        // the documented contract — start() does nothing unless set_enabled(true)
        // has been called. Placing it BEFORE the running-guard guarantees that by
        // the time we reach the guard, is_enabled() is true, so a raised
        // `switching` unambiguously means a device switch.
        if !self.state.is_enabled() {
            log::debug!("CameraEncoder::start() called but encoder is not enabled");
            return;
        }

        // Single-loop + correct-device guard (issue #1295). `loop_running` is the
        // "already running" canary (mirrors the mic's `is_instantiated()`);
        // `loop_device_id` records which device the live loop is bound to. We are
        // enabled here (checked above), so a loop already being alive means we
        // must tear it down before spawning the replacement in TWO cases, then
        // re-enable so the new loop survives its per-frame check:
        //
        //   1. Explicit switch: the UI raised `switching` (select() while
        //      enabled).
        //   2. Different-device with NO `switching` raised — the OFF→switch→ON
        //      hole: select() ran while disabled so it never set `switching`, and
        //      the live loop captured its device_id at spawn and reuses it across
        //      every `'restart`, so it can never retarget itself.
        //
        // In BOTH cases we stop() the stale loop (clearing enabled+switching) and
        // then set_enabled(true) for the new loop. The stale loop is forced to
        // exit by the per-frame/acquire epoch check (its captured epoch is now
        // stale), NOT by the enabled flip — so re-enabling for the new loop does
        // not keep the old one alive. A true SAME-device duplicate returns early
        // instead: the early not-enabled guard above already guarantees we are
        // enabled here, so the early-return only has to test (running, no
        // switching, bound == selected). Spawning a second loop would race two
        // getUserMedia/set_src_object acquisitions on the shared <video>.
        let running = self.loop_running.load(Ordering::Acquire);
        if running {
            let switch_requested = switching.load(Ordering::Acquire);
            let bound = self.loop_device_id.borrow().clone();
            let same_device = bound.as_deref() == Some(device_id.as_str());
            if !switch_requested && same_device {
                // True duplicate on the SAME device: the live loop is already
                // correct — do nothing.
                return;
            }
            // Explicit switch OR different-device-no-switching: tear down the
            // stale loop and re-enable for the replacement.
            self.stop();
            self.state.set_enabled(true);
        }
        crate::encode::reset_output_fps(&self.current_fps);
        let on_error = self.on_error.clone();
        let on_permission_error = self.on_permission_error.clone();

        log::info!(
            "CameraEncoder::start(): using video device_id = {}",
            device_id
        );

        // Commit to running BEFORE spawning (synchronous, no check-then-set race
        // with the guard reads above). Bump the epoch so this loop has a unique
        // generation; record the bound device id; set the canary. The spawned
        // task captures `my_epoch` and clears canary/bound-id on exit ONLY if it
        // is still the latest generation (epoch unchanged) — a superseded loop's
        // clear is a no-op, so it can never clobber a newer loop's state. The
        // per-frame and acquire exit checks also read the epoch, so a superseded
        // loop self-terminates on its next frame / before binding.
        //
        // `fetch_add` returns the PREVIOUS value, so `my_epoch` = previous + 1 =
        // the value now stored; a later `loop_epoch.load() == my_epoch` is true
        // iff no newer start() has bumped it.
        let my_epoch = self
            .loop_epoch
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        *self.loop_device_id.borrow_mut() = Some(device_id.clone());
        self.loop_running.store(true, Ordering::Release);
        // Clear `switching` HERE, at the epoch commit (issue #1295). `switching`
        // is set by select() to REQUEST a switch; the guard above already
        // consumed it (read into `switch_requested`) to decide whether to tear
        // the old loop down. This new loop, with its freshly-bumped epoch, IS the
        // response to that request, so the request is now satisfied and the flag
        // must be lowered. The supersede guards (acquire + per-frame) no longer
        // read `switching` — they rely on the epoch — so this is the single,
        // authoritative place (besides stop()) that lowers it: select() raises,
        // commit (here) and stop() clear. Leaving it raised cannot strand a loop
        // any more, but clearing it keeps the next start()'s guard read honest.
        self.state.switching.store(false, Ordering::Release);
        let loop_running = self.loop_running.clone();
        let loop_device_id = self.loop_device_id.clone();
        let loop_epoch = self.loop_epoch.clone();
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
                    // Clear only if still the latest loop (issue #1295 epoch
                    // guard): a superseded loop must not clobber a newer one's
                    // canary/bound-id. `switching` is NOT touched here: it is
                    // owned by select() (raise) and start()'s commit / stop()
                    // (clear), and no supersede guard reads it any more, so a
                    // task-exit reset would be dead code.
                    if loop_epoch.load(Ordering::Acquire) == my_epoch {
                        loop_running.store(false, Ordering::Release);
                        *loop_device_id.borrow_mut() = None;
                    }
                    return;
                }
            };

            // Per-layer sequence numbers persist across restarts so the
            // receiving side never sees duplicate or regressed sequence numbers.
            // Each simulcast layer (issue #989) carries its own monotonic
            // counter: a receiver decoding only one layer must see a dense
            // 0,1,2,… stream or its `SequenceTracker` reports phantom loss
            // (~(N-1)/N) and storms PLIs. Explicit single-layer mode makes this
            // a one-element Vec behaving exactly like the old scalar.
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

            // Per-rung "continuously shed since" wall-clock (ms, `performance.now()`),
            // indexed by `layer_id` (issue #1230). `Some(t)` once a rung drops out of
            // the active set; cleared to `None` when it is active again or after it is
            // torn down. Declared OUTSIDE `'restart` (like `prev_active_layers`) so a
            // mid-dwell encoder restart does not reset the clock and re-arm a full 30s
            // wait. The encode loop STAMPS this every frame from the same
            // `local_active_layers` it tears down against, so the dwell clock actually
            // advances (it is not a dead timer). Sized `n_layers`; in single-stream
            // mode (`n_layers == 1`) it has one slot that is never used (the base layer
            // is never shed).
            let mut shed_since_ms: Vec<Option<f64>> = vec![None; n_layers];

            // Geometry clear-generation the loop has already reconciled (issue #2170).
            //
            // Seeded from the LIVE counter at spawn, not from 0: every generation's
            // `build_layer` publishes anyway, so a fresh spawn has nothing to repair
            // and must not fire a spurious republish for clears that predate it.
            // Declared outside `'restart` so a codec rebuild does not reset it — the
            // rebuild republishes via `build_layer` regardless, and it re-seeds below.
            let mut seen_dims_cleared = shared_layer_dims_cleared.load(Ordering::Acquire);

            'restart: loop {
                // Reset the published geometry for THIS generation (issue #2170).
                //
                // WHY HERE AND NOWHERE ELSE. This must run when a new encoder
                // generation is actually about to be built, and must NOT run on a
                // `start()` call that changes nothing. Placing it in `start()`'s
                // prologue satisfied neither:
                //
                // - `start()` has three early returns (no device selected, not
                //   enabled, and the #1295 "true duplicate on the SAME device"
                //   guard). A prologue clear zeroed the atomics and then returned,
                //   leaving the LIVE loop running but its geometry blank. Nothing
                //   republishes: `build_layer`'s publish is the only UNGATED one and
                //   it does not re-run for a live loop, while each of the three
                //   RECONFIGURE sites is gated on a CHANGE that a clear does not
                //   touch — the two per-frame re-fits compare against
                //   `layer.current_w/h` (via `needs_reconfigure`, and directly), and
                //   the single-stream tier-change site compares the new TIER BOX
                //   against `local_tier_max_*`. Not the same predicate, but the same
                //   consequence: a clear moves none of those inputs, so a steady
                //   camera on a steady tier fires none of them again and the readout
                //   reads unknown for the rest of the session.
                //   Reachable in practice: `EncoderState::select` raises `switching`
                //   whenever the encoder is enabled WITHOUT comparing device ids, so
                //   overlapping `on_loaded`/`on_devices_changed` events schedule two
                //   starts; the first consumes `switching`, the second hits the
                //   duplicate return.
                // - A prologue clear also never ran for an internal `'restart`, so a
                //   fatal codec rebuild kept the PREVIOUS generation's geometry.
                //
                // Inside the loop, both holes close by construction: the spawn is
                // unreachable from every early return, and every generation — cold
                // start, device switch, and each codec rebuild — starts blank and
                // republishes via `build_layer`. There is deliberately no host test
                // for the PLACEMENT: `start()` needs `getUserMedia`, so that half is
                // guaranteed structurally rather than asserted.
                //
                // EPOCH GUARD (issue #1295 convention, same as the canary/bound-id
                // clears at both supersede sites). A superseded loop must not
                // clobber a newer generation's published geometry. Both
                // `loop_is_superseded` checks `return` rather than re-entering
                // `'restart`, so a stale loop normally cannot reach this line — but
                // one interleaving evades them: loop A passes its per-frame check,
                // parks in `video_reader.read().await` (seconds if the tab is
                // backgrounded or the track stalls), the epoch is bumped, loop B
                // completes `getUserMedia` + `build_layer` and PUBLISHES, then A's
                // read resolves, A takes a fatal `break 'encode`, and falls through
                // cleanup to this loop head. Without the guard A would zero B's
                // slots — and because every other writer is gated on a change a
                // clear does not touch (see the enumeration above),
                // nothing would republish: the readout would read unknown for the
                // rest of the session behind a healthy encoder.
                //
                // This guard is an inline epoch load, NOT reachable from a host test
                // (it is one of the ten unguarded call sites disclosed at
                // `shared_layer_dims`' clone above): deleting or inverting the `if`
                // leaves the whole client suite green. It is deliberately NOT
                // `loop_is_superseded`, which ORs in `!enabled` — a still-current
                // disabled loop SHOULD clear.
                if loop_owns_shared_geometry(loop_epoch.load(Ordering::Acquire), my_epoch) {
                    clear_layer_dims(&shared_layer_dims);
                    clear_layer_output_metrics(
                        &shared_layer_output_fps,
                        &shared_layer_output_fps_ready,
                        &shared_layer_last_chunk_ms,
                    );
                }
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
                        // Clear only if still the latest loop (issue #1295 epoch
                        // guard): a superseded loop must not clobber a newer
                        // one's canary/bound-id. `switching` is NOT touched here:
                        // it is owned by select() (raise) and start()'s commit /
                        // stop() (clear), and no supersede guard reads it any
                        // more, so a task-exit reset would be dead code.
                        if loop_epoch.load(Ordering::Acquire) == my_epoch {
                            loop_running.store(false, Ordering::Release);
                            *loop_device_id.borrow_mut() = None;
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
                        record_camera_restart(RestartReason::Other);
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
                            record_camera_restart(RestartReason::Other);
                            restart_count += 1;
                            continue 'restart;
                        }
                    };

                let device = match JsFuture::from(devices_query).await {
                    Ok(s) => s.unchecked_into::<MediaStream>(),
                    Err(e) => {
                        // Classify the rejection (e.g. NotReadableError →
                        // DeviceInUse) so the UI can show a specific reason and
                        // auto-retry. We emit ONLY the classified permission
                        // callback here — NOT the generic string `on_error` —
                        // because the UI raises a dedicated modal for the
                        // classified error, and firing both would stack two
                        // modals for the same failure. The raw error is still
                        // logged for diagnostics.
                        error!("Failed to get camera stream: {e:?}");
                        if let Some(cb) = &on_permission_error {
                            cb.emit(classify_get_user_media_error(&e));
                        }
                        record_camera_restart(RestartReason::Other);
                        restart_count += 1;
                        continue 'restart;
                    }
                };

                log::info!(
                    "CameraEncoder: getUserMedia OK, stream id={:?}, tracks={}",
                    device.id(),
                    device.get_tracks().length()
                );

                // Acquire-phase supersede check (issue #1295). getUserMedia is an
                // await point: while THIS loop was acquiring (cold start OR a
                // `'restart` re-acquisition), a newer start() may have superseded
                // us. `set_src_object` below binds the SHARED <video> element, so
                // a stale loop binding here is the exact wrong-device race. Bail
                // BEFORE binding: stop the just-acquired tracks so the camera is
                // released, then return. The epoch-guarded clear is a no-op when
                // superseded (a newer loop owns the canary), so we never clobber
                // the latest loop's state.
                //
                // Supersede is decided by `epoch` and `enabled` ONLY — NOT by
                // `switching` — via the shared `loop_is_superseded` predicate (the
                // same one the per-frame guard uses). `switching` means "a switch
                // was requested"; the LATEST loop IS the response to that request
                // and must OWN it, not self-abort on it. A real switch tears the
                // old loop down and bumps the epoch in start()'s guard/commit, so
                // the stale loop is already caught by `loop_epoch != my_epoch`
                // here (and again at the per-frame check); the newest loop — the
                // one that should bind — sees its own epoch and a `switching`
                // already cleared by the commit, so it proceeds. Reading
                // `switching` here would instead kill the very loop that is meant
                // to bind (the initial-join dark square, issue #1295).
                if loop_is_superseded(
                    enabled.load(Ordering::Acquire),
                    loop_epoch.load(Ordering::Acquire),
                    my_epoch,
                ) {
                    log::info!(
                        "CameraEncoder: superseded during acquire (epoch/enabled), releasing stream without binding"
                    );
                    for track in device.get_tracks().iter() {
                        track.unchecked_into::<MediaStreamTrack>().stop();
                    }
                    if loop_epoch.load(Ordering::Acquire) == my_epoch {
                        loop_running.store(false, Ordering::Release);
                        *loop_device_id.borrow_mut() = None;
                    }
                    return;
                }

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

                // Each restart extracts a track from its newly acquired stream; no track is cached.
                let video_track = Box::new(
                    device
                        .get_video_tracks()
                        .find(&mut |_: JsValue, _: u32, _: Array| true)
                        .unchecked_into::<VideoTrack>(),
                );

                // Get track settings to establish the native source dimensions;
                // each simulcast encoder then fits that source into its own
                // fixed tier box via `simulcast_layer_target_dims`.
                let media_track = video_track
                    .as_ref()
                    .clone()
                    .unchecked_into::<MediaStreamTrack>();
                let track_settings = media_track.get_settings();

                let (width, height) = resolve_capture_dimensions(
                    track_settings.get_width().map(f64::from),
                    track_settings.get_height().map(f64::from),
                    video_element.video_width(),
                    video_element.video_height(),
                );

                // Seed the #1196 source-aspect atomics at acquisition from the
                // resolved dims (settings -> preview -> 640x480 fallback). The
                // camera CORRECTS these shared atomics from the first valid
                // decoded frame (and any later size change) below, so a fallback
                // seed self-heals within a frame and the stamp reflects the true
                // source aspect. This is why the camera can seed a non-zero
                // fallback safely; the screen encoder, which does NOT
                // per-frame-correct, instead seeds 0="unknown" when settings are
                // absent so it never stamps a fabricated aspect (see
                // settings_source_stamp). So the seeds intentionally DIVERGE in
                // the settings-absent case.
                let source_width_atomic = Arc::new(AtomicU32::new(width));
                let source_height_atomic = Arc::new(AtomicU32::new(height));

                // --- Setup video encoders (LAZY per-layer construction, #1204) ─
                // The output and error handler closures must be re-created on
                // each restart because Closure::wrap consumes them and the new
                // VideoEncoder needs fresh JS function references. Each layer
                // owns its own output closure (own seq counter + reused buffer),
                // its own error closure, and its own config object. The closures
                // are stored in the LayerEncoder so they outlive the encoder.
                //
                // LAZY CONSTRUCTION (issue #1204): we build layer N's VideoEncoder
                // only on its FIRST ACTIVATION — at cold start that is just the
                // base layer (`shared_active_layer_count` == 1 via the camera's
                // earn-up ramp), so the upper-rung VideoEncoders are NOT allocated
                // until the AQ ramp/restore raises the active count past them.
                // Previously all `n_layers` encoders were built up front even
                // though cold start activates only the base, wasting WebCodecs /
                // VPX allocations for rungs that may never be earned. The
                // OUTPUT/EGRESS is unchanged: the encode loop already only encodes
                // layers with `layer_id < active`, so for any given active-count
                // sequence the same frames are emitted whether the un-earned
                // encoders existed or not.
                //
                // TEARDOWN-AFTER-SHED (issue #1230): a shed upper rung is RETAINED
                // (its encoder + ~100KB output buffer) so a brief shed→restore
                // bounce reuses it with no rebuild stall. But on a device under
                // SUSTAINED distress that never earns the rung back, holding that
                // native VPX/WebCodecs state for the share's lifetime is a leak.
                // So once a rung has been continuously shed for
                // `SHED_TEARDOWN_DWELL_MS` (30s — 20× the 1500ms AQ min transition
                // interval, so a transient bounce never accumulates the dwell) the
                // encode loop closes+drops its `LayerEncoder` to reclaim the memory; the
                // SAME lazy path above (`encoders_to_build`) rebuilds it if it is
                // ever earned back, seeded from its persisted sequence. Teardown
                // pops ONLY the top built rung(s) to keep `layers` a contiguous
                // 0..len prefix (shed is strictly top-down) and never frees the
                // base layer. See `should_teardown_shed_layer` + the per-frame
                // dwell tracking in the encode loop.
                //
                // With an explicit one-layer ceiling only the base layer is ever
                // built, preserving the legacy single-encoder path.
                //
                // `build_layer` constructs ONE `LayerEncoder` for `layer_idx`
                // (seeded from its persisted sequence). It is a closure so both
                // the initial cold-start build and the lazy in-loop build share
                // one construction site. On `VideoEncoder::new` / fatal-configure
                // failure it returns `Err(LayerBuildError)` (the caller does the
                // already-built-layer cleanup + restart bookkeeping, which a
                // closure cannot do via `continue 'restart`).
                enum LayerBuildError {
                    /// `VideoEncoder::new` failed — surface to `on_error`.
                    CreateFailed(String),
                    /// A FATAL `configure()` error before the encode loop.
                    ConfigureFatal,
                }
                // Issue #2170: the build closure publishes each layer's INITIAL
                // fitted dims. Cloned (not borrowed) so the closure does not hold a
                // borrow of the outer `shared_layer_dims` across the encode loop.
                let shared_layer_dims_build = shared_layer_dims.clone();
                // Epoch fence for the build-time publish (issue #2170): cloned
                // alongside the dims for the same reason — the closure must not hold a
                // borrow of the outer binding across the encode loop.
                let loop_epoch_build = loop_epoch.clone();
                // ...and the enabled flag, so a `stop()` during this generation's
                // `getUserMedia`/`play()` window cannot publish over the sentinel
                // `stop()` just wrote (issue #2170).
                let enabled_build = enabled.clone();
                let build_layer = |layer_idx: usize,
                                   initial_seq: u64|
                 -> Result<LayerEncoder, LayerBuildError> {
                    let layer_id = layer_idx as u32;
                    let (layer_output_fps, layer_output_fps_ready, layer_last_chunk_ms) =
                        layer_output_metric_slots(
                            &shared_layer_output_fps,
                            &shared_layer_output_fps_ready,
                            &shared_layer_last_chunk_ms,
                            layer_idx,
                        )
                        .unwrap_or_else(|| {
                            (
                                Rc::new(AtomicU32::new(0)),
                                Rc::new(AtomicBool::new(false)),
                                Rc::new(AtomicU64::new(0)),
                            )
                        });
                    let source_width_for_handler = source_width_atomic.clone();
                    let source_height_for_handler = source_height_atomic.clone();

                    let (video_output_box, seq_out) = {
                        let client = client.clone();
                        let userid = userid.clone();
                        let aes = aes.clone();
                        let current_fps = current_fps.clone();
                        let last_layer0_chunk_ms = last_layer0_chunk_ms.clone();
                        // Epoch + enabled fence for the fps publish (issue #2170).
                        // Cloned for the same reason as `loop_epoch_build` /
                        // `enabled_build`: this closure outlives the encode loop's
                        // frame body and must not hold a borrow of the outer binding.
                        let loop_epoch_fps = loop_epoch.clone();
                        let enabled_fps = enabled.clone();
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
                                let is_keyframe =
                                    matches!(chunk.type_(), web_sys::EncodedVideoChunkType::Key);
                                // Issue #2170: a WebCodecs output callback is
                                // ASYNCHRONOUS: this callback can fire after its
                                // generation was superseded, and the slots are cleared
                                // IN PLACE, so an unfenced late write lands in the live
                                // generation's slot. Gates the METRIC writes only —
                                // never the chunk send below, which must keep flowing
                                // (issue #2170, site 8).
                                let may_publish_fps = loop_may_publish_geometry(
                                    enabled_fps.load(Ordering::Acquire),
                                    loop_epoch_fps.load(Ordering::Acquire),
                                    my_epoch,
                                );
                                if may_publish_fps {
                                    layer_last_chunk_ms.store(now as u64, Ordering::Relaxed);
                                    chunks_in_last_second += 1;
                                }

                                // FPS calculation: ONLY layer 0 updates the
                                // shared `current_fps`. That atomic is the AQ
                                // controller's setpoint (encoder output fps);
                                // summing N layers would inflate it N× and
                                // corrupt every PID/tier decision. Higher layers
                                // still encode and send, they just don't touch
                                // the setpoint.
                                if layer_id == 0 {
                                    last_layer0_chunk_ms.store(now as u64, Ordering::Relaxed);
                                }
                                if may_publish_fps && now - last_chunk_time >= 1000.0 {
                                    let fps = chunks_in_last_second;
                                    // The window closes on the first chunk at or
                                    // AFTER 1000 ms, so normalize by the span it was
                                    // really measured over — otherwise a resume after
                                    // a multi-second gap publishes an accumulated
                                    // count as a per-second rate (issue #2170). The
                                    // raw count still feeds `current_fps`; see
                                    // `publish_layer_output_fps`.
                                    let measured_fps =
                                        normalize_window_fps(fps, now - last_chunk_time);
                                    publish_layer_output_fps(
                                        layer_id,
                                        measured_fps,
                                        fps,
                                        &layer_output_fps,
                                        &layer_output_fps_ready,
                                        &current_fps,
                                    );
                                    // PER-TICK telemetry: fires ~1 Hz while
                                    // encoding (layer 0). Demoted debug!->trace!
                                    // so it stays off even when console-log
                                    // collection bumps to Debug (#1100 follow-up).
                                    if layer_id == 0 {
                                        log::trace!("Encoder output FPS: {fps}");
                                    }
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
                                    source_width_for_handler.load(Ordering::Relaxed),
                                    source_height_for_handler.load(Ordering::Relaxed),
                                    layer_id,
                                );
                                // Phase 2 of WT freeze fix: route camera video on
                                // its dedicated persistent QUIC stream so a stall
                                // on a video keyframe never blocks audio.
                                //
                                // #1737 SCOPE (camera-only, deliberate): only the
                                // camera attaches `FrameDropMeta` to opt into
                                // sender-side age-drop. Screen share rides its own
                                // WT persistent unistream and suffers the SAME
                                // minutes-behind pile-up unmitigated — but it is
                                // deferred on purpose: screen needs its own, more
                                // generous age budget (longer GOP / static content
                                // tolerates more latency than the 200ms camera
                                // budget), so reusing the camera value would be a
                                // regression. The screen encoder passes `None`
                                // (see screen_encoder.rs) pending screen-specific
                                // budget tuning. Do NOT unify the two.
                                client.send_media_packet_with_drop_meta(
                                    packet,
                                    MediaStreamKey::Video,
                                    Some(FrameDropMeta {
                                        enqueue_ms: now,
                                        is_keyframe,
                                    }),
                                );
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
                            return Err(LayerBuildError::CreateFailed(msg));
                        }
                    };

                    // Resolution + initial bitrate per layer:
                    //  - single-stream (n_layers == 1): native camera resolution
                    //    and the shared adaptive bitrate — the legacy path, with
                    //    tier-resolution stepping preserved (see encode loop).
                    //  - simulcast (n_layers > 1): each layer's tier is a
                    //    BOUNDING BOX, not a fixed output size (issue #1196).
                    //    The native capture dims are fitted INSIDE the layer's
                    //    SIMULCAST_LAYER_TIERS rung (aspect-preserving) so the
                    //    very first GOP already carries the source aspect — a
                    //    non-16:9 capture (e.g. a 4:3 webcam) is never
                    //    per-axis-squashed into the 16:9 tier dims. `tier_w` /
                    //    `tier_h` are recorded so the per-frame loop can re-fit
                    //    against THIS layer's box when the source dims change.
                    //    The per-layer bitrate still adapts (tier ideal here).
                    // `layer_fps` is `Some(target_fps)` for a simulcast rung and
                    // `None` on the single-stream path. It drives BOTH the encoder
                    // framerate hint (rate-control budgets bitrate at this fps) and
                    // the per-layer frame-drop throttle (issue #1768). Resolution
                    // and fps are set independently: the fps comes from the rung's
                    // `target_fps`, the dims from `fit_within_preserving_aspect`.
                    let (layer_w, layer_h, tier_w, tier_h, init_bitrate_bps, layer_fps) =
                        if simulcast {
                            // Delegated to a pure fn so THE ENCODE-PATH LADDER READ IS
                            simulcast_layer_encode_params(n_layers, layer_idx, width, height)
                        } else {
                            // Single-stream: native resolution; the tier_w/tier_h
                            // fields are unused on this path (the legacy loop fits
                            // against the shared `local_tier_max_*`), so mirror the
                            // layer dims to keep them well-defined. No per-layer fps
                            // cap — the stream follows the capture/adaptive cadence.
                            (
                                width,
                                height,
                                width,
                                height,
                                current_bitrate.load(Ordering::Relaxed) as f64 * 1000.0,
                                None,
                            )
                        };
                    // Per-layer frame-drop interval (ms). 0.0 = no cap (single
                    // stream); simulcast rungs cap at their rung fps (issue #1768).
                    let min_encode_interval_ms =
                        layer_fps.map(|fps| 1000.0 / fps as f64).unwrap_or(0.0);
                    let config =
                        VideoEncoderConfig::new(get_video_codec_string(), layer_h, layer_w);
                    config.set_bitrate(init_bitrate_bps);
                    config.set_latency_mode(LatencyMode::Realtime);
                    // Framerate hint: tell the encoder's rate controller each rung's
                    // target fps so it budgets bitrate-per-frame correctly (a 7 fps
                    // rung must not be rate-budgeted as if it were 30 fps). web-sys'
                    // `VideoEncoderConfig` has no `framerate` setter, so set it via
                    // `Reflect` (same pattern as the screen encoder's `bitrateMode`).
                    if let Some(fps) = layer_fps {
                        let _ = Reflect::set(
                            &config,
                            &JsValue::from_str("framerate"),
                            &JsValue::from_f64(fps as f64),
                        );
                    }

                    if let Err(e) = video_encoder.configure(&config) {
                        CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL.fetch_add(1, Ordering::Relaxed);
                        if is_fatal_encoder_error(&e) {
                            error!("CameraEncoder: fatal configure error before encode loop (layer {layer_id}), restarting: {e:?}");
                            let _ = video_encoder.close();
                            return Err(LayerBuildError::ConfigureFatal);
                        }
                        error!("Error configuring video encoder (layer {layer_id}): {e:?}");
                    }

                    let mut built = LayerEncoder {
                        encoder: video_encoder,
                        config,
                        seq_out,
                        layer_id,
                        current_w: layer_w,
                        current_h: layer_h,
                        tier_w,
                        tier_h,
                        local_bitrate: init_bitrate_bps as u32,
                        last_failed_bitrate: None,
                        min_encode_interval_ms,
                        last_encode_ms: f64::NEG_INFINITY,
                        _output_closure: output_closure,
                        _error_closure: error_closure,
                    };
                    // Publish the INITIAL fitted dims (issue #2170). Without this a
                    // layer that is built and never re-fitted — the common case for a
                    // steady camera — would report unknown forever, since the other
                    // three publish sites only fire on a dimension CHANGE.
                    //
                    // Published unconditionally, including after the non-fatal
                    // `configure()` error logged above. That case is a KNOWN
                    // imprecision, deliberately left as-is: the encoder may not be
                    // running at this geometry, so the readout can be optimistic for
                    // a refused config. Gating the publish on acceptance was
                    // prototyped and dropped — it turned one non-fatal refusal into a
                    // 30-50 Hz `configure`+`error!` retry storm (each iteration
                    // shipping a Matomo event and bumping a Prometheus counter) and
                    // fed `(0, 0)` into `fit_within_preserving_aspect`, squashing
                    // aspect for a frame. Tracked as issue #2177 with the design that
                    // avoids both.
                    built.set_encode_dims(
                        layer_w,
                        layer_h,
                        &shared_layer_dims_build,
                        &enabled_build,
                        &loop_epoch_build,
                        my_epoch,
                    );
                    Ok(built)
                };

                // Cold-start build: only the layers that are ACTIVE right now
                // (base layer at cold start; more only if a prior cycle already
                // earned them and the shared atomic still reflects that). Upper
                // rungs are built lazily on first activation in the encode loop.
                // `sequence_numbers` has exactly `n_layers` elements, so the
                // index range is always in-bounds.
                let initial_active_layers = encoders_to_build(
                    shared_active_layer_count.load(Ordering::Relaxed) as usize,
                    n_layers,
                );
                let mut layers: Vec<LayerEncoder> = Vec::with_capacity(n_layers);
                for (layer_idx, &initial_seq) in
                    sequence_numbers[..initial_active_layers].iter().enumerate()
                {
                    match build_layer(layer_idx, initial_seq) {
                        Ok(le) => layers.push(le),
                        Err(LayerBuildError::CreateFailed(msg)) => {
                            for built in &layers {
                                let _ = built.encoder.close();
                            }
                            stop_media_stream_tracks(&device);
                            // Classify by the create error message BEFORE it is
                            // moved into the on_error callback (memory vs other).
                            record_camera_restart(restart_reason_from_message(&msg));
                            if let Some(cb) = &on_error {
                                cb.emit(msg);
                            }
                            restart_count += 1;
                            continue 'restart;
                        }
                        Err(LayerBuildError::ConfigureFatal) => {
                            for built in &layers {
                                let _ = built.encoder.close();
                            }
                            stop_media_stream_tracks(&device);
                            record_camera_restart(RestartReason::Configure);
                            restart_count += 1;
                            continue 'restart;
                        }
                    }
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

                // Wall-clock (`performance.now()`, ms) of the last keyframe this
                // publisher emitted — periodic OR PLI-forced. Drives the forced-
                // keyframe emit coalescer (issue #1287): PLIs landing within
                // FORCED_KEYFRAME_COOLDOWN_MS of the last keyframe are held, not
                // re-emitted. `None` until the first keyframe goes out.
                //
                // Declared INSIDE `'restart` (unlike `prev_active_layers` /
                // `shed_since_ms`, which are declared OUTSIDE so they survive a
                // restart): the per-`'restart` reset to `None` is INTENTIONAL. A
                // `'restart` is fatal-encoder-error recovery — the codec was rebuilt
                // and the receivers need a fresh keyframe immediately, so the
                // cooldown clock must start clean. `prev_active_layers`/`shed_since_ms`
                // deliberately persist because they track ladder/dwell state that a
                // codec restart must NOT reset. A reconnect/re-election does NOT take
                // this `'restart` path (the encode loop runs uninterrupted), so it
                // gets its own reset via `keyframe_cooldown_reset` below (issue #1311).
                let mut last_keyframe_emit_ms: Option<f64> = None;

                // Per-encoder bitrate and dimensions now live in each
                // `LayerEncoder` (local_bitrate / current_w / current_h) so each
                // layer reconfigures independently. The tier-controlled caches
                // below are shared across layers because they are driven by the
                // publisher-wide tier atomics; fixed rung boxes and per-layer
                // bitrate atomics provide the layer-specific targets.

                // The `low` rung, the ceiling the single-layer >3-peer pin
                // (issue #1136) applies below. Only meaningful for n_layers == 1.
                let low_rung = &simulcast_layers(1)[0];
                let low_rung_w = low_rung.max_width;
                let low_rung_h = low_rung.max_height;
                let low_rung_bitrate_bps = low_rung.ideal_bitrate_kbps * 1000;

                // Cache tier-controlled values (shared across layers).
                let mut local_keyframe_interval = tier_keyframe_interval.load(Ordering::Relaxed);
                let mut local_tier_max_width = tier_max_width.load(Ordering::Relaxed);
                let mut local_tier_max_height = tier_max_height.load(Ordering::Relaxed);
                // Simulcast: how many layers are currently active (encoded+sent).
                // In single-stream mode this is always 1 and gates nothing.
                // Refreshed from the shared atomic at the top of every frame.
                let mut local_active_layers: usize;

                // Issue #1531: sticky "am I on a lossless (WS) transport" cache for
                // the transport-aware periodic-keyframe ceiling. Refreshed each frame
                // from `client.active_transport()`, but only on a definitive `Some`
                // read — a transient `None` (controller cell momentarily borrowed,
                // pre-election cold start) leaves the last-known value intact rather
                // than dropping the WS relief for one frame. Defaults `false` (lossy /
                // WebTransport), i.e. the tighter #1510 ceiling, until the transport
                // is first known — the safe direction.
                let mut lossless_transport = false;

                // Track whether we have successfully encoded at least one frame
                // in this restart cycle. Used to reset restart_count on success.
                let mut encoded_ok_this_cycle = false;

                'encode: loop {
                    // Exit when disabled OR superseded (issue #1295), via the
                    // shared `loop_is_superseded` predicate (the same one the
                    // acquire-phase guard uses): a newer start() bumped
                    // `loop_epoch` past our captured `my_epoch`, so we are a stale
                    // loop and must self-terminate even though `enabled` may have
                    // been set true again for the newer loop. This is what forces
                    // the OFF→switch→ON stale loop to die — the enabled flip alone
                    // would not. `switching` is NOT read here: the epoch already
                    // covers supersede (a real switch bumps it), and the loop that
                    // owns the current epoch is the switch's intended response,
                    // which must keep running rather than exit on the request flag.
                    if loop_is_superseded(
                        enabled.load(Ordering::Acquire),
                        loop_epoch.load(Ordering::Acquire),
                        my_epoch,
                    ) {
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
                        // Clear only if still the latest loop (issue #1295 epoch
                        // guard): a superseded loop must not clobber a newer
                        // one's canary/bound-id.
                        if loop_epoch.load(Ordering::Acquire) == my_epoch {
                            loop_running.store(false, Ordering::Release);
                            *loop_device_id.borrow_mut() = None;
                        }
                        return;
                    }

                    // REPUBLISH after a geometry CLEAR this loop did not make
                    // (issue #2170).
                    //
                    // Closes the OFF→ON stranding path. `stop()` clears the published
                    // geometry, but a camera OFF→ON pair does not always produce a new
                    // encoder generation, so nothing would republish and a LIVE camera
                    // would read `—` for the rest of the session:
                    //
                    // 1. `host.rs` camera OFF = `set_enabled(false)` + `stop()`. That
                    //    clears the slots but does NOT stop the track — only this
                    //    loop's own exit path does.
                    // 2. Camera ON = `select(device_id)` → `set_enabled(true)` →
                    //    `start()`. `select()` raises `switching` ONLY when already
                    //    enabled (`EncoderState::select`), and here it runs while
                    //    still disabled — so `switching` stays false.
                    // 3. `start()`'s #1295 duplicate guard then matches
                    //    `running && !switch_requested && same_device` and returns
                    //    early. No new generation ⇒ no epoch bump ⇒ no `'restart`-head
                    //    clear and no `build_layer` publish.
                    // 4. This loop resumes with `enabled == true` and an unchanged
                    //    epoch, so it keeps encoding — but all three reconfigure sites
                    //    are gated on a change a CLEAR does not move
                    //    (`needs_reconfigure`, `clamped != current_w`,
                    //    `tier_dims_changed`), so none of them fires.
                    //
                    // WHY A COUNTER AND NOT AN `enabled` EDGE. The first cut of this
                    // fix compared `prev_enabled`/`now_enabled` per frame, and it
                    // MISSED THE VERY CASE IT WAS WRITTEN FOR: the whole hazard is that
                    // the loop is parked in `video_reader.read().await` across the
                    // OFF→ON pair, so it never samples the disabled state and on resume
                    // reads `(true, true)` — no edge, no republish. Comparing a clear
                    // GENERATION is edge-independent: `stop()` records the clear
                    // whether or not any frame boundary observed it.
                    //
                    // Reachability of the underlying stall is narrow — the per-frame
                    // exit above normally clears `loop_running` within one frame period
                    // (~33 ms), letting `start()` spawn a fresh generation instead — but
                    // it is a real device-level stall (OS camera shutter, another app
                    // grabbing the device), and this is a NEW failure mode: before
                    // #2170 the readout showed a wrong-but-present tier box, never a
                    // permanent blank.
                    //
                    // This is the same hazard the `'restart`-head clear's own doc gives
                    // as the reason NOT to put a clear in `start()`'s prologue. That
                    // reasoning applies verbatim to `stop()`'s clear, which reopens the
                    // hole through a different door. Republishing `current_w`/
                    // `current_h` is exactly right because they are the geometry this
                    // loop's encoders are STILL configured at — the clear erased only
                    // the published copy, not the encoders.
                    //
                    // Gated on `enabled` so a clear observed while STOPPED does not
                    // undo the sentinel; the next enable re-runs this check because
                    // `seen_dims_cleared` is only advanced once a republish happens.
                    //
                    // DELIBERATELY NO SEPARATE EPOCH FENCE: this is one of two
                    // encode-loop shared-geometry writers that rely on the frame-head
                    // `loop_is_superseded` check. There is no `.await` between that
                    // check and this call, so wasm's single-threaded executor cannot
                    // switch generations in this interval. The shed-teardown clear
                    // below relies on the same structural guarantee. If an await is
                    // introduced into this part of the frame body, both sites need an
                    // explicit `loop_may_publish_geometry` /
                    // `loop_owns_shared_geometry` fence.
                    let live_dims_cleared = shared_layer_dims_cleared.load(Ordering::Acquire);
                    if republish_geometry_on_tick(
                        seen_dims_cleared,
                        live_dims_cleared,
                        enabled.load(Ordering::Acquire),
                    ) {
                        for layer in &layers {
                            publish_layer_dims(
                                &shared_layer_dims,
                                layer.layer_id,
                                layer.current_w,
                                layer.current_h,
                            );
                        }
                        seen_dims_cleared = live_dims_cleared;
                    }

                    // --- Guard: check if any encoder has been closed externally ---
                    // This can happen if the browser closes the codec (e.g. due to
                    // GPU process crash, OOM, or an error callback we didn't intercept).
                    // If ANY layer's encoder is closed we restart all layers;
                    // per-layer restart remains an unimplemented optimization.
                    if layers
                        .iter()
                        .any(|l| l.encoder.state() == CodecState::Closed)
                    {
                        log::warn!("CameraEncoder: an encoder state is Closed, triggering restart");
                        record_camera_restart(RestartReason::ClosedCodec);
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

                    // Lazy per-layer construction (issue #1204). If the AQ ramp /
                    // restore raised the active count past the layers we have
                    // built so far, construct the newly-activated rung(s) NOW,
                    // before the reconfigure + encode passes below read
                    // `layers[layer_id]`. The clamp keeps this in-bounds; in
                    // single-stream mode `n_layers == 1` so the base is always
                    // already present and this loop never runs. Each new layer is
                    // seeded from its PERSISTED sequence number so a receiver that
                    // picks up the freshly-earned rung sees a dense stream.
                    if simulcast && layers.len() < encoders_to_build(local_active_layers, n_layers)
                    {
                        let want = encoders_to_build(local_active_layers, n_layers);
                        let already_built = layers.len();
                        // Restart reason captured from the failing build (issue
                        // #527); `None` while the rung builds succeed.
                        let mut build_failed: Option<RestartReason> = None;
                        for (offset, &initial_seq) in
                            sequence_numbers[already_built..want].iter().enumerate()
                        {
                            let layer_idx = already_built + offset;
                            // #1230 rebuild-latency: time the construct+configure
                            // cost of the (re)build so it is field-measurable on
                            // real devices/bots. This delta is the build CALL cost;
                            // the configure→first-emitted-keyframe latency can be
                            // derived in the field by correlating this log with the
                            // first chunk emitted for `layer_idx` (the per-chunk
                            // handler computes `now` already). This is the
                            // "documented rebuild-latency measurement" that #1204
                            // gated teardown on — now enabled.
                            let build_started_ms = window().performance().unwrap().now();
                            match build_layer(layer_idx, initial_seq) {
                                Ok(le) => {
                                    let build_ms =
                                        window().performance().unwrap().now() - build_started_ms;
                                    log::info!(
                                        "CameraEncoder: lazily (re)built simulcast layer {} on activation in {:.1}ms (#1204/#1230 rebuild-latency)",
                                        layer_idx,
                                        build_ms
                                    );
                                    layers.push(le);
                                }
                                Err(e) => {
                                    // VideoEncoder::new or a fatal configure failed
                                    // for the newly-activated rung. Restart the
                                    // whole encode cycle (the normal 'encode
                                    // cleanup persists already-built layers' seqs).
                                    error!(
                                        "CameraEncoder: failed to lazily construct simulcast layer {}, restarting",
                                        layer_idx
                                    );
                                    // #527: classify by the build error variant —
                                    // a create failure carries a message (memory/
                                    // other), a fatal configure is structural.
                                    build_failed = Some(match &e {
                                        LayerBuildError::CreateFailed(msg) => {
                                            restart_reason_from_message(msg)
                                        }
                                        LayerBuildError::ConfigureFatal => RestartReason::Configure,
                                    });
                                    break;
                                }
                            }
                        }
                        if let Some(reason) = build_failed {
                            record_camera_restart(reason);
                            restart_count += 1;
                            break 'encode;
                        }
                    }

                    // ── Sustained-shed teardown (issue #1230) ──────────────────
                    // SIMULCAST-ONLY. In single-stream mode (`n_layers == 1`,
                    // `simulcast == false`) this entire block is skipped, so the
                    // legacy path is byte-identical. Runs in the SAME loop that
                    // reads `local_active_layers` and would rebuild a rung, so a
                    // layer is freed only while observed inactive by the loop that
                    // would earn it back — no separate thread, no AQ-loop teardown.
                    if simulcast {
                        let now_ms = window().performance().unwrap().now();
                        // 1) STAMP per-rung shed-since each frame from the active
                        // count we just read. A rung is "shed" iff its id >=
                        // active. Stamp the start on the shed edge; clear when it
                        // is active again. This is what makes the dwell clock
                        // advance — it is updated every frame, not in a side task.
                        for layer in layers.iter() {
                            let id = layer.layer_id as usize;
                            if id >= local_active_layers {
                                // Currently shed: arm the clock if not already.
                                if shed_since_ms[id].is_none() {
                                    shed_since_ms[id] = Some(now_ms);
                                }
                            } else {
                                // Active: clear any prior shed timer.
                                shed_since_ms[id] = None;
                            }
                        }

                        // 2) TEAR DOWN the top built rung(s) whose shed dwell has
                        // exceeded the threshold. Pop ONLY from the end so `layers`
                        // stays a contiguous 0..len prefix (the lazy-build path
                        // above rebuilds `already_built..want` and assumes
                        // `layers[i].layer_id == i`). Shed is strictly top-down
                        // (the AQ controller's `drop_top_layer` decrements the
                        // active count from the top), so the only shed rungs are at
                        // the end — popping the tail is exactly the shed set. Floor
                        // at keeping >= 1 layer: never free the base (id 0). Guard
                        // `layers.len() > local_active_layers` so we never free an
                        // ACTIVE rung even if its (stale) timer were armed.
                        while layers.len() > local_active_layers
                            && layers.len() > 1
                            && should_teardown_shed_layer(
                                shed_since_ms[layers.len() - 1],
                                now_ms,
                                SHED_TEARDOWN_DWELL_MS,
                            )
                        {
                            // Pop the top rung. Its `layer_id` equals its index
                            // (contiguous prefix invariant), so this is the highest
                            // shed layer.
                            if let Some(top) = layers.pop() {
                                let id = top.layer_id as usize;
                                let dwell_s = shed_since_ms[id]
                                    .map(|t| (now_ms - t) / 1000.0)
                                    .unwrap_or(0.0);
                                // CRITICAL: persist this rung's sequence back into
                                // `sequence_numbers[id]` BEFORE dropping it, exactly
                                // like the end-of-loop writeback
                                // (`sequence_numbers[layer.layer_id] = layer.seq_out.get()`),
                                // so a future lazy rebuild seeds from the continued
                                // (non-regressed) sequence and a receiver that
                                // re-acquires the rung never sees a duplicate seq.
                                sequence_numbers[id] = top.seq_out.get();
                                // Close + drop frees the native encoder and the
                                // ~100KB output buffer owned by the output closure.
                                let _ = top.encoder.close();
                                drop(top);
                                // The rung is gone; clear its timer so a future
                                // rebuild+shed re-arms a fresh dwell.
                                shed_since_ms[id] = None;
                                // Issue #2170: and clear its published geometry —
                                // this rung's encoder was just closed and freed, so
                                // leaving dims behind describes a rung emitting
                                // nothing. Note the readout's `[..active]` bound
                                // already excludes shed rungs, so this is not what
                                // keeps a shed rung out of the readout; it is what
                                // stops a TORN-DOWN rung reappearing as live if the
                                // active count later rises before the rebuild
                                // publishes.
                                // DELIBERATELY UNFENCED: this is one of two encode-loop
                                // shared-geometry writers without an explicit epoch
                                // fence; the OFF→ON republish above is the other. Both
                                // are safe ONLY because there is no `.await` between
                                // the `'encode`-head supersede check and either call,
                                // so a superseded or stopped loop cannot reach them.
                                // The other writers cannot rely on that structural
                                // argument and carry `loop_may_publish_geometry` /
                                // `loop_owns_shared_geometry`. If a future refactor
                                // introduces an await anywhere above this inside the
                                // frame body, both unfenced sites need an explicit
                                // fence.
                                clear_layer_dim(&shared_layer_dims, id);
                                clear_layer_output_metric(
                                    &shared_layer_output_fps,
                                    &shared_layer_output_fps_ready,
                                    &shared_layer_last_chunk_ms,
                                    id,
                                );
                                CAMERA_ENCODER_LAYERS_TORN_DOWN_AFTER_DWELL
                                    .fetch_add(1, Ordering::Relaxed);
                                log::info!(
                                    "CameraEncoder: tore down shed simulcast layer {} after {:.1}s sustained shed dwell, reclaiming encoder+buffer (#1230); lazy path rebuilds it if earned back",
                                    id,
                                    dwell_s
                                );
                            }
                        }
                    }

                    // Single-stream tier dims + shared bitrate (only meaningful
                    // when NOT simulcast — the adaptive single-stream resolution
                    // path is preserved verbatim for n_layers == 1).
                    let mut new_tier_w = tier_max_width.load(Ordering::Relaxed);
                    let mut new_tier_h = tier_max_height.load(Ordering::Relaxed);
                    let mut new_current_bitrate = current_bitrate.load(Ordering::Relaxed) * 1000;

                    // Single-layer low-rung pin (issue #1136). When this lone
                    // stream is in the flooding regime (single-stream + >3 peers,
                    // decided LIVE by the AQ control loop), cap the effective tier
                    // ceiling and bitrate to the `low` rung so every receiver
                    // decodes 640×360 instead of an adaptive medium-tier stream.
                    // Applied as a `min` CEILING, not a hard pin: if the network
                    // already drove the adaptive tier BELOW `low` we keep the
                    // smaller value (never force quality back UP). Reading the
                    // gate per frame means a peer count crossing 3 mid-call
                    // changes the effective dims here, which trips
                    // `tier_dims_changed` below and reconfigures the encoder —
                    // and dropping back to ≤3 peers restores the adaptive tier.
                    // No-op in simulcast mode (pin stays cleared).
                    if !simulcast && single_layer_low_pin.load(Ordering::Relaxed) {
                        new_tier_w = new_tier_w.min(low_rung_w);
                        new_tier_h = new_tier_h.min(low_rung_h);
                        new_current_bitrate = new_current_bitrate.min(low_rung_bitrate_bps);
                    }

                    let tier_dims_changed = !simulcast
                        && (new_tier_w != local_tier_max_width
                            || new_tier_h != local_tier_max_height);
                    if tier_dims_changed {
                        local_tier_max_width = new_tier_w;
                        local_tier_max_height = new_tier_h;
                    }

                    // Per-layer reconfiguration (PRE-FRAME pass: bitrate only).
                    //
                    //  - Single-stream (n_layers == 1): the legacy logic —
                    //    tier-resolution stepping (tier dims) + shared adaptive
                    //    bitrate, applied verbatim. N=1 behavior is unchanged.
                    //  - Simulcast (n_layers > 1): only the per-layer adaptive
                    //    bitrate is reconfigured HERE. Per-layer RESOLUTION is
                    //    aspect-fitted into each layer's tier bounding box (issue
                    //    #1196) — seeded at construction and re-fitted in the
                    //    per-frame encode loop below where the live frame dims are
                    //    known (so a source-aspect change is followed). It is NOT
                    //    a fixed 16:9 tier size and is not handled in this pass.
                    //    Layers with layer_id >= active count are skipped entirely
                    //    (not reconfigured, not encoded) so a dropped top layer
                    //    costs no encode CPU.
                    // Restart reason captured from a fatal reconfigure cause
                    // (issue #527): a closed-codec guard vs a fatal configure().
                    // `None` while reconfigure succeeds.
                    let mut fatal_reconfigure: Option<RestartReason> = None;
                    for layer in layers.iter_mut() {
                        // Simulcast: skip inactive (shed) top layers entirely.
                        if simulcast && (layer.layer_id as usize) >= local_active_layers {
                            continue;
                        }

                        if simulcast {
                            // Per-layer adaptive bitrate. Resolution for
                            // simulcast layers is aspect-fitted in the per-frame
                            // encode loop (issue #1196), not here.
                            let new_layer_bitrate = {
                                let atomics = shared_layer_bitrates_bps.borrow();
                                atomics
                                    .get(layer.layer_id as usize)
                                    .map(|a| a.load(Ordering::Relaxed))
                                    .unwrap_or(0)
                            };
                            if new_layer_bitrate > 0
                                && layer.should_attempt_bitrate(new_layer_bitrate)
                            {
                                if layer.encoder.state() == CodecState::Closed {
                                    log::warn!("CameraEncoder: encoder closed before per-layer bitrate reconfigure (layer {})", layer.layer_id);
                                    fatal_reconfigure = Some(RestartReason::ClosedCodec);
                                    break;
                                }
                                if let Err(e) = layer.reconfigure_at_bitrate(new_layer_bitrate) {
                                    CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL
                                        .fetch_add(1, Ordering::Relaxed);
                                    if is_fatal_encoder_error(&e) {
                                        error!("CameraEncoder: fatal configure error (layer {}), restarting: {e:?}", layer.layer_id);
                                        fatal_reconfigure = Some(RestartReason::Configure);
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
                        let plan = plan_single_stream_reconfigure(
                            tier_dims_changed,
                            new_current_bitrate,
                            layer.should_attempt_bitrate(new_current_bitrate),
                        );
                        if let SingleStreamReconfigure::Dims { bitrate_bps } = plan {
                            // Guard: do not configure a closed encoder.
                            if layer.encoder.state() == CodecState::Closed {
                                log::warn!("CameraEncoder: encoder closed before tier reconfigure (layer {})", layer.layer_id);
                                fatal_reconfigure = Some(RestartReason::ClosedCodec);
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
                            // Issue #2170: route through the chokepoint so the
                            // readout tracks the re-fit. Necessarily BEFORE
                            // `configure()`, since the config below is built from the
                            // fields this sets — see `set_encode_dims`' doc and
                            // issue #2177 for the refusal path.
                            layer.set_encode_dims(
                                constrained_w,
                                constrained_h,
                                &shared_layer_dims,
                                &enabled,
                                &loop_epoch,
                                my_epoch,
                            );

                            if let Err(e) =
                                layer.reconfigure_at_current_dims_and_bitrate(bitrate_bps)
                            {
                                CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL
                                    .fetch_add(1, Ordering::Relaxed);
                                if is_fatal_encoder_error(&e) {
                                    error!("CameraEncoder: fatal configure error (layer {}), restarting: {e:?}", layer.layer_id);
                                    fatal_reconfigure = Some(RestartReason::Configure);
                                    break;
                                }
                                error!("Error reconfiguring camera encoder for tier change (layer {}): {e:?}", layer.layer_id);
                            }
                        }

                        if let SingleStreamReconfigure::Bitrate { bitrate_bps } = plan {
                            // Guard: do not configure a closed encoder.
                            if layer.encoder.state() == CodecState::Closed {
                                log::warn!("CameraEncoder: encoder closed before bitrate reconfigure (layer {})", layer.layer_id);
                                fatal_reconfigure = Some(RestartReason::ClosedCodec);
                                break;
                            }
                            log::info!(
                                "Updating video bitrate to {bitrate_bps} (layer {})",
                                layer.layer_id
                            );
                            if let Err(e) = layer.reconfigure_at_bitrate(bitrate_bps) {
                                CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL
                                    .fetch_add(1, Ordering::Relaxed);
                                if is_fatal_encoder_error(&e) {
                                    error!("CameraEncoder: fatal configure error (layer {}), restarting: {e:?}", layer.layer_id);
                                    fatal_reconfigure = Some(RestartReason::Configure);
                                    break;
                                }
                                error!(
                                    "Error configuring video encoder (layer {}): {e:?}",
                                    layer.layer_id
                                );
                            }
                        }
                    }
                    if let Some(reason) = fatal_reconfigure {
                        record_camera_restart(reason);
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
                        //
                        // The directional reason classification and the per-layer
                        // detail formatting are extracted into host-testable pure
                        // helpers (`shed_reason` / `format_layer_transition`,
                        // issue #1106) so the exact emitted string is covered by a
                        // unit test off-wasm. We project each live `LayerEncoder`
                        // into a decoder-free `LayerView` here; the helper produces
                        // the byte-identical message the inline block did.
                        let layer_views: Vec<LayerView> = layers
                            .iter()
                            .map(|l| LayerView {
                                id: l.layer_id,
                                w: l.current_w,
                                h: l.current_h,
                                bitrate_bps: l.local_bitrate,
                            })
                            .collect();
                        log::info!(
                            "{}",
                            format_layer_transition(
                                prev_active_layers,
                                local_active_layers,
                                &layer_views
                            )
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

                            // Resolve the PLI keyframe request ONCE per frame and
                            // apply the SAME keyframe flag to every layer (reading
                            // it per-layer would let only the first layer see the
                            // request, desynchronizing keyframes across layers).
                            //
                            // Emit-side coalescer (issue #1287): a forced keyframe is
                            // broadcast to ALL receivers, so ONE emission satisfies
                            // every pending requester. We therefore PEEK the request
                            // (`load`, not `swap`) and only honor it outside the
                            // forced-keyframe cooldown window — collapsing a burst of
                            // PLIs from many receivers into at most one forced
                            // keyframe per FORCED_KEYFRAME_COOLDOWN_MS. A request that
                            // arrives mid-window is left PENDING (flag not cleared)
                            // and honored the instant the window expires, so it is
                            // never lost; added recovery latency is bounded by the
                            // cooldown. The periodic GOP keyframe is never gated.
                            let now_ms = window().performance().unwrap().now();
                            // Issue #1531: refresh the sticky transport cache, then
                            // resolve the tier- and transport-aware wall-clock ceiling.
                            // On the lowest AQ tiers the ceiling relaxes so a
                            // bandwidth-scarce publisher spends fewer forced I-frames;
                            // full_hd … low keep the flat 5s #1510 guarantee over WT.
                            if let Some(t) = client.active_transport() {
                                lossless_transport = t == "websocket";
                            }
                            let keyframe_max_interval_ms = camera_periodic_keyframe_max_interval_ms(
                                shared_video_tier_index.load(Ordering::Relaxed) as usize,
                                lossless_transport,
                            );
                            let is_periodic_keyframe = periodic_keyframe_due(
                                video_frame_counter,
                                local_keyframe_interval,
                                now_ms,
                                last_keyframe_emit_ms,
                                keyframe_max_interval_ms,
                            );
                            // Resolve the keyframe decision via the shared single
                            // source of truth (issue #1347 item 2: the camera AND
                            // screen loops call the same pure `keyframe_tick_decision`,
                            // which the host tests pin). It folds:
                            //  * #1311 cooldown reset — a reconnect or re-election just
                            //    happened (the `keyframe_cooldown_reset` one-shot edge,
                            //    `.swap(false)`-consumed here so a single transition
                            //    resets exactly once); the decision clears the stale
                            //    cooldown clock so the FIRST post-transition PLI emits
                            //    immediately instead of being coalesced away (up to
                            //    FORCED_KEYFRAME_COOLDOWN_MS = 250ms of suppressed
                            //    recovery). It only un-gates an ALREADY-pending PLI —
                            //    never forces an unrequested keyframe.
                            //  * #1287 PLI coalescer — PEEK the request flag (`load`,
                            //    not `swap`) so a mid-cooldown PLI stays pending and is
                            //    honored at window expiry rather than dropped.
                            //  * periodic GOP — never gated by the cooldown.
                            let decision = keyframe_tick_decision(KeyframeTickInput {
                                now_ms,
                                pli_pending: force_keyframe.load(Ordering::Acquire),
                                is_periodic: is_periodic_keyframe,
                                cooldown_reset: keyframe_cooldown_reset
                                    .swap(false, Ordering::AcqRel),
                                last_keyframe_emit_ms,
                                cooldown_ms: FORCED_KEYFRAME_COOLDOWN_MS,
                                tier_change_pending: tier_change_keyframe.load(Ordering::Acquire),
                            });
                            let want_keyframe = decision.want_keyframe;
                            last_keyframe_emit_ms = decision.last_keyframe_emit_ms;
                            if decision.clear_force_keyframe {
                                // ANY keyframe (periodic or forced) is broadcast to
                                // the whole room, so it satisfies every pending PLI
                                // AND makes the reconfigured stream decodable — clear
                                // both request flags. Clearing only on an actual emit
                                // is what lets a mid-cooldown request survive to be
                                // honored at window expiry.
                                force_keyframe.store(false, Ordering::Release);
                                tier_change_keyframe.store(false, Ordering::Release);
                            }
                            if let Some(cause) = decision.forced_cause {
                                log::info!(
                                    "CameraEncoder: forcing keyframe at frame {} ({})",
                                    video_frame_counter,
                                    cause.label()
                                );
                            }

                            // Frame display dimensions, read once; each layer
                            // clamps to its own current dims + the shared tier max.
                            let frame_width = video_frame.display_width();
                            let frame_height = video_frame.display_height();
                            // #1196: correct the source-aspect stamp from the
                            // first real frame (and later size changes). The
                            // decision lives in the unit-tested pure
                            // `corrected_source_dims`; steady-state frames store
                            // nothing, and a blank (0x0) frame never clobbers a
                            // known stamp.
                            if let Some((corrected_w, corrected_h)) = corrected_source_dims(
                                frame_width,
                                frame_height,
                                source_width_atomic.load(Ordering::Relaxed),
                                source_height_atomic.load(Ordering::Relaxed),
                            ) {
                                source_width_atomic.store(corrected_w, Ordering::Relaxed);
                                source_height_atomic.store(corrected_h, Ordering::Relaxed);
                            }

                            // Restart reason captured from a fatal per-frame
                            // encode/reconfigure cause (issue #527). `None` while
                            // the frame encodes cleanly.
                            let mut fatal_encode: Option<RestartReason> = None;
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

                                // Per-layer framerate cap (issue #1768): a simulcast
                                // rung encodes at most its `target_fps`. DROP (skip)
                                // this frame for the layer if it arrived faster than
                                // the rung's frame interval — real-time over
                                // smoothness, never queued. A keyframe (periodic GOP
                                // or PLI) is NEVER dropped, so every layer's GOP
                                // stays coherent and the shared keyframe cadence is
                                // preserved. Single stream is never gated here
                                // (`simulcast == false` short-circuits, and its
                                // `min_encode_interval_ms` is 0.0 regardless).
                                if simulcast
                                    && !should_encode_layer_frame(
                                        now_ms,
                                        layer.last_encode_ms,
                                        layer.min_encode_interval_ms,
                                        want_keyframe,
                                    )
                                {
                                    continue;
                                }
                                // Passed the cap (or it's a keyframe): this layer
                                // WILL encode this frame. Anchor the next interval to
                                // the ACTUAL encode time (drift-free) rather than a
                                // fixed grid. Only simulcast layers track this.
                                if simulcast {
                                    layer.last_encode_ms = now_ms;
                                }

                                // Dimension-change handling (rotation, camera
                                // switch). Both paths now treat the tier as a
                                // BOUNDING BOX and fit the source frame inside it
                                // aspect-preserving (issue #1196) — neither path
                                // configures the encoder at raw per-axis tier
                                // dims, which would bake a stretch/squash into the
                                // stream for a non-16:9 capture.
                                //  - SIMULCAST: each layer re-fits the frame into
                                //    ITS OWN tier rung (`layer.tier_w/tier_h`) and
                                //    reconfigures when the fitted dims drift from
                                //    `layer.current_w/current_h` (handled in the
                                //    `simulcast` branch just below). The seed at
                                //    construction already fitted the first GOP, so
                                //    in steady state this is a no-op.
                                //  - SINGLE-STREAM: legacy behavior — follow the
                                //    frame size, constrained to the shared current
                                //    tier max while preserving aspect (#1037),
                                //    handled in the `!simulcast` branch.
                                //
                                // `frame_width` / `frame_height` are the raw
                                // native VideoFrame dimensions (the true source
                                // aspect). The single-stream path fits them
                                // against the shared `local_tier_max_*` (computed
                                // inside the `!simulcast` branch below so the fit
                                // runs only when consumed — not ~N× per second on
                                // the simulcast path); the simulcast branch fits
                                // against each layer's own tier box via
                                // `simulcast_layer_target_dims`.

                                // SIMULCAST per-layer aspect re-fit (issue #1196).
                                // Fit the source frame into THIS layer's tier box
                                // and reconfigure only when the fitted dims drift
                                // from the current config. Mirrors the single-
                                // stream reconfigure block below (closed-encoder
                                // guard, fatal handling, log line).
                                if simulcast {
                                    let decision = simulcast_layer_target_dims(
                                        frame_width,
                                        frame_height,
                                        layer.tier_w,
                                        layer.tier_h,
                                        layer.current_w,
                                        layer.current_h,
                                    );
                                    if decision.needs_reconfigure {
                                        // Guard: do not configure a closed encoder.
                                        if layer.encoder.state() == CodecState::Closed {
                                            log::warn!("CameraEncoder: encoder closed before per-layer dimension reconfigure (layer {})", layer.layer_id);
                                            fatal_encode = Some(RestartReason::ClosedCodec);
                                            break;
                                        }

                                        log::info!(
                                            "CameraEncoder: layer dimension change -> {}x{} (was {}x{}) within tier {}x{} (layer {})",
                                            decision.target_w,
                                            decision.target_h,
                                            layer.current_w,
                                            layer.current_h,
                                            layer.tier_w,
                                            layer.tier_h,
                                            layer.layer_id,
                                        );

                                        // Issue #2170: publish the re-fit through the
                                        // chokepoint (see `set_encode_dims`).
                                        layer.set_encode_dims(
                                            decision.target_w,
                                            decision.target_h,
                                            &shared_layer_dims,
                                            &enabled,
                                            &loop_epoch,
                                            my_epoch,
                                        );

                                        if let Err(e) = layer.reconfigure_at_current_dims() {
                                            CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL
                                                .fetch_add(1, Ordering::Relaxed);
                                            if is_fatal_encoder_error(&e) {
                                                error!("CameraEncoder: fatal configure error (layer {}), restarting: {e:?}", layer.layer_id);
                                                fatal_encode = Some(RestartReason::Configure);
                                                break;
                                            }
                                            error!("Error reconfiguring camera layer for dimension change (layer {}): {e:?}", layer.layer_id);
                                        }
                                    }
                                }

                                // SINGLE-STREAM aspect re-fit (#1037). Compute the
                                // fit only here, inside the `!simulcast` branch, so
                                // the simulcast path doesn't pay ~N× fit calls per
                                // second for a value it never consumes.
                                if !simulcast {
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

                                    if clamped_width > 0
                                        && clamped_height > 0
                                        && (clamped_width != layer.current_w
                                            || clamped_height != layer.current_h)
                                    {
                                        // Guard: do not configure a closed encoder.
                                        if layer.encoder.state() == CodecState::Closed {
                                            log::warn!("CameraEncoder: encoder closed before dimension reconfigure (layer {})", layer.layer_id);
                                            fatal_encode = Some(RestartReason::ClosedCodec);
                                            break;
                                        }

                                        log::info!("Camera dimensions changed from {}x{} to {clamped_width}x{clamped_height}, reconfiguring encoder (layer {})", layer.current_w, layer.current_h, layer.layer_id);

                                        // Issue #2170: publish the re-fit through the
                                        // chokepoint (see `set_encode_dims`).
                                        layer.set_encode_dims(
                                            clamped_width,
                                            clamped_height,
                                            &shared_layer_dims,
                                            &enabled,
                                            &loop_epoch,
                                            my_epoch,
                                        );

                                        if let Err(e) = layer.reconfigure_at_current_dims() {
                                            CAMERA_ENCODER_ERRORS_CONFIGURE_FATAL
                                                .fetch_add(1, Ordering::Relaxed);
                                            if is_fatal_encoder_error(&e) {
                                                error!("CameraEncoder: fatal configure error (layer {}), restarting: {e:?}", layer.layer_id);
                                                fatal_encode = Some(RestartReason::Configure);
                                                break;
                                            }
                                            error!("Error reconfiguring camera encoder with new dimensions (layer {}): {e:?}", layer.layer_id);
                                        }
                                    }
                                }

                                let video_encoder_encode_options = VideoEncoderEncodeOptions::new();
                                video_encoder_encode_options.set_key_frame(want_keyframe);

                                match layer.encoder.encode_with_options(
                                    &video_frame,
                                    &video_encoder_encode_options,
                                ) {
                                    Ok(_) => {
                                        // Per-frame submission counter, anchored to the
                                        // BASE layer (`layer_id == 0`) only (#1067). At
                                        // N>1 every source frame is encoded once per
                                        // active simulcast layer; counting each layer's
                                        // submission would inflate this by ~N× relative
                                        // to the actual delivered-frame cadence (one
                                        // logical frame per capture). Gating on the base
                                        // layer makes the counter track real frame
                                        // cadence regardless of N, and keeps the metric
                                        // a single series — existing dashboards that
                                        // `rate()` it continue to read the true fps at
                                        // both N=1 (unchanged: the only layer IS layer 0)
                                        // and N>1. A per-layer label was rejected because
                                        // it would change cardinality and break those
                                        // single-series panels.
                                        if layer.layer_id == 0 {
                                            CAMERA_ENCODER_FRAMES_SUBMITTED_OK
                                                .fetch_add(1, Ordering::Relaxed);
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
                                            // #527: reuse the same message classification
                                            // as the error counter just bumped above so the
                                            // restart reason agrees with the error bucket
                                            // (closed_codec vs memory).
                                            fatal_encode = Some(restart_reason_from_message(&msg));
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
                            // which only a higher layer encoded is NOT healthy because
                            // it does not prove the invariant base encoder recovered.
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

                            if let Some(reason) = fatal_encode {
                                record_camera_restart(reason);
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

impl Drop for CameraEncoder {
    fn drop(&mut self) {
        // Release immediately on Host teardown. The AQ future also owns this Rc,
        // so relying on the tracker's eventual Drop would retain the registry
        // entry until the sleeping loop observed its liveness token.
        self.screen_ready_stall_tracker.borrow_mut().release();
    }
}

#[cfg(test)]
mod tests {
    use super::apply_camera_tier_change;
    use super::CameraEncoder;
    use super::RestartReason;
    use super::{
        bitrate_attempt_allowed, build_simulcast_layers, camera_encoder_restarts_closed_codec,
        camera_encoder_restarts_configure, camera_encoder_restarts_memory,
        camera_encoder_restarts_other, camera_wt_stale_drop_step_down_decision, clamp_layer_count,
        clear_video_at_floor_on_enable_edge, encoders_to_build, format_layer_transition,
        frame_is_healthy, initial_active_layer_count, is_fatal_encoder_error_message,
        keyframe_tick_decision, layer_ceiling_to_count, loop_is_superseded, next_single_layer_pin,
        periodic_keyframe_due, plan_single_stream_reconfigure, record_camera_restart,
        republish_geometry_on_tick, resolve_capture_dimensions,
        screen_ready_stall_threshold_update, shed_reason, should_encode_layer_frame,
        should_pin_single_layer_low, should_teardown_shed_layer, simulcast_layer_encode_params,
        video_at_floor_on_tick, wt_drop_step_down_decision, wt_saturation_step_down_decision,
        CameraTierChangeTargets, KeyframeTickInput, LayerView, ScreenReadyStallThresholdTracker,
        SimulcastLayerInfo, SingleStreamReconfigure, FORCED_KEYFRAME_COOLDOWN_MS,
        SHED_TEARDOWN_DWELL_MS, SIMULCAST_MAX_SUPPORTED_LAYERS,
        SINGLE_LAYER_LOW_PIN_ENGAGE_THRESHOLD, SINGLE_LAYER_LOW_PIN_RELEASE_THRESHOLD,
    };
    use crate::encode::encoder_state::ForcedKeyframeCause;

    use crate::adaptive_quality_constants::{
        CAMERA_WT_STALE_DROP_THRESHOLD, CAMERA_WT_STALE_DROP_WINDOW_MS,
        WS_SELF_CONGESTION_DROP_THRESHOLD, WS_SELF_CONGESTION_WINDOW_MS,
        WT_SATURATION_STALL_THRESHOLD, WT_SATURATION_WINDOW_MS, WT_SELF_CONGESTION_DROP_THRESHOLD,
        WT_SELF_CONGESTION_WINDOW_MS,
    };
    use crate::{Callback, VideoCallClient, VideoCallClientOptions};
    // Issue #2170: the fitted-dims publication primitives + the shared-cell types
    // the seeding helper needs to stand in for the encode loop.
    use super::loop_may_publish_geometry;
    use super::{
        clear_layer_dim, clear_layer_dims, clear_layer_output_metric, clear_layer_output_metrics,
        loop_owns_shared_geometry, normalize_window_fps, publish_layer_output_fps,
    };
    use super::{pack_layer_dims, publish_layer_dims};
    use super::{unpack_layer_dims, VIDEO_QUALITY_TIERS};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::Ordering;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};

    #[test]
    fn camera_keyframe_ceiling_stays_inside_receiver_hold_ceiling_2199() {
        use crate::adaptive_quality_constants::{
            camera_periodic_keyframe_max_interval_ms, screen_share_camera_ceiling_index,
        };
        use videocall_codecs::jitter_buffer::MAX_KEYFRAME_LESS_HOLD_MS;

        for idx in 0..VIDEO_QUALITY_TIERS.len() {
            for &lossless in &[false, true] {
                let ms = camera_periodic_keyframe_max_interval_ms(idx, lossless);
                assert!(
                    ms < MAX_KEYFRAME_LESS_HOLD_MS,
                    "tier {idx} (lossless={lossless}) publisher ceiling {ms}ms must stay under \
                     the receiver's {MAX_KEYFRAME_LESS_HOLD_MS}ms keyframe-less-hold escalation"
                );
            }
        }

        let shared_ms =
            camera_periodic_keyframe_max_interval_ms(screen_share_camera_ceiling_index(), true);
        assert!(
            shared_ms < MAX_KEYFRAME_LESS_HOLD_MS,
            "screen-share forces the camera to tier {}; over WebSocket that resolves \
             {shared_ms}ms, which must stay under the receiver's {MAX_KEYFRAME_LESS_HOLD_MS}ms \
             escalation",
            screen_share_camera_ceiling_index()
        );
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

    /// #2060 (PR #2077 review): `CameraEncoder::stop()` must reset the shared
    /// `current_fps` atomic to 0 so a re-enable re-warms honestly. Camera is the
    /// path wired to `window.__videocall_encoder_fps` and the bots
    /// RESOURCE_STARVED rule, so this call-site is the highest-stakes one (its
    /// `ScreenEncoder` sibling is pinned by `stop_resets_current_fps_to_zero`).
    /// MUTATION: delete `reset_output_fps(&self.current_fps)` from `stop()` and
    /// this assertion fails (current_fps stays 30).
    #[test]
    fn stop_resets_current_fps_to_zero() {
        let client = build_test_client();
        let mut encoder = CameraEncoder::new(
            client,
            "test-video-elem",
            500,
            Callback::from(|_: String| {}),
            Callback::from(|_: String| {}),
            1, // max_layers (single layer)
        );
        // Simulate a live encoder that has produced layer-0 output.
        encoder.current_fps.store(30, Ordering::Relaxed);
        assert_eq!(encoder.get_current_fps(), 30);
        encoder.stop();
        assert_eq!(
            encoder.get_current_fps(),
            0,
            "#2060: CameraEncoder::stop() must reset current_fps to 0"
        );
    }

    /// #2458. MUTATION: return a fresh handle from `control_loop_cancel_token()`.
    #[test]
    fn control_loop_cancel_token_exits_loop_without_dropping_encoder_2458() {
        let encoder = encoder_with_layers(1);
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

    fn encoder_with_layers(max_layers: u32) -> CameraEncoder {
        CameraEncoder::new(
            build_test_client(),
            "test-video-elem",
            500,
            Callback::from(|_: String| {}),
            Callback::from(|_: String| {}),
            max_layers,
        )
    }

    // ---------------------------------------------------------------------
    // Issue #2170 — the FITTED-vs-NOMINAL geometry guards for the SELF-VIEW
    // readout.
    //
    // The reader tests below exercise the real `CameraEncoder` methods, NOT a pure
    // helper. That distinction is the point: before #2170 the fitted dims existed
    // only inside the encode loop's `LayerEncoder`, and the readout re-derived the
    // AQ tier's BOUNDING BOX from the table. A test that exercised only an
    // extracted helper would leave the call site revertible-green.
    //
    // Seeded values are deliberately dims that NO ladder rung and NO AQ tier
    // contains (`613x461` etc.), so an implementation that reads any table cannot
    // accidentally produce them. Each such test also asserts the fixture is
    // DISCRIMINATING rather than assuming it.
    //
    // What these CANNOT reach: the ten write call sites inside the encode-loop
    // future (four `set_encode_dims` callers, the shed-teardown `clear_layer_dim`,
    // the `'restart`-head `clear_layer_dims`, the OFF→ON republish loop, and issue
    // #2170's three fps writes — `publish_layer_output_fps` plus the restart-head and
    // shed-teardown `clear_layer_output_metric(s)` calls). They
    // need a real `VideoEncoder`. The disclosure lives at `shared_layer_dims`'
    // clone in `start()`.
    // ---------------------------------------------------------------------

    /// Seed `shared_layer_dims` as the encode loop would, without a browser.
    fn seed_published_dims(encoder: &CameraEncoder, dims: &[(u32, u32)]) {
        let mut slots = encoder.shared_layer_dims.borrow_mut();
        *slots = dims
            .iter()
            .map(|(w, h)| Rc::new(AtomicU32::new(pack_layer_dims(*w, *h))))
            .collect();
    }

    fn seed_layer_output_fps(encoder: &CameraEncoder, values: &[Option<u32>]) {
        *encoder.shared_layer_output_fps.borrow_mut() = values
            .iter()
            .map(|value| Rc::new(AtomicU32::new(value.unwrap_or(0))))
            .collect();
        *encoder.shared_layer_output_fps_ready.borrow_mut() = values
            .iter()
            .map(|value| Rc::new(AtomicBool::new(value.is_some())))
            .collect();
        *encoder.shared_layer_last_chunk_ms.borrow_mut() = values
            .iter()
            .map(|_| Rc::new(AtomicU64::new(1_000)))
            .collect();
    }

    #[test]
    fn metric_source_excludes_shed_layers() {
        let encoder = encoder_with_layers(3);
        seed_published_dims(&encoder, &[(241, 181), (481, 361)]);
        seed_layer_output_fps(&encoder, &[Some(7), Some(15)]);
        encoder
            .shared_active_layer_count
            .store(1, Ordering::Relaxed);

        assert_eq!(
            encoder
                .camera_layer_metric_source()
                .reportable_layers_at(1_100.0),
            vec![(0, 241, 181, Some(7))]
        );
    }

    #[test]
    fn metric_source_excludes_unbuilt_layers_and_preserves_ladder_indices() {
        let encoder = encoder_with_layers(3);
        seed_published_dims(&encoder, &[(241, 181), (0, 0), (613, 461)]);
        seed_layer_output_fps(&encoder, &[Some(7), None, Some(30)]);
        encoder
            .shared_active_layer_count
            .store(3, Ordering::Relaxed);

        assert_eq!(
            encoder
                .camera_layer_metric_source()
                .reportable_layers_at(1_100.0),
            vec![(0, 241, 181, Some(7)), (2, 613, 461, Some(30))]
        );
    }

    #[test]
    fn metric_source_reads_live_at_report_time() {
        let encoder = encoder_with_layers(1);
        seed_published_dims(&encoder, &[(241, 181)]);
        seed_layer_output_fps(&encoder, &[Some(7)]);
        let source = encoder.camera_layer_metric_source();
        assert_eq!(
            source.reportable_layers_at(1_100.0),
            vec![(0, 241, 181, Some(7))]
        );

        encoder.shared_layer_dims.borrow()[0].store(pack_layer_dims(319, 179), Ordering::Relaxed);
        encoder.shared_layer_output_fps.borrow()[0].store(6, Ordering::Relaxed);
        assert_eq!(
            source.reportable_layers_at(1_100.0),
            vec![(0, 319, 179, Some(6))]
        );
    }

    #[test]
    fn metric_source_preserves_unmeasured_vs_measured_zero_fps() {
        let encoder = encoder_with_layers(2);
        seed_published_dims(&encoder, &[(241, 181), (481, 361)]);
        seed_layer_output_fps(&encoder, &[None, Some(0)]);
        encoder
            .shared_active_layer_count
            .store(2, Ordering::Relaxed);

        assert_eq!(
            encoder
                .camera_layer_metric_source()
                .reportable_layers_at(1_100.0),
            vec![(0, 241, 181, None), (1, 481, 361, Some(0))]
        );
    }

    /// A rung that stopped emitting must report `Some(0)`, not its last measured fps.
    ///
    /// Guards the AQ shed-then-restore-inside-the-30s-dwell window, where
    /// `clear_layer_output_metric` never runs and the slot keeps its pre-shed value.
    #[test]
    fn metric_source_decays_a_stale_rung_to_zero_but_not_a_fresh_one() {
        let encoder = encoder_with_layers(2);
        seed_published_dims(&encoder, &[(241, 181), (481, 361)]);
        seed_layer_output_fps(&encoder, &[Some(7), Some(15)]);
        encoder
            .shared_active_layer_count
            .store(2, Ordering::Relaxed);
        // `seed_layer_output_fps` stamps both rungs' last chunk at 1_000.
        let idle = crate::adaptive_quality_constants::ENCODER_FPS_IDLE_DECAY_MS;

        // FRESH: just inside the threshold — the measured value must survive. Uses
        // `idle` rather than a literal so a constant change cannot silently move
        // this case onto the other side of the boundary.
        assert_eq!(
            encoder
                .camera_layer_metric_source()
                .reportable_layers_at(1_000.0 + idle - 1.0),
            vec![(0, 241, 181, Some(7)), (1, 481, 361, Some(15))],
            "a rung still inside the idle window must report its measured fps"
        );

        // STALE: at/past the threshold — both decay to a measured 0, and must stay
        // PRESENT (`Some(0)`, a real "encoding nothing" reading), never collapse to
        // `None`, which means "no measurement yet".
        assert_eq!(
            encoder
                .camera_layer_metric_source()
                .reportable_layers_at(1_000.0 + idle),
            vec![(0, 241, 181, Some(0)), (1, 481, 361, Some(0))],
            "a rung that stopped emitting must decay to Some(0), not keep stale fps"
        );

        // The decay must not resurrect an UNMEASURED rung as a measured zero: a
        // `ready == false` slot stays absent no matter how stale it looks.
        seed_layer_output_fps(&encoder, &[None, Some(15)]);
        assert_eq!(
            encoder
                .camera_layer_metric_source()
                .reportable_layers_at(1_000.0 + idle)[0]
                .3,
            None,
            "an unmeasured rung must stay absent, not decay into Some(0)"
        );
    }

    #[test]
    fn per_layer_fps_never_inflates_the_aq_setpoint() {
        let current_fps = AtomicU32::new(0);
        let layer0_fps = AtomicU32::new(0);
        let layer1_fps = AtomicU32::new(0);
        let layer0_ready = AtomicBool::new(false);
        let layer1_ready = AtomicBool::new(false);

        publish_layer_output_fps(0, 7, 7, &layer0_fps, &layer0_ready, &current_fps);
        publish_layer_output_fps(1, 15, 15, &layer1_fps, &layer1_ready, &current_fps);

        assert_eq!(layer0_fps.load(Ordering::Relaxed), 7);
        assert_eq!(layer1_fps.load(Ordering::Relaxed), 15);
        assert_eq!(
            current_fps.load(Ordering::Relaxed),
            7,
            "the AQ setpoint must remain layer 0 only"
        );
    }

    /// The count is normalized by the window it was ACTUALLY measured over, so a resume
    /// after a gap cannot publish an accumulated count as a per-second rate.
    ///
    /// The window closes on the first chunk at or after 1000 ms and the closure's
    /// counter survives the gap, so unnormalized a ~6s stall reads 30 chunks as 30 fps.
    /// Pins the math only; the call site is site 8 (browser-only).
    #[test]
    fn window_fps_is_normalized_by_real_elapsed_but_the_setpoint_keeps_the_raw_count() {
        // Steady state: a 1000-1033ms window must still read the nominal rate, not 29.
        assert_eq!(normalize_window_fps(30, 1000.0), 30);
        assert_eq!(normalize_window_fps(30, 1033.0), 29); // 29.04 -> rounds to 29
        assert_eq!(normalize_window_fps(30, 1016.0), 30); // 29.53 -> rounds to 30
        assert_eq!(normalize_window_fps(7, 1000.0), 7);

        // THE DEFECT THIS EXISTS FOR: 30 chunks accumulated across a ~6s stall is a
        // true rate of 5 fps, not 30.
        assert_eq!(
            normalize_window_fps(30, 5967.0),
            5,
            "an accumulated count over a long window must not read as a per-second rate"
        );
        // A 2x-long window halves the reported rate.
        assert_eq!(normalize_window_fps(30, 2000.0), 15);
        // Degenerate elapsed cannot divide-by-zero or produce a wild value; the
        // caller's `>= 1000.0` guard already excludes it.
        assert_eq!(normalize_window_fps(30, 0.0), 30);

        // And the AQ setpoint deliberately still receives the RAW count: normalizing a
        // control input would change adaptive behavior on the stalled/resumed paths,
        // which is out of scope for an observability change.
        let current_fps = AtomicU32::new(0);
        let layer0_fps = AtomicU32::new(0);
        let layer0_ready = AtomicBool::new(false);
        publish_layer_output_fps(0, 5, 30, &layer0_fps, &layer0_ready, &current_fps);
        assert_eq!(
            layer0_fps.load(Ordering::Relaxed),
            5,
            "the EXPORTED metric takes the normalized rate"
        );
        assert_eq!(
            current_fps.load(Ordering::Relaxed),
            30,
            "the AQ setpoint keeps the raw window count — pre-existing behavior, \
             deliberately unchanged by issue 2170"
        );
    }

    /// `stop()` clears the fps slots, and the two clear HELPERS do what their names say:
    /// a per-rung clear leaves its neighbours alone, a whole-vec clear does not.
    ///
    /// Pins `stop()` (a real call site) plus both helpers — NOT the `'restart`-head or
    /// shed-teardown calls, which are sites 9 and 10.
    #[test]
    fn per_layer_fps_clears_on_stop_and_the_clear_helpers() {
        let mut encoder = encoder_with_layers(3);
        seed_published_dims(&encoder, &[(241, 181), (481, 361), (613, 461)]);
        seed_layer_output_fps(&encoder, &[Some(7), Some(15), Some(30)]);

        encoder.stop();
        assert!(encoder
            .shared_layer_output_fps
            .borrow()
            .iter()
            .all(|slot| slot.load(Ordering::Relaxed) == 0));
        assert!(encoder
            .shared_layer_output_fps_ready
            .borrow()
            .iter()
            .all(|slot| !slot.load(Ordering::Relaxed)));

        seed_layer_output_fps(&encoder, &[Some(7), Some(15), Some(30)]);
        clear_layer_output_metric(
            &encoder.shared_layer_output_fps,
            &encoder.shared_layer_output_fps_ready,
            &encoder.shared_layer_last_chunk_ms,
            2,
        );
        assert_eq!(
            encoder.shared_layer_output_fps.borrow()[1].load(Ordering::Relaxed),
            15
        );
        assert!(!encoder.shared_layer_output_fps_ready.borrow()[2].load(Ordering::Relaxed));

        clear_layer_output_metrics(
            &encoder.shared_layer_output_fps,
            &encoder.shared_layer_output_fps_ready,
            &encoder.shared_layer_last_chunk_ms,
        );
        assert!(encoder
            .shared_layer_output_fps_ready
            .borrow()
            .iter()
            .all(|slot| !slot.load(Ordering::Relaxed)));
    }

    /// **THE STALE-LOOP FENCE.** Only the CURRENT encode-loop generation may write
    /// the shared geometry (issue #2170).
    ///
    /// Why this is a fence and not a nicety: there are awaits between a loop's
    /// acquire-phase supersede check and its first publish
    /// (`video_element.play().await`, plus a 200 ms sleep on the autoplay retry) and
    /// before every per-frame publish (`video_reader.read().await`). So a stale loop
    /// CAN resume after a newer one has published. If it wrote, the corruption would
    /// be permanent: it then exits without clearing (the `'restart`-head clear is
    /// fenced by this same predicate, so it skips too) and every other writer is
    /// gated on a change its write does not move — the readout would report a dead
    /// generation's geometry for the rest of the session.
    ///
    /// This is deliberately NOT `loop_is_superseded`: that ORs in `!enabled`, which
    /// would make a still-current DISABLED loop decline to clear its own geometry,
    /// breaking `stop()`'s contract. The asserts below pin that difference, so
    /// "simplifying" one into the other fails.
    ///
    /// MUTATION (run): invert to `!=` and every case flips. Replace the body with
    /// `loop_is_superseded(enabled, loop_epoch, my_epoch)` and the disabled-but-
    /// current case fails.
    #[test]
    fn loop_owns_shared_geometry_only_for_the_current_generation() {
        // The current generation owns the shared slots.
        assert!(loop_owns_shared_geometry(7, 7));
        // A superseded loop does not — neither behind...
        assert!(!loop_owns_shared_geometry(8, 7));
        // ...nor (defensively) ahead of the epoch it captured.
        assert!(!loop_owns_shared_geometry(6, 7));
        // Cold start: epoch 0 is a legitimate generation, not a sentinel.
        assert!(loop_owns_shared_geometry(0, 0));

        // THE DIVERGENCE FROM `loop_is_superseded`, pinned. A still-current loop that
        // has been DISABLED must still own its geometry, because `stop()` clears
        // through this predicate. `loop_is_superseded` says "superseded" there
        // (it ORs in `!enabled`), so the two are NOT interchangeable.
        assert!(
            loop_owns_shared_geometry(7, 7),
            "a disabled-but-current loop must still be allowed to clear its own dims"
        );
        assert!(
            loop_is_superseded(false, 7, 7),
            "precondition: loop_is_superseded treats !enabled as superseded — which is \
             why it cannot be reused as the geometry-ownership fence"
        );
    }

    /// **THE STOP/RESUME FENCE.** A loop of the CURRENT generation that is parked in
    /// an await across a `stop()` must not publish over the sentinel `stop()` wrote.
    ///
    /// This is a distinct hazard from the stale-generation one, and the epoch alone
    /// does NOT cover it: `CameraEncoder::stop()` clears the slots and sets
    /// `enabled = false`, but does **not** bump `loop_epoch` (`EncoderState::stop`
    /// touches only `enabled`/`switching`). So the parked loop still satisfies
    /// `loop_epoch == my_epoch`. The acquire-phase `loop_is_superseded` check does
    /// read `enabled`, but it runs BEFORE `video_element.play().await` and its 200 ms
    /// autoplay-retry sleep, with no re-check before `build_layer` publishes — so a
    /// `stop()` landing in that window would leave a stopped camera reporting a live
    /// resolution, permanently (nothing republishes; every other writer is gated on a
    /// dimension CHANGE a clear does not move).
    ///
    /// The asymmetry with the CLEAR is the point and is asserted below: the
    /// `'restart`-head clear uses the epoch-only predicate on purpose, because a
    /// still-current but DISABLED loop *should* clear — that is `stop()`'s contract.
    /// Publishes tighten; clears do not.
    ///
    /// MUTATION (run): drop the `enabled &&` from `loop_may_publish_geometry` and the
    /// disabled cases fail. Make the `'restart` clear use this predicate instead of
    /// `loop_owns_shared_geometry` and `stop_clears_published_dims_and_the_readout_
    /// reads_unknown`'s contract is what breaks in production.
    #[test]
    fn loop_may_publish_geometry_requires_enabled_as_well_as_the_epoch() {
        // Current generation, still enabled → may publish.
        assert!(loop_may_publish_geometry(true, 7, 7));

        // Current generation but STOPPED → must NOT publish. This is the
        // parked-in-`play().await`-across-`stop()` case; the epoch is unchanged
        // because `stop()` does not bump it.
        assert!(
            !loop_may_publish_geometry(false, 7, 7),
            "a stopped-but-current loop must not republish over stop()'s sentinel"
        );

        // Superseded generation → must NOT publish, enabled or not.
        assert!(!loop_may_publish_geometry(true, 8, 7));
        assert!(!loop_may_publish_geometry(false, 8, 7));

        // THE ASYMMETRY, pinned: the same disabled-but-current state that is barred
        // from PUBLISHING is still allowed to CLEAR. Collapsing the two predicates
        // into one — in either direction — breaks one of the two contracts.
        assert!(
            loop_owns_shared_geometry(7, 7),
            "the clear predicate must still admit a disabled-but-current loop, or \
             stop() would not be able to clear its own geometry"
        );
        assert!(!loop_may_publish_geometry(false, 7, 7));
    }

    #[test]
    fn pack_layer_dims_round_trips_and_clamps() {
        assert_eq!(unpack_layer_dims(pack_layer_dims(640, 480)), (640, 480));
        assert_eq!(unpack_layer_dims(pack_layer_dims(1920, 1080)), (1920, 1080));
        // `0` is the "not yet published" sentinel every reader keys off.
        assert_eq!(unpack_layer_dims(0), (0, 0));
        // Out-of-range clamps rather than corrupting the neighbouring axis: an
        // unclamped shift would let a >16-bit width overwrite the height bits.
        let (w, h) = unpack_layer_dims(pack_layer_dims(70_000, 70_000));
        assert_eq!((w, h), (u16::MAX as u32, u16::MAX as u32));
    }

    /// **THE WRITER guard.** `publish_layer_dims` is the publication mechanism the
    /// whole of #2170 rests on, and it needs its own test because every reader test
    /// seeds the shared vec directly via `seed_published_dims` and bypasses it —
    /// deleting the writer's body would otherwise leave the suite green.
    ///
    /// MUTATION (run): delete the `store(..)` and the stored-pair assertions fail;
    /// delete the `resize_with` and the grow-on-demand assertion panics on index.
    #[test]
    fn publish_layer_dims_grows_on_demand_and_stores_the_packed_pair() {
        let cell: RefCell<Vec<Rc<AtomicU32>>> = RefCell::new(Vec::new());

        // Publishing layer 2 into an EMPTY vec must grow it to length 3, not panic
        // and not silently no-op: the cold-start build publishes only the layers it
        // constructs, so a lazily-earned upper rung is the normal first writer.
        publish_layer_dims(&cell, 2, 640, 480);
        assert_eq!(cell.borrow().len(), 3, "must grow to hold layer_id 2");
        assert_eq!(
            unpack_layer_dims(cell.borrow()[2].load(Ordering::Relaxed)),
            (640, 480)
        );
        // Slots skipped by the grow stay at the not-yet-published sentinel.
        assert_eq!(
            unpack_layer_dims(cell.borrow()[0].load(Ordering::Relaxed)),
            (0, 0)
        );
        assert_eq!(
            unpack_layer_dims(cell.borrow()[1].load(Ordering::Relaxed)),
            (0, 0)
        );

        // A later publish OVERWRITES in place and does not re-grow — an encoder
        // reconfigure must not leave the previous geometry readable anywhere.
        publish_layer_dims(&cell, 2, 960, 540);
        assert_eq!(cell.borrow().len(), 3, "re-publish must not grow again");
        assert_eq!(
            unpack_layer_dims(cell.borrow()[2].load(Ordering::Relaxed)),
            (960, 540)
        );

        // Filling a lower slot leaves the higher one intact.
        publish_layer_dims(&cell, 0, 240, 180);
        let got: Vec<(u32, u32)> = cell
            .borrow()
            .iter()
            .map(|a| unpack_layer_dims(a.load(Ordering::Relaxed)))
            .collect();
        assert_eq!(got, vec![(240, 180), (0, 0), (960, 540)]);
    }

    /// A failed borrow must SKIP the publish, never panic — a diagnostics write
    /// must not be able to take down a live encode.
    ///
    /// MUTATION (run): change `try_borrow_mut` to `borrow_mut` and this panics with
    /// "already mutably borrowed".
    #[test]
    fn publish_layer_dims_skips_rather_than_panics_when_the_cell_is_borrowed() {
        let cell: RefCell<Vec<Rc<AtomicU32>>> = RefCell::new(Vec::new());
        let _held = cell.borrow_mut(); // simulate a reader mid-scan
        publish_layer_dims(&cell, 0, 640, 480); // must not panic
    }

    /// The shed-teardown clear zeroes ONLY the torn-down rung; rungs still encoding
    /// keep their geometry.
    ///
    /// Extracted from the teardown loop so this decision is reachable at all — but
    /// note the limit: this pins the HELPER. The `clear_layer_dim(...)` CALL in that
    /// loop is one of the ten browser-only sites.
    ///
    /// MUTATION (run): delete the `store(0, ..)` and the torn-down assertion fails.
    /// Widen it to clear ALL slots (i.e. call `clear_layer_dims`) and the
    /// surviving-rung assertions fail. Drop the `dims.get(layer_id)` bound and the
    /// out-of-range case panics instead of no-op'ing.
    #[test]
    fn clear_layer_dim_zeroes_only_the_torn_down_rung() {
        let dims: RefCell<Vec<Rc<AtomicU32>>> = RefCell::new(
            [(240, 180), (480, 360), (640, 480)]
                .iter()
                .map(|(w, h)| Rc::new(AtomicU32::new(pack_layer_dims(*w, *h))))
                .collect(),
        );
        let read = |i: usize| unpack_layer_dims(dims.borrow()[i].load(Ordering::Relaxed));

        // Tear down the TOP rung, as the dwell path does when the AQ sheds it.
        clear_layer_dim(&dims, 2);
        assert_eq!(read(2), (0, 0), "the torn-down rung reports the sentinel");
        assert_eq!(read(0), (240, 180), "the base rung is still encoding");
        assert_eq!(read(1), (480, 360), "the middle rung is still encoding");

        // Out of range is a no-op, not a panic: the ladder can shrink under the
        // teardown path's `id` between the borrow and the call.
        clear_layer_dim(&dims, 99);
        assert_eq!(read(0), (240, 180));
    }

    /// `clear_layer_dims` zeroes EVERY slot — the whole-generation reset used by
    /// `stop()` and the `'restart` head.
    ///
    /// MUTATION (run): delete the `store(0, ..)` inside the loop and both
    /// assertions fail; narrow the loop to the first slot and the second fails.
    #[test]
    fn clear_layer_dims_zeroes_every_slot() {
        let dims: RefCell<Vec<Rc<AtomicU32>>> = RefCell::new(
            [(240, 180), (640, 480)]
                .iter()
                .map(|(w, h)| Rc::new(AtomicU32::new(pack_layer_dims(*w, *h))))
                .collect(),
        );
        clear_layer_dims(&dims);
        let got: Vec<(u32, u32)> = dims
            .borrow()
            .iter()
            .map(|a| unpack_layer_dims(a.load(Ordering::Relaxed)))
            .collect();
        assert_eq!(got, vec![(0, 0), (0, 0)]);
    }

    /// **CALL-SITE guard.** `live_quality_snapshot()` — the self-view "what am I
    /// sending" readout — must report the fitted encode dims, not the AQ tier box.
    ///
    /// MUTATION (run): restore `video_width: v.max_width, video_height:
    /// v.max_height` and this fails with left `(1920, 1080)` right `(613, 461)` — no
    /// entry in `VIDEO_QUALITY_TIERS` is `613x461`.
    ///
    /// `(1920, 1080)` is tier 0 (`full_hd`), NOT the `DEFAULT_VIDEO_TIER_INDEX = 4`
    /// (`medium`, 854x480) that reasoning from the AQ default suggests:
    /// `CameraEncoder::new` seeds `shared_video_tier_index` to `0`, and this fixture
    /// never moves it. An earlier version of this comment said `(854, 480)` — a
    /// value I derived instead of observing. Four sibling tests fail on this same
    /// mutation; this one names it because the tier box is what it exists to reject.
    #[test]
    fn quality_snapshot_reports_fitted_dims_not_the_aq_tier_box() {
        let encoder = encoder_with_layers(3);
        // What a 4:3 webcam actually produces once fitted into the 16:9 rungs.
        seed_published_dims(&encoder, &[(241, 181), (481, 361), (613, 461)]);
        // All three rungs ACTIVE. Without this the fixture would be asserting the
        // geometry of a rung the encoder is not emitting — see the shed test below.
        encoder
            .shared_active_layer_count
            .store(3, Ordering::Relaxed);

        let snap = encoder.live_quality_snapshot();

        assert_eq!(
            (snap.video_width, snap.video_height),
            (613, 461),
            "self-view must show the TOP ACTIVE layer's fitted dims"
        );
        assert!(
            !VIDEO_QUALITY_TIERS
                .iter()
                .any(|t| t.max_width == snap.video_width && t.max_height == snap.video_height),
            "fixture is not discriminating — seeded dims coincide with an AQ tier box"
        );
    }

    /// With nothing published, the self-view reports the `(0, 0)` NOT-YET-PUBLISHED
    /// sentinel — it must NOT substitute the AQ tier box.
    ///
    /// Substituting the box would be unfalsifiable downstream because a tier box is
    /// never 0: `host.rs`'s `self_metrics_overlay` gates on
    /// `video_width > 0 && video_height > 0`, so the self tile could never render
    /// "unknown". It would also mean a STOPPED camera (atomics cleared) went back to
    /// reporting a resolution.
    ///
    /// MUTATION (run): add `.unwrap_or((v.max_width, v.max_height))` in
    /// `live_quality_snapshot` and this fails with left `(1920, 1080)`.
    #[test]
    fn quality_snapshot_reports_the_sentinel_before_any_publish() {
        let encoder = encoder_with_layers(3);
        let snap = encoder.live_quality_snapshot();
        assert_eq!(
            (snap.video_width, snap.video_height),
            (0, 0),
            "pre-first-frame must read as UNKNOWN, not as a tier box"
        );
        // Prove the assertion is discriminating: the box it used to report is a
        // real, non-zero pair, so (0,0) cannot coincide with it.
        let tier = &VIDEO_QUALITY_TIERS[snap.video_tier_index];
        assert!(
            tier.max_width > 0 && tier.max_height > 0,
            "the tier box is non-zero, so (0,0) cannot coincide with it"
        );
    }

    /// `stop()` must clear the published geometry, exactly as it clears the fps
    /// diagnostic directly above (issue #2170), taking the readout back to UNKNOWN.
    ///
    /// Without this, turning the camera off after it has been on once leaves the
    /// last dims standing for the rest of the session: `CameraEncoder` is
    /// constructed once per Host mount and `start`/`stop` merely toggle it, so
    /// nothing else would ever reset them. The readout then shows a live-looking
    /// resolution for a publisher that has stopped.
    ///
    /// MUTATIONS (run):
    /// - remove `clear_layer_dims(&self.shared_layer_dims)` from `stop()` and the
    ///   geometry assertion fails with the stale seeded `(640, 480)`;
    /// - remove the `shared_layer_dims_cleared.fetch_add(...)` producer and the
    ///   counter assertion fails with left `0`, right `1`. Without that producer a
    ///   stalled loop never learns that `stop()` cleared its published geometry, so
    ///   the OFF→ON repair silently stops working.
    #[test]
    fn stop_clears_published_dims_and_the_readout_reads_unknown() {
        let mut encoder = encoder_with_layers(3);
        seed_published_dims(&encoder, &[(240, 180), (480, 360), (640, 480)]);
        encoder
            .shared_active_layer_count
            .store(3, Ordering::Relaxed);
        let live = encoder.live_quality_snapshot();
        assert_eq!(
            (live.video_width, live.video_height),
            (640, 480),
            "precondition: a publishing camera reports its fitted top rung"
        );

        let before_clear_generation = encoder.shared_layer_dims_cleared.load(Ordering::Relaxed);
        encoder.stop();
        assert_eq!(
            encoder.shared_layer_dims_cleared.load(Ordering::Relaxed),
            before_clear_generation + 1,
            "#2170: stop() must record the clear so a stalled loop republishes",
        );

        assert_eq!(
            encoder.top_published_layer_dims(),
            None,
            "#2170: stop() must clear published dims"
        );
        let after = encoder.live_quality_snapshot();
        assert_eq!(
            (after.video_width, after.video_height),
            (0, 0),
            "a stopped camera reports UNKNOWN, not a resolution"
        );
    }

    /// **THE SHED-REGRESSION guard.** Under AQ shed the self-view must report the
    /// top ACTIVE rung, never the highest ever published.
    ///
    /// This is the direction that matters: the pre-#2170 readout used
    /// `VIDEO_QUALITY_TIERS[v_idx]`, whose index steps down under congestion, so it
    /// at least tracked the shed. An unbounded scan of published dims tracks nothing
    /// and over-reports by 16x on the Default ladder at `active == 1` — shown to a
    /// user who opened the overlay precisely because their video looked bad.
    ///
    /// MUTATION (run): drop the `[..upto]` bound in `top_published_layer_dims` and
    /// this fails with left `(613, 461)` right `(241, 181)`.
    #[test]
    fn quality_snapshot_tracks_the_shed_and_does_not_report_shed_rungs() {
        let encoder = encoder_with_layers(3);
        // All three published at some point...
        seed_published_dims(&encoder, &[(241, 181), (481, 361), (613, 461)]);
        // ...but AQ has shed back to the base rung only. A built-then-shed rung
        // keeps NON-zero dims until dwell-teardown, so only the bound excludes it.
        encoder
            .shared_active_layer_count
            .store(1, Ordering::Relaxed);

        let snap = encoder.live_quality_snapshot();
        assert_eq!(
            (snap.video_width, snap.video_height),
            (241, 181),
            "a shed publisher must report its BASE rung, not one it stopped emitting"
        );

        // Restoring the middle rung moves the readout up, but no further.
        encoder
            .shared_active_layer_count
            .store(2, Ordering::Relaxed);
        let snap = encoder.live_quality_snapshot();
        assert_eq!((snap.video_width, snap.video_height), (481, 361));
    }

    /// `top_published_layer_dims` scans DOWNWARD **within the active range**, so a
    /// rung the AQ has counted active but which has not yet lazily built still
    /// yields the best live geometry instead of `None`.
    ///
    /// The bound (previous test) handles SHED rungs; this scan handles UNBUILT ones.
    /// A built-then-shed rung retains non-zero dims, so only a never-built rung
    /// reads `(0, 0)` inside the active range.
    ///
    /// MUTATION (run): drop the `.rev()` and this returns the BASE rung `(241, 181)`.
    #[test]
    fn top_published_layer_dims_picks_the_highest_published_active_rung() {
        let encoder = encoder_with_layers(3);
        // Top rung counted active but NOT yet built -> 0; lower two published.
        seed_published_dims(&encoder, &[(241, 181), (481, 361), (0, 0)]);
        encoder
            .shared_active_layer_count
            .store(3, Ordering::Relaxed);
        assert_eq!(encoder.top_published_layer_dims(), Some((481, 361)));

        let empty = encoder_with_layers(3);
        assert_eq!(empty.top_published_layer_dims(), None);
    }

    /// An active count of 0 (pre-init, before the AQ loop has written) must not
    /// suppress the base rung — the `.max(1)` in `top_published_layer_dims`.
    ///
    /// MUTATION (run): drop the `.max(1)` and this returns `None`, taking the
    /// readout to an em-dash for a camera that is publishing.
    #[test]
    fn top_published_layer_dims_reports_the_base_rung_when_active_count_is_zero() {
        let encoder = encoder_with_layers(3);
        seed_published_dims(&encoder, &[(241, 181)]);
        encoder
            .shared_active_layer_count
            .store(0, Ordering::Relaxed);
        assert_eq!(encoder.top_published_layer_dims(), Some((241, 181)));
    }

    /// The single-stream path (`experimentalSimulcastMaxLayers: 1`, the DEPLOYMENT
    /// DEFAULT) publishes and reads exactly one slot.
    ///
    /// `shared_layer_dims` is sized for single-stream unlike
    /// `shared_layer_bitrates_bps`, precisely so the readout works on this path.
    #[test]
    fn quality_snapshot_reports_fitted_dims_in_single_stream_mode() {
        let encoder = encoder_with_layers(1);
        seed_published_dims(&encoder, &[(613, 461)]);
        let snap = encoder.live_quality_snapshot();
        assert_eq!(
            (snap.video_width, snap.video_height),
            (613, 461),
            "single-stream must report its one fitted rung"
        );
        assert!(
            !VIDEO_QUALITY_TIERS
                .iter()
                .any(|t| t.max_width == snap.video_width && t.max_height == snap.video_height),
            "fixture is not discriminating — seeded dims coincide with an AQ tier box"
        );
    }

    /// The diagnostics ladder is DELIBERATELY unchanged by this PR: it still
    /// resolves rung bounding boxes from the ladder table.
    ///
    /// This pins the descope boundary so the split is explicit rather than an
    /// accident of what got ported. Routing `live_simulcast_snapshot` through
    /// `shared_layer_dims` requires rendering the sentinel in the drawer (em-dash
    /// copy, a11y labels, shed-rung CSS) and is #2170's second half. If you are
    /// implementing that, this test SHOULD fail — replace it, do not delete it.
    #[test]
    fn simulcast_ladder_still_reports_rung_boxes_not_fitted_dims() {
        use videocall_aq::constants::simulcast_layers;

        let encoder = encoder_with_layers(3);
        // Publish fitted dims no rung contains...
        seed_published_dims(&encoder, &[(241, 181), (481, 361), (613, 461)]);
        encoder
            .shared_active_layer_count
            .store(3, Ordering::Relaxed);

        // ...and the ladder still reports the NOMINAL boxes.
        let got: Vec<(u32, u32)> = encoder
            .live_simulcast_snapshot()
            .layers
            .iter()
            .map(|l| (l.width, l.height))
            .collect();
        let nominal: Vec<(u32, u32)> = simulcast_layers(3)
            .iter()
            .map(|t| (t.max_width, t.max_height))
            .collect();
        assert_eq!(
            got, nominal,
            "the ladder half of #2170 is deliberately out of scope in this PR"
        );
    }

    /// **The ENCODE-PATH guard.** The other encoder tests observe
    /// `live_simulcast_snapshot()`, which is observability only, so they stay green
    /// if the encode path alone drifts.
    ///
    /// MUTATION: hardcode any rung field in `simulcast_layer_encode_params` and the
    /// per-rung assertions fail.
    #[test]
    fn encode_path_geometry_comes_from_the_camera_ladder() {
        use videocall_aq::constants::SIMULCAST_VIDEO_LAYERS;

        // A 16:9 source, so the aspect fit is an identity on the rung box and the
        // assertion reads as the rung's own geometry.
        const SRC_W: u32 = 1920;
        const SRC_H: u32 = 1080;

        for (idx, expected) in SIMULCAST_VIDEO_LAYERS.iter().enumerate().take(3) {
            let (w, h, tier_w, tier_h, bitrate_bps, fps) =
                simulcast_layer_encode_params(3, idx, SRC_W, SRC_H);
            assert_eq!(
                (tier_w, tier_h),
                (expected.max_width, expected.max_height),
                "rung {idx}: the encode path must use the ladder's box"
            );
            assert_eq!((w, h), (expected.max_width, expected.max_height));
            assert_eq!(fps, Some(expected.target_fps));
            assert_eq!(bitrate_bps, expected.ideal_bitrate_kbps as f64 * 1000.0);
        }
    }

    /// **The bounding-box guard (issues #1196, #2208).** A rung is a BOX the source
    /// is fitted INSIDE, never upscaled to fill — which is why a lower top rung
    /// saves a typical publisher nothing, the fact #2208 backed the reduced ladder
    /// out on.
    ///
    /// MUTATION: drop `fit_within_preserving_aspect` from
    /// `simulcast_layer_encode_params` and return the raw box dims, or remove its
    /// `.min(1.0)` scale clamp, and these fail.
    #[test]
    fn encode_path_fits_a_non_16_9_source_inside_the_rung_box() {
        // A 4:3 SD webcam is SMALLER than the top-rung box on both axes.
        let (w, h, tier_w, tier_h, _, _) = simulcast_layer_encode_params(3, 2, 640, 480);
        assert_eq!((tier_w, tier_h), (1280, 720), "the box is the rung's");
        assert_eq!(
            (w, h),
            (640, 480),
            "a smaller 4:3 source must pass through un-upscaled, NOT be squashed to \
             the 16:9 box and NOT be stretched up to it"
        );

        // A LARGER 4:3 source scales DOWN into the box keeping 4:3 — height binds,
        // so 720*4/3 = 960 wide. A "return the raw box" mutation fails here.
        let (dw, dh, _, _, _, _) = simulcast_layer_encode_params(3, 2, 1600, 1200);
        assert_eq!(
            (dw, dh),
            (960, 720),
            "a large 4:3 source must fit INSIDE the 720p box at 4:3 (960x720), \
             not be squashed to 1280x720"
        );
    }

    /// An out-of-range rung index degrades to the top rung rather than panicking a
    /// live call.
    #[test]
    fn encode_path_clamps_an_out_of_range_rung_index() {
        let (_, _, tier_w, tier_h, _, _) = simulcast_layer_encode_params(3, 99, 1920, 1080);
        assert_eq!((tier_w, tier_h), (1280, 720), "clamped to the top rung");
    }

    /// The encoder resolves EVERY rung's geometry from the shipped camera ladder.
    /// `live_simulcast_snapshot()` is the observation point because it is the one
    /// geometry read reachable off-browser; the other three sit inside `spawn_local`
    /// futures that need WebCodecs.
    ///
    /// MUTATION: hardcode any rung in `live_simulcast_snapshot`, or resolve it from
    /// a table other than the camera ladder, and this fails.
    #[test]
    fn encoder_geometry_comes_from_the_shipped_camera_ladder() {
        use videocall_aq::constants::SIMULCAST_VIDEO_LAYERS;

        let encoder = encoder_with_layers(3);
        let snap = encoder.live_simulcast_snapshot();
        assert!(snap.simulcast_active, "3 layers => simulcast active");
        assert_eq!(snap.layers.len(), 3, "one entry per effective layer");

        let expected: Vec<(u32, u32)> = SIMULCAST_VIDEO_LAYERS
            .iter()
            .map(|t| (t.max_width, t.max_height))
            .collect();
        let got: Vec<(u32, u32)> = snap.layers.iter().map(|l| (l.width, l.height)).collect();
        assert_eq!(
            got, expected,
            "every rung must resolve from SIMULCAST_VIDEO_LAYERS"
        );
        assert_eq!(
            (snap.layers[2].width, snap.layers[2].height),
            (1280, 720),
            "the camera ladder publishes a 720p top rung"
        );
    }

    // These regression tests guard issue #2034's width-less capture track path.
    #[test]
    fn resolve_capture_dimensions_uses_video_element_when_settings_are_absent() {
        assert_eq!(
            resolve_capture_dimensions(None, None, 1280, 720),
            (1280, 720)
        );
    }

    #[test]
    fn resolve_capture_dimensions_uses_video_element_when_settings_are_zero() {
        assert_eq!(
            resolve_capture_dimensions(Some(0.0), Some(0.0), 1280, 720),
            (1280, 720)
        );
    }

    #[test]
    fn resolve_capture_dimensions_prefers_valid_settings() {
        assert_eq!(
            resolve_capture_dimensions(Some(800.0), Some(600.0), 1280, 720),
            (800, 600)
        );
    }

    #[test]
    fn resolve_capture_dimensions_keeps_partial_settings_pair_coherent() {
        assert_eq!(
            resolve_capture_dimensions(Some(1280.0), None, 0, 0),
            (640, 480)
        );
    }

    #[test]
    fn resolve_capture_dimensions_uses_safe_default_without_dimensions() {
        assert_eq!(resolve_capture_dimensions(None, None, 0, 0), (640, 480));
    }

    #[test]
    fn screen_ready_stall_threshold_tracks_live_tier_changes() {
        let top = 0u32;
        let out_of_range = u32::MAX;

        assert_eq!(
            screen_ready_stall_threshold_update(false, true, top, top),
            Some((800.0, 100.0))
        );
        assert_eq!(
            screen_ready_stall_threshold_update(true, true, top, top),
            None
        );
        assert_eq!(
            screen_ready_stall_threshold_update(true, false, top, top),
            None
        );
        assert_eq!(
            screen_ready_stall_threshold_update(false, true, out_of_range, out_of_range),
            Some((800.0, 100.0))
        );
    }

    #[test]
    fn screen_ready_stall_tracker_drives_production_transport_threshold() {
        use videocall_transport::webtransport::{
            raised_threshold_owner_count, ready_stall_threshold_ms,
            reset_ready_stall_threshold_on_construction, set_uplink_rtt_baseline_ms,
        };

        set_uplink_rtt_baseline_ms(None);
        reset_ready_stall_threshold_on_construction();
        let mut tracker = ScreenReadyStallThresholdTracker::new(0);

        assert_eq!(tracker.tick(true, 0), Some((800.0, 100.0)));
        assert_eq!(ready_stall_threshold_ms(), 800.0);
        assert_eq!(raised_threshold_owner_count(), 1);

        assert_eq!(tracker.tick(false, 0), None);
        assert_eq!(raised_threshold_owner_count(), 0);
        assert_eq!(ready_stall_threshold_ms(), 250.0);
    }
    use videocall_aq::constants::simulcast_layers;

    #[test]
    fn record_camera_restart_increments_each_reason_counter() {
        let before_closed = camera_encoder_restarts_closed_codec();
        let before_memory = camera_encoder_restarts_memory();
        let before_configure = camera_encoder_restarts_configure();
        let before_other = camera_encoder_restarts_other();

        record_camera_restart(RestartReason::ClosedCodec);
        record_camera_restart(RestartReason::Memory);
        record_camera_restart(RestartReason::Configure);
        record_camera_restart(RestartReason::Other);

        assert!(camera_encoder_restarts_closed_codec() > before_closed);
        assert!(camera_encoder_restarts_memory() > before_memory);
        assert!(camera_encoder_restarts_configure() > before_configure);
        assert!(camera_encoder_restarts_other() > before_other);
    }

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
    fn loop_is_not_superseded_when_only_switching_is_raised() {
        // Issue #1295 dark-square regression pin. On initial-join-with-camera-ON
        // a post-permission `devicechange` raises `switching` on the very loop
        // that should bind the capture stream to `<video id="webcam">`. The fixed
        // supersede predicate is epoch + enabled ONLY, so a raised `switching`
        // (with `enabled == true` and a matching epoch) must NOT mark the loop
        // superseded — otherwise it bails before `set_src_object` and the camera
        // stays dark until a manual OFF→ON. The pre-fix predicate OR-ed
        // `switching` in and returned `true` here, producing the dark square.
        //
        // `loop_is_superseded` takes NO `switching` argument precisely so the bug
        // cannot be reintroduced without changing this signature: re-adding a
        // `switching` term forces a call-site/signature change that breaks this
        // test. With `enabled == true` and equal epochs the loop is the live,
        // current generation and must keep going.
        assert!(
            !loop_is_superseded(true, 7, 7),
            "enabled with a matching epoch must NOT be superseded, even though a \
             switch was requested (the dark-square bug returned true here)"
        );

        // The real supersede conditions DO trip it, so the assertion above is not
        // vacuously "always false":
        assert!(
            loop_is_superseded(false, 7, 7),
            "disabled → superseded (the loop must tear down)"
        );
        assert!(
            loop_is_superseded(true, 8, 7),
            "a newer start() bumped the epoch past ours → superseded"
        );
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

    // --- simulcast layer-transition log formatting (issue #1106) ----------

    #[test]
    fn shed_reason_is_directional() {
        // Falling active count == a layer was shed under load; rising == restore.
        // Pins the comparison so flipping `<` to `>`/`<=` FAILS here.
        assert_eq!(shed_reason(3, 2), "shed-under-load");
        assert_eq!(shed_reason(1, 3), "restore");
        // Caller only emits on prev != cur; the equal case folds into "restore".
        assert_eq!(shed_reason(2, 2), "restore");
    }

    #[test]
    fn format_layer_transition_matches_canonical_shed_example() {
        // Canonical example from issue #1106 (a 3->2 shed). The resolutions are
        // sourced from the SAME ladder the encode loop builds layers from
        // (`simulcast_layers(3)` == [low, standard, hd]), so they can never drift
        // from a hand-hardcoded copy. The per-layer bitrates are RUNTIME values
        // (the AQ controller's live `local_bitrate`, here below the issue #1768
        // tier ideals 120/350) — they are an input to the helper, not a ladder
        // constant, so we pass them explicitly. The shed top layer (id 2) shows
        // no bitrate.
        let ladder = simulcast_layers(3);
        assert_eq!(
            ladder.len(),
            3,
            "canonical example assumes the 3-rung ladder"
        );
        let layers = [
            LayerView {
                id: 0,
                w: ladder[0].max_width,
                h: ladder[0].max_height,
                bitrate_bps: 100_000,
            },
            LayerView {
                id: 1,
                w: ladder[1].max_width,
                h: ladder[1].max_height,
                bitrate_bps: 300_000,
            },
            LayerView {
                id: 2,
                w: ladder[2].max_width,
                h: ladder[2].max_height,
                // SHED: bitrate is the zero shed marker and must NOT be rendered.
                bitrate_bps: 0,
            },
        ];

        // Build the expected string from the ladder constants the code uses, so
        // tuning the ladder updates the expectation in lockstep rather than
        // silently diverging from a literal.
        let expected = format!(
            "Simulcast layer change: active 3->2 (reason=shed-under-load) | \
             [0] {w0}x{h0} ~100kbps ACTIVE | \
             [1] {w1}x{h1} ~300kbps ACTIVE | \
             [2] {w2}x{h2} SHED",
            w0 = ladder[0].max_width,
            h0 = ladder[0].max_height,
            w1 = ladder[1].max_width,
            h1 = ladder[1].max_height,
            w2 = ladder[2].max_width,
            h2 = ladder[2].max_height,
        );
        // Spot-check the literal too: with today's ladder (issue #1768) this is
        // exactly the string the encode loop logs. (If the ladder changes, the
        // `expected` above is the authority; this literal documents the shape.)
        assert_eq!(
            expected,
            "Simulcast layer change: active 3->2 (reason=shed-under-load) | \
             [0] 320x180 ~100kbps ACTIVE | [1] 640x360 ~300kbps ACTIVE | [2] 1280x720 SHED"
        );

        assert_eq!(format_layer_transition(3, 2, &layers), expected);
    }

    #[test]
    fn format_layer_transition_active_shed_boundary_is_layer_id_lt_active() {
        // The ACTIVE/SHED split is `layer_id < active`. With active == 2, layer 1
        // is the last ACTIVE rung and layer 2 is the first SHED rung. This pins
        // the boundary at `id == active`: a mutation to `<=` would flip layer 2
        // to ACTIVE (and render a bitrate for it) and FAIL here.
        let layers = [
            LayerView {
                id: 0,
                w: 320,
                h: 180,
                bitrate_bps: 100_000,
            },
            LayerView {
                id: 1,
                w: 640,
                h: 360,
                bitrate_bps: 300_000,
            },
            LayerView {
                id: 2,
                w: 1280,
                h: 720,
                bitrate_bps: 0,
            },
        ];
        let s = format_layer_transition(3, 2, &layers);
        assert!(
            s.contains("[1] 640x360 ~300kbps ACTIVE"),
            "id 1 < 2 is ACTIVE"
        );
        assert!(s.contains("[2] 1280x720 SHED"), "id 2 == active is SHED");
        assert!(
            !s.contains("[2] 1280x720 ~"),
            "the shed layer must not render a bitrate"
        );
    }

    #[test]
    fn format_layer_transition_restore_direction() {
        // A rising active count (2 -> 3) is a restore: all three rungs ACTIVE,
        // reason=restore. Pins the directional reason through the full formatter.
        let layers = [
            LayerView {
                id: 0,
                w: 320,
                h: 180,
                bitrate_bps: 120_000,
            },
            LayerView {
                id: 1,
                w: 640,
                h: 360,
                bitrate_bps: 350_000,
            },
            LayerView {
                id: 2,
                w: 1280,
                h: 720,
                bitrate_bps: 1_500_000,
            },
        ];
        assert_eq!(
            format_layer_transition(2, 3, &layers),
            "Simulcast layer change: active 2->3 (reason=restore) | \
             [0] 320x180 ~120kbps ACTIVE | [1] 640x360 ~350kbps ACTIVE | \
             [2] 1280x720 ~1500kbps ACTIVE"
        );
    }

    // --- issue #1768: per-layer framerate-cap throttle -------------------

    #[test]
    fn should_encode_layer_frame_keyframe_and_single_stream_always_encode() {
        // A keyframe is NEVER dropped, even mid-interval, so every layer's GOP
        // stays coherent (drop a keyframe and receivers on that layer freeze).
        assert!(should_encode_layer_frame(1000.0, 995.0, 1000.0 / 7.0, true));
        // Single stream / no cap: min_interval 0.0 => always encode, even with
        // zero elapsed. This is the byte-identical single-stream path.
        assert!(should_encode_layer_frame(1000.0, 1000.0, 0.0, false));
        assert!(should_encode_layer_frame(0.0, 0.0, 0.0, false));
    }

    #[test]
    fn should_encode_layer_frame_seed_encodes_first_frame() {
        // NEG_INFINITY seed (the post-build/-restart value) makes the elapsed
        // term +∞, so the first frame after any (re)build always encodes.
        assert!(should_encode_layer_frame(
            1000.0,
            f64::NEG_INFINITY,
            1000.0 / 7.0,
            false
        ));
    }

    #[test]
    fn should_encode_layer_frame_drops_within_interval_encodes_after() {
        // 7 fps rung: interval 142.857ms; with the 15% slack the threshold is
        // ~121.4ms. A non-keyframe frame BELOW the threshold is DROPPED (real-
        // time over smoothness); at/above it the frame is ENCODED. The 130ms
        // case ALSO pins the slack: without it the bare interval (142.857ms)
        // would DROP 130ms too, so removing the slack fails this assertion.
        let interval = 1000.0 / 7.0;
        assert!(!should_encode_layer_frame(100.0, 0.0, interval, false)); // 100 < 121.4 -> drop
        assert!(should_encode_layer_frame(130.0, 0.0, interval, false)); // 130 >= 121.4 -> encode (needs slack)
        assert!(should_encode_layer_frame(300.0, 0.0, interval, false)); // well past -> encode
    }

    #[test]
    fn should_encode_layer_frame_15fps_faster_cadence_than_7fps() {
        // 15 fps rung (interval 66.7ms, threshold ~56.7ms) encodes roughly twice
        // as often as the 7 fps rung: 60ms elapsed ENCODES at 15fps but DROPS at
        // 7fps. Pins that each rung's fps is applied independently of the others.
        let fps15 = 1000.0 / 15.0;
        let fps7 = 1000.0 / 7.0;
        assert!(should_encode_layer_frame(60.0, 0.0, fps15, false));
        assert!(!should_encode_layer_frame(60.0, 0.0, fps7, false));
    }

    #[test]
    fn layer_ceiling_to_count_maps_sentinel_to_fail_open() {
        // u32::MAX (Auto / no cap) must map to usize::MAX (fail-open) on EVERY
        // target — not `u32::MAX as usize` (== 2^32-1 on 64-bit native), which
        // would be a finite cap of ~4 billion layers and (harmlessly, but
        // incorrectly) not the sentinel. This guards the explicit-check mapping.
        assert_eq!(layer_ceiling_to_count(u32::MAX), usize::MAX);
    }

    #[test]
    fn layer_ceiling_to_count_passes_through_real_counts() {
        // A real user selection (a layer count) widens losslessly.
        assert_eq!(layer_ceiling_to_count(1), 1);
        assert_eq!(layer_ceiling_to_count(2), 2);
        assert_eq!(layer_ceiling_to_count(3), 3);
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
    fn lazy_construction_builds_only_active_encoders_not_the_ceiling() {
        // Issue #1204: the number of per-layer VideoEncoders CONSTRUCTED is the
        // ACTIVE count (floored at 1, capped at the ceiling), NOT the ladder
        // ceiling. This is the single source of truth used by BOTH the cold-start
        // build loop and the in-loop lazy-build trigger, so testing it pins the
        // lazy boundary without a live VideoEncoder.

        // Cold start: active == 1 over a 3-rung ceiling builds ONLY the base
        // encoder. The upper two rungs' encoders are NOT constructed — this is
        // the core #1204 assertion (an un-earned layer has no encoder yet).
        assert_eq!(
            encoders_to_build(1, 3),
            1,
            "cold start must build only the base encoder, not the 3-rung ceiling"
        );
        // A mutation that built the ceiling at cold start (e.g. returning
        // `ceiling`) would make this 3 and FAIL.
        assert_ne!(
            encoders_to_build(1, 3),
            3,
            "un-earned upper rungs must NOT be constructed at cold start"
        );

        // After the ramp earns the 2nd rung, exactly 2 encoders exist; the 3rd is
        // still un-built until active rises to 3.
        assert_eq!(encoders_to_build(2, 3), 2);
        assert_eq!(encoders_to_build(3, 3), 3);

        // Floor: a degenerate active 0 still builds the base (base is always
        // present). Cap: active above the ceiling never builds more than the
        // ladder has rungs.
        assert_eq!(encoders_to_build(0, 3), 1, "base is always built");
        assert_eq!(encoders_to_build(99, 3), 3, "never build past the ceiling");

        // Single-stream ceiling (PR-A default): only ever the one base encoder.
        assert_eq!(encoders_to_build(1, 1), 1);
    }

    #[test]
    fn sustained_shed_teardown_decision_fires_only_past_dwell() {
        // Issue #1230: pins the SINGLE SOURCE OF TRUTH for teardown
        // (`should_teardown_shed_layer`). The encode loop frees a shed upper rung
        // iff this returns true, so pinning it here pins the whole behavior off-wasm
        // (the counter is bumped in the live loop and is not host-runnable).
        //
        // Mutations these assertions CATCH:
        //  * dropping the `None` guard (an un-shed rung would tear down) — first case
        //  * inverting the comparison (`>=`→`<`) — every Some case flips and FAILS
        //  * swapping `>=`→`>` — the exact-boundary case (29_999 retain, 30_000 tear
        //    down) flips and FAILS
        let dwell = SHED_TEARDOWN_DWELL_MS; // 30_000.0

        // Not currently shed (None) → never tear down, no matter how late the clock.
        assert!(
            !should_teardown_shed_layer(None, 100_000.0, dwell),
            "a rung that is not shed (None) must never be torn down"
        );
        // Dwelled 29.999s (< 30s) → RETAIN (boundary just under threshold).
        assert!(
            !should_teardown_shed_layer(Some(0.0), 29_999.0, dwell),
            "29.999s < 30s dwell must retain the rung"
        );
        // Exactly 30s → TEAR DOWN (pins the inclusive `>=`; a `>` mutation fails).
        assert!(
            should_teardown_shed_layer(Some(0.0), 30_000.0, dwell),
            "exactly 30s dwell must tear down (>= is inclusive)"
        );
        // 35s dwell (armed at t=10s, now 45s) → TEAR DOWN.
        assert!(
            should_teardown_shed_layer(Some(10_000.0), 45_000.0, dwell),
            "35s dwell must tear down"
        );
        // 10s dwell (armed at t=10s, now 20s) → RETAIN.
        assert!(
            !should_teardown_shed_layer(Some(10_000.0), 20_000.0, dwell),
            "10s dwell must retain"
        );

        // FREED-COUNT SEMANTICS: drive the REAL decision helper over a per-rung
        // shed-since array (as the encode loop does) and count the trues. This
        // pins "freed count == number of rungs whose dwell exceeded N" via the
        // actual decision path — NOT a literal-against-itself. now = 40_000ms:
        //   id0 base: never shed (None)            → retain
        //   id1: armed t=0   (40s dwell >= 30s)     → tear down
        //   id2: armed t=20s (20s dwell <  30s)     → retain
        let now_ms = 40_000.0;
        let shed_since: [Option<f64>; 3] = [None, Some(0.0), Some(20_000.0)];
        let freed = shed_since
            .iter()
            .filter(|s| should_teardown_shed_layer(**s, now_ms, dwell))
            .count();
        assert_eq!(
            freed, 1,
            "exactly the rungs whose dwell exceeded the threshold are freed"
        );
    }

    #[test]
    fn forced_keyframe_pli_coalesces_within_cooldown_window() {
        // Issue #1287 / #1347 item 2: drives the REAL per-frame keyframe decision the
        // camera encode loop calls (`keyframe_tick_decision` — the production loop
        // calls this exact fn, so a mutation to the real decision logic breaks this
        // test off-wasm; the live encode loop is not host-runnable).
        //
        // ACCEPTANCE (#1287): a frame stream where EVERY frame has a PLI pending
        // (worst case: N receivers hammering one publisher), replicating the encode
        // loop's state update by feeding the decision's returned
        // `last_keyframe_emit_ms` back in each tick. Assert the burst coalesces to
        // ~one forced keyframe per cooldown. Removing the cooldown gate from the
        // decision makes `forced` jump to 30 (one per frame), failing `== 4`; the
        // inclusive-`>=` boundary itself is pinned exactly by
        // `keyframe_tick_decision_coalesces_and_holds_pli` in `encoder_state`.
        // 30fps => a frame every ~33.33ms; no periodic GOP in this 1s slice, so all
        // emits are PLI-forced.
        let cd = FORCED_KEYFRAME_COOLDOWN_MS; // 250.0
        let frame_interval_ms = 1000.0 / 30.0;
        let mut last_keyframe_emit_ms: Option<f64> = None;
        let mut forced = 0u32;
        let mut now = 0.0_f64;
        for _ in 0..30 {
            // pli_pending is ALWAYS true in this saturated worst case; no reconnect.
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
            // Feed the decision's post-tick clock back, exactly like the loop.
            last_keyframe_emit_ms = decision.last_keyframe_emit_ms;
            now += frame_interval_ms;
        }
        // 1s of saturated PLI at a 250ms cooldown: the first frame at/after each window
        // fires, so emissions land at t≈0, 266.7, 533.3, 800ms => exactly 4 (vs up to
        // 30 = frame-rate with no coalescer).
        assert_eq!(
            forced, 4,
            "saturated PLI for 1s must coalesce to exactly 4 forced keyframes, got {forced}"
        );
    }

    #[test]
    fn keyframe_cooldown_reset_unblocks_first_post_reconnect_pli() {
        // Issue #1311: after a reconnect/re-election the camera encode loop keeps
        // running (it is NOT torn down — only the connection is rebuilt / the
        // re-election atomic flips), so `last_keyframe_emit_ms` carries a STALE
        // pre-transition timestamp. Without a reset, a recovery PLI on the first
        // post-transition frame would be coalesced away for up to
        // FORCED_KEYFRAME_COOLDOWN_MS. The fix arms a one-shot reset
        // (`keyframe_cooldown_reset`) that the encode loop `.swap(false)`-consumes
        // each frame and passes into the decision as `cooldown_reset`, which clears
        // the stale clock so the PLI emits immediately.
        //
        // This drives the REAL `keyframe_tick_decision` (the exact fn the camera
        // production loop calls). It is mutation-proof: the CONTROL arm pins that the
        // cooldown genuinely WOULD suppress (so the assert is not vacuous), and the
        // RESET arm fails if the `cooldown_reset` clear is removed from the decision.
        let cd = FORCED_KEYFRAME_COOLDOWN_MS; // 250.0

        // A keyframe was emitted just before the transition.
        let pre_reconnect_emit_ms = 1_000.0;
        // The first post-transition frame arrives only 33ms later — well INSIDE the
        // 250ms cooldown window, with a PLI pending (a receiver requesting recovery).
        let first_frame_after_ms = pre_reconnect_emit_ms + 33.0;

        // CONTROL: reset NOT armed. The stale timestamp must SUPPRESS the PLI — this
        // pins that the window is real, so the reset arm below is a true behavioral
        // difference, not an always-true assertion.
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
            "control: a PLI {}ms after the last keyframe must be coalesced when no \
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
            "after a reconnect/re-election reset, the first PLI must emit a forced \
             keyframe immediately even {}ms < cooldown ({}ms) since the last keyframe",
            first_frame_after_ms - pre_reconnect_emit_ms,
            cd
        );

        // One-shot: the reset is a per-frame edge (the loop already consumed it via
        // `.swap`), so a SUBSEQUENT early frame — still inside the cooldown of the
        // keyframe we just emitted, reset NOT re-armed — is coalesced again. The reset
        // does not stick and disable the coalescer.
        let next = keyframe_tick_decision(KeyframeTickInput {
            now_ms: first_frame_after_ms + 33.0,
            pli_pending: true,
            is_periodic: false,
            cooldown_reset: false,
            last_keyframe_emit_ms: reset.last_keyframe_emit_ms,
            cooldown_ms: cd,
            tier_change_pending: false,
        });
        assert!(
            !next.want_keyframe,
            "after the one-shot reset is consumed, the coalescer must resume \
             suppressing PLIs inside the cooldown window"
        );
    }

    #[test]
    fn camera_tier_change_requests_keyframe_on_next_frame_inside_cooldown() {
        let cd = FORCED_KEYFRAME_COOLDOWN_MS;
        let previous_keyframe_ms = 1_000.0;
        let next_frame_ms = previous_keyframe_ms + 33.0;
        let tier = &VIDEO_QUALITY_TIERS[5];
        let tier_max_width = AtomicU32::new(VIDEO_QUALITY_TIERS[0].max_width);
        let tier_max_height = AtomicU32::new(VIDEO_QUALITY_TIERS[0].max_height);
        let tier_keyframe_interval =
            AtomicU32::new(VIDEO_QUALITY_TIERS[0].keyframe_interval_frames);
        let shared_video_tier_index = AtomicU32::new(0);
        let tier_change_keyframe = AtomicBool::new(false);
        let keyframe_cooldown_reset = AtomicBool::new(false);

        let control = keyframe_tick_decision(KeyframeTickInput {
            now_ms: next_frame_ms,
            pli_pending: true,
            is_periodic: false,
            cooldown_reset: false,
            last_keyframe_emit_ms: Some(previous_keyframe_ms),
            cooldown_ms: cd,
            tier_change_pending: false,
        });
        assert!(
            !control.want_keyframe,
            "test premise: a plain PLI inside the cooldown would be held"
        );

        apply_camera_tier_change(
            CameraTierChangeTargets {
                tier_max_width: &tier_max_width,
                tier_max_height: &tier_max_height,
                tier_keyframe_interval: &tier_keyframe_interval,
                shared_video_tier_index: &shared_video_tier_index,
                tier_change_keyframe: &tier_change_keyframe,
                keyframe_cooldown_reset: &keyframe_cooldown_reset,
            },
            tier,
            5,
        );

        assert_eq!(tier_max_width.load(Ordering::Relaxed), tier.max_width);
        assert_eq!(tier_max_height.load(Ordering::Relaxed), tier.max_height);
        assert_eq!(
            tier_keyframe_interval.load(Ordering::Relaxed),
            tier.keyframe_interval_frames
        );
        assert_eq!(shared_video_tier_index.load(Ordering::Relaxed), 5);

        let decision = keyframe_tick_decision(KeyframeTickInput {
            now_ms: next_frame_ms,
            pli_pending: false,
            is_periodic: false,
            cooldown_reset: keyframe_cooldown_reset.swap(false, Ordering::AcqRel),
            last_keyframe_emit_ms: Some(previous_keyframe_ms),
            cooldown_ms: cd,
            tier_change_pending: tier_change_keyframe.load(Ordering::Acquire),
        });

        assert!(
            decision.want_keyframe,
            "a camera AQ tier change must force the next frame to be a keyframe"
        );
        assert!(
            decision.forced_cause.is_some(),
            "tier-change recovery must use the existing forced-keyframe path"
        );
        if decision.clear_force_keyframe {
            tier_change_keyframe.store(false, Ordering::Release);
        }
        assert!(!tier_change_keyframe.load(Ordering::Acquire));
        assert!(!keyframe_cooldown_reset.load(Ordering::Acquire));
    }

    /// A forced camera keyframe must be attributed to what actually requested it:
    /// a receiver's PLI, the publisher's tier change, or both. The three `label()`
    /// values ARE the text of the encode loop's forced-keyframe log line.
    #[test]
    fn forced_camera_keyframe_attributes_tier_change_apart_from_pli() {
        let cd = FORCED_KEYFRAME_COOLDOWN_MS;
        let now = 1_000.0;
        let tier_change_keyframe = AtomicBool::new(false);
        let keyframe_cooldown_reset = AtomicBool::new(false);

        // Arm ONLY the tier change through the production AQ path — no PLI pending.
        apply_camera_tier_change(
            CameraTierChangeTargets {
                tier_max_width: &AtomicU32::new(0),
                tier_max_height: &AtomicU32::new(0),
                tier_keyframe_interval: &AtomicU32::new(0),
                shared_video_tier_index: &AtomicU32::new(0),
                tier_change_keyframe: &tier_change_keyframe,
                keyframe_cooldown_reset: &keyframe_cooldown_reset,
            },
            &VIDEO_QUALITY_TIERS[5],
            5,
        );
        let tick = |pli_pending: bool, tier_change_pending: bool| {
            keyframe_tick_decision(KeyframeTickInput {
                now_ms: now,
                pli_pending,
                is_periodic: false,
                cooldown_reset: false,
                last_keyframe_emit_ms: None,
                cooldown_ms: cd,
                tier_change_pending,
            })
        };

        let tier_only = tick(false, tier_change_keyframe.load(Ordering::Acquire));
        assert_eq!(
            tier_only.forced_cause,
            Some(ForcedKeyframeCause::TierChange),
            "a tier change with no PLI pending must NOT be attributed to a PLI"
        );
        assert_eq!(ForcedKeyframeCause::TierChange.label(), "tier change");

        let pli_only = tick(true, false);
        assert_eq!(
            pli_only.forced_cause,
            Some(ForcedKeyframeCause::Pli),
            "a receiver PLI with no tier change stays attributed to the PLI"
        );
        assert_eq!(ForcedKeyframeCause::Pli.label(), "PLI");

        let both = tick(true, tier_change_keyframe.load(Ordering::Acquire));
        assert_eq!(
            both.forced_cause,
            Some(ForcedKeyframeCause::Both),
            "concurrent causes must report both, not silently drop one"
        );
        assert_eq!(ForcedKeyframeCause::Both.label(), "PLI + tier change");
    }

    #[test]
    fn initial_active_layer_count_is_one() {
        // Issue #1140 / #1141: every camera publisher cold-starts ACTIVE at the
        // base layer only (1), regardless of the device ceiling. This preserves
        // byte-identical ENCODE/EGRESS OUTPUT vs the legacy single-stream path at
        // startup; the runtime ramp earns more layers up to the ceiling.
        assert_eq!(initial_active_layer_count(), 1);
        // And it must be strictly below the full ladder so there is genuinely room
        // to ramp (this would fail if someone "fixed" the cold-start back to the
        // ceiling).
        assert!(
            initial_active_layer_count() < clamp_layer_count(SIMULCAST_MAX_SUPPORTED_LAYERS),
            "the cold-start active count must be below the full ladder so the ramp has room"
        );
    }

    #[test]
    fn single_layer_emits_layer_id_zero() {
        // The build loop assigns layer_id = layer_idx for idx in 0..n_layers.
        // For an explicit n_layers == 1 input the only id is 0. This pins that
        // invariant without needing a live camera/VideoEncoder.
        let n_layers = clamp_layer_count(1) as usize;
        let ids: Vec<u32> = (0..n_layers).map(|i| i as u32).collect();
        assert_eq!(ids, vec![0]);
    }

    // --- single-layer low-rung pin gate (issue #1136 + #1156 hysteresis) ----

    #[test]
    fn single_layer_low_pin_engages_only_above_three_other_peers() {
        // Single-stream (effective_layers == 1): the pin ENGAGES strictly ABOVE 3
        // other peers, regardless of prior state. Below the release threshold it
        // RELEASES regardless of prior state. These pin the comparison directions
        // and thresholds (a mutation of `>`→`>=`, the engage 3→2/4, or the release
        // direction FAILS the test). `currently_pinned` is varied to prove the
        // engage/release decisions are NOT prior-state-dependent.
        for prior in [false, true] {
            assert!(
                !should_pin_single_layer_low(1, 0, prior),
                "no peers → release (solo call), prior={prior}"
            );
            assert!(
                !should_pin_single_layer_low(1, 2, prior),
                "2 other peers (< release) → release, prior={prior}"
            );
            assert!(
                should_pin_single_layer_low(1, 4, prior),
                "4 other peers (> engage) → pin, prior={prior}"
            );
            assert!(
                should_pin_single_layer_low(1, 50, prior),
                "large call → pin, prior={prior}"
            );
        }
    }

    #[test]
    fn single_layer_low_pin_holds_state_in_dead_band() {
        // #1156 hysteresis: at EXACTLY the boundary (3 others) the pin HOLDS its
        // prior state — it neither engages nor releases. This is the property that
        // stops 3 ↔ 4 oscillation from flipping the pin every tick. A mutation
        // that drops the dead-band (e.g. returns a fixed bool at 3, or compares
        // `>=`/`<=` so 3 forces a decision) FAILS one of these.
        assert!(
            !should_pin_single_layer_low(1, 3, false),
            "at the boundary while UNPINNED → stay unpinned"
        );
        assert!(
            should_pin_single_layer_low(1, 3, true),
            "at the boundary while PINNED → stay pinned"
        );
    }

    #[test]
    fn single_layer_low_pin_does_not_flip_on_boundary_oscillation() {
        // #1156 ACCEPTANCE: simulate the participant count oscillating 3 ↔ 4 each
        // AQ tick and assert the pin does NOT flip every tick. Once engaged at 4,
        // dropping back to 3 must HOLD the pin (dead-band), so across the whole
        // oscillation the pin flips at most ONCE (the initial engage) — not once
        // per tick. Without the hysteresis (release == engage band) the value
        // would toggle true/false/true/false and `flips` would equal the tick
        // count. This is the regression guard for the per-second keyframe storm.
        let mut pinned = false;
        let mut flips = 0usize;
        // Start at 3 (unpinned), then alternate 4,3,4,3,… for many ticks.
        let counts = std::iter::once(3usize).chain((0..20).map(|i| if i % 2 == 0 { 4 } else { 3 }));
        for c in counts {
            let next = should_pin_single_layer_low(1, c, pinned);
            if next != pinned {
                flips += 1;
            }
            pinned = next;
        }
        assert_eq!(
            flips, 1,
            "across a sustained 3↔4 oscillation the pin must flip exactly once \
             (the initial engage at 4), then hold — got {flips} flips"
        );
        assert!(
            pinned,
            "the pin must be ENGAGED after the oscillation (last high was 4)"
        );
    }

    #[test]
    fn single_layer_low_pin_never_engages_in_simulcast_mode() {
        // effective_layers > 1: the receiver-driven layer chooser already sheds
        // cost, so the pin must NEVER engage regardless of peer count OR prior
        // state. A mutation dropping the `effective_layers == 1` guard FAILS here.
        for layers in [2u32, 3] {
            for peers in [0usize, 3, 4, 100] {
                for prior in [false, true] {
                    assert!(
                        !should_pin_single_layer_low(layers, peers, prior),
                        "simulcast ({layers} layers) must never pin (peers={peers}, prior={prior})"
                    );
                }
            }
        }
    }

    #[test]
    fn single_layer_low_pin_threshold_is_three_others() {
        // The threshold values themselves: ">3 others" engage == "publisher + 4+
        // others" == a 5+-participant call. Pin both constants so the documented
        // semantics and the gate stay in lockstep. The band is one-peer-wide
        // (engage == release == 3), so 3 is the sole dead-band count.
        assert_eq!(SINGLE_LAYER_LOW_PIN_ENGAGE_THRESHOLD, 3);
        assert_eq!(SINGLE_LAYER_LOW_PIN_RELEASE_THRESHOLD, 3);
        // Lockstep with the gate at the band edges. Use prior=false so the
        // boundary count (3) does not pin and engage(+1) does.
        assert!(!should_pin_single_layer_low(
            1,
            SINGLE_LAYER_LOW_PIN_ENGAGE_THRESHOLD,
            false
        ));
        assert!(should_pin_single_layer_low(
            1,
            SINGLE_LAYER_LOW_PIN_ENGAGE_THRESHOLD + 1,
            false
        ));
    }

    /// Issue #1172: a borrow-fail tick (`peer_count() == None`) must PRESERVE the
    /// prior pin state — it is NOT 0 peers. A genuine `Some(0)` reading still
    /// releases. This test fails if the tick maps `None` onto 0 peers (the bug)
    /// because `next_single_layer_pin(1, None, true)` would then release to
    /// `false`.
    #[test]
    fn single_layer_pin_holds_on_borrow_fail_releases_on_real_zero() {
        // Borrow-fail while PINNED: hold the pin (do not release on a spurious 0).
        assert!(
            next_single_layer_pin(1, None, true),
            "None (borrow-fail) must hold a prior ENGAGED pin, not release it"
        );
        // Borrow-fail while UNPINNED: hold unpinned (do not spuriously engage).
        assert!(
            !next_single_layer_pin(1, None, false),
            "None (borrow-fail) must hold a prior RELEASED pin"
        );

        // A GENUINE 0-peer reading still releases an engaged pin (0 < release
        // threshold). This is the behavior the borrow-fail case must NOT mimic.
        assert!(
            !next_single_layer_pin(1, Some(0), true),
            "a real 0-peer reading must RELEASE the pin"
        );
        // A genuine high count still engages from unpinned.
        assert!(
            next_single_layer_pin(1, Some(SINGLE_LAYER_LOW_PIN_ENGAGE_THRESHOLD + 1), false),
            "a real >threshold reading must ENGAGE the pin"
        );
        // A genuine simulcast reading (>1 layer) always releases regardless of
        // prior — the pin only applies in single-stream mode.
        assert!(
            !next_single_layer_pin(2, Some(50), true),
            "simulcast publisher: a real reading releases the pin"
        );
    }

    // -----------------------------------------------------------------------
    // WebTransport backpressure wiring (#509 parity audit, item #2).
    //
    // These pin that the camera AQ loop consults the WT *drop* and *saturation*
    // counters through the WT constants — the client-side congestion signal WT
    // would otherwise lack (WS-only, per PR #339). The wasm loop itself depends
    // on `js_sys::Date::now()` and cannot run on host, so the per-axis decision
    // (counter → `evaluate_self_congestion` → WT window/threshold) is extracted
    // into `wt_drop_step_down_decision` / `wt_saturation_step_down_decision`,
    // which the loop calls with the live transport counters. A mutation that
    // pointed an axis at the WRONG constants (e.g. the WS or the sibling WT
    // axis) shifts the firing boundary and is caught below; the transport-side
    // increment of those counters is pinned separately by the
    // `record_unistream_drop` / `record_ready_stall` tests in
    // `videocall-transport`.
    // -----------------------------------------------------------------------

    /// A sustained WT unistream-drop cluster at/above the WT-drop threshold,
    /// observed over a fully-elapsed WT-drop window, MUST request a step-down.
    #[test]
    fn camera_wt_drop_axis_fires_on_sustained_drops() {
        let current = WT_SELF_CONGESTION_DROP_THRESHOLD; // exactly at threshold
        let decision = wt_drop_step_down_decision(
            current,
            0, // snapshot
            WT_SELF_CONGESTION_WINDOW_MS,
        );
        assert!(
            decision.step_down,
            "a WT-drop delta == WT threshold over a closed WT window must step down"
        );
    }

    /// A drop count BELOW the WT-drop threshold must NOT fire — a transient
    /// stream reset on a lossy link cannot shed a layer.
    #[test]
    fn camera_wt_drop_axis_does_not_fire_below_threshold() {
        let current = WT_SELF_CONGESTION_DROP_THRESHOLD - 1;
        let decision = wt_drop_step_down_decision(current, 0, WT_SELF_CONGESTION_WINDOW_MS);
        assert!(
            !decision.step_down,
            "a WT-drop delta below the WT threshold must NOT step down"
        );
    }

    /// The drop axis must use the WT-drop constants, NOT the WS constants. This
    /// is the anti-misweave pin: at an elapsed past the (narrower) WS window but
    /// before the WT window closes, the WT axis must still treat the window as
    /// OPEN. If the helper were mutated to the WS window the same elapsed would
    /// CLOSE the window and roll/fire. The test is meaningful only because the
    /// two windows genuinely differ — that premise is pinned at COMPILE TIME by
    /// `WT_WINDOW_WIDER_THAN_WS` below (a const assert, so it cannot be a
    /// runtime `assert!` on constants — clippy `assertions_on_constants`).
    #[test]
    fn camera_wt_drop_axis_uses_wt_constants_not_ws() {
        // Compile-time premise: the WT drop window is strictly wider than the
        // WS window, so an elapsed between them is "open" under WT but "closed"
        // under WS. If a future tuning made them equal, this fails the BUILD,
        // flagging that the anti-misweave test below no longer discriminates.
        const _: () = assert!(
            WT_SELF_CONGESTION_WINDOW_MS > WS_SELF_CONGESTION_WINDOW_MS,
            "test premise: WT drop window must be wider than WS window"
        );
        // Elapsed sits past the WS window but before the WT window closes.
        let elapsed = WS_SELF_CONGESTION_WINDOW_MS + 1.0;
        // A delta at/above BOTH thresholds, evaluated over a still-OPEN WT
        // window, must not fire or roll.
        let delta = WS_SELF_CONGESTION_DROP_THRESHOLD.max(WT_SELF_CONGESTION_DROP_THRESHOLD);
        let decision = wt_drop_step_down_decision(delta, 0, elapsed);
        assert!(
            !decision.step_down,
            "the WT-drop axis must treat the WT window as still open at WS-window elapsed \
             (proves WT constants, not WS, are used)"
        );
        assert!(
            !decision.roll_window,
            "an open WT window must not roll (would roll early under WS window)"
        );
    }

    /// A sustained slow-`ready()` cluster at/above the WT-saturation threshold
    /// over a fully-elapsed saturation window MUST request a step-down — the
    /// slow-but-alive uplink case the drop axis structurally cannot see.
    #[test]
    fn camera_wt_saturation_axis_fires_on_sustained_stalls() {
        let current = WT_SATURATION_STALL_THRESHOLD;
        let decision = wt_saturation_step_down_decision(current, 0, WT_SATURATION_WINDOW_MS);
        assert!(
            decision.step_down,
            "a saturation delta == saturation threshold over a closed window must step down"
        );
    }

    /// A flat saturation counter (a WS user, or a healthy WT uplink that never
    /// crosses the producer-side ready-stall threshold) must NEVER fire.
    #[test]
    fn camera_wt_saturation_axis_never_fires_when_flat() {
        let decision = wt_saturation_step_down_decision(0, 0, WT_SATURATION_WINDOW_MS);
        assert!(
            !decision.step_down,
            "a flat-at-0 saturation counter must never step down (WS users / healthy WT)"
        );
    }

    #[test]
    fn camera_wt_stale_drop_axis_fires_on_sustained_drops() {
        let decision = camera_wt_stale_drop_step_down_decision(
            CAMERA_WT_STALE_DROP_THRESHOLD,
            0,
            CAMERA_WT_STALE_DROP_WINDOW_MS,
        );
        assert!(
            decision.step_down,
            "a stale-delta drop delta == threshold over a closed window must step down"
        );
    }

    #[test]
    fn camera_wt_stale_drop_axis_does_not_fire_below_threshold() {
        let decision = camera_wt_stale_drop_step_down_decision(
            CAMERA_WT_STALE_DROP_THRESHOLD - 1,
            0,
            CAMERA_WT_STALE_DROP_WINDOW_MS,
        );
        assert!(
            !decision.step_down,
            "a stale-delta drop delta below threshold must NOT step down"
        );
    }

    #[test]
    fn camera_wt_stale_drop_axis_never_fires_when_flat() {
        let decision =
            camera_wt_stale_drop_step_down_decision(0, 0, CAMERA_WT_STALE_DROP_WINDOW_MS);
        assert!(
            !decision.step_down,
            "a flat-at-0 stale-delta drop counter must never step down"
        );
    }

    #[test]
    fn camera_wt_stale_drop_axis_uses_its_own_window_not_ws() {
        const _: () = assert!(
            CAMERA_WT_STALE_DROP_WINDOW_MS > WS_SELF_CONGESTION_WINDOW_MS,
            "test premise: camera stale-drop window must be wider than WS window"
        );
        let elapsed = WS_SELF_CONGESTION_WINDOW_MS + 1.0;
        let delta = WS_SELF_CONGESTION_DROP_THRESHOLD.max(CAMERA_WT_STALE_DROP_THRESHOLD);
        let decision = camera_wt_stale_drop_step_down_decision(delta, 0, elapsed);
        assert!(
            !decision.step_down,
            "the camera stale-drop axis must treat its own window as still open at WS-window elapsed"
        );
        assert!(
            !decision.roll_window,
            "an open camera stale-drop window must not roll"
        );
    }

    /// Issue #1510: the wall-clock periodic keyframe ceiling fires when elapsed
    /// time since the last keyframe exceeds the cap, even when the frame-count
    /// modulo has not triggered. Calls the PRODUCTION `periodic_keyframe_due`
    /// function (the same one the camera/screen encode loops call).
    ///
    /// Mutation guards:
    /// - Removing the `wallclock_periodic` disjunction from `periodic_keyframe_due`
    ///   prevents the wall-clock trigger → `periodic_at[1]` would be 50 → FAILS.
    /// - Setting `PERIODIC_KEYFRAME_MAX_INTERVAL_MS` too high makes the wall-clock
    ///   check never fire within the simulated window → FAILS.
    #[test]
    fn wallclock_periodic_keyframe_fires_at_low_fps() {
        use crate::adaptive_quality_constants::PERIODIC_KEYFRAME_MAX_INTERVAL_MS;

        let keyframe_interval_frames: u32 = 50; // "minimal" tier
        let actual_fps = 7.0_f64; // CPU-bound, well below nominal 10fps
        let frame_interval_ms = 1000.0 / actual_fps;
        let cd = FORCED_KEYFRAME_COOLDOWN_MS;

        let mut last_keyframe_emit_ms: Option<f64> = None;
        let mut now = 0.0_f64;
        let mut periodic_at: Vec<u32> = Vec::new();

        for frame in 0u32..60 {
            let is_periodic = periodic_keyframe_due(
                frame,
                keyframe_interval_frames,
                now,
                last_keyframe_emit_ms,
                PERIODIC_KEYFRAME_MAX_INTERVAL_MS,
            );

            let decision = keyframe_tick_decision(KeyframeTickInput {
                now_ms: now,
                pli_pending: false,
                is_periodic,
                cooldown_reset: false,
                last_keyframe_emit_ms,
                cooldown_ms: cd,
                tier_change_pending: false,
            });

            if decision.want_keyframe {
                periodic_at.push(frame);
            }
            last_keyframe_emit_ms = decision.last_keyframe_emit_ms;
            now += frame_interval_ms;
        }

        assert_eq!(periodic_at[0], 0, "frame 0 must emit a periodic keyframe");
        assert!(
            periodic_at.len() >= 2,
            "the wall-clock ceiling must trigger at least one extra periodic keyframe \
             before the frame-counted boundary at frame 50"
        );
        let wallclock_fire = periodic_at[1];
        assert!(
            wallclock_fire < 50,
            "wall-clock periodic must fire before the frame-counted boundary (frame 50), \
             but fired at frame {wallclock_fire}"
        );
        let fire_time_ms = wallclock_fire as f64 * frame_interval_ms;
        assert!(
            fire_time_ms >= PERIODIC_KEYFRAME_MAX_INTERVAL_MS
                && fire_time_ms < PERIODIC_KEYFRAME_MAX_INTERVAL_MS + frame_interval_ms * 2.0,
            "wall-clock periodic should fire within two frames of {PERIODIC_KEYFRAME_MAX_INTERVAL_MS}ms, \
             but fired at {fire_time_ms:.1}ms (frame {wallclock_fire})"
        );
    }

    /// Issue #1510 / Blocker 2: after a reconnect clears `last_keyframe_emit_ms`
    /// to `None` mid-session (frame_counter > 0), the wall-clock ceiling must
    /// still emit a keyframe immediately — not wait for the next frame-count modulo.
    ///
    /// Mutation guard: removing the `None => frame_counter > 0` arm from
    /// `periodic_keyframe_due` makes it return `false` here → FAILS.
    #[test]
    fn wallclock_periodic_fires_immediately_after_reconnect_clears_clock() {
        use crate::adaptive_quality_constants::PERIODIC_KEYFRAME_MAX_INTERVAL_MS;

        let keyframe_interval_frames: u32 = 150; // full_hd tier
                                                 // Mid-session: frame counter is well past 0 but nowhere near the next modulo.
        let frame_counter: u32 = 73;
        let now_ms = 10_000.0; // 10s into the session

        // Reconnect cleared last_keyframe_emit_ms to None.
        let result = periodic_keyframe_due(
            frame_counter,
            keyframe_interval_frames,
            now_ms,
            None, // reconnect cleared this
            PERIODIC_KEYFRAME_MAX_INTERVAL_MS,
        );
        assert!(
            result,
            "after a reconnect clears the keyframe clock (None) mid-session \
             (frame {frame_counter} > 0), periodic_keyframe_due must return true \
             to re-arm the wall-clock ceiling immediately"
        );

        // Verify frame 0 also fires (first-ever frame, not a reconnect).
        let first_frame = periodic_keyframe_due(
            0,
            keyframe_interval_frames,
            0.0,
            None,
            PERIODIC_KEYFRAME_MAX_INTERVAL_MS,
        );
        assert!(
            first_frame,
            "frame 0 must fire via the frame-count modulo (0 % 150 == 0)"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Issue #1678: the camera's per-tick `video_at_floor` flag must force-clear
    // on the camera-ENABLE rising edge so a STALE `true` left from a
    // disabled-while-at-floor period cannot open the mic audio-after-video
    // backstop on the SAME 1 Hz tick the camera returns. `video_at_floor_on_tick`
    // is the SINGLE source of truth for the stored value (the loop has exactly
    // one writer). All other transitions pass the live detector value through.
    // ─────────────────────────────────────────────────────────────────────
    /// **THE OFF→ON STRANDING GUARD** (issue #2170, found in review of #2194).
    ///
    /// `stop()` clears the published geometry, but a camera OFF→ON pair does not
    /// always create a new encoder generation, so nothing would republish and a LIVE
    /// camera would read `—` for the rest of the session. The chain, each link
    /// verified in code: `host.rs` camera-ON calls `select()` BEFORE
    /// `set_enabled(true)`; `select()` raises `switching` only when already enabled,
    /// so it stays false; `start()`'s #1295 duplicate guard then matches
    /// `running && !switch_requested && same_device` and returns early; no new
    /// generation means no `'restart`-head clear and no `build_layer` publish; and all
    /// three reconfigure sites are gated on a change a CLEAR does not move.
    ///
    /// **THE FIRST FIX FOR THIS WAS WRONG and this test is why the shape changed.**
    /// It compared an `enabled` rising edge per frame, which cannot see the very
    /// scenario above: the loop is parked in `video_reader.read().await` across the
    /// OFF→ON pair, so it never samples the disabled state and on resume reads
    /// `(true, true)` — no edge, no republish, still stranded. Comparing a clear
    /// GENERATION is edge-independent: `stop()` records its clear whether or not any
    /// frame boundary observed it. The unobserved-transition case below is the one the
    /// edge version failed.
    ///
    /// MUTATION (run): drop the `&& enabled` and the disabled cases fail (that would
    /// undo `stop()`'s sentinel). Change `!=` to `>` and it still passes here but
    /// breaks on `u64` wrap — `!=` is deliberate. Compare against a constant instead
    /// of `seen` and the already-reconciled case fails, which is the every-frame
    /// republish this must not become.
    #[test]
    fn republish_geometry_after_an_unreconciled_clear_only_while_enabled() {
        // THE CASE THE EDGE VERSION MISSED: a clear the loop never observed (it was
        // parked in `read().await` across the whole OFF→ON pair). `seen` still trails
        // `live`, so the republish fires on resume even though `enabled` never
        // appeared to change from this loop's point of view.
        assert!(republish_geometry_on_tick(0, 1, true));
        // Several unobserved clears collapse into one republish — the loop only needs
        // to reach the current state, not replay each clear.
        assert!(republish_geometry_on_tick(3, 7, true));

        // Already reconciled: must NOT republish. This is the steady state at 30-50 Hz
        // per layer, and republishing here would defeat the change-gating the whole
        // channel depends on.
        assert!(!republish_geometry_on_tick(7, 7, true));

        // Clear seen while STOPPED: must NOT republish — that would write live-looking
        // geometry back over the sentinel `stop()` just wrote, the exact contract
        // `loop_may_publish_geometry` protects. The caller advances its seen-generation
        // only after a republish, so this is repaired on the next enabled frame rather
        // than lost.
        assert!(!republish_geometry_on_tick(0, 1, false));
        assert!(!republish_geometry_on_tick(7, 7, false));
    }

    #[test]
    fn video_at_floor_on_tick_clears_on_camera_enable_rising_edge() {
        // RISING edge (disabled -> enabled): force-clear even if the detector
        // still reads at-floor. THE FIX — without it a stale `true` from the
        // disabled period would open the mic backstop on the re-enable tick.
        assert!(
            !video_at_floor_on_tick(false, true, true),
            "camera-enable rising edge must force-clear video_at_floor (#1678)",
        );
        // Steady ENABLED: detector value passes through (true stays true).
        assert!(
            video_at_floor_on_tick(true, true, true),
            "steady enabled must pass the detector's at-floor through",
        );
        // Steady ENABLED: detector value passes through (false stays false).
        assert!(
            !video_at_floor_on_tick(true, true, false),
            "steady enabled must pass the detector's not-at-floor through",
        );
        // Steady DISABLED (not an edge): pass through — the disabled state is not
        // a rising edge and the detector value flows unchanged.
        assert!(
            video_at_floor_on_tick(false, false, true),
            "steady disabled is not a rising edge — detector value passes through",
        );
        // FALLING edge (enabled -> disabled): pass through. Only the RISING edge
        // clears; a falling edge does not (the detector continues to drive).
        assert!(
            video_at_floor_on_tick(true, false, true),
            "falling edge must NOT clear — only the rising edge force-clears",
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Issue #1678 (pre-submit follow-up): the SYNCHRONOUS clear on `set_enabled`.
    // `video_at_floor_on_tick` only clears on the next ~1 Hz camera AQ tick; the
    // mic backstop detector runs on its OWN loop and can read the flag in the
    // window before that tick. `clear_video_at_floor_on_enable_edge` performs the
    // store at the same synchronous point the `enabled` atom flips. This test
    // exercises the actual atomic store (not just a predicate): mutating the fn
    // to drop the `store(false)` (or to fire on the wrong edge) turns it red.
    // ─────────────────────────────────────────────────────────────────────
    #[test]
    fn clear_video_at_floor_on_enable_edge_clears_only_on_real_rising_edge() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        // Pre-seed the flag `true` (a prior at-floor distress episode while the
        // camera was disabled).
        let flag = Arc::new(AtomicBool::new(true));

        // Disable -> enable rising edge (changed=true, now_enabled=true): MUST
        // clear synchronously. THE FIX — without the store the mic detector could
        // read the stale `true` before the next camera AQ tick.
        clear_video_at_floor_on_enable_edge(&flag, true, true);
        assert!(
            !flag.load(Ordering::Acquire),
            "real enable rising edge must clear video_at_floor synchronously (#1678)",
        );

        // A no-op set_enabled(true) (already enabled, changed=false): must NOT
        // touch the flag — only a genuine transition clears.
        flag.store(true, Ordering::Release);
        clear_video_at_floor_on_enable_edge(&flag, false, true);
        assert!(
            flag.load(Ordering::Acquire),
            "no-op set_enabled(true) (changed=false) must NOT clear the flag",
        );

        // A disable (now_enabled=false): must NOT clear — the falling edge leaves
        // the flag to the live detector / steady state.
        flag.store(true, Ordering::Release);
        clear_video_at_floor_on_enable_edge(&flag, true, false);
        assert!(
            flag.load(Ordering::Acquire),
            "set_enabled(false) must NOT clear video_at_floor (only the enable edge does)",
        );
    }

    #[test]
    fn same_tick_tier_and_bitrate_change_folds_the_new_bitrate_into_the_fresh_config_2476() {
        assert_eq!(
            plan_single_stream_reconfigure(true, 900_000, true),
            SingleStreamReconfigure::Dims {
                bitrate_bps: 900_000
            },
            "a same-tick tier+bitrate change must rebuild at the NEW bitrate (#2476)",
        );
    }

    #[test]
    fn single_stream_reconfigure_plan_covers_the_dims_only_bitrate_only_and_idle_ticks_2476() {
        assert_eq!(
            plan_single_stream_reconfigure(true, 2_000_000, false),
            SingleStreamReconfigure::Dims {
                bitrate_bps: 2_000_000
            },
            "dims-only tick rebuilds at the unchanged bitrate",
        );
        assert_eq!(
            plan_single_stream_reconfigure(false, 900_000, true),
            SingleStreamReconfigure::Bitrate {
                bitrate_bps: 900_000
            },
            "bitrate-only tick reconfigures the stored config in place",
        );
        assert_eq!(
            plan_single_stream_reconfigure(false, 2_000_000, false),
            SingleStreamReconfigure::Nothing,
            "an idle tick must not configure() at all",
        );
    }

    #[test]
    fn the_applied_and_the_latched_rate_are_the_only_targets_withheld_2550() {
        assert!(
            !bitrate_attempt_allowed(900_000, 2_000_000, Some(900_000)),
            "a rejected bitrate must not be re-attempted every captured frame (#2550)",
        );
        assert!(
            bitrate_attempt_allowed(900_001, 2_000_000, Some(900_000)),
            "the latch withholds ONE value, not a band around it",
        );
        assert!(
            bitrate_attempt_allowed(899_999, 2_000_000, Some(900_000)),
            "the latch withholds ONE value, not a band around it",
        );
        assert!(
            bitrate_attempt_allowed(900_000, 2_000_000, Some(850_000)),
            "a latch on some OTHER value must not withhold this target",
        );
        assert!(
            !bitrate_attempt_allowed(2_000_000, 2_000_000, None),
            "a target already applied needs no configure()",
        );
    }

    #[test]
    fn a_withheld_bitrate_still_rides_a_dims_change_2550() {
        assert_eq!(
            plan_single_stream_reconfigure(true, 900_000, false),
            SingleStreamReconfigure::Dims {
                bitrate_bps: 900_000
            },
            "a dims change rebuilds the config, so the latched bitrate rides it (#2550)",
        );
        assert_eq!(
            plan_single_stream_reconfigure(false, 900_000, false),
            SingleStreamReconfigure::Nothing,
            "without a dims change a withheld bitrate must not configure() (#2550)",
        );
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
        // Stamp a 4:3 source (640x480) so the new source-dims field (issue
        // #1196) is exercised end-to-end through the real production function.
        let wrapper = super::transform_video_chunk(
            make_chunk(),
            0,
            buf.as_mut_slice(),
            "alice",
            aes,
            640,
            480,
            0,
        );
        // Layer 0 round-trips to 0 and (proto3 tag-5-when-nonzero) is wire-absent.
        // (The source dims live in the AES-encrypted inner MediaPacket, so they
        // cannot be asserted from the outer wrapper here; the host-runnable
        // `transform::tests::video_metadata_carries_source_dims` covers the
        // VideoMetadata stamping on the unencrypted path.)
        let bytes = wrapper.write_to_bytes().unwrap();
        let parsed = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        assert_eq!(parsed.simulcast_layer_id, 0);
    }

    #[wasm_bindgen_test]
    fn transform_video_chunk_layer_two_round_trips() {
        let aes = Rc::new(Aes128State::new(false));
        let mut buf = vec![0u8; 100_000];
        let wrapper = super::transform_video_chunk(
            make_chunk(),
            0,
            buf.as_mut_slice(),
            "alice",
            aes,
            640,
            480,
            2,
        );
        let bytes = wrapper.write_to_bytes().unwrap();
        let parsed = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        assert_eq!(parsed.simulcast_layer_id, 2);
    }

    /// A `LayerEncoder` around a real WebCodecs `VideoEncoder`, cold-start shaped.
    fn built_layer(width: u32, height: u32, bitrate_bps: u32) -> LayerEncoder {
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
        encoder.configure(&config).expect("cold-start configure");
        LayerEncoder {
            encoder,
            config,
            seq_out: Rc::new(std::cell::Cell::new(0)),
            layer_id: 0,
            current_w: width,
            current_h: height,
            tier_w: width,
            tier_h: height,
            local_bitrate: bitrate_bps,
            last_failed_bitrate: None,
            min_encode_interval_ms: 0.0,
            last_encode_ms: f64::NEG_INFINITY,
            _output_closure: output_closure,
            _error_closure: error_closure,
        }
    }

    /// Re-fit through the production dims chokepoint, as the encode loop does.
    fn fit_to(layer: &mut LayerEncoder, width: u32, height: u32) {
        let dims = std::cell::RefCell::new(Vec::new());
        let enabled = std::sync::atomic::AtomicBool::new(true);
        let epoch = std::sync::atomic::AtomicU64::new(0);
        layer.set_encode_dims(width, height, &dims, &enabled, &epoch, 0);
    }

    fn set_stored_config_width(layer: &LayerEncoder, width: f64) {
        assert!(
            Reflect::set(
                &layer.config,
                &JsValue::from_str("width"),
                &JsValue::from_f64(width),
            )
            .expect("set width on the stored config"),
            "Reflect::set refused the write, so the fixture proves nothing",
        );
    }

    fn config_key(layer: &LayerEncoder, key: &str) -> Option<f64> {
        Reflect::get(&layer.config, &JsValue::from_str(key))
            .ok()
            .and_then(|v| v.as_f64())
    }

    fn stored_config_dims(layer: &LayerEncoder) -> (Option<f64>, Option<f64>) {
        (config_key(layer, "width"), config_key(layer, "height"))
    }

    #[wasm_bindgen_test]
    fn tier_dim_change_survives_the_next_bitrate_only_reconfigure() {
        let mut layer = built_layer(1280, 720, 2_000_000);
        assert_eq!(stored_config_dims(&layer), (Some(1280.0), Some(720.0)));

        fit_to(&mut layer, 640, 360);
        layer
            .reconfigure_at_current_dims()
            .expect("tier-change reconfigure");
        assert_eq!(
            stored_config_dims(&layer),
            (Some(640.0), Some(360.0)),
            "a dimension reconfigure must store its config back (#2176)",
        );

        layer
            .reconfigure_at_bitrate(800_000)
            .expect("bitrate-only reconfigure");
        assert_eq!(
            stored_config_dims(&layer),
            (Some(640.0), Some(360.0)),
            "bitrate-only reconfigure resurrected the pre-step-down geometry (#2176)",
        );
        assert_eq!(layer.local_bitrate, 800_000);
    }

    #[wasm_bindgen_test]
    fn same_tick_tier_and_bitrate_change_stores_both_in_the_config_2476() {
        let mut layer = built_layer(1280, 720, 2_000_000);
        assert_eq!(config_key(&layer, "bitrate"), Some(2_000_000.0));

        fit_to(&mut layer, 640, 360);
        layer
            .reconfigure_at_current_dims_and_bitrate(900_000)
            .expect("same-tick tier+bitrate reconfigure");

        assert_eq!(stored_config_dims(&layer), (Some(640.0), Some(360.0)));
        assert_eq!(
            config_key(&layer, "bitrate"),
            Some(900_000.0),
            "the fresh config was built from the OLD cached bitrate (#2476)",
        );
        assert_eq!(layer.local_bitrate, 900_000);
    }

    #[wasm_bindgen_test]
    fn dim_change_reapplies_the_rung_framerate_hint_only_when_capped() {
        let mut layer = built_layer(1280, 720, 2_000_000);
        layer.min_encode_interval_ms = 1000.0 / 20.0;
        fit_to(&mut layer, 640, 360);
        layer
            .reconfigure_at_current_dims()
            .expect("capped dim reconfigure");
        assert_eq!(
            config_key(&layer, "framerate"),
            Some(20.0),
            "a fresh config drops the rung fps hint; it must be re-applied (#1768)",
        );

        // Single-stream: no fps cap at construction, none after a rebuild.
        let mut uncapped = built_layer(1280, 720, 2_000_000);
        fit_to(&mut uncapped, 640, 360);
        uncapped
            .reconfigure_at_current_dims()
            .expect("uncapped dim reconfigure");
        assert_eq!(config_key(&uncapped, "framerate"), None);
    }

    #[wasm_bindgen_test]
    fn a_rejected_dims_and_bitrate_reconfigure_leaves_the_cache_at_the_applied_rate_2550() {
        let mut layer = built_layer(1280, 720, 2_000_000);

        fit_to(&mut layer, 0, 0);
        let err = layer
            .reconfigure_at_current_dims_and_bitrate(900_000)
            .expect_err("a zero-dimension config must be rejected by configure()");
        assert!(
            !is_fatal_encoder_error(&err),
            "fixture must exercise the NON-fatal branch the AQ loop continues past: {err:?}",
        );

        assert_eq!(
            layer.local_bitrate, 2_000_000,
            "the cache claimed a rate configure() rejected (#2550)",
        );
        assert_eq!(
            plan_single_stream_reconfigure(false, 850_000, layer.should_attempt_bitrate(850_000)),
            SingleStreamReconfigure::Bitrate {
                bitrate_bps: 850_000
            },
            "the next unlatched target saw no delta left to re-apply (#2550)",
        );
    }

    #[wasm_bindgen_test]
    fn a_rejected_bitrate_only_reconfigure_leaves_the_cache_at_the_applied_rate_2550() {
        let mut layer = built_layer(1280, 720, 2_000_000);

        set_stored_config_width(&layer, 0.0);
        let err = layer
            .reconfigure_at_bitrate(800_000)
            .expect_err("a zero-width stored config must be rejected by configure()");
        assert!(
            !is_fatal_encoder_error(&err),
            "fixture must exercise the NON-fatal branch the AQ loop continues past: {err:?}",
        );

        assert_eq!(
            layer.local_bitrate, 2_000_000,
            "the cache claimed a rate configure() rejected (#2550)",
        );
        assert_eq!(
            plan_single_stream_reconfigure(false, 750_000, layer.should_attempt_bitrate(750_000)),
            SingleStreamReconfigure::Bitrate {
                bitrate_bps: 750_000
            },
            "the next unlatched target saw no delta left to re-apply (#2550)",
        );
    }

    #[wasm_bindgen_test]
    fn a_rejected_bitrate_is_withheld_from_the_next_encode_pass_2550() {
        let mut layer = built_layer(1280, 720, 2_000_000);

        set_stored_config_width(&layer, 0.0);
        layer
            .reconfigure_at_bitrate(800_000)
            .expect_err("a zero-width stored config must be rejected by configure()");

        assert_eq!(layer.last_failed_bitrate, Some(800_000));
        assert_eq!(
            plan_single_stream_reconfigure(false, 800_000, layer.should_attempt_bitrate(800_000)),
            SingleStreamReconfigure::Nothing,
            "the pass runs per captured frame, so an un-withheld rejection storms (#2550)",
        );
    }

    #[wasm_bindgen_test]
    fn a_dims_change_and_an_accepted_configure_both_re_admit_a_latched_bitrate_2550() {
        let mut layer = built_layer(1280, 720, 2_000_000);

        set_stored_config_width(&layer, 0.0);
        layer
            .reconfigure_at_bitrate(900_000)
            .expect_err("a zero-width stored config must be rejected by configure()");
        assert_eq!(layer.last_failed_bitrate, Some(900_000));

        fit_to(&mut layer, 640, 360);
        assert_eq!(
            layer.last_failed_bitrate, None,
            "a dims change rebuilds the config, so the latch must not outlive it (#2550)",
        );
        assert_eq!(
            plan_single_stream_reconfigure(false, 900_000, layer.should_attempt_bitrate(900_000)),
            SingleStreamReconfigure::Bitrate {
                bitrate_bps: 900_000
            },
            "a previously rejected bitrate must be retried at the new dims (#2550)",
        );

        set_stored_config_width(&layer, 0.0);
        layer
            .reconfigure_at_bitrate(700_000)
            .expect_err("re-arm the latch on a SECOND value");
        assert_eq!(layer.last_failed_bitrate, Some(700_000));

        layer
            .reconfigure_at_current_dims_and_bitrate(900_000)
            .expect("the rebuilt config at the new dims is accepted");
        assert_eq!(layer.local_bitrate, 900_000);
        assert_eq!(
            layer.last_failed_bitrate, None,
            "an accepted dims+bitrate configure() must retire a latch on another value (#2550)",
        );
    }

    #[wasm_bindgen_test]
    fn a_target_back_at_the_applied_rate_retires_the_latch_2550() {
        let mut layer = built_layer(1280, 720, 2_000_000);

        set_stored_config_width(&layer, 0.0);
        layer
            .reconfigure_at_bitrate(900_000)
            .expect_err("a zero-width stored config must be rejected by configure()");
        assert_eq!(layer.last_failed_bitrate, Some(900_000));

        assert!(
            !layer.should_attempt_bitrate(2_000_000),
            "the encoder is already at the applied rate",
        );
        assert_eq!(
            layer.last_failed_bitrate, None,
            "a target back at the applied rate must retire the latch (#2550)",
        );
        assert!(
            layer.should_attempt_bitrate(900_000),
            "an A->B->A->B oscillation must retry B, as it did before the latch (#2550)",
        );
    }

    #[wasm_bindgen_test]
    fn a_latched_bitrate_stays_withheld_across_repeated_passes_2550() {
        let mut layer = built_layer(1280, 720, 2_000_000);

        set_stored_config_width(&layer, 0.0);
        layer
            .reconfigure_at_bitrate(900_000)
            .expect_err("a zero-width stored config must be rejected by configure()");

        assert!(!layer.should_attempt_bitrate(900_000));
        assert!(
            !layer.should_attempt_bitrate(900_000),
            "the pass runs per captured frame, so one dwell must not re-attempt (#2550)",
        );
        assert_eq!(layer.last_failed_bitrate, Some(900_000));
    }
    #[wasm_bindgen_test]
    fn an_accepted_bitrate_only_configure_retires_the_latch_2550() {
        let mut layer = built_layer(1280, 720, 2_000_000);

        set_stored_config_width(&layer, 0.0);
        layer
            .reconfigure_at_bitrate(700_000)
            .expect_err("a zero-width stored config must be rejected by configure()");
        assert_eq!(layer.last_failed_bitrate, Some(700_000));

        set_stored_config_width(&layer, 1280.0);
        layer
            .reconfigure_at_bitrate(900_000)
            .expect("the restored config is accepted");
        assert_eq!(layer.local_bitrate, 900_000);
        assert_eq!(
            layer.last_failed_bitrate, None,
            "an accepted bitrate-only configure() must retire a latch on another value (#2550)",
        );
    }
}

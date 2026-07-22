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

// This submodule defines two pub types:
//
//      AudioPeerDecoder
//      VideoPeerDecoder
//
// Both implement a method decoder.decode(packet) that decodes and sends the result to the
// appropriate output, as configured in the new() constructor.
//
// Both are specializations of a generic type PeerDecoder<...> for the decoding logic,
// and each one's new() contains the type-specific creation/configuration code.
//

use super::audio_decoder_wrapper::{AudioDecoderTrait, AudioDecoderWrapper};
use super::config::configure_audio_context;
use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_CODEC;
use crate::constants::AUDIO_SAMPLE_RATE;
use log::error;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use videocall_codecs::decoder::WasmDecoder;
use videocall_codecs::frame::{FrameBuffer, FrameCodec, FrameType, VideoFrame as CodecVideoFrame};
use videocall_codecs::playout::LatestFrameMailbox;
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent, Metric, MetricValue};
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::media_packet::VideoCodec;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::HtmlCanvasElement;
use web_sys::{AudioData, AudioDecoderConfig, AudioDecoderInit};
use web_sys::{CanvasRenderingContext2d, CodecState};
use web_sys::{MediaStreamTrackGenerator, MediaStreamTrackGeneratorInit};
use web_time;

pub struct DecodeStatus {
    pub _rendered: bool,
    pub first_frame: bool,
}

pub trait PeerDecode {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus>;
}

/// Cached canvas rendering context to avoid expensive DOM queries
struct CanvasRenderer {
    canvas: HtmlCanvasElement,
    context: CanvasRenderingContext2d,
    last_width: u32,
    last_height: u32,
    /// Peer context for diagnostics. Set via [`VideoPeerDecoder::set_stream_context`].
    from_peer: Option<String>,
    to_peer: Option<String>,
}

/// Shared slot for the proactive keyframe-request route (issue #1025). `Rc` so the decoder's
/// worker-message closure and `VideoPeerDecoder` share it; `RefCell<Option<..>>` because the
/// route is installed after construction (and may be `None` before the transport is wired).
///
/// The route closure receives the head-of-line backlog age (`head_age_ms`, issue #1479) that
/// tripped the freshness deadline, so the manager's per-receiver cross-sender PLI budget can
/// prioritize the stalest stream when its global cap is reached.
type KeyframeRequestRoute = Rc<RefCell<Option<Box<dyn Fn(f64)>>>>;

/// Shared handle to the `requestAnimationFrame` paint closure (issue #1783). `Rc` so the
/// `on_video_frame` offer closure can schedule it while `VideoPeerDecoder` keeps it alive;
/// `RefCell<Option<..>>` because the closure is filled in after the cell is created in
/// [`VideoPeerDecoder::new`] (it captures a clone of the cell for scheduling).
type RafPaintClosure = Rc<RefCell<Option<Closure<dyn FnMut()>>>>;

/// Shared handle to the painted-fps sample-timer closure (issue #1784). Same shape as
/// [`RafPaintClosure`] — an `Rc<RefCell<Option<..>>>` kept on `VideoPeerDecoder` so the browser
/// `setInterval` callback stays alive for the decoder's lifetime; a dedicated alias keeps the
/// struct field and the local both under clippy's `type_complexity` threshold.
type SampleTimerClosure = Rc<RefCell<Option<Closure<dyn FnMut()>>>>;

/// Issue #1784: diagnostics subsystem + metric for per-peer PAINTED fps — the rate
/// of frames actually drawn to the canvas at the rAF paint site, as opposed to the
/// arrival-rate `video` / `fps_received` bucket (`diagnostics_manager.rs`) that
/// counts packets the instant they are dispatched into the decode pipeline, before
/// the jitter buffer drops/holds/skips and before #1783 coalesces a burst of late
/// frames to a single draw. The media-metrics overlay's "↓ fps" reads THIS.
///
/// A dedicated subsystem (not a new metric on the existing `video` event) because
/// the two are produced at different sites: `fps_received` is emitted by the
/// `DiagnosticManager` heartbeat, while `fps_painted` is emitted here, from the
/// per-peer `VideoPeerDecoder` that owns the paint. `health_reporter` and the
/// diagnostics drawer ignore unknown subsystems, so this is additive.
pub const SUBSYSTEM_VIDEO_PAINTED: &str = "video_painted";
pub const METRIC_FPS_PAINTED: &str = "fps_painted";

/// Sample/emit cadence for the painted-fps meter (issue #1784): one 1 s rollup +
/// broadcast per video decoder. Mirrors `FpsTracker`'s ~1 s bucket and
/// `render_fps`'s 1 Hz interval.
#[cfg(target_arch = "wasm32")]
const PAINTED_FPS_SAMPLE_INTERVAL_MS: i32 = 1000;

/// Pure, host-testable 1-second painted-fps meter (issue #1784).
///
/// Counts frames ACTUALLY PAINTED to the canvas — `record_paint` is called at the
/// rAF paint site in [`VideoPeerDecoder`], right where `render_to_canvas_cached`
/// reports it drew — and `sample` rolls the count into an fps value on the ~1 Hz
/// tick that emits the [`SUBSYSTEM_VIDEO_PAINTED`] event. This is deliberately
/// distinct from the arrival-rate `FpsTracker` (`fps_received`): a burst of late
/// frames that #1783 coalesces to a single draw counts as ONE paint here, so the
/// value never exceeds the source frame rate.
///
/// A window with zero paints (a stopped or hidden tile — no `record_paint` calls)
/// samples to exactly `0.0`, so the overlay's snap-down reverts the readout to the
/// em-dash. Timestamps are passed in (`js_sys::Date::now()` in production, synthetic
/// values in tests) so the bucketing math carries no web-sys dependency and is unit
/// tested on the host.
///
/// EMISSION GATING (issue #1784, perf): the sampler does NOT broadcast a zero every
/// second forever after a peer stops painting. [`sample_and_gate`] emits while
/// painting, then only the first [`ZERO_EMIT_GRACE`] zero samples after a
/// paint→no-paint edge (so the overlay still snaps to the em-dash), then goes SILENT
/// until the next `record_paint` re-arms it. The re-arm trigger is a paint EVENT (not
/// a consecutive-success counter that could reset under contention), so quiescence is
/// event-driven and cannot wedge a healthy stream into silence — a single paint fully
/// re-arms the trailing-zero budget.
struct PaintRateMeter {
    /// Frames painted since the last `sample()`.
    frames_since_sample: u32,
    /// Wall-clock ms at the last `sample()` — the current window's start. Read only
    /// by `sample`, which the production caller invokes only under wasm (the browser
    /// timer); host tests exercise it too, but the plain host (non-test) build of
    /// the crate — how `videocall-ui` compiles this dep for its host test binary —
    /// never reads it, hence the same wasm-scoped allow as `raf_id` above.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    last_sample_ms: f64,
    /// Trailing-zero emission budget (issue #1784 emission gating). `record_paint`
    /// re-arms it to [`ZERO_EMIT_GRACE`]; `sample_and_gate` spends one unit per
    /// zero-fps tick it emits, then falls silent at 0. SEEDED to [`ZERO_EMIT_GRACE`]
    /// (not 0) at construction — see [`PaintRateMeter::new`] for the decoder-
    /// replacement rationale. Written live by `record_paint` but read only in the
    /// wasm-only/test `sample_and_gate`, hence the wasm-scoped allow (same rationale
    /// as `last_sample_ms`).
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    zero_emit_budget: u8,
}

/// Number of zero-fps samples the painted-fps sampler still broadcasts after a peer
/// stops painting, before going silent (issue #1784). Two (not one) so the overlay's
/// snap-down-to-em-dash survives a single dropped bus message; the overlay only needs
/// one zero to revert, so this is a deliberate safety margin, not a correctness floor.
const ZERO_EMIT_GRACE: u8 = 2;

impl PaintRateMeter {
    /// A fresh meter is SEEDED with a full [`ZERO_EMIT_GRACE`] trailing-zero budget so
    /// its first few ticks (before any paint) emit snap-down zeros rather than nothing.
    /// This clears a STALE overlay value across decoder replacement (issue #1784
    /// lifecycle edge): on reset / re-election `peer.video` is swapped for a new
    /// `VideoPeerDecoder`, dropping the old (possibly still-armed) meter before its
    /// trailing zeros fire; the overlay's `fps_painted` signal is keyed by peer_id and
    /// SURVIVES that swap, so a `> 0` value would otherwise persist — visibly, since the
    /// overlay renders over the "Video Disabled" placeholder too — until the peer's
    /// video happens to resume (permanent if it never does). Seeding makes the new
    /// meter broadcast up to 2 zeros that snap the readout to the em-dash. Bounded cost:
    /// at most `ZERO_EMIT_GRACE` events per decoder construction.
    fn new(now_ms: f64) -> Self {
        Self {
            frames_since_sample: 0,
            last_sample_ms: now_ms,
            zero_emit_budget: ZERO_EMIT_GRACE,
        }
    }

    /// Record one frame painted to the canvas. Called at the paint site. Also RE-ARMS
    /// the trailing-zero emission budget: any paint is the event that resumes emission,
    /// so a healthy (even intermittently painting) stream can never be wedged silent.
    fn record_paint(&mut self) {
        self.frames_since_sample = self.frames_since_sample.saturating_add(1);
        self.zero_emit_budget = ZERO_EMIT_GRACE;
    }

    /// Roll the elapsed window into a painted-fps value and reset the window.
    ///
    /// Returns `0.0` when the window had zero paints (stopped/hidden tile) or a
    /// non-positive elapsed span; otherwise `frames * 1000 / elapsed_ms`, the
    /// painted rate normalized to per-second.
    ///
    /// Called by [`sample_and_gate`](Self::sample_and_gate) (and host unit tests); the
    /// plain host build has no live caller, so it carries the same wasm-scoped allow as
    /// the rAF fields above.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn sample(&mut self, now_ms: f64) -> f64 {
        let elapsed = now_ms - self.last_sample_ms;
        let count = self.frames_since_sample;
        self.frames_since_sample = 0;
        self.last_sample_ms = now_ms;
        if count == 0 || elapsed <= 0.0 {
            return 0.0;
        }
        (count as f64 * 1000.0) / elapsed
    }

    /// Sample the window AND apply emission gating (issue #1784). Returns:
    ///   * `Some(fps)` with `fps > 0` — the window painted; always emitted so the
    ///     overlay tracks a live rate (this path also leaves the budget armed via
    ///     `record_paint`).
    ///   * `Some(0.0)` — the first [`ZERO_EMIT_GRACE`] samples after painting stops,
    ///     so the overlay snaps DOWN to the em-dash; the budget is spent one per tick.
    ///   * `None` — the budget is exhausted: stay SILENT (no bus broadcast) until the
    ///     next `record_paint` re-arms. Because the re-arm is a paint event, a never-
    ///     painting or long-stopped decoder emits nothing per second, and a resuming
    ///     stream re-emits immediately — quiescence is event-driven, not a counter that
    ///     could pin a healthy stream.
    ///
    /// wasm-only caller (the sample timer) plus host tests, hence the wasm-scoped allow.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn sample_and_gate(&mut self, now_ms: f64) -> Option<f64> {
        let fps = self.sample(now_ms);
        if fps > 0.0 {
            return Some(fps);
        }
        if self.zero_emit_budget > 0 {
            self.zero_emit_budget -= 1;
            return Some(0.0);
        }
        None
    }
}

/// Build the per-peer painted-fps diagnostics event (issue #1784). Mirrors the
/// `video_resolution` event this same decoder already emits: `to_peer` is the
/// SENDING peer's session id (the id the UI's `PeerTile` matches its own `peer_id`
/// against), `from_peer` is the local session, and `media_type` distinguishes the
/// camera decoder (`VIDEO`) from the screen decoder (`SCREEN`). Extracted so the
/// event shape is host-testable without a real decoder or browser clock.
///
/// Takes the peer ids BY VALUE and moves them into the metrics (`From<String>` is a
/// move, no re-allocation), so the caller's one clone of the shared stream-context is
/// the only allocation. `media_type` is `&'static`, so it uses the zero-alloc
/// [`MetricValue::text_static`] borrow (the #1421 pattern) rather than allocating a
/// `String` on every 1 Hz emission and again on every per-subscriber delivery clone.
pub fn build_painted_fps_event(
    from_peer: String,
    to_peer: String,
    media_type: &'static str,
    fps: f64,
) -> DiagEvent {
    DiagEvent {
        subsystem: SUBSYSTEM_VIDEO_PAINTED,
        stream_id: None,
        ts_ms: now_ms(),
        metrics: vec![
            metric!(METRIC_FPS_PAINTED, fps),
            metric!("from_peer", from_peer),
            metric!("to_peer", to_peer),
            Metric {
                name: "media_type",
                value: MetricValue::text_static(media_type),
            },
        ],
    }
}

///
/// VideoPeerDecoder
///
/// Caches canvas and rendering context to avoid expensive DOM queries on every frame.
/// The canvas can be set after creation using `set_canvas()`, enabling flexible initialization.
///
pub struct VideoPeerDecoder {
    decoder: Box<dyn VideoFrameDecoder>,
    canvas_renderer: Rc<RefCell<Option<CanvasRenderer>>>,
    /// Discriminator tag emitted on diagnostics events so consumers can tell
    /// camera-video resolution events apart from screen-share ones. Mirrors
    /// the `media_type` metric already carried by the FPS/bitrate events
    /// (`"VIDEO"` or `"SCREEN"`).
    media_type: &'static str,
    /// Last `(source_width, source_height)` we saw on a `MediaPacket`'s
    /// `VideoMetadata`. Used to dedupe `video_source_resolution` diag events
    /// — those would otherwise fire on every decoded frame. `(0, 0)` means
    /// either we've never seen the field or the publisher is older /
    /// doesn't report it; in both cases we suppress the broadcast.
    last_source_dims: RefCell<(u32, u32)>,
    /// Issue #903: last `(encoder_target_bitrate_kbps, adaptive_tier,
    /// cause_hint)` we saw on a `VideoMetadata`. Used to dedupe
    /// `screen_encoder_state` diag events the same way `last_source_dims`
    /// dedupes resolution events. Empty / zero tuple means either the
    /// publisher hasn't stamped the fields yet or the field has never
    /// changed since the last broadcast. The tuple is owned strings (not
    /// `&'static str`) because the values flow from a protobuf message
    /// the consumer can't reason about lifetime-wise.
    last_encoder_state: RefCell<(u32, String, String)>,
    /// Peer-id pair used to tag the source-resolution diag event. We can't
    /// borrow it from the `CanvasRenderer` because that storage may be
    /// `None` when the canvas hasn't been wired yet, but
    /// `set_stream_context` *does* run before any decoded frames. Set there.
    ///
    /// Shared (`Rc`) so the painted-fps sample timer (issue #1784, captured in
    /// [`Self::new`]) can read the `(from_peer, to_peer)` pair to tag its
    /// `video_painted` broadcast. `RefCell` because the pair is written after
    /// construction by `set_stream_context`; every access is on the render thread.
    stream_context: Rc<RefCell<Option<(String, String)>>>,
    /// HCL issue 893: pending acknowledgement that the underlying
    /// `WasmDecoder` has produced its first decoded frame and rendered it
    /// to the canvas. The decoder pipeline is asynchronous — `decode()`
    /// pushes a `FrameBuffer` into a worker and returns immediately, so
    /// the synchronous return value cannot carry a "first frame decoded"
    /// signal. Instead the `on_video_frame` callback (which runs on the
    /// render thread when the decoder hands a real `VideoFrame` back) sets
    /// this flag to `true` on its first invocation. The next `decode()`
    /// call observes the flag, swaps it back to `false`, and returns
    /// `first_frame: true` so `peer_decode_manager` can fire the
    /// `PEER_EVENT(screen_decode_started)` ack to the publisher. Without
    /// this signal the screen-share visibility toast on the publisher
    /// would time out at 10s on every share, even on the happy path.
    first_render_pending_ack: Rc<RefCell<bool>>,
    /// Issue #1183 (late-frame race): gate for the async paint callback.
    ///
    /// `clear_canvas()` (called synchronously on the decode-stop edge) sets
    /// this `false`; the next successful `decode()` (which is only reached when
    /// the tile is visible — `peer_decode_manager` returns `SKIPPED` *before*
    /// calling us otherwise) sets it `true`. The `on_video_frame` callback
    /// reads it and, when `false`, drops (closes) the frame instead of
    /// painting. This closes the window where a `VideoFrame` decoded from a
    /// packet pushed BEFORE the visible→false flip fires its async callback
    /// AFTER `clear_canvas()`, repainting one stale frame and re-freezing the
    /// tile the #1183 clear was meant to wipe.
    ///
    /// Shared (`Rc`) with the paint closure captured in [`Self::new`]; the
    /// `Cell` is sufficient because every access is on the single render thread.
    paint_enabled: Rc<Cell<bool>>,
    /// Issue #1025: proactive keyframe-request route. The underlying `WasmDecoder`'s
    /// worker-message closure (captured in [`Self::new`]) holds a clone of this `Rc` and,
    /// when the worker posts a `RequestKeyframeMessage`, invokes the inner closure if set.
    /// The owner (`PeerDecodeManager`) installs the closure via
    /// [`Self::set_keyframe_request_route`] once it has the transport send-packet callback,
    /// the local user id, and this peer's identity. `None` (the initial state, and after a
    /// disconnect that clears the route) makes the proactive path a safe no-op.
    ///
    /// Shared on the single render thread; `RefCell` because the closure is installed after
    /// construction. The boxed closure issues a `KEYFRAME_REQUEST` for this decoder's
    /// peer/stream — it is bound to one (peer, media_type), so the worker message carries no
    /// identity.
    keyframe_request_route: KeyframeRequestRoute,
    /// Issue #1783 realtime-first playout: latest-wins presentation mailbox. The `on_video_frame`
    /// callback (which drains every decoded frame from the worker→main queue) `offer`s each frame
    /// here instead of painting it synchronously; a `requestAnimationFrame`-scheduled paint then
    /// `take`s the newest and draws it. A burst of late frames thus collapses to a single draw (an
    /// instant jump to live) rather than a fast-forward replay. Steady state (≤1 frame pending per
    /// frame interval) is unchanged: the one held frame is presented untouched.
    ///
    /// Shared (`Rc`) with the paint closure and the offer closure captured in [`Self::new`]; kept
    /// on `self` so `clear_canvas` and `Drop` can close any still-held frame (freeing GPU memory the
    /// browser would not reclaim on its own). `web_sys::VideoFrame` is not `Send`/`Sync`, but every
    /// access is on the single render thread.
    latest_frame: Rc<RefCell<LatestFrameMailbox<web_sys::VideoFrame>>>,
    /// Issue #1783: whether a `requestAnimationFrame` paint is currently pending. Prevents
    /// scheduling more than one rAF at a time — the single scheduled paint always presents whatever
    /// is newest when it fires, so a burst of offers coalesces to one draw.
    ///
    /// Read only in the wasm-gated `Drop` (its writers are the closures in `new`, which hold their
    /// own `Rc` clones), so on the host build it is held-but-not-read; the `allow(dead_code)` is
    /// scoped to non-wasm and does not mask a real unused field on the target that ships.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    raf_scheduled: Rc<Cell<bool>>,
    /// Issue #1783: handle of the pending rAF paint, so `Drop` can `cancel_animation_frame` it
    /// before the paint closure is freed (mirrors `render_fps::RenderFpsObserver`). `0` when none is
    /// pending. Read only in the wasm-gated `Drop`; see `raf_scheduled` for the non-wasm allow.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    raf_id: Rc<Cell<i32>>,
    /// Issue #1783: keeps the rAF paint closure alive for the decoder's lifetime. The offer closure
    /// (stored inside the `WasmDecoder`) holds a clone of this `Rc` to schedule the paint; storing
    /// it here too makes the lifetime explicit and independent of the decoder's internals.
    _raf_paint_closure: RafPaintClosure,
    /// Issue #1784: `setInterval` handle of the 1 Hz painted-fps sampler so `Drop` can clear it
    /// (mirrors `render_fps::RenderFpsObserver`). `0` when none is installed. Set + read only in
    /// wasm-gated code (the sampler is a browser timer); on the host build it is held-but-not-read.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    painted_fps_interval_id: Rc<Cell<i32>>,
    /// Issue #1784: keeps the painted-fps sampler closure alive for the decoder's lifetime, exactly
    /// as `_raf_paint_closure` does for the paint closure. Populated only in the wasm branch of
    /// [`Self::new`]; `None` on the host build (and in `noop()`).
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    _painted_fps_interval_closure: SampleTimerClosure,
}

// Trait to handle VideoFrame callbacks in WASM
trait VideoFrameDecoder {
    fn push_frame(&self, frame: FrameBuffer);
    fn is_waiting_for_keyframe(&self) -> bool;
    fn flush(&self);
    fn set_stream_context(&self, _from_peer: String, _to_peer: String) {}
}

struct WasmVideoFrameDecoder {
    decoder: WasmDecoder,
}

impl VideoFrameDecoder for WasmVideoFrameDecoder {
    fn push_frame(&self, frame: FrameBuffer) {
        self.decoder.push_frame(frame);
    }

    fn is_waiting_for_keyframe(&self) -> bool {
        self.decoder.is_waiting_for_keyframe()
    }

    fn flush(&self) {
        self.decoder.flush()
    }

    fn set_stream_context(&self, from_peer: String, to_peer: String) {
        self.decoder.set_context(from_peer, to_peer);
    }
}

/// Media-type discriminator passed to [`VideoPeerDecoder::new`]. Distinguishes
/// camera video streams from screen-share streams in diagnostics events so the
/// UI can chart them separately. The values match the existing `media_type`
/// metric carried on FPS/bitrate events.
pub const MEDIA_TYPE_CAMERA: &str = "VIDEO";
pub const MEDIA_TYPE_SCREEN: &str = "SCREEN";

/// Decide what `(from_peer, to_peer)` to stamp on a freshly-constructed
/// [`CanvasRenderer`] inside [`VideoPeerDecoder::set_canvas`].
///
/// Two real-world orderings have to converge here:
///
/// 1. Canvas attached *before* `set_stream_context` (camera path: the
///    `<canvas>` element exists at peer-tile mount, before the first packet
///    arrives). The renderer was created with `(None, None)`, then
///    `set_stream_context` populated it directly. Subsequent re-attachments
///    must preserve that pair.
/// 2. Canvas attached *after* `set_stream_context` (screen-share path: the
///    `ScreenCanvas` tile only mounts once the peer's screen-share is
///    advertised, which is after the first media packet — and the first
///    packet is what triggers `set_stream_context`). The prior renderer is
///    either absent or carries `(None, None)` and we must seed the new
///    renderer from the decoder-level `stream_context` instead, otherwise
///    `render_to_canvas_cached` cannot emit `video_resolution` diag events
///    (it gates on `renderer.to_peer.is_some()`) and the screen-share
///    resolution stays hidden in the Signal Quality tooltip for the whole
///    session. This was the #883 regression.
fn resolve_renderer_context(
    prior_renderer_ctx: Option<(Option<String>, Option<String>)>,
    decoder_stream_ctx: Option<&(String, String)>,
) -> (Option<String>, Option<String>) {
    if let Some((fp, tp)) = prior_renderer_ctx {
        if fp.is_some() || tp.is_some() {
            return (fp, tp);
        }
    }
    match decoder_stream_ctx {
        Some((fp, tp)) => (Some(fp.clone()), Some(tp.clone())),
        None => (None, None),
    }
}

impl VideoPeerDecoder {
    /// Create a new video decoder with optional canvas element.
    /// Use `set_canvas()` to provide the canvas if not available at construction time.
    ///
    /// `media_type` tags the resolution diagnostics event so the UI can route
    /// camera-video and screen-share resolution updates to the right place.
    /// Use [`MEDIA_TYPE_CAMERA`] for the peer's camera decoder and
    /// [`MEDIA_TYPE_SCREEN`] for the peer's screen-share decoder.
    pub fn new(
        canvas: Option<HtmlCanvasElement>,
        media_type: &'static str,
    ) -> Result<Self, JsValue> {
        let canvas_renderer = Rc::new(RefCell::new(None));

        // Initialize canvas if provided
        if let Some(canvas) = canvas {
            let context = canvas
                .get_context("2d")?
                .ok_or_else(|| JsValue::from_str("Failed to get 2d context"))?
                .dyn_into::<CanvasRenderingContext2d>()?;

            *canvas_renderer.borrow_mut() = Some(CanvasRenderer {
                canvas,
                context,
                last_width: 0,
                last_height: 0,
                from_peer: None,
                to_peer: None,
            });
        }

        // HCL #893: shared flag the async render callback uses to tell the
        // next synchronous `decode()` call that a frame has actually
        // landed on the canvas. See doc comment on `first_render_pending_ack`.
        let first_render_pending_ack = Rc::new(RefCell::new(false));

        // Issue #1183 late-frame race: starts enabled (a freshly-constructed
        // decoder belongs to a visible tile), gated off by `clear_canvas()`,
        // back on by the next `decode()`.
        let paint_enabled = Rc::new(Cell::new(true));

        // Issue #1783 realtime-first playout: the latest-wins presentation mailbox and its
        // rAF-scheduled paint. The offer closure below hands every decoded frame here and schedules
        // a single paint; the paint presents only the newest, closing the rest. See the field docs.
        let latest_frame: Rc<RefCell<LatestFrameMailbox<web_sys::VideoFrame>>> =
            Rc::new(RefCell::new(LatestFrameMailbox::new()));
        let raf_scheduled = Rc::new(Cell::new(false));
        let raf_id = Rc::new(Cell::new(0i32));

        // Issue #1784 painted-fps: the 1 s-bucket paint counter, shared with the rAF paint closure
        // (which `record_paint`s on each successful draw) and the sample timer below. Seed the
        // window start with the current wall clock so the first `sample()` normalizes correctly.
        let paint_meter = Rc::new(RefCell::new(PaintRateMeter::new(js_sys::Date::now())));

        // Track within the closure (cheap `Cell` would suffice but we already
        // need an `Rc<RefCell<bool>>` on `self` so we mirror the cell into the
        // closure). `mark_first_render` only flips once per
        // `VideoPeerDecoder` — every later render is a no-op (see the
        // `mark_first_render_*` tests for the pinned semantics).
        let first_render_fired = Rc::new(RefCell::new(false));

        // The rAF paint: present the single newest held frame, at most one draw per animation
        // frame. Held in an `Rc<RefCell<Option<Closure>>>` so the offer closure can schedule it and
        // so it stays alive for the decoder's lifetime.
        let raf_paint_closure: RafPaintClosure = Rc::new(RefCell::new(None));
        {
            let mailbox = latest_frame.clone();
            let canvas_ref = canvas_renderer.clone();
            let paint_flag = paint_enabled.clone();
            let first_render_flag = first_render_pending_ack.clone();
            let first_render_fired = first_render_fired.clone();
            let raf_scheduled_for_paint = raf_scheduled.clone();
            let paint_meter_for_paint = paint_meter.clone();
            *raf_paint_closure.borrow_mut() = Some(Closure::wrap(Box::new(move || {
                // This rAF has fired; allow the next offer to schedule the following paint.
                raf_scheduled_for_paint.set(false);
                // Present the newest frame; older frames of the burst were already closed at offer
                // time. `take` releases the mailbox borrow before we render (render never touches
                // the mailbox, but keep the borrow window minimal).
                let frame = mailbox.borrow_mut().take();
                if let Some(video_frame) = frame {
                    // Issue #1183 late-frame race, re-checked at *paint* time: if the tile was
                    // hidden (its `clear_canvas()` ran) between the offer and this rAF, drop the
                    // frame WITHOUT painting — still close it to release GPU memory — so it cannot
                    // repaint the wiped tile. Checking here (not only at offer) closes the window
                    // fully, since the actual draw now happens later than the offer.
                    if !paint_flag.get() {
                        video_frame.close();
                        return;
                    }
                    mark_first_render(&first_render_fired, &first_render_flag);
                    // Issue #1784: count this frame in the painted-fps meter ONLY when the draw
                    // actually landed on a wired canvas. `render_to_canvas_cached` returns `false`
                    // when no canvas is set yet or the `drawImage` errored — those are not paints,
                    // so they must not inflate the rate.
                    if Self::render_to_canvas_cached(&canvas_ref, video_frame, media_type) {
                        paint_meter_for_paint.borrow_mut().record_paint();
                    }
                }
            })
                as Box<dyn FnMut()>));
        }

        // The worker→main drain callback (issue #1783): offer each decoded frame to the latest-wins
        // mailbox and ensure exactly one rAF paint is pending. This runs synchronously per
        // `postMessage` the `WasmDecoder` drains; the per-frame `frames_painted` ACK is incremented
        // one step upstream (in `decoder/wasm.rs`, before this callback), so a frame coalesced away
        // here is still counted as consumed and `paint_lag_ms` stays correct.
        let paint_flag = paint_enabled.clone();
        let mailbox_for_offer = latest_frame.clone();
        let raf_scheduled_for_offer = raf_scheduled.clone();
        let raf_id_for_offer = raf_id.clone();
        let raf_paint_for_offer = raf_paint_closure.clone();
        // Cache the `Window` handle once (the global window is stable for the page lifetime) and
        // capture it in the offer closure, rather than re-fetching `web_sys::window()` on every
        // scheduled frame — mirrors `render_fps::RenderFpsObserver::start`. `None` (no window) makes
        // rAF scheduling a no-op, exactly as before.
        let window_for_offer = web_sys::window();
        let on_video_frame = move |video_frame: web_sys::VideoFrame| {
            // Issue #1183 late-frame race: if painting was disabled on the
            // decode-stop edge, drop this frame WITHOUT holding or painting
            // (still close it to release the GPU/codec resource) so a frame that
            // finished decoding after `clear_canvas()` cannot repaint the wiped
            // tile — and so nothing accumulates in the mailbox for a hidden tile.
            if !paint_flag.get() {
                video_frame.close();
                return;
            }
            // Latest-wins: hold only the newest decoded frame. A displaced (older, still-unpainted)
            // frame is closed here — issue #1783 presents only the newest, and closing the skipped
            // `VideoFrame` is mandatory to free its GPU memory.
            if let Some(stale) = mailbox_for_offer.borrow_mut().offer(video_frame) {
                stale.close();
            }
            // Ensure a single rAF paint is pending. It will present whatever is newest when it
            // fires, so a burst arriving across several message tasks coalesces to one draw.
            if !raf_scheduled_for_offer.get() {
                if let Some(win) = window_for_offer.as_ref() {
                    if let Some(cb) = raf_paint_for_offer.borrow().as_ref() {
                        if let Ok(id) = win.request_animation_frame(cb.as_ref().unchecked_ref()) {
                            raf_id_for_offer.set(id);
                            raf_scheduled_for_offer.set(true);
                        }
                    }
                }
            }
        };

        // Issue #1025: shared slot for the proactive keyframe-request route. The closure
        // handed to the decoder reads this slot when the worker signals a keyframe-less
        // eviction; the manager installs the real route later via
        // `set_keyframe_request_route`. While `None` the proactive path is a no-op.
        let keyframe_request_route: KeyframeRequestRoute = Rc::new(RefCell::new(None));
        let route_for_decoder = keyframe_request_route.clone();
        let on_request_keyframe = move |head_age_ms: f64| {
            if let Some(route) = route_for_decoder.borrow().as_ref() {
                route(head_age_ms);
            }
        };

        let wasm_decoder = videocall_codecs::decoder::WasmDecoder::new_with_video_frame_callback(
            videocall_codecs::decoder::VideoCodec::Vp9Profile0Level10Bit8,
            Box::new(on_video_frame),
            Box::new(on_request_keyframe),
            // Issue #1641: thread this decoder's stream kind (MEDIA_TYPE_CAMERA / MEDIA_TYPE_SCREEN)
            // into the worker→main re-broadcast so its "video" playout-stats DiagEvent is bucketed
            // into the correct camera-vs-screen slot by health_reporter. The worker cannot supply
            // this (it only knows peer IDs), so the kind is stamped on the main thread here.
            media_type,
        );

        let decoder = Box::new(WasmVideoFrameDecoder {
            decoder: wasm_decoder,
        });

        // Issue #1784: shared peer-id context, read by the painted-fps timer to tag its broadcast.
        // Populated after construction by `set_stream_context` (which the manager calls before any
        // frame is decoded); while `None`, the timer skips emission for that tick.
        let stream_context: Rc<RefCell<Option<(String, String)>>> = Rc::new(RefCell::new(None));

        // Issue #1784: the 1 Hz painted-fps sampler — installed ONLY on the camera decoder
        // (`MEDIA_TYPE_CAMERA`). The overlay does not show a peer's screen-share painted-fps
        // (`overlay_painted_fps_sample` discards `SCREEN`), so a sampler on the screen decoder would
        // broadcast a useless zero every second that every one of the N tiles parses and drops.
        // Each tick rolls the paint counter through the emission gate (`sample_and_gate`); on an emit
        // decision it broadcasts a `video_painted` event tagged with this stream's peer ids. A
        // stopped/hidden tile paints nothing, so the gate emits the trailing zeros (overlay → em-dash)
        // then goes silent until a paint re-arms it. Browser-timer, hence wasm-only; on the host build
        // `PaintRateMeter` is exercised directly by unit tests. Mirrors
        // `render_fps::RenderFpsObserver`'s interval + teardown.
        let painted_fps_interval_id = Rc::new(Cell::new(0i32));
        let painted_fps_interval_closure: SampleTimerClosure = Rc::new(RefCell::new(None));
        #[cfg(target_arch = "wasm32")]
        if media_type == MEDIA_TYPE_CAMERA {
            let meter_for_timer = paint_meter.clone();
            let ctx_for_timer = stream_context.clone();
            let closure = Closure::<dyn FnMut()>::new(move || {
                // Emission gate: `None` means stay silent this tick (no broadcast).
                let Some(fps) = meter_for_timer
                    .borrow_mut()
                    .sample_and_gate(js_sys::Date::now())
                else {
                    return;
                };
                // Release the stream-context borrow BEFORE broadcasting (do not hold a RefCell borrow
                // across `try_broadcast`). This single clone is the only allocation the emission
                // costs — `build_painted_fps_event` moves the owned strings straight into the metrics.
                let ctx = ctx_for_timer.borrow().clone();
                if let Some((from_peer, to_peer)) = ctx {
                    let _ = global_sender().try_broadcast(build_painted_fps_event(
                        from_peer, to_peer, media_type, fps,
                    ));
                }
            });
            if let Some(win) = web_sys::window() {
                if let Ok(id) = win.set_interval_with_callback_and_timeout_and_arguments_0(
                    closure.as_ref().unchecked_ref(),
                    PAINTED_FPS_SAMPLE_INTERVAL_MS,
                ) {
                    painted_fps_interval_id.set(id);
                }
            }
            *painted_fps_interval_closure.borrow_mut() = Some(closure);
        }

        Ok(Self {
            decoder,
            canvas_renderer,
            media_type,
            last_source_dims: RefCell::new((0, 0)),
            last_encoder_state: RefCell::new((0, String::new(), String::new())),
            stream_context,
            first_render_pending_ack,
            paint_enabled,
            keyframe_request_route,
            latest_frame,
            raf_scheduled,
            raf_id,
            _raf_paint_closure: raf_paint_closure,
            painted_fps_interval_id,
            _painted_fps_interval_closure: painted_fps_interval_closure,
        })
    }

    /// Set or update the canvas element for rendering. Can be called multiple times.
    /// Preserves existing peer context (from_peer / to_peer) if already set.
    ///
    /// issue 508: DEFENSIVE idempotency guard. When the SAME `<canvas>` DOM node
    /// is re-handed to us (e.g. a peer's canvas-mount effect re-runs without the
    /// element changing), skip the renderer rebuild: returning `Ok(())` the moment
    /// the stored canvas `== canvas` (wasm-bindgen `PartialEq` → JS `===` object
    /// identity) preserves the existing `CanvasRenderer` — its cached
    /// `last_width`/`last_height`, from/to-peer context, and 2D context — so no
    /// redundant rebuild and no spurious `Resized canvas to WxH` resize on the next
    /// frame. This is a cheap correctness backstop, NOT the fix for the issue 508
    /// FPS collapse.
    ///
    /// The actual issue 508 cause was upstream in the UI: on a peer-leave the
    /// surviving single tile's `full_bleed` prop flipped, which under Dioxus 0.7
    /// (templates are diffed by template-pointer identity) swapped `generate_for_peer`
    /// from the regular-grid template to a SEPARATE full-bleed template. A template
    /// swap tears down and recreates the subtree, so the remaining peer got a FRESH
    /// `<canvas>` node — a different element, for which THIS guard's `==` is false.
    /// `set_canvas` then rebuilt the renderer with `last_width:0`, every later frame
    /// hit the resize branch in `render_to_canvas_cached`, and inbound FPS collapsed
    /// to 2–3. The load-bearing fix is the single-template unification in
    /// `canvas_generator.rs` (full-bleed is now a plain CSS class toggle inside the
    /// one grid template), which keeps Dioxus diffing the tile in place and REUSING
    /// the same canvas node so this method is never re-entered with a torn-down tile.
    /// Return the canvas element currently wired to this decoder, if any.
    ///
    /// Used by the recording module to composite peer video directly from the
    /// decoder's canvas rather than relying on the Dioxus-rendered DOM element,
    /// which may not be mounted when `show_canvas = false` (budget pressure,
    /// camera-on/off race, etc.).
    pub fn get_canvas(&self) -> Option<HtmlCanvasElement> {
        self.canvas_renderer
            .borrow()
            .as_ref()
            .map(|r| r.canvas.clone())
    }

    pub fn set_canvas(&self, canvas: HtmlCanvasElement) -> Result<(), JsValue> {
        // Idempotency guard: borrow is dropped at the end of this block, before
        // the later `borrow_mut()`, so there is no RefCell conflict.
        {
            if let Some(renderer) = self.canvas_renderer.borrow().as_ref() {
                if renderer.canvas == canvas {
                    return Ok(());
                }
            }
        }

        // Different element (real remount with a new node) or first-ever wiring:
        // full rebuild exactly as before.
        let context = canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("Failed to get 2d context"))?
            .dyn_into::<CanvasRenderingContext2d>()?;

        let mut guard = self.canvas_renderer.borrow_mut();
        let prior_ctx = guard
            .as_ref()
            .map(|r| (r.from_peer.clone(), r.to_peer.clone()));
        let (from_peer, to_peer) =
            resolve_renderer_context(prior_ctx, self.stream_context.borrow().as_ref());
        *guard = Some(CanvasRenderer {
            canvas,
            context,
            last_width: 0,
            last_height: 0,
            from_peer,
            to_peer,
        });
        Ok(())
    }

    /// Provide original peer IDs to the underlying decoder so worker can tag diagnostics.
    /// Also stores the peer context in the canvas renderer so resolution changes can
    /// be broadcast with the correct peer_id.
    pub fn set_stream_context(&self, from_peer: String, to_peer: String) {
        // Mirror the peer-id pair on `self` so `decode()` can tag the
        // source-resolution diag event regardless of whether the canvas
        // renderer is set yet.
        *self.stream_context.borrow_mut() = Some((from_peer.clone(), to_peer.clone()));

        // Store peer context in the canvas renderer for resolution broadcasts.
        if let Some(renderer) = self.canvas_renderer.borrow_mut().as_mut() {
            renderer.from_peer = Some(from_peer.clone());
            renderer.to_peer = Some(to_peer.clone());
            // If the canvas already has dimensions (frames arrived before
            // set_stream_context was called), broadcast the resolution now.
            if renderer.last_width > 0 && renderer.last_height > 0 {
                let evt = DiagEvent {
                    subsystem: "video_resolution",
                    stream_id: None,
                    ts_ms: now_ms(),
                    metrics: vec![
                        metric!("resolution_width", renderer.last_width as u64),
                        metric!("resolution_height", renderer.last_height as u64),
                        metric!("from_peer", from_peer.clone()),
                        metric!("to_peer", to_peer.clone()),
                        metric!("media_type", self.media_type.to_string()),
                    ],
                };
                let _ = global_sender().try_broadcast(evt);
            }
        }
        self.decoder.set_stream_context(from_peer, to_peer);
    }

    /// Render video frame using cached canvas and context. Only resizes when dimensions change.
    ///
    /// Aspect-ratio correctness (per-peer "squashed video" fix):
    ///
    /// A WebCodecs `VideoFrame` carries three distinct geometries:
    ///   * `coded_width/coded_height`  — the raw encoded buffer, padded up to the
    ///     codec's macroblock alignment (16px for VP8/VP9) and *before* rotation.
    ///   * `visibleRect`               — the cropped picture region inside the
    ///     coded buffer (the alignment padding removed). This is the *intrinsic*
    ///     source `drawImage` reads from.
    ///   * `display_width/display_height` — the dimensions the frame is meant to
    ///     be shown at, after crop, rotation, and any non-square sample-aspect
    ///     correction.
    ///
    /// The old code sized the canvas buffer to `display_*` but drew with the
    /// 3-arg `drawImage(frame, dx, dy)`, which paints the *intrinsic* (visible)
    /// source at 1:1 with no scaling. When `visibleRect` happened to equal
    /// `display_*` (clean codec-aligned, square-pixel, un-rotated frames) the
    /// painted region exactly filled the buffer and looked fine. But for peers
    /// whose frames carried crop padding, a non-square sample aspect ratio, or
    /// rotation, the visible source dimensions differed from `display_*`: only a
    /// sub-region of the `display`-sized buffer got painted, yet the buffer
    /// (and therefore the CSS `object-fit: cover` scaling) still declared the
    /// `display` aspect — so the picture rendered squashed/stretched. That
    /// "only some peers" split is the signature of this bug.
    ///
    /// The fix: keep the canvas buffer at the true `display_*` dimensions, but
    /// draw with the 6-arg `drawImage(frame, 0, 0, dw, dh)` form so the entire
    /// visible source is *scaled to fill* the whole display-sized buffer. This
    /// corrects the crop-padding and non-square-sample-aspect cases (the browser
    /// folds SAR into `display_*`), so the painted content's aspect matches the
    /// buffer's declared `display` aspect. NOTE: `drawImage` does NOT apply a
    /// frame's *rotation* metadata — it paints the visible pixels unrotated and
    /// only scales them, so a genuinely 90°/270°-rotated source would still need a
    /// canvas transform (out of scope here; the VP9 decode path in this pipeline
    /// does not carry rotation metadata — capture-side rotation is already baked
    /// into the pixels). Applies to both the camera and screen-share decoders
    /// (same `VideoPeerDecoder` path).
    ///
    /// Returns `true` when a frame was actually drawn to a wired canvas (the honest
    /// "painted" signal the #1784 painted-fps meter counts), and `false` when no
    /// canvas is wired yet or the `drawImage` call errored — neither of which put a
    /// frame on screen. The `video_frame` is always closed regardless.
    fn render_to_canvas_cached(
        canvas_renderer: &Rc<RefCell<Option<CanvasRenderer>>>,
        video_frame: web_sys::VideoFrame,
        media_type: &'static str,
    ) -> bool {
        let mut renderer_guard = canvas_renderer.borrow_mut();

        let painted = if let Some(renderer) = renderer_guard.as_mut() {
            // Always size the canvas buffer to the frame's *display* dimensions
            // (post-crop / post-rotation / sample-aspect-corrected). This is the
            // aspect the tile should present.
            let (width, height) =
                canvas_buffer_dims(video_frame.display_width(), video_frame.display_height());

            // Only resize canvas if dimensions changed (expensive operation)
            if renderer.last_width != width || renderer.last_height != height {
                renderer.canvas.set_width(width);
                renderer.canvas.set_height(height);
                renderer.last_width = width;
                renderer.last_height = height;
                log::debug!("Resized canvas to {width}x{height}");

                // Broadcast resolution change so the UI can display it in tooltips.
                if let Some(to_peer) = &renderer.to_peer {
                    let evt = DiagEvent {
                        subsystem: "video_resolution",
                        stream_id: None,
                        ts_ms: now_ms(),
                        metrics: vec![
                            metric!("resolution_width", width as u64),
                            metric!("resolution_height", height as u64),
                            metric!("from_peer", renderer.from_peer.clone().unwrap_or_default()),
                            metric!("to_peer", to_peer.clone()),
                            metric!("media_type", media_type.to_string()),
                        ],
                    };
                    let _ = global_sender().try_broadcast(evt);
                }
            }

            // Clear and draw frame.
            renderer
                .context
                .clear_rect(0.0, 0.0, width as f64, height as f64);
            // Draw the frame's full visible source scaled to fill the entire
            // display-sized buffer. The 6-arg form (dx, dy, dw, dh) is what makes
            // this aspect-correct for frames where the intrinsic (visible) source
            // size differs from the display size — see the doc comment above.
            match renderer.context.draw_image_with_video_frame_and_dw_and_dh(
                &video_frame,
                0.0,
                0.0,
                width as f64,
                height as f64,
            ) {
                Ok(()) => true,
                Err(e) => {
                    log::error!("Error drawing video frame: {e:?}");
                    false
                }
            }
        } else {
            log::debug!("Canvas not yet set, skipping frame render");
            false
        };

        video_frame.close();
        painted
    }

    /// Clear the canvas backing bitmap to transparent (issue #1183).
    ///
    /// Called when a peer leaves the active decode set (its tile transitions
    /// `visible: true -> false`). At that edge `decode()` starts returning
    /// `SKIPPED` synchronously, so no further `VideoFrame` is ever drawn — but
    /// the `<canvas>` element is only removed from the DOM later, when Dioxus
    /// commits the layout diff. In the window between decode-stop and DOM
    /// unmount (or indefinitely if the commit stalls / the budget re-pressures
    /// before the tile unmounts) the canvas keeps showing its LAST PAINTED
    /// FRAME — the "1 live + N frozen tiles" symptom.
    ///
    /// We clear through the SAME cached `CanvasRenderingContext2d` the painter
    /// (`render_to_canvas_cached`) draws into, so we are guaranteed to be
    /// clearing the exact backing bitmap that holds the stale frame — not a
    /// stale DOM lookup by id. The clear region uses the canvas element's
    /// current backing dimensions (`width()`/`height()`), which the painter
    /// keeps in sync with the decoded frame size via `set_width`/`set_height`.
    ///
    /// No-op when no canvas is wired yet (`canvas_renderer` is `None`), which is
    /// also the state of the `noop()` decoder used in host unit tests — so this
    /// is safe to call from non-wasm test code.
    pub fn clear_canvas(&self) {
        // Issue #1183 late-frame race: disable painting FIRST, so any
        // `VideoFrame` decoded from a packet pushed before the decode-stop edge
        // — whose `on_video_frame` callback may fire after this clear — is
        // dropped instead of repainting the tile we are about to wipe. The next
        // `decode()` (only reached while visible) re-enables painting.
        self.paint_enabled.set(false);
        // Issue #1783 compose with #1183: drop any frame still held for presentation. The tile is
        // leaving the active set, so a held frame must never be painted — not even by a rAF that
        // fires after a subsequent `decode()` re-enables `paint_enabled`. The rAF re-check of
        // `paint_enabled` already guards the still-hidden case, but taking the frame here also
        // covers the re-enable-before-rAF race and releases GPU memory immediately. On the host
        // test path the mailbox is always empty, so `take()` is `None` and `close()` is never hit.
        if let Some(frame) = self.latest_frame.borrow_mut().take() {
            frame.close();
        }
        if let Some(renderer) = self.canvas_renderer.borrow().as_ref() {
            let width = renderer.canvas.width();
            let height = renderer.canvas.height();
            renderer
                .context
                .clear_rect(0.0, 0.0, width as f64, height as f64);
        }
    }

    /// Clear the canvas pixels unconditionally — used when the remote peer
    /// actually turns their camera off (not budget-pressure invisibility).
    ///
    /// This explicit variant documents call sites where the media source itself
    /// stopped, rather than a decode-budget visibility transition.
    pub fn force_clear_canvas(&self) {
        self.paint_enabled.set(false);
        // Issue #1783 compose with #1746: drop any frame still held for
        // presentation so a pending rAF cannot repaint the tile after we wipe it
        // (mirror of the drain in `clear_canvas`). Unlike `clear_canvas`, this
        // runs even while recording, so the recording output switches to the
        // avatar immediately instead of freezing on the peer's last frame.
        if let Some(frame) = self.latest_frame.borrow_mut().take() {
            frame.close();
        }
        if let Some(renderer) = self.canvas_renderer.borrow().as_ref() {
            let width = renderer.canvas.width();
            let height = renderer.canvas.height();
            renderer
                .context
                .clear_rect(0.0, 0.0, width as f64, height as f64);
        }
    }

    fn get_frame_type(&self, packet: &Arc<MediaPacket>) -> FrameType {
        match packet.frame_type.as_str() {
            "key" => FrameType::KeyFrame,
            _ => FrameType::DeltaFrame,
        }
    }

    pub fn is_waiting_for_keyframe(&self) -> bool {
        self.decoder.is_waiting_for_keyframe()
    }

    pub fn flush(&self) {
        self.decoder.flush()
    }

    /// Install the proactive keyframe-request route (issue #1025).
    ///
    /// `route` is invoked on the main thread when the worker signals that it evicted a stale
    /// keyframe-less backlog for this decoder's stream (no buffered keyframe to resume from).
    /// The `PeerDecodeManager` supplies a closure that emits a `KEYFRAME_REQUEST` for this
    /// decoder's peer + media type — it is already bound to one (peer, stream), so the worker
    /// message carries no identity. Installing it here (rather than at construction) lets the
    /// manager build it once the transport send-packet callback, the local user id, and the
    /// peer's identity are all known. Replaces any previously-installed route; pass-through to
    /// the shared slot the decoder closure reads.
    ///
    /// The `route` receives the head-of-line backlog age (`head_age_ms`, issue #1479) that tripped
    /// the freshness deadline; the manager's route closure feeds it to the per-receiver
    /// cross-sender PLI budget as the staleness-priority key.
    pub fn set_keyframe_request_route(&self, route: Box<dyn Fn(f64)>) {
        *self.keyframe_request_route.borrow_mut() = Some(route);
    }

    /// Drop any installed keyframe-request route. The route closure captures a
    /// clone of the transport `send_packet` `Callback` (a strong `Rc` that
    /// reaches `Inner`), so it must be cleared on teardown alongside
    /// `send_packet` itself — otherwise it keeps `Inner` alive after the UI scope
    /// unmounts (the cc7tp/#502 `Rc`-cycle class). See
    /// [`PeerDecodeManager::clear_send_packet_callback`].
    pub fn clear_keyframe_request_route(&self) {
        *self.keyframe_request_route.borrow_mut() = None;
    }

    /// Test/observability helper: whether a keyframe-request route is currently
    /// installed. Used by the #1025 teardown regression test to assert that
    /// `clear_send_packet_callback` actually drops the route (breaking the
    /// `Rc`-cycle leg).
    #[doc(hidden)]
    pub fn has_keyframe_request_route(&self) -> bool {
        self.keyframe_request_route.borrow().is_some()
    }

    /// No-op decoder for unit tests — avoids requiring WebCodecs / worker link tags.
    #[cfg(test)]
    pub(crate) fn noop() -> Self {
        struct NoopDecoder;
        impl VideoFrameDecoder for NoopDecoder {
            fn push_frame(&self, _: FrameBuffer) {}
            fn is_waiting_for_keyframe(&self) -> bool {
                true
            }
            fn flush(&self) {}
        }
        Self {
            decoder: Box::new(NoopDecoder),
            canvas_renderer: Rc::new(RefCell::new(None)),
            media_type: MEDIA_TYPE_CAMERA,
            last_source_dims: RefCell::new((0, 0)),
            last_encoder_state: RefCell::new((0, String::new(), String::new())),
            stream_context: Rc::new(RefCell::new(None)),
            first_render_pending_ack: Rc::new(RefCell::new(false)),
            paint_enabled: Rc::new(Cell::new(true)),
            keyframe_request_route: Rc::new(RefCell::new(None)),
            latest_frame: Rc::new(RefCell::new(LatestFrameMailbox::new())),
            raf_scheduled: Rc::new(Cell::new(false)),
            raf_id: Rc::new(Cell::new(0)),
            _raf_paint_closure: Rc::new(RefCell::new(None)),
            painted_fps_interval_id: Rc::new(Cell::new(0)),
            _painted_fps_interval_closure: Rc::new(RefCell::new(None)),
        }
    }

    /// Test accessor for the #1183 late-frame paint gate. The async paint
    /// callback consults this exact flag, so a test that toggles it via
    /// `clear_canvas()` / `decode()` is exercising the real source of truth
    /// the callback reads — not a parallel copy.
    #[cfg(test)]
    pub(crate) fn paint_enabled_for_test(&self) -> bool {
        self.paint_enabled.get()
    }

    /// issue 508: test seam — overwrite the renderer's cached dimensions so a
    /// `#[wasm_bindgen_test]` can simulate a frame having sized the canvas
    /// without needing a real decoded `VideoFrame`. No-op when no renderer is
    /// wired. Mirrors the `paint_enabled_for_test` pattern.
    #[cfg(test)]
    pub(crate) fn set_renderer_dims_for_test(&self, w: u32, h: u32) {
        if let Some(renderer) = self.canvas_renderer.borrow_mut().as_mut() {
            renderer.last_width = w;
            renderer.last_height = h;
        }
    }

    /// issue 508: test seam — read back the renderer's cached `(last_width,
    /// last_height)`. Returns `None` when no renderer is wired.
    #[cfg(test)]
    pub(crate) fn renderer_dims_for_test(&self) -> Option<(u32, u32)> {
        self.canvas_renderer
            .borrow()
            .as_ref()
            .map(|r| (r.last_width, r.last_height))
    }

    /// issue #1640: test seam — read back the `(from_peer, to_peer)` pair most
    /// recently written by `set_stream_context`. Returns `None` when
    /// `set_stream_context` has not yet been called (e.g. noop decoder before
    /// any lifecycle point fires). Guards the call-site ID-type fix: if the
    /// production code is reverted to pass `userid` instead of
    /// `local_session_id`, the `from_peer` field will be an email string and
    /// the assertion in the regression test will fail.
    #[cfg(test)]
    pub(crate) fn stream_context_for_test(&self) -> Option<(String, String)> {
        self.stream_context.borrow().clone()
    }
}

impl PeerDecode for VideoPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        if let Some(video_metadata) = packet.video_metadata.as_ref() {
            // Surface publisher-side source dimensions (from
            // `MediaStreamTrack.getSettings()` on the encoder side) so the
            // UI can show Source vs Received and detect in-transit
            // downscaling. Dedupe by tracking the last-seen pair — without
            // this we'd flood the diag bus with one event per decoded frame.
            // Proto3 default-zero acts as "unknown": older publishers that
            // don't stamp the fields are skipped here.
            let src_w = video_metadata.source_width;
            let src_h = video_metadata.source_height;
            if src_w != 0 && src_h != 0 {
                let mut last = self.last_source_dims.borrow_mut();
                if *last != (src_w, src_h) {
                    *last = (src_w, src_h);
                    drop(last);
                    if let Some((from_peer, to_peer)) = self.stream_context.borrow().clone() {
                        let evt = DiagEvent {
                            subsystem: "video_source_resolution",
                            stream_id: None,
                            ts_ms: now_ms(),
                            metrics: vec![
                                metric!("source_width", src_w as u64),
                                metric!("source_height", src_h as u64),
                                metric!("from_peer", from_peer),
                                metric!("to_peer", to_peer),
                                metric!("media_type", self.media_type.to_string()),
                            ],
                        };
                        let _ = global_sender().try_broadcast(evt);
                    }
                }
            }

            // Issue #903: surface publisher-side encoder state so the UI
            // can render a `Cause:` line below the Screen row explaining
            // *why* the encoder downscaled. Only emitted for the screen
            // decoder (`media_type=SCREEN`); the camera path ignores these
            // fields today. We dedupe on the full `(bitrate, tier, hint)`
            // tuple so the diag bus only fires on actual change.
            //
            // Suppression rules:
            //   * `media_type != SCREEN` — only the screen decoder forwards.
            //   * All three values zero / empty — older publishers that
            //     don't stamp the fields; emitting would mislead the UI
            //     into rendering a Cause line with no data.
            if self.media_type == MEDIA_TYPE_SCREEN {
                let target_bitrate = video_metadata.encoder_target_bitrate_kbps;
                let adaptive_tier = video_metadata.adaptive_tier.as_str();
                let cause_hint = video_metadata.cause_hint.as_str();
                let any_present =
                    target_bitrate != 0 || !adaptive_tier.is_empty() || !cause_hint.is_empty();
                if any_present {
                    let mut last = self.last_encoder_state.borrow_mut();
                    let changed =
                        last.0 != target_bitrate || last.1 != adaptive_tier || last.2 != cause_hint;
                    if changed {
                        *last = (
                            target_bitrate,
                            adaptive_tier.to_string(),
                            cause_hint.to_string(),
                        );
                        drop(last);
                        if let Some((from_peer, to_peer)) = self.stream_context.borrow().clone() {
                            let evt = DiagEvent {
                                subsystem: "screen_encoder_state",
                                stream_id: None,
                                ts_ms: now_ms(),
                                metrics: vec![
                                    metric!("encoder_target_bitrate_kbps", target_bitrate as f64),
                                    metric!("adaptive_tier", adaptive_tier.to_string()),
                                    metric!("cause_hint", cause_hint.to_string()),
                                    metric!("from_peer", from_peer),
                                    metric!("to_peer", to_peer),
                                    metric!("media_type", self.media_type.to_string()),
                                ],
                            };
                            let _ = global_sender().try_broadcast(evt);
                        }
                    }
                }
            }

            // Convert protobuf VideoCodec to internal FrameCodec
            let frame_codec = match video_metadata.codec.enum_value() {
                Ok(VideoCodec::VP8) => FrameCodec::Vp8,
                Ok(VideoCodec::VP9_PROFILE0_LEVEL10_8BIT) => FrameCodec::Vp9Profile0Level10Bit8,
                Ok(VideoCodec::VIDEO_CODEC_UNSPECIFIED) | Err(_) => {
                    // Skip decoding for unknown codec (e.g., older clients)
                    log::warn!("Skipping video frame with unknown codec");
                    return Ok(DecodeStatus {
                        _rendered: false,
                        first_frame: false,
                    });
                }
            };

            let video_frame = CodecVideoFrame {
                sequence_number: video_metadata.sequence,
                timestamp: packet.timestamp,
                frame_type: self.get_frame_type(packet),
                codec: frame_codec,
                data: packet.data.clone(),
            };

            // Create a FrameBuffer and push it to the decoder
            let current_time_ms = web_time::SystemTime::now()
                .duration_since(web_time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis();

            let frame_buffer = FrameBuffer::new(video_frame, current_time_ms);

            // Issue #1183 late-frame race: re-enable painting before pushing.
            // Reaching here means `peer_decode_manager` did NOT take the
            // `!self.visible` SKIPPED path, i.e. the tile is back in the decode
            // set — so the frame this push produces (and any subsequent ones)
            // is wanted on the canvas again. Re-arming here (not on the
            // visibility edge) keeps the gate paired with the actual paint
            // pipeline: the flag is off from `clear_canvas()` until a real new
            // frame is on its way in.
            self.paint_enabled.set(true);

            // Use the new ergonomic API - decoder handles jitter buffer internally,
            // and calls our VideoFrame callback for rendering
            self.decoder.push_frame(frame_buffer);
        }

        // HCL #893: consume the async "first frame rendered" flag set by the
        // `on_video_frame` callback. The decode pipeline is async, so the
        // very first `push_frame` call returns here BEFORE the worker has
        // produced a `VideoFrame`. The flag will fire on a later `decode()`
        // call (typically the second or third packet for SCREEN, where the
        // worker has had time to decode the keyframe). When we observe the
        // flag we return `first_frame: true` exactly once, which lets
        // `peer_decode_manager` fire the `PEER_EVENT(screen_decode_started)`
        // ack to the publisher.
        let first_frame = consume_first_render_flag(&self.first_render_pending_ack);

        Ok(DecodeStatus {
            _rendered: true,
            first_frame,
        })
    }
}

impl Drop for VideoPeerDecoder {
    fn drop(&mut self) {
        // Issue #1783: tear down the presentation holder cleanly.
        //   1. Cancel any pending `requestAnimationFrame` paint BEFORE its closure is freed (the
        //      closure is a struct field dropped *after* this body runs), so the browser cannot
        //      invoke a freed callback.
        //   2. Close any frame still held in the mailbox — a `web_sys::VideoFrame` is not reclaimed
        //      promptly by GC, so an un-closed held frame leaks GPU memory.
        // Guarded to wasm: on the host test target these web-sys calls are unavailable (they would
        // abort), and there the mailbox is always empty and no rAF is ever scheduled anyway.
        #[cfg(target_arch = "wasm32")]
        {
            if self.raf_scheduled.get() {
                if let Some(win) = web_sys::window() {
                    let _ = win.cancel_animation_frame(self.raf_id.get());
                }
            }
            if let Some(frame) = self.latest_frame.borrow_mut().take() {
                frame.close();
            }
            // Issue #1784: clear the painted-fps sample interval BEFORE its closure field is freed,
            // so the browser cannot invoke a freed callback (mirrors the rAF teardown above).
            if self.painted_fps_interval_id.get() != 0 {
                if let Some(win) = web_sys::window() {
                    win.clear_interval_with_handle(self.painted_fps_interval_id.get());
                }
            }
        }
    }
}

/// HCL #893: consume the `first_render_pending_ack` flag, returning `true`
/// exactly once after the async render callback flips it.
///
/// Extracted from `VideoPeerDecoder::decode()` so the consume semantics
/// can be unit-tested without a real `WasmDecoder` (which would require
/// WebCodecs / a browser worker).
///
/// Invariants:
///   - First call after the flag is set: returns `true` and clears the flag.
///   - Subsequent calls (until the flag is set again): return `false`.
///   - Calls before the flag is ever set: return `false`.
///
/// A regression that "fixes" this to keep returning `true` on every call
/// would make `peer_decode_manager` fire `PEER_EVENT(screen_decode_started)`
/// on every SCREEN packet — a per-frame storm to the publisher. The
/// unit tests pin the exactly-once semantics.
fn consume_first_render_flag(flag: &Rc<RefCell<bool>>) -> bool {
    let mut guard = flag.borrow_mut();
    if *guard {
        *guard = false;
        true
    } else {
        false
    }
}

/// Decide the canvas drawing-buffer dimensions for a decoded frame, given the
/// frame's WebCodecs *display* dimensions (`display_width`, `display_height`).
///
/// The display dimensions already encode the intended presentation aspect —
/// post-crop, post-rotation, and corrected for any non-square sample aspect
/// ratio — so the canvas buffer is sized to them directly and the frame's full
/// visible source is then scaled to fill that buffer in
/// [`VideoPeerDecoder::render_to_canvas_cached`]. Keeping the buffer at the
/// display aspect (rather than the raw coded/visible aspect) is what makes the
/// CSS `object-fit: cover` tile scaling render the correct shape for *every*
/// peer, not just codec-aligned square-pixel ones.
///
/// A WebCodecs `VideoFrame` cannot have a zero-size display rect in practice,
/// but a defensive `(0, 0)` would zero the canvas buffer, turn the subsequent
/// `clear_rect` into a no-op and give `drawImage` an empty destination rect
/// (drawing nothing). Clamp each axis to a minimum of 1 so the render path
/// always has a valid, non-degenerate buffer.
///
/// Extracted as a pure function so the buffer-sizing rule is host-unit-testable
/// without a real `web_sys::VideoFrame` (which only exists under wasm).
fn canvas_buffer_dims(display_width: u32, display_height: u32) -> (u32, u32) {
    (display_width.max(1), display_height.max(1))
}

/// HCL #893: helper used by the `on_video_frame` callback the first time
/// the WasmDecoder hands a `VideoFrame` back to the render path. Flips
/// the shared `first_render_pending_ack` flag exactly once per decoder
/// lifetime; subsequent calls are no-ops.
///
/// Extracted so the "first call sets, later calls don't" behaviour is
/// covered by a unit test independent of the real WasmDecoder.
fn mark_first_render(fired: &Rc<RefCell<bool>>, ack: &Rc<RefCell<bool>>) {
    if !*fired.borrow() {
        *fired.borrow_mut() = true;
        *ack.borrow_mut() = true;
    }
}

///
/// AudioPeerDecoder
///
/// Plays audio to the standard audio stream.
///
/// This is important https://plnkr.co/edit/1yQd8ozGXlV9bwK6?preview
/// https://github.com/WebAudio/web-audio-api-v2/issues/133
pub struct StandardAudioPeerDecoder {
    pub decoder: AudioDecoderWrapper,
    decoded: bool,
    _error: Closure<dyn FnMut(JsValue)>, // member exists to keep the closure in scope for the life of the struct
    _output: Closure<dyn FnMut(AudioData)>, // member exists to keep the closure in scope for the life of the struct
    _audio_context: web_sys::AudioContext,  // Keep audio context alive
}

impl StandardAudioPeerDecoder {
    pub fn new(speaker_device_id: Option<String>) -> Result<Self, JsValue> {
        let error = Closure::wrap(Box::new(move |e: JsValue| {
            error!("{e:?}");
        }) as Box<dyn FnMut(JsValue)>);
        let audio_stream_generator =
            MediaStreamTrackGenerator::new(&MediaStreamTrackGeneratorInit::new("audio")).unwrap();
        // The audio context is used to reproduce audio.
        let audio_context =
            configure_audio_context(&audio_stream_generator, speaker_device_id).unwrap();

        let output = Closure::wrap(Box::new(move |audio_data: AudioData| {
            let writable = audio_stream_generator.writable();
            if writable.locked() {
                return;
            }
            if let Err(e) = writable.get_writer().map(|writer| {
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(e) = JsFuture::from(writer.ready()).await {
                        error!("write chunk error {e:?}");
                    }
                    if let Err(e) = JsFuture::from(writer.write_with_chunk(&audio_data)).await {
                        error!("write chunk error {e:?}");
                    };
                    writer.release_lock();
                });
            }) {
                error!("error {e:?}");
            }
        }) as Box<dyn FnMut(AudioData)>);
        let decoder = AudioDecoderWrapper::new(&AudioDecoderInit::new(
            error.as_ref().unchecked_ref(),
            output.as_ref().unchecked_ref(),
        ))?;
        decoder.configure(&AudioDecoderConfig::new(
            AUDIO_CODEC,
            AUDIO_CHANNELS,
            AUDIO_SAMPLE_RATE,
        ))?;
        Ok(Self {
            decoder,
            decoded: false,
            _error: error,
            _output: output,
            _audio_context: audio_context,
        })
    }
}

impl Drop for StandardAudioPeerDecoder {
    fn drop(&mut self) {
        if let Err(e) = self._audio_context.close() {
            error!("Error closing audio context: {e:?}");
        }
    }
}

impl PeerDecode for StandardAudioPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        let first_frame = !self.decoded;
        let current_state = self.decoder.state();
        log::debug!("Audio decoder state before decode: {current_state:?}");

        match current_state {
            CodecState::Configured => {
                log::debug!(
                    "Decoding audio packet with sequence: {}",
                    packet.audio_metadata.sequence
                );
                if let Err(e) = self.decoder.decode(packet.clone()) {
                    log::error!("Error decoding audio packet: {e:?}");
                    // Phase 1: This error will be caught and counted as a frame drop in peer_decode_manager
                    return Err(anyhow::anyhow!("Failed to decode audio packet"));
                }
                self.decoded = true;
                log::debug!(
                    "Audio packet decoded, new state: {:?}",
                    self.decoder.state()
                );
            }
            CodecState::Closed => {
                log::error!("Audio decoder closed unexpectedly");
                return Err(anyhow::anyhow!("decoder closed"));
            }
            CodecState::Unconfigured => {
                log::warn!("Audio decoder unconfigured, attempting to reconfigure");
                if let Err(e) = self.decoder.configure(&AudioDecoderConfig::new(
                    AUDIO_CODEC,
                    AUDIO_CHANNELS,
                    AUDIO_SAMPLE_RATE,
                )) {
                    log::error!("Failed to reconfigure audio decoder: {e:?}");
                    return Err(anyhow::anyhow!("Failed to reconfigure audio decoder"));
                }
            }
            _ => {
                log::warn!("Unexpected audio decoder state: {current_state:?}");
            }
        }

        Ok(DecodeStatus {
            _rendered: true,
            first_frame,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Camera path: canvas was attached first, so the renderer already carries
    /// a valid `(from_peer, to_peer)`. Even if `stream_context` is populated,
    /// we must keep the prior pair — overwriting it would erase a peer-id swap
    /// that landed via `set_stream_context` after the renderer was constructed.
    #[test]
    fn resolve_renderer_context_keeps_prior_pair_when_present() {
        let prior = Some((Some("alice".to_string()), Some("session-1".to_string())));
        let stream_ctx = ("bob".to_string(), "session-2".to_string());
        let (fp, tp) = resolve_renderer_context(prior, Some(&stream_ctx));
        assert_eq!(fp.as_deref(), Some("alice"));
        assert_eq!(tp.as_deref(), Some("session-1"));
    }

    /// Screen-share path: the first packet arrives before the dioxus
    /// `ScreenCanvas` tile mounts, so `set_stream_context` populates the
    /// decoder-level `stream_context` while the renderer is still absent. When
    /// the tile finally calls `set_canvas`, we have to seed the new renderer
    /// from `stream_context` — otherwise `render_to_canvas_cached`'s
    /// `video_resolution` broadcast stays gated on `to_peer.is_some()` and
    /// never fires. This is the #883 regression.
    #[test]
    fn resolve_renderer_context_seeds_from_stream_ctx_when_renderer_absent() {
        let stream_ctx = ("alice".to_string(), "session-1".to_string());
        let (fp, tp) = resolve_renderer_context(None, Some(&stream_ctx));
        assert_eq!(fp.as_deref(), Some("alice"));
        assert_eq!(tp.as_deref(), Some("session-1"));
    }

    /// Renderer existed but was created before `set_stream_context` ran (canvas
    /// passed at construction time, peer-id pair plumbed in later). Both
    /// fields are `None`, so we must fall back to `stream_context`.
    #[test]
    fn resolve_renderer_context_seeds_from_stream_ctx_when_prior_pair_empty() {
        let prior = Some((None, None));
        let stream_ctx = ("alice".to_string(), "session-1".to_string());
        let (fp, tp) = resolve_renderer_context(prior, Some(&stream_ctx));
        assert_eq!(fp.as_deref(), Some("alice"));
        assert_eq!(tp.as_deref(), Some("session-1"));
    }

    /// Neither source has data — return `(None, None)` so the renderer
    /// remains in an un-tagged state until `set_stream_context` runs.
    #[test]
    fn resolve_renderer_context_returns_none_when_both_empty() {
        let (fp, tp) = resolve_renderer_context(None, None);
        assert!(fp.is_none());
        assert!(tp.is_none());
    }

    /// Partial prior context (only `from_peer` or only `to_peer` known) is
    /// still preserved — never overwritten by `stream_context`. This avoids
    /// accidentally clobbering a half-set state during a canvas swap, which
    /// can happen if `set_canvas` is called twice in a row by Dioxus
    /// `use_effect` re-runs.
    #[test]
    fn resolve_renderer_context_preserves_partial_prior() {
        let prior = Some((Some("alice".to_string()), None));
        let stream_ctx = ("bob".to_string(), "session-2".to_string());
        let (fp, tp) = resolve_renderer_context(prior, Some(&stream_ctx));
        assert_eq!(fp.as_deref(), Some("alice"));
        assert!(tp.is_none());
    }

    // --- Aspect-ratio fix: `canvas_buffer_dims` --------------------------
    //
    // These pin the buffer-sizing rule that backs the per-peer "squashed
    // video" fix. The render path sizes the canvas to these dims and then
    // scales the frame's full visible source to fill it; if the buffer
    // carried the wrong aspect (e.g. coded/visible dims instead of display),
    // `object-fit: cover` would stretch the tile. The wasm `drawImage` call
    // itself can't be host-tested, so this isolates the host-testable math.

    /// Common 16:9 / 4:3 / portrait display sizes pass straight through —
    /// the canvas buffer must match the display dims exactly so the tile's
    /// aspect is correct. A regression that returned coded/aligned dims (e.g.
    /// padded the height to a 16px multiple) would change 720 -> 720 but
    /// would break a non-aligned size like 1080; both are checked.
    #[test]
    fn canvas_buffer_dims_passes_display_through() {
        assert_eq!(canvas_buffer_dims(1280, 720), (1280, 720)); // 16:9
        assert_eq!(canvas_buffer_dims(640, 480), (640, 480)); // 4:3
        assert_eq!(canvas_buffer_dims(720, 1280), (720, 1280)); // portrait 9:16
        assert_eq!(canvas_buffer_dims(1920, 1080), (1920, 1080)); // 1080 not 16-aligned
    }

    /// A non-square sample-aspect / cropped frame whose display dims are an
    /// arbitrary (not codec-aligned) size must still produce a buffer at that
    /// exact display aspect — this is the case that rendered squashed before
    /// the fix. 854x480 (the classic 16:9 480p with width not divisible by 16)
    /// must NOT be rounded to a coded 864x480.
    #[test]
    fn canvas_buffer_dims_preserves_unaligned_display_aspect() {
        assert_eq!(canvas_buffer_dims(854, 480), (854, 480));
        // Aspect must be preserved exactly, not snapped to a coded multiple.
        let (w, h) = canvas_buffer_dims(854, 480);
        assert_eq!(w, 854, "width must not be padded to a 16px multiple");
        assert_eq!(h, 480);
    }

    /// Degenerate `(0, 0)` is clamped to a valid 1x1 buffer so the render
    /// path never produces a zero-size canvas (which would no-op `clear_rect`
    /// and `drawImage`). Mutating the `.max(1)` to plain pass-through would
    /// return `(0, 0)` here and fail.
    #[test]
    fn canvas_buffer_dims_clamps_zero_to_one() {
        assert_eq!(canvas_buffer_dims(0, 0), (1, 1));
        assert_eq!(canvas_buffer_dims(1280, 0), (1280, 1));
        assert_eq!(canvas_buffer_dims(0, 720), (1, 720));
    }

    // --- HCL #893: `first_render_pending_ack` flag semantics --------------
    //
    // These tests pin down the exactly-once behaviour of the async
    // "first SCREEN frame rendered" signal. A regression that loosens
    // the helpers — e.g., "set the flag on every render" or "return
    // true on every decode" — would produce a per-frame PEER_EVENT
    // storm to the publisher and fail these tests.

    /// `consume_first_render_flag` returns `false` when the flag was
    /// never set — the no-op case the decoder hits on every packet
    /// before the worker has produced its first frame.
    #[test]
    fn consume_first_render_flag_returns_false_when_unset() {
        let flag = Rc::new(RefCell::new(false));
        assert!(!consume_first_render_flag(&flag));
        // Unchanged on the return — no side effect when there was
        // nothing to consume.
        assert!(!*flag.borrow());
    }

    /// `consume_first_render_flag` returns `true` exactly once after the
    /// flag is set, and clears the flag so subsequent calls return `false`.
    /// This is the LOAD-BEARING behaviour: it guarantees
    /// `peer_decode_manager` fires `PEER_EVENT(screen_decode_started)`
    /// exactly once per share, not on every SCREEN packet.
    #[test]
    fn consume_first_render_flag_returns_true_once_and_clears() {
        let flag = Rc::new(RefCell::new(true));
        assert!(
            consume_first_render_flag(&flag),
            "first call must observe the set flag and return true"
        );
        assert!(
            !*flag.borrow(),
            "flag must be cleared after consume so the next call \
             does NOT re-fire the publisher ack"
        );
        assert!(
            !consume_first_render_flag(&flag),
            "subsequent calls must return false until the render \
             callback sets the flag again"
        );
    }

    /// Multiple consume calls after a single flag-set must return
    /// `true` exactly once — defends against accidentally inverting
    /// the clear semantics inside `decode()`.
    #[test]
    fn consume_first_render_flag_is_exactly_once() {
        let flag = Rc::new(RefCell::new(true));
        let mut true_count = 0;
        for _ in 0..10 {
            if consume_first_render_flag(&flag) {
                true_count += 1;
            }
        }
        assert_eq!(
            true_count, 1,
            "exactly one consume call must observe the flag — got {true_count}"
        );
    }

    /// `mark_first_render` flips both flags on the first invocation but
    /// is a no-op on every subsequent call, even if the consumer side
    /// already cleared `ack`. This guarantees a SINGLE PEER_EVENT per
    /// VideoPeerDecoder lifetime.
    #[test]
    fn mark_first_render_fires_once_per_decoder() {
        let fired = Rc::new(RefCell::new(false));
        let ack = Rc::new(RefCell::new(false));

        mark_first_render(&fired, &ack);
        assert!(*fired.borrow(), "first call must set `fired`");
        assert!(*ack.borrow(), "first call must set `ack`");

        // Consumer side clears the ack (simulates `decode()` reading
        // the flag).
        *ack.borrow_mut() = false;

        // A subsequent render must NOT re-arm the ack — that would
        // cause `decode()` to return `first_frame: true` again and
        // fire a duplicate PEER_EVENT to the publisher.
        mark_first_render(&fired, &ack);
        assert!(
            !*ack.borrow(),
            "subsequent renders must not re-arm `ack` — would cause \
             a duplicate PEER_EVENT(screen_decode_started) per share"
        );
    }

    /// End-to-end: simulate the decoder loop. Decode N packets,
    /// have the async callback fire once between packet 2 and 3,
    /// confirm exactly one `decode()` returns `first_frame: true`.
    #[test]
    fn first_render_ack_round_trip() {
        let fired = Rc::new(RefCell::new(false));
        let ack = Rc::new(RefCell::new(false));

        // Packet 1: no render yet → false.
        assert!(!consume_first_render_flag(&ack));

        // Packet 2: still no render → false.
        assert!(!consume_first_render_flag(&ack));

        // Worker produces its first VideoFrame.
        mark_first_render(&fired, &ack);

        // Packet 3: consume the ack.
        assert!(
            consume_first_render_flag(&ack),
            "the first decode() call after the render callback fires \
             must return first_frame: true"
        );

        // Packets 4..N: never again.
        for _ in 0..5 {
            assert!(!consume_first_render_flag(&ack));
        }

        // Worker produces more frames — does NOT re-arm the ack.
        for _ in 0..3 {
            mark_first_render(&fired, &ack);
        }
        assert!(
            !consume_first_render_flag(&ack),
            "additional render callbacks must not produce a second \
             first_frame: true"
        );
    }

    // --- Issue #1183 late-frame race: paint gate toggle -------------------
    //
    // The async `on_video_frame` callback paints only when `paint_enabled` is
    // true. These tests pin the two edges that drive it: `clear_canvas()` (the
    // decode-stop edge) must disable painting so a frame whose callback lands
    // after the clear is dropped; the next `decode()` of a video packet (only
    // reached while visible) must re-enable it. They use `noop()`, whose
    // `NoopDecoder` never actually decodes — so the gate is the only state
    // under test — and `decode()` here is exercised on the host (a plain
    // `#[test]`, unlike the `#[wasm_bindgen_test]` cases that no-op in CI).

    /// Build a minimal VP8 video `MediaPacket` whose `video_metadata` carries a
    /// decodable codec, so `decode()` reaches the `push_frame` / paint re-enable
    /// path rather than the unknown-codec early return.
    fn minimal_video_packet() -> Arc<MediaPacket> {
        use videocall_types::protos::media_packet::media_packet::MediaType;
        use videocall_types::protos::media_packet::VideoMetadata;

        let mut pkt = MediaPacket {
            media_type: MediaType::VIDEO.into(),
            ..Default::default()
        };
        pkt.video_metadata = Some(VideoMetadata {
            sequence: 1,
            codec: VideoCodec::VP8.into(),
            ..Default::default()
        })
        .into();
        pkt.frame_type = "key".to_string();
        Arc::new(pkt)
    }

    #[test]
    fn paint_gate_starts_enabled() {
        let d = VideoPeerDecoder::noop();
        assert!(
            d.paint_enabled_for_test(),
            "a freshly-constructed decoder belongs to a visible tile, so \
             painting starts enabled"
        );
    }

    #[test]
    fn clear_canvas_disables_painting() {
        let d = VideoPeerDecoder::noop();
        d.clear_canvas();
        assert!(
            !d.paint_enabled_for_test(),
            "clear_canvas() (decode-stop edge) must disable painting so a \
             late async frame callback cannot repaint the wiped tile (#1183)"
        );
    }

    #[test]
    fn decode_reenables_painting_after_clear() {
        let mut d = VideoPeerDecoder::noop();
        d.clear_canvas();
        assert!(!d.paint_enabled_for_test(), "disabled by clear");
        d.decode(&minimal_video_packet())
            .expect("noop decode of a VP8 packet succeeds on host");
        assert!(
            d.paint_enabled_for_test(),
            "reaching decode() means the tile is visible again (the \
             manager's !visible guard returns SKIPPED before us), so the \
             next frame is wanted — painting must be re-enabled"
        );
    }

    // --- Issue #1784: painted-fps meter ----------------------------------
    //
    // These pin the SOURCE of the media-metrics overlay's "↓ fps": frames
    // ACTUALLY PAINTED, not packets ARRIVED. The meter is the host-testable
    // core `record_paint`/`sample` that the rAF paint site and the 1 Hz sample
    // timer drive in production.

    /// The canonical fails-on-unfixed guard (issue #1784 acceptance): a window in
    /// which packets ARRIVE but nothing is PAINTED (no `record_paint`) must sample
    /// to exactly 0.0 — the overlay's snap-down then reverts the readout to the
    /// em-dash. A window with real paints reports the painted rate. If the metric
    /// were sourced at arrival (the un-fixed behaviour), the no-paint window would
    /// read a positive fps and the first assertion would fail.
    #[test]
    fn paint_rate_meter_zero_without_paints_then_rate_with_paints() {
        let mut m = PaintRateMeter::new(0.0);
        // Arrival-only window: no paints recorded → 0 fps.
        assert_eq!(
            m.sample(1000.0),
            0.0,
            "a window with zero paints must sample to 0 fps (overlay → em-dash)"
        );
        // 30 frames painted over the next ~1 s window → 30 fps.
        for _ in 0..30 {
            m.record_paint();
        }
        let fps = m.sample(2000.0);
        assert!(
            (fps - 30.0).abs() < 1e-9,
            "30 paints over 1000ms must sample to 30 fps, got {fps}"
        );
    }

    /// A partial window normalizes to a per-second rate (so the 1 Hz timer is
    /// robust to slightly-off tick spacing): 15 paints over 500 ms → 30 fps.
    #[test]
    fn paint_rate_meter_normalizes_to_per_second() {
        let mut m = PaintRateMeter::new(0.0);
        for _ in 0..15 {
            m.record_paint();
        }
        let fps = m.sample(500.0);
        assert!(
            (fps - 30.0).abs() < 1e-9,
            "15 paints over 500ms must normalize to 30 fps, got {fps}"
        );
    }

    /// `sample` resets the window, so a subsequent empty window reads 0 — this is
    /// what makes a video that STOPS drop to the em-dash within one tick rather
    /// than latching the last painted rate.
    #[test]
    fn paint_rate_meter_resets_window_each_sample() {
        let mut m = PaintRateMeter::new(0.0);
        for _ in 0..10 {
            m.record_paint();
        }
        let _ = m.sample(1000.0);
        assert_eq!(
            m.sample(2000.0),
            0.0,
            "count must reset after sample → the next paint-free window is 0 fps"
        );
    }

    /// A non-positive elapsed span (two samples at the same instant — a defensive
    /// case a jittery timer could produce) returns 0 rather than dividing by zero.
    #[test]
    fn paint_rate_meter_guards_nonpositive_elapsed() {
        let mut m = PaintRateMeter::new(1000.0);
        m.record_paint();
        assert_eq!(
            m.sample(1000.0),
            0.0,
            "zero elapsed must return 0, not NaN/inf"
        );
    }

    /// Emission gating (issue #1784, perf): the sampler emits while painting, then
    /// only [`ZERO_EMIT_GRACE`] (2) trailing zeros after painting stops, then goes
    /// SILENT until a paint re-arms it. Mutation-sensitive:
    ///   * remove the trailing zeros (zero path → always `None`) → the `Some(0.0)`
    ///     asserts fail (breaks the overlay's snap-down-to-em-dash);
    ///   * remove the re-arm in `record_paint` → the budget is never armed, so NO
    ///     trailing zero ever emits and the first `Some(0.0)` assert fails (the
    ///     snap-down would never be delivered). The resume round below additionally
    ///     pins that a paint re-arms the budget for a SECOND stop.
    #[test]
    fn paint_rate_meter_emits_two_trailing_zeros_then_silent_until_repaint() {
        let mut m = PaintRateMeter::new(0.0);

        // Active painting: a window with paints emits the live rate.
        for _ in 0..30 {
            m.record_paint();
        }
        assert_eq!(
            m.sample_and_gate(1000.0),
            Some(30.0),
            "a painted window must emit the rate"
        );

        // Painting stops. The first two zero windows STILL emit 0 so the overlay
        // snaps down to the em-dash even if one bus message is dropped.
        assert_eq!(
            m.sample_and_gate(2000.0),
            Some(0.0),
            "1st post-stop zero must emit (snap-down)"
        );
        assert_eq!(
            m.sample_and_gate(3000.0),
            Some(0.0),
            "2nd post-stop zero must emit (grace)"
        );

        // Budget spent → SILENT: no more per-second zero broadcasts.
        assert_eq!(
            m.sample_and_gate(4000.0),
            None,
            "after the grace zeros the sampler must go silent"
        );
        assert_eq!(
            m.sample_and_gate(5000.0),
            None,
            "and stay silent while there are no paints"
        );

        // A paint RE-ARMS (event-driven resume): the next painted window emits its
        // rate, and the trailing-zero budget is reset for the next stop.
        for _ in 0..24 {
            m.record_paint();
        }
        assert_eq!(
            m.sample_and_gate(6000.0),
            Some(24.0),
            "a paint must resume emission with the live rate"
        );
        assert_eq!(
            m.sample_and_gate(7000.0),
            Some(0.0),
            "re-armed: trailing zeros must fire again after the second stop"
        );
        assert_eq!(
            m.sample_and_gate(8000.0),
            Some(0.0),
            "2nd re-armed trailing zero"
        );
        assert_eq!(
            m.sample_and_gate(9000.0),
            None,
            "silent again once the re-armed budget is spent"
        );
    }

    /// A FRESH meter is seeded with a full trailing-zero budget (issue #1784 lifecycle
    /// edge): its first `ZERO_EMIT_GRACE` ticks emit snap-down zeros — clearing any
    /// stale `fps_painted` value that survived a decoder replacement (reset/re-election)
    /// on the same peer_id — then it goes silent until the first paint. Fails if the
    /// constructor seed is reverted to `0` (the two `Some(0.0)` become `None`, so a
    /// stale overlay value would never be cleared when a replaced decoder never
    /// repaints).
    #[test]
    fn paint_rate_meter_fresh_meter_emits_snap_down_zeros_then_silent() {
        let mut m = PaintRateMeter::new(0.0);
        assert_eq!(
            m.sample_and_gate(1000.0),
            Some(0.0),
            "a fresh meter must emit a snap-down zero to clear a stale overlay value"
        );
        assert_eq!(
            m.sample_and_gate(2000.0),
            Some(0.0),
            "and a second (grace margin against a dropped bus message)"
        );
        assert_eq!(
            m.sample_and_gate(3000.0),
            None,
            "then go silent — no useless per-second zeros while never painting"
        );
    }

    /// The painted-fps event carries the painted rate + peer routing + media_type
    /// under the dedicated subsystem, so the overlay can source "↓ fps" from it and
    /// distinguish it from the arrival-rate `video`/`fps_received` event.
    #[test]
    fn build_painted_fps_event_shape() {
        let evt = build_painted_fps_event(
            "me-session".to_string(),
            "peer-session".to_string(),
            MEDIA_TYPE_CAMERA,
            28.0,
        );
        assert_eq!(evt.subsystem, SUBSYSTEM_VIDEO_PAINTED);
        assert!(evt.stream_id.is_none());
        let get = |name: &str| {
            evt.metrics
                .iter()
                .find(|m| m.name == name)
                .map(|m| &m.value)
        };
        match get(METRIC_FPS_PAINTED) {
            Some(MetricValue::F64(v)) => assert!((v - 28.0).abs() < 1e-9),
            other => panic!("expected fps_painted F64, got {other:?}"),
        }
        // `to_peer` is the SENDING peer id the UI's PeerTile matches against — it
        // must round-trip so the overlay routes the sample to the right tile.
        assert!(
            matches!(get("to_peer"), Some(MetricValue::Text(t)) if t.as_ref() == "peer-session"),
            "to_peer must carry the sending peer id"
        );
        // media_type must be the camera literal AND the zero-alloc borrowed form
        // (#1421): reverting to `.to_string()` yields `Cow::Owned` and fails this.
        assert!(
            matches!(
                get("media_type"),
                Some(MetricValue::Text(std::borrow::Cow::Borrowed("VIDEO")))
            ),
            "media_type must be a zero-alloc borrowed 'VIDEO' (text_static, not to_string)"
        );
    }
}

/// issue 508: regression test for the `set_canvas` idempotency guard.
///
/// SCOPE: this pins the DEFENSIVE same-node guard only (a redundant rebuild is
/// skipped when the identical `<canvas>` node is re-handed). It does NOT cover
/// the load-bearing issue 508 fix — the single-template unification in
/// `canvas_generator.rs` that keeps Dioxus reusing the same canvas node across a
/// peer-leave `full_bleed` flip. That node-reuse behaviour is exercised by the
/// `e2e/tests/peer-leave-canvas-stability.spec.ts` Playwright spec, since it
/// depends on the real Dioxus template diff and cannot be asserted from a unit
/// test on `VideoPeerDecoder` alone.
///
/// These tests require a real browser `document` (they create `<canvas>`
/// elements via `document.createElement`) and therefore run only under
/// `wasm-bindgen-test-runner` / `wasm-pack test --headless`. They compile
/// under `cargo check --target wasm32-unknown-unknown --tests` but are NOT
/// executed by `cargo test` on the host.
///
/// ONE-LINE MUTATION THAT MAKES THE TEST FAIL:
///   Delete the `if renderer.canvas == canvas { return Ok(()); }` block in
///   `set_canvas`. The second `set_canvas(canvas_a)` call will then rebuild
///   the renderer with `last_width: 0, last_height: 0`, so
///   `renderer_dims_for_test()` returns `Some((0, 0))` instead of
///   `Some((640, 480))` and the assertion panics.
#[cfg(test)]
mod wasm_canvas_tests {
    use super::*;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::wasm_bindgen_test;

    // DOM-touching tests (`document.createElement`) require a real browser
    // environment; the wasm-bindgen-test runner defaults to Node.js, which has
    // no `window`/`document`. Mirrors `peer_decode_manager.rs` et al.
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    fn make_canvas() -> HtmlCanvasElement {
        web_sys::window()
            .expect("no window")
            .document()
            .expect("no document")
            .create_element("canvas")
            .expect("create_element failed")
            .dyn_into::<HtmlCanvasElement>()
            .expect("not an HtmlCanvasElement")
    }

    /// issue 508: same-node re-hand must be a no-op — cached renderer dims
    /// (set by the test seam to simulate a sized frame) must survive a second
    /// `set_canvas` call with the identical `<canvas>` DOM node.
    #[wasm_bindgen_test]
    fn set_canvas_same_node_preserves_renderer_dims() {
        let decoder = VideoPeerDecoder::noop();

        // Wire the canvas for the first time (fresh renderer, dims are 0).
        let canvas_a = make_canvas();
        decoder
            .set_canvas(canvas_a.clone())
            .expect("initial set_canvas failed");
        assert_eq!(
            decoder.renderer_dims_for_test(),
            Some((0, 0)),
            "renderer dims start at zero after initial wiring"
        );

        // Simulate a frame having been decoded and the renderer sized.
        decoder.set_renderer_dims_for_test(640, 480);
        assert_eq!(
            decoder.renderer_dims_for_test(),
            Some((640, 480)),
            "test seam must write the sentinel dims"
        );

        // Re-hand the SAME canvas node (Dioxus re-running the mount effect on
        // peer-leave). The idempotency guard must short-circuit: dims preserved.
        decoder
            .set_canvas(canvas_a.clone())
            .expect("second set_canvas (same node) failed");
        assert_eq!(
            decoder.renderer_dims_for_test(),
            Some((640, 480)),
            "issue 508: same-node re-hand must NOT rebuild the renderer — \
             cached dims must be preserved"
        );

        // A DIFFERENT canvas node (real remount with a new element) must still
        // trigger a full rebuild, resetting dims to zero.
        let canvas_b = make_canvas();
        decoder
            .set_canvas(canvas_b)
            .expect("set_canvas with new node failed");
        assert_eq!(
            decoder.renderer_dims_for_test(),
            Some((0, 0)),
            "a genuinely new canvas node must rebuild the renderer (dims reset \
             to zero)"
        );
    }
}

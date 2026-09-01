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

//! The WASM decoder implementation using a Web Worker with internal JitterBuffer.

use super::{Decodable, DecodedFrame};
use crate::frame::FrameBuffer;
use crate::messages::{
    classify_worker_message_kind, FreshnessSkipMessage, KeyframeArrivalMessage,
    RequestKeyframeMessage, StreamContext, VideoStatsMessage, WorkerLogMessage, WorkerMessage,
    WorkerMessageKind, WorkerReadyMessage,
};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
#[cfg(feature = "wasm")]
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent, Metric, MetricValue};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{console, window, VideoFrame, Worker};

unsafe impl Send for WasmDecoder {}
unsafe impl Sync for WasmDecoder {}

pub struct WasmDecoder {
    worker: Worker,
    // The closure that handles messages from the worker.
    // We must store it to keep it alive.
    _on_message_closure: Closure<dyn FnMut(web_sys::MessageEvent)>,
    // Store the user's callback
    _on_decoded_frame: Box<dyn Fn(DecodedFrame)>,
    /// Mirrored onto the MAIN thread because the worker→main DiagEvent forward is dropped
    /// outright when the worker-supplied `to_peer` is empty (#2524).
    freshness_evictions: Rc<Cell<FreshnessEvictionAccum>>,
    gate: Rc<BootGate>,
    context_reemit: ContextReemitRamp,
}

impl Decodable for WasmDecoder {
    /// The decoded frame type for WASM decoding (now consistent with native).
    type Frame = DecodedFrame;

    fn new(
        _codec: crate::decoder::VideoCodec,
        on_decoded_frame: Box<dyn Fn(Self::Frame) + Send + Sync>,
    ) -> Self {
        log::info!("Creating WASM decoder with internal jitter buffer");
        // Issue #1641: this `Decodable::new` path is not used for real peer rendering (peer
        // decoders use `new_with_video_frame_callback`, which threads the owner's true media_type).
        // It still forwards the worker's "video" stats DiagEvent, which health_reporter buckets by
        // the `media_type` metric — so we must stamp *something*. "VIDEO" (the camera literal,
        // `MEDIA_TYPE_CAMERA`) is the safe default: there is no real screen-share decoder on this
        // path, so the camera bucket is correct.
        const DECODABLE_DEFAULT_MEDIA_TYPE: &str = "VIDEO";
        // Camera's `PERIODIC_KEYFRAME_MAX_INTERVAL_MS`, from the non-dependency `videocall-aq`.
        const DECODABLE_DEFAULT_BOOT_REPLAY_TTL_MS: f64 = 5000.0;
        // Find the worker script URL from the link tag added by Trunk.
        let worker_url = window()
            .expect("no window")
            .document()
            .expect("no document")
            .get_element_by_id("codecs-worker")
            .expect("worker link tag with id 'codecs-worker' not found")
            .get_attribute("href")
            .expect("worker link tag has no href attribute");

        // Create the worker.
        let worker = Worker::new(&worker_url).expect("Failed to create worker");

        // Convert the Send + Sync callback to a non-Send one for WASM
        let callback: Box<dyn Fn(DecodedFrame)> = unsafe { std::mem::transmute(on_decoded_frame) };

        let freshness_evictions = Rc::new(Cell::new(FreshnessEvictionAccum::default()));
        let gate = Rc::new(BootGate::new(DECODABLE_DEFAULT_BOOT_REPLAY_TTL_MS));

        // Create a closure to handle messages from the worker.
        let on_message_closure = {
            // We need to use Rc<RefCell<>> to share the callback since trait objects can't be cloned
            let callback_rc = Rc::new(RefCell::new(callback));
            let callback_for_closure = callback_rc.clone();
            // Stage-3 paint lag (issue #1252): mirror of the active render path's frame-drain count
            // + ACK. This Decodable path is not used for real rendering, but counting here keeps
            // the worker's emitted/painted accounting coherent if it ever is.
            let painted = Rc::new(Cell::new(0u64));
            let last_ack_ms = Rc::new(Cell::new(0f64));
            let ack_worker = worker.clone();
            let evictions_for_closure = freshness_evictions.clone();
            let gate_for_closure = gate.clone();

            Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
                let js_val = event.data();

                // Clone js_val before trying to convert it to avoid move issues
                match js_val.clone().dyn_into::<VideoFrame>() {
                    Ok(video_frame) => {
                        painted.set(painted.get().wrapping_add(1));
                        // Convert VideoFrame to DecodedFrame for consistency
                        let decoded_frame = DecodedFrame {
                            sequence_number: 0, // Note: sequence number tracking happens in jitter buffer
                            width: video_frame.display_width(),
                            height: video_frame.display_height(),
                            data: vec![], // For now, we don't copy the actual video data
                        };

                        // Call the callback through RefCell
                        if let Ok(cb) = callback_for_closure.try_borrow() {
                            cb(decoded_frame);
                        }
                        video_frame.close();
                        post_paint_progress(&ack_worker, &painted, &last_ack_ms, &gate_for_closure);
                    }
                    Err(_) => {
                        // Issue #1025: this `Decodable::new` path is not used for real peer
                        // rendering (peer decoders use `new_with_video_frame_callback`), so there
                        // is no proactive keyframe hook to fire here — but we still recognize the
                        // worker's RequestKeyframeMessage so it isn't logged as "unexpected". The
                        // carried `head_age_ms` (#1479) is ignored on this no-render path.
                        if !drain_on_worker_ready(&js_val, &ack_worker, &gate_for_closure)
                            && handle_worker_request_keyframe(&js_val).is_none()
                            && !handle_worker_diag_message(
                                &js_val,
                                DECODABLE_DEFAULT_MEDIA_TYPE,
                                &evictions_for_closure,
                            )
                        {
                            log::warn!("Received unexpected message from worker: {js_val:?}");
                        }
                    }
                }
            }) as Box<dyn FnMut(_)>)
        };

        worker.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));

        // Create a dummy callback for the struct field since the real one is in Rc<RefCell<>>
        let dummy_callback = Box::new(|_: DecodedFrame| {
            // The actual callback is handled through the Rc<RefCell<>> in the closure
        });

        WasmDecoder {
            worker,
            _on_message_closure: on_message_closure,
            _on_decoded_frame: dummy_callback,
            freshness_evictions,
            gate,
            context_reemit: ContextReemitRamp::default(),
        }
    }

    fn decode(&self, frame: FrameBuffer) {
        self.push_frame(frame, None);
    }
}

impl WasmDecoder {
    /// Create a WasmDecoder with VideoFrame callback for direct canvas rendering.
    ///
    /// `on_request_keyframe` (issue #1025) is invoked on the main thread whenever the
    /// worker posts a [`RequestKeyframeMessage`] — i.e. the worker's jitter buffer just
    /// evicted a stale keyframe-less backlog and wants a fresh keyframe fetched now. The
    /// owner (e.g. `VideoPeerDecoder`) supplies a closure that issues a `KEYFRAME_REQUEST`
    /// for this decoder's peer/stream. The closure receives the head-of-line backlog age
    /// (`head_age_ms`, issue #1479) that tripped the freshness deadline. Pass a no-op
    /// (`Box::new(|_| {})`) when no proactive keyframe path is wired.
    ///
    /// `media_type` (issue #1641) is the owner's stream kind — `"VIDEO"` for a camera decoder
    /// or `"SCREEN"` for a screen-share decoder (the `MEDIA_TYPE_CAMERA`/`MEDIA_TYPE_SCREEN`
    /// constants in `videocall-client`). The worker does NOT know which stream it decodes (its
    /// `SetContext` carries only peer IDs), so this main-thread re-broadcast is the only place
    /// the kind is known. It is stamped onto the worker's "video" stats DiagEvent (see
    /// [`handle_worker_diag_message`]) so `health_reporter` buckets the playout-family metrics
    /// (latency / paint-lag / skip-to-live / content-staleness) into the correct camera-vs-screen
    /// slot, mirroring how `emit_loss_metrics` already stamps its loss/keyframe metrics.
    ///
    /// `boot_replay_ttl_ms` (issue 2572): ONE publisher keyframe interval for the stream's kind.
    pub fn new_with_video_frame_callback(
        _codec: crate::decoder::VideoCodec,
        on_video_frame: Box<dyn Fn(VideoFrame)>,
        on_request_keyframe: Box<dyn Fn(f64)>,
        media_type: &'static str,
        boot_replay_ttl_ms: f64,
    ) -> Self {
        log::info!("Creating WASM decoder with VideoFrame callback");
        // Find the worker script URL from the link tag added by Trunk.
        let worker_url = web_sys::window()
            .expect("no window")
            .document()
            .expect("no document")
            .get_element_by_id("codecs-worker")
            .expect("worker link tag with id 'codecs-worker' not found")
            .get_attribute("href")
            .expect("worker link tag has no href attribute");

        // Create the worker.
        let worker = Worker::new(&worker_url).expect("Failed to create worker");

        let freshness_evictions = Rc::new(Cell::new(FreshnessEvictionAccum::default()));
        let gate = Rc::new(BootGate::new(boot_replay_ttl_ms));

        // Create a closure to handle messages from the worker.
        let on_message_closure = {
            let callback = on_video_frame;
            // Stage-3 paint lag (issue #1252): count every decoded VideoFrame this (main-thread)
            // closure drains from the worker->main postMessage queue — count the queue-drain, not
            // paint success, so a hidden tile (frame consumed but not actually painted) still
            // counts. The cumulative count is ACK'd back to the worker (which holds the un-delayed
            // emitted count) so it can compute emitted - painted at its 1Hz tick.
            let painted = Rc::new(Cell::new(0u64));
            let last_ack_ms = Rc::new(Cell::new(0f64));
            // Clone the worker into the closure so the ACK can be posted back upstream.
            let ack_worker = worker.clone();
            // Issue #1025: proactive keyframe-request callback. Moved into the closure (which is
            // stored on the struct and kept alive for the worker's lifetime) so it survives as
            // long as the decoder. Invoked when the worker posts a RequestKeyframeMessage.
            let request_keyframe = on_request_keyframe;
            let evictions_for_closure = freshness_evictions.clone();
            let gate_for_closure = gate.clone();
            Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
                let js_val = event.data();

                // Clone js_val before trying to convert it to avoid move issues
                match js_val.clone().dyn_into::<VideoFrame>() {
                    Ok(video_frame) => {
                        painted.set(painted.get().wrapping_add(1));
                        callback(video_frame);
                        post_paint_progress(&ack_worker, &painted, &last_ack_ms, &gate_for_closure);
                    }
                    Err(_) => {
                        // Worker->main serde messages: try the proactive keyframe-request signal
                        // (#1025) first, then the diagnostics stats message. Order is irrelevant
                        // (each gates on its own `kind`), but checking the keyframe request first
                        // keeps the recovery path off the (more frequent) stats path. The carried
                        // `head_age_ms` (#1479) is forwarded to the route so the main thread's PLI
                        // budget can prioritize the stalest stream.
                        if !drain_on_worker_ready(&js_val, &ack_worker, &gate_for_closure) {
                            if let Some(head_age_ms) = handle_worker_request_keyframe(&js_val) {
                                request_keyframe(head_age_ms);
                            } else if !handle_worker_diag_message(
                                &js_val,
                                media_type,
                                &evictions_for_closure,
                            ) {
                                log::warn!("Received unexpected message from worker: {js_val:?}");
                            }
                        }
                    }
                }
            }) as Box<dyn FnMut(_)>)
        };

        worker.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));

        // Create a dummy DecodedFrame callback since we need it for the trait
        let dummy_callback = Box::new(|_: DecodedFrame| {
            // This won't be called when using VideoFrame callback
        });

        WasmDecoder {
            worker,
            _on_message_closure: on_message_closure,
            _on_decoded_frame: dummy_callback,
            freshness_evictions,
            gate,
            context_reemit: ContextReemitRamp::default(),
        }
    }

    pub fn freshness_eviction_counts(&self) -> (u64, u64) {
        self.freshness_evictions.get().exported()
    }

    fn send(&self, msg: WorkerMessage) {
        self.gate.send(&self.worker, msg);
    }

    pub fn worker_handshake_seen(&self) -> bool {
        self.gate.handshake_seen.get()
    }

    pub fn boot_queue_dropped(&self) -> bool {
        self.gate.dropped.get()
    }

    pub fn boot_replay_ttl_ms(&self) -> f64 {
        self.gate.ttl_ms
    }

    pub fn boot_replay_max_messages(&self) -> usize {
        BOOT_REPLAY_MAX_MESSAGES
    }

    pub fn context_reemit_ramp_frames(&self) -> u32 {
        CONTEXT_REEMIT_RAMP_FRAMES
    }

    pub fn context_reemit_interval_ms(&self) -> f64 {
        CONTEXT_REEMIT_INTERVAL_MS
    }

    /// New ergonomic API: simply push a frame and let the decoder handle the rest
    pub fn push_frame(&self, frame: FrameBuffer, context: Option<StreamContext>) {
        self.post_frame(WorkerMessage::DecodeFrame(frame), context);
    }

    /// **Test-only** (issue #1022): post a crafted frame the worker will insert at the
    /// `arrival_time_ms` carried in the `FrameBuffer` (NOT the worker's wall clock the way
    /// [`push_frame`](Self::push_frame) does). With a back-dated arrival, an E2E spec can form a
    /// stale head-of-line backlog so the worker's ~10ms tick trips the #1020 freshness deadline
    /// and emits an observable `freshness_skip` (#1045). Only the `MOCK_PEERS_ENABLED`-gated
    /// injection hook (`videocall_client::freshness_inject`) calls this; production never does.
    pub fn inject_stale_frame(&self, frame: FrameBuffer, context: Option<StreamContext>) {
        self.post_frame(WorkerMessage::InjectStaleFrame(frame), context);
    }

    /// A frame that takes a re-emit slot has its attribution enqueued ahead of it (FIFO), but
    /// best-effort: a bound breach between the two sends posts the frame bare (issue 1741).
    fn post_frame(&self, message: WorkerMessage, context: Option<StreamContext>) {
        if let Some(context) = context {
            if self
                .context_reemit
                .take_reemit_slot(&context, js_sys::Date::now)
            {
                self.send_context(context);
            }
        }
        self.send(message);
    }

    /// Provide diagnostic context to the worker so that metrics include original peer IDs
    pub fn set_context(&self, from_peer: String, to_peer: String) {
        let context = StreamContext { from_peer, to_peer };
        self.context_reemit
            .open_epoch(&context, js_sys::Date::now());
        self.send_context(context);
    }

    fn send_context(&self, context: StreamContext) {
        let StreamContext { from_peer, to_peer } = context;
        self.send(WorkerMessage::SetContext { from_peer, to_peer });
    }

    /// Check if the decoder is waiting for a keyframe
    /// Note: This is now handled internally by the jitter buffer in the worker
    pub fn is_waiting_for_keyframe(&self) -> bool {
        // Since the jitter buffer is in the worker, we can't easily check this
        // For now, return false and let the worker handle keyframe logic
        false
    }

    /// Flush the internal decoder buffer
    pub fn flush(&self) {
        self.send(WorkerMessage::Flush);
    }

    /// Reset the decoder to initial state (waiting for keyframe)
    pub fn reset(&self) {
        self.send(WorkerMessage::Reset);
    }
}

impl Drop for WasmDecoder {
    fn drop(&mut self) {
        console::log_1(&"Terminating worker".into());
        self.worker.terminate();
    }
}

/// Throttled ACK of the cumulative number of decoded frames the main thread has drained from the
/// worker->main `postMessage` queue (issue #1252, stage-3 paint lag). Posts a
/// `WorkerMessage::PaintProgress` back to the worker at most every `PAINT_PROGRESS_ACK_INTERVAL_MS`
/// (≤2 msgs/s) so the worker — which holds the un-delayed `frames_emitted` count — can compute
/// `emitted - painted` at its 1Hz tick. Kept cheap; serialized with serde_wasm_bindgen, mirroring
/// [`WasmDecoder::push_frame`].
#[inline]
fn post_paint_progress(
    worker: &Worker,
    painted: &Cell<u64>,
    last_ack_ms: &Cell<f64>,
    gate: &BootGate,
) {
    const PAINT_PROGRESS_ACK_INTERVAL_MS: f64 = 500.0;
    let now = js_sys::Date::now();
    if now - last_ack_ms.get() < PAINT_PROGRESS_ACK_INTERVAL_MS {
        return;
    }
    last_ack_ms.set(now);
    gate.send(
        worker,
        WorkerMessage::PaintProgress {
            painted: painted.get(),
        },
    );
}

/// Frames from the start of a context epoch that each carry a `SetContext` re-emit.
const CONTEXT_REEMIT_RAMP_FRAMES: u32 = 30;
const CONTEXT_REEMIT_INTERVAL_MS: f64 = 1000.0;

/// An epoch is one distinct context VALUE; a differing value re-emits and restarts the ramp.
#[derive(Default)]
struct ContextReemitRamp {
    last_sent: RefCell<Option<StreamContext>>,
    frames_in_epoch: Cell<u32>,
    last_sent_ms: Cell<f64>,
}

impl ContextReemitRamp {
    /// `true` means the slot is ALREADY taken: deciding and stamping are one step, so a caller
    /// cannot advance `last_sent_ms` on a frame it does not send. `now` is read lazily.
    fn take_reemit_slot(&self, context: &StreamContext, now: impl FnOnce() -> f64) -> bool {
        // Bind, so the shared borrow is released before `open_epoch` takes a mutable one.
        let changed = self.last_sent.borrow().as_ref() != Some(context);
        if changed {
            self.open_epoch(context, now());
            return true;
        }
        let seen = self.frames_in_epoch.get();
        self.frames_in_epoch.set(seen.saturating_add(1));
        if seen < CONTEXT_REEMIT_RAMP_FRAMES {
            return true;
        }
        let now_ms = now();
        if now_ms - self.last_sent_ms.get() < CONTEXT_REEMIT_INTERVAL_MS {
            return false;
        }
        self.last_sent_ms.set(now_ms);
        true
    }

    /// Force-opens an epoch for the ungated `set_context` path.
    fn open_epoch(&self, context: &StreamContext, now_ms: f64) {
        *self.last_sent.borrow_mut() = Some(context.clone());
        self.frames_in_epoch.set(0);
        self.last_sent_ms.set(now_ms);
    }
}

/// Holds main->worker posts made before the worker's async `main()` installs `onmessage`, and
/// replays them on its handshake (issue 2572). Exceeding either bound DISCARDS the backlog rather
/// than flushing it: `insert_frame_to_jitter_buffer` re-stamps arrival with the worker's clock.
/// Counts MESSAGES, not frames — a frame that takes a re-emit slot costs two. Local on purpose:
/// aliasing the jitter buffer's frame capacity would let a retune there silently retune this.
const BOOT_REPLAY_MAX_MESSAGES: usize = 200;

struct BootGate {
    /// Set by the handshake OR by a bound; `handshake_seen` is what tells those two apart.
    ready: Cell<bool>,
    handshake_seen: Cell<bool>,
    dropped: Cell<bool>,
    pending: RefCell<VecDeque<WorkerMessage>>,
    /// `Date::now()` of the FIRST queued message, or `0.0` while nothing has been queued.
    first_queued_ms: Cell<f64>,
    ttl_ms: f64,
}

impl BootGate {
    fn new(ttl_ms: f64) -> Self {
        Self {
            ready: Cell::new(false),
            handshake_seen: Cell::new(false),
            dropped: Cell::new(false),
            pending: RefCell::new(VecDeque::new()),
            first_queued_ms: Cell::new(0.0),
            ttl_ms,
        }
    }

    fn over_bound(&self) -> bool {
        let first = self.first_queued_ms.get();
        (first != 0.0 && js_sys::Date::now() - first >= self.ttl_ms)
            || self.pending.borrow().len() >= BOOT_REPLAY_MAX_MESSAGES
    }

    fn discard(&self) {
        self.pending.borrow_mut().clear();
        self.dropped.set(true);
        self.ready.set(true);
    }

    fn send(&self, worker: &Worker, msg: WorkerMessage) {
        if !self.ready.get() && self.over_bound() {
            self.discard();
        }
        // `msg` survives the discard above; once queued it can still be dropped by a later one.
        if self.ready.get() {
            post_worker_message(worker, &msg);
            return;
        }
        if self.first_queued_ms.get() == 0.0 {
            self.first_queued_ms.set(js_sys::Date::now());
        }
        self.pending.borrow_mut().push_back(msg);
    }
}

fn post_worker_message(worker: &Worker, msg: &WorkerMessage) {
    let label = worker_message_label(msg);
    match serde_wasm_bindgen::to_value(msg) {
        Ok(js_message) => {
            if let Err(e) = worker.post_message(&js_message) {
                log::error!("Error posting {label} to worker: {e:?}");
            }
        }
        Err(e) => log::error!("Error serializing {label}: {e:?}"),
    }
}

fn worker_message_label(msg: &WorkerMessage) -> &'static str {
    match msg {
        WorkerMessage::DecodeFrame(_) => "message",
        WorkerMessage::Flush => "flush message",
        WorkerMessage::Reset => "reset message",
        WorkerMessage::SetContext { .. } => "context message",
        WorkerMessage::PaintProgress { .. } => "PaintProgress",
        WorkerMessage::InjectStaleFrame(_) => "inject-stale-frame message",
    }
}

/// Called from BOTH `onmessage` closures: a constructor that omitted it would queue forever.
/// The `handshake_seen` guard keeps a one-shot's `from_value` off every worker->main message.
fn drain_on_worker_ready(js_val: &JsValue, worker: &Worker, gate: &BootGate) -> bool {
    if gate.handshake_seen.get() {
        return false;
    }
    match serde_wasm_bindgen::from_value::<WorkerReadyMessage>(js_val.clone()) {
        Ok(msg) if classify_worker_message_kind(&msg.kind) == WorkerMessageKind::WorkerReady => {}
        _ => return false,
    }
    gate.handshake_seen.set(true);
    if gate.over_bound() {
        gate.discard();
    } else {
        loop {
            let Some(msg) = gate.pending.borrow_mut().pop_front() else {
                break;
            };
            post_worker_message(worker, &msg);
        }
    }
    gate.ready.set(true);
    true
}

/// Recognize the worker's proactive keyframe-request signal (issue #1025). Returns
/// `Some(head_age_ms)` if the posted value is a [`RequestKeyframeMessage`] (so the caller should
/// fire its keyframe callback, passing the carried head-of-line backlog age), `None` otherwise
/// (the caller falls through to the diagnostics parse).
///
/// The `head_age_ms` (issue #1479) is the backlog age that tripped the freshness deadline,
/// forwarded so the main thread's per-receiver cross-sender PLI budget can prioritize the stalest
/// stream when its global cap is reached. Old payloads that omit the field decode it as `0.0`
/// (`#[serde(default)]`), which the budget treats as the freshest possible request.
///
/// Both this and [`handle_worker_diag_message`] deserialize the same JS object shape via serde
/// and disambiguate on the `kind` field, mirroring the existing stats dispatch. We check the
/// discriminant explicitly so a `VideoStatsMessage` (whose extra fields are all `Option` and
/// would deserialize fine into this struct's subset) is NOT mistaken for a keyframe request.
fn handle_worker_request_keyframe(js_val: &JsValue) -> Option<f64> {
    match serde_wasm_bindgen::from_value::<RequestKeyframeMessage>(js_val.clone()) {
        Ok(msg)
            if classify_worker_message_kind(&msg.kind) == WorkerMessageKind::RequestKeyframe =>
        {
            log::debug!(
                "Proactive keyframe request from worker (#1025): from_peer={:?} to_peer={:?} head_age_ms={:.0}",
                msg.from_peer,
                msg.to_peer,
                msg.head_age_ms
            );
            Some(msg.head_age_ms)
        }
        _ => None,
    }
}

/// The worker's counters restart at 0 on a #1662 rebuild while this cell survives, so the
/// export keeps rising rather than showing a drop (#2524).
#[derive(Clone, Copy, Default)]
struct FreshnessEvictionAccum {
    base_total: u64,
    base_keyframeless: u64,
    last_raw_total: u64,
    last_raw_keyframeless: u64,
}

impl FreshnessEvictionAccum {
    fn exported(self) -> (u64, u64) {
        (
            self.base_total.saturating_add(self.last_raw_total),
            self.base_keyframeless
                .saturating_add(self.last_raw_keyframeless),
        )
    }
}

/// A reading below the previous one is a rebuild, never a reorder: both posts ride one FIFO
/// `postMessage` port. `total` alone decides it — the pair is ONE snapshot of ONE buffer.
fn accumulate_freshness_evictions(
    cell: &std::cell::Cell<FreshnessEvictionAccum>,
    total: u64,
    keyframeless: u64,
) {
    let mut acc = cell.get();
    if total < acc.last_raw_total {
        acc.base_total = acc.base_total.saturating_add(acc.last_raw_total);
        acc.base_keyframeless = acc
            .base_keyframeless
            .saturating_add(acc.last_raw_keyframeless);
    }
    acc.last_raw_total = total;
    acc.last_raw_keyframeless = keyframeless;
    cell.set(acc);
}

/// Diagnostics event for a peer-attributed worker message. `to_peer` is the per-peer key consumers
/// fold state under, so an absent or empty one yields `None` rather than a phantom entry;
/// `from_peer` is stamped only when non-empty.
#[cfg(feature = "wasm")]
fn peer_diag_event(
    subsystem: &'static str,
    from_peer: Option<String>,
    to_peer: Option<String>,
    mut metrics: Vec<Metric>,
) -> Option<DiagEvent> {
    let to_peer = to_peer.filter(|p| !p.is_empty())?;
    if let Some(from_peer) = from_peer.filter(|p| !p.is_empty()) {
        metrics.push(metric!("from_peer", from_peer));
    }
    metrics.push(metric!("to_peer", to_peer));
    Some(DiagEvent {
        subsystem,
        stream_id: None,
        ts_ms: now_ms(),
        metrics,
    })
}

/// Handle diagnostics objects posted by the worker. Returns true if handled.
///
/// `media_type` (issue #1641) is the owning decoder's stream kind (`"VIDEO"` / `"SCREEN"`),
/// stamped onto the re-broadcast "video" DiagEvents so `health_reporter` routes their metrics
/// into the correct camera-vs-screen bucket. The worker itself does not know its media_type
/// (its `SetContext` carries only peer IDs), so the value is supplied here on the main thread by
/// the owner (`VideoPeerDecoder`), the only place the kind is known. The `worker_log` branch is a
/// `"worker_log"` subsystem event that `health_reporter` does NOT camera/screen-bucket, so it is
/// intentionally left unstamped.
fn handle_worker_diag_message(
    js_val: &JsValue,
    media_type: &'static str,
    freshness_evictions: &std::cell::Cell<FreshnessEvictionAccum>,
) -> bool {
    // video_stats (issue #1252). A freshness_skip message ALSO deserializes into
    // VideoStatsMessage (its fields are all `Option`), so we must check `kind` and
    // fall through rather than treating a successful deserialize as a match.
    if let Ok(stats_msg) = serde_wasm_bindgen::from_value::<VideoStatsMessage>(js_val.clone()) {
        if classify_worker_message_kind(&stats_msg.kind) == WorkerMessageKind::VideoStats {
            // Gated on presence, not `unwrap_or(0)`: an older worker build that omits the
            // fields would otherwise read as a rebuild and inflate the banked base.
            if let (Some(total), Some(keyframeless)) = (
                stats_msg.freshness_evictions_total,
                stats_msg.freshness_evictions_keyframeless_total,
            ) {
                accumulate_freshness_evictions(freshness_evictions, total, keyframeless);
            }
            #[cfg(feature = "wasm")]
            {
                let evt = peer_diag_event(
                    "video",
                    stats_msg.from_peer,
                    stats_msg.to_peer,
                    vec![
                        // Issue #1641: stamp the owning decoder's stream kind so health_reporter
                        // routes the playout-family metrics below into the correct camera-vs-screen
                        // bucket. Without this, a peer's SCREEN-decoder worker stats landed in the
                        // CAMERA bucket (is_screen defaults false) and raced/overwrote it. The
                        // worker cannot supply this (it only knows peer IDs), so it is supplied here
                        // on the main thread, mirroring `emit_loss_metrics`'s media_type stamp.
                        // `media_type` is `&'static str`, so use the zero-alloc borrowed form
                        // (#1421) rather than `metric!`'s allocating `From<&str>` path.
                        Metric {
                            name: "media_type",
                            value: MetricValue::text_static(media_type),
                        },
                        metric!("frames_buffered", stats_msg.frames_buffered.unwrap_or(0)),
                        metric!(
                            "playout_latency_ms",
                            stats_msg.playout_latency_ms.unwrap_or(0.0)
                        ),
                        metric!(
                            "playout_stage1_span_ms",
                            stats_msg.playout_stage1_span_ms.unwrap_or(0.0)
                        ),
                        metric!(
                            "playout_paint_lag_ms",
                            stats_msg.playout_paint_lag_ms.unwrap_or(0.0)
                        ),
                        // Resync-to-live governor skip count (#1252): cumulative counter, default 0.
                        metric!(
                            "playout_skip_to_live_total",
                            stats_msg.playout_skip_to_live_total.unwrap_or(0)
                        ),
                        // Content-staleness (#1641): content AGE of the painted video, distinct
                        // from the paint-lag DEPTH above. This MAIN-THREAD re-broadcast is the
                        // load-bearing one for health_reporter — the worker's own in-process
                        // DiagEvent broadcast does not cross the worker→main boundary, so the
                        // field reaches the health packet only via this forward.
                        metric!(
                            "content_staleness_ms",
                            stats_msg.content_staleness_ms.unwrap_or(0.0)
                        ),
                        // Keyframe ARRIVALS (#2201): lifetime counter, default 0. Like
                        // content_staleness_ms above, THIS main-thread re-broadcast is the
                        // load-bearing one — the worker's in-process DiagEvent does not cross
                        // the worker→main boundary, so `keyframe_arrivals_total` reaches
                        // health_reporter (and Prometheus) only via this forward.
                        metric!(
                            "keyframe_arrivals_total",
                            stats_msg.keyframe_arrivals_total.unwrap_or(0)
                        ),
                    ],
                );
                if let Some(evt) = evt {
                    let _ = global_sender().try_broadcast(evt);
                }
            }
            return true;
        }
    }

    // keyframe_arrival (issue #2201): a keyframe ARRIVED at the receiver's jitter buffer.
    //
    // Same delivery reasoning as `freshness_skip` below: the load-bearing path to uploaded
    // field logs is the re-emitted `console` line, not the DiagEvent —
    // `console-log-collector.js` intercepts `log`/`warn`/`error`/`info`/`debug` alike, so every
    // level below reaches the upload buffer.
    //
    // LEVEL SPLIT — for GREPPABILITY, not for volume. Periodic keyframes are one per
    // `PERIODIC_KEYFRAME_MAX_INTERVAL_MS` (camera 5000ms) or
    // `SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS` (screen 3000ms) per stream — so a 20-peer
    // meeting with a share generates a few arrivals per second across all decoders, and
    // emitting all of them at WARN would dilute the freshness/escalation warnings this signal
    // exists to be correlated against.
    //
    // The split does NOT reduce log VOLUME: `console-log-collector.js` intercepts
    // `log`/`warn`/`error`/`info`/`debug` alike and calls `pushEntry` identically for every
    // one, so the level has zero effect on its line/byte budget or upload cadence. What
    // actually bounds the volume is the keyframe cadence itself — the periodic intervals
    // above, plus `PROACTIVE_KEYFRAME_REQUEST_MIN_INTERVAL_MS` on the receiver and
    // `FORCED_KEYFRAME_COOLDOWN_MS` on the publisher during a freeze.
    //
    // So, as issue #2201 itself proposes:
    //   * WARN  — the INTERESTING arrivals: one landing during a keyframe-less hold (the
    //             recovery datapoint) or one REJECTED as old/duplicate (arrived but cannot
    //             recover). These are rare and are the reason the signal exists.
    //   * DEBUG — routine healthy arrivals. Still captured by the collector, so the full
    //             arrival timeline is reconstructable from an uploaded log, without competing
    //             with the WARN band. The per-pair Prometheus counter covers the
    //             always-on/aggregate view independently of any log level.
    //
    // Checked BEFORE `freshness_skip`: this payload structurally satisfies that message's
    // required fields except `dropped`, so ordering plus the kind guards keep them apart (see
    // `keyframe_arrival_falls_through_overlapping_kinds` in `messages.rs`, which pins it).
    if let Ok(arrival_msg) =
        serde_wasm_bindgen::from_value::<KeyframeArrivalMessage>(js_val.clone())
    {
        if classify_worker_message_kind(&arrival_msg.kind) == WorkerMessageKind::KeyframeArrival {
            #[cfg(feature = "wasm")]
            {
                // Formatting lives in the pure, host-tested
                // `KeyframeArrivalMessage::console_line` (mirroring #1384's treatment of the
                // skip line) so a typo in the grep prefix or a dropped field token fails a
                // host test rather than shipping green. The same line text is emitted at both
                // levels — the grep contract does not depend on which branch ran.
                let line: wasm_bindgen::JsValue = arrival_msg.console_line().into();
                if arrival_msg.was_in_keyframe_less_hold
                    || arrival_msg.rejected_as_old
                    || arrival_msg.stream_restart
                {
                    console::warn_1(&line);
                } else {
                    console::debug_1(&line);
                }

                let evt = peer_diag_event(
                    "video",
                    arrival_msg.from_peer,
                    arrival_msg.to_peer,
                    vec![
                        Metric {
                            name: "event",
                            value: MetricValue::text_static("keyframe_arrival"),
                        },
                        // #1641: stamp the owner's stream kind so health_reporter buckets this
                        // `subsystem: "video"` event camera-vs-screen instead of defaulting a
                        // SCREEN decoder's arrival into the CAMERA bucket.
                        Metric {
                            name: "media_type",
                            value: MetricValue::text_static(media_type),
                        },
                        metric!("keyframe_seq", arrival_msg.seq),
                        metric!("head_age_ms", arrival_msg.head_age_ms),
                        // `MetricValue` has no bool variant; emit the flags as 1/0 u64,
                        // which is also the shape a Prometheus consumer wants.
                        metric!(
                            "was_in_keyframe_less_hold",
                            u64::from(arrival_msg.was_in_keyframe_less_hold)
                        ),
                        metric!("rejected_as_old", u64::from(arrival_msg.rejected_as_old)),
                        metric!("stream_restart", u64::from(arrival_msg.stream_restart)),
                    ],
                );
                if let Some(evt) = evt {
                    let _ = global_sender().try_broadcast(evt);
                }
            }
            return true;
        }
    }

    // freshness_skip (issue #1045): the #1020 freshness-deadline outcome, forwarded
    // from the worker so it lands in uploaded field logs.
    //
    // Delivery (issue #1045 follow-up): the upload pipeline captures the main thread's
    // `console.*` (see `console-log-collector.js`), so the load-bearing delivery is the
    // re-emitted `console` line below — NOT the DiagEvent. The DiagEvent broadcast goes only
    // to the in-process diagnostics bus, which has no console bridge: its `"video"` subsystem
    // is consumed by `health_reporter` (folded into the Prometheus health packet, where every
    // freshness field hits a catch-all and is dropped) and the diagnostics drawer (rendered to
    // the DOM, never uploaded) — so on its own it would NOT reach the field logs the issue
    // targets. This was the same gap fixed for `worker_log` in #1356/#1372; the skip path was
    // missed there and is corrected here. The DiagEvent is kept for any future structured
    // consumer, mirroring the other worker->main diagnostics. The `[JITTER_BUFFER]` prefix
    // matches the grep the field investigation already uses for this signal.
    if let Ok(skip_msg) = serde_wasm_bindgen::from_value::<FreshnessSkipMessage>(js_val.clone()) {
        if classify_worker_message_kind(&skip_msg.kind) == WorkerMessageKind::FreshnessSkip {
            // Ungated on `SetContext`, unlike the 1Hz stats tick that also carries these.
            if let (Some(total), Some(keyframeless)) = (
                skip_msg.freshness_evictions_total,
                skip_msg.freshness_evictions_keyframeless_total,
            ) {
                accumulate_freshness_evictions(freshness_evictions, total, keyframeless);
            }
            #[cfg(feature = "wasm")]
            {
                // A skip means the head-of-line frame aged past the playout deadline and stale
                // frames were evicted to recover — a real degradation signal, surfaced at WARN so
                // it stands out in field logs. This cannot amplify per tick: the worker only
                // surfaces a skip for forwarding at most ~once/sec/stream (the rate-limit +
                // coalescing in `JitterBuffer::record_freshness_skip`, gated on
                // `PROACTIVE_KEYFRAME_REQUEST_MIN_INTERVAL_MS`), so one console line maps to one
                // forwarded event, not one per eviction.
                //
                // The `[JITTER_BUFFER] freshness_skip` line is a grep contract the #1045/#1020
                // field investigation keys on; its formatting lives in the pure, host-tested
                // `FreshnessSkipMessage::console_line` (#1384) so a typo in the prefix or the
                // keyframe-`None` rendering fails a host test rather than shipping green.
                console::warn_1(&skip_msg.console_line().into());

                let evt = peer_diag_event(
                    "video",
                    skip_msg.from_peer,
                    skip_msg.to_peer,
                    vec![
                        // Static literal → zero-alloc borrow (#1421).
                        Metric {
                            name: "event",
                            value: MetricValue::text_static("freshness_skip"),
                        },
                        // Issue #1641: this is also a `subsystem: "video"` event, so
                        // health_reporter's video handler buckets it camera-vs-screen by
                        // `media_type` (and bumps that bucket's timestamp). Stamp the owner's kind
                        // here too — without it a SCREEN-decoder skip lands in the CAMERA bucket —
                        // for consistency with the video_stats stamp above. The worker cannot supply
                        // it (peer IDs only); it is the main thread's per-decoder static.
                        Metric {
                            name: "media_type",
                            value: MetricValue::text_static(media_type),
                        },
                        metric!("head_age_ms", skip_msg.head_age_ms),
                        // -1 marks the keyframe-less (held last-good) case, since the
                        // metric value is numeric and `keyframe_seq` is optional.
                        metric!(
                            "keyframe_seq",
                            skip_msg.keyframe_seq.map(|s| s as i64).unwrap_or(-1)
                        ),
                        metric!("dropped", skip_msg.dropped),
                        // Issue #1662: 1 marks the keyframe-less hold-ceiling escalation
                        // (decoder-pipeline reset), 0 a routine skip. Encoded as i64 to match the
                        // numeric-metric convention here (`keyframe_seq` above); lets a structured
                        // consumer (e.g. a future "reconnecting video" UI) key off the escalation
                        // without parsing the console line.
                        metric!("escalated", i64::from(skip_msg.escalated)),
                        // Issue #1851: wall-clock gap since the previous worker poll. Seconds-large
                        // means the decode worker's tick was starved and this skip is the resume
                        // poll; small means a normal-cadence skip. Lets a structured consumer
                        // distinguish a tick-starvation freeze from a delivery-starvation freeze
                        // without parsing the console line.
                        metric!("tick_gap_ms", skip_msg.tick_gap_ms),
                    ],
                );
                if let Some(evt) = evt {
                    let _ = global_sender().try_broadcast(evt);
                }
            }
            return true;
        }
    }

    // worker_log (issue #1356): a `log::` record emitted INSIDE the decoder worker, forwarded so
    // it lands in uploaded field logs (the worker's own `log`/`console` output is invisible to the
    // main-thread capture pipeline). Delivered by re-emitting a real main-thread `console` line
    // (what the upload pipeline hooks) tagged with the worker's peer context, plus a structured
    // DiagEvent for future consumers. NOTE on serde ordering: like the branches above we must
    // deserialize *then* check `kind`, because these worker messages share one JS-object channel
    // and their field sets overlap (a `RequestKeyframeMessage` is a structural subset, and a
    // `VideoStatsMessage`'s fields are all optional). `WorkerLogMessage`'s `level`/`target`/
    // `message` are required strings, so a stats/skip object will NOT deserialize into it — but we
    // still gate on `WORKER_LOG_KIND` so nothing can be misrouted in either direction.
    if let Ok(log_msg) = serde_wasm_bindgen::from_value::<WorkerLogMessage>(js_val.clone()) {
        if classify_worker_message_kind(&log_msg.kind) == WorkerMessageKind::WorkerLog {
            #[cfg(feature = "wasm")]
            {
                // Deliver into the console-log capture+upload pipeline (issue #1356). That pipeline
                // hooks the main thread's `console.*`, so the worker record MUST be re-emitted here
                // as a real console line — that is the load-bearing delivery. (The DiagEvent
                // broadcast below goes only to the in-process diagnostics bus, which has no console
                // bridge and no `worker_log` subscriber, so it would NOT reach the upload buffer on
                // its own; it is kept for any future structured consumer, mirroring the other
                // worker->main diagnostics.) Map the worker level onto the matching console method
                // so the captured line keeps its severity.
                let from = log_msg.from_peer.clone().unwrap_or_default();
                let to = log_msg.to_peer.clone().unwrap_or_default();
                let suppressed_note = if log_msg.suppressed > 0 {
                    format!(" (+{} suppressed)", log_msg.suppressed)
                } else {
                    String::new()
                };
                let line = format!(
                    "[worker {} {}] {}->{}: {}{}",
                    log_msg.level, log_msg.target, from, to, log_msg.message, suppressed_note
                );
                match log_msg.level.as_str() {
                    "ERROR" => console::error_1(&line.into()),
                    "WARN" => console::warn_1(&line.into()),
                    _ => console::log_1(&line.into()),
                }

                let evt = peer_diag_event(
                    "worker_log",
                    log_msg.from_peer,
                    log_msg.to_peer,
                    vec![
                        // Static literal → zero-alloc borrow (#1421).
                        Metric {
                            name: "event",
                            value: MetricValue::text_static("worker_log"),
                        },
                        metric!("level", log_msg.level),
                        metric!("target", log_msg.target),
                        metric!("message", log_msg.message),
                        // Records coalesced by the worker's rate limit since the last forwarded
                        // line (issue #1356); 0 on a normal line. Surfaces dropped volume without
                        // per-record network amplification.
                        metric!("suppressed", log_msg.suppressed),
                    ],
                );
                if let Some(evt) = evt {
                    let _ = global_sender().try_broadcast(evt);
                }
            }
            return true;
        }
    }

    // Not a recognized diagnostic message
    log::debug!("Received unexpected message from worker: {js_val:?}");
    false
}

#[cfg(all(test, feature = "wasm"))]
mod context_reemit_ramp_tests {
    use super::*;

    fn ctx(from: &str, to: &str) -> StreamContext {
        StreamContext {
            from_peer: from.to_string(),
            to_peer: to.to_string(),
        }
    }

    fn exhaust_ramp(ramp: &ContextReemitRamp, context: &StreamContext, now_ms: f64) {
        for i in 0..CONTEXT_REEMIT_RAMP_FRAMES {
            assert!(
                ramp.take_reemit_slot(context, || now_ms),
                "ramp frame {i} must re-emit; the ramp is shorter than it claims"
            );
        }
        assert!(
            !ramp.take_reemit_slot(context, || now_ms),
            "the ramp must end at exactly {CONTEXT_REEMIT_RAMP_FRAMES} frames"
        );
    }

    #[test]
    fn every_frame_of_the_opening_ramp_reemits_then_the_throttle_takes_over() {
        let ramp = ContextReemitRamp::default();
        let context = ctx("local", "peer");
        ramp.open_epoch(&context, 1000.0);
        // Every post shares one millisecond, so only the ramp can be what admits them.
        exhaust_ramp(&ramp, &context, 1000.0);
    }

    #[test]
    fn a_first_frame_with_an_unsent_context_opens_the_epoch_itself() {
        let ramp = ContextReemitRamp::default();
        let context = ctx("local", "peer");
        assert!(
            ramp.take_reemit_slot(&context, || 1000.0),
            "a context the worker was never told must re-emit on its first frame"
        );
        exhaust_ramp(&ramp, &context, 1000.0);
    }

    #[test]
    fn a_post_ramp_burst_inside_one_interval_reemits_exactly_once() {
        let ramp = ContextReemitRamp::default();
        let context = ctx("local", "peer");
        ramp.open_epoch(&context, 1000.0);
        exhaust_ramp(&ramp, &context, 1000.0);

        let burst_ms = 1000.0 + CONTEXT_REEMIT_INTERVAL_MS;
        let sent = (0..20)
            .filter(|i| ramp.take_reemit_slot(&context, || burst_ms + f64::from(*i)))
            .count();
        assert_eq!(
            sent, 1,
            "past the ramp, a burst inside one interval must re-emit exactly once"
        );
    }

    /// The 1Hz floor holds ACROSS intervals: one tick must not push the next one out.
    #[test]
    fn the_throttle_keeps_ticking_over_consecutive_intervals() {
        let ramp = ContextReemitRamp::default();
        let context = ctx("local", "peer");
        ramp.open_epoch(&context, 0.0);
        exhaust_ramp(&ramp, &context, 0.0);

        let mut sent = u32::from(ramp.take_reemit_slot(&context, || CONTEXT_REEMIT_INTERVAL_MS));
        let step = CONTEXT_REEMIT_INTERVAL_MS / 3.0;
        sent += (1..=5)
            .filter(|i| {
                ramp.take_reemit_slot(&context, || {
                    CONTEXT_REEMIT_INTERVAL_MS + step * f64::from(*i)
                })
            })
            .count() as u32;
        assert_eq!(
            sent, 2,
            "two intervals elapsed, so exactly two ticks must fire"
        );
    }

    #[test]
    fn a_post_ramp_frame_just_under_the_interval_is_still_throttled() {
        let ramp = ContextReemitRamp::default();
        let context = ctx("local", "peer");
        ramp.open_epoch(&context, 1000.0);
        exhaust_ramp(&ramp, &context, 1000.0);
        assert!(
            !ramp.take_reemit_slot(&context, || 1000.0 + CONTEXT_REEMIT_INTERVAL_MS - 1.0),
            "one millisecond short of the interval must not re-emit"
        );
        assert!(
            ramp.take_reemit_slot(&context, || 1000.0 + CONTEXT_REEMIT_INTERVAL_MS),
            "the interval boundary itself must re-emit"
        );
    }

    /// The SESSION_ASSIGNED case: `from_peer` fills in while the gate is throttled.
    #[test]
    fn a_changed_context_reemits_mid_throttle_and_restarts_the_ramp() {
        let ramp = ContextReemitRamp::default();
        let first = ctx("", "peer");
        ramp.open_epoch(&first, 1000.0);
        exhaust_ramp(&ramp, &first, 1000.0);

        let second = ctx("42", "peer");
        assert!(
            ramp.take_reemit_slot(&second, || 1000.0),
            "a changed context must re-emit without waiting for the interval"
        );
        exhaust_ramp(&ramp, &second, 1000.0);
    }
}

#[cfg(all(test, feature = "wasm"))]
mod peer_attribution_tests {
    use super::*;

    fn text(evt: &DiagEvent, name: &str) -> Option<String> {
        evt.metrics
            .iter()
            .find(|m| m.name == name)
            .and_then(|m| match &m.value {
                MetricValue::Text(s) => Some(s.to_string()),
                _ => None,
            })
    }

    /// Detecting per counter dropped the pre-rebuild keyframeless run whenever the subset did
    /// not also decrease — and that subset is the one this PR calls the FREEZE.
    #[test]
    fn a_rebuild_banks_the_keyframeless_run_even_when_it_did_not_decrease() {
        let cell = std::cell::Cell::new(FreshnessEvictionAccum::default());
        accumulate_freshness_evictions(&cell, 12, 1);
        assert_eq!(cell.get().exported(), (12, 1));

        accumulate_freshness_evictions(&cell, 2, 1);
        assert_eq!(
            cell.get().exported(),
            (14, 2),
            "the pre-rebuild keyframeless run must be banked off the TOTAL decrease"
        );
    }

    #[test]
    fn a_jitter_buffer_rebuild_is_absorbed_not_discarded() {
        let cell = std::cell::Cell::new(FreshnessEvictionAccum::default());
        accumulate_freshness_evictions(&cell, 12, 9);
        assert_eq!(cell.get().exported(), (12, 9));

        accumulate_freshness_evictions(&cell, 4, 0);
        assert_eq!(
            cell.get().exported(),
            (16, 9),
            "post-rebuild skips must ADD to the pre-rebuild run, not be dropped"
        );
        accumulate_freshness_evictions(&cell, 5, 1);
        assert_eq!(
            cell.get().exported(),
            (17, 10),
            "and keep advancing on the new run"
        );

        accumulate_freshness_evictions(&cell, 1, 0);
        assert_eq!(cell.get().exported(), (18, 10));
    }

    #[test]
    fn an_unidentified_target_peer_emits_nothing() {
        let cases = [
            (Some("alice"), None),
            (Some("alice"), Some("")),
            (None, None),
            (Some(""), Some("")),
        ];
        for (from_peer, to_peer) in cases {
            let evt = peer_diag_event(
                "video",
                from_peer.map(str::to_string),
                to_peer.map(str::to_string),
                vec![metric!("frames_buffered", 3u64)],
            );
            assert!(
                evt.is_none(),
                "from_peer={from_peer:?} to_peer={to_peer:?} must not be broadcast"
            );
        }
    }

    // Issue #1640: `SetContext` carries an empty local id until SESSION_ASSIGNED arrives.
    #[test]
    fn an_identified_target_peer_reports_without_a_source_peer() {
        for from_peer in [None, Some("")] {
            let evt = peer_diag_event(
                "video",
                from_peer.map(str::to_string),
                Some("bob".to_string()),
                vec![metric!("frames_buffered", 3u64)],
            )
            .expect("an identified target peer must be broadcast");
            assert_eq!(text(&evt, "to_peer").as_deref(), Some("bob"));
            assert_eq!(text(&evt, "from_peer"), None);
            assert_eq!(evt.metrics.len(), 2);
        }
    }

    #[test]
    fn a_complete_peer_pair_is_stamped_onto_the_event() {
        let evt = peer_diag_event(
            "video",
            Some("alice".to_string()),
            Some("bob".to_string()),
            vec![metric!("frames_buffered", 3u64)],
        )
        .expect("a complete peer pair must be broadcast");
        assert_eq!(evt.subsystem, "video");
        assert_eq!(text(&evt, "from_peer").as_deref(), Some("alice"));
        assert_eq!(text(&evt, "to_peer").as_deref(), Some("bob"));
        assert_eq!(evt.metrics.len(), 3, "caller metrics must be preserved");
    }

    #[test]
    fn peer_labels_are_constructed_at_one_site() {
        let src: String = include_str!("wasm.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        for label in ["from_peer", "to_peer"] {
            let count: usize = [format!("metric!(\"{label}\""), format!("name:\"{label}\"")]
                .iter()
                .map(|spelling| src.matches(spelling.as_str()).count())
                .sum();
            assert_eq!(
                count, 1,
                "expected exactly 1 {label} construction in this file, found {count}"
            );
        }
    }
}

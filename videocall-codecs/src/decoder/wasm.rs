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
    FreshnessSkipMessage, RequestKeyframeMessage, VideoStatsMessage, WorkerLogMessage,
    WorkerMessage, FRESHNESS_SKIP_KIND, REQUEST_KEYFRAME_KIND, WORKER_LOG_KIND,
};
#[cfg(feature = "wasm")]
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
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
}

impl Decodable for WasmDecoder {
    /// The decoded frame type for WASM decoding (now consistent with native).
    type Frame = DecodedFrame;

    fn new(
        _codec: crate::decoder::VideoCodec,
        on_decoded_frame: Box<dyn Fn(Self::Frame) + Send + Sync>,
    ) -> Self {
        log::info!("Creating WASM decoder with internal jitter buffer");
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

        // Create a closure to handle messages from the worker.
        let on_message_closure = {
            // We need to use Rc<RefCell<>> to share the callback since trait objects can't be cloned
            use std::cell::{Cell, RefCell};
            use std::rc::Rc;
            let callback_rc = Rc::new(RefCell::new(callback));
            let callback_for_closure = callback_rc.clone();
            // Stage-3 paint lag (issue #1252): mirror of the active render path's frame-drain count
            // + ACK. This Decodable path is not used for real rendering, but counting here keeps
            // the worker's emitted/painted accounting coherent if it ever is.
            let painted = Rc::new(Cell::new(0u64));
            let last_ack_ms = Rc::new(Cell::new(0f64));
            let ack_worker = worker.clone();

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
                        post_paint_progress(&ack_worker, &painted, &last_ack_ms);
                    }
                    Err(_) => {
                        // Issue #1025: this `Decodable::new` path is not used for real peer
                        // rendering (peer decoders use `new_with_video_frame_callback`), so there
                        // is no proactive keyframe hook to fire here — but we still recognize the
                        // worker's RequestKeyframeMessage so it isn't logged as "unexpected".
                        if !handle_worker_request_keyframe(&js_val)
                            && !handle_worker_diag_message(&js_val)
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
        }
    }

    fn decode(&self, frame: FrameBuffer) {
        self.push_frame(frame);
    }
}

impl WasmDecoder {
    /// Create a WasmDecoder with VideoFrame callback for direct canvas rendering.
    ///
    /// `on_request_keyframe` (issue #1025) is invoked on the main thread whenever the
    /// worker posts a [`RequestKeyframeMessage`] — i.e. the worker's jitter buffer just
    /// evicted a stale keyframe-less backlog and wants a fresh keyframe fetched now. The
    /// owner (e.g. `VideoPeerDecoder`) supplies a closure that issues a `KEYFRAME_REQUEST`
    /// for this decoder's peer/stream. Pass a no-op (`Box::new(|| {})`) when no proactive
    /// keyframe path is wired.
    pub fn new_with_video_frame_callback(
        _codec: crate::decoder::VideoCodec,
        on_video_frame: Box<dyn Fn(VideoFrame)>,
        on_request_keyframe: Box<dyn Fn()>,
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

        // Create a closure to handle messages from the worker.
        let on_message_closure = {
            use std::cell::Cell;
            use std::rc::Rc;
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
            Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
                let js_val = event.data();

                // Clone js_val before trying to convert it to avoid move issues
                match js_val.clone().dyn_into::<VideoFrame>() {
                    Ok(video_frame) => {
                        painted.set(painted.get().wrapping_add(1));
                        callback(video_frame);
                        post_paint_progress(&ack_worker, &painted, &last_ack_ms);
                    }
                    Err(_) => {
                        // Worker->main serde messages: try the proactive keyframe-request signal
                        // (#1025) first, then the diagnostics stats message. Order is irrelevant
                        // (each gates on its own `kind`), but checking the keyframe request first
                        // keeps the recovery path off the (more frequent) stats path.
                        if handle_worker_request_keyframe(&js_val) {
                            request_keyframe();
                        } else if !handle_worker_diag_message(&js_val) {
                            log::warn!("Received unexpected message from worker: {js_val:?}");
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
        }
    }

    /// New ergonomic API: simply push a frame and let the decoder handle the rest
    pub fn push_frame(&self, frame: FrameBuffer) {
        let message = WorkerMessage::DecodeFrame(frame);
        match serde_wasm_bindgen::to_value(&message) {
            Ok(js_message) => {
                if let Err(e) = self.worker.post_message(&js_message) {
                    log::error!("Error posting message to worker: {e:?}");
                }
            }
            Err(e) => {
                log::error!("Error serializing message: {e:?}");
            }
        }
    }

    /// **Test-only** (issue #1022): post a crafted frame the worker will insert at the
    /// `arrival_time_ms` carried in the `FrameBuffer` (NOT the worker's wall clock the way
    /// [`push_frame`](Self::push_frame) does). With a back-dated arrival, an E2E spec can form a
    /// stale head-of-line backlog so the worker's ~10ms tick trips the #1020 freshness deadline
    /// and emits an observable `freshness_skip` (#1045). Only the `MOCK_PEERS_ENABLED`-gated
    /// injection hook (`videocall_client::freshness_inject`) calls this; production never does.
    pub fn inject_stale_frame(&self, frame: FrameBuffer) {
        let message = WorkerMessage::InjectStaleFrame(frame);
        match serde_wasm_bindgen::to_value(&message) {
            Ok(js_message) => {
                if let Err(e) = self.worker.post_message(&js_message) {
                    log::error!("Error posting inject-stale-frame message to worker: {e:?}");
                }
            }
            Err(e) => {
                log::error!("Error serializing inject-stale-frame message: {e:?}");
            }
        }
    }

    /// Provide diagnostic context to the worker so that metrics include original peer IDs
    pub fn set_context(&self, from_peer: String, to_peer: String) {
        let message = WorkerMessage::SetContext { from_peer, to_peer };
        match serde_wasm_bindgen::to_value(&message) {
            Ok(js_message) => {
                if let Err(e) = self.worker.post_message(&js_message) {
                    log::error!("Error posting context message to worker: {e:?}");
                } else {
                    log::debug!("Sent context to worker");
                }
            }
            Err(e) => log::error!("Error serializing context message: {e:?}"),
        }
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
        let message = WorkerMessage::Flush;
        match serde_wasm_bindgen::to_value(&message) {
            Ok(js_message) => {
                if let Err(e) = self.worker.post_message(&js_message) {
                    log::error!("Error posting flush message to worker: {e:?}");
                } else {
                    log::debug!("Sent flush message to worker");
                }
            }
            Err(e) => {
                log::error!("Error serializing flush message: {e:?}");
            }
        }
    }

    /// Reset the decoder to initial state (waiting for keyframe)
    pub fn reset(&self) {
        let message = WorkerMessage::Reset;
        match serde_wasm_bindgen::to_value(&message) {
            Ok(js_message) => {
                if let Err(e) = self.worker.post_message(&js_message) {
                    log::error!("Error posting reset message to worker: {e:?}");
                } else {
                    log::debug!("Sent reset message to worker");
                }
            }
            Err(e) => {
                log::error!("Error serializing reset message: {e:?}");
            }
        }
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
    painted: &std::rc::Rc<std::cell::Cell<u64>>,
    last_ack_ms: &std::rc::Rc<std::cell::Cell<f64>>,
) {
    const PAINT_PROGRESS_ACK_INTERVAL_MS: f64 = 500.0;
    let now = js_sys::Date::now();
    if now - last_ack_ms.get() < PAINT_PROGRESS_ACK_INTERVAL_MS {
        return;
    }
    last_ack_ms.set(now);
    let message = WorkerMessage::PaintProgress {
        painted: painted.get(),
    };
    match serde_wasm_bindgen::to_value(&message) {
        Ok(js_message) => {
            if let Err(e) = worker.post_message(&js_message) {
                log::error!("Error posting PaintProgress to worker: {e:?}");
            }
        }
        Err(e) => log::error!("Error serializing PaintProgress: {e:?}"),
    }
}

/// Recognize the worker's proactive keyframe-request signal (issue #1025). Returns `true` if
/// the posted value is a [`RequestKeyframeMessage`] (so the caller should fire its keyframe
/// callback), `false` otherwise (the caller falls through to the diagnostics parse).
///
/// Both this and [`handle_worker_diag_message`] deserialize the same JS object shape via serde
/// and disambiguate on the `kind` field, mirroring the existing stats dispatch. We check the
/// discriminant explicitly so a `VideoStatsMessage` (whose extra fields are all `Option` and
/// would deserialize fine into this struct's subset) is NOT mistaken for a keyframe request.
fn handle_worker_request_keyframe(js_val: &JsValue) -> bool {
    match serde_wasm_bindgen::from_value::<RequestKeyframeMessage>(js_val.clone()) {
        Ok(msg) if msg.kind == REQUEST_KEYFRAME_KIND => {
            log::debug!(
                "Proactive keyframe request from worker (#1025): from_peer={:?} to_peer={:?}",
                msg.from_peer,
                msg.to_peer
            );
            true
        }
        _ => false,
    }
}

/// Handle diagnostics objects posted by the worker. Returns true if handled.
fn handle_worker_diag_message(js_val: &JsValue) -> bool {
    // video_stats (issue #1252). A freshness_skip message ALSO deserializes into
    // VideoStatsMessage (its fields are all `Option`), so we must check `kind` and
    // fall through rather than treating a successful deserialize as a match.
    if let Ok(stats_msg) = serde_wasm_bindgen::from_value::<VideoStatsMessage>(js_val.clone()) {
        if stats_msg.kind == "video_stats" {
            #[cfg(feature = "wasm")]
            {
                let evt = DiagEvent {
                    subsystem: "video",
                    stream_id: None,
                    ts_ms: now_ms(),
                    metrics: vec![
                        metric!("from_peer", stats_msg.from_peer.unwrap_or_default()),
                        metric!("to_peer", stats_msg.to_peer.unwrap_or_default()),
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
                    ],
                };
                let _ = global_sender().try_broadcast(evt);
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
        if skip_msg.kind == FRESHNESS_SKIP_KIND {
            #[cfg(feature = "wasm")]
            {
                let from = skip_msg.from_peer.clone().unwrap_or_default();
                let to = skip_msg.to_peer.clone().unwrap_or_default();
                // `None` keyframe_seq is the keyframe-less held (last-good) case; otherwise the
                // sequence we skipped forward to.
                let keyframe = skip_msg
                    .keyframe_seq
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "none (held last-good)".into());
                // A skip means the head-of-line frame aged past the playout deadline and stale
                // frames were evicted to recover — a real degradation signal, surfaced at WARN so
                // it stands out in field logs. This cannot amplify per tick: the worker only
                // surfaces a skip for forwarding at most ~once/sec/stream (the rate-limit +
                // coalescing in `JitterBuffer::record_freshness_skip`, gated on
                // `PROACTIVE_KEYFRAME_REQUEST_MIN_INTERVAL_MS`), so one console line maps to one
                // forwarded event, not one per eviction.
                console::warn_1(
                    &format!(
                        "[JITTER_BUFFER] freshness_skip {from}->{to}: head_age={:.0}ms dropped={} keyframe_seq={keyframe}",
                        skip_msg.head_age_ms, skip_msg.dropped
                    )
                    .into(),
                );

                let evt = DiagEvent {
                    subsystem: "video",
                    stream_id: None,
                    ts_ms: now_ms(),
                    metrics: vec![
                        metric!("event", "freshness_skip"),
                        metric!("from_peer", skip_msg.from_peer.unwrap_or_default()),
                        metric!("to_peer", skip_msg.to_peer.unwrap_or_default()),
                        metric!("head_age_ms", skip_msg.head_age_ms),
                        // -1 marks the keyframe-less (held last-good) case, since the
                        // metric value is numeric and `keyframe_seq` is optional.
                        metric!(
                            "keyframe_seq",
                            skip_msg.keyframe_seq.map(|s| s as i64).unwrap_or(-1)
                        ),
                        metric!("dropped", skip_msg.dropped),
                    ],
                };
                let _ = global_sender().try_broadcast(evt);
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
        if log_msg.kind == WORKER_LOG_KIND {
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

                let evt = DiagEvent {
                    subsystem: "worker_log",
                    stream_id: None,
                    ts_ms: now_ms(),
                    metrics: vec![
                        metric!("event", "worker_log"),
                        metric!("level", log_msg.level),
                        metric!("target", log_msg.target),
                        metric!("message", log_msg.message),
                        metric!("from_peer", log_msg.from_peer.unwrap_or_default()),
                        metric!("to_peer", log_msg.to_peer.unwrap_or_default()),
                        // Records coalesced by the worker's rate limit since the last forwarded
                        // line (issue #1356); 0 on a normal line. Surfaces dropped volume without
                        // per-record network amplification.
                        metric!("suppressed", log_msg.suppressed),
                    ],
                };
                let _ = global_sender().try_broadcast(evt);
            }
            return true;
        }
    }

    // Not a recognized diagnostic message
    log::debug!("Received unexpected message from worker: {js_val:?}");
    false
}

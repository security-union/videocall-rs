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

//! Main-thread half of the #2728 session Worker.

use crate::clock;
use crate::inbound::InboundLane;
use crate::webtransport::{WebTransportCloseInfo, WebTransportStatus};
use crate::worker_proto::{self, to_main, to_worker, TelemetryPush};
use futures::channel::oneshot;
use js_sys::{Array, ArrayBuffer, Reflect, Uint8Array};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use videocall_types::Callback;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{MessageEvent, WritableStream};

const WORKER_URL_ELEMENT_ID: &str = "wt-session-worker";
const WORKER_URL_FALLBACK: &str = "/wt_session_worker_loader.js";

struct MainTelemetry {
    fold: worker_proto::TelemetryFold,
    max_handoff_delay_ms: f64,
    frames_received: u64,
}

impl MainTelemetry {
    const fn new() -> Self {
        Self {
            fold: worker_proto::TelemetryFold::new(),
            max_handoff_delay_ms: 0.0,
            frames_received: 0,
        }
    }
}

static MAIN_TELEMETRY: StdMutex<MainTelemetry> = StdMutex::new(MainTelemetry::new());

static NEXT_SOURCE_ID: AtomicU64 = AtomicU64::new(1);

/// How many `WorkerSession`s are alive right now.
static LIVE_SESSIONS: AtomicU64 = AtomicU64::new(0);

/// True when this `start` is the one that should clear the session window.
pub fn is_cold_start(live_before: u64) -> bool {
    live_before == 0
}

fn with_telemetry<T>(f: impl FnOnce(&mut MainTelemetry) -> T, fallback: T) -> T {
    match MAIN_TELEMETRY.lock() {
        Ok(mut guard) => f(&mut guard),
        Err(_) => fallback,
    }
}

/// Drain the datagram read-loop window the Worker pushed; 0 when it is silent.
pub fn take_read_loop_max_gap_ms() -> f64 {
    with_telemetry(|t| t.fold.take_read_loop_max_gap_ms(), 0.0)
}

/// Drain the #2724 audio-lane reader window the Worker pushed.
pub fn take_audio_lane_max_gap_ms() -> f64 {
    with_telemetry(|t| t.fold.take_audio_lane_max_gap_ms(), 0.0)
}

pub fn incoming_queue_readback() -> Option<(f64, f64)> {
    with_telemetry(|t| t.fold.incoming_queue_readback, None)
}

pub fn inbound_unistream_reset_count() -> u64 {
    with_telemetry(|t| t.fold.inbound_unistream_reset_count.total(), 0)
}

pub fn send_order_fallback_count() -> u64 {
    with_telemetry(|t| t.fold.send_order_fallback_count.total(), 0)
}

/// Non-draining reads, for the diagnostics seam.
pub fn audio_lane_session_max_gap_ms() -> f64 {
    with_telemetry(|t| t.fold.audio_lane_session_max_gap_ms, 0.0)
}

pub fn max_handoff_delay_ms() -> f64 {
    with_telemetry(|t| t.max_handoff_delay_ms, 0.0)
}

pub fn frames_received() -> u64 {
    with_telemetry(|t| t.frames_received, 0)
}

pub fn inbox_shed_count() -> u64 {
    with_telemetry(|t| t.fold.inbox_shed_count.total(), 0)
}

pub fn reset_session_telemetry() {
    with_telemetry(
        |t| {
            t.fold.reset_session();
            t.max_handoff_delay_ms = 0.0;
            t.frames_received = 0;
        },
        (),
    );
}

#[cfg(test)]
fn apply_push_for_test(source: u64, push: worker_proto::TelemetryPush) {
    with_telemetry(|t| t.fold.apply(source, push), ());
}

#[cfg(test)]
fn record_frame_for_test(received_at_ms: f64, main_now_ms: f64) {
    record_frame(received_at_ms, main_now_ms);
}

fn record_frame(received_at_ms: f64, main_now_ms: f64) {
    let delay = main_now_ms - received_at_ms;
    with_telemetry(
        |t| {
            t.max_handoff_delay_ms = t.max_handoff_delay_ms.max(delay);
            t.frames_received = t.frames_received.saturating_add(1);
        },
        (),
    );
}

/// One inbound frame as main receives it.
pub struct WorkerFrame {
    pub bytes: Vec<u8>,
    pub lane: InboundLane,
    /// Already converted into main's `performance.now()` domain.
    pub received_at_ms: f64,
}

/// Callbacks the transport hands the session at construction.
pub struct WorkerSessionCallbacks {
    pub on_frame: Callback<WorkerFrame>,
    pub notification: Callback<WebTransportStatus>,
}

/// Holds messages until the Worker reports BOOTED, then releases them in order.
#[derive(Debug)]
pub struct BootQueue<M> {
    booted: Cell<bool>,
    outbox: RefCell<Vec<M>>,
}

impl<M> Default for BootQueue<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> BootQueue<M> {
    pub const fn new() -> Self {
        Self {
            booted: Cell::new(false),
            outbox: RefCell::new(Vec::new()),
        }
    }

    /// `Some(msg)` when it may be sent now, `None` when it was queued instead.
    pub fn admit(&self, msg: M) -> Option<M> {
        if self.booted.get() {
            return Some(msg);
        }
        self.outbox.borrow_mut().push(msg);
        None
    }

    /// Mark booted and take everything queued, oldest first.
    pub fn release(&self) -> Vec<M> {
        self.booted.set(true);
        self.outbox.borrow_mut().drain(..).collect()
    }

    pub fn booted(&self) -> bool {
        self.booted.get()
    }
}

struct Shared {
    datagram_writable: RefCell<Option<WritableStream>>,
    worker_time_origin_ms: Cell<f64>,
    pending_streams: RefCell<HashMap<u64, oneshot::Sender<Result<WritableStream, String>>>>,
    drained_bytes: Cell<f64>,
    frames_since_ack: Cell<u32>,
    closed_ack: Cell<bool>,
    /// Set once the Worker reported a close; `terminate` reads it (#2733).
    terminal_status_seen: Cell<bool>,
    boot_queue: BootQueue<(Array, Option<Array>)>,
    source_id: u64,
}

/// How one `CREATE_SEND_STREAM` round trip ended.
#[derive(Debug, PartialEq, Eq)]
enum StreamRequestOutcome<T> {
    Answered(Result<T, String>),
    SenderGone,
    TimedOut,
}

async fn await_stream_reply<T>(
    rx: oneshot::Receiver<Result<T, String>>,
    timeout_ms: u32,
) -> StreamRequestOutcome<T> {
    let timeout = gloo_timers::future::TimeoutFuture::new(timeout_ms);
    futures::pin_mut!(rx, timeout);
    match futures::future::select(rx, timeout).await {
        futures::future::Either::Left((Ok(result), _)) => StreamRequestOutcome::Answered(result),
        futures::future::Either::Left((Err(_), _)) => StreamRequestOutcome::SenderGone,
        futures::future::Either::Right(_) => StreamRequestOutcome::TimedOut,
    }
}

/// Pure: the caller's result, plus whether the pending entry must be reclaimed
/// — only a timeout leaves one behind, the other two are off the map already.
fn resolve_stream_request<T>(
    outcome: StreamRequestOutcome<T>,
    timeout_ms: u32,
) -> (Result<T, String>, bool) {
    match outcome {
        StreamRequestOutcome::Answered(result) => (result, false),
        StreamRequestOutcome::SenderGone => (
            Err("session worker closed before the stream was created".to_string()),
            false,
        ),
        StreamRequestOutcome::TimedOut => (
            Err(format!(
                "session worker did not answer the send-stream request within {timeout_ms}ms"
            )),
            true,
        ),
    }
}

fn status_kind_is_terminal(kind: u8) -> bool {
    kind != worker_proto::status_kind::OPENED
}

/// Pure: does terminating the Worker still owe main a status? This arm has no
/// the Worker already reported, or this is the idempotent re-call.
fn terminate_owes_status(terminal_status_seen: bool, already_terminated: bool) -> bool {
    !terminal_status_seen && !already_terminated
}

/// A live Worker running one WebTransport session.
pub struct WorkerSession {
    worker: web_sys::Worker,
    shared: Rc<Shared>,
    next_req_id: Cell<u64>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_error: Closure<dyn FnMut(web_sys::Event)>,
    _on_message_error: Closure<dyn FnMut(MessageEvent)>,
    terminated: Cell<bool>,
    notification: Callback<WebTransportStatus>,
}

impl WorkerSession {
    /// `url` passes through verbatim, so `DOWNLINK_STREAMS_QUERY` rides along;
    pub fn start(
        url: &str,
        cert_hashes: Vec<String>,
        callbacks: WorkerSessionCallbacks,
    ) -> Result<Rc<Self>, String> {
        if is_cold_start(LIVE_SESSIONS.load(Ordering::Relaxed)) {
            reset_session_telemetry();
            crate::inbound::reset_audio_lane_anchor();
        }
        let worker_url = resolve_worker_url();
        let worker = web_sys::Worker::new(&worker_url)
            .map_err(|e| format!("failed to start the WebTransport session worker: {e:?}"))?;

        LIVE_SESSIONS.fetch_add(1, Ordering::Relaxed);

        let shared = Rc::new(Shared {
            datagram_writable: RefCell::new(None),
            worker_time_origin_ms: Cell::new(clock::time_origin_ms()),
            pending_streams: RefCell::new(HashMap::new()),
            drained_bytes: Cell::new(0.0),
            frames_since_ack: Cell::new(0),
            closed_ack: Cell::new(false),
            terminal_status_seen: Cell::new(false),
            boot_queue: BootQueue::new(),
            source_id: NEXT_SOURCE_ID.fetch_add(1, Ordering::Relaxed),
        });

        let on_message = {
            let shared = shared.clone();
            let worker_for_ack = worker.clone();
            let on_frame = callbacks.on_frame.clone();
            let notification = callbacks.notification.clone();
            Closure::wrap(Box::new(move |event: MessageEvent| {
                handle_worker_message(
                    &shared,
                    &worker_for_ack,
                    &on_frame,
                    &notification,
                    event.data(),
                );
            }) as Box<dyn FnMut(MessageEvent)>)
        };
        worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let on_error = {
            let notification = callbacks.notification.clone();
            Closure::wrap(Box::new(move |event: web_sys::Event| {
                let detail = Reflect::get(&event, &JsValue::from_str("message"))
                    .ok()
                    .and_then(|v| v.as_string())
                    .unwrap_or_else(|| "worker error".to_string());
                notification.emit(WebTransportStatus::ClosedBeforeReady(format!(
                    "session worker error: {detail}"
                )));
            }) as Box<dyn FnMut(web_sys::Event)>)
        };
        worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));

        let on_message_error = {
            let notification = callbacks.notification.clone();
            Closure::wrap(Box::new(move |_: MessageEvent| {
                notification.emit(WebTransportStatus::ClosedAfterReady(
                    "session worker message could not be deserialised".to_string(),
                ));
            }) as Box<dyn FnMut(MessageEvent)>)
        };
        worker.set_onmessageerror(Some(on_message_error.as_ref().unchecked_ref()));

        let session = Rc::new(Self {
            worker,
            shared,
            next_req_id: Cell::new(1),
            _on_message: on_message,
            _on_error: on_error,
            _on_message_error: on_message_error,
            terminated: Cell::new(false),
            notification: callbacks.notification,
        });

        let hashes = Array::new();
        for hash in cert_hashes {
            hashes.push(&JsValue::from_str(&hash));
        }
        let init = Array::new();
        init.push(&JsValue::from_f64(f64::from(to_worker::INIT)));
        init.push(&JsValue::from_str(url));
        init.push(&hashes);
        post_to_worker(&session.shared, &session.worker, init, None)
            .map_err(|e| format!("failed to hand the session worker its init message: {e}"))?;
        Ok(session)
    }

    /// Transferred `datagrams.writable`, available once the Worker is READY.
    pub fn datagram_writable(&self) -> Option<WritableStream> {
        self.shared.datagram_writable.borrow().clone()
    }

    pub async fn create_send_stream(
        &self,
        stream_key: u8,
        send_order: i32,
    ) -> Result<WritableStream, String> {
        let req_id = self.next_req_id.get();
        self.next_req_id.set(req_id.wrapping_add(1));
        let (tx, rx) = oneshot::channel();
        self.shared.pending_streams.borrow_mut().insert(req_id, tx);

        let msg = Array::new();
        msg.push(&JsValue::from_f64(f64::from(to_worker::CREATE_SEND_STREAM)));
        msg.push(&JsValue::from_f64(req_id as f64));
        msg.push(&JsValue::from_f64(f64::from(stream_key)));
        msg.push(&JsValue::from_f64(f64::from(send_order)));
        if let Err(e) = post_to_worker(&self.shared, &self.worker, msg, None) {
            self.shared.pending_streams.borrow_mut().remove(&req_id);
            return Err(format!("send-stream request did not reach the worker: {e}"));
        }
        let (result, reclaim) = resolve_stream_request(
            await_stream_reply(rx, worker_proto::CREATE_SEND_STREAM_TIMEOUT_MS).await,
            worker_proto::CREATE_SEND_STREAM_TIMEOUT_MS,
        );
        if reclaim {
            self.shared.pending_streams.borrow_mut().remove(&req_id);
        }
        result
    }

    /// Teardown terminates the Worker regardless once the grace expires.
    pub fn request_close(&self) {
        let msg = Array::new();
        msg.push(&JsValue::from_f64(f64::from(to_worker::CLOSE)));
        let _ = post_to_worker(&self.shared, &self.worker, msg, None);
    }

    /// True once the Worker acknowledged the close.
    pub fn closed_ack(&self) -> bool {
        self.shared.closed_ack.get()
    }

    /// Unconditional, so a wedged Worker cannot outlive its task.
    pub fn terminate(&self) {
        let owes_status = terminate_owes_status(
            self.shared.terminal_status_seen.get(),
            self.terminated.get(),
        );
        if !self.terminated.replace(true) {
            let _ = LIVE_SESSIONS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(1))
            });
        }
        if owes_status {
            self.shared.terminal_status_seen.set(true);
            self.notification.emit(WebTransportStatus::ClosedAfterReady(
                "session worker terminated without reporting a close".to_string(),
            ));
        }
        self.worker.set_onmessage(None);
        self.worker.set_onerror(None);
        self.worker.set_onmessageerror(None);
        self.worker.terminate();
        self.shared.pending_streams.borrow_mut().clear();
    }
}

fn post_to_worker(
    shared: &Shared,
    worker: &web_sys::Worker,
    msg: Array,
    transfer: Option<Array>,
) -> Result<(), String> {
    let Some((msg, transfer)) = shared.boot_queue.admit((msg, transfer)) else {
        return Ok(());
    };
    let result = match &transfer {
        Some(list) => worker.post_message_with_transfer(&msg, list),
        None => worker.post_message(&msg),
    };
    result.map_err(|e| format!("{e:?}"))
}

fn flush_outbox(shared: &Shared, worker: &web_sys::Worker) {
    for (msg, transfer) in shared.boot_queue.release() {
        let result = match &transfer {
            Some(list) => worker.post_message_with_transfer(&msg, list),
            None => worker.post_message(&msg),
        };
        if let Err(e) = result {
            log::error!("session worker rejected a queued message: {e:?}");
        }
    }
}

fn resolve_worker_url() -> String {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(WORKER_URL_ELEMENT_ID))
        .and_then(|el| Reflect::get(&el, &JsValue::from_str("href")).ok())
        .and_then(|href| href.as_string())
        .unwrap_or_else(|| WORKER_URL_FALLBACK.to_string())
}

/// The `DRAINED` ack, stamped with MAIN's instant in the WORKER's clock domain.
fn drained_ack(shared: &Shared) -> Array {
    let ack = Array::new();
    ack.push(&JsValue::from_f64(f64::from(to_worker::DRAINED)));
    ack.push(&JsValue::from_f64(shared.drained_bytes.get()));
    ack.push(&JsValue::from_f64(clock::convert_to_domain(
        clock::now_ms(),
        clock::own_time_origin_ms(),
        shared.worker_time_origin_ms.get(),
    )));
    ack
}

fn tag_of(data: &JsValue) -> Option<(Array, u8)> {
    let array: Array = data.clone().dyn_into().ok()?;
    let tag = array.get(0).as_f64()? as u8;
    Some((array, tag))
}

fn handle_worker_message(
    shared: &Rc<Shared>,
    worker: &web_sys::Worker,
    on_frame: &Callback<WorkerFrame>,
    notification: &Callback<WebTransportStatus>,
    data: JsValue,
) {
    let Some((array, tag)) = tag_of(&data) else {
        log::warn!("session worker sent a message with no tag");
        return;
    };
    match tag {
        to_main::READY => {
            shared
                .worker_time_origin_ms
                .set(array.get(1).as_f64().unwrap_or_else(clock::time_origin_ms));
            if let Ok(writable) = array.get(2).dyn_into::<WritableStream>() {
                *shared.datagram_writable.borrow_mut() = Some(writable);
            }
        }
        to_main::FRAME => {
            let lane = worker_proto::lane_from_code(array.get(1).as_f64().unwrap_or(0.0) as u8);
            let worker_stamp = array.get(2).as_f64().unwrap_or(0.0);
            let Ok(buffer) = array.get(3).dyn_into::<ArrayBuffer>() else {
                return;
            };
            let bytes = Uint8Array::new(&buffer).to_vec();
            let len = bytes.len();
            let received_at_ms = clock::convert_to_domain(
                worker_stamp,
                shared.worker_time_origin_ms.get(),
                clock::own_time_origin_ms(),
            );
            record_frame(received_at_ms, clock::now_ms());
            shared
                .drained_bytes
                .set(shared.drained_bytes.get() + len as f64);
            let since = shared.frames_since_ack.get() + 1;
            if since >= worker_proto::ACK_EVERY_FRAMES {
                shared.frames_since_ack.set(0);
                let _ = post_to_worker(shared, worker, drained_ack(shared), None);
            } else {
                shared.frames_since_ack.set(since);
            }
            on_frame.emit(WorkerFrame {
                bytes,
                lane,
                received_at_ms,
            });
        }
        to_main::STATUS => {
            let kind = array.get(1).as_f64().unwrap_or(0.0) as u8;
            let message = array.get(2).as_string().unwrap_or_default();
            let code = array.get(3).as_f64().unwrap_or(0.0) as u32;
            if status_kind_is_terminal(kind) {
                shared.terminal_status_seen.set(true);
            }
            notification.emit(match kind {
                worker_proto::status_kind::OPENED => WebTransportStatus::Opened,
                worker_proto::status_kind::CLOSED_BEFORE_READY => {
                    WebTransportStatus::ClosedBeforeReady(message)
                }
                worker_proto::status_kind::CLOSED_AFTER_READY_WITH_CODE => {
                    WebTransportStatus::ClosedAfterReadyWithCode(WebTransportCloseInfo {
                        code,
                        reason: message,
                    })
                }
                _ => WebTransportStatus::ClosedAfterReady(message),
            });
        }
        to_main::SEND_STREAM_CREATED => {
            let req_id = array.get(1).as_f64().unwrap_or(0.0) as u64;
            let writable = array.get(2).dyn_into::<WritableStream>();
            if let Some(tx) = shared.pending_streams.borrow_mut().remove(&req_id) {
                let _ = tx.send(
                    writable
                        .map_err(|_| "worker sent a non-writable for a send stream".to_string()),
                );
            }
        }
        to_main::SEND_STREAM_FAILED => {
            let req_id = array.get(1).as_f64().unwrap_or(0.0) as u64;
            let message = array.get(2).as_string().unwrap_or_default();
            if let Some(tx) = shared.pending_streams.borrow_mut().remove(&req_id) {
                let _ = tx.send(Err(message));
            }
        }
        to_main::TELEMETRY => {
            let has_readback = array.get(4).as_f64().unwrap_or(0.0) != 0.0;
            let push = TelemetryPush {
                read_loop_max_gap_ms: array.get(1).as_f64().unwrap_or(0.0),
                audio_lane_max_gap_ms: array.get(2).as_f64().unwrap_or(0.0),
                incoming_queue_readback: has_readback.then(|| {
                    (
                        array.get(5).as_f64().unwrap_or(0.0),
                        array.get(6).as_f64().unwrap_or(0.0),
                    )
                }),
                inbound_unistream_reset_count: array.get(7).as_f64().unwrap_or(0.0) as u64,
                send_order_fallback_count: array.get(8).as_f64().unwrap_or(0.0) as u64,
                inbox_shed_count: array.get(9).as_f64().unwrap_or(0.0) as u64,
            };
            with_telemetry(|t| t.fold.apply(shared.source_id, push), ());
            let _ = post_to_worker(shared, worker, drained_ack(shared), None);
        }
        to_main::LOG => {
            let message = array.get(2).as_string().unwrap_or_default();
            match array.get(1).as_f64().unwrap_or(0.0) as u8 {
                1 => log::error!("[wt-worker] {message}"),
                2 => log::warn!("[wt-worker] {message}"),
                3 => log::info!("[wt-worker] {message}"),
                _ => log::debug!("[wt-worker] {message}"),
            }
        }
        to_main::BOOTED => flush_outbox(shared, worker),
        to_main::CLOSED => shared.closed_ack.set(true),
        other => log::warn!("session worker sent an unknown tag {other}"),
    }
}

/// The one lock every test that touches [`MAIN_TELEMETRY`] takes, wherever that
/// test lives.
#[cfg(test)]
pub(crate) static TELEMETRY_TEST_LOCK: StdMutex<()> = StdMutex::new(());

#[cfg(test)]
pub(crate) fn telemetry_test_lock() -> std::sync::MutexGuard<'static, ()> {
    TELEMETRY_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker_proto::TelemetryPush;

    fn serialised() -> std::sync::MutexGuard<'static, ()> {
        crate::worker_session::telemetry_test_lock()
    }

    #[test]
    fn an_unanswered_stream_request_errors_and_reclaims_its_slot() {
        let (result, reclaim) =
            resolve_stream_request(StreamRequestOutcome::<u8>::TimedOut, 10_000);
        assert!(reclaim, "only the timeout leaves a pending entry behind");
        assert!(result
            .unwrap_err()
            .contains("did not answer the send-stream request within 10000ms"));

        let (result, reclaim) =
            resolve_stream_request(StreamRequestOutcome::<u8>::SenderGone, 10_000);
        assert!(
            !reclaim,
            "teardown already cleared the map; touching it again is a second borrow"
        );
        assert!(result
            .unwrap_err()
            .contains("closed before the stream was created"));

        let (result, reclaim) =
            resolve_stream_request(StreamRequestOutcome::Answered(Ok(7u8)), 10_000);
        assert_eq!(
            (result, reclaim),
            (Ok(7u8), false),
            "a reply passes through"
        );

        let (result, reclaim) = resolve_stream_request(
            StreamRequestOutcome::<u8>::Answered(Err("worker said no".to_string())),
            10_000,
        );
        assert_eq!(
            (result, reclaim),
            (Err("worker said no".to_string()), false),
            "the Worker's own refusal must reach the caller verbatim"
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn the_stream_request_deadline_fires_on_a_worker_that_never_answers() {
        let (_tx, rx) = oneshot::channel::<Result<u8, String>>();
        assert_eq!(
            await_stream_reply(rx, 1).await,
            StreamRequestOutcome::TimedOut,
            "a live sender that never sends must not be awaited forever"
        );

        let (tx, rx) = oneshot::channel::<Result<u8, String>>();
        tx.send(Ok(9)).expect("the receiver is still live");
        assert_eq!(
            await_stream_reply(rx, worker_proto::CREATE_SEND_STREAM_TIMEOUT_MS).await,
            StreamRequestOutcome::Answered(Ok(9)),
            "anti-vacuity: a reply that lands inside the deadline still wins"
        );
    }

    #[test]
    fn terminating_the_worker_owes_main_a_status_unless_the_worker_sent_one() {
        assert!(
            terminate_owes_status(false, false),
            "a Worker killed without reporting a close leaves main with no \
             reconnect trigger at all"
        );
        assert!(
            !terminate_owes_status(true, false),
            "the Worker's own report is the better one; a second would be a \
             spurious trigger"
        );
        assert!(
            !terminate_owes_status(false, true),
            "terminate is idempotent, so the second call must stay silent"
        );
        assert!(!terminate_owes_status(true, true));
    }

    #[test]
    fn only_the_opened_status_leaves_the_session_still_running() {
        use crate::worker_proto::status_kind;
        assert!(
            !status_kind_is_terminal(status_kind::OPENED),
            "OPENED must not satisfy the debt terminate is checking"
        );
        for kind in [
            status_kind::CLOSED_BEFORE_READY,
            status_kind::CLOSED_AFTER_READY,
            status_kind::CLOSED_AFTER_READY_WITH_CODE,
        ] {
            assert!(
                status_kind_is_terminal(kind),
                "status kind {kind} is a close and must settle the debt"
            );
        }
    }

    #[test]
    fn only_a_cold_start_clears_the_session_window() {
        assert!(is_cold_start(0), "the first session of a page starts clean");
        for live in 1..=4 {
            assert!(
                !is_cold_start(live),
                "a candidate starting beside {live} live session(s) must not \
                 clear their window"
            );
        }
    }

    #[test]
    fn nothing_reaches_the_worker_before_it_reports_booted() {
        let queue: BootQueue<&str> = BootQueue::new();
        assert!(!queue.booted());
        assert_eq!(queue.admit("init"), None, "INIT must wait for BOOTED");
        assert_eq!(queue.admit("datagram"), None);
        assert_eq!(
            queue.release(),
            vec!["init", "datagram"],
            "and everything queued must arrive, oldest first"
        );
        assert!(queue.booted());
    }

    #[test]
    fn after_booted_a_message_is_posted_rather_than_queued() {
        let queue: BootQueue<u8> = BootQueue::new();
        let _ = queue.release();
        assert_eq!(queue.admit(7), Some(7));
        assert_eq!(
            queue.release(),
            Vec::<u8>::new(),
            "a post-BOOTED message must not sit in the outbox waiting for a \
             second release that never comes"
        );
    }

    #[test]
    fn releasing_twice_does_not_deliver_a_message_twice() {
        let queue: BootQueue<u8> = BootQueue::new();
        assert_eq!(queue.admit(1), None);
        assert_eq!(queue.release(), vec![1]);
        assert_eq!(queue.release(), Vec::<u8>::new());
    }

    #[test]
    fn the_main_fold_is_drained_by_main_and_reads_zero_when_the_worker_is_silent() {
        let _guard = serialised();
        reset_session_telemetry();

        assert_eq!(
            take_read_loop_max_gap_ms(),
            0.0,
            "no Worker has pushed, so the transport's unconditional fold must \
             report 0 and let the server gauge recover"
        );

        apply_push_for_test(
            1,
            TelemetryPush {
                read_loop_max_gap_ms: 41.0,
                audio_lane_max_gap_ms: 18.0,
                incoming_queue_readback: Some((2048.0, 3000.0)),
                inbound_unistream_reset_count: 3,
                send_order_fallback_count: 1,
                inbox_shed_count: 7,
            },
        );
        apply_push_for_test(
            1,
            TelemetryPush {
                read_loop_max_gap_ms: 9.0,
                audio_lane_max_gap_ms: 2980.0,
                inbound_unistream_reset_count: 5,
                ..Default::default()
            },
        );

        assert_eq!(
            take_read_loop_max_gap_ms(),
            41.0,
            "two pushes inside one reporting window fold with max, so a quiet \
             second push cannot erase the first's stall"
        );
        assert_eq!(
            take_read_loop_max_gap_ms(),
            0.0,
            "and the window drains, so the next health tick starts clean"
        );
        assert_eq!(take_audio_lane_max_gap_ms(), 2980.0);
        assert_eq!(
            audio_lane_session_max_gap_ms(),
            2980.0,
            "the session high-water the diagnostics seam reads survives the \
             drain a health tick performs"
        );
        assert_eq!(
            incoming_queue_readback(),
            Some((2048.0, 3000.0)),
            "a one-shot read-back is not a window and a later push without one \
             must not clear it"
        );
        assert_eq!(
            inbound_unistream_reset_count(),
            5,
            "each push carries the Worker's absolute total, so the reading \
             tracks it rather than summing the pushes"
        );
        assert_eq!(send_order_fallback_count(), 1);
        assert_eq!(inbox_shed_count(), 7);

        reset_session_telemetry();
        assert_eq!(
            audio_lane_session_max_gap_ms(),
            0.0,
            "a reconnect must not report the previous session's high-water"
        );
        assert_eq!(
            inbound_unistream_reset_count(),
            5,
            "but the page-lifetime total survives the reconnect, as it did \
             when the counter lived on the main thread"
        );
    }

    #[test]
    fn a_frame_received_before_main_drained_it_books_the_hand_off_delay() {
        let _guard = serialised();
        reset_session_telemetry();
        record_frame_for_test(1000.0, 4000.0);
        record_frame_for_test(3990.0, 4000.0);
        assert_eq!(
            max_handoff_delay_ms(),
            3000.0,
            "the high-water is the frame the Worker received during the stall, \
             not the fresh one that followed it"
        );
        assert_eq!(frames_received(), 2);
    }
}

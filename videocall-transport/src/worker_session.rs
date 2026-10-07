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

struct Spare {
    worker: web_sys::Worker,
    booted: Rc<Cell<bool>>,
    failed: Rc<Cell<bool>>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_error: Closure<dyn FnMut(web_sys::Event)>,
}

thread_local! {
    static SPARE: RefCell<Option<Spare>> = const { RefCell::new(None) };
    static WANTED: Cell<u32> = const { Cell::new(0) };
}

/// One holder's claim on the spare; the spare is discarded when the last held lease drops.
#[derive(Default)]
pub struct SpareLease {
    held: Cell<bool>,
}

impl SpareLease {
    pub fn acquire(&self) {
        if !self.held.replace(true) {
            WANTED.with(|wanted| wanted.set(wanted.get() + 1));
        }
    }

    pub fn held(&self) -> bool {
        self.held.get()
    }

    #[doc(hidden)]
    pub fn held_count() -> u32 {
        WANTED.with(Cell::get)
    }
}

impl Drop for SpareLease {
    fn drop(&mut self) {
        if !self.held.get() {
            return;
        }
        let remaining = WANTED.with(|wanted| {
            wanted.set(wanted.get().saturating_sub(1));
            wanted.get()
        });
        if remaining == 0 {
            if let Some((worker, _)) = take_spare() {
                worker.terminate();
            }
        }
    }
}

/// While a lease is held and no spare without a load error is waiting, boot one for the
/// next adopting `start`. Not counted in `LIVE_SESSIONS`.
pub fn prewarm() {
    if WANTED.with(Cell::get) == 0 {
        return;
    }
    let healthy = SPARE.with(|slot| slot.borrow().as_ref().map(|spare| !spare.failed.get()));
    match healthy {
        Some(true) => return,
        Some(false) => {
            let _ = take_spare();
        }
        None => {}
    }
    let worker = match web_sys::Worker::new(&resolve_worker_url()) {
        Ok(worker) => worker,
        Err(e) => {
            log::warn!("WT session worker prewarm failed: {e:?}");
            return;
        }
    };
    let booted = Rc::new(Cell::new(false));
    let failed = Rc::new(Cell::new(false));
    let on_message = {
        let booted = booted.clone();
        Closure::wrap(
            Box::new(move |event: MessageEvent| match tag_of(&event.data()) {
                Some((_, to_main::BOOTED)) => booted.set(true),
                Some((array, to_main::LOG)) => forward_worker_log(&array),
                _ => {}
            }) as Box<dyn FnMut(MessageEvent)>,
        )
    };
    let on_error = {
        let failed = failed.clone();
        Closure::wrap(Box::new(move |event: web_sys::Event| {
            log::warn!(
                "prewarmed WT session worker failed: {}",
                worker_error_detail(&event)
            );
            failed.set(true);
        }) as Box<dyn FnMut(web_sys::Event)>)
    };
    worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    SPARE.with(|slot| {
        *slot.borrow_mut() = Some(Spare {
            worker,
            booted,
            failed,
            _on_message: on_message,
            _on_error: on_error,
        })
    });
}

/// `Some((worker, booted))` when a healthy spare was waiting.
fn take_spare() -> Option<(web_sys::Worker, bool)> {
    let spare = SPARE.with(|slot| slot.borrow_mut().take())?;
    spare.worker.set_onmessage(None);
    spare.worker.set_onerror(None);
    if spare.failed.get() {
        spare.worker.terminate();
        return None;
    }
    Some((spare.worker, spare.booted.get()))
}

#[cfg(all(test, target_arch = "wasm32"))]
struct SpareView {
    worker: web_sys::Worker,
    booted: bool,
    failed: bool,
}

#[cfg(all(test, target_arch = "wasm32"))]
fn spare_for_test() -> Option<SpareView> {
    SPARE.with(|slot| {
        slot.borrow().as_ref().map(|spare| SpareView {
            worker: spare.worker.clone(),
            booted: spare.booted.get(),
            failed: spare.failed.get(),
        })
    })
}

fn worker_error_detail(event: &web_sys::Event) -> String {
    Reflect::get(event, &JsValue::from_str("message"))
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| "worker error".to_string())
}

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
        adopt_spare: bool,
    ) -> Result<Rc<Self>, String> {
        if is_cold_start(LIVE_SESSIONS.load(Ordering::Relaxed)) {
            reset_session_telemetry();
            crate::inbound::reset_audio_lane_anchor();
        }
        let spare = if adopt_spare { take_spare() } else { None };
        log::info!(
            "WT session worker start: spare={}",
            match spare {
                Some((_, true)) => "booted",
                Some((_, false)) => "booting",
                None if adopt_spare => "none",
                None => "skipped",
            }
        );
        let (worker, prebooted) = match spare {
            Some(spare) => spare,
            None => (
                web_sys::Worker::new(&resolve_worker_url()).map_err(|e| {
                    format!("failed to start the WebTransport session worker: {e:?}")
                })?,
                false,
            ),
        };

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
        if prebooted {
            let _ = shared.boot_queue.release();
        }

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
                notification.emit(WebTransportStatus::ClosedBeforeReady(format!(
                    "session worker error: {}",
                    worker_error_detail(&event)
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

fn forward_worker_log(array: &Array) {
    let message = array.get(2).as_string().unwrap_or_default();
    match array.get(1).as_f64().unwrap_or(0.0) as u8 {
        1 => log::error!("[wt-worker] {message}"),
        2 => log::warn!("[wt-worker] {message}"),
        3 => log::info!("[wt-worker] {message}"),
        _ => log::debug!("[wt-worker] {message}"),
    }
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
        to_main::LOG => forward_worker_log(&array),
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

#[cfg(all(test, target_arch = "wasm32"))]
mod prewarm_tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    // Stands in for the real Worker: it ignores everything until it posts
    // BOOTED, answers INIT with STATUS(OPENED), and "ping" with "pong".
    #[wasm_bindgen(inline_js = r#"
function install(src) {
  let link = document.getElementById("wt-session-worker");
  if (!link) {
    link = document.createElement("link");
    link.id = "wt-session-worker";
    document.head.appendChild(link);
  }
  link.href = URL.createObjectURL(new Blob([src], { type: "text/javascript" }));
}
export function install_fake_session_worker(boot_delay_ms) {
  install(`setTimeout(() => {
    onmessage = (e) => {
      if (e.data === "ping") postMessage("pong");
      else if (e.data[0] === 0) postMessage([1, 0, "", 0]);
    };
    postMessage([8]);
  }, ${boot_delay_ms});`);
}
export function install_failing_session_worker() {
  install(`throw new Error("fake worker failed to load");`);
}
export function install_shipped_loader_with_missing_wasm(loader) {
  const base = JSON.stringify(new URL("/missing-2988/", location.href).href);
  install(`self.importScripts = () => {
    self.wasm_bindgen = (p) => fetch(new URL(p, ${base})).then(WebAssembly.instantiateStreaming);
  };
${loader}`);
}
export function answers_ping(worker, timeout_ms) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve(false), timeout_ms);
    worker.addEventListener("message", (e) => {
      if (e.data === "pong") { clearTimeout(timer); resolve(true); }
    });
    worker.postMessage("ping");
  });
}
"#)]
    extern "C" {
        fn install_fake_session_worker(boot_delay_ms: u32);
        fn install_failing_session_worker();
        fn install_shipped_loader_with_missing_wasm(loader: &str);
        fn answers_ping(worker: &web_sys::Worker, timeout_ms: u32) -> js_sys::Promise;
    }

    async fn pings(worker: &web_sys::Worker) -> bool {
        wasm_bindgen_futures::JsFuture::from(answers_ping(worker, 500))
            .await
            .map(|v| v.as_bool() == Some(true))
            .unwrap_or(false)
    }

    fn held_lease() -> SpareLease {
        let lease = SpareLease::default();
        lease.acquire();
        lease
    }

    type Seen = Rc<RefCell<Vec<WebTransportStatus>>>;

    fn start_recording() -> (Rc<WorkerSession>, Seen) {
        start_recording_adopting(true)
    }

    fn start_recording_adopting(adopt_spare: bool) -> (Rc<WorkerSession>, Seen) {
        let seen: Seen = Rc::new(RefCell::new(Vec::new()));
        let sink = seen.clone();
        let session = WorkerSession::start(
            "https://relay.invalid/lobby",
            Vec::new(),
            WorkerSessionCallbacks {
                on_frame: Callback::from(|_: WorkerFrame| {}),
                notification: Callback::from(move |s| sink.borrow_mut().push(s)),
            },
            adopt_spare,
        )
        .expect("start");
        (session, seen)
    }

    async fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        for _ in 0..100 {
            if done() {
                return true;
            }
            gloo_timers::future::TimeoutFuture::new(20).await;
        }
        done()
    }

    fn opened(seen: &Seen) -> impl FnMut() -> bool + '_ {
        move || seen.borrow().contains(&WebTransportStatus::Opened)
    }

    #[wasm_bindgen_test]
    async fn a_spare_that_booted_before_start_is_adopted_and_receives_init() {
        install_fake_session_worker(0);
        let lease = held_lease();
        let live_before = LIVE_SESSIONS.load(Ordering::Relaxed);
        prewarm();
        assert_eq!(
            LIVE_SESSIONS.load(Ordering::Relaxed),
            live_before,
            "the spare is not a session"
        );
        assert!(wait_until(|| spare_for_test().is_some_and(|s| s.booted)).await);
        let spare_worker = spare_for_test().expect("spare").worker;

        let (session, seen) = start_recording();
        assert!(spare_for_test().is_none(), "start consumes the spare");
        assert!(js_sys::Object::is(&session.worker, &spare_worker));
        assert_eq!(LIVE_SESSIONS.load(Ordering::Relaxed), live_before + 1);
        assert!(
            wait_until(opened(&seen)).await,
            "BOOTED already went to the spare, so INIT must not wait for another"
        );
        session.terminate();
        drop(lease);
    }

    #[wasm_bindgen_test]
    async fn a_spare_still_booting_at_start_gets_init_after_it_boots() {
        install_fake_session_worker(300);
        let lease = held_lease();
        prewarm();
        let spare = spare_for_test().expect("spare");
        assert!(!spare.booted);
        let (session, seen) = start_recording();
        assert!(js_sys::Object::is(&session.worker, &spare.worker));
        assert!(
            wait_until(opened(&seen)).await,
            "BOOTED must reach the session's handler and release INIT"
        );
        session.terminate();
        drop(lease);
    }

    #[wasm_bindgen_test]
    async fn one_spare_per_prewarm_and_a_second_start_spawns_fresh() {
        install_fake_session_worker(0);
        let lease = held_lease();
        prewarm();
        let first = spare_for_test().expect("spare").worker;
        prewarm();
        let again = spare_for_test().expect("spare").worker;
        assert!(js_sys::Object::is(&first, &again), "no second spare");

        let (adopted, _) = start_recording();
        let (fresh, seen) = start_recording();
        assert!(!js_sys::Object::is(&fresh.worker, &first));
        assert!(wait_until(opened(&seen)).await);
        adopted.terminate();
        fresh.terminate();
        drop(lease);
    }

    #[wasm_bindgen_test]
    async fn a_spare_that_failed_to_load_is_not_adopted() {
        install_failing_session_worker();
        let lease = held_lease();
        prewarm();
        assert!(wait_until(|| spare_for_test().is_some_and(|s| s.failed)).await);
        let failed = spare_for_test().expect("spare").worker;

        install_fake_session_worker(0);
        let (session, seen) = start_recording();
        assert!(!js_sys::Object::is(&session.worker, &failed));
        assert!(wait_until(opened(&seen)).await);
        session.terminate();
        drop(lease);
    }

    const SHIPPED_LOADER: &str = include_str!("bin/wt_session_worker_loader.js");

    #[wasm_bindgen_test]
    async fn a_spare_whose_wasm_fails_to_load_is_marked_failed_and_not_adopted() {
        install_shipped_loader_with_missing_wasm(SHIPPED_LOADER);
        let lease = held_lease();
        prewarm();
        let marked = wait_until(|| spare_for_test().is_some_and(|s| s.failed)).await;
        let adopted = take_spare();
        if let Some((worker, _)) = &adopted {
            worker.terminate();
        }
        drop(lease);
        assert!(
            marked,
            "a rejected wasm_bindgen() must reach Worker.onerror"
        );
        assert!(adopted.is_none());
    }

    #[wasm_bindgen_test]
    async fn a_fresh_worker_whose_wasm_fails_to_load_closes_before_ready() {
        install_shipped_loader_with_missing_wasm(SHIPPED_LOADER);
        let (session, seen) = start_recording_adopting(false);
        let closed = wait_until(|| {
            seen.borrow()
                .iter()
                .any(|s| matches!(s, WebTransportStatus::ClosedBeforeReady(_)))
        })
        .await;
        session.terminate();
        assert!(closed);
    }

    #[wasm_bindgen_test]
    async fn a_start_that_does_not_adopt_leaves_the_spare_for_the_next_start() {
        install_fake_session_worker(0);
        let lease = held_lease();
        prewarm();
        let spare = spare_for_test().expect("spare").worker;

        let (observer, seen) = start_recording_adopting(false);
        assert!(!js_sys::Object::is(&observer.worker, &spare));
        assert!(wait_until(opened(&seen)).await);
        let (joiner, _) = start_recording();
        assert!(js_sys::Object::is(&joiner.worker, &spare));
        observer.terminate();
        joiner.terminate();
        drop(lease);
    }

    #[wasm_bindgen_test]
    async fn prewarm_replaces_a_spare_that_failed_to_load() {
        install_failing_session_worker();
        let lease = held_lease();
        prewarm();
        assert!(wait_until(|| spare_for_test().is_some_and(|s| s.failed)).await);
        let failed = spare_for_test().expect("spare").worker;

        install_fake_session_worker(0);
        prewarm();
        let replacement = spare_for_test().expect("a replacement spare");
        assert!(!js_sys::Object::is(&replacement.worker, &failed));
        assert!(!replacement.failed);
        drop(lease);
    }

    #[wasm_bindgen_test]
    fn dropping_a_lease_that_was_never_acquired_does_not_release_a_held_one() {
        install_fake_session_worker(0);
        let held = held_lease();
        prewarm();
        drop(SpareLease::default());
        assert!(spare_for_test().is_some());
        drop(held);
    }

    #[wasm_bindgen_test]
    fn acquiring_one_lease_twice_takes_one_claim() {
        install_fake_session_worker(0);
        let lease = held_lease();
        lease.acquire();
        prewarm();
        drop(lease);
        assert!(spare_for_test().is_none());
    }

    #[wasm_bindgen_test]
    async fn the_last_release_terminates_the_spare() {
        install_fake_session_worker(0);
        let lease = held_lease();
        prewarm();
        assert!(wait_until(|| spare_for_test().is_some_and(|s| s.booted)).await);
        let worker = spare_for_test().expect("spare").worker;
        assert!(pings(&worker).await, "the fake answers while it lives");

        drop(lease);
        assert!(spare_for_test().is_none());
        assert!(
            !pings(&worker).await,
            "a discarded spare must be terminated"
        );
    }

    #[wasm_bindgen_test]
    fn the_spare_survives_until_every_lease_is_released() {
        install_fake_session_worker(0);
        let outgoing = held_lease();
        let incoming = held_lease();
        prewarm();
        drop(outgoing);
        assert!(spare_for_test().is_some(), "one lease is still held");
        drop(incoming);
        assert!(spare_for_test().is_none());
    }

    #[wasm_bindgen_test]
    fn the_election_end_refill_boots_a_spare_only_while_a_lease_is_held() {
        install_fake_session_worker(0);
        crate::webtransport::prewarm_session_worker();
        assert!(spare_for_test().is_none(), "no lease, no spare");

        let lease = held_lease();
        crate::webtransport::prewarm_session_worker();
        assert!(spare_for_test().is_some());
        drop(lease);
    }
}

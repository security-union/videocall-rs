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

//! Dedicated Worker that owns one WebTransport session (#2728).

#![cfg_attr(target_arch = "wasm32", no_main)]

#[cfg(target_arch = "wasm32")]
mod wasm_worker {
    use js_sys::{Array, ArrayBuffer, Reflect, Uint8Array};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use videocall_transport::clock;
    use videocall_transport::downlink_stream::StreamKey;
    use videocall_transport::inbound::{InboundFrame, InboundLane};
    use videocall_transport::webtransport::{
        SessionHost, WebTransportService, WebTransportStatus, WebTransportTask,
    };
    use videocall_transport::worker_proto::{
        self, status_kind, to_main, to_worker, MAX_MAIN_INBOX_BYTES,
    };
    use videocall_types::Callback;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::{DedicatedWorkerGlobalScope, MessageEvent, WritableStream};

    struct PortLogger;

    impl log::Log for PortLogger {
        fn enabled(&self, _metadata: &log::Metadata) -> bool {
            true
        }

        fn log(&self, record: &log::Record) {
            let level = match record.level() {
                log::Level::Error => 1u8,
                log::Level::Warn => 2,
                log::Level::Info => 3,
                _ => 4,
            };
            let msg = Array::new();
            msg.push(&JsValue::from_f64(f64::from(to_main::LOG)));
            msg.push(&JsValue::from_f64(f64::from(level)));
            msg.push(&JsValue::from_str(&format!("{}", record.args())));
            post(&msg);
        }

        fn flush(&self) {}
    }

    static LOGGER: PortLogger = PortLogger;

    fn worker_scope() -> Option<DedicatedWorkerGlobalScope> {
        js_sys::global()
            .dyn_into::<DedicatedWorkerGlobalScope>()
            .ok()
    }

    fn post(msg: &Array) {
        if let Some(scope) = worker_scope() {
            let _ = scope.post_message(msg);
        }
    }

    fn post_with_transfer(msg: &Array, transfer: &Array) {
        if let Some(scope) = worker_scope() {
            let _ = scope.post_message_with_transfer(msg, transfer);
        }
    }

    fn post_status(kind: u8, message: &str, code: u32) {
        let msg = Array::new();
        msg.push(&JsValue::from_f64(f64::from(to_main::STATUS)));
        msg.push(&JsValue::from_f64(f64::from(kind)));
        msg.push(&JsValue::from_str(message));
        msg.push(&JsValue::from_f64(f64::from(code)));
        post(&msg);
    }

    struct Inbox {
        posted_bytes: Cell<f64>,
        acked_bytes: Cell<f64>,
        shed_count: Cell<u64>,
        last_ack_ms: Cell<f64>,
    }

    impl Inbox {
        fn in_flight(&self) -> usize {
            (self.posted_bytes.get() - self.acked_bytes.get()).max(0.0) as usize
        }

        /// How long main has gone without acknowledging a frame. Both
        fn main_silent_ms(&self, now_ms: f64) -> f64 {
            (now_ms - self.last_ack_ms.get()).max(0.0)
        }
    }

    struct Session {
        task: RefCell<Option<WebTransportTask>>,
        inbox: Rc<Inbox>,
    }

    thread_local! {
        static SESSION: RefCell<Option<Rc<Session>>> = const { RefCell::new(None) };
    }

    fn emit_frame(inbox: &Inbox, key: Option<StreamKey>, frame: InboundFrame) {
        let main_silent_ms = inbox.main_silent_ms(videocall_transport::clock::now_ms());
        if worker_proto::should_shed_frame(
            key,
            &frame.bytes,
            inbox.in_flight(),
            MAX_MAIN_INBOX_BYTES,
            main_silent_ms,
        ) {
            inbox.shed_count.set(inbox.shed_count.get() + 1);
            return;
        }
        let payload = Uint8Array::from(frame.bytes.as_slice());
        let buffer: ArrayBuffer = payload.buffer();
        let msg = Array::new();
        msg.push(&JsValue::from_f64(f64::from(to_main::FRAME)));
        msg.push(&JsValue::from_f64(f64::from(worker_proto::lane_code(
            frame.lane,
        ))));
        msg.push(&JsValue::from_f64(frame.received_at.0));
        msg.push(&buffer);
        let transfer = Array::new();
        transfer.push(&buffer);
        inbox
            .posted_bytes
            .set(inbox.posted_bytes.get() + frame.bytes.len() as f64);
        post_with_transfer(&msg, &transfer);
    }

    fn handle_init(url: String, cert_hashes: Array) {
        if cert_hashes.length() > 0 {
            let _ = Reflect::set(
                &js_sys::global(),
                &JsValue::from_str("__VC_WT_CERT_HASHES__"),
                &cert_hashes,
            );
        }

        let inbox = Rc::new(Inbox {
            posted_bytes: Cell::new(0.0),
            acked_bytes: Cell::new(0.0),
            shed_count: Cell::new(0),
            last_ack_ms: Cell::new(videocall_transport::clock::now_ms()),
        });

        let on_frame = {
            let inbox = inbox.clone();
            Callback::from(move |(key, frame): (Option<StreamKey>, InboundFrame)| {
                emit_frame(&inbox, key, frame);
            })
        };
        let notification = Callback::from(|status: WebTransportStatus| match status {
            WebTransportStatus::Opened => post_status(status_kind::OPENED, "", 0),
            WebTransportStatus::ClosedBeforeReady(msg) => {
                post_status(status_kind::CLOSED_BEFORE_READY, &msg, 0)
            }
            WebTransportStatus::ClosedAfterReady(msg) => {
                post_status(status_kind::CLOSED_AFTER_READY, &msg, 0)
            }
            WebTransportStatus::ClosedAfterReadyWithCode(info) => post_status(
                status_kind::CLOSED_AFTER_READY_WITH_CODE,
                &info.reason,
                info.code,
            ),
            WebTransportStatus::Closed(e) | WebTransportStatus::Error(e) => {
                post_status(status_kind::CLOSED_AFTER_READY, &format!("{e:?}"), 0)
            }
        });

        log::info!(
            "session worker opening transport: ds={} cert_hashes={}",
            url.contains(videocall_transport::downlink_stream::DOWNLINK_STREAMS_QUERY),
            cert_hashes.length()
        );
        match WebTransportService::connect_here(&url, on_frame, notification) {
            Ok(task) => {
                let Some(transport) = task.host.in_page().cloned() else {
                    post_status(
                        status_kind::CLOSED_BEFORE_READY,
                        "session worker did not receive an in-page transport",
                        0,
                    );
                    return;
                };
                let datagram_writable: WritableStream = transport.datagrams().writable();
                let ready = Array::new();
                ready.push(&JsValue::from_f64(f64::from(to_main::READY)));
                ready.push(&JsValue::from_f64(clock::time_origin_ms()));
                ready.push(&datagram_writable);
                let transfer = Array::new();
                transfer.push(&datagram_writable);
                post_with_transfer(&ready, &transfer);

                SESSION.with(|slot| {
                    *slot.borrow_mut() = Some(Rc::new(Session {
                        task: RefCell::new(Some(task)),
                        inbox,
                    }));
                });
                start_telemetry_pump();
                log::info!("session worker transport constructed; awaiting ready");
            }
            Err(e) => post_status(
                status_kind::CLOSED_BEFORE_READY,
                &format!("session worker could not open the transport: {e}"),
                0,
            ),
        }
    }

    fn handle_create_send_stream(req_id: f64, stream_key: u8, send_order: i32) {
        let Some(session) = SESSION.with(|slot| slot.borrow().clone()) else {
            post_stream_failed(req_id, "no session");
            return;
        };
        let host: Option<SessionHost> =
            session.task.borrow().as_ref().map(|task| task.host.clone());
        let Some(transport) = host.as_ref().and_then(|h| h.in_page().cloned()) else {
            post_stream_failed(req_id, "session already closed");
            return;
        };
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = wasm_bindgen_futures::JsFuture::from(transport.ready()).await {
                post_stream_failed(req_id, &format!("transport.ready() failed: {e:?}"));
                return;
            }
            match videocall_transport::webtransport::create_persistent_unistream(
                &transport, stream_key, send_order,
            )
            .await
            {
                Ok(send_stream) => {
                    let transform = match videocall_transport::webtransport::build_uplink_transform(
                        &send_stream,
                    ) {
                        Ok(t) => t,
                        Err(e) => {
                            post_stream_failed(req_id, &format!("transform stream: {e:?}"));
                            return;
                        }
                    };
                    let writable = transform.writable;
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Err(e) = wasm_bindgen_futures::JsFuture::from(transform.pipe).await {
                            log::debug!("uplink stream {stream_key} ended: {e:?}");
                        }
                    });
                    let msg = Array::new();
                    msg.push(&JsValue::from_f64(f64::from(to_main::SEND_STREAM_CREATED)));
                    msg.push(&JsValue::from_f64(req_id));
                    msg.push(&writable);
                    let transfer = Array::new();
                    transfer.push(&writable);
                    post_with_transfer(&msg, &transfer);
                }
                Err(e) => post_stream_failed(req_id, &format!("{e}")),
            }
        });
    }

    fn post_stream_failed(req_id: f64, message: &str) {
        let msg = Array::new();
        msg.push(&JsValue::from_f64(f64::from(to_main::SEND_STREAM_FAILED)));
        msg.push(&JsValue::from_f64(req_id));
        msg.push(&JsValue::from_str(message));
        post(&msg);
    }

    fn start_telemetry_pump() {
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                gloo_timers::future::TimeoutFuture::new(worker_proto::TELEMETRY_PUSH_MS).await;
                let Some(session) = SESSION.with(|slot| slot.borrow().clone()) else {
                    return;
                };
                let readback =
                    videocall_transport::webtransport::incoming_datagram_queue_readback();
                let msg = Array::new();
                msg.push(&JsValue::from_f64(f64::from(to_main::TELEMETRY)));
                msg.push(&JsValue::from_f64(
                    videocall_transport::webtransport::take_datagram_read_loop_max_gap_ms(),
                ));
                msg.push(&JsValue::from_f64(
                    videocall_transport::inbound::take_audio_lane_max_gap_ms(),
                ));
                msg.push(&JsValue::from_f64(0.0));
                msg.push(&JsValue::from_f64(f64::from(u8::from(readback.is_some()))));
                msg.push(&JsValue::from_f64(readback.map(|(h, _)| h).unwrap_or(0.0)));
                msg.push(&JsValue::from_f64(readback.map(|(_, a)| a).unwrap_or(0.0)));
                msg.push(&JsValue::from_f64(
                    videocall_transport::webtransport::inbound_unistream_reset_count() as f64,
                ));
                msg.push(&JsValue::from_f64(
                    videocall_transport::webtransport::send_order_fallback_count() as f64,
                ));
                msg.push(&JsValue::from_f64(session.inbox.shed_count.get() as f64));
                post(&msg);
            }
        });
    }

    fn handle_message(data: JsValue) {
        let Ok(array) = data.dyn_into::<Array>() else {
            return;
        };
        let Some(tag) = array.get(0).as_f64() else {
            return;
        };
        match tag as u8 {
            to_worker::INIT => {
                let url = array.get(1).as_string().unwrap_or_default();
                let hashes: Array = array.get(2).dyn_into().unwrap_or_else(|_| Array::new());
                handle_init(url, hashes);
            }
            to_worker::CREATE_SEND_STREAM => handle_create_send_stream(
                array.get(1).as_f64().unwrap_or(0.0),
                array.get(2).as_f64().unwrap_or(0.0) as u8,
                array.get(3).as_f64().unwrap_or(0.0) as i32,
            ),
            to_worker::DRAINED => {
                if let Some(session) = SESSION.with(|slot| slot.borrow().clone()) {
                    session
                        .inbox
                        .acked_bytes
                        .set(array.get(1).as_f64().unwrap_or(0.0));
                    session.inbox.last_ack_ms.set(worker_proto::ack_stamp_ms(
                        array.get(2).as_f64(),
                        videocall_transport::clock::now_ms(),
                    ));
                }
            }
            to_worker::CLOSE => {
                SESSION.with(|slot| {
                    if let Some(session) = slot.borrow().as_ref() {
                        session.task.borrow_mut().take();
                    }
                    *slot.borrow_mut() = None;
                });
                let msg = Array::new();
                msg.push(&JsValue::from_f64(f64::from(to_main::CLOSED)));
                post(&msg);
            }
            _ => {}
        }
    }

    #[wasm_bindgen(start)]
    pub fn start() {
        console_error_panic_hook::set_once();
        let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info));
        let on_message = Closure::wrap(Box::new(move |event: MessageEvent| {
            handle_message(event.data());
        }) as Box<dyn FnMut(MessageEvent)>);
        if let Some(scope) = worker_scope() {
            scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        }
        on_message.forget();

        let _ = Reflect::set(
            &js_sys::global(),
            &JsValue::from_str(videocall_transport::webtransport::WT_RECEIVE_WORKER_GLOBAL),
            &JsValue::from_str(videocall_transport::webtransport::WT_RECEIVE_WORKER_SELF_DISABLE),
        );

        let booted = Array::new();
        booted.push(&JsValue::from_f64(f64::from(to_main::BOOTED)));
        post(&booted);
        log::info!("session worker booted");
        let _ = InboundLane::Reliable;
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    println!("wt_session_worker is only compiled for the wasm32 target");
}

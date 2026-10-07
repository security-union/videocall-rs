// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Once;
use std::time::Duration;

use videocall_transport::downlink_stream::StreamKey;
use videocall_transport::inbound::InboundFrame;
use videocall_transport::webtransport::{
    WebTransportService, WebTransportStatus, WebTransportTask,
};
use videocall_types::Callback;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

const DROPPED_CLOSURE: &str = "after being dropped";

thread_local! {
    static ERRORS_LOGGED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

struct ErrorRecorder;

impl log::Log for ErrorRecorder {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Error
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            ERRORS_LOGGED.with(|e| e.borrow_mut().push(record.args().to_string()));
        }
    }

    fn flush(&self) {}
}

type Statuses = Rc<RefCell<Vec<WebTransportStatus>>>;

fn install_fake() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        log::set_logger(&ErrorRecorder).expect("no other logger in this test binary");
        log::set_max_level(log::LevelFilter::Error);
        js_sys::eval(
            r#"
            window.__wtFakeErrors = [];
            window.addEventListener('unhandledrejection', (e) =>
                window.__wtFakeErrors.push(String(e.reason && e.reason.message || e.reason)));
            window.WebTransport = class {
                constructor(url) {
                    this.ready = new Promise((resolve) => { this.resolveReady = resolve; });
                    this.closed = new Promise((resolve) => { this.resolveClosed = resolve; });
                    this.datagrams = {
                        readable: new ReadableStream(),
                        writable: new WritableStream(),
                        incomingHighWaterMark: 1,
                        incomingMaxAge: 0,
                    };
                    this.incomingUnidirectionalStreams = new ReadableStream();
                    this.incomingBidirectionalStreams = new ReadableStream();
                    window.__wtFakeLast = this;
                }
                close() {
                    setTimeout(() => this.resolveClosed({ closeCode: 0, reason: '' }), 100);
                }
            };
            "#,
        )
        .expect("install the fake WebTransport");
    });
}

fn resolve_ready() {
    js_sys::eval("window.__wtFakeLast.resolveReady()").expect("resolve the fake ready");
}

fn connect(statuses: Statuses) -> WebTransportTask {
    WebTransportService::connect_here(
        "https://fake.invalid/lobby",
        Callback::from(|_: (Option<StreamKey>, InboundFrame)| {}),
        Callback::from(move |status: WebTransportStatus| statuses.borrow_mut().push(status)),
    )
    .expect("the fake constructor does not throw")
}

fn dropped_closure_errors() -> Vec<String> {
    let errors = js_sys::eval("window.__wtFakeErrors").expect("read the recorder");
    js_sys::Array::from(&errors)
        .iter()
        .filter_map(|e| e.as_string())
        .filter(|e| e.contains(DROPPED_CLOSURE))
        .collect()
}

async fn sleep_ms(ms: u64) {
    gloo_timers::future::sleep(Duration::from_millis(ms)).await;
}

#[wasm_bindgen_test]
async fn ready_queued_when_the_task_drops_does_not_emit_opened() {
    install_fake();
    let statuses: Statuses = Rc::default();
    let task = connect(statuses.clone());

    resolve_ready();
    drop(task);
    sleep_ms(300).await;

    assert!(
        !statuses
            .borrow()
            .iter()
            .any(|s| matches!(s, WebTransportStatus::Opened)),
        "a dropped task must not emit Opened: {:?}",
        statuses.borrow()
    );
}

#[wasm_bindgen_test]
async fn closing_after_the_handshake_frees_the_closures_without_throwing() {
    install_fake();
    let statuses: Statuses = Rc::default();
    let task = connect(statuses.clone());

    resolve_ready();
    sleep_ms(50).await;
    assert_eq!(*statuses.borrow(), vec![WebTransportStatus::Opened]);
    assert_eq!(Rc::strong_count(&statuses), 2);

    drop(task);
    sleep_ms(300).await;

    assert_eq!(dropped_closure_errors(), Vec::<String>::new());
    assert_eq!(
        ERRORS_LOGGED.with(|e| e.borrow().clone()),
        Vec::<String>::new()
    );
    assert_eq!(*statuses.borrow(), vec![WebTransportStatus::Opened]);
    assert_eq!(
        Rc::strong_count(&statuses),
        1,
        "the dropped task's closures must be freed once its promise chains settle"
    );
}

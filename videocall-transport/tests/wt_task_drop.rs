// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use videocall_transport::downlink_stream::StreamKey;
use videocall_transport::inbound::InboundFrame;
use videocall_transport::webtransport::{WebTransportService, WebTransportStatus};
use videocall_types::Callback;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

const DROPPED_CLOSURE: &str = "after being dropped";
const UNREACHABLE: &str = "https://127.0.0.1:59999/lobby";

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

fn record_page_errors() {
    js_sys::eval(
        r#"
        window.__wtDropErrors = [];
        const push = (v) => window.__wtDropErrors.push(String(v && v.message || v));
        window.addEventListener('unhandledrejection', (e) => push(e.reason));
        window.addEventListener('error', (e) => push(e.error || e.message));
        "#,
    )
    .expect("install the page error recorder");
}

fn dropped_closure_errors() -> Vec<String> {
    let errors = js_sys::eval("window.__wtDropErrors").expect("read the recorder");
    js_sys::Array::from(&errors)
        .iter()
        .filter_map(|e| e.as_string())
        .filter(|e| e.contains(DROPPED_CLOSURE))
        .collect()
}

fn connect(
    statuses: Rc<RefCell<Vec<WebTransportStatus>>>,
) -> videocall_transport::webtransport::WebTransportTask {
    WebTransportService::connect_here(
        UNREACHABLE,
        Callback::from(|_: (Option<StreamKey>, InboundFrame)| {}),
        Callback::from(move |status: WebTransportStatus| statuses.borrow_mut().push(status)),
    )
    .expect("the constructor accepts a well-formed https URL")
}

#[wasm_bindgen_test]
async fn dropping_a_task_before_ready_does_not_invoke_freed_closures() {
    record_page_errors();
    log::set_logger(&ErrorRecorder).expect("no other logger in this test binary");
    log::set_max_level(log::LevelFilter::Error);

    let dropped_statuses: Rc<RefCell<Vec<WebTransportStatus>>> = Rc::default();
    let dropped = connect(dropped_statuses.clone());
    assert_eq!(Rc::strong_count(&dropped_statuses), 2);
    drop(dropped);

    let live_statuses: Rc<RefCell<Vec<WebTransportStatus>>> = Rc::default();
    let live = connect(live_statuses.clone());
    live.host.close();

    for _ in 0..40 {
        if !live_statuses.borrow().is_empty() {
            break;
        }
        gloo_timers::future::sleep(Duration::from_millis(50)).await;
    }
    gloo_timers::future::sleep(Duration::from_millis(300)).await;

    assert!(
        matches!(
            live_statuses.borrow().as_slice(),
            [WebTransportStatus::ClosedBeforeReady(_)]
        ),
        "close() before ready must reach the live task's closed handler: {:?}",
        live_statuses.borrow()
    );
    assert_eq!(dropped_closure_errors(), Vec::<String>::new());
    assert_eq!(
        ERRORS_LOGGED.with(|e| e.borrow().clone()),
        Vec::<String>::new()
    );
    assert!(
        dropped_statuses.borrow().is_empty(),
        "a dropped task must not notify: {:?}",
        dropped_statuses.borrow()
    );
    assert_eq!(
        Rc::strong_count(&dropped_statuses),
        1,
        "the dropped task's closures must be freed once its promise chains settle"
    );
    drop(live);
}

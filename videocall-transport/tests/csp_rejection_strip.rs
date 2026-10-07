// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Its own test binary, so the enforced CSP it installs cannot reach other tests' page.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use videocall_transport::downlink_stream::StreamKey;
use videocall_transport::inbound::InboundFrame;
use videocall_transport::webtransport::{WebTransportService, WebTransportStatus};
use videocall_types::Callback;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
async fn csp_blocked_handshake_does_not_carry_the_token() {
    js_sys::Function::new_no_args(
        "const m = document.createElement('meta');\
         m.httpEquiv = 'Content-Security-Policy';\
         m.content = \"connect-src 'self'\";\
         document.head.appendChild(m);",
    )
    .call0(&wasm_bindgen::JsValue::NULL)
    .expect("install CSP meta");

    let seen: Rc<RefCell<Option<String>>> = Rc::default();
    let sink = seen.clone();
    let _task = WebTransportService::connect_here(
        "https://blocked.example:4433/lobby?token=SECRETJWT",
        Callback::from(|_: (Option<StreamKey>, InboundFrame)| {}),
        Callback::from(move |status: WebTransportStatus| {
            if let WebTransportStatus::ClosedBeforeReady(msg) = status {
                *sink.borrow_mut() = Some(msg);
            }
        }),
    )
    .expect("a CSP block rejects ready(), it does not throw from the constructor");

    for _ in 0..100 {
        if seen.borrow().is_some() {
            break;
        }
        gloo_timers::future::sleep(Duration::from_millis(50)).await;
    }
    let message = seen
        .borrow()
        .clone()
        .expect("ready() must reject under the CSP");
    assert!(message.contains("blocked.example"), "{message}");
    assert!(!message.contains("SECRETJWT"), "{message}");
}

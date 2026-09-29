// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test-only read-back for the #2728 WebTransport receive path.

#[cfg(target_arch = "wasm32")]
const STATS_GLOBAL: &str = "__videocall_wt_receive_stats";

/// Idempotent and cheap; safe from a `use_hook` that runs once per mount.
#[cfg(target_arch = "wasm32")]
pub fn register_wt_receive_stats_hook() {
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    let Some(window) = web_sys::window() else {
        return;
    };

    let closure = Closure::wrap(Box::new(move || -> JsValue {
        let out = js_sys::Object::new();
        let set = |key: &str, value: f64| {
            let _ = js_sys::Reflect::set(&out, &JsValue::from_str(key), &JsValue::from_f64(value));
        };
        set(
            "audioLaneSessionMaxGapMs",
            videocall_transport::inbound::peek_audio_lane_session_max_gap_ms(),
        );
        set(
            "maxHandoffDelayMs",
            videocall_transport::worker_session::max_handoff_delay_ms(),
        );
        set(
            "framesReceived",
            videocall_transport::worker_session::frames_received() as f64,
        );
        set(
            "inboxShedCount",
            videocall_transport::worker_session::inbox_shed_count() as f64,
        );
        set(
            "unistreamReadyStallCount",
            videocall_transport::webtransport::unistream_ready_stall_count() as f64,
        );
        set(
            "unistreamQueueDepthBytes",
            videocall_transport::webtransport::unistream_queue_depth_bytes() as f64,
        );
        set(
            "unistreamBytesOfferedTotal",
            videocall_transport::webtransport::unistream_bytes_offered_total() as f64,
        );
        out.into()
    }) as Box<dyn FnMut() -> JsValue>);

    let _ = js_sys::Reflect::set(
        &window,
        &JsValue::from_str(STATS_GLOBAL),
        closure.as_ref().unchecked_ref(),
    );
    closure.forget();
}

/// Native stub so the call site can stay target-agnostic.
#[cfg(not(target_arch = "wasm32"))]
pub fn register_wt_receive_stats_hook() {}

/*
 * Copyright 2026 Security Union LLC
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

//! Test-only injection + observation hooks for the jitter-buffer freshness
//! deadline (issue #1022 — E2E coverage for the #1020 freshness deadline).
//!
//! ## Why this exists
//!
//! The #1020 freshness deadline (drop a stale buffered-video backlog and skip to
//! live / hold the last-good frame) runs *inside the decoder Web Worker's*
//! [`JitterBuffer`](videocall_codecs::jitter_buffer::JitterBuffer), on a ~10ms
//! tick. It has 6 deterministic unit tests, but no Playwright E2E coverage was
//! feasible because:
//!
//!   1. there was no way to deterministically force a *stale* head-of-line
//!      backlog into the worker's buffer from a browser test, and
//!   2. the skip outcome never crossed the worker→main boundary.
//!
//! Issue #1045 fixed (2): the worker now posts a `FreshnessSkipMessage` that the
//! main thread re-broadcasts as a `freshness_skip` `DiagEvent` (subsystem
//! `video`). This module fixes (1) and exposes the event to a browser test:
//!
//!   - `window.__videocall_inject_stale_video_backlog(num_frames, age_ms)`
//!     builds a self-contained test [`WasmDecoder`] (its own worker, running the
//!     *production* `worker_decoder` binary), then injects `num_frames` delta
//!     frames whose `arrival_time_ms` is back-dated by `age_ms`. With no buffered
//!     keyframe, the worker holds (waiting for a keyframe); once the back-dated
//!     head ages past `MAX_PLAYOUT_AGE_MS` (1800ms) the ~10ms tick trips the
//!     freshness deadline's keyframe-less eviction and posts a `freshness_skip`
//!     (`keyframe_seq` → `-1`, `dropped >= 1`).
//!   - `window.__videocall_freshness_skips` is a JS array this module appends to
//!     from a diagnostics-bus subscriber every time a `freshness_skip` `DiagEvent`
//!     arrives, so the spec can poll for the event and assert its shape.
//!   - `window.__videocall_keyframe_requests` is a JS array this module appends to
//!     from the test decoder's `on_request_keyframe` callback every time the worker's
//!     jitter buffer fires its proactive `request_keyframe` hook (issue 1899 /
//!     discussion 1960). It surfaces the stream-open one-shot PLI — the keyframe
//!     request fix (a) fires at *insert time* the moment a never-decoded stream holds
//!     delta-only frames with no keyframe — which arrives well before the
//!     `MAX_PLAYOUT_AGE_MS` freshness deadline that the eviction path would otherwise
//!     wait for. Each entry is `{ head_age_ms, ts_ms }`.
//!
//! The injected frames carry empty `data` and are *never decoded*: in the
//! keyframe-less path the deltas are evicted by the deadline before any release,
//! so WebCodecs is never fed a chunk. The test decoder reuses the production
//! worker byte-for-byte, so the freshness path under test is the real one.
//!
//! ## Gating
//!
//! [`register_freshness_inject_hooks`] is a no-op unless its caller has decided
//! the mock/debug feature is enabled — the dioxus-ui call site gates it on the
//! same `MOCK_PEERS_ENABLED` runtime-config flag that gates the mock-peers debug
//! feature and the #987 decode-budget injection hook. Production deploys leave
//! that flag `false`, so neither `window` global is ever attached and no test
//! decoder is ever created.

#[cfg(target_arch = "wasm32")]
use std::cell::RefCell;

#[cfg(target_arch = "wasm32")]
thread_local! {
    /// Keeps the test-only injection decoder (and therefore its Web Worker) alive
    /// for the page lifetime. Dropping a `WasmDecoder` would terminate its worker
    /// before the ~10ms tick could trip the deadline, so the hook stashes it here
    /// on first use. Test-only; never populated in production (the registrar is
    /// gated off).
    static INJECT_DECODER: RefCell<Option<videocall_codecs::decoder::WasmDecoder>> =
        const { RefCell::new(None) };
    static COLD_INJECT_DECODER: RefCell<Option<videocall_codecs::decoder::WasmDecoder>> =
        const { RefCell::new(None) };
    /// Sequence high-water mark across the COLD path's repeated bursts.
    static COLD_SEQ_BASE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// JS global the spec polls for captured `freshness_skip` events.
#[cfg(target_arch = "wasm32")]
const SKIPS_GLOBAL: &str = "__videocall_freshness_skips";

/// JS global the spec polls for captured proactive keyframe requests (issue 1899 / discussion 1960).
/// Each entry is a plain JS object `{ head_age_ms, ts_ms }`, pushed by the test decoder's
/// `on_request_keyframe` callback every time the worker's jitter buffer fires its `request_keyframe`
/// hook and the resulting `RequestKeyframeMessage` reaches the main thread — i.e. the stream-open
/// one-shot PLI (fix (a): fired at insert time, `head_age_ms == 0`) OR the #1025 freshness-deadline
/// eviction PLI (`head_age_ms >= MAX_PLAYOUT_AGE_MS`). `ts_ms` is the main-thread wall-clock capture
/// time, so the spec can bound how quickly the request surfaced after injection.
#[cfg(target_arch = "wasm32")]
const KEYFRAME_REQUESTS_GLOBAL: &str = "__videocall_keyframe_requests";

/// JS global the spec calls to inject a stale backlog.
#[cfg(target_arch = "wasm32")]
const INJECT_GLOBAL: &str = "__videocall_inject_stale_video_backlog";

#[cfg(target_arch = "wasm32")]
const INJECT_FROM_PEER: &str = "inject-local";
#[cfg(target_arch = "wasm32")]
const INJECT_TO_PEER: &str = "inject-peer";

#[cfg(target_arch = "wasm32")]
const COLD_INJECT_GLOBAL: &str = "__videocall_inject_stale_video_backlog_cold";

/// Issue 2572: separates a boot failure from a replay failure for the spec.
#[cfg(target_arch = "wasm32")]
const COLD_BOOT_STATE_GLOBAL: &str = "__videocall_cold_worker_boot_state";

/// Distinct from the warm pair because the collector reads one global bus.
#[cfg(target_arch = "wasm32")]
const COLD_FROM_PEER: &str = "cold-local";
#[cfg(target_arch = "wasm32")]
const COLD_TO_PEER: &str = "cold-peer";

/// Register the test-only freshness injection + observation hooks on `window`.
///
/// **The caller is responsible for gating** — pass only when the mock/debug
/// feature is enabled (the dioxus-ui call site checks `mock_peers_enabled()`).
/// Idempotent and cheap; safe to call from a `use_hook` that runs once per mount.
#[cfg(target_arch = "wasm32")]
pub fn register_freshness_inject_hooks() {
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    let Some(window) = web_sys::window() else {
        return;
    };

    // Seed the capture array if absent so the spec can read it even before the
    // first event (and so re-registration on remount does not clobber prior
    // captures).
    if js_sys::Reflect::get(&window, &JsValue::from_str(SKIPS_GLOBAL))
        .map(|v| !v.is_object())
        .unwrap_or(true)
    {
        let _ = js_sys::Reflect::set(
            &window,
            &JsValue::from_str(SKIPS_GLOBAL),
            &js_sys::Array::new(),
        );
    }

    // Seed the keyframe-request capture array (issue 1899 / discussion 1960) if absent, the same
    // way and for the same reasons as SKIPS_GLOBAL: the array must exist before the first request
    // fires (the recorder skips silently if it is missing), and a remount must not clobber prior
    // captures. Seeded BEFORE `ensure_test_decoder` below so the pre-warm can never race it.
    if js_sys::Reflect::get(&window, &JsValue::from_str(KEYFRAME_REQUESTS_GLOBAL))
        .map(|v| !v.is_object())
        .unwrap_or(true)
    {
        let _ = js_sys::Reflect::set(
            &window,
            &JsValue::from_str(KEYFRAME_REQUESTS_GLOBAL),
            &js_sys::Array::new(),
        );
    }

    // Subscriber: append every freshness_skip DiagEvent to the capture array.
    spawn_freshness_skip_collector();

    ensure_test_decoder();

    // window.__videocall_inject_stale_video_backlog(num_frames, age_ms):
    // inject `num_frames` back-dated delta frames into the (pre-warmed) test
    // decoder so the keyframe-less freshness deadline trips on the next tick.
    let inject_cb = Closure::<dyn Fn(f64, f64)>::new(|num_frames: f64, age_ms: f64| {
        inject_stale_video_backlog(num_frames as u32, age_ms);
    });
    let _ = js_sys::Reflect::set(
        &window,
        &JsValue::from_str(INJECT_GLOBAL),
        inject_cb.as_ref().unchecked_ref(),
    );
    // Leak the closure so the JS reference stays valid for the page lifetime.
    inject_cb.forget();

    // window.__videocall_inject_stale_video_backlog_cold(num_frames, age_ms) — issue 1741.
    let cold_cb = Closure::<dyn Fn(f64)>::new(|num_frames: f64| {
        inject_stale_video_backlog_cold(num_frames as u32);
    });
    let _ = js_sys::Reflect::set(
        &window,
        &JsValue::from_str(COLD_INJECT_GLOBAL),
        cold_cb.as_ref().unchecked_ref(),
    );
    cold_cb.forget();

    let boot_state_cb = Closure::<dyn Fn() -> JsValue>::new(cold_worker_boot_state);
    let _ = js_sys::Reflect::set(
        &window,
        &JsValue::from_str(COLD_BOOT_STATE_GLOBAL),
        boot_state_cb.as_ref().unchecked_ref(),
    );
    boot_state_cb.forget();
}

/// `null` until the first cold injection has constructed the decoder.
#[cfg(target_arch = "wasm32")]
fn cold_worker_boot_state() -> wasm_bindgen::JsValue {
    use wasm_bindgen::JsValue;
    COLD_INJECT_DECODER.with(|cell| {
        let slot = cell.borrow();
        let Some(decoder) = slot.as_ref() else {
            return JsValue::NULL;
        };
        let obj = js_sys::Object::new();
        let _ = js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("handshake_seen"),
            &JsValue::from_bool(decoder.worker_handshake_seen()),
        );
        let _ = js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("queue_dropped"),
            &JsValue::from_bool(decoder.boot_queue_dropped()),
        );
        obj.into()
    })
}

/// Native stub: no `window`/worker, nothing to register. Keeps the call site
/// target-agnostic and `cargo test --lib` green on the host target.
#[cfg(not(target_arch = "wasm32"))]
pub fn register_freshness_inject_hooks() {}

/// Spawn a diagnostics-bus subscriber that pushes each `freshness_skip`
/// `DiagEvent` (subsystem `video`; see issue #1045) onto
/// `window.__videocall_freshness_skips` as a plain JS object the spec can read:
/// `{ head_age_ms, keyframe_seq, dropped, ts_ms, escalated, tick_gap_ms }` (`escalated` is the
/// #1662 keyframe-less hold-ceiling escalation flag, a real JS boolean; `tick_gap_ms` is the #1851
/// wall-clock gap in ms since the previous worker poll — an f64, mirroring `head_age_ms`).
#[cfg(target_arch = "wasm32")]
fn spawn_freshness_skip_collector() {
    use videocall_diagnostics::{recv_loop_action, subscribe, MetricValue, RecvLoopAction};
    use wasm_bindgen::prelude::*;

    wasm_bindgen_futures::spawn_local(async move {
        let mut rx = subscribe();
        loop {
            // Issue 2174: a bare `while let Ok(..)` here died permanently on the
            // first `Overflowed`, which is recoverable — see
            // `videocall_diagnostics::recv_loop_action`.
            let evt = match rx.recv().await {
                Ok(evt) => evt,
                Err(e) => match recv_loop_action(&e) {
                    RecvLoopAction::Continue => continue,
                    RecvLoopAction::Break => break,
                },
            };
            if evt.subsystem != "video" {
                continue;
            }
            // A freshness_skip event carries an `event` text metric == "freshness_skip".
            let is_skip = evt.metrics.iter().any(|m| {
                m.name == "event"
                    && matches!(&m.value, MetricValue::Text(t) if t == "freshness_skip")
            });
            if !is_skip {
                continue;
            }

            let mut head_age_ms = f64::NAN;
            let mut keyframe_seq = i64::MIN;
            let mut dropped = 0u64;
            // #1662: the worker encodes `escalated` as i64 0/1 (mirroring keyframe_seq's i64). It is
            // absent on older events / never set false-vs-true confusion: default false.
            let mut escalated = false;
            // #1851: wall-clock gap (ms) since the previous worker poll, an f64 metric mirroring
            // head_age_ms. Absent on pre-#1851 events → stays NaN, which the e2e `>= 0` assertion
            // rejects, so a missing metric surfaces as a failure rather than a spurious 0.
            let mut tick_gap_ms = f64::NAN;
            let mut from_peer = String::new();
            let mut to_peer = String::new();
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("head_age_ms", MetricValue::F64(v)) => head_age_ms = *v,
                    ("from_peer", MetricValue::Text(v)) => from_peer = v.to_string(),
                    ("to_peer", MetricValue::Text(v)) => to_peer = v.to_string(),
                    // #1045 encodes keyframe_seq as i64 with -1 for the keyframe-less case.
                    ("keyframe_seq", MetricValue::I64(v)) => keyframe_seq = *v,
                    ("dropped", MetricValue::U64(v)) => dropped = *v,
                    // #1662 escalation flag: i64 0/1 → coerce to a real bool for the JS object.
                    ("escalated", MetricValue::I64(v)) => escalated = *v != 0,
                    // #1851 tick-gap: f64 ms since the previous worker poll (mirrors head_age_ms).
                    ("tick_gap_ms", MetricValue::F64(v)) => tick_gap_ms = *v,
                    _ => {}
                }
            }

            let obj = js_sys::Object::new();
            let _ = js_sys::Reflect::set(
                &obj,
                &JsValue::from_str("head_age_ms"),
                &JsValue::from_f64(head_age_ms),
            );
            let _ = js_sys::Reflect::set(
                &obj,
                &JsValue::from_str("keyframe_seq"),
                &JsValue::from_f64(keyframe_seq as f64),
            );
            let _ = js_sys::Reflect::set(
                &obj,
                &JsValue::from_str("dropped"),
                &JsValue::from_f64(dropped as f64),
            );
            let _ = js_sys::Reflect::set(
                &obj,
                &JsValue::from_str("ts_ms"),
                &JsValue::from_f64(evt.ts_ms as f64),
            );
            // #1662: surface the escalation flag as a real JS boolean so an e2e spec can assert
            // `skip.escalated === true` cleanly (rather than reading a 0/1 number).
            let _ = js_sys::Reflect::set(
                &obj,
                &JsValue::from_str("escalated"),
                &JsValue::from_bool(escalated),
            );
            // #1851: surface the tick-gap as a real JS number so an e2e spec can assert the
            // collector entry carries a numeric tick_gap_ms (and, in the field-log analogue,
            // distinguish a tick-starvation resume poll from a normal-cadence skip).
            let _ = js_sys::Reflect::set(
                &obj,
                &JsValue::from_str("tick_gap_ms"),
                &JsValue::from_f64(tick_gap_ms),
            );
            let _ = js_sys::Reflect::set(
                &obj,
                &JsValue::from_str("from_peer"),
                &JsValue::from_str(&from_peer),
            );
            let _ = js_sys::Reflect::set(
                &obj,
                &JsValue::from_str("to_peer"),
                &JsValue::from_str(&to_peer),
            );

            if let Some(window) = web_sys::window() {
                if let Ok(arr) = js_sys::Reflect::get(&window, &JsValue::from_str(SKIPS_GLOBAL)) {
                    if let Ok(arr) = arr.dyn_into::<js_sys::Array>() {
                        arr.push(&obj);
                    }
                }
            }
        }
    });
}

/// Push a captured proactive keyframe request onto [`KEYFRAME_REQUESTS_GLOBAL`] as a plain JS
/// object `{ head_age_ms, ts_ms }` the spec can read (issue 1899 / discussion 1960).
///
/// Wired as the test decoder's `on_request_keyframe` callback (see [`ensure_test_decoder`]), so it
/// runs on the MAIN thread each time the worker's jitter buffer fires its `request_keyframe` hook
/// and the resulting `RequestKeyframeMessage` is handled by `WasmDecoder` — the SAME production
/// worker→main path a real peer's PLI travels. `head_age_ms` is the backlog age the worker carried
/// (`0.0` for the stream-open one-shot in fix (a); `>= MAX_PLAYOUT_AGE_MS` for the #1025 eviction
/// path). `ts_ms` is `Date::now()` at capture, letting the spec bound how quickly the request
/// surfaced after injection. Silent no-op if `window`/the array is absent (never true post-seed).
#[cfg(target_arch = "wasm32")]
fn record_keyframe_request(head_age_ms: f64) {
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    let Some(window) = web_sys::window() else {
        return;
    };

    let obj = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &obj,
        &JsValue::from_str("head_age_ms"),
        &JsValue::from_f64(head_age_ms),
    );
    let _ = js_sys::Reflect::set(
        &obj,
        &JsValue::from_str("ts_ms"),
        &JsValue::from_f64(js_sys::Date::now()),
    );

    if let Ok(arr) = js_sys::Reflect::get(&window, &JsValue::from_str(KEYFRAME_REQUESTS_GLOBAL)) {
        if let Ok(arr) = arr.dyn_into::<js_sys::Array>() {
            arr.push(&obj);
        }
    }
}

/// Build (on first call) the self-contained test decoder and inject `num_frames`
/// delta frames back-dated by `age_ms`, forming a stale keyframe-less head-of-line
/// backlog. The worker holds (no keyframe to release) until the head ages past
/// `MAX_PLAYOUT_AGE_MS`, at which point the ~10ms tick evicts the stale deltas and
/// posts a `freshness_skip` (issue #1045) — captured by the collector above.
#[cfg(target_arch = "wasm32")]
fn inject_stale_video_backlog(num_frames: u32, age_ms: f64) {
    use videocall_codecs::messages::StreamContext;

    ensure_test_decoder();

    INJECT_DECODER.with(|cell| {
        let slot = cell.borrow();
        if let Some(decoder) = slot.as_ref() {
            inject_backlog(
                decoder,
                num_frames,
                age_ms,
                0,
                &StreamContext {
                    from_peer: INJECT_FROM_PEER.to_string(),
                    to_peer: INJECT_TO_PEER.to_string(),
                },
            );
        }
    });
}

/// Must exceed the spec's `COLD_BOOT_TIMEOUT_MS`.
#[cfg(target_arch = "wasm32")]
const COLD_HARNESS_BOOT_REPLAY_TTL_MS: f64 = 30_000.0;

/// Issue 1741 harness: constructs the `WasmDecoder` and injects into it in ONE synchronous block,
/// so `main()` cannot have run `set_onmessage`.
#[cfg(target_arch = "wasm32")]
fn inject_stale_video_backlog_cold(num_frames: u32) {
    use videocall_codecs::messages::StreamContext;

    COLD_INJECT_DECODER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(build_test_decoder(false));
        }
        if let Some(decoder) = slot.as_ref() {
            let context = StreamContext {
                from_peer: COLD_FROM_PEER.to_string(),
                to_peer: COLD_TO_PEER.to_string(),
            };
            let base = COLD_SEQ_BASE.with(|c| {
                let base = c.get();
                c.set(base + u64::from(num_frames.max(1)));
                base
            });
            // `push_frame`, not `inject_stale_frame`: this must drive the PRODUCTION method, so
            // deleting its `SetContext` re-emit fails this test. The worker stamps arrival with
            // its own clock, so the delta-only head ages past MAX_PLAYOUT_AGE_MS unaided.
            let now_ms = js_sys::Date::now() as u128;
            for i in 0..num_frames.max(1) {
                decoder.push_frame(
                    delta_frame(base + u64::from(i) + 1, now_ms),
                    Some(context.clone()),
                );
            }
        }
    });
}

/// Contiguous back-dated deltas, so the keyframe-less eviction is what fires.
#[cfg(target_arch = "wasm32")]
fn inject_backlog(
    decoder: &videocall_codecs::decoder::WasmDecoder,
    num_frames: u32,
    age_ms: f64,
    seq_base: u64,
    context: &videocall_codecs::messages::StreamContext,
) {
    let now_ms = js_sys::Date::now() as u128;
    let arrival_time_ms = now_ms.saturating_sub(age_ms.max(0.0) as u128);

    for i in 0..num_frames.max(1) {
        decoder.inject_stale_frame(
            delta_frame(seq_base + u64::from(i) + 1, arrival_time_ms),
            Some(context.clone()),
        );
    }
}

/// A delta frame with no payload. Delta-only means the buffer never releases, so the
/// keyframe-less eviction is what eventually fires.
#[cfg(target_arch = "wasm32")]
fn delta_frame(
    sequence_number: u64,
    arrival_time_ms: u128,
) -> videocall_codecs::frame::FrameBuffer {
    use videocall_codecs::frame::{FrameBuffer, FrameCodec, FrameType, VideoFrame};

    FrameBuffer::new(
        VideoFrame {
            sequence_number,
            frame_type: FrameType::DeltaFrame,
            codec: FrameCodec::Vp9Profile0Level10Bit8,
            data: Vec::new(),
            timestamp: 0.0,
        },
        arrival_time_ms,
    )
}

#[cfg(target_arch = "wasm32")]
fn ensure_test_decoder() {
    INJECT_DECODER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(build_test_decoder(true));
        }
    });
}

/// Mirrors the production peer-decode constructor (peer_decoder.rs). The issue-1741 cold decoder
/// passes `record_requests: false` because it shares one page, and one array, with the warm one.
#[cfg(target_arch = "wasm32")]
fn build_test_decoder(record_requests: bool) -> videocall_codecs::decoder::WasmDecoder {
    use videocall_codecs::decoder::{VideoCodec, WasmDecoder};

    let on_request: Box<dyn Fn(f64)> = if record_requests {
        Box::new(record_keyframe_request)
    } else {
        Box::new(|_head_age_ms| {})
    };
    WasmDecoder::new_with_video_frame_callback(
        VideoCodec::Vp9Profile0Level10Bit8,
        Box::new(|_frame| {}),
        on_request,
        // Issue #1641: the harness exercises the CAMERA freshness path, so tag it as such.
        crate::decode::peer_decoder::MEDIA_TYPE_CAMERA,
        // NOT the production TTL: this harness measures boot latency directly, so a short TTL
        // makes a slow boot read as a replay failure.
        COLD_HARNESS_BOOT_REPLAY_TTL_MS,
    )
}

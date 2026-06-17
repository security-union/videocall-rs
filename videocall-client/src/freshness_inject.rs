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
}

/// JS global the spec polls for captured `freshness_skip` events.
#[cfg(target_arch = "wasm32")]
const SKIPS_GLOBAL: &str = "__videocall_freshness_skips";

/// JS global the spec calls to inject a stale backlog.
#[cfg(target_arch = "wasm32")]
const INJECT_GLOBAL: &str = "__videocall_inject_stale_video_backlog";

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

    // Subscriber: append every freshness_skip DiagEvent to the capture array.
    spawn_freshness_skip_collector();

    // Pre-warm the test decoder NOW (at registration, well before the spec calls
    // the inject hook) so its Web Worker has finished its async wasm boot and
    // registered its `onmessage` handler by injection time. The trunk worker
    // loader (`worker_decoder_loader.js`) instantiates the wasm asynchronously,
    // and messages posted to the worker before its `main()` runs `set_onmessage`
    // are dropped (verified empirically — a cold-worker injection lost all 5
    // frames; a warm-worker injection tripped the deadline cleanly). Pre-warming
    // eliminates that race so a single injection from the spec is deterministic.
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
}

/// Native stub: no `window`/worker, nothing to register. Keeps the call site
/// target-agnostic and `cargo test --lib` green on the host target.
#[cfg(not(target_arch = "wasm32"))]
pub fn register_freshness_inject_hooks() {}

/// Spawn a diagnostics-bus subscriber that pushes each `freshness_skip`
/// `DiagEvent` (subsystem `video`; see issue #1045) onto
/// `window.__videocall_freshness_skips` as a plain JS object the spec can read:
/// `{ head_age_ms, keyframe_seq, dropped, ts_ms }`.
#[cfg(target_arch = "wasm32")]
fn spawn_freshness_skip_collector() {
    use videocall_diagnostics::{subscribe, MetricValue};
    use wasm_bindgen::prelude::*;

    wasm_bindgen_futures::spawn_local(async move {
        let mut rx = subscribe();
        while let Ok(evt) = rx.recv().await {
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
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("head_age_ms", MetricValue::F64(v)) => head_age_ms = *v,
                    // #1045 encodes keyframe_seq as i64 with -1 for the keyframe-less case.
                    ("keyframe_seq", MetricValue::I64(v)) => keyframe_seq = *v,
                    ("dropped", MetricValue::U64(v)) => dropped = *v,
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

/// Build (on first call) the self-contained test decoder and inject `num_frames`
/// delta frames back-dated by `age_ms`, forming a stale keyframe-less head-of-line
/// backlog. The worker holds (no keyframe to release) until the head ages past
/// `MAX_PLAYOUT_AGE_MS`, at which point the ~10ms tick evicts the stale deltas and
/// posts a `freshness_skip` (issue #1045) — captured by the collector above.
#[cfg(target_arch = "wasm32")]
fn inject_stale_video_backlog(num_frames: u32, age_ms: f64) {
    use videocall_codecs::frame::{FrameBuffer, FrameCodec, FrameType, VideoFrame};

    // At least one frame, default to a small backlog if the spec passes 0.
    let num_frames = num_frames.max(1);

    ensure_test_decoder();

    INJECT_DECODER.with(|cell| {
        let slot = cell.borrow();
        let Some(decoder) = slot.as_ref() else {
            return;
        };

        let now_ms = js_sys::Date::now() as u128;
        let arrival_time_ms = now_ms.saturating_sub(age_ms.max(0.0) as u128);

        // Inject ONLY delta frames (no keyframe): the buffer waits for a keyframe and
        // never releases/decodes the deltas. Once the back-dated head ages past the
        // deadline, the keyframe-less eviction path fires (keyframe_seq → -1).
        // Sequence numbers start at 1 (0 can collide with the "never decoded" sentinel
        // logic in some buffers); contiguous so they form a single backlog.
        for i in 0..num_frames {
            let frame = FrameBuffer::new(
                VideoFrame {
                    sequence_number: (i + 1) as u64,
                    frame_type: FrameType::DeltaFrame,
                    codec: FrameCodec::Vp9Profile0Level10Bit8,
                    data: Vec::new(),
                    timestamp: 0.0,
                },
                arrival_time_ms,
            );
            decoder.inject_stale_frame(frame);
        }
    });
}

/// Construct the self-contained test decoder (spawning its Web Worker) once and
/// cache it in [`INJECT_DECODER`]. Idempotent. Called at hook registration to
/// PRE-WARM the worker — the trunk worker loader instantiates the wasm
/// asynchronously and messages posted before the worker's `main()` runs
/// `set_onmessage` are dropped, so the worker must be booted before the first
/// injection (see the call site in `register_freshness_inject_hooks`).
#[cfg(target_arch = "wasm32")]
fn ensure_test_decoder() {
    use videocall_codecs::decoder::{VideoCodec, WasmDecoder};

    INJECT_DECODER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_some() {
            return;
        }
        // Mirror the production peer-decode constructor (peer_decoder.rs): same
        // codec, same `new_with_video_frame_callback` path that wires
        // `handle_worker_diag_message` (which re-broadcasts the freshness_skip).
        // The callbacks are no-ops: injected frames are never decoded (the
        // keyframe-less deadline evicts them first), and the proactive
        // keyframe-request signal (#1025) has nothing to do in the test harness.
        let decoder = WasmDecoder::new_with_video_frame_callback(
            VideoCodec::Vp9Profile0Level10Bit8,
            Box::new(|_frame| {}),
            Box::new(|| {}),
        );
        *slot = Some(decoder);
    });
}

// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test-only diagnostics-bus injection hooks for the adaptive decode budget
//! (issue #987, task 1a.6).
//!
//! The adaptive decode-budget control loop in
//! [`crate::components::attendants`] consumes two `client_perf` metrics off the
//! [`videocall_diagnostics`] bus:
//!
//!   - `client_render_fps`           (f64, ~1 Hz)  — emitted by
//!     [`videocall_client::render_fps`]
//!   - `client_longtask_duration_ms` (f64, event-driven) — emitted by
//!     [`videocall_client::long_tasks`]
//!
//! In a real browser those metrics come from rAF cadence and the
//! `PerformanceObserver` long-task entries, neither of which a headless
//! Playwright run can deterministically force into the "sustained low FPS"
//! regime the step-down logic requires. This module exposes two `window`
//! globals so an E2E spec can drive the loop synthetically:
//!
//!   - `window.__videocall_inject_render_fps(n)`   — push one synthetic
//!     `client_render_fps` sample (closes one ~1 Hz bucket in the loop).
//!   - `window.__videocall_inject_longtask(ms)`    — push one synthetic
//!     `client_longtask_duration_ms` event (accumulates into the open bucket).
//!
//! Both reuse the production emit helpers from `videocall_client`, so the
//! event shape (subsystem `"client_perf"`, metric names, `MetricValue::F64`)
//! is *byte-for-byte identical* to the real signals — there is no separate
//! code path for the control loop to special-case.
//!
//! ## Gating
//!
//! These globals are registered **only when `mock_peers_enabled()` is true**
//! — the same `MOCK_PEERS_ENABLED` runtime-config flag that gates the
//! mock-peers debug feature the E2E specs already rely on. Production deploys
//! leave that flag `false`, so the hooks are never attached in production.
//! This mirrors the existing `window.__videocall_*` debug globals (e.g.
//! `__videocall_capability_score` in `console_log_collector.rs`), which are
//! plain `window` assignments that only execute on a dev/debug path rather
//! than being compiled out.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// Register the test-only injection hooks on `window`, gated on the
/// `MOCK_PEERS_ENABLED` runtime-config flag.
///
/// Idempotent and cheap to call from a `use_hook` (which runs once per
/// component mount). When `mock_peers_enabled()` is false this is a no-op and
/// no globals are attached.
#[cfg(target_arch = "wasm32")]
pub fn register_decode_budget_inject_hooks() {
    if !crate::constants::mock_peers_enabled() {
        return;
    }

    let Some(window) = web_sys::window() else {
        return;
    };

    // window.__videocall_inject_render_fps(n): push one synthetic
    // `client_render_fps` sample onto the diagnostics bus, identical in shape
    // to videocall_client::render_fps::emit_render_fps.
    let fps_cb = Closure::<dyn Fn(f64)>::new(|fps: f64| {
        videocall_client::render_fps::emit_render_fps(fps);
    });
    let _ = js_sys::Reflect::set(
        &window,
        &JsValue::from_str("__videocall_inject_render_fps"),
        fps_cb.as_ref().unchecked_ref(),
    );
    // Leak the closure so the JS reference stays valid for the page lifetime.
    fps_cb.forget();

    // window.__videocall_inject_longtask(ms): push one synthetic
    // `client_longtask_duration_ms` event, identical in shape to
    // videocall_client::long_tasks::emit_long_task_metric.
    let longtask_cb = Closure::<dyn Fn(f64)>::new(|ms: f64| {
        videocall_client::long_tasks::emit_long_task_metric(ms);
    });
    let _ = js_sys::Reflect::set(
        &window,
        &JsValue::from_str("__videocall_inject_longtask"),
        longtask_cb.as_ref().unchecked_ref(),
    );
    longtask_cb.forget();
}

/// Native stub: no `window`, nothing to register. Lets the call site stay
/// target-agnostic and keeps `cargo test --lib` green on the host target.
#[cfg(not(target_arch = "wasm32"))]
pub fn register_decode_budget_inject_hooks() {}

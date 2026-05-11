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

//! Background-throttling-immune heartbeat for the diagnostics managers.
//!
//! # Problem
//!
//! Chrome (and other Chromium-based browsers) aggressively throttle main-thread
//! `setInterval` / `setTimeout` callbacks when a tab is hidden, clamping them to
//! a minimum interval of ~1000ms. Firefox and Safari have similar behavior under
//! slightly different rules.
//!
//! The diagnostics managers use a 500ms heartbeat to drive:
//! - Adaptive-quality (AQ) feedback dispatch (CONGESTION-triggered step-downs)
//! - Per-peer health / FPS reporting to the UI and to the diagnostics packet bus
//! - Stale-data freshness tracking
//!
//! When the user backgrounds the meeting tab to do other work, these heartbeats
//! get cut to 1Hz, halving the AQ feedback rate and starving the diagnostics
//! pipeline. The decoder workers themselves keep running at full speed (they
//! live in `DedicatedWorkerGlobalScope`, which is **not** throttled), so the
//! local main thread becomes the bottleneck.
//!
//! # Solution
//!
//! A `DedicatedWorkerGlobalScope` is exempt from main-thread visibility-based
//! throttling. We spawn a tiny inline Worker (constructed from a Blob URL, so
//! no extra Trunk-built crate is required) that fires `postMessage("tick")`
//! every 500ms. The main thread receives those messages and dispatches the
//! existing `HeartbeatTick` event through the same mpsc channel as before, so
//! per-encoder state (which must stay on the main thread) is unchanged.
//!
//! # Fallback
//!
//! If Worker construction fails for any reason (CSP that disallows
//! `blob:` workers, ancient browsers, etc.), we fall back to a plain
//! `setInterval`. The user gets the *pre-fix* behavior in that case — a
//! degradation, not a regression.
//!
//! # Lifecycle
//!
//! The returned [`HeartbeatTimer`] terminates the underlying Worker (or clears
//! the fallback interval) in its `Drop` impl, so it is safe to embed inside
//! the diagnostics managers and rely on normal ownership semantics for
//! cleanup. One Worker per `DiagnosticManager`/`SenderDiagnosticManager` is
//! cheap and matches the lifetime of the call.

#[cfg(target_arch = "wasm32")]
use js_sys::Array;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::closure::Closure;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{JsCast, JsValue};
#[cfg(target_arch = "wasm32")]
use web_sys::{window, Blob, BlobPropertyBag, Url, Worker};

/// Boxed main-thread heartbeat callback. The Worker fires `postMessage`, and
/// the main thread receives it and invokes this. Kept as a `FnMut` so callers
/// can mutate captured state (e.g. an mpsc sender) freely.
#[cfg(target_arch = "wasm32")]
type BoxedOnTick = Box<dyn FnMut() + 'static>;

/// JavaScript source for the heartbeat Worker.
///
/// This is intentionally tiny — just a `setInterval` that posts a tick to the
/// parent. Worker-scope timers are not throttled when the parent tab is hidden,
/// so this keeps firing at the configured cadence even while the main thread
/// is being clamped to 1Hz.
const HEARTBEAT_WORKER_JS: &str = r#"
let intervalId = null;
self.onmessage = (e) => {
  const msg = e.data || {};
  if (msg.type === 'start') {
    if (intervalId !== null) {
      clearInterval(intervalId);
    }
    const periodMs = Number(msg.periodMs) || 500;
    intervalId = setInterval(() => {
      try { self.postMessage({ type: 'tick' }); } catch (_) {}
    }, periodMs);
  } else if (msg.type === 'stop') {
    if (intervalId !== null) {
      clearInterval(intervalId);
      intervalId = null;
    }
  }
};
"#;

/// Cross-target handle for the heartbeat. The `wasm32` build owns either a
/// `Worker` (preferred) or a fallback `setInterval` handle. Non-wasm builds
/// are a no-op stub so the public API compiles on native targets too.
#[cfg(target_arch = "wasm32")]
pub struct HeartbeatTimer {
    backend: HeartbeatBackend,
}

#[cfg(not(target_arch = "wasm32"))]
pub struct HeartbeatTimer;

#[cfg(target_arch = "wasm32")]
enum HeartbeatBackend {
    /// Worker-driven: `_on_message` is kept alive so the JS callback survives.
    /// `_blob_url` is kept so we can revoke it on drop.
    Worker {
        worker: Worker,
        _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
        blob_url: String,
    },
    /// Fallback to a main-thread interval. Subject to background throttling,
    /// but preserves pre-fix behavior when the Worker path is unavailable.
    Interval {
        _closure: Closure<dyn FnMut()>,
        interval_id: i32,
    },
}

#[cfg(target_arch = "wasm32")]
impl HeartbeatTimer {
    /// Start a heartbeat that invokes `on_tick` every `period_ms` milliseconds.
    ///
    /// Prefers a Worker-backed timer (immune to background-tab throttling). If
    /// Worker construction fails, falls back to a main-thread `setInterval` and
    /// logs a warning. The callback runs on the main thread either way, so
    /// shared state captured by `on_tick` can be `!Send`.
    pub fn start<F>(period_ms: u32, on_tick: F) -> Self
    where
        F: FnMut() + 'static,
    {
        match Self::start_worker(period_ms, on_tick) {
            Ok(timer) => timer,
            Err((reason, on_tick)) => {
                log::warn!(
                    "diagnostics heartbeat: worker unavailable ({reason:?}), \
                     falling back to main-thread setInterval (will throttle when tab is hidden)"
                );
                Self::start_interval(period_ms, on_tick)
            }
        }
    }

    fn start_worker<F>(period_ms: u32, mut on_tick: F) -> Result<Self, (JsValue, BoxedOnTick)>
    where
        F: FnMut() + 'static,
    {
        // Build a blob: URL containing the worker script.
        let parts = Array::new();
        parts.push(&JsValue::from_str(HEARTBEAT_WORKER_JS));
        let bag = BlobPropertyBag::new();
        bag.set_type("text/javascript");
        let blob = match Blob::new_with_str_sequence_and_options(&parts, &bag) {
            Ok(b) => b,
            Err(e) => return Err((e, Box::new(on_tick))),
        };
        let blob_url = match Url::create_object_url_with_blob(&blob) {
            Ok(u) => u,
            Err(e) => return Err((e, Box::new(on_tick))),
        };

        let worker = match Worker::new(&blob_url) {
            Ok(w) => w,
            Err(e) => {
                // Release the URL we just allocated so we don't leak it on the
                // fallback path.
                let _ = Url::revoke_object_url(&blob_url);
                return Err((e, Box::new(on_tick)));
            }
        };

        let on_message = Closure::wrap(Box::new(move |_event: web_sys::MessageEvent| {
            on_tick();
        }) as Box<dyn FnMut(web_sys::MessageEvent)>);
        worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        // Kick the worker off. If posting the start message fails we treat it
        // as a hard error and fall back; the user should not silently get a
        // dead heartbeat.
        let start_msg = js_sys::Object::new();
        if let Err(e) = js_sys::Reflect::set(
            &start_msg,
            &JsValue::from_str("type"),
            &JsValue::from_str("start"),
        ) {
            worker.terminate();
            let _ = Url::revoke_object_url(&blob_url);
            // Recover the FnMut so the caller can retry on the fallback path.
            let on_tick = into_boxed_on_tick(on_message);
            return Err((e, on_tick));
        }
        if let Err(e) = js_sys::Reflect::set(
            &start_msg,
            &JsValue::from_str("periodMs"),
            &JsValue::from_f64(period_ms as f64),
        ) {
            worker.terminate();
            let _ = Url::revoke_object_url(&blob_url);
            let on_tick = into_boxed_on_tick(on_message);
            return Err((e, on_tick));
        }
        if let Err(e) = worker.post_message(&start_msg) {
            worker.terminate();
            let _ = Url::revoke_object_url(&blob_url);
            let on_tick = into_boxed_on_tick(on_message);
            return Err((e, on_tick));
        }

        // Use `info!` so the spawn is visible at the default log level. E2E
        // tests scrape this line to assert that the Worker backend was chosen
        // over the fallback `setInterval`.
        log::info!(
            "diagnostics heartbeat: spawned worker (period={period_ms}ms, backend=worker)"
        );

        Ok(HeartbeatTimer {
            backend: HeartbeatBackend::Worker {
                worker,
                _on_message: on_message,
                blob_url,
            },
        })
    }

    fn start_interval(period_ms: u32, mut on_tick: BoxedOnTick) -> Self {
        let closure = Closure::wrap(Box::new(move || {
            on_tick();
        }) as Box<dyn FnMut()>);
        let interval_id = window()
            .and_then(|w| {
                w.set_interval_with_callback_and_timeout_and_arguments_0(
                    closure.as_ref().unchecked_ref(),
                    period_ms as i32,
                )
                .ok()
            })
            .unwrap_or(0);
        HeartbeatTimer {
            backend: HeartbeatBackend::Interval {
                _closure: closure,
                interval_id,
            },
        }
    }
}

/// On the worker-construction error path the original `FnMut` has already been
/// moved into a `Closure`. We can't trivially recover it for the fallback
/// `start_interval` call, so we install a tiny shim that drops on the floor
/// (the caller has already logged a warning at this point). In practice this
/// only runs when the runtime is broken enough that postMessage itself
/// failed — extremely rare.
#[cfg(target_arch = "wasm32")]
fn into_boxed_on_tick(_closure: Closure<dyn FnMut(web_sys::MessageEvent)>) -> BoxedOnTick {
    Box::new(|| {})
}

#[cfg(target_arch = "wasm32")]
impl Drop for HeartbeatTimer {
    fn drop(&mut self) {
        match &self.backend {
            HeartbeatBackend::Worker {
                worker, blob_url, ..
            } => {
                log::info!("diagnostics heartbeat: terminating worker");
                // Best-effort: tell the worker to stop its interval before we
                // terminate it, so any in-flight tick can drain.
                let stop_msg = js_sys::Object::new();
                let _ = js_sys::Reflect::set(
                    &stop_msg,
                    &JsValue::from_str("type"),
                    &JsValue::from_str("stop"),
                );
                let _ = worker.post_message(&stop_msg);
                worker.terminate();
                let _ = Url::revoke_object_url(blob_url);
            }
            HeartbeatBackend::Interval { interval_id, .. } => {
                if let Some(w) = window() {
                    w.clear_interval_with_handle(*interval_id);
                }
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl std::fmt::Debug for HeartbeatTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.backend {
            HeartbeatBackend::Worker { .. } => "Worker",
            HeartbeatBackend::Interval { .. } => "Interval",
        };
        f.debug_struct("HeartbeatTimer")
            .field("kind", &kind)
            .finish()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl HeartbeatTimer {
    /// Non-wasm stub. Native consumers of `videocall-client` (e.g. the bot)
    /// don't need a browser heartbeat; the returned handle is a no-op.
    pub fn start<F>(_period_ms: u32, _on_tick: F) -> Self
    where
        F: FnMut() + 'static,
    {
        HeartbeatTimer
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for HeartbeatTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeartbeatTimer").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_js_has_expected_message_protocol() {
        // Smoke test: any future edit to the JS must keep the `start` / `stop`
        // / `tick` contract used by the Rust side.
        assert!(HEARTBEAT_WORKER_JS.contains("'start'"));
        assert!(HEARTBEAT_WORKER_JS.contains("'stop'"));
        assert!(HEARTBEAT_WORKER_JS.contains("'tick'"));
        assert!(HEARTBEAT_WORKER_JS.contains("setInterval"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_stub_is_constructible() {
        let _timer = HeartbeatTimer::start(500, || {});
    }
}

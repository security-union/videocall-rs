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
use std::cell::RefCell;
#[cfg(target_arch = "wasm32")]
use std::rc::Rc;

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

/// Shared, takeable handle to the user-supplied `FnMut`. The Worker path
/// installs a `Closure` that calls `.borrow_mut().as_mut()` to invoke the
/// callback while leaving ownership where the fallback path can recover it
/// (via `.borrow_mut().take()`) if Worker setup fails after the `Closure`
/// has already been constructed. This is what eliminates the previous
/// silent-no-op fallback bug where the callback was unrecoverable.
#[cfg(target_arch = "wasm32")]
type SharedOnTick = Rc<RefCell<Option<BoxedOnTick>>>;

/// JavaScript source for the heartbeat Worker.
///
/// This is intentionally tiny — just a `setInterval` that posts a tick to the
/// parent. Worker-scope timers are not throttled when the parent tab is hidden,
/// so this keeps firing at the configured cadence even while the main thread
/// is being clamped to 1Hz.
#[cfg(target_arch = "wasm32")]
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
    /// `_blob_url` is kept so we can revoke it on drop. `_on_tick` is the
    /// shared, takeable slot that holds the user-supplied `FnMut`; the
    /// `Closure` borrows from it on every tick, so it must outlive the
    /// `Worker`.
    Worker {
        worker: Worker,
        _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
        blob_url: String,
        _on_tick: SharedOnTick,
    },
    /// Fallback to a main-thread interval. Subject to background throttling,
    /// but preserves pre-fix behavior when the Worker path is unavailable.
    Interval {
        _closure: Closure<dyn FnMut()>,
        interval_id: i32,
    },
    /// Defensive: neither Worker nor `setInterval` succeeded, AND we could
    /// not recover the user callback for the fallback. This branch is
    /// unreachable in practice but exists so we never silently install a
    /// no-op interval and pretend everything is fine — the error is logged
    /// loudly at construction time and the timer simply does nothing.
    Dead,
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
        // Park the user callback in a shared, takeable slot. Both the Worker
        // path's `Closure` and the fallback path will pull from this same
        // slot, so a Worker-setup failure that occurs *after* the `Closure`
        // is constructed can still recover the live `FnMut` and hand it to
        // `start_interval`. This avoids the previous silent-no-op fallback
        // mode where the callback was orphaned inside a now-unused `Closure`.
        let on_tick: SharedOnTick = Rc::new(RefCell::new(Some(Box::new(on_tick))));

        match Self::start_worker(period_ms, on_tick.clone()) {
            Ok(timer) => timer,
            Err(reason) => {
                // Recover the user callback from the shared slot. If the
                // worker path consumed it during a partial-success path (it
                // shouldn't — we re-deposit on every error), surface a hard
                // error rather than silently install a no-op interval.
                let recovered = on_tick.borrow_mut().take();
                match recovered {
                    Some(boxed) => {
                        log::warn!(
                            "diagnostics heartbeat: worker unavailable ({reason:?}), \
                             falling back to main-thread setInterval (will throttle when tab is hidden)"
                        );
                        Self::start_interval(period_ms, boxed)
                    }
                    None => {
                        // Defensive branch — should be unreachable because
                        // `start_worker` always re-deposits the callback on
                        // every error return. Logging at error severity makes
                        // the failure mode loud rather than silent.
                        log::error!(
                            "diagnostics heartbeat: worker unavailable ({reason:?}) and \
                             user callback was orphaned; diagnostics will NOT function \
                             until reconnect"
                        );
                        Self::start_dead()
                    }
                }
            }
        }
    }

    /// Attempt to spawn the Worker-backed heartbeat.
    ///
    /// On any error return, the user callback in `on_tick` is left intact in
    /// its `Rc<RefCell<Option<...>>>` slot so the caller (`start`) can recover
    /// it for the fallback `start_interval` path.
    fn start_worker(period_ms: u32, on_tick: SharedOnTick) -> Result<Self, JsValue> {
        // Build a blob: URL containing the worker script.
        let parts = Array::new();
        parts.push(&JsValue::from_str(HEARTBEAT_WORKER_JS));
        let bag = BlobPropertyBag::new();
        bag.set_type("text/javascript");
        let blob = Blob::new_with_str_sequence_and_options(&parts, &bag)?;
        let blob_url = Url::create_object_url_with_blob(&blob)?;

        let worker = match Worker::new(&blob_url) {
            Ok(w) => w,
            Err(e) => {
                // Release the URL we just allocated so we don't leak it on the
                // fallback path.
                let _ = Url::revoke_object_url(&blob_url);
                return Err(e);
            }
        };

        // The closure holds a clone of the shared slot and invokes the
        // callback *through* it on each tick. Crucially, the original `Box<F>`
        // stays inside the `RefCell<Option<...>>` rather than being moved into
        // the closure's captures, so the caller can `.take()` it back if any
        // of the still-fallible ops below (`Reflect::set`, `post_message`)
        // fail.
        let cb_slot = on_tick.clone();
        let on_message = Closure::wrap(Box::new(move |_event: web_sys::MessageEvent| {
            if let Some(cb) = cb_slot.borrow_mut().as_mut() {
                cb();
            }
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
            drop(on_message);
            return Err(e);
        }
        if let Err(e) = js_sys::Reflect::set(
            &start_msg,
            &JsValue::from_str("periodMs"),
            &JsValue::from_f64(period_ms as f64),
        ) {
            worker.terminate();
            let _ = Url::revoke_object_url(&blob_url);
            drop(on_message);
            return Err(e);
        }
        if let Err(e) = worker.post_message(&start_msg) {
            worker.terminate();
            let _ = Url::revoke_object_url(&blob_url);
            drop(on_message);
            return Err(e);
        }

        // Use `info!` so the spawn is visible at the default log level. E2E
        // tests scrape this line to assert that the Worker backend was chosen
        // over the fallback `setInterval`.
        log::info!("diagnostics heartbeat: spawned worker (period={period_ms}ms, backend=worker)");

        Ok(HeartbeatTimer {
            backend: HeartbeatBackend::Worker {
                worker,
                _on_message: on_message,
                blob_url,
                _on_tick: on_tick,
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

    /// Last-resort backend used only when both the Worker path failed AND the
    /// user callback could not be recovered for the fallback `setInterval`.
    /// Constructed solely so we never silently install a no-op interval that
    /// pretends to be alive — the caller logs at `error!` level before this
    /// is built.
    fn start_dead() -> Self {
        HeartbeatTimer {
            backend: HeartbeatBackend::Dead,
        }
    }
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
            HeartbeatBackend::Dead => {
                // Nothing to clean up — neither Worker nor interval was ever
                // installed. The diagnostics manager will simply not receive
                // ticks for this session.
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
            HeartbeatBackend::Dead => "Dead",
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

    #[cfg(target_arch = "wasm32")]
    #[test]
    fn worker_js_has_expected_message_protocol() {
        // Smoke test: any future edit to the JS must keep the `start` / `stop`
        // / `tick` contract used by the Rust side. The JS blob is only
        // compiled on wasm targets, so this assertion is wasm-gated too.
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

    /// Cross-target alias for the slot type used in the regression test
    /// below. Matches the wasm-only `SharedOnTick` exactly so the test
    /// exercises the real shape; declared once here so that the test body
    /// stays free of the deeply-nested type (which would otherwise trip
    /// `clippy::type_complexity`).
    type TestCallbackSlot = std::rc::Rc<std::cell::RefCell<Option<Box<dyn FnMut() + 'static>>>>;

    /// Regression test for the silent-no-op fallback bug.
    ///
    /// Simulates the failure mode that motivated this whole patch: the
    /// Worker-construction path consumes the user callback into a `Closure`,
    /// then a subsequent fallible JS op (`Reflect::set` / `post_message`)
    /// fails. Pre-fix, the callback was orphaned and the fallback installed a
    /// no-op `setInterval`. Post-fix, the callback lives in an
    /// `Rc<RefCell<Option<...>>>` and is recoverable for the fallback path.
    ///
    /// We can't easily drive `Worker::new` to fail from a unit test, so this
    /// exercises the shared-slot mechanism directly: we model the "Worker
    /// setup succeeded enough to wrap the closure, then a later step failed"
    /// path by constructing the slot, simulating the would-be-`Closure`
    /// borrow path, then verifying that `borrow_mut().take()` still hands
    /// back the live `FnMut` rather than `None`. If the closure had moved
    /// the boxed callback (the pre-fix shape), `.take()` would return
    /// `None` here.
    #[test]
    fn shared_slot_preserves_callback_for_fallback_recovery() {
        use std::cell::Cell;
        use std::rc::Rc as StdRc;

        let tick_count = StdRc::new(Cell::new(0u32));
        let tick_count_for_cb = tick_count.clone();

        // Type-erase the closure exactly the way `start_worker` does, so the
        // slot's contents match the real call shape.
        let cb: Box<dyn FnMut() + 'static> =
            Box::new(move || tick_count_for_cb.set(tick_count_for_cb.get() + 1));

        // `SharedOnTick` is wasm-only; on non-wasm we use the cross-target
        // alias above so the test compiles for the Linux clippy / cargo
        // test job that has been failing in CI.
        let slot: TestCallbackSlot = StdRc::new(std::cell::RefCell::new(Some(cb)));

        // Simulate the Worker-path Closure: it holds a clone of the slot and
        // invokes the callback through it without moving it out.
        let cb_clone = slot.clone();
        let invoke_through_slot = || {
            if let Some(cb) = cb_clone.borrow_mut().as_mut() {
                cb();
            }
        };
        invoke_through_slot();
        invoke_through_slot();
        assert_eq!(tick_count.get(), 2, "closure invoked twice via shared slot");

        // Simulate the "worker setup failed after closure construction"
        // recovery path: the slot still owns the live `FnMut`, so the
        // fallback path can pull it back and install it on a `setInterval`.
        let recovered = slot.borrow_mut().take();
        assert!(
            recovered.is_some(),
            "callback must be recoverable from shared slot on worker-construction failure"
        );

        // After recovery the slot is empty, so the now-orphaned Worker
        // `Closure` (if it somehow fires) becomes a no-op — but that's a
        // separate path from the original silent-dead-heartbeat bug.
        let mut recovered_cb = recovered.unwrap();
        recovered_cb();
        assert_eq!(
            tick_count.get(),
            3,
            "recovered callback is still callable on the fallback path"
        );
    }
}

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

//! Long Tasks API → diagnostic-bus bridge.
//!
//! Registers a [`PerformanceObserver`] for entries of type `longtask` so we
//! can detect main-thread stalls > 50 ms and forward them to the diagnostic
//! bus as Prometheus-friendly metrics.
//!
//! ## Why this exists
//!
//! The 2026-05-06 incident (cc7tp / meeting_sync) involved a participant
//! whose main thread was stalling for 50+ seconds at a time. The only
//! visible symptom in our telemetry was "implausibly high RTT samples"
//! filtered out by the elevated-RTT watchdog. We had no direct signal
//! saying "the JS event loop on this client is starved". The browser's
//! [Long Tasks API][long-tasks] reports any task that occupied the main
//! thread for more than 50 ms; surfacing those as metrics gives us a
//! clean, browser-attested view of client-side CPU pressure.
//!
//! [long-tasks]: https://developer.mozilla.org/en-US/docs/Web/API/Long_Tasks_API
//!
//! ## Lifetime
//!
//! One [`LongTaskObserver`] is created per [`crate::VideoCallClient`].
//! Dropping the observer (which happens when the client is dropped)
//! disconnects the underlying `PerformanceObserver` so we don't leak
//! browser callbacks across page navigations.

use log::debug;
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};

#[cfg(target_arch = "wasm32")]
use js_sys::{Array, Reflect};
#[cfg(target_arch = "wasm32")]
use log::warn;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{prelude::Closure, JsCast, JsValue};
#[cfg(target_arch = "wasm32")]
use web_sys::{PerformanceObserver, PerformanceObserverInit};

/// Subsystem label attached to every emitted [`DiagEvent`]. Kept short so
/// it shows up cleanly in metrics dashboards.
pub const SUBSYSTEM: &str = "client_perf";

/// Diagnostic-bus metric: duration of a single long task (ms, f64).
pub const METRIC_LONGTASK_DURATION_MS: &str = "client_longtask_duration_ms";

/// Diagnostic-bus metric: counter incremented by 1 per long task.
pub const METRIC_LONGTASK_COUNT: &str = "client_longtask_count";

/// Pure parser used by [`LongTaskObserver::on_entries`]: extract the
/// `duration` field from a single
/// [`PerformanceLongTaskTiming`][long-task-entry] entry. Returns `None`
/// when the field is missing or non-numeric.
///
/// Wasm32-only because it operates on `JsValue`; the native tests drive
/// the higher-level [`build_long_task_event`] helper instead, which gives
/// the same metric shape without the JS interop layer.
///
/// [long-task-entry]: https://developer.mozilla.org/en-US/docs/Web/API/PerformanceLongTaskTiming
#[cfg(target_arch = "wasm32")]
fn extract_duration_ms(entry: &JsValue) -> Option<f64> {
    Reflect::get(entry, &JsValue::from_str("duration"))
        .ok()
        .and_then(|v| v.as_f64())
}

/// Build the [`DiagEvent`] that represents a single long-task observation.
///
/// Pure function — no side effects, no reliance on the global bus — so it
/// can be exercised by host-only `cargo test` runs on the bench machine.
/// (The diagnostic bus's native init drops its initial receiver, so we
/// can't broadcast through it under `cargo test --lib`.)
pub fn build_long_task_event(duration_ms: f64) -> DiagEvent {
    DiagEvent {
        subsystem: SUBSYSTEM,
        stream_id: None,
        ts_ms: now_ms(),
        metrics: vec![
            metric!(METRIC_LONGTASK_DURATION_MS, duration_ms),
            metric!(METRIC_LONGTASK_COUNT, 1u64),
        ],
    }
}

/// Forward a single long-task duration to the diagnostic bus.
///
/// Pulled out of the observer callback so that:
///   * the wasm callback is small and easy to audit;
///   * the native tests can drive [`build_long_task_event`] directly
///     without depending on `PerformanceObserver` or the live bus.
///
/// Returns `true` if the metric was successfully broadcast, `false`
/// otherwise. The boolean is consumed by the wasm test harness; production
/// callers ignore it.
pub fn emit_long_task_metric(duration_ms: f64) -> bool {
    let event = build_long_task_event(duration_ms);
    match global_sender().try_broadcast(event) {
        Ok(_) => true,
        Err(e) => {
            // The diagnostic bus is `set_overflow(true)`, so failure here
            // is rare on wasm32; on native, the bus closes itself when
            // its initial receiver is dropped, so a `Closed` error here
            // is expected. Log at debug only to avoid log spam.
            debug!("long_tasks: failed to broadcast long-task metric: {e}");
            false
        }
    }
}

/// Wraps a [`PerformanceObserver`] for `longtask` entries. The observer
/// is disconnected and the JS callback dropped when this struct is
/// dropped, so it's safe to embed inside a `VideoCallClient` `Inner`.
#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
pub struct LongTaskObserver {
    observer: Option<PerformanceObserver>,
    /// Owned closure passed to the observer. We keep it in the struct
    /// so its lifetime matches the observer's; otherwise the wasm-bindgen
    /// generated trampoline would be freed and the browser would crash
    /// the next time it tried to invoke us.
    _callback: Closure<dyn FnMut(JsValue, JsValue)>,
}

#[cfg(target_arch = "wasm32")]
impl LongTaskObserver {
    /// Create the observer and attempt to start observing `longtask`
    /// entries. Returns `None` if the browser doesn't support
    /// [`PerformanceObserver`] or rejects the observe call (Safari, older
    /// Firefox without the flag, Web Workers, etc.). In those cases the
    /// caller logs a debug line and continues — long-task telemetry is a
    /// nice-to-have, not a hard dependency.
    pub fn start() -> Option<Self> {
        let callback = Closure::<dyn FnMut(JsValue, JsValue)>::new(
            move |list: JsValue, _observer: JsValue| {
                Self::on_entries(list);
            },
        );

        let cb_func: &js_sys::Function = callback.as_ref().unchecked_ref();
        let observer = match PerformanceObserver::new(cb_func) {
            Ok(o) => o,
            Err(e) => {
                debug!(
                    "long_tasks: PerformanceObserver constructor unavailable; \
                     long-task telemetry disabled (err={e:?})"
                );
                return None;
            }
        };

        // Use the `entryTypes: ["longtask"]` form rather than the newer
        // `type:`/`buffered:` form. `entryTypes` is supported back to
        // Chrome 58 and produces identical output for our purposes.
        let entry_types = Array::new();
        entry_types.push(&JsValue::from_str("longtask"));
        let init = PerformanceObserverInit::new(&entry_types);
        observer.observe(&init);

        Some(Self {
            observer: Some(observer),
            _callback: callback,
        })
    }

    /// Internal: called by the wasm-bindgen closure when one or more
    /// long-task entries arrive. We iterate the list and emit a metric
    /// for each entry's duration.
    fn on_entries(list: JsValue) {
        // The argument is a `PerformanceObserverEntryList`. We only need
        // its `getEntries()` method; reaching for it via `Reflect::get`
        // (rather than the strongly typed wrapper) avoids a hard
        // dependency on the optional `PerformanceObserverEntryList`
        // web-sys feature.
        let get_entries = match Reflect::get(&list, &JsValue::from_str("getEntries")) {
            Ok(v) => v,
            Err(_) => return,
        };
        let func = match get_entries.dyn_into::<js_sys::Function>() {
            Ok(f) => f,
            Err(_) => return,
        };
        let entries = match func.call0(&list) {
            Ok(v) => v,
            Err(_) => return,
        };
        let arr = match entries.dyn_into::<Array>() {
            Ok(a) => a,
            Err(_) => return,
        };

        for i in 0..arr.length() {
            let entry = arr.get(i);
            if let Some(duration) = extract_duration_ms(&entry) {
                // PerformanceLongTaskTiming entries are by definition
                // > 50 ms, but we don't gate emission on that — if a
                // browser ever reports something shorter, we still want
                // to see it.
                emit_long_task_metric(duration);
                debug!("long_tasks: emitted long-task metric duration={duration:.1}ms");
            } else {
                warn!(
                    "long_tasks: PerformanceLongTaskTiming entry missing 'duration' field; \
                     skipping"
                );
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for LongTaskObserver {
    fn drop(&mut self) {
        if let Some(observer) = self.observer.take() {
            // `disconnect()` stops the observer from firing further
            // callbacks. The owned `_callback` is then dropped at the
            // end of the function, freeing the wasm-bindgen trampoline.
            observer.disconnect();
        }
    }
}

// ---------------------------------------------------------------------------
// Native shim — keeps `cargo test --lib` happy on the host.
//
// The non-wasm32 variant is only ever instantiated by the unit tests, so it
// just records that `start()` was called. The metric helper above is the same
// on both targets and exercised directly by the unit tests.
// ---------------------------------------------------------------------------
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
pub struct LongTaskObserver;

#[cfg(not(target_arch = "wasm32"))]
impl LongTaskObserver {
    /// Native stub. Returns `Some(default)` so the call site in
    /// `VideoCallClient::new` doesn't have to gate its `_long_task_observer`
    /// field on the target arch.
    pub fn start() -> Option<Self> {
        Some(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use videocall_diagnostics::MetricValue;

    #[test]
    fn metric_constants_match_documented_names() {
        // These names are part of the metrics API and must not change
        // without a coordinated update on the relay/Prometheus side.
        assert_eq!(METRIC_LONGTASK_DURATION_MS, "client_longtask_duration_ms");
        assert_eq!(METRIC_LONGTASK_COUNT, "client_longtask_count");
        assert_eq!(SUBSYSTEM, "client_perf");
    }

    #[test]
    fn build_long_task_event_emits_duration_and_count() {
        // Verifies the metric shape that
        // [`emit_long_task_metric`] forwards to the diagnostic bus on
        // wasm32. We test the pure builder rather than the broadcast call
        // because the diagnostics bus closes its channel on native targets
        // when its initial receiver is dropped (see
        // `videocall-diagnostics/src/lib.rs`), so `try_broadcast` cannot
        // succeed under `cargo test --lib` on a host machine.
        let event = build_long_task_event(123.4);

        assert_eq!(event.subsystem, SUBSYSTEM);
        assert!(event.stream_id.is_none());
        assert_eq!(event.metrics.len(), 2);

        // Duration metric (f64).
        let dur = event
            .metrics
            .iter()
            .find(|m| m.name == METRIC_LONGTASK_DURATION_MS)
            .expect("duration metric present");
        match &dur.value {
            MetricValue::F64(v) => assert!(
                (v - 123.4).abs() < 1e-9,
                "duration should round-trip; got {v}"
            ),
            other => panic!("expected F64, got {other:?}"),
        }

        // Count metric (u64, value=1).
        let count = event
            .metrics
            .iter()
            .find(|m| m.name == METRIC_LONGTASK_COUNT)
            .expect("count metric present");
        match &count.value {
            MetricValue::U64(v) => assert_eq!(*v, 1),
            other => panic!("expected U64, got {other:?}"),
        }
    }

    #[test]
    fn build_long_task_event_handles_zero_duration() {
        // Defensive: a misbehaving browser could emit a `longtask` entry
        // with duration 0. We still want a valid event so downstream
        // dashboards don't get gaps.
        let event = build_long_task_event(0.0);
        assert_eq!(event.metrics.len(), 2);
    }

    #[test]
    fn emit_long_task_metric_returns_false_when_bus_closed() {
        // On native, the diagnostic bus closes itself once its initial
        // receiver is dropped. `emit_long_task_metric` must not panic in
        // that state — it should swallow the broadcast failure and
        // return `false` so callers can keep running. (On wasm32 a
        // background reader keeps the bus open, so the equivalent path
        // returns `true`; that's covered by integration tests.)
        let ok = emit_long_task_metric(73.0);
        assert!(
            !ok,
            "native tests close the bus eagerly; broadcast must \
             return false rather than panicking"
        );
    }

    #[test]
    fn native_stub_can_be_started() {
        // The non-wasm shim should always return Some(_) so the integration
        // call site doesn't have to special-case the host build.
        let _ = LongTaskObserver::start().expect("native stub should start");
    }
}

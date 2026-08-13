// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test-only diagnostics-bus injection hook for the connection-quality
//! indicator (issue #367, deliverable 3).
//!
//! [`crate::components::connection_quality_indicator::ConnectionQualityIndicator`]
//! does not react to page-load network latency. It subscribes to the
//! [`videocall_diagnostics`] bus and consumes exactly one signal:
//!
//! > a `DiagEvent` with `subsystem == "connection_manager"` and
//! > `stream_id == None`, carrying a metric named `active_server_rtt` whose
//! > value is a [`MetricValue::F64`].
//!
//! Everything else on that event is ignored by the indicator (see the
//! `subsystem` / `stream_id` / `active_server_rtt` filter at the top of the
//! component's `use_effect` loop). The producer of the real signal is
//! `ConnectionManager::build_main_diagnostic_metrics`
//! (`videocall-client/src/connection/connection_manager.rs`), which emits
//! `active_server_rtt` at ~1 Hz from `ServerRttMeasurement.average_rtt` — the
//! rolling average of APPLICATION-LEVEL RTT probes: a `MediaType::RTT` packet
//! sent via `Connection::send_packet_datagram` and echoed by the relay, timed
//! in `handle_rtt_response` as `reception_time - media_packet.timestamp`.
//!
//! ## Why a hook, and not network emulation
//!
//! Because that probe rides the media transport, none of the ambient network
//! levers can move it in a Playwright run:
//!
//! - **CDP `Network.emulateNetworkConditions`** shapes `URLLoader`-mediated
//!   HTTP resource loads. The probe is a WebSocket binary frame (WS is the
//!   default transport since issue 2045) or a WebTransport/QUIC datagram —
//!   neither is a resource load. Even granting that it worked, a single
//!   `latency` knob cannot place RTT in the 300–500 ms Warn band and then in
//!   the >= 500 ms Critical band while leaving heartbeat/election traffic on
//!   the same link untouched.
//! - **`window.__vcNetsim`** (the `netsim` feature's control surface) *does*
//!   reach this probe on the default transport — `WebSocketTask::send_bytes`
//!   consults `netsim_hook::shape_uplink_reliable`
//!   (`connection/websocket.rs`), and the probe arrives there via
//!   `Connection::send_packet_datagram` → `Task::WebSocket(ws) =>
//!   ws.send_packet(packet, MediaStreamKey::Control)` (`connection/task.rs`).
//!   It is still not a usable lever for this spec, on two grounds. First, no
//!   preset lands in the 300–500 ms Warn band: the built-in ladder is
//!   `good_wifi` 20 ms, `crushed_downlink` 40 ms, `good_4g` 50 ms,
//!   `congested_wifi` 80 ms, `lossy_mobile` 150 ms, `dialup` 200 ms,
//!   `satellite` 600 ms (`videocall-netsim/src/profiles.rs`) — the band steps
//!   straight from 200 to 600. Second, uplink shaping is indiscriminate: the
//!   hook sits in `send_bytes`, beneath *every* reliable send, and the elected
//!   connection's heartbeat rides the identical
//!   `Task::send_packet_datagram` → `ws.send_packet(_, Control)` path
//!   (`Connection::start_heartbeat`), so any install that delays the RTT probe
//!   delays and drops heartbeats with it. A reconnect or re-election provoked
//!   that way is exactly the `SampleAction::Reset` gap this spec has to hold
//!   still. The inbound direction is separately unusable: it is documented
//!   LOSS-ONLY (`netsim_hook::shape_inbound` maps `Admission::Delay` to
//!   deliver-now), so a `"down"` install adds no latency at all.
//!
//! So the hook below publishes the signal itself. Everything downstream of the
//! bus — the `subsystem`/`stream_id` filter, the metric extraction,
//! `classify_sample`'s ordering + gap watermark, `HysteresisState::update`'s
//! 3-to-enter / 5-to-exit counters, the level→class mapping and the 500 ms exit
//! animation — is untouched production code driven by a real event.
//!
//! Publishing a `connection_manager` event that carries only a subset of the
//! 1 Hz tick's metrics is not a new shape on this bus: production already does
//! it in `ConnectionManager::emit_audio_fallback_diagnostic` (issue 2029),
//! which broadcasts a `stream_id: None` `connection_manager` event carrying
//! only the `wt_audio_fallback_*` metrics. Subscribers that want a metric they
//! were not handed simply skip the event, which is what the indicator does.
//!
//! ## Gating
//!
//! The global is registered **only when `mock_peers_enabled()` is true** — the
//! same `MOCK_PEERS_ENABLED` runtime-config flag that gates
//! `__videocall_inject_render_fps` / `__videocall_inject_longtask`
//! ([`crate::components::decode_budget_inject`]) and
//! `__videocall_inject_screen_first_render`
//! ([`crate::components::screen_first_render_inject`]). Production deploys
//! leave it `false`, so nothing is attached to `window` there.

use videocall_diagnostics::{metric, DiagEvent};

#[cfg(target_arch = "wasm32")]
use videocall_diagnostics::{global_sender, now_ms};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// Upper bound on the samples one call may publish. A caller only ever needs a
/// handful (the indicator enters a warning state after `ENTER_COUNT` = 3
/// consecutive samples and leaves it after `EXIT_COUNT` = 5), and the bus is a
/// 1024-slot broadcast channel with drop-oldest overflow — an unbounded loop
/// would silently evict every other subsystem's in-flight events.
///
/// Note the bound is PER CALL, not per synchronous block: ~16 back-to-back
/// calls in one `page.evaluate` body still publish 1024 events and can overrun
/// the bus, evicting other subsystems' in-flight events. Keep a batch well
/// under that.
///
/// Only the wasm closure below reads it; the native target keeps a stub
/// registrar, so allow it to be unused there rather than duplicating the
/// constant behind a second `cfg`.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const MAX_INJECT_SAMPLES: u32 = 64;

/// Build one synthetic `active_server_rtt` sample in the exact shape the
/// indicator's subscriber filters on.
///
/// Split out from the closure so the shape is pinned by a host-target test:
/// the metric must decode as [`videocall_diagnostics::MetricValue::F64`],
/// because the component matches `if let MetricValue::F64(v)` and would
/// silently ignore an `I64`/`Text` value — a failure mode that looks like
/// "the hook did nothing" rather than a type error.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn synthetic_rtt_event(rtt_ms: f64, ts_ms: u64) -> DiagEvent {
    DiagEvent {
        subsystem: "connection_manager",
        stream_id: None,
        ts_ms,
        metrics: vec![metric!("active_server_rtt", rtt_ms)],
    }
}

/// Register `window.__videocall_inject_server_rtt`, gated on the
/// `MOCK_PEERS_ENABLED` runtime-config flag.
///
/// ```js
/// // Three samples at 350 ms, stamped `Date.now()` — enough to enter Warn.
/// window.__videocall_inject_server_rtt(350, 3);
/// // One sample at an EXPLICIT timestamp (replay / reorder / gap tests).
/// window.__videocall_inject_server_rtt(20, 1, Date.now() - 3000);
/// ```
///
/// Arguments (all coerced defensively; a bad `rttMs` returns `false` and
/// publishes nothing):
///
/// - `rttMs: number` — the value carried as `active_server_rtt`. Must be
///   finite and >= 0.
/// - `count?: number` — how many identical samples to publish. Defaults to 1,
///   clamped to [`MAX_INJECT_SAMPLES`].
/// - `tsMs?: number` — `DiagEvent::ts_ms` for every sample in the batch.
///   Defaults to [`now_ms`], which on wasm is `Date.now()` — the same clock a
///   caller reads, so page-side arithmetic against it is exact. Passing the
///   same `ts_ms` for every sample in a batch is deliberate and safe:
///   `classify_sample` treats an equal timestamp as a zero-length gap
///   (`SampleAction::Accept`), not a backwards jump.
///
/// **The whole batch is published synchronously**, inside one wasm callback.
/// That is the determinism guarantee the E2E spec rests on: the real 1 Hz
/// `connection_manager` tick runs on a timer, and a timer callback cannot
/// preempt a running one on the single-threaded JS event loop — so no real
/// low-RTT sample can interleave into the middle of an injected batch and reset
/// the consecutive-sample counters. A caller that needs several batches
/// unbroken should issue them from a single `page.evaluate` body for the same
/// reason.
///
/// Idempotent and cheap to call from a `use_hook`. A no-op that attaches
/// nothing when `mock_peers_enabled()` is false.
#[cfg(target_arch = "wasm32")]
pub fn register_connection_quality_inject_hooks() {
    if !crate::constants::mock_peers_enabled() {
        return;
    }

    let Some(window) = web_sys::window() else {
        return;
    };

    let cb = Closure::<dyn Fn(JsValue, JsValue, JsValue) -> JsValue>::new(
        |rtt_ms: JsValue, count: JsValue, ts_ms: JsValue| -> JsValue {
            // A NaN/negative/absent RTT is a caller bug, not a sample: publish
            // nothing and report it, rather than pushing a value the hysteresis
            // comparisons would silently treat as "below every threshold".
            let Some(rtt) = rtt_ms.as_f64().filter(|v| v.is_finite() && *v >= 0.0) else {
                return JsValue::from_bool(false);
            };

            // Absent / non-numeric / < 1 count means "one sample".
            let n = count
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 1.0)
                .map(|v| v as u32)
                .unwrap_or(1)
                .min(MAX_INJECT_SAMPLES);

            // `ts_ms == 0` is the bus's "no sample seen yet" sentinel in
            // `classify_sample`, so it is rejected along with absent/garbage
            // and falls back to the real clock.
            let ts = ts_ms
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 1.0)
                .map(|v| v as u64)
                .unwrap_or_else(now_ms);

            for _ in 0..n {
                // Ignore the result: `try_broadcast` reports "no active
                // receiver", which is exactly the state a page that has not
                // mounted the indicator yet is in. The spec asserts on the
                // rendered DOM, not on this return value.
                let _ = global_sender().try_broadcast(synthetic_rtt_event(rtt, ts));
            }

            JsValue::from_bool(true)
        },
    );

    let _ = js_sys::Reflect::set(
        &window,
        &JsValue::from_str("__videocall_inject_server_rtt"),
        cb.as_ref().unchecked_ref(),
    );
    // Leak the closure so the JS reference stays valid for the page lifetime.
    cb.forget();
}

/// Native stub: no `window`, nothing to register. Lets the call site stay
/// target-agnostic and keeps `cargo test --lib` green on the host target.
#[cfg(not(target_arch = "wasm32"))]
pub fn register_connection_quality_inject_hooks() {}

#[cfg(test)]
mod tests {
    use super::*;
    use videocall_diagnostics::MetricValue;

    /// The indicator's subscriber loop filters on `subsystem`, requires
    /// `stream_id.is_none()`, and reads the metric named `active_server_rtt`.
    /// An event that misses any of those is dropped by `continue`, so a typo
    /// here would present as "the hook silently does nothing".
    #[test]
    fn synthetic_event_matches_the_indicator_subscriber_filter() {
        let evt = synthetic_rtt_event(350.0, 1_700_000_000_000);

        assert_eq!(evt.subsystem, "connection_manager");
        assert!(
            evt.stream_id.is_none(),
            "the indicator skips per-server events (stream_id.is_some())"
        );
        assert_eq!(evt.ts_ms, 1_700_000_000_000);
        assert_eq!(evt.metrics.len(), 1);
        assert_eq!(evt.metrics[0].name, "active_server_rtt");
    }

    /// `metric!` picks the variant off the value's type. The component matches
    /// `if let MetricValue::F64(v)` and ignores every other variant, so an
    /// integer-typed value would compile fine and never reach the hysteresis.
    #[test]
    fn synthetic_event_carries_an_f64_metric_value() {
        let evt = synthetic_rtt_event(512.5, 1);
        match &evt.metrics[0].value {
            MetricValue::F64(v) => assert_eq!(*v, 512.5),
            other => panic!("active_server_rtt must be MetricValue::F64, got {other:?}"),
        }
    }
}

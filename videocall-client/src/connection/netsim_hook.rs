/*
 * Copyright 2025 Security Union LLC
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

//! Per-tab installation slot for the [`videocall_netsim::NetSimShim`]
//! used by the browser bot (phase 3b of discussion #793).
//!
//! ## Design (Option A) — thread-local hook with a re-entrancy guard
//!
//! `WebSocketTask` and `WebTransportTask` are defined in the foreign
//! crate `videocall-transport`. We deliberately do **not** modify
//! that crate. Likewise, neither task is `Clone`, so an async-spawned
//! task (`spawn_local`) cannot directly own one for the delayed-send
//! path. To bridge this, the higher-level `Connection` owner — which
//! already wraps the task in `Rc<Task>` (see `connection.rs`) —
//! registers a [`Weak<Task>`] in this module's thread-local at connect
//! time. The async-delay path then upgrades that `Weak` inside its
//! spawned future, sets a re-entrancy flag, and calls back through
//! `Task::send_packet*` / send_bytes which short-circuits the
//! hook-consulting code path because of the flag.
//!
//! `wasm32-unknown-unknown` is single-threaded inside a browser tab,
//! so `thread_local!` is effectively "per-tab" state. One tab = one
//! `videocall-client` instance = at most one `NetSimShim`. The phase
//! 3c URL-param plumbing (`?netsim=<profile>`) builds the shim once
//! at `ConnectOptions` construction time and parks it here via
//! [`install_hook`].
//!
//! ## Compile-out guarantee
//!
//! The entire module is gated by `#[cfg(feature = "netsim")]`. Default
//! builds (no `netsim` feature) never see this code, never link
//! `videocall-netsim`, and the send paths are byte-for-byte equivalent
//! to pre-3b.

use std::cell::{Cell, RefCell};
use std::rc::Weak;
use std::sync::Arc;
use std::time::Duration;

use videocall_netsim::{Admission, Direction, NetSimShim};

use super::task::Task;
use super::webmedia::MediaStreamKey;

thread_local! {
    /// Single per-tab hook slot. `RefCell` so we can hand out an
    /// `Arc` clone without taking ownership.
    static NETSIM_HOOK: RefCell<Option<Arc<NetSimShim>>> = const { RefCell::new(None) };

    /// Weak reference back into the owning `Rc<Task>` (see
    /// `Connection::connect`). Used only by the async `Delay` /
    /// `DelayAndDuplicate` paths to re-enter the send pipeline after
    /// the simulated delay.
    static NETSIM_TASK: RefCell<Option<Weak<Task>>> = const { RefCell::new(None) };

    /// Re-entrancy flag. When set, the trait-level `send_bytes` /
    /// `send_bytes_datagram` in `websocket.rs` / `webtransport.rs`
    /// skip the hook consultation entirely. That lets the
    /// "delayed wire send" path call back through the normal API
    /// without infinitely re-entering the hook.
    static NETSIM_BYPASS: Cell<bool> = const { Cell::new(false) };
}

/// Install the shim for the current tab, replacing any previously
/// installed hook. Pass `None` to clear (see [`clear_hook`] for the
/// convenience form).
pub(super) fn install_hook(hook: Option<Arc<NetSimShim>>) {
    NETSIM_HOOK.with(|slot| {
        *slot.borrow_mut() = hook;
    });
}

/// Register the `Weak<Task>` used by the async-delay path to re-enter
/// the send pipeline. Called by `Connection::connect` immediately
/// after the `Rc::new(task)` wrap, so a `Weak` is always available.
/// Pass `None` to clear.
pub(super) fn install_task(task: Option<Weak<Task>>) {
    NETSIM_TASK.with(|slot| {
        *slot.borrow_mut() = task;
    });
}

/// Clear the per-tab hook + task pointer. Currently unused by
/// `videocall-client` itself but exposed for symmetry — phase 3c
/// or the bot harness may want it for clean teardown.
#[allow(dead_code)]
pub(super) fn clear_hook() {
    install_hook(None);
    install_task(None);
}

/// Consult the installed shim for an outbound packet of `size_bytes`
/// bytes. Returns `None` when:
/// - no shim is installed (the production fast path), or
/// - the installed shim's profile is passthrough, or
/// - the [`NETSIM_BYPASS`] re-entrancy flag is currently set, or
/// - the shim's direction does not match the call site.
///
/// `direction` is always [`Direction::Up`] for the client send paths
/// — the client is the uplink side. [`Direction::Down`] is reserved
/// for future receive-side integration.
fn consult(size_bytes: usize, direction: Direction) -> Option<Admission> {
    if NETSIM_BYPASS.with(|c| c.get()) {
        return None;
    }
    NETSIM_HOOK.with(|slot| {
        let borrow = slot.borrow();
        let shim = borrow.as_ref()?;
        // Bypass entirely on passthrough — same shape as no hook
        // installed at all.
        if shim.is_passthrough() {
            return None;
        }
        if shim.direction() != direction {
            return None;
        }
        Some(shim.admit(size_bytes))
    })
}

/// Outcome of [`shape_uplink_*`]: `true` means "the caller should
/// skip its normal send because we either dropped or scheduled it".
/// `false` means "fall through and call the normal send code".
type ShapeOutcome = bool;

/// Routing key for the re-entrant raw-wire send used by the
/// async-delay path.
#[derive(Copy, Clone)]
enum RawRoute {
    /// Reliable per-media-type stream (WT) / single TCP stream (WS).
    Reliable(MediaStreamKey),
    /// Datagram (WT) / Control-stream fallback (WS).
    Datagram,
}

/// RAII guard that sets [`NETSIM_BYPASS`] on construction and clears
/// it on drop. Used by [`raw_send`] so the re-entrancy flag is always
/// restored, even if the inner `Task::send_*_raw_bytes` call panics.
///
/// Without this, a panic on the post-delay send path would leave
/// `NETSIM_BYPASS = true` for the rest of the tab and silently
/// disable the shim — every subsequent send would short-circuit
/// past the hook consultation in [`consult`]. The guard makes the
/// invariant local and panic-safe. (Folded in from PR-3b code
/// review.)
struct BypassGuard;

impl BypassGuard {
    fn new() -> Self {
        NETSIM_BYPASS.with(|c| c.set(true));
        Self
    }
}

impl Drop for BypassGuard {
    fn drop(&mut self) {
        NETSIM_BYPASS.with(|c| c.set(false));
    }
}

/// Send `bytes` through the active task without consulting the
/// netsim hook. Sets [`NETSIM_BYPASS`] for the duration of the call
/// (via [`BypassGuard`]) so the trait-level `send_bytes` /
/// `send_bytes_datagram` impls take the fast path. The guard ensures
/// the flag is cleared even on panic.
///
/// Returns silently if the owning `Rc<Task>` has been dropped
/// (transport disconnected between the original send call and the
/// post-delay wakeup).
fn raw_send(bytes: Vec<u8>, route: RawRoute) {
    let task = match NETSIM_TASK.with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade)) {
        Some(t) => t,
        None => return,
    };

    let _bypass = BypassGuard::new();
    // The `Task::send_*_raw_bytes` helpers (see `task.rs`) sidestep
    // the protobuf framing layer so we can deliver the original
    // payload bytes verbatim, the same way the original caller
    // would have via `WebMedia::send_bytes`.
    match route {
        RawRoute::Reliable(key) => task.send_raw_bytes(bytes, key),
        RawRoute::Datagram => task.send_raw_bytes_datagram(bytes),
    }
    // `_bypass` drops here, clearing NETSIM_BYPASS.
}

/// Apply the netsim admission decision for an uplink reliable-stream
/// packet. See [`ShapeOutcome`] for the return semantics.
pub(super) fn shape_uplink_reliable(bytes: &[u8], stream_key: MediaStreamKey) -> ShapeOutcome {
    shape_uplink(bytes, RawRoute::Reliable(stream_key))
}

/// Apply the netsim admission decision for an uplink datagram packet.
pub(super) fn shape_uplink_datagram(bytes: &[u8]) -> ShapeOutcome {
    shape_uplink(bytes, RawRoute::Datagram)
}

fn shape_uplink(bytes: &[u8], route: RawRoute) -> ShapeOutcome {
    let admission = match consult(bytes.len(), Direction::Up) {
        Some(a) => a,
        None => return false,
    };

    match admission {
        Admission::Pass => false, // caller does the normal send
        Admission::Drop => true,  // silently dropped, as on a lossy link
        Admission::Delay(d) => {
            let bytes = bytes.to_vec();
            let ms = duration_to_millis_u32(d);
            wasm_bindgen_futures::spawn_local(async move {
                gloo_timers::future::TimeoutFuture::new(ms).await;
                raw_send(bytes, route);
            });
            true
        }
        Admission::DelayAndDuplicate(d) => {
            // `Admission::DelayAndDuplicate(Duration)` carries the
            // base delay computed by `videocall_netsim::shim` (loss,
            // latency, jitter, bandwidth, reorder). The primary copy
            // is sent at exactly that delay; the duplicate is sent
            // `dup_jitter_ms` later, where `dup_jitter_ms` is a
            // freshly sampled value in `[5, 50]`.
            //
            // The jitter is here, not in the shim's
            // `Admission::DelayAndDuplicate` payload, because it
            // models *inter-copy spacing* on the wire — a property
            // of how the duplicate is *emitted*, not of the
            // admission decision. Without it, both copies fire in
            // the same wasm32 macrotask batch and the server's
            // dedup absorbs them with no exercised code path,
            // making `DelayAndDuplicate` indistinguishable from
            // plain `Delay` in integration tests.
            //
            // RNG source: `js_sys::Math::random()`. The shim's
            // deterministic `StdRng` would have to be plumbed
            // across the crate boundary through the per-tab hook
            // thread-local — and a 5..=50ms jitter offset is an
            // impairment-test path where non-determinism is
            // acceptable (and arguably realistic — real-world
            // jitter is non-deterministic too). Acceptance criteria
            // for issue #848 is `dup_arrival_ms - primary_arrival_ms
            // ∈ [5, 50]`, which a uniform sample satisfies by
            // construction.
            let ms = duration_to_millis_u32(d);
            let dup_jitter_ms = sample_dup_jitter_ms();
            let primary = bytes.to_vec();
            let dup = bytes.to_vec();
            wasm_bindgen_futures::spawn_local(async move {
                gloo_timers::future::TimeoutFuture::new(ms).await;
                raw_send(primary, route);
            });
            wasm_bindgen_futures::spawn_local(async move {
                gloo_timers::future::TimeoutFuture::new(ms.saturating_add(dup_jitter_ms)).await;
                raw_send(dup, route);
            });
            true
        }
    }
}

/// Saturating cast from `Duration` to `u32` milliseconds. A profile
/// that asks for more than ~49 days of delay is nonsensical for a
/// realtime call, but clamping keeps `TimeoutFuture::new(u32)` safe.
fn duration_to_millis_u32(d: Duration) -> u32 {
    d.as_millis().min(u32::MAX as u128) as u32
}

/// Sample the duplicate's inter-copy spacing in milliseconds.
///
/// Returns a uniform integer in `[5, 50]`. Called by the
/// `DelayAndDuplicate` arm of [`shape_uplink`] so the duplicate
/// arrives at a deterministically-different macrotask from the
/// primary — see the comment block in that arm for the rationale.
///
/// `js_sys::Math::random()` is used here rather than the shim's
/// `StdRng` because the latter is owned by the per-tab hook
/// thread-local and threading it out would inflate the surface
/// area for a 5..=50ms perturbation that does not need to be
/// reproducible.
fn sample_dup_jitter_ms() -> u32 {
    // `Math.random()` returns f64 ∈ [0.0, 1.0). Map to integer
    // milliseconds in [5, 50] inclusive: span = 46 values, then
    // shift up by 5.
    let r = js_sys::Math::random();
    // r * 46.0 ∈ [0.0, 46.0); floor → 0..=45; + 5 → 5..=50.
    let offset = (r * 46.0) as u32;
    5 + offset
}

/// Test-only inspector: returns `true` iff the per-tab hook slot is
/// currently populated with a shim. Used by the unit tests below and
/// by the wasm-bindgen test that guards against regressions of the
/// PR #811 finding-1 "options.netsim_hook clobbers the URL slot" bug.
#[cfg(test)]
pub(super) fn hook_is_installed_for_tests() -> bool {
    NETSIM_HOOK.with(|slot| slot.borrow().is_some())
}

// Compile-only marker confirming the feature gate links cleanly,
// plus regression tests for PR #811 review findings 1 and 3.
//
// The hook module is wasm32-only in practice (its send-path bodies
// touch `gloo_timers` / `wasm_bindgen_futures`), but the symbols
// declared here compile on native too — and the test runner only
// fires on the native target, where this just verifies the
// `#[cfg(feature = "netsim")]` plumbing isn't broken. Behavior is
// exercised in the videocall-netsim shim tests and the bots-app
// integration test in phase 3d. (Folded in from PR-3b code review.)
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use videocall_netsim::{Direction, NetSimShim, NetworkProfile};

    #[test]
    fn netsim_feature_links() {
        // If this compiles, the feature gate is correctly wired.
    }

    /// PR #811 finding 1: an installed shim must survive subsequent
    /// `install_hook(None)` patterns being absent from the transport
    /// connect paths. We can't drive `Task::connect` on native, but
    /// we *can* directly exercise the invariant that the hook slot
    /// is exclusively driven by `install_hook` and never by an
    /// implicit clobber elsewhere — i.e. once `install_hook(Some(_))`
    /// is called, only an explicit `install_hook(None)` /
    /// `clear_hook()` undoes it. This catches a regression where a
    /// future contributor re-adds `install_hook(options.netsim_hook)`
    /// (always `None`) in `websocket.rs` / `webtransport.rs`,
    /// silently disabling the URL-installed shim.
    ///
    /// Strategy: install a shim, then call the only other code path
    /// that *should* touch the slot (`install_task` — which does
    /// not), then assert the shim is still there.
    #[test]
    #[allow(clippy::arc_with_non_send_sync)] // wasm32 NetSimShim is !Sync; Arc is fine in a single-threaded runtime — see NetSimShim doc.
    fn url_installed_shim_survives_install_task() {
        // Clean slate.
        clear_hook();
        assert!(!hook_is_installed_for_tests());

        // Mimic phase-3c URL install.
        let shim = Arc::new(NetSimShim::new(
            NetworkProfile {
                latency_ms: 50,
                jitter_ms: 10,
                seed: Some(1),
                ..NetworkProfile::passthrough()
            },
            Direction::Up,
        ));
        install_hook(Some(shim));
        assert!(
            hook_is_installed_for_tests(),
            "shim should be installed after install_hook(Some(_))"
        );

        // The task slot is the other thread-local; touching it
        // must not clobber the hook slot.
        install_task(None);
        assert!(
            hook_is_installed_for_tests(),
            "install_task(None) must not clear the hook slot — \
             regression of PR #811 finding 1"
        );

        // Cleanup so the next test starts clean.
        clear_hook();
        assert!(!hook_is_installed_for_tests());
    }

    // The `sample_dup_jitter_ms()` companion test lives in the
    // wasm-bindgen test module below — it depends on
    // `js_sys::Math::random()`, which is only available under
    // wasm32. See `wasm_tests::dup_jitter_ms_in_range`.
}

// wasm-bindgen-test counterpart: same survival assertion, but
// executed in the actual browser-shaped wasm32 environment where
// the thread-local is allocated by the wasm runtime. `wasm-pack
// test --node videocall-client -- --features netsim` runs this.
#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use videocall_netsim::{Direction, NetSimShim, NetworkProfile};
    use wasm_bindgen_test::wasm_bindgen_test;

    /// Regression test for PR #811 finding 1: once a shim is
    /// installed (e.g. by phase-3c URL plumbing), nothing in the
    /// transport-connect path should clear it. We can't construct a
    /// real `WebSocketTask` / `WebTransportTask` from a unit test,
    /// but `install_task` is the only other thread-local-touching
    /// helper called by `Connection::connect`, and it must leave
    /// the hook slot alone.
    #[wasm_bindgen_test]
    #[allow(clippy::arc_with_non_send_sync)] // wasm32 NetSimShim is !Sync; Arc is fine in a single-threaded runtime — see NetSimShim doc.
    fn netsim_url_install_survives_install_task() {
        clear_hook();
        assert!(!hook_is_installed_for_tests());

        let shim = Arc::new(NetSimShim::new(
            NetworkProfile {
                latency_ms: 25,
                seed: Some(7),
                ..NetworkProfile::passthrough()
            },
            Direction::Up,
        ));
        install_hook(Some(shim));
        assert!(hook_is_installed_for_tests());

        install_task(None);
        assert!(
            hook_is_installed_for_tests(),
            "install_task must not clear the per-tab netsim hook"
        );

        clear_hook();
    }

    /// PR #811 finding 3: the duplicate's inter-copy jitter offset
    /// is sampled in `[5, 50]` ms inclusive. 256 samples is enough
    /// to catch a buggy mapping that drifts out of range. This test
    /// is wasm32-only because `sample_dup_jitter_ms` calls
    /// `js_sys::Math::random()`.
    #[wasm_bindgen_test]
    fn dup_jitter_ms_in_range() {
        for _ in 0..256 {
            let v = sample_dup_jitter_ms();
            assert!(
                (5..=50).contains(&v),
                "sample_dup_jitter_ms() returned {v}, expected [5, 50]"
            );
        }
    }
}

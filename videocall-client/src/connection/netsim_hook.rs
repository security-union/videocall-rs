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
            // `Admission::DelayAndDuplicate(Duration)` carries a
            // single delay applied to both copies — see
            // `videocall_netsim::shim`. The duplicate is a second
            // send of the byte-identical payload after the same
            // delay; the server's de-dup / sequence handling
            // decides what to do with it.
            let ms = duration_to_millis_u32(d);
            let primary = bytes.to_vec();
            let dup = bytes.to_vec();
            wasm_bindgen_futures::spawn_local(async move {
                gloo_timers::future::TimeoutFuture::new(ms).await;
                raw_send(primary, route);
            });
            wasm_bindgen_futures::spawn_local(async move {
                gloo_timers::future::TimeoutFuture::new(ms).await;
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

// Compile-only marker confirming the feature gate links cleanly.
// The hook module is wasm32-only in practice (its send-path bodies
// touch `gloo_timers` / `wasm_bindgen_futures`), but the symbols
// declared here compile on native too — and the test runner only
// fires on the native target, where this just verifies the
// `#[cfg(feature = "netsim")]` plumbing isn't broken. Behavior is
// exercised in the videocall-netsim shim tests and the bots-app
// integration test in phase 3d. (Folded in from PR-3b code review.)
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    #[test]
    fn netsim_feature_links() {
        // If this compiles, the feature gate is correctly wired.
    }
}

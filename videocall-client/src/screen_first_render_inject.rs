// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test-only injection hook for the SCREEN "first frame rendered" ack
//! (HCL issue #893 — E2E coverage for the screen-share visibility toast).
//!
//! ## What this drives
//!
//! When a receiver decodes and paints the FIRST frame of a peer's screen
//! share, the rAF paint closure in
//! [`crate::decode::peer_decoder::VideoPeerDecoder`] calls `mark_first_render`,
//! which sets the decoder's `first_render_pending_ack` flag. The very next
//! `VideoPeerDecoder::decode()` call then `consume_first_render_flag`s it and
//! returns `DecodeStatus { first_frame: true, .. }` exactly once. The
//! [`crate::decode::peer_decode_manager::PeerDecodeManager`] sees that on the
//! `media_type == SCREEN` branch and fires
//! `PEER_EVENT(screen_decode_started)` back to the publisher, whose UI then
//! flips the "Starting to share content…" toast to
//! "Others can now see your shared content".
//!
//! ## Why a hook is needed
//!
//! The real ack requires the browser's WebCodecs VP8/VP9 decoder to actually
//! produce a `VideoFrame` and the rAF closure to paint it. Empirically, SCREEN
//! media packets DO reach the guest's `decode()` (~10/s) in the Docker E2E
//! stack — but whether the headless WebCodecs decoder actually paints a frame
//! (and therefore whether `mark_first_render` ever fires) is non-deterministic
//! across headless-Chromium/SwiftShader configurations. That flakiness is what
//! made the end-to-end success-toast test un-runnable.
//!
//! This hook lets an E2E spec deterministically simulate "a screen frame just
//! finished painting" WITHOUT faking the toast or bypassing the manager's ack
//! logic. It sets a one-shot pending flag; the next REAL SCREEN
//! `VideoPeerDecoder::decode()` call (screen packets are already flowing)
//! observes it, calls the SAME `mark_first_render` the real paint closure
//! calls, and the existing `consume_first_render_flag` returns
//! `first_frame: true`. From that point the production path is byte-for-byte
//! the real one: manager → `publish_screen_decode_started`. There is no
//! parallel ack path.
//!
//! It is re-armable: after a share stops, the manager calls
//! `rearm_first_render_ack()` (HCL #893 re-share fix), so a second call to this
//! hook on the next share fires a second ack — matching the re-share flow the
//! E2E spec exercises.
//!
//! ## Gating (why it cannot leak to production)
//!
//! The `window.__videocall_inject_screen_first_render` global — the ONLY thing
//! that ever calls [`request_screen_first_render_inject`] — is attached solely
//! by [`register_screen_first_render_inject_hooks`], and the dioxus-ui call
//! site invokes that only when the `MOCK_PEERS_ENABLED` runtime-config flag is
//! true (the same flag that gates the mock-peers debug feature, the #987
//! decode-budget hook, and the #1022 freshness hook). Production deploys leave
//! `MOCK_PEERS_ENABLED` false, so the global is never attached and
//! `request_screen_first_render_inject` is never reachable from JS. The
//! consume-side check compiled into the hot decode path
//! ([`take_screen_first_render_inject_pending`]) therefore always reads the
//! never-set default (`false`) in production and is inert.
//!
//! This gating is security-relevant: the signal it forges is "a peer decoded
//! your screen share." If it were reachable in a production build, arbitrary
//! page JS could tell a publisher that someone is watching their shared content
//! when no one is — a UI trust regression. Because the setter is unreachable
//! unless `MOCK_PEERS_ENABLED` is true (never in prod), and the default pending
//! state is `false`, no production code path can flip it.

use std::cell::Cell;

thread_local! {
    /// One-shot "a screen frame just painted" request, set by the test hook and
    /// taken by the next SCREEN `VideoPeerDecoder::decode()`. Defaults to
    /// `false`; in production nothing ever sets it (the window global that would
    /// is never attached — see the module gating docs).
    static PENDING: Cell<bool> = const { Cell::new(false) };
}

/// Set the one-shot "a screen frame just painted" request.
///
/// The ONLY caller is the `window.__videocall_inject_screen_first_render`
/// closure attached by [`register_screen_first_render_inject_hooks`], which the
/// dioxus-ui call site installs only under `MOCK_PEERS_ENABLED`. Do not call
/// this from production code paths.
pub fn request_screen_first_render_inject() {
    PENDING.with(|p| p.set(true));
}

/// Take (and clear) the pending "a screen frame just painted" request.
///
/// Called from the SCREEN branch of `VideoPeerDecoder::decode()`. Returns
/// `true` at most once per [`request_screen_first_render_inject`] call. In
/// production this is always `false` (nothing sets the flag).
pub fn take_screen_first_render_inject_pending() -> bool {
    PENDING.with(|p| p.replace(false))
}

/// Register the test-only injection hook on `window`, exposing
/// `window.__videocall_inject_screen_first_render()`.
///
/// **The caller is responsible for gating** — the dioxus-ui shim calls this
/// only when `mock_peers_enabled()` is true. Idempotent and cheap; safe to call
/// from a `use_hook` that runs once per mount. The attached closure calls only
/// [`request_screen_first_render_inject`].
#[cfg(target_arch = "wasm32")]
pub fn register_screen_first_render_inject_hooks() {
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    let Some(window) = web_sys::window() else {
        return;
    };

    // window.__videocall_inject_screen_first_render(): arm one synthetic
    // "screen frame painted" so the next REAL SCREEN decode() returns
    // first_frame:true and the manager fires the real screen-decode-started ack.
    let cb = Closure::<dyn Fn()>::new(request_screen_first_render_inject);
    let _ = js_sys::Reflect::set(
        &window,
        &JsValue::from_str("__videocall_inject_screen_first_render"),
        cb.as_ref().unchecked_ref(),
    );
    // Leak the closure so the JS reference stays valid for the page lifetime.
    cb.forget();
}

/// Native stub: no `window`, nothing to register. Lets the call site stay
/// target-agnostic and keeps `cargo test --lib` green on the host target.
#[cfg(not(target_arch = "wasm32"))]
pub fn register_screen_first_render_inject_hooks() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_is_one_shot_and_defaults_false() {
        // Default (production) state: never set → always false.
        assert!(!take_screen_first_render_inject_pending());
        // After a request, the first take returns true, the second false.
        request_screen_first_render_inject();
        assert!(take_screen_first_render_inject_pending());
        assert!(!take_screen_first_render_inject_pending());
    }
}

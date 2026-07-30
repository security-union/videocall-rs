// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test-only injection hook for the SCREEN "first frame rendered" ack
//! (HCL issue #893 — E2E coverage for the screen-share visibility toast).
//!
//! Thin dioxus-ui shim that registers the actual hook — which lives in
//! [`videocall_client::screen_first_render_inject`] because it drives the peer
//! decoder's `first_render` latch — only when the `MOCK_PEERS_ENABLED`
//! runtime-config flag is on.
//!
//! When enabled it attaches one `window` global an E2E spec drives:
//!
//!   - `window.__videocall_inject_screen_first_render()` — arms one synthetic
//!     "a screen frame just painted". The next REAL SCREEN
//!     `VideoPeerDecoder::decode()` (screen packets already flow ~10/s) applies
//!     it through the same `mark_first_render` a real paint uses, so the manager
//!     fires the real `PEER_EVENT(screen_decode_started)` ack to the publisher.
//!     Re-armable across shares (the manager's #893 re-share `rearm` resets the
//!     latch), so it can be called once per share.
//!
//! ## Gating
//!
//! Gated on the same `MOCK_PEERS_ENABLED` flag that gates the mock-peers debug
//! feature, the #987 decode-budget hook, and the #1022 freshness hook.
//! Production deploys leave that flag `false`, so the global is never attached
//! and the setter in `videocall_client` is unreachable. See the
//! security-rationale docs on
//! [`videocall_client::screen_first_render_inject`].

/// Register the test-only screen-first-render injection hook, gated on the
/// `MOCK_PEERS_ENABLED` runtime-config flag.
///
/// Idempotent and cheap to call from a `use_hook` (runs once per component
/// mount). When `mock_peers_enabled()` is false this is a no-op and no global
/// is attached.
#[cfg(target_arch = "wasm32")]
pub fn register_screen_first_render_inject_hooks() {
    if !crate::constants::mock_peers_enabled() {
        return;
    }
    videocall_client::screen_first_render_inject::register_screen_first_render_inject_hooks();
}

/// Native stub: no `window`, nothing to register. Lets the call site stay
/// target-agnostic and keeps `cargo test --lib` green on the host target.
#[cfg(not(target_arch = "wasm32"))]
pub fn register_screen_first_render_inject_hooks() {}

// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test-only injection + observation hooks for the jitter-buffer freshness
//! deadline (issue #1022 — E2E coverage for the #1020 freshness deadline).
//!
//! The freshness deadline runs inside the decoder Web Worker and (since #1045)
//! surfaces a `freshness_skip` `DiagEvent` (subsystem `video`) across the
//! worker→main boundary. This thin dioxus-ui shim registers the actual hooks —
//! which live in [`videocall_client::freshness_inject`] because the worker
//! plumbing lives in the codecs crate (dioxus-ui does not depend on it
//! directly) — only when the `MOCK_PEERS_ENABLED` runtime-config flag is on.
//!
//! When enabled it attaches two `window` globals an E2E spec drives:
//!
//!   - `window.__videocall_inject_stale_video_backlog(num_frames, age_ms)`
//!     forces a stale keyframe-less head-of-line backlog into a self-contained
//!     test decoder, tripping the freshness deadline on the next ~10ms tick.
//!   - `window.__videocall_freshness_skips` — an array the spec polls for the
//!     resulting `freshness_skip` events.
//!
//! ## Gating
//!
//! Gated on the same `MOCK_PEERS_ENABLED` flag that gates the mock-peers debug
//! feature and the #987 decode-budget injection hook
//! ([`crate::components::decode_budget_inject`]). Production deploys leave that
//! flag `false`, so nothing is attached and no test decoder/worker is created.

/// Register the test-only freshness injection + observation hooks, gated on the
/// `MOCK_PEERS_ENABLED` runtime-config flag.
///
/// Idempotent and cheap to call from a `use_hook` (runs once per component
/// mount). When `mock_peers_enabled()` is false this is a no-op and no globals
/// are attached.
#[cfg(target_arch = "wasm32")]
pub fn register_freshness_inject_hooks() {
    if !crate::constants::mock_peers_enabled() {
        return;
    }
    videocall_client::freshness_inject::register_freshness_inject_hooks();
}

/// Native stub: no `window`, nothing to register. Lets the call site stay
/// target-agnostic and keeps `cargo test --lib` green on the host target.
#[cfg(not(target_arch = "wasm32"))]
pub fn register_freshness_inject_hooks() {}

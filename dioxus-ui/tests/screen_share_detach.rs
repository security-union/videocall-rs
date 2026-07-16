// SPDX-License-Identifier: MIT OR Apache-2.0

//! Issue 1175: wasm-bindgen tests for the received-shared-content detach glue.
//!
//! These guard the SAFE public surface that runs headless without opening a real
//! separate window (which needs a user gesture / Document PiP the test runner
//! lacks): feature detection and — most importantly — that the idempotent
//! teardown/reattach paths are no-ops when nothing is detached. The reverted v1
//! (#1634) crashed precisely on lifecycle paths that ran teardown against
//! stale/absent state, so "safe to call when not detached" is a real regression
//! guard, not a triviality.
//!
//! The interactive detach/mirror flow (opening the window, the captureStream
//! mirror, the in-window controls) is validated by the Playwright e2e spec
//! against a real 2-peer screen-share, not here.

// The detach module is `#[cfg(target_arch = "wasm32")]`-only, so this test is
// too (mirrors the other browser tests, e.g. `screen_share_state.rs`).
#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use wasm_bindgen_test::*;

use dioxus_ui::components::screen_share_detach as ssd;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn detach_supported_returns_without_panicking() {
    // Feature detect must never panic; it just reports whether a separate
    // window (Document PiP or a popup) is available in this environment.
    let _ = ssd::detach_supported();
}

#[wasm_bindgen_test]
fn teardown_when_nothing_detached_is_noop() {
    // No window is open in the test runner, so tearing down any peer must be a
    // harmless no-op (the guard against v1's teardown-into-stale-state crashes).
    ssd::teardown("no-such-peer");
    ssd::teardown("42");
    // Calling twice must also be safe (idempotent).
    ssd::teardown("42");
}

#[wasm_bindgen_test]
fn reattach_when_nothing_detached_is_noop() {
    // Reattach with no open window / no pending open must not panic.
    ssd::reattach("no-such-peer");
    ssd::reattach("42");
}

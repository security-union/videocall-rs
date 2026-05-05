// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Unit tests for the ScreenShareState state machine.
// Verifies that `is_sharing()` returns the correct value for each variant,
// locking in the invariant that `Requesting` does NOT trigger the encoder.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use dioxus_ui::components::attendants::ScreenShareState;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn is_sharing_false_for_idle() {
    assert!(
        !ScreenShareState::Idle.is_sharing(),
        "Idle must not be sharing"
    );
}

#[wasm_bindgen_test]
fn is_sharing_false_for_requesting() {
    assert!(
        !ScreenShareState::Requesting.is_sharing(),
        "Requesting (picker dialog open) must not trigger encoder start"
    );
}

#[wasm_bindgen_test]
fn is_sharing_true_for_stream_ready() {
    assert!(
        ScreenShareState::StreamReady.is_sharing(),
        "StreamReady (stream acquired, awaiting encoder) must be sharing"
    );
}

#[wasm_bindgen_test]
fn is_sharing_true_for_active() {
    assert!(
        ScreenShareState::Active.is_sharing(),
        "Active (encoder running) must be sharing"
    );
}

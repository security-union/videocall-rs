// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Unit tests for RecordButtonState state-machine invariants.
//
// `is_busy()` governs whether the RecordButton is disabled mid-transition —
// locking it in prevents regressions where a state is accidentally excluded
// from the busy set and lets the user double-click while a transition is
// in flight.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use dioxus_ui::components::video_control_buttons::RecordButtonState;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

// ── is_busy ──────────────────────────────────────────────────────────────────

#[wasm_bindgen_test]
fn is_busy_false_for_idle() {
    assert!(
        !RecordButtonState::Idle.is_busy(),
        "Idle is not a transition state — button must be enabled"
    );
}

#[wasm_bindgen_test]
fn is_busy_false_for_recording() {
    assert!(
        !RecordButtonState::Recording.is_busy(),
        "Recording is stable — button must be enabled so the user can stop"
    );
}

#[wasm_bindgen_test]
fn is_busy_true_for_activating() {
    assert!(
        RecordButtonState::Activating.is_busy(),
        "Activating is a transition — button must be disabled"
    );
}

#[wasm_bindgen_test]
fn is_busy_true_for_stopping() {
    assert!(
        RecordButtonState::Stopping.is_busy(),
        "Stopping is a transition — button must be disabled"
    );
}

#[wasm_bindgen_test]
fn is_busy_true_for_saving() {
    assert!(
        RecordButtonState::Saving.is_busy(),
        "Saving is a transition — button must be disabled"
    );
}

// ── PartialEq / Clone sanity ─────────────────────────────────────────────────

#[wasm_bindgen_test]
fn clone_and_eq_roundtrip() {
    for state in [
        RecordButtonState::Idle,
        RecordButtonState::Activating,
        RecordButtonState::Recording,
        RecordButtonState::Stopping,
        RecordButtonState::Saving,
    ] {
        assert_eq!(state.clone(), state, "{state:?} must equal its own clone");
    }
}

#[wasm_bindgen_test]
fn idle_ne_recording() {
    assert_ne!(
        RecordButtonState::Idle,
        RecordButtonState::Recording,
        "Idle and Recording must be distinct variants"
    );
}

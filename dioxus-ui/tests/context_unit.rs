// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Unit tests for pure functions in dioxus-ui/src/context.rs.
//
// No DOM rendering needed — these test validation and data logic directly.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use wasm_bindgen_test::*;

use dioxus_ui::context::{is_valid_username, MeetingHost};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// is_valid_username tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn valid_username_alphanumeric() {
    assert!(
        is_valid_username("alice123"),
        "alphanumeric username should be valid"
    );
}

#[wasm_bindgen_test]
fn valid_username_underscore() {
    assert!(
        is_valid_username("my_name"),
        "username with underscores should be valid"
    );
}

#[wasm_bindgen_test]
fn invalid_username_empty() {
    assert!(
        !is_valid_username(""),
        "empty string should be invalid"
    );
}

#[wasm_bindgen_test]
fn invalid_username_spaces() {
    assert!(
        !is_valid_username("has space"),
        "username with spaces should be invalid"
    );
}

// ---------------------------------------------------------------------------
// MeetingHost tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn meeting_host_is_host() {
    let host = MeetingHost {
        host_email: Some("alice@example.com".to_string()),
    };

    assert!(
        host.is_host("alice@example.com"),
        "should identify the host correctly"
    );
    assert!(
        !host.is_host("bob@example.com"),
        "should return false for non-host"
    );

    let empty_host = MeetingHost::default();
    assert!(
        !empty_host.is_host("anyone@example.com"),
        "should return false when host_email is None"
    );
}

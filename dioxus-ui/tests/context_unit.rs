// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Unit tests for context helpers: display-name validation & local storage.
//
// These don't need a full Dioxus render — they test pure functions
// and `window.localStorage` interactions.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use wasm_bindgen_test::*;

use dioxus_ui::context::{
    email_to_display_name, load_username_from_storage, normalize_spaces, save_username_to_storage,
    validate_display_name, DISPLAY_NAME_MAX_LEN,
};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// Display-name validation
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn valid_display_names_accepted() {
    assert!(validate_display_name("alice").is_ok());
    assert!(validate_display_name("Bob 123").is_ok());
    assert!(validate_display_name("user_name").is_ok());
    assert!(validate_display_name("A").is_ok());
    assert!(validate_display_name("O'Brien").is_ok());
    assert!(validate_display_name("Mary-Jane").is_ok());
}

#[wasm_bindgen_test]
fn empty_display_name_rejected() {
    assert!(validate_display_name("").is_err());
    assert!(validate_display_name("   ").is_err());
}

#[wasm_bindgen_test]
fn too_long_display_name_rejected() {
    let long_name = "a".repeat(DISPLAY_NAME_MAX_LEN + 1);
    assert!(validate_display_name(&long_name).is_err());
}

#[wasm_bindgen_test]
fn display_names_with_special_chars_rejected() {
    assert!(validate_display_name("user@name").is_err());
    assert!(validate_display_name("user.name").is_err());
    assert!(validate_display_name("user!").is_err());
}

#[wasm_bindgen_test]
fn display_name_normalizes_spaces() {
    let result = validate_display_name("  hello   world  ").unwrap();
    assert_eq!(result, "hello world");
}

// ---------------------------------------------------------------------------
// normalize_spaces
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn normalize_spaces_collapses_whitespace() {
    assert_eq!(normalize_spaces("  a   b  "), "a b");
    assert_eq!(normalize_spaces("hello"), "hello");
    assert_eq!(normalize_spaces("   "), "");
}

// ---------------------------------------------------------------------------
// email_to_display_name
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn email_to_display_name_works() {
    assert_eq!(email_to_display_name("john.doe"), "John Doe");
    assert_eq!(email_to_display_name("john.doe@example.com"), "John Doe");
    assert_eq!(email_to_display_name("jane_smith"), "Jane Smith");
    assert_eq!(email_to_display_name("bob-jones"), "Bob Jones");
}

// ---------------------------------------------------------------------------
// LocalStorage round-trip
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn storage_round_trip() {
    // Clear any previous value
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item("vc_username");
    }

    // Initially empty
    assert_eq!(load_username_from_storage(), None);

    // Save and reload
    save_username_to_storage("test_user");
    assert_eq!(load_username_from_storage(), Some("test_user".to_string()));

    // Overwrite
    save_username_to_storage("new_user");
    assert_eq!(load_username_from_storage(), Some("new_user".to_string()));

    // Cleanup
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item("vc_username");
    }
}

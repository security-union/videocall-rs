// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Unit tests for context helpers: username validation & local storage.
//
// These don't need a full Dioxus render — they test pure functions
// and `window.localStorage` interactions.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use wasm_bindgen_test::*;

use dioxus_ui::context::{
    is_valid_username, load_username_from_storage, save_username_to_storage,
};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// Username validation
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn valid_usernames_accepted() {
    assert!(is_valid_username("alice"));
    assert!(is_valid_username("Bob123"));
    assert!(is_valid_username("user_name"));
    assert!(is_valid_username("A"));
}

#[wasm_bindgen_test]
fn empty_username_rejected() {
    assert!(!is_valid_username(""));
}

#[wasm_bindgen_test]
fn usernames_with_spaces_rejected() {
    assert!(!is_valid_username("hello world"));
    assert!(!is_valid_username(" leading"));
    assert!(!is_valid_username("trailing "));
}

#[wasm_bindgen_test]
fn usernames_with_special_chars_rejected() {
    assert!(!is_valid_username("user@name"));
    assert!(!is_valid_username("user.name"));
    assert!(!is_valid_username("user-name"));
    assert!(!is_valid_username("user!"));
}

// ---------------------------------------------------------------------------
// LocalStorage round-trip
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn storage_round_trip() {
    // Clear any previous value
    if let Some(storage) = web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
    {
        let _ = storage.remove_item("vc_username");
    }

    // Initially empty
    assert_eq!(load_username_from_storage(), None);

    // Save and reload
    save_username_to_storage("test_user");
    assert_eq!(
        load_username_from_storage(),
        Some("test_user".to_string())
    );

    // Overwrite
    save_username_to_storage("new_user");
    assert_eq!(
        load_username_from_storage(),
        Some("new_user".to_string())
    );

    // Cleanup
    if let Some(storage) = web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
    {
        let _ = storage.remove_item("vc_username");
    }
}

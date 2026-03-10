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
    clear_display_name_from_storage, email_to_display_name, load_display_name_from_storage,
    normalize_spaces, save_display_name_to_storage, validate_display_name, DISPLAY_NAME_MAX_LEN,
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

#[wasm_bindgen_test]
fn email_to_display_name_single_word() {
    // A single-word local part should be title-cased as-is.
    assert_eq!(email_to_display_name("alice"), "Alice");
    assert_eq!(email_to_display_name("alice@corp.io"), "Alice");
}

#[wasm_bindgen_test]
fn email_to_display_name_with_numbers() {
    // Numbers in the local part should be preserved.
    assert_eq!(email_to_display_name("user123@example.com"), "User123");
    assert_eq!(email_to_display_name("john.doe2@example.com"), "John Doe2");
}

#[wasm_bindgen_test]
fn email_to_display_name_mixed_separators() {
    // Mixing dots, underscores, and hyphens should all split correctly.
    assert_eq!(
        email_to_display_name("first.middle_last-jr@example.com"),
        "First Middle Last Jr"
    );
}

#[wasm_bindgen_test]
fn email_to_display_name_uppercase_input() {
    // Input in all-caps should be lowered then title-cased.
    assert_eq!(email_to_display_name("JOHN.DOE@EXAMPLE.COM"), "John Doe");
}

#[wasm_bindgen_test]
fn email_to_display_name_consecutive_separators() {
    // Consecutive separators should not produce empty words or extra spaces.
    assert_eq!(email_to_display_name("john..doe@example.com"), "John Doe");
    assert_eq!(email_to_display_name("a__b--c@x.com"), "A B C");
}

// ---------------------------------------------------------------------------
// email_to_display_name -> validate_display_name round-trip
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn email_to_display_name_produces_valid_display_names() {
    // The output of email_to_display_name should always pass validation.
    let inputs = [
        "john.doe@example.com",
        "jane_smith",
        "bob-jones",
        "alice",
        "user123@example.com",
        "JOHN.DOE@EXAMPLE.COM",
        "first.middle_last-jr@example.com",
    ];
    for input in &inputs {
        let display = email_to_display_name(input);
        assert!(
            validate_display_name(&display).is_ok(),
            "email_to_display_name({:?}) produced {:?} which fails validation: {:?}",
            input,
            display,
            validate_display_name(&display).err()
        );
    }
}

// ---------------------------------------------------------------------------
// LocalStorage round-trip
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn storage_round_trip() {
    // Clear any previous value
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item("vc_display_name");
    }

    // Initially empty
    assert_eq!(load_display_name_from_storage(), None);

    // Save and reload
    save_display_name_to_storage("test_user");
    assert_eq!(
        load_display_name_from_storage(),
        Some("test_user".to_string())
    );

    // Overwrite
    save_display_name_to_storage("new_user");
    assert_eq!(
        load_display_name_from_storage(),
        Some("new_user".to_string())
    );

    // Cleanup
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item("vc_display_name");
    }
}

#[wasm_bindgen_test]
fn clear_display_name_from_storage_removes_key() {
    // Ensure there is a value first.
    save_display_name_to_storage("to_be_cleared");
    assert_eq!(
        load_display_name_from_storage(),
        Some("to_be_cleared".to_string())
    );

    // Clear and verify it is gone.
    clear_display_name_from_storage();
    assert_eq!(load_display_name_from_storage(), None);
}

#[wasm_bindgen_test]
fn clear_display_name_from_storage_is_idempotent() {
    // Clearing when there is nothing stored should not panic or error.
    clear_display_name_from_storage();
    assert_eq!(load_display_name_from_storage(), None);

    // Clearing twice in a row should also be fine.
    save_display_name_to_storage("temp");
    clear_display_name_from_storage();
    clear_display_name_from_storage();
    assert_eq!(load_display_name_from_storage(), None);
}

// ---------------------------------------------------------------------------
// Auto-set display name from profile (non-email used directly)
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn profile_name_without_at_sign_used_directly() {
    // When the profile name does NOT contain '@', it should be usable
    // as a display name without transformation through email_to_display_name.
    // The auto-set logic in the UI checks: if name.contains('@') then
    // email_to_display_name(), otherwise use the name as-is.
    let profile_name = "Alice Johnson";
    assert!(!profile_name.contains('@'));
    // The name should pass validation unchanged (after normalization).
    let validated = validate_display_name(profile_name).unwrap();
    assert_eq!(validated, "Alice Johnson");
}

#[wasm_bindgen_test]
fn profile_name_with_at_sign_goes_through_email_conversion() {
    // When the profile name contains '@', it gets converted via
    // email_to_display_name which title-cases the local part.
    let profile_name = "alice.johnson@example.com";
    assert!(profile_name.contains('@'));
    let display = email_to_display_name(profile_name);
    assert_eq!(display, "Alice Johnson");
    // And the result passes validation.
    assert!(validate_display_name(&display).is_ok());
}

#[wasm_bindgen_test]
fn profile_name_preserves_casing_when_not_email() {
    // A non-email profile name like "McDowell" should NOT be title-cased
    // or otherwise transformed -- it should be kept exactly as provided
    // (modulo whitespace normalization).
    let profile_name = "McDowell";
    let validated = validate_display_name(profile_name).unwrap();
    assert_eq!(validated, "McDowell");
}

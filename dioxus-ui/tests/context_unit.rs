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
    load_transport_preference, resolve_transport_config, save_display_name_to_storage,
    save_transport_preference, validate_display_name, TransportPreference, DISPLAY_NAME_MAX_LEN,
};
use videocall_types::validation::normalize_spaces;

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

// ---------------------------------------------------------------------------
// TransportPreference — Display trait
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn transport_preference_display_auto() {
    assert_eq!(TransportPreference::Auto.to_string(), "auto");
}

#[wasm_bindgen_test]
fn transport_preference_display_webtransport_only() {
    assert_eq!(
        TransportPreference::WebTransportOnly.to_string(),
        "webtransport"
    );
}

#[wasm_bindgen_test]
fn transport_preference_display_websocket_only() {
    assert_eq!(TransportPreference::WebSocketOnly.to_string(), "websocket");
}

// ---------------------------------------------------------------------------
// TransportPreference — FromStr trait
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn transport_preference_parse_auto() {
    assert_eq!(
        "auto".parse::<TransportPreference>().unwrap(),
        TransportPreference::Auto
    );
}

#[wasm_bindgen_test]
fn transport_preference_parse_webtransport() {
    assert_eq!(
        "webtransport".parse::<TransportPreference>().unwrap(),
        TransportPreference::WebTransportOnly
    );
}

#[wasm_bindgen_test]
fn transport_preference_parse_websocket() {
    assert_eq!(
        "websocket".parse::<TransportPreference>().unwrap(),
        TransportPreference::WebSocketOnly
    );
}

#[wasm_bindgen_test]
fn transport_preference_parse_invalid_returns_err() {
    assert!("".parse::<TransportPreference>().is_err());
    assert!("Auto".parse::<TransportPreference>().is_err());
    assert!("WEBTRANSPORT".parse::<TransportPreference>().is_err());
    assert!("WebSocket".parse::<TransportPreference>().is_err());
    assert!("quic".parse::<TransportPreference>().is_err());
    assert!("tcp".parse::<TransportPreference>().is_err());
    assert!("something_random".parse::<TransportPreference>().is_err());
}

// ---------------------------------------------------------------------------
// TransportPreference — Default trait
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn transport_preference_default_is_auto() {
    assert_eq!(TransportPreference::default(), TransportPreference::Auto);
}

// ---------------------------------------------------------------------------
// TransportPreference — Display/FromStr round-trip
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn transport_preference_display_fromstr_roundtrip() {
    // Every variant should survive a Display -> FromStr round-trip.
    let variants = [
        TransportPreference::Auto,
        TransportPreference::WebTransportOnly,
        TransportPreference::WebSocketOnly,
    ];
    for variant in &variants {
        let s = variant.to_string();
        let parsed: TransportPreference = s.parse().unwrap_or_else(|_| {
            panic!(
                "TransportPreference::from_str({:?}) should succeed for variant {:?}",
                s, variant
            )
        });
        assert_eq!(
            *variant, parsed,
            "Round-trip failed for variant {:?} (serialized as {:?})",
            variant, s
        );
    }
}

// ---------------------------------------------------------------------------
// resolve_transport_config — Auto mode
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn resolve_auto_with_server_wt_enabled_passes_through() {
    let ws = vec!["ws://a:8080".to_string(), "ws://b:8080".to_string()];
    let wt = vec!["https://a:4433".to_string(), "https://b:4433".to_string()];
    let (enable_wt, ws_out, wt_out) =
        resolve_transport_config(TransportPreference::Auto, true, ws.clone(), wt.clone());
    assert!(
        enable_wt,
        "Auto + server WT enabled => enable_webtransport = true"
    );
    assert_eq!(ws_out, ws, "Auto should pass WS URLs through unchanged");
    assert_eq!(wt_out, wt, "Auto should pass WT URLs through unchanged");
}

#[wasm_bindgen_test]
fn resolve_auto_with_server_wt_disabled_passes_through() {
    let ws = vec!["ws://a:8080".to_string()];
    let wt = vec!["https://a:4433".to_string()];
    let (enable_wt, ws_out, wt_out) =
        resolve_transport_config(TransportPreference::Auto, false, ws.clone(), wt.clone());
    assert!(
        !enable_wt,
        "Auto + server WT disabled => enable_webtransport = false"
    );
    assert_eq!(ws_out, ws, "Auto should pass WS URLs through unchanged");
    assert_eq!(wt_out, wt, "Auto should pass WT URLs through unchanged");
}

#[wasm_bindgen_test]
fn resolve_auto_with_empty_urls_passes_through_empties() {
    let (enable_wt, ws_out, wt_out) =
        resolve_transport_config(TransportPreference::Auto, true, vec![], vec![]);
    assert!(enable_wt);
    assert!(ws_out.is_empty(), "Auto should pass empty WS list through");
    assert!(wt_out.is_empty(), "Auto should pass empty WT list through");
}

// ---------------------------------------------------------------------------
// resolve_transport_config — WebTransportOnly mode
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn resolve_webtransport_only_clears_ws_urls_and_enables_wt() {
    let ws = vec!["ws://a:8080".to_string(), "ws://b:8080".to_string()];
    let wt = vec!["https://a:4433".to_string()];
    let (enable_wt, ws_out, wt_out) =
        resolve_transport_config(TransportPreference::WebTransportOnly, true, ws, wt.clone());
    assert!(
        enable_wt,
        "WebTransportOnly should force enable_webtransport = true"
    );
    assert!(
        ws_out.is_empty(),
        "WebTransportOnly should clear all WS URLs"
    );
    assert_eq!(wt_out, wt, "WebTransportOnly should keep WT URLs unchanged");
}

#[wasm_bindgen_test]
fn resolve_webtransport_only_overrides_server_wt_disabled() {
    // Even when the server says WT is disabled, WebTransportOnly forces it on.
    let ws = vec!["ws://a:8080".to_string()];
    let wt = vec!["https://a:4433".to_string()];
    let (enable_wt, ws_out, wt_out) =
        resolve_transport_config(TransportPreference::WebTransportOnly, false, ws, wt.clone());
    assert!(
        enable_wt,
        "WebTransportOnly should override server_wt_enabled=false and force true"
    );
    assert!(
        ws_out.is_empty(),
        "WebTransportOnly should clear WS URLs even when server says WT disabled"
    );
    assert_eq!(wt_out, wt);
}

#[wasm_bindgen_test]
fn resolve_webtransport_only_with_empty_urls() {
    let (enable_wt, ws_out, wt_out) =
        resolve_transport_config(TransportPreference::WebTransportOnly, false, vec![], vec![]);
    assert!(
        enable_wt,
        "WebTransportOnly should enable WT even with empty URL lists"
    );
    assert!(ws_out.is_empty());
    assert!(wt_out.is_empty());
}

// ---------------------------------------------------------------------------
// resolve_transport_config — WebSocketOnly mode
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn resolve_websocket_only_clears_wt_urls_and_disables_wt() {
    let ws = vec!["ws://a:8080".to_string()];
    let wt = vec!["https://a:4433".to_string(), "https://b:4433".to_string()];
    let (enable_wt, ws_out, wt_out) =
        resolve_transport_config(TransportPreference::WebSocketOnly, false, ws.clone(), wt);
    assert!(
        !enable_wt,
        "WebSocketOnly should force enable_webtransport = false"
    );
    assert_eq!(ws_out, ws, "WebSocketOnly should keep WS URLs unchanged");
    assert!(wt_out.is_empty(), "WebSocketOnly should clear all WT URLs");
}

#[wasm_bindgen_test]
fn resolve_websocket_only_overrides_server_wt_enabled() {
    // Even when the server says WT is enabled, WebSocketOnly forces it off.
    let ws = vec!["ws://a:8080".to_string()];
    let wt = vec!["https://a:4433".to_string()];
    let (enable_wt, ws_out, wt_out) =
        resolve_transport_config(TransportPreference::WebSocketOnly, true, ws.clone(), wt);
    assert!(
        !enable_wt,
        "WebSocketOnly should override server_wt_enabled=true and force false"
    );
    assert_eq!(
        ws_out, ws,
        "WebSocketOnly should keep WS URLs even when server says WT enabled"
    );
    assert!(
        wt_out.is_empty(),
        "WebSocketOnly should clear WT URLs even when server says WT enabled"
    );
}

#[wasm_bindgen_test]
fn resolve_websocket_only_with_empty_urls() {
    let (enable_wt, ws_out, wt_out) =
        resolve_transport_config(TransportPreference::WebSocketOnly, true, vec![], vec![]);
    assert!(
        !enable_wt,
        "WebSocketOnly should disable WT even with empty URL lists"
    );
    assert!(ws_out.is_empty());
    assert!(wt_out.is_empty());
}

// ---------------------------------------------------------------------------
// resolve_transport_config — multiple URLs preserved correctly
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn resolve_auto_preserves_order_of_multiple_urls() {
    let ws = vec![
        "ws://first:8080".to_string(),
        "ws://second:8080".to_string(),
        "ws://third:8080".to_string(),
    ];
    let wt = vec![
        "https://first:4433".to_string(),
        "https://second:4433".to_string(),
    ];
    let (_, ws_out, wt_out) =
        resolve_transport_config(TransportPreference::Auto, true, ws.clone(), wt.clone());
    assert_eq!(ws_out, ws, "URL order must be preserved");
    assert_eq!(wt_out, wt, "URL order must be preserved");
}

// ---------------------------------------------------------------------------
// Transport preference localStorage round-trip
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn transport_preference_storage_round_trip() {
    // Clear any previous value
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item("vc_transport_preference");
    }

    // Default (nothing stored) should return Auto
    assert_eq!(
        load_transport_preference(),
        TransportPreference::Auto,
        "With nothing stored, load_transport_preference should return Auto"
    );

    // Save WebTransportOnly and reload
    save_transport_preference(TransportPreference::WebTransportOnly);
    assert_eq!(
        load_transport_preference(),
        TransportPreference::WebTransportOnly,
    );

    // Save WebSocketOnly and reload
    save_transport_preference(TransportPreference::WebSocketOnly);
    assert_eq!(
        load_transport_preference(),
        TransportPreference::WebSocketOnly,
    );

    // Save Auto explicitly and reload
    save_transport_preference(TransportPreference::Auto);
    assert_eq!(load_transport_preference(), TransportPreference::Auto);

    // Cleanup
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item("vc_transport_preference");
    }
}

#[wasm_bindgen_test]
fn transport_preference_storage_invalid_value_returns_auto() {
    // If localStorage contains an invalid string, load_transport_preference
    // should fall back to Auto (the default).
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item("vc_transport_preference", "invalid_value");
    }

    assert_eq!(
        load_transport_preference(),
        TransportPreference::Auto,
        "Invalid stored value should fall back to Auto"
    );

    // Cleanup
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item("vc_transport_preference");
    }
}

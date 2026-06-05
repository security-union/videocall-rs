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
    clear_display_name_from_storage, clear_transport_sticky_and_pref, email_to_display_name,
    load_display_name_from_storage, load_transport_preference, resolve_transport_config,
    save_display_name_to_storage, save_transport_preference, save_transport_sticky,
    validate_display_name, TransportPreference, DISPLAY_NAME_MAX_LEN,
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
fn transport_preference_display_webtransport() {
    assert_eq!(
        TransportPreference::WebTransport.to_string(),
        "webtransport"
    );
}

#[wasm_bindgen_test]
fn transport_preference_display_websocket() {
    assert_eq!(TransportPreference::WebSocket.to_string(), "websocket");
}

// ---------------------------------------------------------------------------
// TransportPreference — FromStr trait
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn transport_preference_parse_webtransport() {
    assert_eq!(
        "webtransport".parse::<TransportPreference>().unwrap(),
        TransportPreference::WebTransport
    );
}

#[wasm_bindgen_test]
fn transport_preference_parse_websocket() {
    assert_eq!(
        "websocket".parse::<TransportPreference>().unwrap(),
        TransportPreference::WebSocket
    );
}

/// Legacy migration: persisted `"auto"` values from the pre-simplification
/// release must parse as `WebTransport` since the new `WebTransport`
/// variant carries the same WT-with-WS-fallback semantics that `Auto`
/// used to have. This is the load-time half of the one-shot data
/// migration; see `load_transport_preference` for the storage-canonicalisation
/// half.
#[wasm_bindgen_test]
fn transport_preference_parse_legacy_auto_migrates_to_webtransport() {
    assert_eq!(
        "auto".parse::<TransportPreference>().unwrap(),
        TransportPreference::WebTransport,
        "Legacy \"auto\" must migrate to WebTransport (Auto removed in favour of \
         WebTransport-with-WS-fallback)"
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
fn transport_preference_default_is_webtransport() {
    assert_eq!(
        TransportPreference::default(),
        TransportPreference::WebTransport,
        "Default became WebTransport after Auto removal (same semantics)"
    );
}

// ---------------------------------------------------------------------------
// TransportPreference — Display/FromStr round-trip
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn transport_preference_display_fromstr_roundtrip() {
    // Every variant should survive a Display -> FromStr round-trip.
    let variants = [
        TransportPreference::WebTransport,
        TransportPreference::WebSocket,
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
// resolve_transport_config — WebTransport (with WS fallback) mode
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn resolve_webtransport_with_server_wt_enabled_passes_through_both_lists() {
    // The whole point of the new WebTransport variant: BOTH URL lists are
    // surfaced so the connection manager's election can fall back to WS if
    // every WT candidate fails. Compare to the now-removed `Auto` variant
    // which had identical wire-level behaviour.
    let ws = vec!["ws://a:8080".to_string(), "ws://b:8080".to_string()];
    let wt = vec!["https://a:4433".to_string(), "https://b:4433".to_string()];
    let (enable_wt, ws_out, wt_out) = resolve_transport_config(
        TransportPreference::WebTransport,
        true,
        ws.clone(),
        wt.clone(),
    );
    assert!(
        enable_wt,
        "WebTransport + server WT enabled => enable_webtransport = true"
    );
    assert_eq!(
        ws_out, ws,
        "WebTransport must keep the WS list — it's the fallback path"
    );
    assert_eq!(
        wt_out, wt,
        "WebTransport should pass WT URLs through unchanged"
    );
}

#[wasm_bindgen_test]
fn resolve_webtransport_with_server_wt_disabled_passes_through() {
    let ws = vec!["ws://a:8080".to_string()];
    let wt = vec!["https://a:4433".to_string()];
    let (enable_wt, ws_out, wt_out) = resolve_transport_config(
        TransportPreference::WebTransport,
        false,
        ws.clone(),
        wt.clone(),
    );
    assert!(
        !enable_wt,
        "WebTransport + server WT disabled => enable_webtransport = false"
    );
    assert_eq!(
        ws_out, ws,
        "WebTransport should pass WS URLs through unchanged"
    );
    assert_eq!(
        wt_out, wt,
        "WebTransport should pass WT URLs through unchanged"
    );
}

#[wasm_bindgen_test]
fn resolve_webtransport_with_empty_urls_passes_through_empties() {
    let (enable_wt, ws_out, wt_out) =
        resolve_transport_config(TransportPreference::WebTransport, true, vec![], vec![]);
    assert!(enable_wt);
    assert!(
        ws_out.is_empty(),
        "WebTransport should pass empty WS list through"
    );
    assert!(
        wt_out.is_empty(),
        "WebTransport should pass empty WT list through"
    );
}

// ---------------------------------------------------------------------------
// resolve_transport_config — WebSocket-only mode
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn resolve_websocket_clears_wt_urls_and_disables_wt() {
    let ws = vec!["ws://a:8080".to_string()];
    let wt = vec!["https://a:4433".to_string(), "https://b:4433".to_string()];
    let (enable_wt, ws_out, wt_out) =
        resolve_transport_config(TransportPreference::WebSocket, false, ws.clone(), wt);
    assert!(
        !enable_wt,
        "WebSocket should force enable_webtransport = false"
    );
    assert_eq!(ws_out, ws, "WebSocket should keep WS URLs unchanged");
    assert!(wt_out.is_empty(), "WebSocket should clear all WT URLs");
}

#[wasm_bindgen_test]
fn resolve_websocket_overrides_server_wt_enabled() {
    // Even when the server says WT is enabled, WebSocket forces it off.
    let ws = vec!["ws://a:8080".to_string()];
    let wt = vec!["https://a:4433".to_string()];
    let (enable_wt, ws_out, wt_out) =
        resolve_transport_config(TransportPreference::WebSocket, true, ws.clone(), wt);
    assert!(
        !enable_wt,
        "WebSocket should override server_wt_enabled=true and force false"
    );
    assert_eq!(
        ws_out, ws,
        "WebSocket should keep WS URLs even when server says WT enabled"
    );
    assert!(
        wt_out.is_empty(),
        "WebSocket should clear WT URLs even when server says WT enabled"
    );
}

#[wasm_bindgen_test]
fn resolve_websocket_with_empty_urls() {
    let (enable_wt, ws_out, wt_out) =
        resolve_transport_config(TransportPreference::WebSocket, true, vec![], vec![]);
    assert!(
        !enable_wt,
        "WebSocket should disable WT even with empty URL lists"
    );
    assert!(ws_out.is_empty());
    assert!(wt_out.is_empty());
}

// ---------------------------------------------------------------------------
// resolve_transport_config — multiple URLs preserved correctly
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn resolve_webtransport_preserves_order_of_multiple_urls() {
    let ws = vec![
        "ws://first:8080".to_string(),
        "ws://second:8080".to_string(),
        "ws://third:8080".to_string(),
    ];
    let wt = vec![
        "https://first:4433".to_string(),
        "https://second:4433".to_string(),
    ];
    let (_, ws_out, wt_out) = resolve_transport_config(
        TransportPreference::WebTransport,
        true,
        ws.clone(),
        wt.clone(),
    );
    assert_eq!(ws_out, ws, "URL order must be preserved");
    assert_eq!(wt_out, wt, "URL order must be preserved");
}

// ---------------------------------------------------------------------------
// Transport preference localStorage round-trip
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn transport_preference_storage_round_trip() {
    // Start clean — no sticky, no pref, no session value.
    clear_transport_sticky_and_pref();

    // Default (nothing stored) should return WebTransport.
    assert_eq!(
        load_transport_preference(),
        TransportPreference::WebTransport,
        "With nothing stored, load_transport_preference should return the default \
         (WebTransport — was Auto before the protocol-settings simplification)"
    );

    // The sticky path: save_transport_preference writes to localStorage and is
    // only honoured by load_transport_preference when sticky == true.
    save_transport_sticky(true);

    save_transport_preference(TransportPreference::WebTransport);
    assert_eq!(
        load_transport_preference(),
        TransportPreference::WebTransport,
    );

    save_transport_preference(TransportPreference::WebSocket);
    assert_eq!(load_transport_preference(), TransportPreference::WebSocket);

    // Cleanup — restore to default state.
    clear_transport_sticky_and_pref();
}

/// Persisted `"auto"` from the pre-simplification release must load as
/// `WebTransport` (the same wire-level behaviour) and be canonicalised in
/// storage so subsequent reads no longer see the legacy value. This is the
/// load-time migration the user asked for — older users with `"auto"` in
/// storage see WebTransport selected with no errors and no manual cleanup.
#[wasm_bindgen_test]
fn transport_preference_storage_migrates_legacy_auto() {
    // Start clean so we can plant the legacy value precisely.
    clear_transport_sticky_and_pref();

    let local_storage = web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .expect("test environment must have localStorage");

    // Plant the legacy values directly so the migration can fire.
    local_storage
        .set_item("vc_transport_sticky", "true")
        .expect("set sticky");
    local_storage
        .set_item("vc_transport_preference", "auto")
        .expect("plant legacy auto");

    // Load: should migrate to WebTransport and rewrite storage.
    let loaded = load_transport_preference();
    assert_eq!(
        loaded,
        TransportPreference::WebTransport,
        "Legacy \"auto\" must load as WebTransport"
    );

    // Storage should now contain the canonical "webtransport" — not the
    // legacy "auto" string.
    let canonical = local_storage
        .get_item("vc_transport_preference")
        .ok()
        .flatten();
    assert_eq!(
        canonical.as_deref(),
        Some("webtransport"),
        "Migration must canonicalise the stored value to \"webtransport\""
    );

    clear_transport_sticky_and_pref();
}

#[wasm_bindgen_test]
fn transport_preference_storage_invalid_value_returns_default() {
    // If localStorage contains an invalid string, load_transport_preference
    // should fall back to the default (WebTransport).
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item("vc_transport_preference", "invalid_value");
    }

    assert_eq!(
        load_transport_preference(),
        TransportPreference::WebTransport,
        "Invalid stored value should fall back to the default (WebTransport)"
    );

    // Cleanup
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item("vc_transport_preference");
    }
}

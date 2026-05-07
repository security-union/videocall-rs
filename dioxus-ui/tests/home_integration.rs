// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Integration test for the Home (landing) page (Dioxus).
//
// Verifies that the real Home component renders without errors when
// window.__APP_CONFIG is present. Rather than
// asserting on every single DOM node, we check a handful of landmarks
// that uniquely identify the page.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use support::{
    cleanup, create_mount_point, inject_app_config_oauth_enabled, mock_fetch_401,
    mock_fetch_meetings_empty, remove_app_config, render_into, reset_test_browser_state,
    restore_fetch, wait_for_selector, yield_now,
};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;
use web_sys::{Event, EventInit, HtmlInputElement};

use dioxus::prelude::*;
use dioxus_ui::components::config_error::ConfigError;
use dioxus_ui::components::search_modal::SearchVisibleCtx;
use dioxus_ui::constants::app_config;
use dioxus_ui::context::{
    load_transport_preference, validate_display_name, DisplayNameCtx, TransportPreferenceCtx,
};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

/// Create a bubbling "input" event so Dioxus event delegation picks it up.
fn bubbling_input_event() -> Event {
    let init = EventInit::new();
    init.set_bubbles(true);
    Event::new_with_event_init_dict("input", &init).unwrap()
}

/// Create a bubbling, cancelable "submit" event.
fn bubbling_submit_event() -> Event {
    let init = EventInit::new();
    init.set_bubbles(true);
    init.set_cancelable(true);
    Event::new_with_event_init_dict("submit", &init).unwrap()
}

// ---------------------------------------------------------------------------
// Wrapper component — provides the context Home needs via the full Router.
// ---------------------------------------------------------------------------

/// Push the browser URL to "/" so that Router renders the Home route, then
/// render the full app shell (DisplayNameCtx + Router) matching `main.rs`.
fn ensure_root_url() {
    let _ = gloo_utils::window().history().unwrap().push_state_with_url(
        &wasm_bindgen::JsValue::NULL,
        "",
        Some("/"),
    );
}

/// Full app wrapper: provides all three context providers that `main.rs`
/// supplies (DisplayNameCtx, TransportPreferenceCtx, SearchVisibleCtx),
/// then renders Router<Route>.  The Router picks the component based on
/// the current URL (pushed to "/").
fn home_wrapper_direct() -> Element {
    let username_signal = use_signal(|| None::<String>);
    use_context_provider(|| DisplayNameCtx(username_signal));

    let transport_pref = use_signal(load_transport_preference);
    use_context_provider(|| TransportPreferenceCtx(transport_pref));

    let search_visible = use_signal(|| false);
    use_context_provider(|| SearchVisibleCtx {
        is_visible: search_visible,
    });

    match app_config() {
        Ok(_) => rsx! {
            Router::<dioxus_ui::routing::Route> {}
        },
        Err(e) => rsx! {
            ConfigError { message: e }
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn home_page_renders() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    // Wait for the Router to resolve and Home to render.
    assert!(
        wait_for_selector(&mount, "#username", 2000).await,
        "Timed out waiting for Home page to render (#username)"
    );

    // No error banner — config loaded and browser checks passed.
    assert!(
        mount.query_selector(".error-container").unwrap().is_none(),
        "BrowserCompatibility should not show an error in Chrome"
    );

    // The page text should contain landmarks that identify the home screen.
    let text = mount.text_content().unwrap_or_default();
    assert!(text.contains("Concept Car"), "title missing");
    assert!(
        text.contains("Start or Join a Meeting"),
        "form heading missing"
    );
    assert!(
        text.contains("Generate a New Meeting ID"),
        "create button missing"
    );

    // The two inputs the user fills in must be present.
    assert!(
        mount.query_selector("#username").unwrap().is_some(),
        "username input missing"
    );
    assert!(
        mount.query_selector("#meeting-id").unwrap().is_some(),
        "meeting-id input missing"
    );
    assert!(
        mount
            .query_selector("button[type='submit']")
            .unwrap()
            .is_none(),
        "join button should not render until a meeting ID is entered"
    );

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn home_shows_login_when_unauthenticated() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    // Wait for the auth button rendered in the top-right dropdown container.
    assert!(
        wait_for_selector(&mount, ".generic-sign-in-button", 2000).await,
        "Timed out waiting for sign-in button (.generic-sign-in-button)"
    );

    let text = mount.text_content().unwrap_or_default();
    assert!(
        text.contains("Sign in"),
        "Sign-in button text should be shown"
    );

    assert!(
        mount
            .query_selector(".generic-sign-in-button")
            .unwrap()
            .is_some(),
        "Sign-in button should be rendered"
    );

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn home_hides_login_when_authenticated() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_meetings_empty();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    // Wait for the auth effect to resolve the mocked session/profile and for
    // the meetings fetch to render the empty authenticated state.
    assert!(
        wait_for_selector(&mount, ".meetings-empty", 2000).await,
        "Timed out waiting for empty meetings state (.meetings-empty)"
    );

    // The top-right sign-in button should NOT be visible once a profile loads.
    assert!(
        mount
            .query_selector(".generic-sign-in-button")
            .unwrap()
            .is_none(),
        "Sign-in button should NOT be visible when the user is authenticated"
    );

    // Should show the empty meetings state instead.
    let text = mount.text_content().unwrap_or_default();
    assert!(
        text.contains("No meetings yet"),
        "Empty meetings message should be shown when authenticated"
    );

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn missing_config_shows_error_not_home() {
    reset_test_browser_state();
    remove_app_config();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    // ConfigError renders synchronously (no Router needed), but still wait
    // for at least the initial Dioxus flush.
    assert!(
        wait_for_selector(&mount, ".error-container", 2000).await,
        "Timed out waiting for ConfigError (.error-container)"
    );

    let text = mount.text_content().unwrap_or_default();
    assert!(
        text.contains("__APP_CONFIG"),
        "Error message should mention the missing config"
    );

    // Home should NOT have rendered.
    assert!(
        mount.query_selector(".hero-container").unwrap().is_none(),
        "Home page should not render when config is missing"
    );

    cleanup(&mount);
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn home_rejects_invalid_display_name() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_meetings_empty();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    assert!(
        wait_for_selector(&mount, "#username", 2000).await,
        "Timed out waiting for Home page to render"
    );

    let username = mount
        .query_selector("#username")
        .unwrap()
        .unwrap()
        .dyn_into::<HtmlInputElement>()
        .unwrap();
    username.set_value("John&Doe");
    username.dispatch_event(&bubbling_input_event()).unwrap();

    let meeting_id = mount
        .query_selector("#meeting-id")
        .unwrap()
        .unwrap()
        .dyn_into::<HtmlInputElement>()
        .unwrap();
    meeting_id.set_value("abc_123");
    meeting_id.dispatch_event(&bubbling_input_event()).unwrap();

    // Yield so Dioxus processes the oninput state updates before submit reads them.
    yield_now().await;

    let form = mount.query_selector("form").unwrap().unwrap();
    form.dispatch_event(&bubbling_submit_event()).unwrap();

    yield_now().await;

    let text = mount.text_content().unwrap_or_default();
    assert!(
        text.contains("Invalid character"),
        "Expected invalid character error, got page text: {text}"
    );

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn home_normalizes_spaces_in_display_name() {
    assert_eq!(
        validate_display_name("  John    Doe   ").unwrap(),
        "John Doe"
    );
}

#[wasm_bindgen_test]
async fn home_rejects_empty_display_name() {
    assert!(
        validate_display_name("   ")
            .unwrap_err()
            .contains("Name cannot be empty"),
        "Expected empty-name validation error"
    );
}

#[wasm_bindgen_test]
async fn home_rejects_display_name_exceeding_max_length() {
    let long_name = "A".repeat(51);
    assert!(
        validate_display_name(&long_name)
            .unwrap_err()
            .contains("too long"),
        "Expected max-length validation error"
    );
}

#[wasm_bindgen_test]
async fn home_accepts_display_name_with_special_characters() {
    assert!(validate_display_name("O'Brien-Smith").is_ok());
}

#[wasm_bindgen_test]
async fn home_shows_create_button_when_no_meeting_id() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    assert!(
        wait_for_selector(&mount, "#meeting-id", 2000).await,
        "Timed out waiting for Home page to render"
    );

    assert!(
        mount
            .query_selector("button[type='submit']")
            .unwrap()
            .is_none(),
        "Join button should not render until a meeting ID is entered"
    );

    let text = mount.text_content().unwrap_or_default();
    assert!(
        text.contains("Generate a New Meeting ID"),
        "Create button should always be visible"
    );

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn home_join_button_enabled_when_meeting_id_entered() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    assert!(
        wait_for_selector(&mount, "#meeting-id", 2000).await,
        "Timed out waiting for Home page to render"
    );

    // Enter a meeting ID.
    let meeting_input = mount
        .query_selector("#meeting-id")
        .unwrap()
        .unwrap()
        .dyn_into::<HtmlInputElement>()
        .unwrap();
    meeting_input.set_value("test_meeting");
    meeting_input
        .dispatch_event(&bubbling_input_event())
        .unwrap();

    yield_now().await;

    // The submit button should now be rendered.
    assert!(
        mount
            .query_selector("button[type='submit']")
            .unwrap()
            .is_some(),
        "Join button should render when meeting ID is entered"
    );

    let text = mount.text_content().unwrap_or_default();
    assert!(
        text.contains("Start or Join Meeting"),
        "Join button label should be shown when meeting ID is entered"
    );

    let _btn = mount
        .query_selector("button[type='submit']")
        .unwrap()
        .expect("submit button should exist");

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

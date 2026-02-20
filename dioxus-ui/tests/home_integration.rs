// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Integration test for the Home (landing) page (Dioxus).
//
// Verifies that the real Home component renders without errors when
// window.__APP_CONFIG is present with OAuth disabled.  Rather than
// asserting on every single DOM node, we check a handful of landmarks
// that uniquely identify the page.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use support::{
    cleanup, create_mount_point, inject_app_config, mock_fetch_401, mock_fetch_meetings_empty,
    remove_app_config, render_into, restore_fetch, yield_now,
};
use wasm_bindgen_test::*;

use dioxus::prelude::*;
use dioxus_ui::components::config_error::ConfigError;
use dioxus_ui::constants::app_config;
use dioxus_ui::context::UsernameCtx;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// Wrapper component — provides the context Home needs via the full Router.
// ---------------------------------------------------------------------------

/// Push the browser URL to "/" so that Router renders the Home route, then
/// render the full app shell (UsernameCtx + Router) matching `main.rs`.
fn ensure_root_url() {
    let _ = gloo_utils::window()
        .history()
        .unwrap()
        .push_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some("/"));
}

/// Full app wrapper: provides UsernameCtx then renders Router<Route>.
/// The Router picks the component based on the current URL (pushed to "/").
fn home_wrapper_direct() -> Element {
    let username_signal = use_signal(|| None::<String>);
    use_context_provider(|| UsernameCtx(username_signal));
    match app_config() {
        Ok(_) => rsx! {
            Router::<dioxus_ui::routing::Route> {}
        },
        Err(e) => rsx! { ConfigError { message: e } },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn home_page_renders_with_oauth_disabled() {
    ensure_root_url();
    inject_app_config();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);
    yield_now().await;

    // No error banner — config loaded and browser checks passed.
    assert!(
        mount.query_selector(".error-container").unwrap().is_none(),
        "BrowserCompatibility should not show an error in Chrome"
    );

    // The page text should contain landmarks that identify the home screen.
    let text = mount.text_content().unwrap_or_default();
    assert!(text.contains("videocall.rs"), "title missing");
    assert!(
        text.contains("Start or Join a Meeting"),
        "form heading missing"
    );
    assert!(
        text.contains("Create a New Meeting ID"),
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

    cleanup(&mount);
    remove_app_config();
}

#[wasm_bindgen_test]
async fn home_shows_login_when_unauthenticated() {
    ensure_root_url();
    inject_app_config();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    // Allow the mock fetch to resolve and Dioxus to re-render.
    yield_now().await;
    // Extra yield for async fetch resolution
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        gloo_utils::window()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 100)
            .unwrap();
    });
    wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();
    yield_now().await;

    // The meetings section should show the sign-in prompt instead of an error.
    assert!(
        mount
            .query_selector(".meetings-auth-prompt")
            .unwrap()
            .is_some(),
        "Sign-in prompt should be visible when API returns 401"
    );

    let text = mount.text_content().unwrap_or_default();
    assert!(
        text.contains("Sign in to see your meetings"),
        "Auth prompt text should be shown"
    );

    // A sign-in button should be rendered (generic when no provider is configured).
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
}

#[wasm_bindgen_test]
async fn home_hides_login_when_authenticated() {
    ensure_root_url();
    inject_app_config();
    mock_fetch_meetings_empty();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    // Allow the mock fetch to resolve and Dioxus to re-render.
    yield_now().await;
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        gloo_utils::window()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 100)
            .unwrap();
    });
    wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();
    yield_now().await;

    // The sign-in prompt should NOT be visible.
    assert!(
        mount
            .query_selector(".meetings-auth-prompt")
            .unwrap()
            .is_none(),
        "Sign-in prompt should NOT be visible when API returns 200"
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
}

#[wasm_bindgen_test]
async fn missing_config_shows_error_not_home() {
    remove_app_config();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);
    yield_now().await;

    // ConfigError should be visible — same as the real app.
    let error = mount.query_selector(".error-container").unwrap();
    assert!(
        error.is_some(),
        "ConfigError should be shown when __APP_CONFIG is missing"
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
}

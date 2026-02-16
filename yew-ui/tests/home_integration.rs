// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Integration test for the Home (landing) page.
//
// Verifies that the real Home component renders without errors when
// window.__APP_CONFIG is present with OAuth disabled.  Rather than
// asserting on every single DOM node, we check a handful of landmarks
// that uniquely identify the page — the way a human would glance at
// the screen and say "yep, that's the login page."

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::time::Duration;

use support::{
    cleanup, create_mount_point, inject_app_config, mock_fetch_401, mock_fetch_meetings_empty,
    remove_app_config, restore_fetch,
};
use wasm_bindgen_test::*;
use yew::platform::time::sleep;
use yew::prelude::*;
use yew_router::prelude::*;

use videocall_ui::components::config_error::ConfigError;
use videocall_ui::constants::app_config;
use videocall_ui::context::UsernameCtx;
use videocall_ui::pages::home::Home;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// Wrapper component — mirrors AppRoot's context without the route switch,
// so we always render Home regardless of the test-runner's URL path.
// ---------------------------------------------------------------------------

/// Renders `Home` or `ConfigError` — same logic as the real `switch()`
/// function in `main.rs`, just without the full router.
#[function_component(HomeTestWrapper)]
fn home_test_wrapper() -> Html {
    let username_state = use_state(|| None::<String>);
    let inner = match app_config() {
        Ok(_) => html! { <Home /> },
        Err(e) => html! { <ConfigError message={e} /> },
    };
    html! {
        <ContextProvider<UsernameCtx> context={username_state.clone()}>
            <BrowserRouter>
                { inner }
            </BrowserRouter>
        </ContextProvider<UsernameCtx>>
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn home_page_renders_with_oauth_disabled() {
    inject_app_config();

    let mount = create_mount_point();
    yew::Renderer::<HomeTestWrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    // No error banner — config loaded and browser checks passed.
    assert!(
        mount.query_selector(".error-container").unwrap().is_none(),
        "BrowserCompatibility should not show an error in Chrome"
    );

    // The page text should contain landmarks that identify the login screen.
    let text = mount.text_content().unwrap_or_default();
    assert!(text.contains("videocall.rs"), "title missing");
    assert!(
        text.contains("Start or Join a Meeting"),
        "form heading missing"
    );
    // Note: "Start or Join Meeting" button only appears when meeting ID is entered,
    // so we check for the always-visible "Create a New Meeting ID" button instead
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
    inject_app_config();
    mock_fetch_401();

    let mount = create_mount_point();
    yew::Renderer::<HomeTestWrapper>::with_root(mount.clone()).render();

    // Allow the mock fetch to resolve and Yew to re-render.
    sleep(Duration::from_millis(100)).await;

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
    inject_app_config();
    mock_fetch_meetings_empty();

    let mount = create_mount_point();
    yew::Renderer::<HomeTestWrapper>::with_root(mount.clone()).render();

    // Allow the mock fetch to resolve and Yew to re-render.
    sleep(Duration::from_millis(100)).await;

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
    yew::Renderer::<HomeTestWrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

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

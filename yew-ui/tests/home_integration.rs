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

use support::{cleanup, create_mount_point, inject_app_config, remove_app_config};
use wasm_bindgen_test::*;
use yew::platform::time::sleep;
use yew::prelude::*;
use yew_router::prelude::*;

use videocall_ui::context::UsernameCtx;
use videocall_ui::pages::home::Home;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// Wrapper component — mirrors AppRoot's context without the route switch,
// so we always render Home regardless of the test-runner's URL path.
// ---------------------------------------------------------------------------

#[function_component(HomeTestWrapper)]
fn home_test_wrapper() -> Html {
    let username_state = use_state(|| None::<String>);
    html! {
        <ContextProvider<UsernameCtx> context={username_state.clone()}>
            <BrowserRouter>
                <Home />
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
    assert!(text.contains("Join Meeting"), "join button missing");
    assert!(text.contains("Create New Meeting"), "create button missing");

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
async fn home_page_renders_without_config() {
    // No __APP_CONFIG injected — verify the component doesn't panic.
    remove_app_config();

    let mount = create_mount_point();
    yew::Renderer::<HomeTestWrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    assert!(
        mount.query_selector(".hero-container").unwrap().is_some(),
        "Home should still render its container even without config"
    );

    cleanup(&mount);
}

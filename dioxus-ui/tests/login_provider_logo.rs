// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Component tests for the Login screen's OAuth provider branding (Dioxus).
//
// Verifies that the correct logo SVG and button text are rendered
// based on the `oauthProvider` value in `window.__APP_CONFIG`.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use support::{cleanup, create_mount_point, render_into, yield_now};
use wasm_bindgen_test::*;

use dioxus_ui::components::login::Login;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// Config injection helpers with provider
// ---------------------------------------------------------------------------

fn inject_app_config_with_provider(provider: &str) {
    let config = js_sys::Object::new();
    let set = |key: &str, val: &wasm_bindgen::JsValue| {
        js_sys::Reflect::set(&config, &key.into(), val).unwrap();
    };
    set("apiBaseUrl", &"http://test:8080".into());
    set("meetingApiBaseUrl", &"http://test:8081".into());
    set("wsUrl", &"ws://test:8080".into());
    set("webTransportHost", &"https://test:4433".into());
    set("oauthEnabled", &"true".into());
    set("e2eeEnabled", &"false".into());
    set("webTransportEnabled", &"false".into());
    set("firefoxEnabled", &"false".into());
    set("usersAllowedToStream", &"".into());
    set("oauthProvider", &provider.into());
    set(
        "serverElectionPeriodMs",
        &wasm_bindgen::JsValue::from(2000),
    );
    set("audioBitrateKbps", &wasm_bindgen::JsValue::from(65));
    set("videoBitrateKbps", &wasm_bindgen::JsValue::from(100));
    set("screenBitrateKbps", &wasm_bindgen::JsValue::from(100));

    let frozen = js_sys::Object::freeze(&config);
    let window = gloo_utils::window();
    js_sys::Reflect::set(&window, &"__APP_CONFIG".into(), &frozen).unwrap();
}

fn remove_app_config() {
    let window = gloo_utils::window();
    let _ = js_sys::Reflect::delete_property(&window.into(), &"__APP_CONFIG".into());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn google_provider_shows_google_logo_and_text() {
    inject_app_config_with_provider("google");

    let mount = create_mount_point();
    render_into(&mount, Login);
    yield_now().await;

    // The button should use the official GSI Material class.
    let btn = mount
        .query_selector(".gsi-material-button")
        .unwrap()
        .expect("should have .gsi-material-button");

    // Button text should say "Sign in with Google".
    let label = btn
        .query_selector(".gsi-material-button-contents")
        .unwrap()
        .expect("should have .gsi-material-button-contents span");
    assert_eq!(
        label.text_content().unwrap_or_default(),
        "Sign in with Google"
    );

    // Should contain the Google SVG logo inside the icon wrapper.
    let logo = btn
        .query_selector(".gsi-material-button-icon svg")
        .unwrap();
    assert!(logo.is_some(), "Google SVG logo should be present");

    // Should NOT have Okta or generic buttons.
    assert!(
        mount
            .query_selector(".okta-sign-in-button")
            .unwrap()
            .is_none(),
        "should not have Okta button"
    );
    assert!(
        mount
            .query_selector(".generic-sign-in-button")
            .unwrap()
            .is_none(),
        "should not have generic button"
    );

    cleanup(&mount);
    remove_app_config();
}

#[wasm_bindgen_test]
async fn okta_provider_shows_okta_logo_and_text() {
    inject_app_config_with_provider("okta");

    let mount = create_mount_point();
    render_into(&mount, Login);
    yield_now().await;

    // The button should use the Okta class.
    let btn = mount
        .query_selector(".okta-sign-in-button")
        .unwrap()
        .expect("should have .okta-sign-in-button");

    // Button text should say "Sign in with Okta".
    let label = btn
        .query_selector(".okta-sign-in-button-label")
        .unwrap()
        .expect("should have .okta-sign-in-button-label span");
    assert_eq!(
        label.text_content().unwrap_or_default(),
        "Sign in with Okta"
    );

    // Should contain the Okta SVG logo inside the icon wrapper.
    let logo = btn.query_selector(".okta-sign-in-button-icon svg").unwrap();
    assert!(logo.is_some(), "Okta SVG logo should be present");

    // Should NOT have Google or generic buttons.
    assert!(
        mount
            .query_selector(".gsi-material-button")
            .unwrap()
            .is_none(),
        "should not have Google button"
    );
    assert!(
        mount
            .query_selector(".generic-sign-in-button")
            .unwrap()
            .is_none(),
        "should not have generic button"
    );

    cleanup(&mount);
    remove_app_config();
}

#[wasm_bindgen_test]
async fn no_provider_shows_generic_sign_in() {
    inject_app_config_with_provider("");

    let mount = create_mount_point();
    render_into(&mount, Login);
    yield_now().await;

    // The button should use the generic class.
    let btn = mount
        .query_selector(".generic-sign-in-button")
        .unwrap()
        .expect("should have .generic-sign-in-button");

    // Button text should say "Sign in" (no provider name).
    assert_eq!(btn.text_content().unwrap_or_default(), "Sign in");

    // Should NOT contain any SVG logo.
    let logo = btn.query_selector("svg").unwrap();
    assert!(logo.is_none(), "no provider logo should be present");

    // Should NOT have Google or Okta buttons.
    assert!(
        mount
            .query_selector(".gsi-material-button")
            .unwrap()
            .is_none(),
        "should not have Google button"
    );
    assert!(
        mount
            .query_selector(".okta-sign-in-button")
            .unwrap()
            .is_none(),
        "should not have Okta button"
    );

    cleanup(&mount);
    remove_app_config();
}

#[wasm_bindgen_test]
async fn unknown_provider_falls_back_to_generic() {
    inject_app_config_with_provider("azure-ad");

    let mount = create_mount_point();
    render_into(&mount, Login);
    yield_now().await;

    // Should fall back to generic.
    let btn = mount
        .query_selector(".generic-sign-in-button")
        .unwrap()
        .expect("unknown provider should fall back to .generic-sign-in-button");

    assert_eq!(btn.text_content().unwrap_or_default(), "Sign in");

    // No provider-specific logo.
    let logo = btn.query_selector("svg").unwrap();
    assert!(logo.is_none(), "no provider logo for unknown provider");

    cleanup(&mount);
    remove_app_config();
}

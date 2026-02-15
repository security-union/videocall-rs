// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Component tests for the Login screen's OAuth provider branding.
//
// Verifies that the correct logo SVG and button text are rendered
// based on the `oauthProvider` value in `window.__APP_CONFIG`.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::time::Duration;

use support::{cleanup, create_mount_point, inject_app_config_with_provider, remove_app_config};
use wasm_bindgen_test::*;
use yew::platform::time::sleep;

use videocall_ui::components::login::Login;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn google_provider_shows_google_logo_and_text() {
    inject_app_config_with_provider("google");

    let mount = create_mount_point();
    yew::Renderer::<Login>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    // The button should have the Google-specific CSS class.
    let btn = mount
        .query_selector(".oauth-btn-google")
        .unwrap()
        .expect("should have .oauth-btn-google button");

    // Button text should say "Sign in with Google".
    let label = btn
        .query_selector(".oauth-btn-label")
        .unwrap()
        .expect("should have .oauth-btn-label span");
    assert_eq!(
        label.text_content().unwrap_or_default(),
        "Sign in with Google"
    );

    // Should contain the Google SVG logo.
    let logo = btn.query_selector("svg.oauth-provider-logo").unwrap();
    assert!(logo.is_some(), "Google SVG logo should be present");

    // Should NOT have Okta or generic classes.
    assert!(
        mount.query_selector(".oauth-btn-okta").unwrap().is_none(),
        "should not have Okta class"
    );
    assert!(
        mount
            .query_selector(".oauth-btn-generic")
            .unwrap()
            .is_none(),
        "should not have generic class"
    );

    cleanup(&mount);
    remove_app_config();
}

#[wasm_bindgen_test]
async fn okta_provider_shows_okta_logo_and_text() {
    inject_app_config_with_provider("okta");

    let mount = create_mount_point();
    yew::Renderer::<Login>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    // The button should have the Okta-specific CSS class.
    let btn = mount
        .query_selector(".oauth-btn-okta")
        .unwrap()
        .expect("should have .oauth-btn-okta button");

    // Button text should say "Sign in with Okta".
    let label = btn
        .query_selector(".oauth-btn-label")
        .unwrap()
        .expect("should have .oauth-btn-label span");
    assert_eq!(
        label.text_content().unwrap_or_default(),
        "Sign in with Okta"
    );

    // Should contain the Okta SVG logo.
    let logo = btn.query_selector("svg.oauth-provider-logo").unwrap();
    assert!(logo.is_some(), "Okta SVG logo should be present");

    // Should NOT have Google or generic classes.
    assert!(
        mount.query_selector(".oauth-btn-google").unwrap().is_none(),
        "should not have Google class"
    );
    assert!(
        mount
            .query_selector(".oauth-btn-generic")
            .unwrap()
            .is_none(),
        "should not have generic class"
    );

    cleanup(&mount);
    remove_app_config();
}

#[wasm_bindgen_test]
async fn no_provider_shows_generic_sign_in() {
    inject_app_config_with_provider("");

    let mount = create_mount_point();
    yew::Renderer::<Login>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    // The button should have the generic CSS class.
    let btn = mount
        .query_selector(".oauth-btn-generic")
        .unwrap()
        .expect("should have .oauth-btn-generic button");

    // Button text should say "Sign in" (no provider name).
    let label = btn
        .query_selector(".oauth-btn-label")
        .unwrap()
        .expect("should have .oauth-btn-label span");
    assert_eq!(label.text_content().unwrap_or_default(), "Sign in");

    // Should NOT contain any SVG logo.
    let logo = btn.query_selector("svg.oauth-provider-logo").unwrap();
    assert!(logo.is_none(), "no provider logo should be present");

    // Should NOT have Google or Okta classes.
    assert!(
        mount.query_selector(".oauth-btn-google").unwrap().is_none(),
        "should not have Google class"
    );
    assert!(
        mount.query_selector(".oauth-btn-okta").unwrap().is_none(),
        "should not have Okta class"
    );

    cleanup(&mount);
    remove_app_config();
}

#[wasm_bindgen_test]
async fn unknown_provider_falls_back_to_generic() {
    inject_app_config_with_provider("azure-ad");

    let mount = create_mount_point();
    yew::Renderer::<Login>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    // Should fall back to generic.
    let btn = mount
        .query_selector(".oauth-btn-generic")
        .unwrap()
        .expect("unknown provider should fall back to .oauth-btn-generic");

    let label = btn
        .query_selector(".oauth-btn-label")
        .unwrap()
        .expect("should have .oauth-btn-label span");
    assert_eq!(label.text_content().unwrap_or_default(), "Sign in");

    // No provider-specific logo.
    let logo = btn.query_selector("svg.oauth-provider-logo").unwrap();
    assert!(logo.is_none(), "no provider logo for unknown provider");

    cleanup(&mount);
    remove_app_config();
}

#[wasm_bindgen_test]
async fn login_button_always_has_common_class() {
    inject_app_config_with_provider("google");

    let mount = create_mount_point();
    yew::Renderer::<Login>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    // Every variant should have the common .oauth-sign-in-btn class.
    let btn = mount.query_selector(".oauth-sign-in-btn").unwrap();
    assert!(
        btn.is_some(),
        "sign-in button should have .oauth-sign-in-btn class"
    );

    cleanup(&mount);
    remove_app_config();
}

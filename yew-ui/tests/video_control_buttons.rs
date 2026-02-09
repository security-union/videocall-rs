// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Component tests for video control buttons.
//
// These tests follow the same pattern used by the Yew framework's own test
// suite (packages/yew/tests/):
//
//   1. Configure `wasm_bindgen_test` to run in a real browser.
//   2. Create a mount-point `<div>` and attach it to `<body>`.
//   3. Render the component under test into that div.
//   4. Yield to the Yew scheduler with `sleep(Duration::ZERO).await`.
//   5. Query the DOM and assert on the rendered output.
//   6. Clean up the mount-point so tests don't leak into each other.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use std::time::Duration;

use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;
use yew::platform::time::sleep;
use yew::prelude::*;

use videocall_ui::components::video_control_buttons::{
    CameraButton, HangUpButton, MicButton, ScreenShareButton,
};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a fresh mount-point div, attach it to the document body, and return it.
fn create_mount_point() -> web_sys::Element {
    let document = gloo_utils::document();
    let div = document.create_element("div").unwrap();
    document.body().unwrap().append_child(&div).unwrap();
    div
}

/// Remove the mount-point from the body so subsequent tests start clean.
fn cleanup(mount: &web_sys::Element) {
    gloo_utils::document()
        .body()
        .unwrap()
        .remove_child(mount)
        .ok();
}

// ---------------------------------------------------------------------------
// MicButton tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn mic_button_enabled_shows_mute_tooltip() {
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! { <MicButton enabled={true} onclick={Callback::noop()} /> }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    let tooltip = mount.query_selector(".tooltip").unwrap().unwrap();
    assert_eq!(tooltip.text_content().unwrap(), "Mute");

    let button = mount
        .query_selector("button")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap();
    assert!(
        button.class_list().contains("active"),
        "enabled MicButton should have the 'active' CSS class"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn mic_button_disabled_shows_unmute_tooltip() {
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! { <MicButton enabled={false} onclick={Callback::noop()} /> }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    let tooltip = mount.query_selector(".tooltip").unwrap().unwrap();
    assert_eq!(tooltip.text_content().unwrap(), "Unmute");

    let button = mount
        .query_selector("button")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap();
    assert!(
        !button.class_list().contains("active"),
        "disabled MicButton should NOT have the 'active' CSS class"
    );

    cleanup(&mount);
}

// ---------------------------------------------------------------------------
// CameraButton tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn camera_button_enabled_shows_stop_video_tooltip() {
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! { <CameraButton enabled={true} onclick={Callback::noop()} /> }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    let tooltip = mount.query_selector(".tooltip").unwrap().unwrap();
    assert_eq!(tooltip.text_content().unwrap(), "Stop Video");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn camera_button_disabled_shows_start_video_tooltip() {
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! { <CameraButton enabled={false} onclick={Callback::noop()} /> }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    let tooltip = mount.query_selector(".tooltip").unwrap().unwrap();
    assert_eq!(tooltip.text_content().unwrap(), "Start Video");

    cleanup(&mount);
}

// ---------------------------------------------------------------------------
// ScreenShareButton tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn screen_share_button_disabled_prop_renders_disabled_attribute() {
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! { <ScreenShareButton active={false} disabled={true} onclick={Callback::noop()} /> }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    let button = mount
        .query_selector("button")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlButtonElement>()
        .unwrap();

    assert!(button.disabled(), "button should have disabled attribute");
    assert!(
        button.class_list().contains("disabled"),
        "disabled ScreenShareButton should have the 'disabled' CSS class"
    );

    cleanup(&mount);
}

// ---------------------------------------------------------------------------
// HangUpButton tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn hang_up_button_has_danger_class() {
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! { <HangUpButton onclick={Callback::noop()} /> }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    let button = mount
        .query_selector("button")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap();

    assert!(
        button.class_list().contains("danger"),
        "HangUpButton should have the 'danger' CSS class"
    );
    assert!(
        button.class_list().contains("video-control-button"),
        "HangUpButton should have the 'video-control-button' CSS class"
    );

    let tooltip = mount.query_selector(".tooltip").unwrap().unwrap();
    assert_eq!(tooltip.text_content().unwrap(), "Hang Up");

    cleanup(&mount);
}

// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Component tests for video control buttons.
//
// These tests follow the same pattern used by the Yew test suite, adapted
// for Dioxus:
//
//   1. Configure `wasm_bindgen_test` to run in a real browser.
//   2. Create a mount-point `<div>` and attach it to `<body>`.
//   3. Render the component under test into that div via `dioxus::web::launch_cfg`.
//   4. Yield to the Dioxus scheduler with `gloo_timers::future::TimeoutFuture::new(0)`.
//   5. Query the DOM and assert on the rendered output.
//   6. Clean up the mount-point so tests don't leak into each other.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use dioxus::prelude::*;
use support::{cleanup, create_mount_point, mount_dioxus};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

use dioxus_ui::components::video_control_buttons::{
    CameraButton, HangUpButton, MicButton, PeerListButton, ScreenShareButton,
};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// MicButton tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn mic_button_enabled_shows_mute_tooltip() {
    fn wrapper() -> Element {
        rsx! { MicButton { enabled: true, onclick: move |_| {} } }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

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
    fn wrapper() -> Element {
        rsx! { MicButton { enabled: false, onclick: move |_| {} } }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

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
    fn wrapper() -> Element {
        rsx! { CameraButton { enabled: true, onclick: move |_| {} } }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

    let tooltip = mount.query_selector(".tooltip").unwrap().unwrap();
    assert_eq!(tooltip.text_content().unwrap(), "Stop Video");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn camera_button_disabled_shows_start_video_tooltip() {
    fn wrapper() -> Element {
        rsx! { CameraButton { enabled: false, onclick: move |_| {} } }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

    let tooltip = mount.query_selector(".tooltip").unwrap().unwrap();
    assert_eq!(tooltip.text_content().unwrap(), "Start Video");

    cleanup(&mount);
}

// ---------------------------------------------------------------------------
// ScreenShareButton tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn screen_share_button_disabled_prop_renders_disabled_attribute() {
    fn wrapper() -> Element {
        rsx! { ScreenShareButton { active: false, disabled: true, onclick: move |_| {} } }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

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
    fn wrapper() -> Element {
        rsx! { HangUpButton { onclick: move |_| {} } }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

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

// ---------------------------------------------------------------------------
// PeerListButton tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn peer_list_button_open_shows_close_peers() {
    fn wrapper() -> Element {
        rsx! { PeerListButton { open: true, onclick: move |_| {} } }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

    let tooltip = mount.query_selector(".tooltip").unwrap().unwrap();
    assert_eq!(tooltip.text_content().unwrap(), "Close Peers");

    let button = mount
        .query_selector("button")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap();
    assert!(
        button.class_list().contains("active"),
        "open PeerListButton should have the 'active' CSS class"
    );

    cleanup(&mount);
}

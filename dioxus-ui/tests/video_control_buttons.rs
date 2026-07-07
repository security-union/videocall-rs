// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Component tests for video control buttons (Dioxus).
//
// Pattern:
//   1. Configure `wasm_bindgen_test` to run in a real browser.
//   2. Create a mount-point `<div>` and attach it to `<body>`.
//   3. Render the component into that div via `render_into`.
//   4. Yield to the Dioxus renderer with `yield_now().await`.
//   5. Query the DOM and assert on the rendered output.
//   6. Clean up the mount-point.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use support::{cleanup, create_mount_point, render_into, yield_now};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

use dioxus::prelude::*;
use dioxus_ui::components::video_control_buttons::{
    CameraButton, DeviceSettingsButton, HangUpButton, MeetingOptionsButton, MicButton,
    ScreenShareButton,
};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// MicButton tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn mic_button_enabled_shows_mute_tooltip() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { MicButton { enabled: true, available: true, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let title = mount
        .query_selector(".tooltip .tooltip-title")
        .unwrap()
        .expect(".tooltip-title should exist");
    assert_eq!(title.text_content().unwrap(), "Microphone — Mute");
    let desc = mount
        .query_selector(".tooltip .tooltip-desc")
        .unwrap()
        .expect(".tooltip-desc should exist so users see what the button does");
    assert!(
        desc.text_content().unwrap().contains("microphone"),
        "mute description should mention the microphone, got: {:?}",
        desc.text_content()
    );

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
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { MicButton { enabled: false, available: true, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let title = mount
        .query_selector(".tooltip .tooltip-title")
        .unwrap()
        .expect(".tooltip-title should exist");
    assert_eq!(title.text_content().unwrap(), "Microphone — Unmute");
    let desc = mount
        .query_selector(".tooltip .tooltip-desc")
        .unwrap()
        .expect(".tooltip-desc should exist so users see what the button does");
    assert!(
        desc.text_content().unwrap().contains("microphone"),
        "unmute description should mention the microphone, got: {:?}",
        desc.text_content()
    );

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
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { CameraButton { enabled: true, available: true, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let title = mount
        .query_selector(".tooltip .tooltip-title")
        .unwrap()
        .expect(".tooltip-title should exist");
    assert_eq!(title.text_content().unwrap(), "Camera — Stop Video");
    let desc = mount
        .query_selector(".tooltip .tooltip-desc")
        .unwrap()
        .expect(".tooltip-desc should exist so users see what the button does");
    assert!(
        desc.text_content().unwrap().contains("camera"),
        "stop-video description should mention the camera, got: {:?}",
        desc.text_content()
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn camera_button_disabled_shows_start_video_tooltip() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { CameraButton { enabled: false, available: true, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let title = mount
        .query_selector(".tooltip .tooltip-title")
        .unwrap()
        .expect(".tooltip-title should exist");
    assert_eq!(title.text_content().unwrap(), "Camera — Start Video");
    let desc = mount
        .query_selector(".tooltip .tooltip-desc")
        .unwrap()
        .expect(".tooltip-desc should exist so users see what the button does");
    assert!(
        desc.text_content().unwrap().contains("camera"),
        "start-video description should mention the camera, got: {:?}",
        desc.text_content()
    );

    cleanup(&mount);
}

// ---------------------------------------------------------------------------
// ScreenShareButton tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn screen_share_button_disabled_prop_renders_disabled_attribute() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { ScreenShareButton { active: false, disabled: true, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

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

// Idle state — pin the title text that downstream Playwright specs key off
// (e.g. `presenter-decode-shed.spec.ts`'s `idleShareBtn`). Distinctness from
// the active state is enforced by also asserting the active substring is
// absent: any future rename that collapses idle/active text would break a
// spec selector silently, but trip this assertion.
#[wasm_bindgen_test]
async fn screen_share_button_idle_shows_share_screen_tooltip() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { ScreenShareButton { active: false, disabled: false, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let title = mount
        .query_selector(".tooltip .tooltip-title")
        .unwrap()
        .expect(".tooltip-title should exist");
    let title_text = title.text_content().unwrap();
    assert_eq!(title_text, "Screen share — Share Screen");
    // Guard the e2e selector contract: the idle title must contain the
    // substring "Share Screen" but NOT "Stop Screen Share", so the
    // `hasText: "Share Screen"` matcher in `presenter-decode-shed.spec.ts`
    // (idleShareBtn) keeps idle and active branches distinct.
    assert!(
        title_text.contains("Share Screen"),
        "idle title must contain 'Share Screen' (e2e selector key), got: {title_text:?}"
    );
    assert!(
        !title_text.contains("Stop Screen Share"),
        "idle title must NOT contain 'Stop Screen Share' so it stays distinct \
         from the active button, got: {title_text:?}"
    );

    let desc = mount
        .query_selector(".tooltip .tooltip-desc")
        .unwrap()
        .expect(".tooltip-desc should exist so users see what the button does");
    assert!(
        desc.text_content()
            .unwrap()
            .to_lowercase()
            .contains("screen"),
        "idle share-screen description should mention the screen, got: {:?}",
        desc.text_content()
    );

    cleanup(&mount);
}

// Active state — symmetric guard. The active title must contain
// "Stop Screen Share" so `activeShareBtn` in `presenter-decode-shed.spec.ts`
// keeps matching, and must NOT match the idle e2e selector substring
// "Share Screen" (note: "Stop Screen Share" reverses the word order, so
// "Share Screen" is not a substring — this asserts that property holds
// even if the wording is edited later).
#[wasm_bindgen_test]
async fn screen_share_button_active_shows_stop_screen_share_tooltip() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { ScreenShareButton { active: true, disabled: false, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let title = mount
        .query_selector(".tooltip .tooltip-title")
        .unwrap()
        .expect(".tooltip-title should exist");
    let title_text = title.text_content().unwrap();
    assert_eq!(title_text, "Screen share — Stop Screen Share");
    assert!(
        title_text.contains("Stop Screen Share"),
        "active title must contain 'Stop Screen Share' (e2e selector key), got: {title_text:?}"
    );
    assert!(
        !title_text.to_lowercase().contains("share screen"),
        "active title must NOT contain 'Share Screen' as a substring so it \
         stays distinct from the idle e2e selector, got: {title_text:?}"
    );

    let desc = mount
        .query_selector(".tooltip .tooltip-desc")
        .unwrap()
        .expect(".tooltip-desc should exist so users see what the button does");
    assert!(
        desc.text_content()
            .unwrap()
            .to_lowercase()
            .contains("screen"),
        "active stop-screen-share description should mention the screen, got: {:?}",
        desc.text_content()
    );

    let button = mount
        .query_selector("button.video-control-button")
        .unwrap()
        .expect("ScreenShareButton should render a video-control-button")
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap();
    assert!(
        button.class_list().contains("active"),
        "active ScreenShareButton should carry the 'active' CSS class"
    );

    cleanup(&mount);
}

// ---------------------------------------------------------------------------
// DeviceSettingsButton tests — open/closed (title, desc) tuples
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn device_settings_button_closed_shows_full_tooltip() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { DeviceSettingsButton { open: false, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let title = mount
        .query_selector(".tooltip .tooltip-title")
        .unwrap()
        .expect(".tooltip-title should exist");
    assert_eq!(title.text_content().unwrap(), "Device settings");

    let desc = mount
        .query_selector(".tooltip .tooltip-desc")
        .unwrap()
        .expect(".tooltip-desc should exist so users see what the button does");
    let desc_text = desc.text_content().unwrap();
    assert!(
        desc_text.contains("microphone") && desc_text.contains("camera"),
        "closed device-settings description should mention the microphone and \
         camera devices it switches, got: {desc_text:?}"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn device_settings_button_open_shows_close_tooltip() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { DeviceSettingsButton { open: true, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let title = mount
        .query_selector(".tooltip .tooltip-title")
        .unwrap()
        .expect(".tooltip-title should exist");
    let title_text = title.text_content().unwrap();
    assert_eq!(title_text, "Device settings — Close");
    // The open variant differs from the closed variant only by the "— Close"
    // suffix; pin that explicitly so a refactor that collapses the two
    // branches into one constant tooltip fails the test.
    assert!(
        title_text.contains("Close"),
        "open device-settings title should mark the action as Close, got: {title_text:?}"
    );

    let desc = mount
        .query_selector(".tooltip .tooltip-desc")
        .unwrap()
        .expect(".tooltip-desc should exist so users see what the button does");
    assert!(
        desc.text_content().unwrap().to_lowercase().contains("hide"),
        "open device-settings description should explain it hides the panel, got: {:?}",
        desc.text_content()
    );

    cleanup(&mount);
}

// ---------------------------------------------------------------------------
// MeetingOptionsButton tests — open/closed (title, desc) tuples
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn meeting_options_button_closed_shows_full_tooltip() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { MeetingOptionsButton { open: false, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let title = mount
        .query_selector(".tooltip .tooltip-title")
        .unwrap()
        .expect(".tooltip-title should exist");
    assert_eq!(title.text_content().unwrap(), "Meeting options");

    let desc = mount
        .query_selector(".tooltip .tooltip-desc")
        .unwrap()
        .expect(".tooltip-desc should exist so users see what the button does");
    let desc_text = desc.text_content().unwrap().to_lowercase();
    assert!(
        desc_text.contains("waiting room"),
        "closed meeting-options description should mention the waiting room, got: {desc_text:?}"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn meeting_options_button_open_shows_close_tooltip() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { MeetingOptionsButton { open: true, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let title = mount
        .query_selector(".tooltip .tooltip-title")
        .unwrap()
        .expect(".tooltip-title should exist");
    let title_text = title.text_content().unwrap();
    assert_eq!(title_text, "Meeting options — Close");
    assert!(
        title_text.contains("Close"),
        "open meeting-options title should mark the action as Close, got: {title_text:?}"
    );

    let desc = mount
        .query_selector(".tooltip .tooltip-desc")
        .unwrap()
        .expect(".tooltip-desc should exist so users see what the button does");
    assert!(
        desc.text_content().unwrap().to_lowercase().contains("hide"),
        "open meeting-options description should explain it hides the panel, got: {:?}",
        desc.text_content()
    );

    cleanup(&mount);
}

// ---------------------------------------------------------------------------
// HangUpButton tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn hang_up_button_has_danger_class() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { HangUpButton { onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

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

    let title = mount
        .query_selector(".tooltip .tooltip-title")
        .unwrap()
        .expect(".tooltip-title should exist");
    assert_eq!(title.text_content().unwrap(), "Hang up");
    let desc = mount
        .query_selector(".tooltip .tooltip-desc")
        .unwrap()
        .expect(".tooltip-desc should exist so users see what the button does");
    assert!(
        desc.text_content()
            .unwrap()
            .to_lowercase()
            .contains("leave"),
        "hang-up description should mention leaving the call, got: {:?}",
        desc.text_content()
    );

    cleanup(&mount);
}

// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Component tests for the MeetingEndedOverlay (Dioxus).
//
// Verifies that the overlay renders the expected message, heading, and
// "Return to Home" button when a meeting has ended.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use support::{cleanup, create_mount_point, render_into, yield_now};
use wasm_bindgen_test::*;

use dioxus::prelude::*;
use dioxus_ui::components::meeting_ended_overlay::MeetingEndedOverlay;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn overlay_renders_message_and_heading() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { MeetingEndedOverlay { message: "The meeting has ended.".to_string() } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let text = mount.text_content().unwrap_or_default();

    assert!(
        text.contains("Meeting Ended"),
        "overlay should contain 'Meeting Ended' heading"
    );
    assert!(
        text.contains("The meeting has ended."),
        "overlay should display the message prop"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn overlay_has_return_home_button() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { MeetingEndedOverlay { message: "Host left.".to_string() } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let button = mount
        .query_selector(".meeting-ended-home-btn")
        .unwrap()
        .expect("should have a 'Return to Home' button");

    let btn_text = button.text_content().unwrap_or_default();
    assert_eq!(btn_text, "Return to Home");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn overlay_has_glass_backdrop() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { MeetingEndedOverlay { message: "Done.".to_string() } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let backdrop = mount
        .query_selector(".glass-backdrop.meeting-ended-overlay")
        .unwrap();
    assert!(
        backdrop.is_some(),
        "overlay should have .glass-backdrop.meeting-ended-overlay class"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn overlay_displays_custom_message() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { MeetingEndedOverlay { message: "The host has ended the meeting.".to_string() } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let msg_element = mount
        .query_selector(".meeting-ended-message")
        .unwrap()
        .expect("should have a .meeting-ended-message element");

    let displayed = msg_element.text_content().unwrap_or_default();
    assert_eq!(displayed, "The host has ended the meeting.");

    cleanup(&mount);
}

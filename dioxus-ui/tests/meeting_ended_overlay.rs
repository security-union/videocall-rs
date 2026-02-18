// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Component tests for the MeetingEndedOverlay.
//
// Verifies that the overlay renders the expected message, heading, and
// "Return to Home" button when a meeting has ended.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::RefCell;

use dioxus::prelude::*;
use support::{cleanup, create_mount_point, mount_dioxus};
use wasm_bindgen_test::*;

use dioxus_ui::components::meeting_ended_overlay::MeetingEndedOverlay;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// thread_local for passing message to wrapper
// ---------------------------------------------------------------------------

thread_local! {
    static TEST_MESSAGE: RefCell<String> = RefCell::new(String::new());
}

fn set_test_message(msg: &str) {
    TEST_MESSAGE.with(|m| *m.borrow_mut() = msg.to_string());
}

fn get_test_message() -> String {
    TEST_MESSAGE.with(|m| m.borrow().clone())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn overlay_renders_message_and_heading() {
    fn wrapper() -> Element {
        rsx! {
            MeetingEndedOverlay { message: "The meeting has ended." }
        }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

    let text = mount.text_content().unwrap_or_default();

    // Heading should say "Meeting Ended".
    assert!(
        text.contains("Meeting Ended"),
        "overlay should contain 'Meeting Ended' heading"
    );

    // The message passed via props should be visible.
    assert!(
        text.contains("The meeting has ended."),
        "overlay should display the message prop"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn overlay_has_return_home_button() {
    fn wrapper() -> Element {
        rsx! {
            MeetingEndedOverlay { message: "Host left." }
        }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

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
    fn wrapper() -> Element {
        rsx! {
            MeetingEndedOverlay { message: "Done." }
        }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

    // The root element should be the glass backdrop.
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
    set_test_message("The host has ended the meeting.");

    fn wrapper() -> Element {
        let msg = get_test_message();
        rsx! {
            MeetingEndedOverlay { message: "{msg}" }
        }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

    let msg_element = mount
        .query_selector(".meeting-ended-message")
        .unwrap()
        .expect("should have a .meeting-ended-message element");

    let displayed = msg_element.text_content().unwrap_or_default();
    assert_eq!(displayed, "The host has ended the meeting.");

    cleanup(&mount);
}

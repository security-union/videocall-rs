// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Component tests for the MeetingEndedOverlay.
//
// Verifies that the overlay renders the expected message, heading, and
// "Return to Home" button when a meeting has ended.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::time::Duration;

use support::{cleanup, create_mount_point};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;
use yew::platform::time::sleep;
use yew::prelude::*;

use videocall_ui::components::meeting_ended_overlay::MeetingEndedOverlay;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn overlay_renders_message_and_heading() {
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! {
            <MeetingEndedOverlay message={"The meeting has ended."} />
        }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

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
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! {
            <MeetingEndedOverlay message={"Host left."} />
        }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

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
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! {
            <MeetingEndedOverlay message={"Done."} />
        }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

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
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! {
            <MeetingEndedOverlay message={"The host has ended the meeting."} />
        }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    let msg_element = mount
        .query_selector(".meeting-ended-message")
        .unwrap()
        .expect("should have a .meeting-ended-message element");

    let displayed = msg_element.text_content().unwrap_or_default();
    assert_eq!(displayed, "The host has ended the meeting.");

    cleanup(&mount);
}

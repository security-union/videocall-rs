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
use wasm_bindgen::{JsCast, JsValue};
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

#[component]
fn FocusedControlReplacedByTheOverlay() -> Element {
    let mut ended = use_signal(|| false);
    let mut keydowns_behind = use_signal(|| 0u32);
    rsx! {
        div { onkeydown: move |_| keydowns_behind += 1,
            span { "data-testid": "keydowns-behind", "{keydowns_behind}" }
            if ended() {
                MeetingEndedOverlay { message: "The host has ended the meeting.".to_string() }
            } else {
                button {
                    "data-testid": "focused-control",
                    onclick: move |_| ended.set(true),
                    "end"
                }
            }
        }
    }
}

#[wasm_bindgen_test]
async fn overlay_takes_focus_from_a_control_that_disappears_under_it() {
    let mount = create_mount_point();
    render_into(&mount, FocusedControlReplacedByTheOverlay);
    yield_now().await;

    let control: web_sys::HtmlElement = mount
        .query_selector("[data-testid='focused-control']")
        .unwrap()
        .expect("premise: the control renders before the meeting ends")
        .dyn_into()
        .unwrap();
    control.focus().unwrap();
    control.click();
    yield_now().await;

    let overlay = mount
        .query_selector(".meeting-ended-overlay")
        .unwrap()
        .expect("premise: the overlay replaced the control");
    let active = gloo_utils::document().active_element();
    let inside = active.as_ref().is_some_and(|el| {
        let node: &web_sys::Node = el;
        overlay.contains(Some(node))
    });
    let active_tag = active.map(|el| el.tag_name());
    cleanup(&mount);
    assert!(
        inside,
        "focus must land inside the meeting-ended overlay, got {active_tag:?}"
    );
}

#[wasm_bindgen_test]
async fn keys_pressed_in_the_overlay_do_not_reach_the_meeting_view_behind_it() {
    let mount = create_mount_point();
    render_into(&mount, FocusedControlReplacedByTheOverlay);
    yield_now().await;
    let control: web_sys::HtmlElement = mount
        .query_selector("[data-testid='focused-control']")
        .unwrap()
        .expect("premise: the control renders before the meeting ends")
        .dyn_into()
        .unwrap();
    control.click();
    yield_now().await;

    let card = mount
        .query_selector(".meeting-ended-overlay .card-apple")
        .unwrap()
        .expect("premise: the overlay card rendered");
    let dispatch = js_sys::Function::new_with_args(
        "el",
        "el.dispatchEvent(new KeyboardEvent('keydown', \
         { key: 'Escape', bubbles: true, cancelable: true }));",
    );
    dispatch.call1(&JsValue::NULL, &card).unwrap();
    yield_now().await;

    let seen = mount
        .query_selector("[data-testid='keydowns-behind']")
        .unwrap()
        .and_then(|el| el.text_content())
        .unwrap_or_default();
    cleanup(&mount);
    assert_eq!(
        seen, "0",
        "an Escape in the overlay must not run the meeting view's Escape chain, \
         which would move focus to a control hidden under the overlay"
    );
}

const ENDED_CARD: &str = "#meeting-ended-card";
const HOME_BTN: &str = ".meeting-ended-home-btn";

fn ended_overlay() -> Element {
    rsx! { MeetingEndedOverlay { message: "The host has ended the meeting.".to_string() } }
}

fn el(mount: &web_sys::Element, selector: &str) -> web_sys::Element {
    mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} should be rendered"))
}

fn focus(mount: &web_sys::Element, selector: &str) {
    el(mount, selector)
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap()
        .focus()
        .unwrap();
}

fn active_is(mount: &web_sys::Element, selector: &str) -> bool {
    let target = el(mount, selector);
    let node: &web_sys::Node = &target;
    gloo_utils::document()
        .active_element()
        .is_some_and(|a| a.is_same_node(Some(node)))
}

/// Dispatches a cancelable Tab keydown and reports `defaultPrevented` — the
/// flag that stops the browser carrying focus out of the overlay.
fn tab(target: &web_sys::Element, shift: bool) -> bool {
    let dispatch = js_sys::Function::new_with_args(
        "el, shift",
        "const e = new KeyboardEvent('keydown', \
         { key: 'Tab', shiftKey: shift, bubbles: true, cancelable: true }); \
         el.dispatchEvent(e); return e.defaultPrevented;",
    );
    dispatch
        .call2(&JsValue::NULL, target, &JsValue::from_bool(shift))
        .unwrap()
        .as_bool()
        .unwrap()
}

#[wasm_bindgen_test]
async fn tab_is_trapped_inside_the_ended_overlay() {
    let mount = create_mount_point();
    render_into(&mount, ended_overlay);
    yield_now().await;

    let card_focused_on_mount = active_is(&mount, ENDED_CARD);

    let tab_from_card = tab(&el(&mount, ENDED_CARD), false);
    let home_after_tab = active_is(&mount, HOME_BTN);

    focus(&mount, ENDED_CARD);
    let shift_tab_from_card = tab(&el(&mount, ENDED_CARD), true);
    let home_after_shift_tab = active_is(&mount, HOME_BTN);

    focus(&mount, HOME_BTN);
    let tab_from_home = tab(&el(&mount, HOME_BTN), false);
    let home_after_tab_wrap = active_is(&mount, HOME_BTN);
    let shift_tab_from_home = tab(&el(&mount, HOME_BTN), true);
    let home_after_shift_tab_wrap = active_is(&mount, HOME_BTN);

    cleanup(&mount);

    assert!(
        card_focused_on_mount,
        "premise: the card takes focus on mount"
    );
    assert!(
        home_after_tab && tab_from_card,
        "Tab off the card must move to Return to Home and cancel the browser's \
         own Tab, which would otherwise reach the chrome under the backdrop \
         (focus_on_home={home_after_tab}, default_prevented={tab_from_card})"
    );
    assert!(
        home_after_shift_tab && shift_tab_from_card,
        "Shift+Tab off the card must wrap to Return to Home and cancel the \
         default (focus_on_home={home_after_shift_tab}, \
         default_prevented={shift_tab_from_card})"
    );
    assert!(
        home_after_tab_wrap && tab_from_home,
        "Tab on the only control must stay on it and cancel the default \
         (focus_on_home={home_after_tab_wrap}, default_prevented={tab_from_home})"
    );
    assert!(
        home_after_shift_tab_wrap && shift_tab_from_home,
        "Shift+Tab on the only control must stay on it and cancel the default \
         (focus_on_home={home_after_shift_tab_wrap}, \
         default_prevented={shift_tab_from_home})"
    );
}

// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Component tests for the MeetingRestartedOverlay (Dioxus, #3287): the notice
// shown when the relay refused this client's room token because the meeting
// restarted while it was disconnected.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::Cell;
use support::{cleanup, create_mount_point, render_into, yield_now};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_test::*;

use dioxus::prelude::*;
use dioxus_ui::components::meeting_restarted_overlay::{
    MeetingRestartedOverlay, RejoinOverlayVariant, MEETING_RESTARTED_HOME_BTN_ID,
    MEETING_RESTARTED_MESSAGE, MEETING_RESTARTED_OVERLAY_ID, MEETING_RESTARTED_REJOIN_BTN_ID,
};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

thread_local! {
    static REJOINS: Cell<u32> = const { Cell::new(0) };
}

fn overlay() -> Element {
    rsx! { MeetingRestartedOverlay { on_rejoin: move |_| REJOINS.with(|r| r.set(r.get() + 1)) } }
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
async fn overlay_renders_its_ids_heading_message_and_buttons() {
    let mount = create_mount_point();
    render_into(&mount, overlay);
    yield_now().await;

    assert_eq!(
        el(&mount, &format!("#{MEETING_RESTARTED_OVERLAY_ID}")).get_attribute("data-testid"),
        Some(MEETING_RESTARTED_OVERLAY_ID.to_string())
    );
    assert_eq!(
        el(&mount, "#meeting-restarted-card").get_attribute("role"),
        Some("alertdialog".to_string())
    );
    assert_eq!(
        el(&mount, "#meeting-restarted-title")
            .text_content()
            .unwrap_or_default(),
        "Meeting Restarted"
    );
    assert_eq!(
        el(&mount, ".meeting-restarted-message")
            .text_content()
            .unwrap_or_default(),
        MEETING_RESTARTED_MESSAGE
    );
    assert_eq!(
        el(&mount, &format!("#{MEETING_RESTARTED_REJOIN_BTN_ID}"))
            .text_content()
            .unwrap_or_default(),
        "Rejoin"
    );
    assert_eq!(
        el(&mount, &format!("#{MEETING_RESTARTED_HOME_BTN_ID}"))
            .text_content()
            .unwrap_or_default(),
        "Return to Home"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn rejoin_invokes_the_rejoin_handler_once_across_repeated_clicks() {
    REJOINS.with(|r| r.set(0));
    let mount = create_mount_point();
    render_into(&mount, overlay);
    yield_now().await;

    let rejoin: web_sys::HtmlElement = el(&mount, &format!("#{MEETING_RESTARTED_REJOIN_BTN_ID}"))
        .dyn_into()
        .unwrap();
    assert_eq!(REJOINS.with(Cell::get), 0, "premise: not called on render");
    rejoin.click();
    yield_now().await;
    assert_eq!(REJOINS.with(Cell::get), 1);
    rejoin.click();
    yield_now().await;
    assert_eq!(REJOINS.with(Cell::get), 1);

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn tab_is_trapped_between_return_to_home_and_rejoin() {
    let mount = create_mount_point();
    render_into(&mount, overlay);
    yield_now().await;
    let home = format!("#{MEETING_RESTARTED_HOME_BTN_ID}");
    let rejoin = format!("#{MEETING_RESTARTED_REJOIN_BTN_ID}");

    let rejoin_focused_on_mount = active_is(&mount, &rejoin);
    focus(&mount, &rejoin);
    let prevented_wrapping_forward = tab(&el(&mount, &rejoin), false);
    let home_after_tab = active_is(&mount, &home);
    let prevented_wrapping_back = tab(&el(&mount, &home), true);
    let rejoin_after_shift_tab = active_is(&mount, &rejoin);
    cleanup(&mount);

    assert!(rejoin_focused_on_mount, "Rejoin takes focus on mount");
    assert!(prevented_wrapping_forward && home_after_tab);
    assert!(prevented_wrapping_back && rejoin_after_shift_tab);
}

/// The pages that mount the call are not drivable to the overlay from a
/// component test, so the wiring is pinned by source: each page provides the
/// context the overlay's Rejoin reads, and a rejoin clears the waiting-room
/// flag before re-running the join flow.
#[wasm_bindgen_test]
fn both_join_pages_provide_the_rejoin_context_and_clear_the_waiting_room_flag() {
    let callback = concat!(
        "letrejoin=use_callback(move|_:()|{",
        "came_from_waiting_room.set(false);on_join(None);});"
    );
    let provider = concat!("use_context_provider(||", "MeetingRejoinCtx(rejoin));");
    for (page, source) in [
        ("pages/meeting.rs", include_str!("../src/pages/meeting.rs")),
        (
            "pages/guest_join.rs",
            include_str!("../src/pages/guest_join.rs"),
        ),
    ] {
        let flat: String = source.split_whitespace().collect();
        assert_eq!(flat.matches(provider).count(), 1, "{page}: one provider");
        assert_eq!(flat.matches(callback).count(), 1, "{page}: rejoin callback");
        assert!(
            flat.find(callback) < flat.find(provider),
            "{page}: the provider hands out that callback"
        );
    }
}

thread_local! {
    static VARIANT: Cell<RejoinOverlayVariant> = const { Cell::new(RejoinOverlayVariant::MeetingRestarted) };
}

fn overlay_of_variant() -> Element {
    let variant = VARIANT.with(Cell::get);
    rsx! { MeetingRestartedOverlay { variant, on_rejoin: move |_| {} } }
}

#[wasm_bindgen_test]
async fn each_variant_renders_its_own_heading_and_message_under_the_same_ids() {
    for variant in [
        RejoinOverlayVariant::MeetingRestarted,
        RejoinOverlayVariant::NoLongerInMeeting,
    ] {
        VARIANT.with(|v| v.set(variant));
        let mount = create_mount_point();
        render_into(&mount, overlay_of_variant);
        yield_now().await;

        assert_eq!(
            el(&mount, "#meeting-restarted-title")
                .text_content()
                .unwrap_or_default(),
            variant.heading()
        );
        assert_eq!(
            el(&mount, "#meeting-restarted-message")
                .text_content()
                .unwrap_or_default(),
            variant.message()
        );
        for id in [
            MEETING_RESTARTED_OVERLAY_ID,
            MEETING_RESTARTED_REJOIN_BTN_ID,
            MEETING_RESTARTED_HOME_BTN_ID,
        ] {
            el(&mount, &format!("#{id}"));
        }
        cleanup(&mount);
    }
    assert_ne!(
        RejoinOverlayVariant::MeetingRestarted.heading(),
        RejoinOverlayVariant::NoLongerInMeeting.heading()
    );
    assert_ne!(
        RejoinOverlayVariant::MeetingRestarted.message(),
        RejoinOverlayVariant::NoLongerInMeeting.message()
    );
}

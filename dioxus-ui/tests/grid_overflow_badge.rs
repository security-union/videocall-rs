// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0

// Issue #2264: `readGridOverflowCount` in `dioxus-ui/scripts/recording.js`
// reads `data-overflow-count` off this badge to draw its own "+N" cell.
// Issue 3128: the badge is the button that opens the list behind it.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use support::{cleanup, create_mount_point, render_into, yield_now};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

use dioxus::prelude::*;
use dioxus_ui::components::canvas_generator::PinnedTile;
use dioxus_ui::components::grid_overflow_badge::GridOverflowBadge;
use dioxus_ui::components::overflow_participants::{OverflowParticipant, OverflowParticipantList};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

fn seven_more() -> Element {
    let open = use_signal(|| false);
    rsx! { GridOverflowBadge { overflow_count: 7usize, open, on_open: |_| {} } }
}

fn badge_with_list() -> Element {
    let open = use_signal(|| false);
    let pins = use_signal(Vec::<PinnedTile>::new);
    rsx! {
        GridOverflowBadge { overflow_count: 1usize, open, on_open: |_| {} }
        if open() {
            OverflowParticipantList {
                open,
                participants: vec![OverflowParticipant {
                    session_id: "21".to_string(),
                    name: Some("Dana".to_string()),
                    user_id: Some("dana@example.com".to_string()),
                }],
                owner_user_id: None,
                pins,
                on_toggle_pin: |_: PinnedTile| {},
            }
        }
    }
}

thread_local! {
    static BACKGROUND_CLICKS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static OPENS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// A stand-in for `#main-container`, whose onclick light-dismisses the side panels.
fn badge_in_background() -> Element {
    let open = use_signal(|| false);
    rsx! {
        div { onclick: move |_| BACKGROUND_CLICKS.with(|c| c.set(c.get() + 1)),
            GridOverflowBadge {
                overflow_count: 3usize,
                open,
                on_open: |_| OPENS.with(|c| c.set(c.get() + 1)),
            }
        }
    }
}

fn badge(mount: &web_sys::Element) -> web_sys::HtmlElement {
    mount
        .query_selector("button#grid-overflow-badge.grid-overflow-badge[data-overflow-count]")
        .unwrap()
        .expect("the badge is a button that keeps the class and attribute recording.js reads")
        .dyn_into()
        .unwrap()
}

#[wasm_bindgen_test]
async fn badge_exposes_the_count_the_recorder_reads() {
    let mount = create_mount_point();
    render_into(&mount, seven_more);
    yield_now().await;

    let badge = mount
        .query_selector(".grid-overflow-badge[data-overflow-count]")
        .unwrap()
        .expect("recording.js selects .grid-overflow-badge[data-overflow-count]");
    assert_eq!(
        badge.get_attribute("data-overflow-count").as_deref(),
        Some("7"),
        "the attribute must carry the count, not just the printed chip text"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn badge_still_renders_the_visible_chip() {
    let mount = create_mount_point();
    render_into(&mount, seven_more);
    yield_now().await;

    let text = mount.text_content().unwrap_or_default();
    assert!(
        text.contains("+7"),
        "badge should print the count: {text:?}"
    );
    assert!(
        text.contains("more in meeting"),
        "badge should keep its caption: {text:?}"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn badge_is_a_dialog_disclosure_named_by_its_text() {
    let mount = create_mount_point();
    render_into(&mount, seven_more);
    yield_now().await;

    let badge = badge(&mount);
    assert_eq!(
        badge.get_attribute("aria-haspopup").as_deref(),
        Some("dialog")
    );
    assert_eq!(
        badge.get_attribute("aria-expanded").as_deref(),
        Some("false")
    );
    assert_eq!(
        badge.get_attribute("aria-controls").as_deref(),
        Some("grid-overflow-list")
    );
    assert_eq!(badge.get_attribute("aria-label"), None);

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn clicking_the_badge_toggles_the_list() {
    let mount = create_mount_point();
    render_into(&mount, badge_with_list);
    yield_now().await;
    assert!(mount
        .query_selector("#grid-overflow-list")
        .unwrap()
        .is_none());

    badge(&mount).click();
    yield_now().await;
    assert_eq!(
        badge(&mount).get_attribute("aria-expanded").as_deref(),
        Some("true")
    );
    let list = mount
        .query_selector("#grid-overflow-list[role='dialog']")
        .unwrap()
        .expect("a click opens the list");
    assert!(list.text_content().unwrap_or_default().contains("Dana"));

    badge(&mount).click();
    yield_now().await;
    assert_eq!(
        badge(&mount).get_attribute("aria-expanded").as_deref(),
        Some("false")
    );
    assert!(mount
        .query_selector("#grid-overflow-list")
        .unwrap()
        .is_none());

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_badge_click_stays_off_the_background_and_on_open_fires_only_on_open() {
    BACKGROUND_CLICKS.with(|c| c.set(0));
    OPENS.with(|c| c.set(0));
    let mount = create_mount_point();
    render_into(&mount, badge_in_background);
    yield_now().await;

    badge(&mount).click();
    yield_now().await;
    assert_eq!(
        badge(&mount).get_attribute("aria-expanded").as_deref(),
        Some("true")
    );
    assert_eq!(OPENS.with(|c| c.get()), 1);

    badge(&mount).click();
    yield_now().await;
    assert_eq!(
        badge(&mount).get_attribute("aria-expanded").as_deref(),
        Some("false")
    );
    assert_eq!(OPENS.with(|c| c.get()), 1, "closing must not fire on_open");
    assert_eq!(
        BACKGROUND_CLICKS.with(|c| c.get()),
        0,
        "a tile control must not light-dismiss the side panels"
    );

    cleanup(&mount);
}

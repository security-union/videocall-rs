// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue 3128: the list behind the `+N more in meeting` badge — who is in it,
// how a row pins, and where focus goes when it opens and closes.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;

use dioxus::prelude::*;
use dioxus_ui::components::canvas_generator::PinnedTile;
use dioxus_ui::components::grid_overflow_badge::GridOverflowBadge;
use dioxus_ui::components::overflow_participants::{OverflowParticipant, OverflowParticipantList};
use dioxus_ui::components::pin_order;
use dioxus_ui::context::HostSetCtx;
use support::{cleanup, create_mount_point, render_into, yield_now};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const OWNER: &str = "owner@example.com";
const ZED: &str = "zed@example.com";
const BOB: &str = "bob@example.com";
const AMY: &str = "amy@example.com";
const LIST: &str = "#grid-overflow-list";

#[derive(Clone, Copy)]
struct Handles {
    open: Signal<bool>,
    badge_shown: Signal<bool>,
    pins: Signal<Vec<PinnedTile>>,
    zed_name: Signal<String>,
}

const TOUCH_HOLD_MS: u32 = 600;

thread_local! {
    static START_OPEN: Cell<bool> = const { Cell::new(true) };
    static PROMOTE_PINNED: Cell<bool> = const { Cell::new(false) };
    static PIN_CALLS: RefCell<Vec<PinnedTile>> = const { RefCell::new(Vec::new()) };
    static HANDLES: RefCell<Option<Handles>> = const { RefCell::new(None) };
}

fn row(session_id: &str, name: Option<&str>, user_id: Option<&str>) -> OverflowParticipant {
    OverflowParticipant {
        session_id: session_id.to_string(),
        name: name.map(str::to_string),
        user_id: user_id.map(str::to_string),
    }
}

/// Listed unsorted; the list shows amy, Participant (12), Participant (mock-0), Zed.
fn everyone(zed_name: &str) -> Vec<OverflowParticipant> {
    vec![
        row("11", Some(zed_name), Some(ZED)),
        row("12", None, Some(BOB)),
        row("mock-0", None, None),
        row("13", Some("amy"), Some(AMY)),
    ]
}

/// With `PROMOTE_PINNED`, a pinned participant leaves the list the way the
/// grid promotes them in the meeting view.
fn harness() -> Element {
    let open = use_signal(|| START_OPEN.with(Cell::get));
    let badge_shown = use_signal(|| true);
    let mut pins = use_signal(|| {
        if PROMOTE_PINNED.with(Cell::get) {
            Vec::new()
        } else {
            vec![PinnedTile::camera(AMY)]
        }
    });
    let zed_name = use_signal(|| "Zed".to_string());
    let hosts = use_signal(|| HashSet::from([ZED.to_string()]));
    use_context_provider(|| HostSetCtx(hosts));
    use_hook(|| {
        HANDLES.with(|h| {
            *h.borrow_mut() = Some(Handles {
                open,
                badge_shown,
                pins,
                zed_name,
            })
        })
    });
    let on_toggle_pin = move |tile: PinnedTile| {
        PIN_CALLS.with(|c| c.borrow_mut().push(tile.clone()));
        let mut list = pins.peek().clone();
        pin_order::toggle_pin(&mut list, tile);
        pins.set(list);
    };
    let participants: Vec<OverflowParticipant> = everyone(&zed_name.read())
        .into_iter()
        .filter(|p| {
            !PROMOTE_PINNED.with(Cell::get)
                || !p
                    .user_id
                    .as_ref()
                    .is_some_and(|u| pins.read().contains(&PinnedTile::camera(u.clone())))
        })
        .collect();
    rsx! {
        if badge_shown() {
            GridOverflowBadge { overflow_count: participants.len(), open, on_open: |_| {} }
        }
        if open() && badge_shown() {
            OverflowParticipantList {
                open,
                participants,
                owner_user_id: Some(OWNER.to_string()),
                pins,
                on_toggle_pin,
                touch_hold_ms: TOUCH_HOLD_MS,
            }
        }
    }
}

async fn mount_with(start_open: bool, promote_pinned: bool) -> web_sys::Element {
    START_OPEN.with(|c| c.set(start_open));
    PROMOTE_PINNED.with(|c| c.set(promote_pinned));
    PIN_CALLS.with(|c| c.borrow_mut().clear());
    // A failed test skips its cleanup; its duplicate ids would mislead the next.
    while let Some(stale) = gloo_utils::document()
        .query_selector("body > div:has([id^='grid-overflow'])")
        .unwrap()
    {
        stale.remove();
    }
    let mount = create_mount_point();
    render_into(&mount, harness);
    settle().await;
    mount
}

async fn settle() {
    for _ in 0..3 {
        yield_now().await;
    }
    gloo_timers::future::TimeoutFuture::new(30).await;
    yield_now().await;
}

fn handles() -> Handles {
    HANDLES.with(|h| h.borrow().expect("the harness published its signals"))
}

fn find(mount: &web_sys::Element, selector: &str) -> Option<web_sys::HtmlElement> {
    mount
        .query_selector(selector)
        .unwrap()
        .map(|el| el.dyn_into().unwrap())
}

fn all(mount: &web_sys::Element, selector: &str) -> Vec<web_sys::Element> {
    let nodes = mount.query_selector_all(selector).unwrap();
    (0..nodes.length())
        .filter_map(|i| nodes.item(i))
        .map(|n| n.dyn_into().unwrap())
        .collect()
}

fn active_id() -> String {
    gloo_utils::document()
        .active_element()
        .map(|el| el.id())
        .unwrap_or_default()
}

fn press(el: &web_sys::Element, key: &str) {
    let dispatch = js_sys::Function::new_with_args(
        "el, key",
        "el.dispatchEvent(new KeyboardEvent('keydown', \
         { key: key, bubbles: true, cancelable: true }));",
    );
    dispatch
        .call2(&wasm_bindgen::JsValue::NULL, el, &key.into())
        .unwrap();
}

fn press_escape(el: &web_sys::Element) {
    press(el, "Escape");
}

/// `pointerenter`/`pointerleave` do not bubble; `pointerdown` does.
fn pointer(el: &web_sys::Element, kind: &str, pointer_type: &str) {
    let dispatch = js_sys::Function::new_with_args(
        "el, kind, pointerType",
        "el.dispatchEvent(new PointerEvent(kind, \
         { pointerType: pointerType, bubbles: kind === 'pointerdown' }));",
    );
    dispatch
        .call3(
            &wasm_bindgen::JsValue::NULL,
            el,
            &kind.into(),
            &pointer_type.into(),
        )
        .unwrap();
}

#[wasm_bindgen_test]
async fn rows_show_names_sorted_and_never_a_user_id() {
    let mount = mount_with(true, false).await;

    let list = find(&mount, LIST).expect("the list is open");
    let names: Vec<String> = all(&mount, "#grid-overflow-list li .grid-overflow-row-name")
        .iter()
        .map(|n| n.text_content().unwrap_or_default())
        .collect();
    assert_eq!(names, ["amy", "Participant", "Participant", "Zed"]);
    assert_eq!(
        find(&mount, "#grid-overflow-list-title")
            .unwrap()
            .text_content()
            .as_deref(),
        Some("4 more in meeting")
    );
    let html = list.outer_html();
    assert!(
        !html.contains('@'),
        "a user id leaked into the list: {html}"
    );
    assert_eq!(
        find(&mount, "#grid-overflow-row-11 .peer-indicator")
            .map(|t| t.text_content().unwrap_or_default()),
        Some("(Host)".to_string())
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_host_rows_role_reaches_screen_readers_as_its_description() {
    let mount = mount_with(true, false).await;

    let zed = find(&mount, "#grid-overflow-row-11").unwrap();
    assert_eq!(zed.get_attribute("aria-label").as_deref(), Some("Pin Zed"));
    let described_by = zed
        .get_attribute("aria-describedby")
        .expect("a host row names its role tag");
    let tag = gloo_utils::document()
        .get_element_by_id(&described_by)
        .expect("aria-describedby must resolve");
    assert_eq!(tag.text_content().as_deref(), Some("(Host)"));
    let bob = find(&mount, "#grid-overflow-row-12").unwrap();
    assert_eq!(bob.get_attribute("aria-describedby"), None);

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_mock_row_is_listed_without_a_pin_button() {
    let mount = mount_with(true, false).await;

    let items = all(&mount, "#grid-overflow-list li");
    assert_eq!(items.len(), 4);
    assert_eq!(all(&mount, "#grid-overflow-list li > button").len(), 3);
    assert!(items[2].query_selector("button").unwrap().is_none());

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn clicking_a_row_pins_that_participants_camera() {
    let mount = mount_with(true, false).await;

    find(&mount, "#grid-overflow-row-12").unwrap().click();
    settle().await;

    let calls = PIN_CALLS.with(|c| c.borrow().clone());
    assert_eq!(calls, [PinnedTile::camera(BOB)]);
    assert!(
        find(&mount, LIST).is_some(),
        "the list stays open after a pin"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn aria_pressed_follows_the_pins_while_the_name_stays_fixed() {
    let mount = mount_with(true, false).await;

    let amy = find(&mount, "#grid-overflow-row-13").unwrap();
    assert_eq!(amy.get_attribute("aria-pressed").as_deref(), Some("true"));
    assert_eq!(amy.get_attribute("title").as_deref(), Some("Unpin amy"));
    assert_eq!(amy.get_attribute("aria-label").as_deref(), Some("Pin amy"));
    let zed = find(&mount, "#grid-overflow-row-11").unwrap();
    assert_eq!(zed.get_attribute("aria-pressed").as_deref(), Some("false"));
    assert_eq!(zed.get_attribute("title").as_deref(), Some("Pin Zed"));

    zed.click();
    settle().await;
    let zed = find(&mount, "#grid-overflow-row-11").unwrap();
    assert_eq!(zed.get_attribute("aria-pressed").as_deref(), Some("true"));
    assert_eq!(zed.get_attribute("aria-label").as_deref(), Some("Pin Zed"));

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn opening_focuses_the_first_row() {
    let mount = mount_with(false, false).await;

    let badge = find(&mount, "#grid-overflow-badge").unwrap();
    badge.focus().unwrap();
    badge.click();
    settle().await;

    assert_eq!(active_id(), "grid-overflow-row-13");
    assert!(
        find(&mount, "#grid-overflow-list[data-placed]").is_some(),
        "the list is placed, so it is no longer transparent"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_list_is_placed_again_when_the_badge_resizes() {
    let mount = mount_with(true, false).await;

    let list = find(&mount, LIST).unwrap();
    list.remove_attribute("data-placed").unwrap();
    find(&mount, "#grid-overflow-badge")
        .unwrap()
        .style()
        .set_property("width", "123px")
        .unwrap();
    settle().await;

    assert!(list.has_attribute("data-placed"));

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn arrow_keys_move_between_the_rows_and_the_close_button() {
    let mount = mount_with(true, false).await;

    let first = find(&mount, "#grid-overflow-row-13").unwrap();
    first.focus().unwrap();
    let close = find(&mount, "#grid-overflow-list-close").unwrap();
    press(&first, "ArrowDown");
    assert_eq!(active_id(), "grid-overflow-row-12");
    press(&find(&mount, "#grid-overflow-row-12").unwrap(), "End");
    assert_eq!(
        active_id(),
        "grid-overflow-row-11",
        "End stops on the last row"
    );
    press(&find(&mount, "#grid-overflow-row-11").unwrap(), "ArrowDown");
    assert_eq!(active_id(), "grid-overflow-list-close");
    press(&close, "ArrowDown");
    assert_eq!(active_id(), "grid-overflow-row-13");
    press(&first, "ArrowUp");
    assert_eq!(active_id(), "grid-overflow-list-close");
    press(&close, "End");
    assert_eq!(active_id(), "grid-overflow-row-11");
    press(&close, "Home");
    assert_eq!(active_id(), "grid-overflow-row-13");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn escape_closes_and_returns_focus_to_the_badge() {
    let mount = mount_with(true, false).await;

    let row = find(&mount, "#grid-overflow-row-12").unwrap();
    row.focus().unwrap();
    press_escape(&row);
    settle().await;

    assert!(find(&mount, LIST).is_none());
    assert!(!*handles().open.peek());
    assert_eq!(active_id(), "grid-overflow-badge");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_close_button_closes_and_returns_focus_to_the_badge() {
    let mount = mount_with(true, false).await;

    find(&mount, "#grid-overflow-list-close").unwrap().click();
    settle().await;

    assert!(find(&mount, LIST).is_none());
    assert_eq!(active_id(), "grid-overflow-badge");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_click_outside_closes_but_a_click_inside_does_not() {
    let mount = mount_with(true, false).await;

    find(&mount, "#grid-overflow-list-title").unwrap().click();
    settle().await;
    assert!(find(&mount, LIST).is_some());

    gloo_utils::document().body().unwrap().click();
    settle().await;
    assert!(find(&mount, LIST).is_none());
    assert!(!*handles().open.peek());

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn losing_the_badge_closes_the_list() {
    let mount = mount_with(true, false).await;

    handles().badge_shown.set(false);
    settle().await;

    assert!(!*handles().open.peek());
    handles().badge_shown.set(true);
    settle().await;
    assert!(find(&mount, LIST).is_none(), "the list must not reopen");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_pinned_row_that_leaves_hands_focus_to_the_next_row() {
    let mount = mount_with(true, true).await;

    let bob = find(&mount, "#grid-overflow-row-12").unwrap();
    bob.focus().unwrap();
    bob.click();
    settle().await;

    assert!(find(&mount, "#grid-overflow-row-12").is_none());
    assert_eq!(active_id(), "grid-overflow-row-11");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn rows_hold_still_under_the_pointer_and_resync_when_it_leaves() {
    let mount = mount_with(true, true).await;

    let list = find(&mount, LIST).unwrap();
    pointer(&list, "pointerenter", "mouse");
    settle().await;
    find(&mount, "#grid-overflow-row-12").unwrap().click();
    settle().await;
    assert_eq!(
        PIN_CALLS.with(|c| c.borrow().clone()),
        [PinnedTile::camera(BOB)]
    );
    let held = find(&mount, "#grid-overflow-row-12").expect("a pinned row holds while hovered");
    assert_eq!(held.get_attribute("aria-pressed").as_deref(), Some("true"));
    assert_eq!(
        find(&mount, "#grid-overflow-list-title")
            .unwrap()
            .text_content()
            .as_deref(),
        Some("4 more in meeting")
    );

    pointer(&list, "pointerleave", "mouse");
    settle().await;
    assert!(find(&mount, "#grid-overflow-row-12").is_none());
    assert_eq!(
        find(&mount, "#grid-overflow-list-title")
            .unwrap()
            .text_content()
            .as_deref(),
        Some("3 more in meeting")
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_touch_hold_outlives_pointerleave_until_the_idle_timeout() {
    let mount = mount_with(true, true).await;

    let list = find(&mount, LIST).unwrap();
    pointer(&list, "pointerenter", "touch");
    let bob = find(&mount, "#grid-overflow-row-12").unwrap();
    pointer(&bob, "pointerdown", "touch");
    pointer(&list, "pointerleave", "touch");
    bob.click();
    settle().await;

    assert_eq!(
        PIN_CALLS.with(|c| c.borrow().clone()),
        [PinnedTile::camera(BOB)]
    );
    assert!(
        find(&mount, "#grid-overflow-row-12").is_some(),
        "a touch pointerleave right after the tap must not release the hold"
    );

    gloo_timers::future::TimeoutFuture::new(TOUCH_HOLD_MS + 200).await;
    settle().await;
    assert!(
        find(&mount, "#grid-overflow-row-12").is_none(),
        "the hold ends once the list has been idle"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_pen_hover_hold_releases_after_the_idle_timeout_without_a_touch() {
    let mount = mount_with(true, true).await;

    let list = find(&mount, LIST).unwrap();
    pointer(&list, "pointerenter", "pen");
    let bob = find(&mount, "#grid-overflow-row-12").unwrap();
    bob.click();
    pointer(&list, "pointerleave", "pen");
    settle().await;
    assert!(
        find(&mount, "#grid-overflow-row-12").is_some(),
        "a pen hover holds the rows"
    );

    gloo_timers::future::TimeoutFuture::new(TOUCH_HOLD_MS + 200).await;
    settle().await;
    assert!(
        find(&mount, "#grid-overflow-row-12").is_none(),
        "a hover that never touches down still releases"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_focused_row_that_a_rename_moves_keeps_focus() {
    let mount = mount_with(true, false).await;

    find(&mount, "#grid-overflow-row-11")
        .unwrap()
        .focus()
        .unwrap();
    handles().zed_name.set("Aaron".to_string());
    settle().await;

    let names: Vec<String> = all(&mount, "#grid-overflow-list li .grid-overflow-row-name")
        .iter()
        .map(|n| n.text_content().unwrap_or_default())
        .collect();
    assert_eq!(names[0], "Aaron", "the rename moves the row to the top");
    assert_eq!(active_id(), "grid-overflow-row-11");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_focused_row_that_leaves_hands_focus_to_its_neighbour_not_row_one() {
    let mount = mount_with(true, true).await;

    find(&mount, "#grid-overflow-row-11")
        .unwrap()
        .focus()
        .unwrap();
    handles().pins.set(vec![PinnedTile::camera(ZED)]);
    settle().await;

    assert!(find(&mount, "#grid-overflow-row-11").is_none());
    assert_eq!(active_id(), "grid-overflow-row-12");

    cleanup(&mount);
}

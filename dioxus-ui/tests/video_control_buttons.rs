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
    CameraButton, DeviceSettingsButton, DiagnosticsButton, HangUpButton, MeetingOptionsButton,
    MicButton, MockPeersButton, OverflowShareItems, PeerListButton, ScreenShareButton,
    ScreenShareControl, ShareButtonState,
};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

fn assert_button_aria_label_matches_tooltip_title(mount: &web_sys::Element) {
    let button = mount
        .query_selector("button")
        .unwrap()
        .expect("button should exist")
        .dyn_into::<web_sys::HtmlButtonElement>()
        .unwrap();
    let aria_label = button
        .get_attribute("aria-label")
        .expect("button should expose an aria-label");
    assert!(
        !aria_label.trim().is_empty(),
        "aria-label should be non-empty"
    );

    let title = mount
        .query_selector(".tooltip .tooltip-title")
        .unwrap()
        .expect(".tooltip-title should exist");
    let title_text = title
        .text_content()
        .expect(".tooltip-title should expose text content");
    assert_eq!(
        aria_label, title_text,
        "button aria-label should match tooltip title"
    );
}

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

    assert_button_aria_label_matches_tooltip_title(&mount);

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

    assert_button_aria_label_matches_tooltip_title(&mount);

    cleanup(&mount);
}

// Regression guard (device-in-use deadlock fix): when a device is unavailable
// (in use by another app, denied, or unplugged) the button MUST stay clickable
// so the user can retry acquisition. Previously `disabled: !available` wired the
// unavailable state straight to the HTML `disabled` attribute, killing the only
// manual-retry path (`onclick`) and forcing a leave-and-rejoin. This asserts the
// button is NOT disabled while `available: false`.
#[wasm_bindgen_test]
async fn mic_button_stays_clickable_when_unavailable() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { MicButton { enabled: false, available: false, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let button = mount
        .query_selector("button")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlButtonElement>()
        .unwrap();
    assert!(
        !button.disabled(),
        "unavailable MicButton must remain clickable so the user can retry"
    );
    // The unavailable state must still be conveyed visually.
    assert!(
        button.class_list().contains("error"),
        "unavailable MicButton should carry the 'error' CSS class"
    );
    // Issue #1793 follow-up: the unavailable tooltip becomes part of the button's
    // accessible name, so it must be accurate for EVERY unavailable reason (in
    // use / denied / unplugged), not just "not detected". Assert the generalized
    // wording and that it no longer makes the misleading "detected" claim.
    let desc = mount
        .query_selector(".tooltip .tooltip-desc")
        .unwrap()
        .expect(".tooltip-desc should exist so users see what the button does");
    let desc_text = desc.text_content().unwrap();
    assert_eq!(
        desc_text, "Microphone unavailable — click to retry.",
        "unavailable MicButton tooltip must use the generalized wording, got: {desc_text:?}"
    );
    assert!(
        !desc_text.contains("detected"),
        "unavailable MicButton tooltip must not claim the device was 'not detected', got: {desc_text:?}"
    );

    assert_eq!(
        button.get_attribute("aria-label").as_deref(),
        Some("Microphone unavailable — click to retry."),
        "unavailable MicButton must expose its actual unavailable state"
    );

    assert_button_aria_label_matches_tooltip_title(&mount);

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

    assert_button_aria_label_matches_tooltip_title(&mount);

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

    assert_button_aria_label_matches_tooltip_title(&mount);

    cleanup(&mount);
}

// Regression guard (device-in-use deadlock fix): symmetric to the MicButton
// case above — an unavailable camera must stay clickable so the user can retry.
#[wasm_bindgen_test]
async fn camera_button_stays_clickable_when_unavailable() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { CameraButton { enabled: false, available: false, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let button = mount
        .query_selector("button")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlButtonElement>()
        .unwrap();
    assert!(
        !button.disabled(),
        "unavailable CameraButton must remain clickable so the user can retry"
    );
    assert!(
        button.class_list().contains("error"),
        "unavailable CameraButton should carry the 'error' CSS class"
    );
    // Issue #1793 follow-up: symmetric to the MicButton assertion above.
    let desc = mount
        .query_selector(".tooltip .tooltip-desc")
        .unwrap()
        .expect(".tooltip-desc should exist so users see what the button does");
    let desc_text = desc.text_content().unwrap();
    assert_eq!(
        desc_text, "Camera unavailable — click to retry.",
        "unavailable CameraButton tooltip must use the generalized wording, got: {desc_text:?}"
    );
    assert!(
        !desc_text.contains("detected"),
        "unavailable CameraButton tooltip must not claim the device was 'not detected', got: {desc_text:?}"
    );

    assert_eq!(
        button.get_attribute("aria-label").as_deref(),
        Some("Camera unavailable — click to retry."),
        "unavailable CameraButton must expose its actual unavailable state"
    );

    assert_button_aria_label_matches_tooltip_title(&mount);

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

    assert_button_aria_label_matches_tooltip_title(&mount);

    cleanup(&mount);
}

// Active state (issue 3173): the button opens the sharing options instead of
// stopping, so its name must not promise a stop, and must not match the idle
// e2e selector substring "Share Screen".
#[wasm_bindgen_test]
async fn screen_share_button_active_offers_the_sharing_options_menu() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { ScreenShareButton { active: true, disabled: false, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let title_text = mount
        .query_selector(".tooltip .tooltip-title")
        .unwrap()
        .expect(".tooltip-title should exist")
        .text_content()
        .unwrap();
    assert_eq!(title_text, "Screen share — Sharing options");
    assert!(
        !title_text.to_lowercase().contains("share screen"),
        "active title must stay distinct from the idle e2e selector, got: {title_text:?}"
    );
    assert!(!title_text.contains("Stop Screen Share"));

    let button = mount.query_selector("button").unwrap().unwrap();
    assert!(button.class_list().contains("active"));
    assert_eq!(
        button.get_attribute("aria-haspopup").as_deref(),
        Some("menu")
    );
    assert_eq!(
        button.get_attribute("aria-expanded").as_deref(),
        Some("false")
    );
    assert_eq!(
        button.get_attribute("aria-controls"),
        None,
        "no menu is mounted"
    );
    assert_eq!(
        button.get_attribute("data-state").as_deref(),
        Some("active")
    );

    assert_button_aria_label_matches_tooltip_title(&mount);

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn screen_share_button_while_replacing_stays_focusable_but_busy() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { ScreenShareButton { active: true, disabled: false, busy: true, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let button = mount
        .query_selector("button")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlButtonElement>()
        .unwrap();
    assert!(!button.disabled(), "a native disabled button drops focus");
    assert_eq!(
        button.get_attribute("aria-disabled").as_deref(),
        Some("true")
    );
    assert_eq!(button.get_attribute("aria-busy").as_deref(), Some("true"));
    assert_eq!(button.get_attribute("aria-haspopup"), None);
    assert_eq!(
        button.get_attribute("data-state").as_deref(),
        Some("replacing")
    );
    assert_eq!(
        button.get_attribute("aria-label").as_deref(),
        Some("Screen share — Choosing new content")
    );

    cleanup(&mount);
}

// ---------------------------------------------------------------------------
// ScreenShareControl (issue 3173): the menu a live share opens
// ---------------------------------------------------------------------------

thread_local! {
    static CONTROL_STATE: std::cell::Cell<ShareButtonState> =
        const { std::cell::Cell::new(ShareButtonState::Active) };
    static CONTROL_LOCKED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[allow(non_snake_case)]
fn ControlHarness() -> Element {
    let open = use_signal(|| false);
    let mut calls = use_signal(Vec::<&'static str>::new);
    let state = CONTROL_STATE.with(|s| s.get());
    let mut locked = use_signal(|| CONTROL_LOCKED.with(|l| l.get()));
    let log = calls.read().join(",");
    rsx! {
        div {
            class: "action-bar-slot-wrapper",
            "data-slot": "screen",
            style: if locked() { "display: none;" } else { "" },
            ScreenShareControl {
                state,
                heading: "You're sharing a window",
                open,
                locked: locked(),
                on_start: move |_| calls.write().push("start"),
                on_open: move |_| calls.write().push("open"),
                on_replace: move |_| calls.write().push("replace"),
                on_stop: move |_| calls.write().push("stop"),
            }
        }
        button { id: "outside" }
        button { id: "overflow-menu-trigger" }
        button { id: "overflow-the-slot", onclick: move |_| locked.set(true) }
        span { id: "calls", "{log}" }
        span { id: "open", "{open}" }
    }
}

async fn mount_control(state: ShareButtonState, locked: bool) -> web_sys::Element {
    CONTROL_STATE.with(|s| s.set(state));
    CONTROL_LOCKED.with(|l| l.set(locked));
    let mount = create_mount_point();
    render_into(&mount, ControlHarness);
    yield_now().await;
    mount
}

fn by_id(mount: &web_sys::Element, id: &str) -> Option<web_sys::HtmlElement> {
    mount
        .query_selector(&format!("#{id}"))
        .unwrap()
        .map(|e| e.dyn_into().unwrap())
}

fn text_of(mount: &web_sys::Element, id: &str) -> String {
    by_id(mount, id).unwrap().text_content().unwrap_or_default()
}

fn focused_id() -> String {
    gloo_utils::document()
        .active_element()
        .map(|e| e.id())
        .unwrap_or_default()
}

fn key(el: &web_sys::HtmlElement, key: &str) {
    js_sys::Function::new_with_args(
        "el, key",
        "el.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true }));",
    )
    .call2(&wasm_bindgen::JsValue::NULL, el, &key.into())
    .unwrap();
}

async fn eventually(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..100 {
        if cond() {
            return;
        }
        gloo_timers::future::TimeoutFuture::new(10).await;
    }
    panic!("timed out waiting for {what}");
}

#[wasm_bindgen_test]
async fn a_live_share_click_opens_a_two_item_menu_on_the_first_item() {
    let mount = mount_control(ShareButtonState::Active, false).await;
    let trigger = by_id(&mount, "screen-share-trigger").expect("the trigger keeps a stable id");
    trigger.click();
    eventually("the first item to take focus", || {
        focused_id() == "screen-share-menu-replace"
    })
    .await;

    let menu = by_id(&mount, "screen-share-menu").expect("the menu is mounted");
    assert_eq!(menu.get_attribute("role").as_deref(), Some("menu"));
    assert_eq!(
        menu.get_attribute("aria-labelledby").as_deref(),
        Some("screen-share-menu-heading")
    );
    let items = menu.query_selector_all("[role='menuitem']").unwrap();
    assert_eq!(items.length(), 2);
    assert_eq!(
        text_of(&mount, "screen-share-menu-replace"),
        "Share something else"
    );
    assert_eq!(text_of(&mount, "screen-share-menu-stop"), "Stop sharing");
    assert_eq!(
        trigger.get_attribute("aria-expanded").as_deref(),
        Some("true")
    );
    assert_eq!(
        trigger.get_attribute("aria-controls").as_deref(),
        Some("screen-share-menu")
    );
    assert_eq!(
        text_of(&mount, "calls"),
        "open",
        "siblings are closed on open"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn escape_closes_the_menu_and_returns_focus_to_the_trigger() {
    let mount = mount_control(ShareButtonState::Active, false).await;
    by_id(&mount, "screen-share-trigger").unwrap().click();
    eventually("the menu to open", || {
        focused_id() == "screen-share-menu-replace"
    })
    .await;

    key(
        &by_id(&mount, "screen-share-menu-replace").unwrap(),
        "Escape",
    );
    eventually("the menu to close", || {
        by_id(&mount, "screen-share-menu").is_none()
    })
    .await;
    assert_eq!(focused_id(), "screen-share-trigger");
    assert_eq!(
        text_of(&mount, "calls"),
        "open",
        "Escape neither replaces nor stops"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_arrow_keys_rove_the_items_and_tab_leaves_by_the_trigger() {
    let mount = mount_control(ShareButtonState::Active, false).await;
    let trigger = by_id(&mount, "screen-share-trigger").unwrap();
    key(&trigger, "ArrowUp");
    eventually("ArrowUp to open on the last item", || {
        focused_id() == "screen-share-menu-stop"
    })
    .await;

    let stop = by_id(&mount, "screen-share-menu-stop").unwrap();
    key(&stop, "ArrowDown");
    eventually("ArrowDown to wrap to the first item", || {
        focused_id() == "screen-share-menu-replace"
    })
    .await;
    let replace = by_id(&mount, "screen-share-menu-replace").unwrap();
    eventually("the roving tabindex to follow", || {
        replace.get_attribute("tabindex").as_deref() == Some("0")
            && stop.get_attribute("tabindex").as_deref() == Some("-1")
    })
    .await;

    key(&replace, "End");
    eventually("End to reach the last item", || {
        focused_id() == "screen-share-menu-stop"
    })
    .await;

    key(&stop, "Tab");
    eventually("Tab to close the menu", || {
        by_id(&mount, "screen-share-menu").is_none()
    })
    .await;
    assert_eq!(focused_id(), "screen-share-trigger");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_trigger_closes_its_own_menu_by_click_or_escape() {
    for close in ["click", "Escape"] {
        let mount = mount_control(ShareButtonState::Active, false).await;
        let trigger = by_id(&mount, "screen-share-trigger").unwrap();
        trigger.click();
        eventually("the menu to open", || {
            by_id(&mount, "screen-share-menu").is_some()
        })
        .await;
        trigger.focus().unwrap();
        if close == "click" {
            trigger.click();
        } else {
            key(&trigger, "Escape");
        }
        eventually("the menu to close", || {
            by_id(&mount, "screen-share-menu").is_none()
        })
        .await;
        assert_eq!(focused_id(), "screen-share-trigger", "{close}");
        cleanup(&mount);
    }
}

#[wasm_bindgen_test]
async fn home_returns_to_the_first_item() {
    let mount = mount_control(ShareButtonState::Active, false).await;
    key(&by_id(&mount, "screen-share-trigger").unwrap(), "ArrowUp");
    eventually("the last item", || focused_id() == "screen-share-menu-stop").await;
    key(&by_id(&mount, "screen-share-menu-stop").unwrap(), "Home");
    eventually("the first item", || {
        focused_id() == "screen-share-menu-replace"
    })
    .await;
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn focus_leaving_the_menu_closes_it() {
    let mount = mount_control(ShareButtonState::Active, false).await;
    by_id(&mount, "screen-share-trigger").unwrap().click();
    eventually("the menu to open", || {
        focused_id() == "screen-share-menu-replace"
    })
    .await;
    by_id(&mount, "outside").unwrap().focus().unwrap();
    eventually("the menu to close", || {
        by_id(&mount, "screen-share-menu").is_none()
    })
    .await;
    assert_eq!(focused_id(), "outside");
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_blur_to_nowhere_leaves_the_menu_open() {
    let mount = mount_control(ShareButtonState::Active, false).await;
    by_id(&mount, "screen-share-trigger").unwrap().click();
    eventually("the menu to open", || {
        focused_id() == "screen-share-menu-replace"
    })
    .await;
    js_sys::Function::new_with_args(
        "el",
        "el.dispatchEvent(new FocusEvent('focusout', { bubbles: true, relatedTarget: null }));",
    )
    .call1(
        &wasm_bindgen::JsValue::NULL,
        &by_id(&mount, "screen-share-menu-replace").unwrap(),
    )
    .unwrap();
    yield_now().await;
    assert!(
        by_id(&mount, "screen-share-menu").is_some(),
        "a Safari click blurs to nowhere"
    );
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_menu_pushed_into_the_overflow_hands_focus_to_the_overflow_trigger() {
    let mount = mount_control(ShareButtonState::Active, false).await;
    by_id(&mount, "screen-share-trigger").unwrap().click();
    eventually("the menu to open", || {
        focused_id() == "screen-share-menu-replace"
    })
    .await;
    by_id(&mount, "overflow-the-slot").unwrap().click();
    eventually("the menu to close", || {
        by_id(&mount, "screen-share-menu").is_none()
    })
    .await;
    assert_eq!(focused_id(), "overflow-menu-trigger");
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn each_item_runs_its_action_once_and_closes_the_menu() {
    for (item, call) in [
        ("screen-share-menu-replace", "open,replace"),
        ("screen-share-menu-stop", "open,stop"),
    ] {
        let mount = mount_control(ShareButtonState::Active, false).await;
        by_id(&mount, "screen-share-trigger").unwrap().click();
        eventually("the menu to open", || by_id(&mount, item).is_some()).await;

        by_id(&mount, item).unwrap().click();
        eventually("the menu to close", || {
            by_id(&mount, "screen-share-menu").is_none()
        })
        .await;
        assert_eq!(text_of(&mount, "calls"), call, "{item}");
        assert_eq!(focused_id(), "screen-share-trigger", "{item}");

        cleanup(&mount);
    }
}

#[wasm_bindgen_test]
async fn a_click_with_no_live_share_never_opens_a_menu() {
    for (state, locked, calls) in [
        (ShareButtonState::Idle, false, "start"),
        (ShareButtonState::Replacing, false, ""),
        (ShareButtonState::Active, true, ""),
    ] {
        let mount = mount_control(state, locked).await;
        by_id(&mount, "screen-share-trigger").unwrap().click();
        key(&by_id(&mount, "screen-share-trigger").unwrap(), "ArrowDown");
        yield_now().await;
        assert!(
            by_id(&mount, "screen-share-menu").is_none(),
            "{state:?} locked={locked}: no menu"
        );
        assert_eq!(text_of(&mount, "calls"), calls, "{state:?} locked={locked}");

        cleanup(&mount);
    }
}

#[allow(non_snake_case)]
fn LiveLockHarness() -> Element {
    let open = use_signal(|| false);
    let mut calls = use_signal(Vec::<&'static str>::new);
    let live_lock = use_signal(|| true);
    let state = CONTROL_STATE.with(|s| s.get());
    let log = calls.read().join(",");
    rsx! {
        ScreenShareControl {
            state,
            heading: "You're sharing a window",
            open,
            live_lock,
            on_start: move |_| calls.write().push("start"),
            on_open: move |_| calls.write().push("open"),
            on_replace: move |_| calls.write().push("replace"),
            on_stop: move |_| calls.write().push("stop"),
        }
        span { id: "calls", "{log}" }
    }
}

#[wasm_bindgen_test]
async fn a_live_lock_blocks_the_click_while_the_locked_prop_is_still_false() {
    for state in [ShareButtonState::Idle, ShareButtonState::Active] {
        CONTROL_STATE.with(|s| s.set(state));
        let mount = create_mount_point();
        render_into(&mount, LiveLockHarness);
        yield_now().await;
        by_id(&mount, "screen-share-trigger").unwrap().click();
        yield_now().await;
        yield_now().await;
        assert!(
            by_id(&mount, "screen-share-menu").is_none(),
            "{state:?}: no menu"
        );
        assert_eq!(
            text_of(&mount, "calls"),
            "",
            "{state:?}: nothing started or opened"
        );
        cleanup(&mount);
    }
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

    assert_button_aria_label_matches_tooltip_title(&mount);

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

    assert_button_aria_label_matches_tooltip_title(&mount);

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

    assert_button_aria_label_matches_tooltip_title(&mount);

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

    assert_button_aria_label_matches_tooltip_title(&mount);

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

    assert_button_aria_label_matches_tooltip_title(&mount);

    cleanup(&mount);
}

// ---------------------------------------------------------------------------
// Additional action-bar button aria-label coverage
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn peer_list_button_open_and_closed_match_tooltip_titles() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { PeerListButton { open: false, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;
    assert_button_aria_label_matches_tooltip_title(&mount);

    cleanup(&mount);

    let mount = create_mount_point();
    fn wrapper_open() -> Element {
        rsx! { PeerListButton { open: true, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper_open);
    yield_now().await;
    assert_button_aria_label_matches_tooltip_title(&mount);

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn diagnostics_button_open_and_closed_match_tooltip_titles() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { DiagnosticsButton { open: false, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;
    assert_button_aria_label_matches_tooltip_title(&mount);

    cleanup(&mount);

    let mount = create_mount_point();
    fn wrapper_open() -> Element {
        rsx! { DiagnosticsButton { open: true, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper_open);
    yield_now().await;
    assert_button_aria_label_matches_tooltip_title(&mount);

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn mock_peers_button_matches_tooltip_title() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { MockPeersButton { open: false, onclick: move |_| {} } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    assert_button_aria_label_matches_tooltip_title(&mount);

    cleanup(&mount);
}

mod captions_button {
    use super::*;
    use dioxus_ui::components::captions::{
        captions_button_state, CaptionsButtonState, CaptionsRole, CAPTIONS_OFF_HINT,
    };
    use dioxus_ui::components::video_control_buttons::CaptionsButton;
    use std::cell::Cell;

    thread_local! {
        static STATE: Cell<Option<CaptionsButtonState>> = const { Cell::new(None) };
    }

    #[allow(non_snake_case)]
    fn Harness() -> Element {
        let state = STATE.with(Cell::get).expect("state");
        rsx! { CaptionsButton { state, onclick: |_| {} } }
    }

    async fn render(state: CaptionsButtonState) -> (web_sys::Element, web_sys::Element) {
        STATE.with(|s| s.set(Some(state)));
        let mount = create_mount_point();
        render_into(&mount, Harness);
        yield_now().await;
        let button = mount
            .query_selector("[data-testid='captions-button']")
            .unwrap()
            .expect("captions button");
        (mount, button)
    }

    fn desc(mount: &web_sys::Element) -> String {
        mount
            .query_selector(".tooltip-desc")
            .unwrap()
            .unwrap()
            .text_content()
            .unwrap_or_default()
    }

    #[wasm_bindgen_test]
    async fn a_host_button_names_the_room_action() {
        let (mount, button) = render(captions_button_state(
            CaptionsRole::Host,
            false,
            false,
            false,
            false,
        ))
        .await;
        assert_button_aria_label_matches_tooltip_title(&mount);
        assert_eq!(
            button.get_attribute("data-captions-role").as_deref(),
            Some("host")
        );
        assert_eq!(
            button.get_attribute("aria-pressed").as_deref(),
            Some("false")
        );
        assert_eq!(button.get_attribute("aria-disabled"), None);
        assert_eq!(desc(&mount), "Turn on captions for everyone");
        cleanup(&mount);

        let (mount, button) = render(captions_button_state(
            CaptionsRole::Host,
            false,
            false,
            true,
            false,
        ))
        .await;
        assert_eq!(
            button.get_attribute("aria-pressed").as_deref(),
            Some("true")
        );
        assert_eq!(desc(&mount), "Turn off captions for everyone");
        cleanup(&mount);
    }

    #[wasm_bindgen_test]
    async fn a_viewer_button_is_aria_disabled_with_a_hint_while_captions_are_off() {
        let (mount, button) = render(captions_button_state(
            CaptionsRole::Viewer,
            false,
            false,
            false,
            true,
        ))
        .await;
        assert_button_aria_label_matches_tooltip_title(&mount);
        assert_eq!(
            button.get_attribute("data-captions-role").as_deref(),
            Some("viewer")
        );
        assert_eq!(
            button.get_attribute("aria-disabled").as_deref(),
            Some("true")
        );
        assert_eq!(
            button.get_attribute("aria-pressed").as_deref(),
            Some("false")
        );
        assert!(button.class_list().contains("captions-button--unavailable"));
        assert_eq!(desc(&mount), CAPTIONS_OFF_HINT);
        let described = button.get_attribute("aria-describedby").unwrap();
        let hint = mount
            .query_selector(&format!("#{described}"))
            .unwrap()
            .expect("aria-describedby target");
        assert_eq!(hint.text_content().as_deref(), Some(CAPTIONS_OFF_HINT));
        cleanup(&mount);

        let (mount, button) = render(captions_button_state(
            CaptionsRole::Viewer,
            false,
            false,
            true,
            true,
        ))
        .await;
        assert_eq!(button.get_attribute("aria-disabled"), None);
        assert_eq!(
            button.get_attribute("aria-pressed").as_deref(),
            Some("true")
        );
        assert_eq!(desc(&mount), "Hide captions");
        cleanup(&mount);
    }
}

// ---------------------------------------------------------------------------
// OverflowShareItems (issue 3173): the overflow popover's share entry
// ---------------------------------------------------------------------------

thread_local! {
    static OVERFLOW_STATE: std::cell::Cell<ShareButtonState> =
        const { std::cell::Cell::new(ShareButtonState::Active) };
}

#[allow(non_snake_case)]
fn OverflowHarness() -> Element {
    let mut calls = use_signal(Vec::<&'static str>::new);
    let state = OVERFLOW_STATE.with(|s| s.get());
    let log = calls.read().join(",");
    rsx! {
        button { id: "overflow-menu-trigger" }
        div { class: "action-bar-overflow-popover",
            OverflowShareItems {
                state,
                icon: rsx! {},
                on_done: move |_| calls.write().push("done"),
                on_start: move |_| calls.write().push("start"),
                on_replace: move |_| calls.write().push("replace"),
                on_stop: move |_| calls.write().push("stop"),
            }
        }
        span { id: "calls", "{log}" }
    }
}

async fn mount_overflow(state: ShareButtonState) -> web_sys::Element {
    OVERFLOW_STATE.with(|s| s.set(state));
    let mount = create_mount_point();
    render_into(&mount, OverflowHarness);
    yield_now().await;
    mount
}

fn overflow_items(mount: &web_sys::Element) -> Vec<web_sys::HtmlElement> {
    let items = mount.query_selector_all(".overflow-item").unwrap();
    (0..items.length())
        .map(|i| items.get(i).unwrap().dyn_into().unwrap())
        .collect()
}

#[wasm_bindgen_test]
async fn a_live_share_offers_both_choices_in_the_overflow_and_hands_focus_back() {
    for (item, call) in [
        ("overflow-share-replace", "done,replace"),
        ("overflow-share-stop", "done,stop"),
    ] {
        let mount = mount_overflow(ShareButtonState::Active).await;
        let group = mount.query_selector("[role='group']").unwrap().unwrap();
        assert_eq!(
            group.get_attribute("aria-label").as_deref(),
            Some("Screen share")
        );
        let items = overflow_items(&mount);
        let names: Vec<String> = items.iter().map(|i| i.text_content().unwrap()).collect();
        assert_eq!(names, ["Share something else", "Stop sharing"]);

        let target = mount
            .query_selector(&format!("[data-testid='{item}']"))
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlElement>()
            .unwrap();
        target.focus().unwrap();
        target.click();
        yield_now().await;
        assert_eq!(text_of(&mount, "calls"), call, "{item}");
        assert_eq!(focused_id(), "overflow-menu-trigger", "{item}");
        cleanup(&mount);
    }
}

#[wasm_bindgen_test]
async fn an_idle_overflow_entry_starts_a_share() {
    let mount = mount_overflow(ShareButtonState::Idle).await;
    let items = overflow_items(&mount);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].text_content().unwrap(), "Screen share");
    assert!(items[0].get_attribute("aria-busy").is_none());
    assert!(items[0].get_attribute("aria-description").is_none());
    assert!(mount.query_selector("[role='group']").unwrap().is_none());
    items[0].click();
    yield_now().await;
    assert_eq!(text_of(&mount, "calls"), "done,start");
    assert_eq!(focused_id(), "overflow-menu-trigger");
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_overflow_entry_is_inert_while_a_picker_is_open() {
    for (state, count, desc) in [
        (
            ShareButtonState::Replacing,
            2,
            "Pick what to share in the browser dialog. Your current share continues until you choose.",
        ),
        (
            ShareButtonState::Requesting,
            1,
            "Choose what to share in the browser dialog",
        ),
    ] {
        let mount = mount_overflow(state).await;
        let items = overflow_items(&mount);
        assert_eq!(items.len(), count, "{state:?}");
        for item in &items {
            assert_eq!(item.get_attribute("aria-disabled").as_deref(), Some("true"));
            assert_eq!(item.get_attribute("aria-busy").as_deref(), Some("true"));
            assert_eq!(
                item.get_attribute("aria-description").as_deref(),
                Some(desc),
                "{state:?}"
            );
            item.click();
        }
        yield_now().await;
        assert_eq!(text_of(&mount, "calls"), "", "{state:?}");
        cleanup(&mount);
    }
}

#[wasm_bindgen_test]
async fn a_live_share_button_describes_its_menu_to_assistive_tech() {
    for (active, busy, desc) in [
        (true, false, Some("Share something else or stop sharing.")),
        (true, true, Some("Pick what to share in the browser dialog. Your current share continues until you choose.")),
        (false, false, None),
    ] {
        thread_local! {
            static PROPS: std::cell::Cell<(bool, bool)> = const { std::cell::Cell::new((false, false)) };
        }
        PROPS.with(|p| p.set((active, busy)));
        fn wrapper() -> Element {
            let (active, busy) = PROPS.with(|p| p.get());
            rsx! {
                ScreenShareButton {
                    id: "screen-share-trigger".to_string(),
                    active,
                    busy,
                    disabled: false,
                    onclick: move |_| {},
                }
            }
        }
        let mount = create_mount_point();
        render_into(&mount, wrapper);
        yield_now().await;
        let button = mount.query_selector("button").unwrap().unwrap();
        let described = button
            .get_attribute("aria-describedby")
            .and_then(|id| by_id(&mount, &id))
            .and_then(|el| el.text_content());
        assert_eq!(described.as_deref(), desc, "active={active} busy={busy}");
        cleanup(&mount);
    }
}

#[wasm_bindgen_test]
async fn a_live_share_button_keeps_its_customize_hint_beside_the_description() {
    fn wrapper() -> Element {
        rsx! {
            span { id: "customize-hint", "Drag to reorder" }
            ScreenShareButton {
                id: "screen-share-trigger".to_string(),
                active: true,
                disabled: false,
                describedby: Some("customize-hint"),
                onclick: move |_| {},
            }
        }
    }
    let mount = create_mount_point();
    render_into(&mount, wrapper);
    yield_now().await;
    let button = mount.query_selector("button").unwrap().unwrap();
    assert_eq!(
        button.get_attribute("aria-describedby").as_deref(),
        Some("customize-hint screen-share-trigger-desc")
    );
    cleanup(&mount);
}

fn app_stylesheets() -> web_sys::Element {
    let style = gloo_utils::document().create_element("style").unwrap();
    style.set_text_content(Some(&format!(
        "{}{}",
        include_str!("../static/style.css"),
        include_str!("../static/global.css")
    )));
    gloo_utils::document()
        .head()
        .unwrap()
        .append_child(&style)
        .unwrap();
    style
}

fn computed(el: &web_sys::Element, property: &str) -> String {
    gloo_utils::window()
        .get_computed_style(el)
        .unwrap()
        .unwrap()
        .get_property_value(property)
        .unwrap()
}

#[wasm_bindgen_test]
async fn a_busy_overflow_share_item_reads_as_unavailable_not_selected() {
    let style = app_stylesheets();
    for state in [ShareButtonState::Replacing, ShareButtonState::Requesting] {
        let mount = mount_overflow(state).await;
        let popover = mount
            .query_selector(".action-bar-overflow-popover")
            .unwrap()
            .unwrap();
        let captions = gloo_utils::document().create_element("button").unwrap();
        captions.set_class_name("overflow-item");
        captions.set_attribute("aria-busy", "true").unwrap();
        popover.append_child(&captions).unwrap();
        assert_ne!(computed(&captions, "background-color"), "rgba(0, 0, 0, 0)");
        assert_ne!(computed(&captions, "box-shadow"), "none");

        let items = mount.query_selector_all(".overflow-share-item").unwrap();
        assert!(items.length() > 0, "{state:?}");
        for i in 0..items.length() {
            let item: web_sys::Element = items.get(i).unwrap().dyn_into().unwrap();
            assert_eq!(
                computed(&item, "background-color"),
                "rgba(0, 0, 0, 0)",
                "{state:?}"
            );
            assert_eq!(computed(&item, "box-shadow"), "none", "{state:?}");
            let label = item.query_selector(":scope > span").unwrap().unwrap();
            assert_eq!(computed(&label, "opacity"), "0.7", "{state:?}");
        }
        cleanup(&mount);
    }
    style.remove();
}

#[wasm_bindgen_test]
async fn the_overflow_stop_item_wears_the_error_colour() {
    let style = app_stylesheets();
    let mount = mount_overflow(ShareButtonState::Active).await;
    let probe = gloo_utils::document().create_element("span").unwrap();
    probe
        .set_attribute("style", "color: var(--on-dark-error-text)")
        .unwrap();
    mount.append_child(&probe).unwrap();
    let colour = |el: &web_sys::Element| computed(el, "color");
    let stop = mount
        .query_selector("[data-testid='overflow-share-stop']")
        .unwrap()
        .unwrap();
    let replace = mount
        .query_selector("[data-testid='overflow-share-replace']")
        .unwrap()
        .unwrap();
    assert_ne!(
        colour(&replace),
        colour(&probe),
        "premise: only stop is red"
    );
    assert_eq!(colour(&stop), colour(&probe));
    style.remove();
    cleanup(&mount);
}

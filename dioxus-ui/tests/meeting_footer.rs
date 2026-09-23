// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0

// Issue 2791: the in-call meeting footer line and its "Meeting info" dialog.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use support::{cleanup, create_mount_point, render_into, wait_for_selector, yield_now};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_test::*;

use dioxus::prelude::*;
use dioxus_ui::components::meeting_footer::{MeetingFooter, MeetingInfoDialog};
use dioxus_ui::components::meeting_format::format_datetime_zoned;
use dioxus_ui::context::MeetingTime;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const MEETING_ID: &str = "standup-2791";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const TRIGGER: &str = "#meeting-footer-trigger";
const DIALOG: &str = "#meeting-info-dialog";
const BACKDROP: &str = "[data-testid='meeting-info-dialog-backdrop']";
const CLOSE: &str = "[data-testid='meeting-info-dialog-close']";
const COPY: &str = "[data-testid='meeting-info-copy-link']";
const OPEN_STATE: &str = "[data-testid='open-state']";

fn provide_meeting_time() {
    let meeting_time = use_signal(MeetingTime::default);
    use_context_provider(|| meeting_time);
}

#[component]
fn Meeting(initially_open: bool, is_active: bool, show_git: bool) -> Element {
    provide_meeting_time();
    let open = use_signal(|| initially_open);
    rsx! {
        span { "data-testid": "open-state", "{open}" }
        MeetingFooter {
            open,
            meeting_id: MEETING_ID.to_string(),
            participant_count: 3,
            is_active,
        }
        MeetingInfoDialog {
            open,
            meeting_id: MEETING_ID.to_string(),
            meeting_link: format!("https://example.test/meeting/{MEETING_ID}"),
            participant_count: 3,
            is_active,
            show_git,
        }
    }
}

fn live_closed() -> Element {
    rsx! { Meeting { initially_open: false, is_active: true, show_git: false } }
}

fn live_open() -> Element {
    rsx! { Meeting { initially_open: true, is_active: true, show_git: false } }
}

fn live_open_with_git() -> Element {
    rsx! { Meeting { initially_open: true, is_active: true, show_git: true } }
}

fn ended_open() -> Element {
    rsx! { Meeting { initially_open: true, is_active: false, show_git: false } }
}

const TEST_NODE_MARK: &str = "data-meeting-footer-test";

fn remove_marked_nodes() {
    let marked = gloo_utils::document()
        .query_selector_all(&format!("[{TEST_NODE_MARK}]"))
        .unwrap();
    for i in 0..marked.length() {
        if let Some(el) = marked
            .item(i)
            .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
        {
            el.remove();
        }
    }
}

/// Every test starts here, so a failed test's leftovers (its mount, a stylesheet,
/// a clipboard stub) never leak duplicate ids into the next test.
fn fresh_mount() -> web_sys::Element {
    remove_marked_nodes();
    restore_clipboard();
    let root = create_mount_point();
    root.set_attribute(TEST_NODE_MARK, "").unwrap();
    root
}

fn install_stylesheets(extra: &str) {
    let doc = gloo_utils::document();
    let style = doc.create_element("style").unwrap();
    style.set_attribute(TEST_NODE_MARK, "").unwrap();
    style.set_text_content(Some(&format!(
        "{}{}{extra}",
        include_str!("../static/style.css"),
        include_str!("../static/global.css"),
    )));
    doc.head().unwrap().append_child(&style).unwrap();
}

fn computed(el: &web_sys::Element, property: &str) -> String {
    web_sys::window()
        .unwrap()
        .get_computed_style(el)
        .unwrap()
        .unwrap()
        .get_property_value(property)
        .unwrap()
}

fn find(mount: &web_sys::Element, selector: &str) -> Option<web_sys::Element> {
    mount.query_selector(selector).unwrap()
}

fn text(mount: &web_sys::Element, selector: &str) -> String {
    find(mount, selector)
        .unwrap_or_else(|| panic!("{selector} should be rendered"))
        .text_content()
        .unwrap_or_default()
}

fn html(mount: &web_sys::Element, selector: &str) -> web_sys::HtmlElement {
    find(mount, selector)
        .unwrap_or_else(|| panic!("{selector} should be rendered"))
        .dyn_into()
        .unwrap()
}

fn active_element() -> Option<web_sys::Element> {
    gloo_utils::document().active_element()
}

fn active_is(mount: &web_sys::Element, selector: &str) -> bool {
    match (active_element(), find(mount, selector)) {
        (Some(active), Some(el)) => {
            let node: &web_sys::Node = &el;
            active.is_same_node(Some(node))
        }
        _ => false,
    }
}

fn keydown(target: &web_sys::Element, key: &str, shift: bool) {
    let dispatch = js_sys::Function::new_with_args(
        "el, key, shift",
        "el.dispatchEvent(new KeyboardEvent('keydown', \
         { key: key, shiftKey: shift, bubbles: true, cancelable: true }));",
    );
    dispatch
        .call3(
            &JsValue::NULL,
            target,
            &JsValue::from_str(key),
            &JsValue::from_bool(shift),
        )
        .unwrap();
}

fn mouse(target: &web_sys::Element, kind: &str) {
    let dispatch = js_sys::Function::new_with_args(
        "el, kind",
        "el.dispatchEvent(new MouseEvent(kind, { bubbles: true, cancelable: true }));",
    );
    dispatch
        .call2(&JsValue::NULL, target, &JsValue::from_str(kind))
        .unwrap();
}

#[wasm_bindgen_test]
async fn footer_shows_the_meeting_id_participants_and_app_version() {
    let mount = fresh_mount();
    render_into(&mount, live_closed);
    yield_now().await;

    assert!(find(&mount, "footer.meeting-footer").is_some());
    assert!(text(&mount, "[data-testid='meeting-footer-meeting-id']").contains(MEETING_ID));
    assert_eq!(
        text(&mount, "[data-testid='meeting-footer-version']"),
        format!("v{VERSION}")
    );
    assert!(text(&mount, "[data-testid='meeting-footer-participants']").contains("3 participants"));
    assert!(find(&mount, "[data-testid='meeting-footer-timer']").is_some());
    assert!(find(&mount, "[data-testid='meeting-footer-ended']").is_none());
    assert!(
        find(&mount, DIALOG).is_none(),
        "the dialog stays closed until the footer is clicked"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn trigger_label_is_static_and_the_ticking_line_is_hidden_from_assistive_tech() {
    let mount = fresh_mount();
    render_into(&mount, live_closed);
    yield_now().await;

    let label = html(&mount, TRIGGER).get_attribute("aria-label").unwrap();
    assert!(label.starts_with("Meeting info"), "{label}");
    assert!(label.contains(MEETING_ID), "{label}");
    assert!(label.contains(VERSION), "{label}");
    assert_eq!(
        html(&mount, TRIGGER)
            .get_attribute("aria-haspopup")
            .as_deref(),
        Some("dialog")
    );
    assert_eq!(
        html(&mount, ".meeting-footer-trigger > .meeting-footer-content")
            .get_attribute("aria-hidden")
            .as_deref(),
        Some("true")
    );
    assert!(find(&mount, ".meeting-footer [aria-live]").is_none());

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn clicking_the_footer_opens_the_dialog_and_focuses_it() {
    let mount = fresh_mount();
    render_into(&mount, live_closed);
    yield_now().await;

    html(&mount, TRIGGER).click();
    assert!(wait_for_selector(&mount, DIALOG, 1_000).await);
    yield_now().await;

    assert_eq!(text(&mount, OPEN_STATE), "true");
    let dialog = find(&mount, DIALOG).unwrap();
    assert_eq!(dialog.get_attribute("role").as_deref(), Some("dialog"));
    assert_eq!(dialog.get_attribute("aria-modal").as_deref(), Some("true"));
    assert!(active_is(&mount, DIALOG), "the card takes focus on open");
    assert!(text(&mount, "[data-testid='meeting-info-row-meeting-id']").contains(MEETING_ID));
    assert!(text(&mount, "[data-testid='meeting-info-row-link']")
        .contains(&format!("https://example.test/meeting/{MEETING_ID}")));
    assert!(text(&mount, "[data-testid='meeting-info-row-version']").contains(VERSION));

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_dialog_is_not_rendered_once_the_meeting_has_ended() {
    let mount = fresh_mount();
    render_into(&mount, ended_open);
    yield_now().await;

    assert_eq!(
        text(&mount, OPEN_STATE),
        "true",
        "premise: open is still set"
    );
    assert!(find(&mount, DIALOG).is_none());
    assert!(find(&mount, BACKDROP).is_none());
    assert!(find(&mount, "[data-testid='meeting-footer-ended']").is_some());
    assert!(find(&mount, "[data-testid='meeting-footer-timer']").is_none());

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn commit_and_branch_rows_are_hidden_without_show_git() {
    let mount = fresh_mount();
    render_into(&mount, live_open);
    yield_now().await;

    assert!(
        find(&mount, "[data-testid='meeting-info-row-built']").is_some(),
        "premise: the App section rendered"
    );
    assert!(find(&mount, "[data-testid='meeting-info-row-commit']").is_none());
    assert!(find(&mount, "[data-testid='meeting-info-row-branch']").is_none());

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn commit_and_branch_rows_are_shown_with_show_git() {
    let mount = fresh_mount();
    render_into(&mount, live_open_with_git);
    yield_now().await;

    assert!(find(&mount, "[data-testid='meeting-info-row-commit']").is_some());
    assert!(find(&mount, "[data-testid='meeting-info-row-branch']").is_some());

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn escape_closes_the_dialog_and_returns_focus_to_the_trigger() {
    let mount = fresh_mount();
    render_into(&mount, live_open);
    yield_now().await;

    let dialog = find(&mount, DIALOG).expect("premise: the dialog is open");
    keydown(&dialog, "Escape", false);
    yield_now().await;

    assert_eq!(text(&mount, OPEN_STATE), "false");
    assert!(find(&mount, DIALOG).is_none());
    assert!(
        active_is(&mount, TRIGGER),
        "Escape must return focus to the footer trigger, got {:?}",
        active_element().map(|el| el.id())
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_close_button_returns_focus_to_the_trigger() {
    let mount = fresh_mount();
    render_into(&mount, live_open);
    yield_now().await;

    html(&mount, CLOSE).click();
    yield_now().await;

    assert_eq!(text(&mount, OPEN_STATE), "false");
    assert!(find(&mount, DIALOG).is_none());
    assert!(
        active_is(&mount, TRIGGER),
        "Close must return focus to the footer trigger, got {:?}",
        active_element().map(|el| el.id())
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_press_that_starts_inside_the_card_does_not_close_the_dialog() {
    let mount = fresh_mount();
    render_into(&mount, live_open);
    yield_now().await;

    let card = find(&mount, DIALOG).unwrap();
    let backdrop = find(&mount, BACKDROP).unwrap();
    mouse(&card, "mousedown");
    mouse(&backdrop, "click");
    yield_now().await;
    assert!(
        find(&mount, DIALOG).is_some(),
        "a selection drag from the card onto the backdrop must not close it"
    );

    mouse(&backdrop, "mousedown");
    mouse(&backdrop, "click");
    yield_now().await;
    assert!(
        find(&mount, DIALOG).is_none(),
        "a real backdrop click closes it"
    );
    assert!(active_is(&mount, TRIGGER));

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_backdrop_press_with_no_click_does_not_arm_a_later_drag_out_of_the_card() {
    let mount = fresh_mount();
    render_into(&mount, live_open);
    yield_now().await;

    let card = find(&mount, DIALOG).unwrap();
    let backdrop = find(&mount, BACKDROP).unwrap();
    mouse(&backdrop, "mousedown");
    yield_now().await;
    mouse(&card, "mousedown");
    mouse(&backdrop, "click");
    yield_now().await;
    assert!(
        find(&mount, DIALOG).is_some(),
        "a backdrop press with no click (right-click, release outside the window) \
         must not make a later drag out of the card close the dialog"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn tab_wraps_between_the_first_and_last_dialog_buttons() {
    let mount = fresh_mount();
    render_into(&mount, live_open);
    yield_now().await;

    html(&mount, COPY).focus().unwrap();
    keydown(&find(&mount, COPY).unwrap(), "Tab", false);
    assert!(
        active_is(&mount, CLOSE),
        "Tab on the last button wraps to Close"
    );

    keydown(&find(&mount, CLOSE).unwrap(), "Tab", true);
    assert!(
        active_is(&mount, COPY),
        "Shift+Tab on Close wraps to the last button"
    );

    html(&mount, DIALOG).focus().unwrap();
    keydown(&find(&mount, DIALOG).unwrap(), "Tab", true);
    assert!(
        active_is(&mount, COPY),
        "Shift+Tab on the card wraps to the last button"
    );

    cleanup(&mount);
}

const COPY_STATUS: &str = "#meeting-info-dialog [role='status']";

fn stub_clipboard(stub: &str) {
    js_sys::eval(&format!(
        "window.__copiedText = null; \
         Object.defineProperty(navigator, 'clipboard', {{ value: {stub}, configurable: true }});"
    ))
    .unwrap();
}

fn restore_clipboard() {
    js_sys::eval("delete navigator.clipboard; delete window.__copiedText;").unwrap();
}

#[wasm_bindgen_test]
async fn copy_link_writes_the_meeting_link_and_announces_it() {
    let mount = fresh_mount();
    stub_clipboard("{ writeText: (t) => { window.__copiedText = t; return Promise.resolve(); } }");
    render_into(&mount, live_open);
    yield_now().await;

    assert_eq!(text(&mount, COPY), "Copy link");
    html(&mount, COPY).click();
    yield_now().await;

    let copied = js_sys::eval("window.__copiedText").unwrap().as_string();
    restore_clipboard();
    assert_eq!(
        copied.as_deref(),
        Some(format!("https://example.test/meeting/{MEETING_ID}").as_str())
    );
    assert_eq!(text(&mount, COPY), "Copied");
    assert_eq!(text(&mount, COPY_STATUS), "Meeting link copied");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn copy_link_without_a_clipboard_shows_the_failure_on_the_button_and_says_to_select_the_text()
{
    let mount = fresh_mount();
    stub_clipboard("undefined");
    render_into(&mount, live_open);
    yield_now().await;

    html(&mount, COPY).click();
    yield_now().await;
    restore_clipboard();

    assert_eq!(text(&mount, COPY), "Copy failed");
    assert!(
        text(&mount, COPY_STATUS).contains("select the link text"),
        "got {:?}",
        text(&mount, COPY_STATUS)
    );
    assert!(
        find(&mount, DIALOG).is_some(),
        "a failed copy leaves the dialog open"
    );

    cleanup(&mount);
}

const STARTED_VALUE: &str = "[data-testid='meeting-info-row-started'] .about-modal-value";
const MEETING_START_MS: f64 = 1_790_000_000_000.0;

#[component]
fn MeetingStartsWhileTheDialogIsOpen() -> Element {
    let mut meeting_time = use_signal(MeetingTime::default);
    use_context_provider(|| meeting_time);
    let open = use_signal(|| true);
    rsx! {
        button {
            "data-testid": "start-meeting",
            onclick: move |_| meeting_time.write().meeting_start_time = Some(MEETING_START_MS),
            "start"
        }
        MeetingInfoDialog {
            open,
            meeting_id: MEETING_ID.to_string(),
            meeting_link: format!("https://example.test/meeting/{MEETING_ID}"),
            participant_count: 3,
            is_active: true,
            show_git: false,
        }
    }
}

fn meeting_starts_while_open() -> Element {
    rsx! { MeetingStartsWhileTheDialogIsOpen {} }
}

#[wasm_bindgen_test]
async fn started_follows_a_meeting_start_that_arrives_while_the_dialog_is_open() {
    let mount = fresh_mount();
    render_into(&mount, meeting_starts_while_open);
    yield_now().await;
    assert_eq!(
        text(&mount, STARTED_VALUE),
        "\u{2014}",
        "premise: no start time yet"
    );

    html(&mount, "[data-testid='start-meeting']").click();
    yield_now().await;

    assert_eq!(
        text(&mount, STARTED_VALUE),
        format_datetime_zoned(MEETING_START_MS as i64)
    );

    cleanup(&mount);
}

#[component]
fn FooterInATallMainContainer() -> Element {
    provide_meeting_time();
    let open = use_signal(|| false);
    rsx! {
        div { style: "--drawer-left-reserve: 40px; --drawer-right-reserve: 60px;",
            div {
                "data-testid": "tall-main-container",
                style: "position: relative; height: 200vh; overflow: hidden;",
                MeetingFooter {
                    open,
                    meeting_id: MEETING_ID.to_string(),
                    participant_count: 3,
                    is_active: true,
                }
            }
        }
    }
}

fn footer_in_a_tall_main_container() -> Element {
    rsx! { FooterInATallMainContainer {} }
}

#[wasm_bindgen_test]
async fn the_footer_sits_on_the_visible_viewport_bottom_when_its_container_is_taller() {
    let mount = fresh_mount();
    install_stylesheets("");
    render_into(&mount, footer_in_a_tall_main_container);
    yield_now().await;
    gloo_utils::window().scroll_to_with_x_and_y(0.0, 0.0);

    let root = gloo_utils::document().document_element().unwrap();
    let (vw, vh) = (
        f64::from(root.client_width()),
        f64::from(root.client_height()),
    );
    let container = html(&mount, "[data-testid='tall-main-container']").get_bounding_client_rect();
    assert!(
        container.bottom() > vh + 100.0,
        "premise: the container ends below the visible viewport ({} vs {vh})",
        container.bottom()
    );

    let footer = html(&mount, "footer.meeting-footer").get_bounding_client_rect();
    assert!(
        (footer.bottom() - vh).abs() < 1.0,
        "the footer must sit on the visible viewport bottom ({vh}px), not on its \
         container's bottom ({}px); got {}",
        container.bottom(),
        footer.bottom()
    );
    assert!(
        (footer.left() - 40.0).abs() < 1.0,
        "--drawer-left-reserve must still reach the footer, left is {}",
        footer.left()
    );
    assert!(
        (footer.right() - (vw - 60.0)).abs() < 1.0,
        "--drawer-right-reserve must still reach the footer, right is {} of {vw}",
        footer.right()
    );

    remove_marked_nodes();
}

#[component]
fn PinnedScreenTileWithZoomBar() -> Element {
    rsx! {
        div { class: "split-screen-tile grid-item-pinned",
            div {
                class: "canvas-container video-on",
                "data-testid": "screen-canvas",
                div { class: "ss-zoom-controls", "data-testid": "zoom-bar" }
            }
        }
        div {
            "data-testid": "dock-clearance-line",
            style: "position: fixed; left: 0; width: 1px; height: 1px; bottom: var(--controls-dock-clearance);",
        }
        div {
            "data-testid": "footer-top-line",
            style: "position: fixed; left: 0; width: 1px; height: 1px; bottom: var(--meeting-footer-h);",
        }
    }
}

fn pinned_screen_tile_with_zoom_bar() -> Element {
    rsx! { PinnedScreenTileWithZoomBar {} }
}

#[wasm_bindgen_test]
async fn the_screen_share_zoom_bar_sits_on_the_dock_clearance_line_not_above_it() {
    let mount = fresh_mount();
    install_stylesheets("");
    render_into(&mount, pinned_screen_tile_with_zoom_bar);
    yield_now().await;

    let bottom_of = |selector: &str| html(&mount, selector).get_bounding_client_rect().bottom();
    let footer_top = bottom_of("[data-testid='footer-top-line']");
    assert!(
        (bottom_of("[data-testid='screen-canvas']") - footer_top).abs() < 1.0,
        "premise: the pinned screen tile already ends at the footer's top edge ({footer_top})"
    );
    let clearance_line = bottom_of("[data-testid='dock-clearance-line']");
    let zoom_bar = bottom_of("[data-testid='zoom-bar']");
    assert!(
        (zoom_bar - clearance_line).abs() < 1.0,
        "the zoom bar must sit on the dock clearance line ({clearance_line}px), \
         not the footer height above it; it sits at {zoom_bar}px"
    );

    remove_marked_nodes();
}

#[wasm_bindgen_test]
async fn dialog_values_wrap_inside_a_narrow_card_instead_of_being_clipped() {
    let mount = fresh_mount();
    install_stylesheets("#meeting-info-dialog { width: 260px; }");
    render_into(&mount, live_open_with_git);
    yield_now().await;

    let cells = mount
        .query_selector_all(".meeting-info-row .about-modal-value")
        .unwrap();
    assert!(
        cells.length() >= 8,
        "premise: every value row rendered, got {}",
        cells.length()
    );
    let text_overrun = js_sys::Function::new_with_args(
        "el",
        "const r = document.createRange(); r.selectNodeContents(el); \
         return r.getBoundingClientRect().right - el.getBoundingClientRect().right;",
    );
    for i in 0..cells.length() {
        let cell = cells.item(i).unwrap();
        let overrun = text_overrun
            .call1(&JsValue::NULL, &cell)
            .unwrap()
            .as_f64()
            .unwrap();
        assert!(
            overrun <= 0.5,
            "{:?} runs {overrun}px past its cell, where the table clips it",
            cell.text_content()
        );
    }

    remove_marked_nodes();
}

#[wasm_bindgen_test]
async fn the_dialog_card_does_not_blur_the_call_a_second_time() {
    let mount = fresh_mount();
    install_stylesheets("");
    render_into(&mount, live_open);
    yield_now().await;

    assert_ne!(
        computed(&html(&mount, BACKDROP), "backdrop-filter"),
        "none",
        "premise: the scrim still blurs the call once"
    );
    assert_eq!(computed(&html(&mount, DIALOG), "backdrop-filter"), "none");

    remove_marked_nodes();
}

#[component]
fn HourLongMeetingInANarrowFooter() -> Element {
    let meeting_time = use_signal(|| MeetingTime {
        call_start_time: None,
        meeting_start_time: Some(js_sys::Date::now() - 2.0 * 3_600_000.0),
    });
    use_context_provider(|| meeting_time);
    let open = use_signal(|| false);
    rsx! {
        div { style: "--drawer-left-reserve: 0px; --drawer-right-reserve: calc(100% - 240px);",
            MeetingFooter {
                open,
                meeting_id: MEETING_ID.to_string(),
                participant_count: 12,
                is_active: true,
            }
        }
    }
}

fn hour_long_meeting_in_a_narrow_footer() -> Element {
    rsx! { HourLongMeetingInANarrowFooter {} }
}

#[wasm_bindgen_test]
async fn an_hour_long_meeting_line_is_clipped_in_a_narrow_footer_not_painted_over_the_version() {
    let mount = fresh_mount();
    install_stylesheets("");
    render_into(&mount, hour_long_meeting_in_a_narrow_footer);
    yield_now().await;
    for _ in 0..50 {
        if text(&mount, "[data-testid='meeting-footer-timer']")
            .matches(':')
            .count()
            == 2
        {
            break;
        }
        gloo_timers::future::TimeoutFuture::new(20).await;
    }

    let footer_w = html(&mount, "footer.meeting-footer")
        .get_bounding_client_rect()
        .width();
    assert!(
        (footer_w - 240.0).abs() < 1.0,
        "premise: a 240px footer, got {footer_w}"
    );
    let viewport_w = f64::from(
        gloo_utils::document()
            .document_element()
            .unwrap()
            .client_width(),
    );
    assert!(
        viewport_w >= 560.0,
        "premise: a viewport wide enough ({viewport_w}px) that a footer-width tier \
         and a viewport-width tier disagree"
    );
    assert_eq!(
        computed(&html(&mount, ".meeting-footer-count-text"), "display"),
        "none",
        "the width tiers must query the footer's own width, not the viewport's"
    );
    let timer = text(&mount, "[data-testid='meeting-footer-timer']");
    assert_eq!(
        timer.matches(':').count(),
        2,
        "premise: the timer shows HH:MM:SS, got {timer:?}"
    );
    let group = html(&mount, ".meeting-footer-group--meeting");
    assert!(
        group.scroll_width() > group.client_width(),
        "premise: the meeting group's content ({}px) is wider than its box ({}px)",
        group.scroll_width(),
        group.client_width()
    );
    let overflow_x = computed(&group, "overflow-x");
    assert!(
        overflow_x == "clip" || overflow_x == "hidden",
        "the meeting group must clip what does not fit instead of painting it over \
         the version group, got overflow-x: {overflow_x}"
    );
    let room = html(&mount, "[data-testid='meeting-footer-meeting-id']");
    assert_eq!(computed(&room, "text-overflow"), "ellipsis");
    assert!(
        room.client_width() > 0,
        "the meeting ID keeps its ellipsis slot"
    );

    remove_marked_nodes();
}

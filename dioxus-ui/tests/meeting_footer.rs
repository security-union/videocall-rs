// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0

// Issue 2791: the in-call meeting footer line and its "Meeting info" dialog.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use support::{
    cleanup, create_mount_point, render_into, restore_fetch, wait_for_selector, yield_now,
};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_test::*;

use dioxus::prelude::*;
use dioxus_ui::components::changelog::reset_changelog_cache_for_test;
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
/// a clipboard or fetch stub, a cached change log) never leak into the next test.
fn fresh_mount() -> web_sys::Element {
    remove_marked_nodes();
    restore_clipboard();
    restore_fetch();
    restore_intl_date_format();
    reset_changelog_cache_for_test();
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

    html(&mount, WHATS_NEW).focus().unwrap();
    keydown(&find(&mount, WHATS_NEW).unwrap(), "Tab", false);
    assert!(
        active_is(&mount, CLOSE),
        "Tab on the last button wraps to Close"
    );

    keydown(&find(&mount, CLOSE).unwrap(), "Tab", true);
    assert!(
        active_is(&mount, WHATS_NEW),
        "Shift+Tab on Close wraps to the last button"
    );

    html(&mount, DIALOG).focus().unwrap();
    keydown(&find(&mount, DIALOG).unwrap(), "Tab", true);
    assert!(
        active_is(&mount, WHATS_NEW),
        "Shift+Tab on the card wraps to the last button"
    );

    html(&mount, COPY).focus().unwrap();
    keydown(&find(&mount, COPY).unwrap(), "Tab", false);
    assert!(
        active_is(&mount, COPY),
        "Tab between inner buttons is left to the browser"
    );

    cleanup(&mount);
}

const WHATS_NEW: &str = "[data-testid='changelog-toggle']";
const CHANGELOG_PANEL: &str = "[data-testid='changelog-panel']";
const CHANGELOG_BUILD: &str = "[data-testid='changelog-build']";
const CHANGELOG_STATUS: &str = "[data-testid='changelog-status']";
const SHOW_OLDER: &str = "[data-testid='changelog-show-older']";
const THIS_BUILD: &str = "[data-testid='changelog-this-build']";
const FIXTURE_BUILDS: u32 = 16;
const RUNNING_BUILD: u32 = 15;

/// Builds 1..=16 in a scrambled file order; build `k` was built on September `k`
/// and build 15 carries the running commit.
fn changelog_fixture() -> String {
    let builds: Vec<String> = (1..=FIXTURE_BUILDS)
        .map(|n| (n * 7) % (FIXTURE_BUILDS + 1))
        .map(|k| {
            let commit = if k == RUNNING_BUILD {
                env!("GIT_SHA").to_string()
            } else {
                format!("{k:08x}")
            };
            format!(
                r#"{{"built":"2026-09-{k:02}T12:00:37Z","version":"1.1.{k}","commit":"{commit}","changes":["Change {k}a","Change {k}b"]}}"#
            )
        })
        .collect();
    format!(r#"{{"builds":[{}]}}"#, builds.join(","))
}

fn stub_changelog(body: &str, content_type: &str) {
    let window = gloo_utils::window();
    js_sys::Reflect::set(&window, &"__changelogBody".into(), &body.into()).unwrap();
    js_sys::Reflect::set(&window, &"__changelogType".into(), &content_type.into()).unwrap();
    js_sys::eval(
        "window.__changelogFetches = 0; \
         window.__original_fetch = window.__original_fetch || window.fetch; \
         window.fetch = function (input) { \
           var url = typeof input === 'string' ? input : input.url; \
           if (!url.endsWith('/assets/changelog.json')) { return window.__original_fetch(input); } \
           window.__changelogFetches += 1; \
           var resp = new Response(window.__changelogBody, \
             { status: 200, headers: { 'Content-Type': window.__changelogType } }); \
           Object.defineProperty(resp, 'url', { value: url }); \
           return Promise.resolve(resp); \
         };",
    )
    .unwrap();
}

fn changelog_fetches() -> f64 {
    js_sys::eval("window.__changelogFetches")
        .unwrap()
        .as_f64()
        .unwrap_or(0.0)
}

fn all_text(mount: &web_sys::Element, selector: &str) -> Vec<String> {
    let nodes = mount.query_selector_all(selector).unwrap();
    (0..nodes.length())
        .filter_map(|i| nodes.item(i))
        .map(|n| n.text_content().unwrap_or_default())
        .collect()
}

fn count(mount: &web_sys::Element, selector: &str) -> u32 {
    mount.query_selector_all(selector).unwrap().length()
}

async fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if done() {
            return true;
        }
        gloo_timers::future::TimeoutFuture::new(20).await;
    }
    done()
}

async fn open_whats_new(mount: &web_sys::Element) {
    html(mount, WHATS_NEW).click();
    assert!(
        wait_for_selector(mount, CHANGELOG_BUILD, 2_000).await,
        "the change log should render its builds"
    );
}

fn build_titles(mount: &web_sys::Element) -> Vec<String> {
    all_text(mount, ".changelog-build-version")
}

#[wasm_bindgen_test]
async fn whats_new_fetches_on_first_expand_and_lists_the_newest_three_builds() {
    let mount = fresh_mount();
    stub_changelog(&changelog_fixture(), "application/json");
    render_into(&mount, live_open);
    yield_now().await;

    let toggle = html(&mount, WHATS_NEW);
    assert_eq!(
        toggle.get_attribute("aria-expanded").as_deref(),
        Some("false")
    );
    assert!(find(&mount, CHANGELOG_PANEL).is_none());
    assert_eq!(
        changelog_fetches(),
        0.0,
        "nothing is fetched until expanded"
    );

    open_whats_new(&mount).await;

    assert_eq!(
        toggle.get_attribute("aria-expanded").as_deref(),
        Some("true")
    );
    let panel = find(
        &mount,
        "section[aria-labelledby='meeting-info-section-app'] [data-testid='changelog-panel']",
    )
    .expect("the log opens inline in the App section");
    assert_eq!(toggle.get_attribute("aria-controls"), Some(panel.id()));
    assert_eq!(build_titles(&mount), ["v1.1.16", "v1.1.15", "v1.1.14"]);
    assert_eq!(
        all_text(
            &mount,
            "[data-testid='changelog-build']:has(#meeting-info-changelog-build-0) [data-testid='changelog-change']"
        ),
        ["Change 16a", "Change 16b"]
    );
    assert_eq!(text(&mount, SHOW_OLDER), "Show 10 older builds");
    assert_eq!(changelog_fetches(), 1.0);

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn show_older_reveals_ten_then_the_rest_and_focuses_the_first_revealed_build() {
    let mount = fresh_mount();
    stub_changelog(&changelog_fixture(), "application/json");
    render_into(&mount, live_open);
    yield_now().await;
    open_whats_new(&mount).await;

    html(&mount, SHOW_OLDER).click();
    assert!(wait_until(|| count(&mount, CHANGELOG_BUILD) == 13).await);
    assert!(
        wait_until(
            || active_element().is_some_and(|el| el.id() == "meeting-info-changelog-build-3")
        )
        .await,
        "focus moves to the first revealed build, got {:?}",
        active_element().map(|el| el.id())
    );
    assert!(text(&mount, "#meeting-info-changelog-build-3").contains("v1.1.13"));
    assert_eq!(text(&mount, SHOW_OLDER), "Show 3 older builds");

    html(&mount, SHOW_OLDER).click();
    assert!(wait_until(|| count(&mount, CHANGELOG_BUILD) == FIXTURE_BUILDS).await);
    assert!(find(&mount, SHOW_OLDER).is_none(), "nothing older is left");
    assert!(
        wait_until(
            || active_element().is_some_and(|el| el.id() == "meeting-info-changelog-build-13")
        )
        .await,
        "focus is not dropped when the button goes away, got {:?}",
        active_element().map(|el| el.id())
    );
    assert_eq!(
        build_titles(&mount).last().map(String::as_str),
        Some("v1.1.1")
    );

    keydown(&active_element().unwrap(), "Tab", false);
    assert!(
        active_is(&mount, CLOSE),
        "Tab past the last button still wraps inside the dialog"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn this_build_marks_only_the_running_commit() {
    let mount = fresh_mount();
    stub_changelog(&changelog_fixture(), "application/json");
    render_into(&mount, live_open);
    yield_now().await;
    open_whats_new(&mount).await;

    assert_eq!(
        count(&mount, THIS_BUILD),
        1,
        "exactly one build carries GIT_SHA {:?}",
        env!("GIT_SHA")
    );
    assert!(find(
        &mount,
        "#meeting-info-changelog-build-1 [data-testid='changelog-this-build']"
    )
    .is_some());

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn an_html_page_served_in_place_of_the_log_shows_it_as_unavailable() {
    let mount = fresh_mount();
    stub_changelog(
        "<!DOCTYPE html><html><head><title>videocall</title></head><body></body></html>",
        "text/html",
    );
    render_into(&mount, live_open);
    yield_now().await;

    html(&mount, WHATS_NEW).click();
    assert!(
        wait_until(|| find(&mount, CHANGELOG_STATUS)
            .and_then(|el| el.text_content())
            .as_deref()
            == Some("Change log unavailable"))
        .await,
        "got {:?}",
        find(&mount, CHANGELOG_STATUS).and_then(|el| el.text_content())
    );
    assert_eq!(changelog_fetches(), 1.0);
    assert_eq!(count(&mount, CHANGELOG_BUILD), 0);
    assert!(find(&mount, DIALOG).is_some());

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn an_empty_log_says_so() {
    let mount = fresh_mount();
    stub_changelog(r#"{"builds":[]}"#, "application/json");
    render_into(&mount, live_open);
    yield_now().await;

    html(&mount, WHATS_NEW).click();
    assert!(
        wait_until(|| find(&mount, CHANGELOG_STATUS)
            .and_then(|el| el.text_content())
            .as_deref()
            == Some("No changes recorded yet"))
        .await
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_log_is_fetched_once_per_page_session() {
    let mount = fresh_mount();
    stub_changelog(&changelog_fixture(), "application/json");
    render_into(&mount, live_open);
    yield_now().await;
    open_whats_new(&mount).await;

    html(&mount, WHATS_NEW).click();
    yield_now().await;
    assert!(find(&mount, CHANGELOG_PANEL).is_none(), "collapses");

    html(&mount, CLOSE).click();
    yield_now().await;
    html(&mount, TRIGGER).click();
    assert!(wait_for_selector(&mount, DIALOG, 1_000).await);
    yield_now().await;
    html(&mount, WHATS_NEW).click();
    yield_now().await;

    assert_eq!(
        count(&mount, CHANGELOG_BUILD),
        3,
        "shown straight from the cache"
    );
    assert_eq!(changelog_fetches(), 1.0);

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn escape_still_closes_the_dialog_with_the_log_expanded() {
    let mount = fresh_mount();
    stub_changelog(&changelog_fixture(), "application/json");
    render_into(&mount, live_open);
    yield_now().await;
    open_whats_new(&mount).await;

    keydown(&find(&mount, WHATS_NEW).unwrap(), "Escape", false);
    yield_now().await;

    assert!(find(&mount, DIALOG).is_none());
    assert!(active_is(&mount, TRIGGER));

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_expanded_log_scrolls_inside_a_card_that_fits_the_viewport() {
    let mount = fresh_mount();
    install_stylesheets("");
    stub_changelog(&changelog_fixture(), "application/json");
    render_into(&mount, live_open);
    yield_now().await;
    open_whats_new(&mount).await;
    html(&mount, SHOW_OLDER).click();
    assert!(wait_until(|| count(&mount, CHANGELOG_BUILD) == 13).await);

    let card = html(&mount, DIALOG);
    let viewport_h = f64::from(
        gloo_utils::document()
            .document_element()
            .unwrap()
            .client_height(),
    );
    assert!(
        card.scroll_height() > card.client_height(),
        "premise: the expanded log overflows the card ({} vs {})",
        card.scroll_height(),
        card.client_height()
    );
    assert!(card.get_bounding_client_rect().bottom() <= viewport_h + 0.5);
    assert_eq!(computed(&card, "overflow-y"), "auto");
    assert_eq!(computed(&card, "overscroll-behavior-y"), "contain");

    remove_marked_nodes();
}

#[wasm_bindgen_test]
async fn expanding_in_a_short_card_scrolls_the_log_into_view() {
    let mount = fresh_mount();
    install_stylesheets("#meeting-info-dialog { max-height: 260px; }");
    stub_changelog(&changelog_fixture(), "application/json");
    render_into(&mount, live_open);
    yield_now().await;

    let card = html(&mount, DIALOG);
    let toggle = html(&mount, WHATS_NEW);
    let card_bottom = card.get_bounding_client_rect().bottom();
    assert!(
        toggle.get_bounding_client_rect().top() >= card_bottom,
        "premise: What's new starts below the visible part of the card"
    );

    open_whats_new(&mount).await;
    let in_view = || {
        let card = card.get_bounding_client_rect();
        let toggle = toggle.get_bounding_client_rect();
        let first_build = html(&mount, CHANGELOG_BUILD).get_bounding_client_rect();
        toggle.top() >= card.top() - 1.0
            && toggle.bottom() <= card.bottom()
            && first_build.top() < card.bottom()
    };
    assert!(
        wait_until(in_view).await,
        "the toggle and the first build must scroll into the card (scrollTop {})",
        card.scroll_top()
    );

    let (mut last, mut steady) = (-1, 0);
    while steady < 3 {
        gloo_timers::future::TimeoutFuture::new(50).await;
        let top = card.scroll_top();
        steady = if top == last { steady + 1 } else { 0 };
        last = top;
    }
    let gap = toggle.get_bounding_client_rect().top() - card.get_bounding_client_rect().top();
    assert!(
        (8.0..=40.0).contains(&gap),
        "the settled toggle keeps a margin below the card's top edge, got {gap}px"
    );

    remove_marked_nodes();
}

#[wasm_bindgen_test]
async fn build_headings_lead_with_the_date_to_the_minute_and_keep_the_version_secondary() {
    let mount = fresh_mount();
    install_stylesheets("");
    stub_changelog(&changelog_fixture(), "application/json");
    render_into(&mount, live_open);
    yield_now().await;
    open_whats_new(&mount).await;

    let heading = find(&mount, "#meeting-info-changelog-build-0").unwrap();
    let lead = heading.first_element_child().unwrap();
    assert_eq!(lead.class_name(), "changelog-build-date");
    let date = lead.text_content().unwrap_or_default();
    assert!(date.contains("2026"), "{date}");
    assert!(!date.contains("37"), "the date stops at the minute: {date}");
    let version = find(
        &mount,
        "#meeting-info-changelog-build-0 .changelog-build-version",
    )
    .unwrap();
    assert_eq!(version.text_content().as_deref(), Some("v1.1.16"));
    assert_eq!(computed(&lead, "font-weight"), "600");
    assert_eq!(computed(&version, "font-weight"), "400");

    remove_marked_nodes();
}

fn count_intl_date_formats() {
    js_sys::eval(
        "window.__dtfCount = 0; \
         window.__origDTF = window.__origDTF || Intl.DateTimeFormat; \
         Intl.DateTimeFormat = new Proxy(window.__origDTF, { construct(target, args) { \
           window.__dtfCount += 1; return new target(...args); } });",
    )
    .unwrap();
}

fn intl_date_formats() -> f64 {
    js_sys::eval("window.__dtfCount")
        .unwrap()
        .as_f64()
        .unwrap_or(0.0)
}

fn restore_intl_date_format() {
    js_sys::eval(
        "if (window.__origDTF) { Intl.DateTimeFormat = window.__origDTF; delete window.__origDTF; }",
    )
    .unwrap();
}

#[wasm_bindgen_test]
async fn build_dates_are_formatted_when_the_log_loads_not_on_every_render() {
    let mount = fresh_mount();
    stub_changelog(&changelog_fixture(), "application/json");
    render_into(&mount, live_open);
    yield_now().await;
    count_intl_date_formats();
    open_whats_new(&mount).await;
    let at_load = intl_date_formats();
    assert_eq!(
        at_load, 1.0,
        "one Intl.DateTimeFormat formats every build's date as the log loads"
    );

    html(&mount, SHOW_OLDER).click();
    assert!(wait_until(|| count(&mount, CHANGELOG_BUILD) == 13).await);
    html(&mount, SHOW_OLDER).click();
    assert!(wait_until(|| count(&mount, CHANGELOG_BUILD) == FIXTURE_BUILDS).await);
    yield_now().await;
    let after = intl_date_formats();
    restore_intl_date_format();
    assert_eq!(
        after, at_load,
        "revealing builds must not format dates again"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn try_again_refetches_an_unavailable_log() {
    let mount = fresh_mount();
    stub_changelog("<!DOCTYPE html><html></html>", "text/html");
    render_into(&mount, live_open);
    yield_now().await;
    html(&mount, WHATS_NEW).click();
    assert!(
        wait_until(|| find(&mount, "[data-testid='changelog-retry']").is_some()).await,
        "an unavailable log offers Try again"
    );
    let status = find(&mount, CHANGELOG_STATUS).unwrap();
    assert_eq!(
        status.text_content().as_deref(),
        Some("Change log unavailable")
    );
    assert!(status.class_list().contains("about-modal-status--error"));
    assert_eq!(text(&mount, "[data-testid='changelog-retry']"), "Try again");

    stub_changelog(&changelog_fixture(), "application/json");
    html(&mount, "[data-testid='changelog-retry']").click();
    assert!(wait_for_selector(&mount, CHANGELOG_BUILD, 2_000).await);
    assert_eq!(changelog_fetches(), 1.0);
    assert!(find(&mount, "[data-testid='changelog-retry']").is_none());
    assert!(
        wait_until(|| active_is(&mount, WHATS_NEW)).await,
        "focus lands on What's new instead of being dropped with the Try again button"
    );
    let listed = find(&mount, CHANGELOG_STATUS).expect("the status line stays mounted");
    let node: &web_sys::Node = &status;
    assert!(
        listed.is_same_node(Some(node)),
        "one status node for the panel's life"
    );
    assert_eq!(listed.text_content().as_deref(), Some(""));
    assert!(listed.class_list().contains("visually-hidden"));

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn collapsing_with_focus_in_the_log_moves_focus_to_the_toggle_so_escape_still_closes() {
    let mount = fresh_mount();
    stub_changelog(&changelog_fixture(), "application/json");
    render_into(&mount, live_open);
    yield_now().await;
    open_whats_new(&mount).await;

    html(&mount, "#meeting-info-changelog-build-0")
        .focus()
        .unwrap();
    assert!(
        active_is(&mount, "#meeting-info-changelog-build-0"),
        "premise"
    );
    html(&mount, WHATS_NEW).click();
    assert!(wait_until(|| find(&mount, CHANGELOG_PANEL).is_none()).await);
    assert!(
        wait_until(|| active_is(&mount, WHATS_NEW)).await,
        "focus moves to What's new instead of dropping to the page, got {:?}",
        active_element().map(|el| el.tag_name())
    );

    keydown(&active_element().unwrap(), "Escape", false);
    yield_now().await;
    assert!(find(&mount, DIALOG).is_none(), "Escape still closes");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_failed_retry_updates_the_same_status_line_so_it_is_announced() {
    let mount = fresh_mount();
    stub_changelog("<!DOCTYPE html><html></html>", "text/html");
    render_into(&mount, live_open);
    yield_now().await;
    html(&mount, WHATS_NEW).click();
    assert!(wait_until(|| find(&mount, "[data-testid='changelog-retry']").is_some()).await);
    let status = find(&mount, CHANGELOG_STATUS).unwrap();

    html(&mount, "[data-testid='changelog-retry']").click();
    assert!(
        wait_until(|| changelog_fetches() == 2.0
            && find(&mount, "[data-testid='changelog-retry']").is_some())
        .await,
        "premise: the retry ran and failed again"
    );

    let after = find(&mount, CHANGELOG_STATUS).unwrap();
    let node: &web_sys::Node = &status;
    assert!(
        after.is_same_node(Some(node)),
        "the role=status line must be updated in place, not replaced, so the failure is announced"
    );
    assert_eq!(
        after.text_content().as_deref(),
        Some("Change log unavailable")
    );

    cleanup(&mount);
}

/// Two pending sections sharing a line, the first carrying the running commit,
/// between dated builds 1-4.
fn pending_fixture() -> String {
    let dated = |k: u32| {
        format!(
            r#"{{"built":"2026-09-{k:02}T12:00:37Z","version":"1.1.{k}","commit":"{k:08x}","changes":["Change {k}a"]}}"#
        )
    };
    format!(
        r#"{{"builds":[{},{{"pending":true,"commit":"{}","prs":[2890],"changes":["Pending A","Shared"]}},{},{},{{"pending":true,"changes":["Pending B","Shared"]}},{}]}}"#,
        dated(1),
        env!("GIT_SHA"),
        dated(3),
        dated(4),
        dated(2)
    )
}

#[wasm_bindgen_test]
async fn pending_sections_merge_into_one_unreleased_section_shown_besides_three_builds() {
    let mount = fresh_mount();
    stub_changelog(&pending_fixture(), "application/json");
    render_into(&mount, live_open);
    yield_now().await;
    open_whats_new(&mount).await;

    assert_eq!(
        count(&mount, CHANGELOG_BUILD),
        4,
        "Unreleased plus the three newest builds"
    );
    assert_eq!(
        count(
            &mount,
            "[data-testid='changelog-build'][data-pending='true']"
        ),
        1,
        "every pending section renders as one"
    );
    assert_eq!(
        all_text(
            &mount,
            "[data-testid='changelog-build'][data-pending='true'] [data-testid='changelog-change']"
        ),
        ["Pending A", "Shared", "Pending B"],
        "changes in file order, the repeated line once"
    );
    let heading = "#meeting-info-changelog-unreleased";
    assert_eq!(text(&mount, heading), "Unreleased");
    assert!(find(&mount, &format!("{heading} .changelog-build-version")).is_none());
    assert_eq!(
        count(&mount, THIS_BUILD),
        0,
        "the pending section carries the running commit yet is never This build"
    );
    assert!(
        find(&mount, "#meeting-info-changelog-build-0")
            .and_then(|el| el.parent_element())
            .is_some_and(|section| !section.has_attribute("data-pending")),
        "dated builds carry no data-pending"
    );
    assert_eq!(build_titles(&mount), ["v1.1.4", "v1.1.3", "v1.1.2"]);
    assert_eq!(text(&mount, SHOW_OLDER), "Show 1 older build");

    cleanup(&mount);
}

/// Dated builds 1-6, where build 5 lists no changes and build 3 only a blank line.
fn empty_build_fixture() -> String {
    let builds: Vec<String> = (1..=6u32)
        .map(|k| {
            let changes = match k {
                5 => String::new(),
                3 => r#""  ""#.to_string(),
                _ => format!(r#""Change {k}""#),
            };
            format!(
                r#"{{"built":"2026-09-{k:02}T12:00:00Z","version":"1.1.{k}","commit":"{k:08x}","changes":[{changes}]}}"#
            )
        })
        .collect();
    format!(r#"{{"builds":[{}]}}"#, builds.join(","))
}

#[wasm_bindgen_test]
async fn builds_without_changes_are_skipped_and_not_counted() {
    let mount = fresh_mount();
    stub_changelog(&empty_build_fixture(), "application/json");
    render_into(&mount, live_open);
    yield_now().await;
    open_whats_new(&mount).await;

    assert_eq!(build_titles(&mount), ["v1.1.6", "v1.1.4", "v1.1.2"]);
    assert_eq!(text(&mount, SHOW_OLDER), "Show 1 older build");
    html(&mount, SHOW_OLDER).click();
    assert!(wait_until(|| count(&mount, CHANGELOG_BUILD) == 4).await);
    assert_eq!(
        build_titles(&mount),
        ["v1.1.6", "v1.1.4", "v1.1.2", "v1.1.1"]
    );
    assert!(find(&mount, SHOW_OLDER).is_none());
    assert_eq!(
        count(
            &mount,
            "[data-testid='changelog-build'] [data-testid='changelog-change']"
        ),
        4,
        "one line per listed build, no placeholder for the empty ones"
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
fn EnlargedShareWithZoomBar() -> Element {
    rsx! {
        div {
            class: "split-screen-tile share-tile",
            "data-share-mode": "enlarged",
            style: "position: fixed; left: 0; right: 0; top: 0; bottom: var(--meeting-footer-h); \
                    height: auto;",
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

fn enlarged_share_with_zoom_bar() -> Element {
    rsx! { EnlargedShareWithZoomBar {} }
}

#[wasm_bindgen_test]
async fn the_screen_share_zoom_bar_sits_on_the_dock_clearance_line_not_above_it() {
    let mount = fresh_mount();
    install_stylesheets("");
    render_into(&mount, enlarged_share_with_zoom_bar);
    yield_now().await;

    let bottom_of = |selector: &str| html(&mount, selector).get_bounding_client_rect().bottom();
    let footer_top = bottom_of("[data-testid='footer-top-line']");
    assert!(
        (bottom_of("[data-testid='screen-canvas']") - footer_top).abs() < 1.0,
        "premise: the enlarged screen tile already ends at the footer's top edge ({footer_top})"
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

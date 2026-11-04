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
use dioxus_ui::components::meeting_footer::{
    element_is_shown, focus_was_dropped, MeetingFooter, MeetingInfoDialog,
};
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
const PARTICIPANTS_ID: &str = "meeting-footer-participants-trigger";
const PARTICIPANTS: &str = "#meeting-footer-participants-trigger";
const PEER_LIST_TOGGLES: &str = "[data-testid='peer-list-toggles']";
const HELP: &str = "[data-testid='meeting-footer-help']";

fn provide_meeting_time() {
    let meeting_time = use_signal(MeetingTime::default);
    use_context_provider(|| meeting_time);
}

#[component]
fn Meeting(initially_open: bool, is_active: bool, show_git: bool) -> Element {
    provide_meeting_time();
    let open = use_signal(|| initially_open);
    let mut peer_list_open = use_signal(|| false);
    let mut toggles = use_signal(|| 0u32);
    let on_toggle_participants = use_callback(move |_: ()| {
        toggles += 1;
        peer_list_open.toggle();
    });
    rsx! {
        span { "data-testid": "open-state", "{open}" }
        span { "data-testid": "peer-list-toggles", "{toggles}" }
        MeetingFooter {
            open,
            meeting_id: MEETING_ID.to_string(),
            participant_count: 3,
            is_active,
            participants_open: peer_list_open(),
            on_toggle_participants,
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
        html(&mount, TRIGGER).text_content().as_deref(),
        Some(""),
        "the trigger is named by its static label alone"
    );
    let unhidden_text = js_sys::Function::new_with_args(
        "root, skip",
        "const w = document.createTreeWalker(root, NodeFilter.SHOW_TEXT); const out = []; \
         for (let n = w.nextNode(); n; n = w.nextNode()) { \
           const el = n.parentElement; \
           if (!n.textContent.trim() || el.closest('[aria-hidden=\"true\"]') || el.closest(skip)) continue; \
           out.push(n.textContent); } \
         return out.join('|');",
    );
    let leaked = unhidden_text
        .call2(
            &JsValue::NULL,
            &find(&mount, ".meeting-footer-content").unwrap(),
            &JsValue::from_str(PARTICIPANTS),
        )
        .unwrap()
        .as_string()
        .unwrap();
    assert_eq!(
        leaked, "",
        "every part of the line outside the participants button must be aria-hidden"
    );
    assert!(find(&mount, "[data-testid='meeting-footer-timer']")
        .unwrap()
        .closest("[aria-hidden='true']")
        .unwrap()
        .is_some());
    assert!(find(&mount, ".meeting-footer [aria-live]").is_none());

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_participants_button_sits_beside_the_trigger_and_nothing_focusable_is_aria_hidden() {
    let mount = fresh_mount();
    render_into(&mount, live_closed);
    yield_now().await;

    let trigger = find(&mount, TRIGGER).unwrap();
    let participants = find(&mount, PARTICIPANTS).expect("the count is its own button");
    assert_eq!(participants.tag_name(), "BUTTON");
    let node: &web_sys::Node = &participants;
    assert!(
        !trigger.contains(Some(node)),
        "a button nested in the trigger would bubble its click into Meeting info"
    );
    assert!(participants
        .closest("[aria-hidden='true']")
        .unwrap()
        .is_none());
    assert!(
        find(
            &mount,
            ".meeting-footer [aria-hidden='true'] :is(button, a[href], input, select, textarea, [tabindex])"
        )
        .is_none(),
        "no focusable element may sit in an aria-hidden subtree"
    );
    assert_eq!(
        participants.get_attribute("data-testid").as_deref(),
        Some("meeting-footer-participants")
    );
    for part in [
        ".meeting-footer-count-compact",
        ".meeting-footer-count-text",
    ] {
        assert!(
            participants.query_selector(part).unwrap().is_some(),
            "{part}"
        );
    }

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_participants_button_names_the_count_and_reports_the_peer_list_it_controls() {
    let mount = fresh_mount();
    render_into(&mount, live_closed);
    yield_now().await;

    let participants = html(&mount, PARTICIPANTS);
    let attr = |name: &str| participants.get_attribute(name);
    assert_eq!(attr("type").as_deref(), Some("button"));
    assert_eq!(attr("aria-label").as_deref(), Some("3 participants"));
    assert_eq!(
        attr("aria-controls").as_deref(),
        Some("peer-list-container")
    );
    assert_eq!(attr("aria-expanded").as_deref(), Some("false"));

    participants.click();
    yield_now().await;
    assert_eq!(attr("aria-expanded").as_deref(), Some("true"));

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn clicking_the_participant_count_toggles_the_peer_list_and_leaves_meeting_info_closed() {
    let mount = fresh_mount();
    render_into(&mount, live_closed);
    yield_now().await;

    html(&mount, PARTICIPANTS).click();
    yield_now().await;
    assert_eq!(text(&mount, PEER_LIST_TOGGLES), "1");
    assert_eq!(text(&mount, OPEN_STATE), "false");
    assert!(find(&mount, DIALOG).is_none());

    html(&mount, PARTICIPANTS).click();
    yield_now().await;
    assert_eq!(text(&mount, PEER_LIST_TOGGLES), "2");
    assert_eq!(text(&mount, OPEN_STATE), "false");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn clicking_the_meeting_info_trigger_opens_the_dialog_without_toggling_the_peer_list() {
    let mount = fresh_mount();
    render_into(&mount, live_closed);
    yield_now().await;

    html(&mount, TRIGGER).click();
    assert!(wait_for_selector(&mount, DIALOG, 1_000).await);
    yield_now().await;

    assert_eq!(text(&mount, OPEN_STATE), "true");
    assert_eq!(text(&mount, PEER_LIST_TOGGLES), "0");
    assert_eq!(
        html(&mount, PARTICIPANTS)
            .get_attribute("aria-expanded")
            .as_deref(),
        Some("false")
    );

    cleanup(&mount);
}

fn hit_at_center(mount: &web_sys::Element, selector: &str) -> web_sys::Element {
    let rect = html(mount, selector).get_bounding_client_rect();
    gloo_utils::document()
        .element_from_point(
            ((rect.left() + rect.right()) / 2.0) as f32,
            ((rect.top() + rect.bottom()) / 2.0) as f32,
        )
        .unwrap_or_else(|| panic!("nothing is hit at the centre of {selector}"))
}

#[wasm_bindgen_test]
async fn a_press_on_the_count_reaches_the_participants_button_and_any_other_press_reaches_meeting_info(
) {
    let mount = fresh_mount();
    install_stylesheets("");
    render_into(&mount, live_closed);
    yield_now().await;

    let footer_w = html(&mount, "footer.meeting-footer")
        .get_bounding_client_rect()
        .width();
    assert!(
        footer_w >= 560.0,
        "premise: a footer wide enough ({footer_w}px) to show the count text"
    );
    for selector in [
        ".meeting-footer-status-label",
        "[data-testid='meeting-footer-timer']",
        "[data-testid='meeting-footer-meeting-id']",
        "[data-testid='meeting-footer-version']",
        ".meeting-footer-affordance",
    ] {
        let hit = hit_at_center(&mount, selector);
        assert_eq!(
            hit.id(),
            "meeting-footer-trigger",
            "a press on {selector} must land on the Meeting info trigger, got <{}>",
            hit.tag_name()
        );
    }
    let hit = hit_at_center(&mount, ".meeting-footer-count-text");
    assert!(
        hit.closest(PARTICIPANTS).unwrap().is_some(),
        "a press on the count must land on the participants button, got <{} id={:?}>",
        hit.tag_name(),
        hit.id()
    );
    let count = html(&mount, ".meeting-footer-count-text").get_bounding_client_rect();
    let footer = html(&mount, "footer.meeting-footer").get_bounding_client_rect();
    for y in [footer.top() + 2.0, footer.bottom() - 2.0] {
        let near_miss = gloo_utils::document()
            .element_from_point(((count.left() + count.right()) / 2.0) as f32, y as f32)
            .unwrap();
        assert!(
            near_miss.closest(PARTICIPANTS).unwrap().is_some(),
            "a press above or below the count (y={y}) must still reach the participants button"
        );
    }

    hit.dyn_ref::<web_sys::HtmlElement>()
        .expect("the hit is an HTML element")
        .click();
    yield_now().await;
    assert_eq!(text(&mount, PEER_LIST_TOGGLES), "1");
    assert_eq!(text(&mount, OPEN_STATE), "false");

    remove_marked_nodes();
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

const CHANGELOG_GROUP: &str = "[data-testid='changelog-group']";
const NEWEST_SECTION: &str = "[data-testid='changelog-build']:has(#meeting-info-changelog-build-0)";
const OLDER_SECTION: &str = "[data-testid='changelog-build']:has(#meeting-info-changelog-build-1)";

/// Build 2 lists curated lines out of display order, with a repeat and an
/// uncategorized line; build 1 lists only a raw PR title.
fn kinds_fixture() -> String {
    r#"{"builds":[
      {"built":"2026-09-02T12:00:00Z","version":"1.1.2","commit":"00000002","changes":[
        "bug-fix: No echo after a rejoin (#3003)",
        "Meeting-api: tidy logs (#3004)",
        "UX: Clearer mute button (#3001)",
        "Perf: Faster joins",
        "UX: Clearer mute button (#3005)"]},
      {"built":"2026-09-01T12:00:00Z","version":"1.1.1","commit":"00000001","changes":[
        "Pinning moves a tile to the front of the grid (#2872)"]}]}"#
        .to_string()
}

fn all_attr(mount: &web_sys::Element, selector: &str, name: &str) -> Vec<String> {
    let nodes = mount.query_selector_all(selector).unwrap();
    (0..nodes.length())
        .filter_map(|i| nodes.item(i))
        .filter_map(|n| n.dyn_into::<web_sys::Element>().ok())
        .filter_map(|el| el.get_attribute(name))
        .collect()
}

#[wasm_bindgen_test]
async fn curated_lines_are_grouped_under_kind_pills_without_their_prefix_or_pr_number() {
    let mount = fresh_mount();
    install_stylesheets("");
    stub_changelog(&kinds_fixture(), "application/json");
    render_into(&mount, live_open);
    yield_now().await;
    open_whats_new(&mount).await;

    let groups = format!("{NEWEST_SECTION} > {CHANGELOG_GROUP}");
    assert_eq!(
        all_attr(&mount, &groups, "data-kind"),
        ["ux", "performance", "fixes", "miscellaneous"]
    );
    assert_eq!(
        all_text(
            &mount,
            &format!("{groups} > h6[data-testid='changelog-kind']")
        ),
        ["UX", "Performance", "Fixes", "Miscellaneous"]
    );
    assert_eq!(
        all_text(
            &mount,
            &format!("{groups} > ul.changelog-changes > li[data-testid='changelog-change']")
        ),
        [
            "Clearer mute button",
            "Faster joins",
            "No echo after a rejoin",
            "Meeting-api: tidy logs"
        ]
    );

    let ux = find(&mount, &format!("{groups} > h6.changelog-kind--ux")).unwrap();
    let fixes = find(&mount, &format!("{groups} > h6.changelog-kind--fixes")).unwrap();
    assert_eq!(computed(&ux, "border-top-left-radius"), "9999px");
    assert_ne!(
        computed(&ux, "background-color"),
        computed(&fixes, "background-color"),
        "each kind's pill has its own colour"
    );

    assert_eq!(
        count(&mount, &format!("{OLDER_SECTION} {CHANGELOG_GROUP}")),
        0
    );
    assert_eq!(
        all_text(
            &mount,
            &format!("{OLDER_SECTION} > ul.changelog-changes > li[data-testid='changelog-change']")
        ),
        ["Pinning moves a tile to the front of the grid"],
        "a build with no curated line keeps one plain list"
    );

    remove_marked_nodes();
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
                    participants_open: false,
                    on_toggle_participants: |_| {},
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
        div { style: "--drawer-left-reserve: 0px; --drawer-right-reserve: calc(100% - 180px);",
            MeetingFooter {
                open,
                meeting_id: MEETING_ID.to_string(),
                participant_count: 12,
                is_active: true,
                participants_open: false,
                on_toggle_participants: |_| {},
            }
        }
    }
}

fn hour_long_meeting_in_a_narrow_footer() -> Element {
    rsx! { HourLongMeetingInANarrowFooter {} }
}

#[wasm_bindgen_test]
async fn an_hour_long_meeting_line_is_clipped_in_a_narrow_footer_not_painted_over_the_version() {
    assert_a_narrow_footer_clips_the_hour_long_line("").await;
}

#[wasm_bindgen_test]
async fn in_a_wide_font_an_hour_long_meeting_line_is_clipped_in_a_narrow_footer() {
    assert_a_narrow_footer_clips_the_hour_long_line(WIDE_FONT).await;
}

async fn assert_a_narrow_footer_clips_the_hour_long_line(font_css: &str) {
    let mount = fresh_mount();
    install_stylesheets(font_css);
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
        (footer_w - 180.0).abs() < 1.0,
        "premise: a 180px footer, got {footer_w}"
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
    let app_text = html(&mount, ".meeting-footer-app-text")
        .get_bounding_client_rect()
        .width();
    assert!(
        app_text < 1.0,
        "the meeting line is clipped while {app_text}px of app info still shows"
    );

    remove_marked_nodes();
}

const LONG_MEETING_ID: &str =
    "quarterly-planning-review-for-the-platform-infrastructure-and-developer-experience-teams";

#[component]
fn FooterWithALongMeetingId() -> Element {
    provide_meeting_time();
    let open = use_signal(|| false);
    rsx! {
        div { "data-testid": "footer-width",
            MeetingFooter {
                open,
                meeting_id: LONG_MEETING_ID.to_string(),
                participant_count: 3,
                is_active: true,
                participants_open: false,
                on_toggle_participants: |_| {},
            }
        }
    }
}

fn footer_with_a_long_meeting_id() -> Element {
    rsx! { FooterWithALongMeetingId {} }
}

fn px(el: &web_sys::Element, property: &str) -> f64 {
    computed(el, property)
        .trim_end_matches("px")
        .parse()
        .unwrap_or_else(|_| panic!("{property} is not a px length"))
}

fn ring_right(el: &web_sys::Element) -> f64 {
    let right = el.get_bounding_client_rect().right();
    if computed(el, "outline-style") == "none" {
        right
    } else {
        right + px(el, "outline-width") + px(el, "outline-offset")
    }
}

fn set_footer_width(mount: &web_sys::Element, width: f64) -> web_sys::DomRect {
    html(mount, "[data-testid='footer-width']")
        .style()
        .set_property("--drawer-right-reserve", &format!("calc(100% - {width}px)"))
        .unwrap();
    let footer = html(mount, "footer.meeting-footer").get_bounding_client_rect();
    assert!(
        (footer.width() - width).abs() < 1.0,
        "premise: a {width}px footer, got {}",
        footer.width()
    );
    footer
}

#[wasm_bindgen_test]
async fn the_participants_trigger_reads_as_shown_only_outside_tier_s() {
    let mount = fresh_mount();
    assert!(!element_is_shown(PARTICIPANTS_ID), "premise: no footer yet");
    install_stylesheets("");
    render_into(&mount, footer_with_a_long_meeting_id);
    yield_now().await;

    for (width, shown) in [(340.0, false), (620.0, true), (340.0, false)] {
        set_footer_width(&mount, width);
        assert_eq!(element_is_shown(PARTICIPANTS_ID), shown, "at {width}px");
    }

    remove_marked_nodes();
}

#[wasm_bindgen_test]
async fn focus_reads_as_dropped_on_body_or_on_a_participants_trigger_hidden_by_tier_s() {
    let mount = fresh_mount();
    install_stylesheets("");
    render_into(&mount, footer_with_a_long_meeting_id);
    yield_now().await;

    set_footer_width(&mount, 620.0);
    html(&mount, PARTICIPANTS).focus().unwrap();
    assert!(active_is(&mount, PARTICIPANTS), "premise");
    assert!(!focus_was_dropped());

    set_footer_width(&mount, 340.0);
    assert!(focus_was_dropped());

    html(&mount, TRIGGER).focus().unwrap();
    assert!(active_is(&mount, TRIGGER), "premise");
    assert!(
        !focus_was_dropped(),
        "focus that moved elsewhere was not dropped"
    );
    html(&mount, TRIGGER).blur().unwrap();
    assert!(focus_was_dropped(), "focus on <body> is dropped");

    remove_marked_nodes();
}

#[wasm_bindgen_test]
async fn the_help_link_opens_the_help_page_in_a_new_tab_outside_the_hidden_group() {
    let mount = fresh_mount();
    render_into(&mount, live_closed);
    yield_now().await;

    let help = find(&mount, HELP).expect("the footer links to the help page");
    let attr = |name: &str| help.get_attribute(name).unwrap_or_default();
    assert_eq!(help.tag_name(), "A");
    assert_eq!(attr("href"), "/help?from=call");
    assert_eq!(
        html(&mount, TRIGGER).get_attribute("title").as_deref(),
        Some("Meeting info")
    );
    assert_eq!(attr("title"), "Help and FAQ (opens in a new tab)");
    assert_eq!(
        attr("target"),
        "_blank",
        "navigating the meeting tab away would leave the call"
    );
    assert!(
        attr("rel").split_whitespace().any(|r| r == "noopener"),
        "{}",
        attr("rel")
    );
    assert_eq!(attr("aria-label"), "Help and FAQ (opens in a new tab)");
    assert!(help.closest("[aria-hidden='true']").unwrap().is_none());
    let node: &web_sys::Node = &help;
    assert!(find(&mount, "footer.meeting-footer")
        .unwrap()
        .contains(Some(node)));

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_press_on_the_help_icon_reaches_the_link_at_every_width_and_leaves_meeting_info_closed() {
    let mount = fresh_mount();
    install_stylesheets("");
    render_into(&mount, live_closed);
    yield_now().await;
    js_sys::eval(
        "window.__helpClicks = []; window.__helpGuard = function (e) { \
           var a = e.target.closest && e.target.closest('[data-testid=\"meeting-footer-help\"]'); \
           if (a) { e.preventDefault(); window.__helpClicks.push(a.getAttribute('href')); } }; \
         window.addEventListener('click', window.__helpGuard, true);",
    )
    .unwrap();

    for width in [240.0, 300.0, 450.0, 620.0, 770.0] {
        mount
            .dyn_ref::<web_sys::HtmlElement>()
            .unwrap()
            .style()
            .set_property("--drawer-right-reserve", &format!("calc(100% - {width}px)"))
            .unwrap();
        let footer = html(&mount, "footer.meeting-footer").get_bounding_client_rect();
        assert!(
            (footer.width() - width).abs() < 1.0,
            "premise: a {width}px footer"
        );
        let help = html(&mount, HELP);
        let rect = help.get_bounding_client_rect();
        assert!(
            rect.width() >= 24.0 && rect.height() >= 24.0,
            "at {width}px the help target is {}x{}",
            rect.width(),
            rect.height()
        );
        assert!(
            rect.left() >= footer.left() && rect.right() <= footer.right() + 0.5,
            "at {width}px the help link sits outside the footer"
        );
        let hit = hit_at_center(&mount, HELP);
        assert!(
            hit.closest(HELP).unwrap().is_some(),
            "at {width}px a press on the help icon lands on <{} id={:?}>",
            hit.tag_name(),
            hit.id()
        );
        assert_eq!(
            hit_at_center(&mount, ".meeting-footer-affordance").id(),
            "meeting-footer-trigger",
            "at {width}px the help link must not cover the Meeting info affordance"
        );
        assert_eq!(
            computed(&html(&mount, ".meeting-footer-help-label"), "display"),
            "none",
            "at {width}px the help link stays icon-only"
        );
    }

    mouse(&hit_at_center(&mount, HELP), "click");
    yield_now().await;
    let clicks = js_sys::eval(
        "window.removeEventListener('click', window.__helpGuard, true); \
         JSON.stringify(window.__helpClicks)",
    )
    .unwrap()
    .as_string()
    .unwrap();
    assert_eq!(clicks, r#"["/help?from=call"]"#);
    assert_eq!(
        text(&mount, OPEN_STATE),
        "false",
        "the help link must not open Meeting info"
    );
    assert!(find(&mount, DIALOG).is_none());
    assert_eq!(text(&mount, PEER_LIST_TOGGLES), "0");

    mount
        .dyn_ref::<web_sys::HtmlElement>()
        .unwrap()
        .style()
        .set_property("--drawer-right-reserve", "calc(100% - 900px)")
        .unwrap();
    assert_ne!(
        computed(&html(&mount, ".meeting-footer-help-label"), "display"),
        "none",
        "from the 900px tier the help link shows its Help label"
    );

    remove_marked_nodes();
}

#[wasm_bindgen_test]
async fn the_participants_button_follows_every_width_tier_and_its_focus_ring_is_never_clipped() {
    let mount = fresh_mount();
    install_stylesheets("");
    render_into(&mount, footer_with_a_long_meeting_id);
    yield_now().await;

    let participants = html(&mount, PARTICIPANTS);
    let pill = html(&mount, ".meeting-footer-participants-pill");
    for width in [300.0, 450.0, 620.0, 770.0] {
        let footer = set_footer_width(&mount, width);
        if width < 400.0 {
            assert_eq!(computed(&participants, "display"), "none");
            participants.focus().unwrap();
            assert!(
                !active_is(&mount, PARTICIPANTS),
                "the hidden count must not be a tab stop"
            );
            continue;
        }
        let room = html(&mount, "[data-testid='meeting-footer-meeting-id']");
        assert!(
            room.scroll_width() > room.client_width(),
            "premise: the meeting ID is ellipsized at {width}px"
        );
        participants.focus().unwrap();
        assert!(
            participants.matches(":focus-visible").unwrap()
                && computed(&pill, "outline-style") != "none",
            "premise: the focus ring shows"
        );
        let group = html(&mount, ".meeting-footer-group--meeting").get_bounding_client_rect();
        for ring in [ring_right(&participants), ring_right(&pill)] {
            assert!(
                ring <= group.right() + 0.5,
                "at {width}px the ring ends at {ring}px, past the meeting group's clip edge at {}px",
                group.right()
            );
        }
        let button = participants.get_bounding_client_rect();
        assert!(button.top() >= footer.top() && button.bottom() <= footer.bottom());
        assert!(
            button.height() >= footer.height() - 1.5,
            "at {width}px the count's hit area is {}px of a {}px footer",
            button.height(),
            footer.height()
        );
        assert!((pill.get_bounding_client_rect().height() - 24.0).abs() < 0.5);
        participants.blur().unwrap();
    }

    remove_marked_nodes();
}

#[component]
fn FooterForTheMeetingId(minutes: u32, participants: usize) -> Element {
    let meeting_time = use_signal(|| MeetingTime {
        call_start_time: None,
        meeting_start_time: Some(js_sys::Date::now() - f64::from(minutes) * 60_000.0),
    });
    use_context_provider(|| meeting_time);
    let open = use_signal(|| false);
    rsx! {
        div { "data-testid": "footer-width",
            MeetingFooter {
                open,
                meeting_id: MEETING_ID.to_string(),
                participant_count: participants,
                is_active: true,
                participants_open: false,
                on_toggle_participants: |_| {},
            }
        }
    }
}

fn footer_with_a_five_minute_meeting() -> Element {
    rsx! { FooterForTheMeetingId { minutes: 5, participants: 3 } }
}

fn footer_with_an_hour_long_meeting() -> Element {
    rsx! { FooterForTheMeetingId { minutes: 62, participants: 12 } }
}

const WIDE_FONT: &str = ".meeting-footer { font-family: Verdana, \"DejaVu Sans\", sans-serif; }";

#[derive(Clone, Copy, Debug)]
enum IdFloor {
    Full,
    EightChOrFull,
}

const ID_FLOORS: [(f64, IdFloor); 5] = [
    (320.0, IdFloor::Full),
    (400.0, IdFloor::EightChOrFull),
    (560.0, IdFloor::EightChOrFull),
    (600.0, IdFloor::EightChOrFull),
    (768.0, IdFloor::Full),
];

fn ch_px(room: &web_sys::Element, chars: usize) -> f64 {
    let doc = gloo_utils::document();
    let probe = doc.create_element("span").unwrap();
    probe
        .set_attribute("style", "position: absolute; visibility: hidden;")
        .unwrap();
    probe.set_text_content(Some(&"0".repeat(chars)));
    room.append_child(&probe).unwrap();
    let width = probe.get_bounding_client_rect().width();
    probe.remove();
    width
}

async fn assert_the_meeting_id_keeps_its_room(
    root: fn() -> Element,
    timer_colons: usize,
    font_css: &str,
) {
    let mount = fresh_mount();
    install_stylesheets(font_css);
    render_into(&mount, root);
    yield_now().await;
    let timer_ready = wait_until(|| {
        text(&mount, "[data-testid='meeting-footer-timer']")
            .matches(':')
            .count()
            == timer_colons
    })
    .await;
    assert!(timer_ready, "premise: the timer shows its full length");
    assert!(
        find(&mount, HELP).is_some(),
        "premise: the help link is in the footer"
    );

    for (width, floor) in ID_FLOORS {
        set_footer_width(&mount, width);
        let room = html(&mount, "[data-testid='meeting-footer-meeting-id']");
        let group = html(&mount, ".meeting-footer-group--meeting");
        let (visible, full) = visible_meeting_id_px(&room, &group);
        let floor = match floor {
            IdFloor::Full => full - 0.5,
            IdFloor::EightChOrFull => ch_px(&room, 8).min(full - 0.5),
        };
        let app_text = html(&mount, ".meeting-footer-app-text")
            .get_bounding_client_rect()
            .width();
        assert_eq!(computed(&room, "text-overflow"), "ellipsis", "at {width}px");
        assert_ne!(
            computed(&room, "overflow-x"),
            "visible",
            "at {width}px a truncated ID would spill past its box instead of ending in an ellipsis"
        );
        assert!(
            room.get_bounding_client_rect().right()
                <= group.get_bounding_client_rect().right() + 0.5,
            "at {width}px the meeting ID is hard-clipped instead of ending in an ellipsis"
        );
        if visible < full - 0.5 {
            assert!(
                room.scroll_width() > room.client_width(),
                "at {width}px the ID is cut short by something other than its own ellipsis"
            );
            assert!(
                app_text < 1.0,
                "at {width}px the ID gave up width ({visible} of {full}px) while {app_text}px of app info still shows"
            );
        }
        assert!(
            visible >= floor,
            "at {width}px only {visible}px of the {full}px meeting ID shows (floor {floor})"
        );
    }

    remove_marked_nodes();
}

fn visible_meeting_id_px(room: &web_sys::Element, group: &web_sys::Element) -> (f64, f64) {
    let measure = js_sys::Function::new_with_args(
        "room, group",
        "const text = [...room.childNodes].find((n) => n.nodeType === Node.TEXT_NODE); \
         const range = document.createRange(); range.selectNodeContents(text); \
         const t = range.getBoundingClientRect(); const r = room.getBoundingClientRect(); \
         const visible = Math.min(t.right, r.left + room.clientWidth, group.getBoundingClientRect().right) - t.left; \
         return [visible, t.width];",
    );
    let out: js_sys::Array = measure
        .call2(&JsValue::NULL, room, group)
        .unwrap()
        .unchecked_into();
    (out.get(0).as_f64().unwrap(), out.get(1).as_f64().unwrap())
}

#[wasm_bindgen_test]
async fn beside_the_help_link_a_short_meetings_id_keeps_its_room() {
    assert_the_meeting_id_keeps_its_room(footer_with_a_five_minute_meeting, 1, "").await;
}

#[wasm_bindgen_test]
async fn beside_the_help_link_an_hour_long_meetings_id_keeps_its_room() {
    assert_the_meeting_id_keeps_its_room(footer_with_an_hour_long_meeting, 2, "").await;
}

#[wasm_bindgen_test]
async fn in_a_wide_font_a_short_meetings_id_keeps_its_room() {
    assert_the_meeting_id_keeps_its_room(footer_with_a_five_minute_meeting, 1, WIDE_FONT).await;
}

#[wasm_bindgen_test]
async fn in_a_wide_font_an_hour_long_meetings_id_keeps_its_room() {
    assert_the_meeting_id_keeps_its_room(footer_with_an_hour_long_meeting, 2, WIDE_FONT).await;
}

const APP_INFO_WIDTHS: [f64; 9] = [
    180.0, 240.0, 320.0, 400.0, 560.0, 600.0, 768.0, 840.0, 1024.0,
];
const APP_INFO_FITS: [f64; 2] = [768.0, 1024.0];

async fn assert_app_info_shows_whole_units_beside_the_help_link(font_css: &str) {
    let fixtures: [(fn() -> Element, usize); 2] = [
        (footer_with_a_five_minute_meeting, 1),
        (footer_with_an_hour_long_meeting, 2),
    ];
    for (root, timer_colons) in fixtures {
        let mount = fresh_mount();
        install_stylesheets(font_css);
        render_into(&mount, root);
        yield_now().await;
        let timer_ready = wait_until(|| {
            text(&mount, "[data-testid='meeting-footer-timer']")
                .matches(':')
                .count()
                == timer_colons
        })
        .await;
        assert!(timer_ready, "premise: the timer shows its full length");

        for width in APP_INFO_WIDTHS {
            set_footer_width(&mount, width);
            let text_box = html(&mount, ".meeting-footer-app-text");
            for axis in ["overflow-x", "overflow-y"] {
                assert_ne!(
                    computed(&text_box, axis),
                    "visible",
                    "at {width}px app info that does not fit would paint past its box"
                );
            }
            let b = text_box.get_bounding_client_rect();
            let fits = APP_INFO_FITS.contains(&width);
            if width >= 1024.0 {
                assert_ne!(
                    computed(&html(&mount, ".meeting-footer-app-built"), "display"),
                    "none",
                    "premise: at {width}px the build date is displayed"
                );
            }
            let units = text_box.children();
            for i in 0..units.length() {
                let unit = units.item(i).unwrap();
                if computed(&unit, "display") == "none" {
                    continue;
                }
                let r = unit.get_bounding_client_rect();
                let inside = r.left() >= b.left() - 0.5
                    && r.right() <= b.right() + 0.5
                    && r.top() >= b.top() - 0.5
                    && r.bottom() <= b.bottom() + 0.5;
                let overlap_x = r.right().min(b.right()) - r.left().max(b.left());
                let overlap_y = r.bottom().min(b.bottom()) - r.top().max(b.top());
                assert!(
                    inside || overlap_x <= 0.5 || overlap_y <= 0.5,
                    "at {width}px app info shows part of {:?}",
                    unit.text_content().unwrap_or_default()
                );
                if fits {
                    assert!(
                        inside && r.width() > 0.5 && r.height() > 0.5,
                        "at {width}px app info hides {:?} though it fits",
                        unit.text_content().unwrap_or_default()
                    );
                }
            }

            let icon = html(&mount, ".meeting-footer-affordance").get_bounding_client_rect();
            let help = html(&mount, HELP).get_bounding_client_rect();
            assert!(
                icon.width() >= 13.5 && help.width() >= 14.0,
                "at {width}px the info icon ({}px) or the help link ({}px) is squeezed",
                icon.width(),
                help.width()
            );
            assert!(
                icon.right() <= help.left() + 0.5,
                "at {width}px the info icon ({}..{}px) runs into the help link at {}px",
                icon.left(),
                icon.right(),
                help.left()
            );
        }

        remove_marked_nodes();
    }
}

#[wasm_bindgen_test]
async fn app_info_shows_whole_units_beside_the_help_link() {
    assert_app_info_shows_whole_units_beside_the_help_link("").await;
}

#[wasm_bindgen_test]
async fn in_a_wide_font_app_info_shows_whole_units_beside_the_help_link() {
    assert_app_info_shows_whole_units_beside_the_help_link(WIDE_FONT).await;
}

#[component]
fn FooterWithASeventyCharacterMeetingId() -> Element {
    provide_meeting_time();
    let open = use_signal(|| false);
    rsx! {
        div { "data-testid": "footer-width",
            MeetingFooter {
                open,
                meeting_id: LONG_MEETING_ID[..70].to_string(),
                participant_count: 3,
                is_active: true,
                participants_open: false,
                on_toggle_participants: |_| {},
            }
        }
    }
}

fn footer_with_a_seventy_character_meeting_id() -> Element {
    rsx! { FooterWithASeventyCharacterMeetingId {} }
}

async fn assert_the_app_name_never_shows_without_its_version(font_css: &str) {
    let mount = fresh_mount();
    install_stylesheets(font_css);
    render_into(&mount, footer_with_a_seventy_character_meeting_id);
    yield_now().await;

    let mut version_hidden = false;
    let mut version_shown = false;
    let mut seen = String::new();
    for step in 0..=110 {
        let width = 840.0 + 4.0 * f64::from(step);
        set_footer_width(&mount, width);
        let text_box = html(&mount, ".meeting-footer-app-text");
        let b = text_box.get_bounding_client_rect();
        let shown = |selector: &str| {
            let el = html(&mount, selector);
            let mut item: web_sys::Element = el.clone().into();
            while let Some(parent) = item.parent_element() {
                if parent.is_same_node(Some(&text_box)) {
                    break;
                }
                item = parent;
            }
            let r = el.get_bounding_client_rect();
            computed(&el, "display") != "none"
                && r.width() > 0.5
                && r.left() >= b.left() - 0.5
                && r.right() <= b.right() + 0.5
                && (item.get_bounding_client_rect().top() - b.top()).abs() < 0.5
        };
        let name = shown(".meeting-footer-app-name");
        let version = shown("[data-testid='meeting-footer-version']");
        assert!(
            !name || version,
            "at {width}px the app name shows without its version"
        );
        version_hidden |= !version;
        version_shown |= version;
        seen.push_str(&format!("{width}:{} ", u8::from(version)));
    }

    remove_marked_nodes();
    assert!(
        version_hidden && version_shown,
        "premise: the sweep crosses the width where app info comes back, saw {seen}"
    );
}

#[wasm_bindgen_test]
async fn beside_a_long_meeting_id_the_app_name_never_shows_without_its_version() {
    assert_the_app_name_never_shows_without_its_version("").await;
}

#[wasm_bindgen_test]
async fn in_a_wide_font_the_app_name_never_shows_without_its_version() {
    assert_the_app_name_never_shows_without_its_version(WIDE_FONT).await;
}

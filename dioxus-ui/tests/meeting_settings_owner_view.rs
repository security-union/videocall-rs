// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use dioxus::prelude::*;
use dioxus_ui::components::announcement_overrides::OrphanedOverrideSavesCtx;
use dioxus_ui::pages::meeting_settings::MeetingSettingsPage;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use support::{
    cleanup, create_mount_point, inject_app_config, render_into, reset_test_browser_state,
    restore_fetch, wait_for_selector,
};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

thread_local! {
    static SEED: RefCell<HashMap<String, u32>> = RefCell::new(HashMap::new());
    static ORPHANED: Cell<Option<Signal<HashMap<String, u32>>>> = const { Cell::new(None) };
}

fn orphaned_saves() -> Option<u32> {
    ORPHANED
        .with(Cell::get)
        .and_then(|saves| saves.peek().get("m1").copied())
}

fn meeting_gets() -> u32 {
    js_sys::eval("window.__settingsMeetingGets || 0")
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0) as u32
}

fn query(mount: &web_sys::Element, selector: &str) -> Option<web_sys::Element> {
    mount.query_selector(selector).unwrap()
}

fn text_of(mount: &web_sys::Element, selector: &str) -> Option<String> {
    query(mount, selector).map(|el| el.text_content().unwrap_or_default())
}

fn co_host_gets() -> u32 {
    js_sys::eval("window.__settingsCoHostGets || 0")
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0) as u32
}

/// Mocks the meeting-api: meeting reads (a PATCH merges its body into later
/// ones), `POST .../end` (always 500), and the co-hosts GET.
/// `host_display_name_js`/`host_user_id_js` are JS literals (`"..."` or `null`).
fn mock_settings_fetch(
    viewer_is_owner: bool,
    viewer_can_edit_options: bool,
    host_display_name_js: &str,
    host_user_id_js: &str,
) {
    let script = format!(
        r#"
        window.__original_fetch = window.__original_fetch || window.fetch;
        window.__settingsCoHostGets = 0;
        window.__settingsMeetingGets = 0;
        window.__settingsMeetingFails = false;
        window.__settingsHoldNext = false;
        window.__settingsOverrides = {{}};
        window.fetch = function(input, init) {{
            var url = typeof input === 'string' ? input : input.url;
            var respond = function(status, body) {{
                var resp = new Response(JSON.stringify(body), {{
                    status: status,
                    headers: {{ 'Content-Type': 'application/json' }}
                }});
                Object.defineProperty(resp, 'url', {{ value: url }});
                return resp;
            }};
            if (url.match(/\/co-hosts$/)) {{
                window.__settingsCoHostGets += 1;
                return Promise.resolve(respond(200, {{ success: true, result: {{ co_hosts: [
                    {{ user_id: 'alice@example.com', persistent: true, is_present_host: true,
                       display_name: 'Alice', designated: true, suspended: false }}
                ] }} }}));
            }}
            var meetingBody = function() {{
                return {{ success: true, result: Object.assign({{
                    meeting_id: 'm1',
                    state: 'active',
                    host: 'owner-id@example.com',
                    host_display_name: {host_display_name_js},
                    host_user_id: {host_user_id_js},
                    has_password: false,
                    waiting_room_enabled: false,
                    admitted_can_admit: false,
                    end_on_host_leave: true,
                    participant_count: 1,
                    waiting_count: 0,
                    started_at: 1714323500000,
                    viewer_is_owner: {viewer_is_owner},
                    viewer_can_edit_options: {viewer_can_edit_options}
                }}, window.__settingsOverrides) }};
            }};
            if (url.match(/\/meetings\/m1$/) && typeof input !== 'string' && input.method === 'PATCH') {{
                return input.text().then(function(text) {{
                    Object.assign(window.__settingsOverrides, JSON.parse(text));
                    return respond(200, meetingBody());
                }});
            }}
            if (url.match(/\/meetings\/m1\/end$/)) {{
                return Promise.resolve(respond(500, {{ success: false, result: null }}));
            }}
            if (url.match(/\/api\/v1\/meetings\//)) {{
                var meetingGet = !!url.match(/\/meetings\/m1$/);
                if (meetingGet) {{
                    window.__settingsMeetingGets += 1;
                    if (window.__settingsMeetingFails) {{
                        return Promise.resolve(respond(500, {{ success: false, result: null }}));
                    }}
                }}
                var body = meetingBody();
                if (meetingGet && window.__settingsHoldNext) {{
                    window.__settingsHoldNext = false;
                    return new Promise(function(resolve) {{
                        window.__settingsRelease = function() {{ resolve(respond(200, body)); }};
                    }});
                }}
                return Promise.resolve(respond(200, body));
            }}
            return Promise.resolve(respond(200, {{ success: true, result: {{}} }}));
        }};
        "#
    );
    js_sys::eval(&script).expect("failed to mock the settings page fetch");
}

#[derive(Clone, Routable, PartialEq, Debug)]
enum TestRoute {
    #[route("/")]
    Home {},
}

#[component]
fn Home() -> Element {
    rsx! { MeetingSettingsPage { id: "m1".to_string() } }
}

fn wrapper() -> Element {
    let orphaned = use_signal(|| SEED.with(RefCell::take));
    use_context_provider(|| OrphanedOverrideSavesCtx(orphaned));
    use_hook(|| ORPHANED.with(|o| o.set(Some(orphaned))));
    rsx! { Router::<TestRoute> {} }
}

async fn mount_settings(
    viewer_is_owner: bool,
    viewer_can_edit_options: bool,
    host_display_name_js: &str,
    host_user_id_js: &str,
) -> web_sys::Element {
    reset_test_browser_state();
    inject_app_config();
    mock_settings_fetch(
        viewer_is_owner,
        viewer_can_edit_options,
        host_display_name_js,
        host_user_id_js,
    );
    let mount = create_mount_point();
    render_into(&mount, wrapper);
    assert!(
        wait_for_selector(&mount, ".settings-card-title", 5_000).await,
        "the settings cards must render once the meeting fetch resolves"
    );
    mount
}

#[wasm_bindgen_test]
async fn the_details_card_labels_the_creator_owner() {
    let mount = mount_settings(true, true, "\"Alice Owner\"", "\"owner-id@example.com\"").await;
    let labels: Vec<String> = {
        let nodes = mount.query_selector_all(".settings-field-label").unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i))
            .map(|n| n.text_content().unwrap_or_default())
            .collect()
    };
    assert!(
        labels.iter().any(|l| l == "Owner"),
        "expected an Owner label, got {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l == "Host"),
        "the creator row must not read \"Host\": {labels:?}"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn the_owner_row_shows_the_name_and_the_email_as_a_mailto_link() {
    let mount = mount_settings(true, true, "\"Alice Owner\"", "\"owner-id@example.com\"").await;
    let owner_row = owner_field_row(&mount).expect("an Owner row must render");
    assert_eq!(
        text_of(&owner_row, ".settings-field-value").as_deref(),
        Some("Alice Owner")
    );
    let link = query(&owner_row, ".co-hosts-id").expect("a contact id node must render");
    assert_eq!(link.tag_name(), "A", "an email id must be a mailto link");
    assert_eq!(
        link.get_attribute("href").as_deref(),
        Some("mailto:owner-id@example.com")
    );
    assert_eq!(
        link.get_attribute("aria-label").as_deref(),
        Some("Email the meeting owner, owner-id@example.com")
    );
    assert_eq!(link.text_content().as_deref(), Some("owner-id@example.com"));
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn an_opaque_owner_id_renders_as_plain_text() {
    let mount = mount_settings(true, true, "\"Alice Owner\"", "\"uid-abc123\"").await;
    let owner_row = owner_field_row(&mount).expect("an Owner row must render");
    let node = query(&owner_row, ".co-hosts-id").expect("a contact id node must render");
    assert_eq!(node.tag_name(), "SPAN", "an opaque id must not be a link");
    assert!(node.get_attribute("href").is_none());
    assert_eq!(node.text_content().as_deref(), Some("uid-abc123"));
    cleanup(&mount);
    restore_fetch();
}

/// The Owner row's `.settings-field-compact` container.
fn owner_field_row(mount: &web_sys::Element) -> Option<web_sys::Element> {
    let nodes = mount.query_selector_all(".settings-field-label").unwrap();
    (0..nodes.length())
        .filter_map(|i| nodes.item(i))
        .map(|n| n.dyn_into::<web_sys::Element>().unwrap())
        .find(|el| el.text_content().as_deref() == Some("Owner"))
        .and_then(|label| label.closest(".settings-field-compact").ok().flatten())
}

#[wasm_bindgen_test]
async fn the_owner_row_falls_back_to_the_id_shown_once_with_no_display_name() {
    let mount = mount_settings(true, true, "null", "\"owner-id@example.com\"").await;
    let owner_row =
        owner_field_row(&mount).expect("an Owner row must render even with no display name");
    assert!(
        query(&owner_row, ".settings-field-value").is_none(),
        "no separate name slot when there is no display name"
    );
    let ids = owner_row.query_selector_all(".co-hosts-id").unwrap();
    assert_eq!(ids.length(), 1, "the id must appear exactly once");
    let link = ids.item(0).unwrap().dyn_into::<web_sys::Element>().unwrap();
    assert_eq!(link.tag_name(), "A");
    assert_eq!(
        link.get_attribute("href").as_deref(),
        Some("mailto:owner-id@example.com")
    );
    assert_eq!(link.text_content().as_deref(), Some("owner-id@example.com"));
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn the_owner_gets_the_full_co_host_management_ui() {
    let mount = mount_settings(true, true, "\"Alice Owner\"", "\"owner-id@example.com\"").await;
    assert!(
        wait_for_selector(&mount, "[data-testid='co-host-row']", 5_000).await,
        "the co-host row must render for the owner"
    );
    assert!(query(&mount, "[data-testid='co-host-input']").is_some());
    assert!(query(&mount, "[data-testid='co-host-remove']").is_some());
    let section = query(&mount, "[data-testid='co-hosts-section']").unwrap();
    assert_eq!(section.get_attribute("data-read-only"), None);
    assert_eq!(
        text_of(&section, ".co-hosts-hint").as_deref(),
        Some("Co-hosts share your host controls and can change meeting options. Only you can manage co-hosts.")
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_co_host_viewer_gets_the_read_only_co_hosts_card() {
    let mount = mount_settings(false, true, "\"Alice Owner\"", "\"owner-id@example.com\"").await;
    assert!(
        wait_for_selector(&mount, "[data-testid='co-host-row']", 5_000).await,
        "the read-only roster must still render"
    );
    assert!(query(&mount, "[data-testid='co-host-input']").is_none());
    assert!(query(&mount, "[data-testid='co-host-remove']").is_none());
    let section = query(&mount, "[data-testid='co-hosts-section']").unwrap();
    assert_eq!(
        section.get_attribute("data-read-only").as_deref(),
        Some("true")
    );
    assert_eq!(co_host_gets(), 1, "the list GET fires exactly once");
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_plain_participant_gets_no_co_hosts_card() {
    let mount = mount_settings(false, false, "\"Alice Owner\"", "\"owner-id@example.com\"").await;
    assert!(query(&mount, "[data-testid='co-hosts-section']").is_none());
    assert_eq!(co_host_gets(), 0, "the list GET must never fire");
    cleanup(&mount);
    restore_fetch();
}

fn end_a_save_chain() {
    ORPHANED
        .with(Cell::get)
        .expect("orphaned saves signal")
        .write()
        .insert("m1".to_string(), 1);
}

fn join_message(value: &str) -> String {
    format!("input[data-testid='meeting-option-join-message-{value}']:checked")
}

async fn wait_for_meeting_gets(count: u32) {
    for _ in 0..50 {
        if meeting_gets() == count {
            break;
        }
        support::yield_now().await;
    }
    assert_eq!(meeting_gets(), count);
}

#[wasm_bindgen_test]
async fn the_settings_page_consumes_a_save_chain_that_ended_before_it_opened() {
    SEED.with(|seed| seed.borrow_mut().insert("m1".to_string(), 1));
    let mount = mount_settings(true, true, "\"Alice Owner\"", "\"owner-id@example.com\"").await;
    assert_eq!(orphaned_saves(), None);
    assert_eq!(meeting_gets(), 1, "the page's first read is the refresh");
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn the_settings_page_rereads_when_a_save_chain_ends_while_it_is_open() {
    let mount = mount_settings(true, true, "\"Alice Owner\"", "\"owner-id@example.com\"").await;
    assert!(wait_for_selector(&mount, &join_message("participant"), 5_000).await);
    js_sys::eval("window.__settingsOverrides = { entry_message_override: 'on' }").unwrap();
    end_a_save_chain();
    assert!(
        wait_for_selector(&mount, &join_message("on"), 5_000).await,
        "the page must show the saved choice"
    );
    assert_eq!(meeting_gets(), 2);
    assert_eq!(orphaned_saves(), None);
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_reread_sent_earlier_cannot_undo_a_later_one() {
    let mount = mount_settings(true, true, "\"Alice Owner\"", "\"owner-id@example.com\"").await;
    js_sys::eval("window.__settingsHoldNext = true").unwrap();
    end_a_save_chain();
    wait_for_meeting_gets(2).await;
    js_sys::eval("window.__settingsOverrides = { entry_message_override: 'on' }").unwrap();
    end_a_save_chain();
    assert!(wait_for_selector(&mount, &join_message("on"), 5_000).await);
    js_sys::eval("window.__settingsRelease()").unwrap();
    gloo_timers::future::TimeoutFuture::new(100).await;
    assert!(
        query(&mount, &join_message("on")).is_some(),
        "the held read of the earlier value must not land"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_failed_reread_keeps_the_settings_page() {
    let mount = mount_settings(true, true, "\"Alice Owner\"", "\"owner-id@example.com\"").await;
    js_sys::eval("window.__settingsMeetingFails = true").unwrap();
    end_a_save_chain();
    wait_for_meeting_gets(2).await;
    gloo_timers::future::TimeoutFuture::new(100).await;
    assert!(query(&mount, ".settings-card-title").is_some());
    assert!(query(&mount, ".settings-empty-state").is_none());
    cleanup(&mount);
    restore_fetch();
}

fn waiting_room_switch(mount: &web_sys::Element) -> web_sys::Element {
    let rows = mount.query_selector_all(".settings-option-row").unwrap();
    (0..rows.length())
        .filter_map(|i| rows.item(i))
        .map(|n| n.dyn_into::<web_sys::Element>().unwrap())
        .find(|row| text_of(row, ".settings-option-label").as_deref() == Some("Waiting Room"))
        .and_then(|row| query(&row, "button[role='switch']"))
        .expect("a Waiting Room switch must render")
}

fn waiting_room_on(mount: &web_sys::Element) -> bool {
    waiting_room_switch(mount)
        .get_attribute("aria-checked")
        .as_deref()
        == Some("true")
}

#[wasm_bindgen_test]
async fn a_reread_sent_before_a_toggle_save_cannot_undo_the_toggle() {
    let mount = mount_settings(true, true, "\"Alice Owner\"", "\"owner-id@example.com\"").await;
    assert!(!waiting_room_on(&mount));
    js_sys::eval(
        "window.__settingsOverrides = { entry_message_override: 'on' }; \
         window.__settingsHoldNext = true",
    )
    .unwrap();
    end_a_save_chain();
    wait_for_meeting_gets(2).await;
    waiting_room_switch(&mount)
        .unchecked_into::<web_sys::HtmlElement>()
        .click();
    let saved_on =
        || waiting_room_on(&mount) && !waiting_room_switch(&mount).matches(":disabled").unwrap();
    for _ in 0..50 {
        if saved_on() {
            break;
        }
        support::yield_now().await;
    }
    assert!(
        saved_on(),
        "the toggle's save must land before the re-read does"
    );
    js_sys::eval("window.__settingsRelease()").unwrap();
    assert!(
        wait_for_selector(&mount, &join_message("on"), 5_000).await,
        "the re-read's overrides must land"
    );
    assert!(
        waiting_room_on(&mount),
        "a re-read that read the server before the toggle's save must not undo it"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_first_read_landing_after_a_failed_one_replaces_the_load_error() {
    reset_test_browser_state();
    inject_app_config();
    mock_settings_fetch(true, true, "\"Alice Owner\"", "\"owner-id@example.com\"");
    js_sys::eval("window.__settingsHoldNext = true").unwrap();
    let mount = create_mount_point();
    render_into(&mount, wrapper);
    wait_for_meeting_gets(1).await;
    js_sys::eval("window.__settingsMeetingFails = true").unwrap();
    end_a_save_chain();
    wait_for_meeting_gets(2).await;
    assert!(wait_for_selector(&mount, ".settings-empty-state", 5_000).await);
    assert_eq!(
        text_of(&mount, ".settings-empty-state h2").as_deref(),
        Some("Unable to load meeting"),
        "the failed read lands first and shows the load error"
    );
    js_sys::eval("window.__settingsRelease()").unwrap();
    assert!(
        wait_for_selector(&mount, ".settings-card-title", 5_000).await,
        "the held first read must replace the load error with the page"
    );
    assert!(query(&mount, ".settings-empty-state").is_none());
    cleanup(&mount);
    restore_fetch();
}

fn end_meeting_button(mount: &web_sys::Element) -> web_sys::HtmlElement {
    let buttons = mount.query_selector_all(".settings-action-btn").unwrap();
    (0..buttons.length())
        .filter_map(|i| buttons.item(i))
        .map(|n| n.dyn_into::<web_sys::HtmlElement>().unwrap())
        .find(|b| b.text_content().unwrap_or_default().contains("End Meeting"))
        .expect("an End Meeting button must render for the owner")
}

#[wasm_bindgen_test]
async fn a_reread_landing_after_the_page_loaded_keeps_an_end_meeting_error() {
    let mount = mount_settings(true, true, "\"Alice Owner\"", "\"owner-id@example.com\"").await;
    js_sys::eval(
        "window.__original_confirm = window.confirm; \
         window.confirm = function() { return true; }",
    )
    .unwrap();
    end_meeting_button(&mount).click();
    assert!(wait_for_selector(&mount, ".settings-empty-state", 5_000).await);
    let end_error = || text_of(&mount, ".settings-empty-state p");
    assert!(
        end_error().is_some_and(|t| t.starts_with("Failed to end meeting")),
        "a failed End must show its error, got {:?}",
        end_error()
    );
    end_a_save_chain();
    wait_for_meeting_gets(2).await;
    gloo_timers::future::TimeoutFuture::new(100).await;
    assert!(
        end_error().is_some_and(|t| t.starts_with("Failed to end meeting")),
        "a re-read after the page loaded must not clear the End error, got {:?}",
        end_error()
    );
    js_sys::eval("window.confirm = window.__original_confirm").unwrap();
    cleanup(&mount);
    restore_fetch();
}

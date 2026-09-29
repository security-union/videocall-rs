// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue 2794: the self tile's click-to-rename name chip.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use dioxus::prelude::*;
use dioxus_ui::components::display_name_edit::SelfTileName;
use support::{
    cleanup, create_mount_point, inject_app_config, render_into, restore_fetch, yield_now,
};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const BUTTON: &str = "[data-testid='self-tile-name-button']";
const INPUT: &str = "[data-testid='self-tile-name-input']";
const ERROR: &str = "[data-testid='self-tile-name-error']";
const STATUS: &str = "[data-testid='self-tile-name-status']";

thread_local! {
    static RENAMED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    static PARENT_CLICKS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static PARENT_KEYS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static PANICS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn reset() {
    RENAMED.with(|r| r.borrow_mut().clear());
    PARENT_CLICKS.with(|c| c.set(0));
    PARENT_KEYS.with(|c| c.set(0));
}

#[allow(non_snake_case)]
fn Harness() -> Element {
    rsx! {
        div {
            onclick: move |_| PARENT_CLICKS.with(|c| c.set(c.get() + 1)),
            onkeydown: move |_| PARENT_KEYS.with(|c| c.set(c.get() + 1)),
            SelfTileName {
                display_name: "Alice".to_string(),
                meeting_id: "room-2794".to_string(),
                session_id: Some(7),
                on_renamed: move |name: String| RENAMED.with(|r| r.borrow_mut().push(name)),
            }
        }
    }
}

fn find(mount: &web_sys::Element, selector: &str) -> Option<web_sys::HtmlElement> {
    mount
        .query_selector(selector)
        .ok()
        .flatten()
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
}

fn is_active(el: &web_sys::HtmlElement) -> bool {
    gloo_utils::document()
        .active_element()
        .is_some_and(|a| &a == el.unchecked_ref::<web_sys::Element>())
}

fn js(src: &str) -> wasm_bindgen::JsValue {
    js_sys::eval(src).expect("eval")
}

fn dispatch(el: &web_sys::HtmlElement, event_js: &str) {
    let event: web_sys::Event = js(event_js).unchecked_into();
    el.dispatch_event(&event).expect("dispatch");
}

fn input_in(mount: &web_sys::Element) -> web_sys::HtmlInputElement {
    find(mount, INPUT)
        .expect("inline input")
        .dyn_into::<web_sys::HtmlInputElement>()
        .unwrap()
}

fn type_into_input(mount: &web_sys::Element, value: &str) {
    let input = input_in(mount);
    input.set_value(value);
    dispatch(&input, "new Event('input', { bubbles: true })");
}

fn key_on_input(mount: &web_sys::Element, key: &str) {
    dispatch(
        &input_in(mount),
        &format!(
            "new KeyboardEvent('keydown', {{ key: {key:?}, bubbles: true, cancelable: true }})"
        ),
    );
}

fn blur_input(mount: &web_sys::Element) {
    input_in(mount).blur().expect("blur");
}

async fn sleep_ms(ms: u32) {
    gloo_timers::future::TimeoutFuture::new(ms).await;
}

async fn open_editor(mount: &web_sys::Element) -> web_sys::HtmlInputElement {
    find(mount, BUTTON).expect("name button").click();
    yield_now().await;
    sleep_ms(20).await;
    find(mount, INPUT)
        .expect("inline input after click")
        .dyn_into::<web_sys::HtmlInputElement>()
        .unwrap()
}

#[wasm_bindgen_test]
async fn clicking_name_opens_focused_prefilled_editor_without_bubbling() {
    reset();
    let mount = create_mount_point();
    render_into(&mount, Harness);
    yield_now().await;

    let button = find(&mount, BUTTON).expect("name button");
    let label = button.get_attribute("aria-label").unwrap_or_default();
    assert!(
        label.contains("Edit display name") && label.contains("Alice"),
        "{label}"
    );
    assert_eq!(button.tag_name(), "BUTTON");

    let input = open_editor(&mount).await;
    assert_eq!(input.value(), "Alice");
    assert_eq!(
        input.get_attribute("aria-label").as_deref(),
        Some("Display name")
    );
    assert!(is_active(input.unchecked_ref()), "input must hold focus");
    assert!(find(&mount, BUTTON).is_none());
    assert_eq!(
        PARENT_CLICKS.with(|c| c.get()),
        0,
        "click must not reach the grid"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn escape_cancels_restores_name_and_focus_without_bubbling() {
    reset();
    let mount = create_mount_point();
    render_into(&mount, Harness);
    yield_now().await;

    open_editor(&mount).await;
    type_into_input(&mount, "Bob");
    yield_now().await;
    key_on_input(&mount, "Escape");
    yield_now().await;
    sleep_ms(20).await;

    assert!(find(&mount, INPUT).is_none(), "editor closes on Escape");
    let button = find(&mount, BUTTON).expect("name button back");
    assert_eq!(button.text_content().unwrap_or_default().trim(), "Alice");
    assert!(is_active(&button), "focus returns to the name button");
    assert!(RENAMED.with(|r| r.borrow().is_empty()));
    assert_eq!(
        PARENT_KEYS.with(|c| c.get()),
        0,
        "keydown must not reach the container"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn unchanged_name_on_enter_closes_without_rename() {
    reset();
    let mount = create_mount_point();
    render_into(&mount, Harness);
    yield_now().await;

    open_editor(&mount).await;
    type_into_input(&mount, "  Alice ");
    yield_now().await;
    key_on_input(&mount, "Enter");
    yield_now().await;
    sleep_ms(20).await;

    assert!(find(&mount, INPUT).is_none());
    assert!(find(&mount, BUTTON).is_some());
    assert!(RENAMED.with(|r| r.borrow().is_empty()));

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn invalid_name_keeps_editor_open_with_accessible_error() {
    reset();
    let mount = create_mount_point();
    render_into(&mount, Harness);
    yield_now().await;

    open_editor(&mount).await;
    type_into_input(&mount, "Bob<script>");
    yield_now().await;
    key_on_input(&mount, "Enter");
    yield_now().await;

    let input = find(&mount, INPUT).expect("editor stays open");
    let error = find(&mount, ERROR).expect("error message");
    assert_eq!(error.get_attribute("role").as_deref(), Some("alert"));
    assert_eq!(input.get_attribute("aria-invalid").as_deref(), Some("true"));
    assert_eq!(input.get_attribute("aria-describedby"), Some(error.id()));
    assert!(RENAMED.with(|r| r.borrow().is_empty()));

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn failed_rename_sends_once_and_keeps_editor_open() {
    reset();
    inject_app_config();
    js("window.__rename_calls = 0; window.__original_fetch = window.__original_fetch || window.fetch; \
        window.fetch = function() { window.__rename_calls++; \
        const r = new Response('{\"error\":\"boom\"}', { status: 500, \
        headers: { 'Content-Type': 'application/json' } }); \
        Object.defineProperty(r, 'url', { value: 'http://test:8080/x' }); return Promise.resolve(r); };");

    let mount = create_mount_point();
    render_into(&mount, Harness);
    yield_now().await;

    open_editor(&mount).await;
    type_into_input(&mount, "Bob");
    yield_now().await;
    key_on_input(&mount, "Enter");
    blur_input(&mount);
    yield_now().await;
    sleep_ms(200).await;

    let calls = js_sys::eval("window.__rename_calls")
        .unwrap()
        .as_f64()
        .unwrap_or(-1.0);
    restore_fetch();
    assert_eq!(calls, 1.0, "Enter then blur must send exactly one rename");
    let input = find(&mount, INPUT).expect("editor stays open on failure");
    assert_eq!(input.get_attribute("aria-invalid").as_deref(), Some("true"));
    assert!(
        find(&mount, ERROR).is_some(),
        "failure is shown, not dropped"
    );
    assert!(
        !input
            .unchecked_ref::<web_sys::HtmlInputElement>()
            .read_only(),
        "editable again after the request settles"
    );
    assert!(RENAMED.with(|r| r.borrow().is_empty()));

    cleanup(&mount);
}

const OK_BODY: &str = r#"{"success":true,"result":{"user_id":"u","status":"admitted","is_host":false,"joined_at":0}}"#;

fn mock_pending_fetch() {
    js(
        "window.__rename_calls = 0; window.__rename_resolvers = []; \
        window.__original_fetch = window.__original_fetch || window.fetch; \
        window.fetch = function() { window.__rename_calls++; \
        return new Promise(function(res) { window.__rename_resolvers.push(res); }); };",
    );
}

fn resolve_pending_fetch(status: u16, body: &str) {
    js(&format!(
        "window.__rename_resolvers.forEach(function(res) {{ \
         const r = new Response({body:?}, {{ status: {status}, headers: {{ 'Content-Type': 'application/json' }} }}); \
         Object.defineProperty(r, 'url', {{ value: 'http://test:8080/x' }}); res(r); }}); \
         window.__rename_resolvers = [];"
    ));
}

fn rename_calls() -> f64 {
    js("window.__rename_calls").as_f64().unwrap_or(-1.0)
}

async fn submit_bob(mount: &web_sys::Element) {
    open_editor(mount).await;
    type_into_input(mount, "Bob");
    yield_now().await;
    key_on_input(mount, "Enter");
    yield_now().await;
}

#[allow(non_snake_case)]
fn UnmountHarness() -> Element {
    let mut show = use_signal(|| true);
    rsx! {
        button { "data-testid": "unmount-tile", onclick: move |_| show.set(false) }
        if show() {
            SelfTileName {
                display_name: "Alice".to_string(),
                meeting_id: "room-2794".to_string(),
                session_id: Some(7),
                on_renamed: move |name: String| RENAMED.with(|r| r.borrow_mut().push(name)),
            }
        }
    }
}

#[wasm_bindgen_test]
async fn successful_rename_reports_closes_announces_and_refocuses() {
    reset();
    inject_app_config();
    dioxus_ui::context::clear_display_name_from_storage();
    mock_pending_fetch();
    let mount = create_mount_point();
    render_into(&mount, Harness);
    yield_now().await;

    submit_bob(&mount).await;
    let input = input_in(&mount);
    assert!(
        input.read_only(),
        "read-only while the request is in flight"
    );
    assert_eq!(input.get_attribute("aria-busy").as_deref(), Some("true"));
    assert!(
        is_active(input.unchecked_ref()),
        "focus stays in the field while busy"
    );

    resolve_pending_fetch(200, OK_BODY);
    sleep_ms(100).await;
    yield_now().await;
    restore_fetch();

    assert_eq!(
        RENAMED.with(|r| r.borrow().clone()),
        vec!["Bob".to_string()]
    );
    assert!(find(&mount, INPUT).is_none(), "editor closes on success");
    let button = find(&mount, BUTTON).expect("name button back");
    assert!(is_active(&button), "focus returns to the name button");
    let status = find(&mount, STATUS).expect("status region");
    assert_eq!(
        status.text_content().unwrap_or_default(),
        "Display name updated"
    );
    assert_eq!(
        dioxus_ui::context::load_display_name_from_storage().as_deref(),
        Some("Bob")
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn unmount_mid_rename_still_persists_without_calling_back() {
    reset();
    inject_app_config();
    dioxus_ui::context::clear_display_name_from_storage();
    mock_pending_fetch();
    let mount = create_mount_point();
    render_into(&mount, UnmountHarness);
    yield_now().await;

    submit_bob(&mount).await;
    assert_eq!(rename_calls(), 1.0);
    PANICS.with(|c| c.set(0));
    let prev_hook = std::sync::Arc::new(std::panic::take_hook());
    let chained = prev_hook.clone();
    std::panic::set_hook(Box::new(move |info| {
        PANICS.with(|c| c.set(c.get() + 1));
        chained(info);
    }));
    find(&mount, "[data-testid='unmount-tile']")
        .expect("unmount button")
        .click();
    yield_now().await;
    assert!(find(&mount, INPUT).is_none(), "tile unmounted");

    resolve_pending_fetch(200, OK_BODY);
    sleep_ms(100).await;
    yield_now().await;
    restore_fetch();
    let _ = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| prev_hook(info)));

    assert_eq!(PANICS.with(|c| c.get()), 0, "no panic after unmount");
    assert!(
        RENAMED.with(|r| r.borrow().is_empty()),
        "no callback into an unmounted tile"
    );
    assert_eq!(
        dioxus_ui::context::load_display_name_from_storage().as_deref(),
        Some("Bob"),
        "the rename still completes and is saved"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn enter_and_escape_during_ime_composition_are_ignored() {
    reset();
    let mount = create_mount_point();
    render_into(&mount, Harness);
    yield_now().await;

    open_editor(&mount).await;
    type_into_input(&mount, "Bob<script>");
    yield_now().await;
    for key in ["Enter", "Escape"] {
        dispatch(
            &input_in(&mount),
            &format!(
                "new KeyboardEvent('keydown', {{ key: {key:?}, isComposing: true, bubbles: true, cancelable: true }})"
            ),
        );
        yield_now().await;
    }

    assert!(
        find(&mount, INPUT).is_some(),
        "composition keys neither commit nor cancel"
    );
    assert!(find(&mount, ERROR).is_none(), "no validation ran");

    dispatch(
        &input_in(&mount),
        "new KeyboardEvent('keydown', { key: 'Enter', keyCode: 229, bubbles: true, cancelable: true })",
    );
    yield_now().await;
    assert!(
        find(&mount, ERROR).is_none(),
        "Safari's keyCode-229 confirm Enter is still composition"
    );

    cleanup(&mount);
}

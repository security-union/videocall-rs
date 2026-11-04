// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue 2794: the self tile's click-to-rename name chip.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::collections::HashSet;

use dioxus::prelude::*;
use dioxus_ui::components::display_name_edit::SelfTileName;
use dioxus_ui::context::HostSetCtx;
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
                local_user_id: LOCAL.to_string(),
                on_renamed: move |name: String| RENAMED.with(|r| r.borrow_mut().push(name)),
            }
        }
    }
}

const LOCAL: &str = "alice@example.com";
const OWNER: &str = "owner@example.com";

thread_local! {
    static HOSTS: std::cell::RefCell<Option<Signal<HashSet<String>>>> = const { std::cell::RefCell::new(None) };
}

#[allow(non_snake_case)]
fn RoleHarness() -> Element {
    let hosts = use_signal(|| HashSet::from([LOCAL.to_string(), OWNER.to_string()]));
    use_context_provider(|| HostSetCtx(hosts));
    use_hook(move || HOSTS.with(|h| *h.borrow_mut() = Some(hosts)));
    rsx! {
        SelfTileName {
            display_name: "Alice".to_string(),
            meeting_id: "room-2794".to_string(),
            session_id: Some(7),
            local_user_id: LOCAL.to_string(),
            owner_user_id: Some(OWNER.to_string()),
            on_renamed: move |_: String| {},
        }
    }
}

thread_local! {
    static TILE_WIDTH: std::cell::RefCell<Option<Signal<u32>>> = const { std::cell::RefCell::new(None) };
    static TILE_NAME: std::cell::Cell<&'static str> = const { std::cell::Cell::new("") };
    static TILE_SCALED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

#[allow(non_snake_case)]
fn SizedRoleHarness() -> Element {
    let width = use_signal(|| 1000u32);
    let hosts = use_signal(|| HashSet::from([LOCAL.to_string(), OWNER.to_string()]));
    use_context_provider(|| HostSetCtx(hosts));
    use_hook(move || TILE_WIDTH.with(|w| *w.borrow_mut() = Some(width)));
    let height = width() * 2 / 3;
    let tile_vars = if TILE_SCALED.with(std::cell::Cell::get) {
        format!("--tile-w: {width}px; --tile-h: {height}px;")
    } else {
        String::new()
    };
    rsx! {
        div { style: "position: relative; width: {width}px; height: {height}px; {tile_vars}",
            SelfTileName {
                display_name: TILE_NAME.with(std::cell::Cell::get).to_string(),
                meeting_id: "room-2794".to_string(),
                session_id: Some(7),
                local_user_id: LOCAL.to_string(),
                owner_user_id: Some(OWNER.to_string()),
                on_renamed: move |_: String| {},
            }
        }
    }
}

/// The app's styles as a phone (`touch`) or a mouse sees them, whatever input
/// the test browser itself reports: each hover/pointer media feature is
/// rewritten to an always-true or never-true width query.
fn inject_styles(touch: bool) -> web_sys::Element {
    const ALWAYS: &str = "(min-width: 0px)";
    const NEVER: &str = "(max-width: 0px)";
    let (hover_none, hover_hover, coarse) = if touch {
        (ALWAYS, NEVER, ALWAYS)
    } else {
        (NEVER, ALWAYS, NEVER)
    };
    let css = concat!(
        include_str!("../static/style.css"),
        include_str!("../static/global.css"),
    )
    .replace("(hover: none)", hover_none)
    .replace("(hover: hover)", hover_hover)
    .replace("(pointer: coarse)", coarse)
    .replace("(any-pointer: coarse)", coarse);
    for feature in ["(hover:", "(pointer:", "any-hover", "any-pointer"] {
        assert!(
            !css.contains(feature),
            "{feature} would still follow the test browser's own input"
        );
    }
    let doc = gloo_utils::document();
    let style = doc.create_element("style").unwrap();
    style.set_text_content(Some(&css));
    doc.head().unwrap().append_child(&style).unwrap();
    style
}

fn overflows(el: &web_sys::HtmlElement) -> bool {
    el.scroll_width() > el.client_width() + 1
}

fn computed_px(el: &web_sys::HtmlElement, property: &str) -> f64 {
    gloo_utils::window()
        .get_computed_style(el)
        .unwrap()
        .expect("computed style")
        .get_property_value(property)
        .unwrap_or_default()
        .trim_end_matches("px")
        .parse()
        .unwrap_or(0.0)
}

/// Mounts the self tile at 1000px wide, named `name`; `scaled` sizes the chip
/// from the tile like the meeting grid does.
async fn mount_sized_tile(name: &'static str, scaled: bool) -> (web_sys::Element, Signal<u32>) {
    TILE_NAME.with(|n| n.set(name));
    TILE_SCALED.with(|s| s.set(scaled));
    let mount = create_mount_point();
    render_into(&mount, SizedRoleHarness);
    yield_now().await;
    (mount, TILE_WIDTH.with(|w| w.borrow().expect("tile width")))
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
                local_user_id: LOCAL.to_string(),
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

#[wasm_bindgen_test]
async fn the_you_pill_names_the_local_host_role() {
    let mount = create_mount_point();
    render_into(&mount, RoleHarness);
    yield_now().await;
    let mut hosts = HOSTS.with(|h| h.borrow().expect("role harness hosts"));
    let pill = || find(&mount, ".self-indicator").expect("you pill");
    let separator = || pill().query_selector("[aria-hidden='true']").ok().flatten();

    assert_eq!(pill().text_content().as_deref(), Some("You · Co-host"));
    assert_eq!(pill().get_attribute("dir").as_deref(), Some("ltr"));
    assert_eq!(
        separator().and_then(|s| s.text_content()).as_deref(),
        Some("·")
    );

    hosts.write().remove(OWNER);
    yield_now().await;
    assert_eq!(pill().text_content().as_deref(), Some("You · Host"));

    hosts.write().remove(LOCAL);
    yield_now().await;
    assert_eq!(pill().text_content().as_deref(), Some("You"));
    assert!(separator().is_none());
    let button = find(&mount, BUTTON).expect("name button");
    assert_eq!(
        button.get_attribute("aria-label").as_deref(),
        Some("Edit display name, Alice")
    );

    cleanup(&mount);
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Pointer {
    Mouse,
    MouseFocused,
    Touch,
}

async fn sweep_narrowing_self_tile(name: &'static str, pointer: Pointer) {
    let style = inject_styles(pointer == Pointer::Touch);
    let (mount, mut width) = mount_sized_tile(name, true).await;
    let chip = || find(&mount, "h4.self-tile-name").expect("chip");
    if pointer == Pointer::MouseFocused {
        let button = find(&mount, BUTTON).expect("name button");
        button.focus().unwrap();
        yield_now().await;
        assert!(
            button.matches(":focus-visible").unwrap(),
            "focusing the name button did not make it :focus-visible"
        );
    }
    let natural = chip().get_bounding_client_rect().width();
    let (mut name_alone_cut, mut role_cut) = (false, false);
    for w in (70..=1000).rev().step_by(10) {
        width.set(w);
        yield_now().await;
        let at = format!("{name:?} {pointer:?} {w}px");
        let chip = chip();
        let pill = find(&mount, ".self-indicator").expect("pill");
        let role = find(&mount, ".self-indicator-role").expect("role");
        let text = find(&mount, ".floating-name-text").expect("name");
        let icon = mount
            .query_selector(".self-tile-name-edit-icon")
            .ok()
            .flatten()
            .expect("edit icon");
        let (chip_box, pill_box) = (
            chip.get_bounding_client_rect(),
            pill.get_bounding_client_rect(),
        );
        assert_eq!(pill.text_content().as_deref(), Some("You · Co-host"));
        assert!(
            pill_box.right() <= chip_box.right() + 0.5,
            "{at}: the chip clips the pill"
        );
        assert!(!overflows(&pill), "{at}: \"You\" overflows the pill");
        let icon_box = icon.get_bounding_client_rect();
        let icon_shown = icon_box.width() > 0.0;
        assert_eq!(
            icon_shown,
            pointer != Pointer::Mouse,
            "{at}: edit icon shown"
        );
        let mut name_end = text.get_bounding_client_rect().right();
        if icon_shown {
            name_end = name_end.max(icon_box.right());
        }
        let blank = pill_box.left() - name_end - computed_px(&chip, "column-gap");
        assert!(blank <= 1.0, "{at}: {blank}px blank before the pill");
        assert!(
            blank >= -1.0,
            "{at}: the name overlaps the pill by {}px",
            -blank
        );
        if icon_shown {
            let button = find(&mount, BUTTON).expect("name button");
            assert!(
                icon_box.right() <= button.get_bounding_client_rect().right() + 0.5,
                "{at}: the edit icon spills out of the name button"
            );
        }
        if chip_box.width() >= natural - 0.5 {
            assert!(
                !overflows(&text) && !overflows(&role),
                "{at}: truncated though the chip fits"
            );
        }
        if overflows(&role) {
            role_cut = true;
            let kept = text.get_bounding_client_rect().width();
            assert!(
                kept < 1.0,
                "{at}: the role is cut while the name keeps {kept}px"
            );
        } else if overflows(&text) {
            name_alone_cut = true;
        }
    }
    assert!(
        name_alone_cut,
        "{name:?} {pointer:?}: no width cut the name alone"
    );
    assert!(role_cut, "{name:?} {pointer:?}: no width reached the role");
    style.remove();
    cleanup(&mount);
}

async fn sweep_every_pointer(name: &'static str) {
    for pointer in [Pointer::Mouse, Pointer::MouseFocused, Pointer::Touch] {
        sweep_narrowing_self_tile(name, pointer).await;
    }
}

#[wasm_bindgen_test]
async fn a_short_name_gives_way_before_the_role_and_you_stays_whole() {
    sweep_every_pointer("Jo").await;
}

#[wasm_bindgen_test]
async fn a_medium_name_gives_way_before_the_role_and_you_stays_whole() {
    sweep_every_pointer("Alice").await;
}

#[wasm_bindgen_test]
async fn a_long_name_gives_way_before_the_role_and_you_stays_whole() {
    sweep_every_pointer("Alexandra Whitfield").await;
}

#[wasm_bindgen_test]
async fn while_renaming_the_input_fills_all_the_room_beside_you() {
    let style = inject_styles(false);
    let (mount, mut width) = mount_sized_tile("Alice", false).await;
    let natural = open_editor(&mount).await.get_bounding_client_rect().width();
    for w in (100..=1000).rev().step_by(10) {
        width.set(w);
        yield_now().await;
        let at = format!("editing {w}px");
        let chip = find(&mount, "h4.self-tile-name").expect("chip");
        let pill = find(&mount, ".self-indicator").expect("pill");
        let input = find(&mount, INPUT).expect("still editing");
        let role = find(&mount, ".self-indicator-role").expect("role");
        let (pill_box, input_box) = (
            pill.get_bounding_client_rect(),
            input.get_bounding_client_rect(),
        );
        let you_pill = pill_box.width() - role.get_bounding_client_rect().width();
        assert!(
            input_box.right() <= pill_box.left() + 0.5,
            "{at}: the input runs under the pill"
        );
        assert!(
            pill_box.right() <= chip.get_bounding_client_rect().right() + 0.5,
            "{at}: the chip clips the pill"
        );
        let inner = f64::from(chip.client_width())
            - computed_px(&chip, "padding-left")
            - computed_px(&chip, "padding-right");
        let room = natural.min(inner - computed_px(&chip, "column-gap") - you_pill);
        assert!(
            input_box.width() >= room - 1.0,
            "{at}: the input gets {}px of the {room}px beside \"You\"",
            input_box.width()
        );
    }
    style.remove();
    cleanup(&mount);
}

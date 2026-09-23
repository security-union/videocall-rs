// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue 2793 (+ issue 1955): the speaker-highlight palette and its preview,
// rendered through the real `AppearanceSettingsPanel` in a real browser.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::Cell;

use dioxus::prelude::*;
use dioxus_ui::components::appearance_settings_panel::AppearanceSettingsPanel;
use dioxus_ui::context::{
    AppearanceSettings, AppearanceSettingsCtx, GlowColor, LocalAudioLevelCtx, LocalSpeakingCtx,
    Theme, ThemePreferenceCtx,
};
use support::{cleanup, create_mount_point, render_into, yield_now};
use videocall_diagnostics::{global_sender, metric, DiagEvent};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const PALETTE_KEY: &str = "vc_appearance_glow_palette";
const LEGACY_CUSTOM_KEY: &str = "vc_appearance_custom_colors";
/// Longer than the panel's 400 ms double-delete guard.
const PAST_DELETE_GUARD_MS: i32 = 450;

thread_local! {
    static SEED: Cell<AppearanceSettings> = Cell::new(AppearanceSettings::default());
}

#[allow(non_snake_case)]
fn Panel() -> Element {
    let theme = use_signal(|| Theme::Dark);
    use_context_provider(|| ThemePreferenceCtx(theme));
    let appearance = use_signal(|| SEED.with(Cell::get));
    use_context_provider(|| AppearanceSettingsCtx(appearance));
    let mut level = use_signal(|| 0.0f32);
    let mut speaking = use_signal(|| false);
    use_context_provider(|| LocalAudioLevelCtx(level));
    use_context_provider(|| LocalSpeakingCtx(speaking));
    rsx! {
        AppearanceSettingsPanel {}
        button {
            "data-testid": "mic-speech",
            onclick: move |_| {
                level.set(0.6);
                speaking.set(true);
            },
        }
        // What the microphone encoder emits on mute: VAD off, level zeroed.
        button {
            "data-testid": "mic-mute",
            onclick: move |_| {
                speaking.set(false);
                level.set(0.0);
            },
        }
    }
}

fn storage() -> web_sys::Storage {
    web_sys::window().unwrap().local_storage().unwrap().unwrap()
}

fn reset_storage() {
    let _ = storage().remove_item(PALETTE_KEY);
    let _ = storage().remove_item(LEGACY_CUSTOM_KEY);
}

async fn mount_panel(settings: AppearanceSettings) -> web_sys::Element {
    SEED.with(|s| s.set(settings));
    let mount = create_mount_point();
    render_into(&mount, Panel);
    yield_now().await;
    mount
}

async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .unwrap();
    });
    JsFuture::from(promise).await.unwrap();
}

fn all(root: &web_sys::Element, selector: &str) -> Vec<web_sys::HtmlElement> {
    let nodes = root.query_selector_all(selector).unwrap();
    (0..nodes.length())
        .filter_map(|i| nodes.item(i))
        .map(|n| n.dyn_into::<web_sys::HtmlElement>().unwrap())
        .collect()
}

fn one(root: &web_sys::Element, selector: &str) -> web_sys::HtmlElement {
    root.query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("no element matches {selector}"))
        .dyn_into()
        .unwrap()
}

fn select_labels(root: &web_sys::Element) -> Vec<String> {
    all(root, ".color-swatch-item > .color-swatch")
        .iter()
        .map(|b| b.get_attribute("aria-label").unwrap_or_default())
        .collect()
}

fn pressed_label(root: &web_sys::Element) -> Option<String> {
    root.query_selector(".color-swatch[aria-pressed='true']")
        .unwrap()
        .and_then(|el| el.get_attribute("aria-label"))
}

async fn click(root: &web_sys::Element, selector: &str) {
    one(root, selector).click();
    yield_now().await;
}

fn preview(root: &web_sys::Element) -> web_sys::HtmlElement {
    one(root, ".speaker-highlight-preview .preview-tile")
}

fn caption(root: &web_sys::Element) -> String {
    one(root, "[data-testid='speaker-highlight-preview-caption']")
        .text_content()
        .unwrap_or_default()
}

async fn wait_for_caption(root: &web_sys::Element, want: &str, timeout_ms: i32) -> bool {
    let mut waited = 0;
    while waited <= timeout_ms {
        if caption(root) == want {
            return true;
        }
        sleep_ms(50).await;
        waited += 50;
    }
    false
}

fn key_down(target: &web_sys::HtmlElement, key: &str, repeat: bool) {
    js_sys::Function::new_with_args(
        "el, key, repeat",
        "el.dispatchEvent(new KeyboardEvent('keydown', {key, repeat, bubbles: true, cancelable: true}));",
    )
    .call3(
        &wasm_bindgen::JsValue::NULL,
        target,
        &key.into(),
        &repeat.into(),
    )
    .unwrap();
}

fn focused_label() -> Option<String> {
    gloo_utils::document()
        .active_element()
        .and_then(|el| el.get_attribute("aria-label"))
}

fn preview_source(root: &web_sys::Element) -> String {
    preview(root)
        .get_attribute("data-preview-source")
        .unwrap_or_default()
}

async fn wait_for_source(root: &web_sys::Element, want: &str, timeout_ms: i32) -> bool {
    let mut waited = 0;
    while waited <= timeout_ms {
        if preview_source(root) == want {
            return true;
        }
        sleep_ms(50).await;
        waited += 50;
    }
    false
}

async fn wait_for_lit(root: &web_sys::Element, timeout_ms: i32) -> bool {
    let mut waited = 0;
    while waited <= timeout_ms {
        if preview(root)
            .class_list()
            .contains("preview-tile--speaking")
        {
            return true;
        }
        sleep_ms(50).await;
        waited += 50;
    }
    false
}

fn add_sheet(css: &str) -> web_sys::Element {
    let doc = gloo_utils::document();
    let style = doc.create_element("style").unwrap();
    style.set_text_content(Some(css));
    doc.head().unwrap().append_child(&style).unwrap();
    style
}

fn install_stylesheets() -> web_sys::Element {
    add_sheet(concat!(
        include_str!("../static/style.css"),
        include_str!("../static/global.css"),
    ))
}

fn channels(color: GlowColor) -> String {
    let (r, g, b) = color.to_rgb();
    format!("{r}, {g}, {b}")
}

fn peer_speaking(peer: &str, speaking: u64, level: f64) -> DiagEvent {
    DiagEvent {
        subsystem: "peer_speaking",
        stream_id: Some(format!("speaking->{peer}")),
        ts_ms: 0,
        metrics: vec![
            metric!("to_peer", peer.to_string()),
            metric!("speaking", speaking),
            metric!("audio_level", level),
        ],
    }
}

#[wasm_bindgen_test]
async fn every_swatch_is_a_button_with_a_sibling_delete_and_nothing_nests() {
    reset_storage();
    let mount = mount_panel(AppearanceSettings::default()).await;

    let items = all(&mount, ".color-swatch-item");
    assert_eq!(items.len(), 5, "the five presets render as palette items");
    for item in &items {
        let children = item.children();
        assert_eq!(children.length(), 2, "{}", item.outer_html());
        let select = children.item(0).unwrap();
        let delete = children.item(1).unwrap();
        assert_eq!(select.tag_name(), "BUTTON");
        assert!(select.class_list().contains("color-swatch"));
        assert_eq!(delete.tag_name(), "BUTTON");
        assert!(delete.class_list().contains("color-swatch-delete-btn"));
        assert!(!item.has_attribute("role") && !item.has_attribute("tabindex"));
        assert_eq!(
            select.get_attribute("aria-keyshortcuts").as_deref(),
            Some("Delete Backspace")
        );
    }
    let nested = all(
        &mount,
        "#color-swatches-container :is(button, [role='button'], [tabindex]) :is(button, [role='button'], [tabindex])",
    );
    assert!(nested.is_empty(), "issue 1955: {} nested", nested.len());
    for name in ["White", "Cyan", "Magenta", "Plum", "Mint Green"] {
        one(&mount, &format!("[aria-label='Delete {name} highlight']"));
    }

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_swatch_centre_selects_even_while_its_delete_badge_is_shown() {
    reset_storage();
    let sheet = install_stylesheets();
    let reveal = add_sheet(
        ".color-swatch-delete-btn { opacity: 1 !important; pointer-events: auto !important; }",
    );
    let mount = mount_panel(AppearanceSettings::default()).await;
    let doc = gloo_utils::document();

    let items = all(&mount, ".color-swatch-item");
    assert_eq!(items.len(), 5);
    for item in &items {
        let select = one(item, ".color-swatch");
        let delete = one(item, ".color-swatch-delete-btn");
        select.scroll_into_view_with_bool(false);
        let s = select.get_bounding_client_rect();
        let centre = doc
            .element_from_point(
                (s.left() + s.width() / 2.0) as f32,
                (s.top() + s.height() / 2.0) as f32,
            )
            .expect("swatch centre is on screen");
        assert!(
            centre.is_same_node(Some(select.as_ref())),
            "the delete badge covers the swatch centre: hit <{}>",
            centre.outer_html()
        );
        let d = delete.get_bounding_client_rect();
        let badge = doc
            .element_from_point(
                (d.left() + d.width() / 2.0) as f32,
                (d.top() + d.height() / 2.0) as f32,
            )
            .expect("badge centre is on screen");
        assert!(
            delete.contains(Some(badge.as_ref())),
            "hit <{}>",
            badge.outer_html()
        );
    }

    cleanup(&mount);
    reveal.remove();
    sheet.remove();
}

#[wasm_bindgen_test]
async fn the_selected_swatch_alone_wears_a_ring() {
    reset_storage();
    let sheet = install_stylesheets();
    let mount = mount_panel(AppearanceSettings::default()).await;
    let window = web_sys::window().unwrap();

    for button in all(&mount, ".color-swatch-item > .color-swatch") {
        let ring = window
            .get_computed_style_with_pseudo_elt(&button, "::after")
            .unwrap()
            .unwrap();
        let selected = button.get_attribute("aria-pressed").as_deref() == Some("true");
        let opacity = ring.get_property_value("opacity").unwrap();
        assert_ne!(ring.get_property_value("content").unwrap(), "none");
        assert_eq!(ring.get_property_value("border-top-width").unwrap(), "2px");
        assert_eq!(opacity, if selected { "1" } else { "0" });
    }
    assert_eq!(
        pressed_label(&mount).as_deref(),
        Some("Select Mint Green highlight")
    );

    cleanup(&mount);
    sheet.remove();
}

#[wasm_bindgen_test]
async fn deleting_the_selected_preset_selects_the_swatch_that_slides_in() {
    reset_storage();
    let mount = mount_panel(AppearanceSettings::default()).await;

    click(&mount, "[aria-label='Select Cyan highlight']").await;
    assert_eq!(
        pressed_label(&mount).as_deref(),
        Some("Select Cyan highlight")
    );
    click(&mount, "[aria-label='Delete Cyan highlight']").await;

    assert_eq!(
        select_labels(&mount),
        [
            "Select White highlight",
            "Select Magenta highlight",
            "Select Plum highlight",
            "Select Mint Green highlight",
        ]
    );
    assert_eq!(
        pressed_label(&mount).as_deref(),
        Some("Select Magenta highlight")
    );
    assert_eq!(
        storage().get_item(PALETTE_KEY).unwrap().as_deref(),
        Some("white,magenta,plum,mint-green")
    );

    cleanup(&mount);
    reset_storage();
}

#[wasm_bindgen_test]
async fn an_emptied_palette_shows_only_the_add_button_and_survives_a_remount() {
    reset_storage();
    let mount = mount_panel(AppearanceSettings {
        glow_decay: 0.0,
        ..AppearanceSettings::default()
    })
    .await;

    while mount
        .query_selector(".color-swatch-delete-btn")
        .unwrap()
        .is_some()
    {
        click(&mount, ".color-swatch-delete-btn").await;
        sleep_ms(PAST_DELETE_GUARD_MS).await;
    }
    let container = one(&mount, "#color-swatches-container");
    assert_eq!(container.children().length(), 1);
    one(&container, "#add-custom-color-btn");
    assert_eq!(
        storage().get_item(PALETTE_KEY).unwrap().as_deref(),
        Some("")
    );
    assert!(wait_for_lit(&mount, 2000).await);
    assert!(
        preview(&mount)
            .get_attribute("style")
            .unwrap_or_default()
            .contains(&channels(GlowColor::MintGreen)),
        "the glow keeps its colour once the last swatch is gone"
    );
    cleanup(&mount);

    let remounted = mount_panel(AppearanceSettings::default()).await;
    assert!(select_labels(&remounted).is_empty());
    one(&remounted, "#add-custom-color-btn");

    cleanup(&remounted);
    reset_storage();
}

#[wasm_bindgen_test]
async fn reset_restores_the_five_presets_and_drops_custom_colors() {
    reset_storage();
    storage().set_item(LEGACY_CUSTOM_KEY, "ff5733").unwrap();
    let mount = mount_panel(AppearanceSettings::default()).await;
    assert_eq!(select_labels(&mount).len(), 6, "legacy customs migrate");

    click(&mount, "[aria-label='Delete White highlight']").await;
    click(&mount, "[data-testid='speaker-highlight-reset-btn']").await;

    assert_eq!(
        select_labels(&mount),
        [
            "Select White highlight",
            "Select Cyan highlight",
            "Select Magenta highlight",
            "Select Plum highlight",
            "Select Mint Green highlight",
        ]
    );
    assert_eq!(
        pressed_label(&mount).as_deref(),
        Some("Select Mint Green highlight")
    );
    assert_eq!(
        storage().get_item(PALETTE_KEY).unwrap().as_deref(),
        Some("white,cyan,magenta,plum,mint-green")
    );

    cleanup(&mount);
    reset_storage();
}

#[wasm_bindgen_test]
async fn the_preview_glows_in_the_chosen_colour_under_the_real_stylesheets() {
    reset_storage();
    let sheet = install_stylesheets();
    let frozen = add_sheet(".preview-tile { transition: none !important; }");
    let mount = mount_panel(AppearanceSettings {
        glow_color: GlowColor::Cyan,
        ..AppearanceSettings::default()
    })
    .await;

    let tile = preview(&mount);
    assert_eq!(preview_source(&mount), "simulated");
    assert_eq!(caption(&mount), "Preview: simulated speaker");
    assert!(tile.class_list().contains("preview-tile--speaking"));
    let computed = web_sys::window()
        .unwrap()
        .get_computed_style(&tile)
        .unwrap()
        .unwrap();
    let shadow = computed.get_property_value("box-shadow").unwrap();
    let border = computed.get_property_value("border-top-color").unwrap();
    assert!(shadow.contains(&channels(GlowColor::Cyan)), "{shadow}");
    assert!(border.contains(&channels(GlowColor::Cyan)), "{border}");

    cleanup(&mount);
    frozen.remove();
    sheet.remove();
}

#[wasm_bindgen_test]
async fn the_preview_follows_the_mic_and_goes_dark_on_mute() {
    reset_storage();
    // 0.1 decay: a ~2.1 s silent tail, long enough to observe before the
    // simulation takes back over.
    let mount = mount_panel(AppearanceSettings {
        glow_decay: 0.1,
        ..AppearanceSettings::default()
    })
    .await;

    click(&mount, "[data-testid='mic-speech']").await;
    assert!(wait_for_source(&mount, "mic", 500).await);
    let tile = preview(&mount);
    assert!(tile.class_list().contains("preview-tile--speaking"));
    assert!(!tile.class_list().contains("preview-tile-pulsing"));
    assert!(wait_for_caption(&mount, "Preview: your microphone", 300).await);

    click(&mount, "[data-testid='mic-mute']").await;
    sleep_ms(300).await;
    assert_eq!(preview_source(&mount), "mic");
    assert!(preview(&mount)
        .class_list()
        .contains("preview-tile--silent"));
    assert!(wait_for_source(&mount, "simulated", 3000).await);
    assert_eq!(caption(&mount), "Preview: your microphone");
    assert!(wait_for_caption(&mount, "Preview: simulated speaker", 3000).await);

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_preview_lights_for_remote_speech_and_releases_it() {
    reset_storage();
    let mount = mount_panel(AppearanceSettings {
        glow_decay: 0.0,
        ..AppearanceSettings::default()
    })
    .await;

    global_sender()
        .try_broadcast(peer_speaking("bob", 1, 0.7))
        .unwrap();
    assert!(wait_for_source(&mount, "remote", 500).await);
    assert!(preview(&mount)
        .get_attribute("style")
        .unwrap_or_default()
        .contains(&channels(GlowColor::MintGreen)));
    assert!(wait_for_caption(&mount, "Preview: someone speaking", 300).await);

    global_sender()
        .try_broadcast(peer_speaking("bob", 0, 0.0))
        .unwrap();
    assert!(wait_for_source(&mount, "simulated", 1500).await);

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn one_delete_or_backspace_press_removes_exactly_one_swatch() {
    reset_storage();
    let mount = mount_panel(AppearanceSettings::default()).await;

    let white = one(&mount, "[aria-label='Select White highlight']");
    white.focus().unwrap();
    key_down(&white, "Delete", false);
    yield_now().await;
    sleep_ms(120).await;
    assert_eq!(
        select_labels(&mount),
        [
            "Select Cyan highlight",
            "Select Magenta highlight",
            "Select Plum highlight",
            "Select Mint Green highlight",
        ]
    );
    assert_eq!(focused_label().as_deref(), Some("Select Cyan highlight"));

    sleep_ms(PAST_DELETE_GUARD_MS).await;
    let cyan = one(&mount, "[aria-label='Select Cyan highlight']");
    key_down(&cyan, "Backspace", false);
    yield_now().await;
    sleep_ms(120).await;
    assert_eq!(
        select_labels(&mount),
        [
            "Select Magenta highlight",
            "Select Plum highlight",
            "Select Mint Green highlight",
        ]
    );

    cleanup(&mount);
    reset_storage();
}

#[wasm_bindgen_test]
async fn an_auto_repeat_keydown_deletes_nothing() {
    reset_storage();
    let mount = mount_panel(AppearanceSettings::default()).await;

    for key in ["Delete", "Backspace"] {
        let white = one(&mount, "[aria-label='Select White highlight']");
        white.focus().unwrap();
        key_down(&white, key, true);
        yield_now().await;
        sleep_ms(120).await;
        assert_eq!(select_labels(&mount).len(), 5, "a held {key} deleted");
    }
    assert_eq!(storage().get_item(PALETTE_KEY).unwrap(), None);

    cleanup(&mount);
    reset_storage();
}

#[wasm_bindgen_test]
async fn a_double_tap_on_the_delete_badge_removes_one_color() {
    reset_storage();
    let mount = mount_panel(AppearanceSettings::default()).await;

    click(&mount, "[aria-label='Delete Mint Green highlight']").await;
    sleep_ms(100).await;
    assert_eq!(
        pressed_label(&mount).as_deref(),
        Some("Select Plum highlight"),
        "the neighbour that slid in is now selected, with its badge under the finger"
    );
    click(&mount, "[aria-label='Delete Plum highlight']").await;
    assert_eq!(select_labels(&mount).len(), 4);

    sleep_ms(PAST_DELETE_GUARD_MS).await;
    click(&mount, "[aria-label='Delete Plum highlight']").await;
    assert_eq!(select_labels(&mount).len(), 3);

    cleanup(&mount);
    reset_storage();
}

#[wasm_bindgen_test]
async fn a_hidden_delete_badge_takes_no_clicks() {
    reset_storage();
    let sheet = install_stylesheets();
    let mount = mount_panel(AppearanceSettings::default()).await;
    let doc = gloo_utils::document();

    let items = all(&mount, ".color-swatch-item");
    assert_eq!(items.len(), 5);
    // The selected swatch (Mint Green, last) shows its badge on touch screens.
    for item in &items[..4] {
        let select = one(item, ".color-swatch");
        let delete = one(item, ".color-swatch-delete-btn");
        select.scroll_into_view_with_bool(false);
        let d = delete.get_bounding_client_rect();
        let (badge_x, badge_y) = (d.left() + d.width() / 2.0, d.top() + d.height() / 2.0);
        let at_badge = doc.element_from_point(badge_x as f32, badge_y as f32);
        assert!(
            at_badge
                .as_ref()
                .is_none_or(|hit| !delete.contains(Some(hit.as_ref()))),
            "a hidden badge took the click at its centre"
        );

        let s = select.get_bounding_client_rect();
        let (lens_x, lens_y) = (s.left() + s.width() * 0.8, s.top() + s.height() * 0.2);
        assert!(
            (lens_x - badge_x).hypot(lens_y - badge_y) < d.width() / 2.0,
            "the probe point must sit where the badge overlaps the swatch"
        );
        let lens = doc
            .element_from_point(lens_x as f32, lens_y as f32)
            .expect("swatch rim is on screen");
        assert!(
            lens.is_same_node(Some(select.as_ref())),
            "a hidden badge took the swatch rim: hit <{}>",
            lens.outer_html()
        );
    }

    cleanup(&mount);
    sheet.remove();
}

async fn add_color(root: &web_sys::Element, hex: &str) {
    click(root, "#add-custom-color-btn").await;
    let input: web_sys::HtmlInputElement = one(root, "[aria-label='Hex color value']")
        .dyn_into()
        .unwrap();
    input.set_value(hex);
    js_sys::Function::new_with_args(
        "el",
        "el.dispatchEvent(new Event('input', {bubbles: true}));",
    )
    .call1(&wasm_bindgen::JsValue::NULL, &input)
    .unwrap();
    yield_now().await;
    click(root, ".custom-color-add-btn").await;
}

fn announcement(root: &web_sys::Element) -> String {
    one(root, "[data-testid='speaker-highlight-palette-status']")
        .text_content()
        .unwrap_or_default()
        .trim_end_matches('\u{a0}')
        .to_string()
}

#[wasm_bindgen_test]
async fn adding_a_color_is_announced() {
    reset_storage();
    let mount = mount_panel(AppearanceSettings::default()).await;

    add_color(&mount, "#ABCDEF").await;
    assert_eq!(announcement(&mount), "#ABCDEF highlight added.");
    assert_eq!(
        pressed_label(&mount).as_deref(),
        Some("Select custom highlight #ABCDEF")
    );

    add_color(&mount, "#0CAFFF").await;
    assert_eq!(announcement(&mount), "Cyan highlight selected.");
    assert_eq!(select_labels(&mount).len(), 6);

    cleanup(&mount);
    reset_storage();
}

#[wasm_bindgen_test]
async fn two_deliberate_delete_presses_delete_two_swatches() {
    reset_storage();
    let mount = mount_panel(AppearanceSettings::default()).await;

    let white = one(&mount, "[aria-label='Select White highlight']");
    white.focus().unwrap();
    key_down(&white, "Delete", false);
    yield_now().await;
    sleep_ms(100).await;
    assert_eq!(focused_label().as_deref(), Some("Select Cyan highlight"));
    let cyan = one(&mount, "[aria-label='Select Cyan highlight']");
    key_down(&cyan, "Delete", false);
    yield_now().await;
    sleep_ms(100).await;

    assert_eq!(
        select_labels(&mount),
        [
            "Select Magenta highlight",
            "Select Plum highlight",
            "Select Mint Green highlight",
        ]
    );

    cleanup(&mount);
    reset_storage();
}

#[wasm_bindgen_test]
async fn the_caption_stays_truthful_while_the_highlight_is_off() {
    reset_storage();
    let mount = mount_panel(AppearanceSettings {
        glow_enabled: false,
        ..AppearanceSettings::default()
    })
    .await;

    click(&mount, "[data-testid='mic-speech']").await;
    assert!(wait_for_caption(&mount, "Preview: your microphone", 500).await);

    cleanup(&mount);
}

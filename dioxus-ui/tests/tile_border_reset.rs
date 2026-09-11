// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue 2660: a peer tile stayed lit after the speaker went silent. Only a real
// Dioxus attribute diff in a real browser reaches the interpreter's restore loop.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::RefCell;

use dioxus::prelude::*;
use dioxus_ui::components::canvas_generator::speak_style;
use dioxus_ui::context::AppearanceSettings;
use support::{cleanup, create_mount_point, render_into, yield_now};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const TILE: &str = "[data-testid='tile']";
const SILENCE_BUTTON: &str = "[data-testid='silence']";

const BORDER_TOKEN: &str = "rgb(1, 2, 3)";

thread_local! {
    static LIT_STYLE: RefCell<String> = const { RefCell::new(String::new()) };
    static SILENT_STYLE: RefCell<String> = const { RefCell::new(String::new()) };
}

#[allow(non_snake_case)]
fn TileUnderTest() -> Element {
    let mut silent = use_signal(|| false);
    let style = if silent() {
        SILENT_STYLE.with(|s| s.borrow().clone())
    } else {
        LIT_STYLE.with(|s| s.borrow().clone())
    };
    rsx! {
        div { class: "grid-item", "data-testid": "tile", style: "{style}" }
        button { "data-testid": "silence", onclick: move |_| silent.set(true) }
    }
}

fn seed(lit: String, silent: String) {
    LIT_STYLE.with(|s| *s.borrow_mut() = lit);
    SILENT_STYLE.with(|s| *s.borrow_mut() = silent);
}

/// `speak_style` fades the border over 0.30s, and a computed value read
/// mid-transition is the interpolated colour, not the declared one.
fn suppress_transitions() -> web_sys::Element {
    let document = gloo_utils::document();
    let style = document.create_element("style").unwrap();
    style.set_text_content(Some(
        "[data-testid='tile'] { transition: none !important; }",
    ));
    document.head().unwrap().append_child(&style).unwrap();
    style
}

fn computed(tile: &web_sys::Element, property: &str) -> String {
    gloo_utils::window()
        .get_computed_style(tile)
        .unwrap()
        .unwrap()
        .get_property_value(property)
        .unwrap()
}

fn tile_of(mount: &web_sys::Element) -> web_sys::Element {
    mount
        .query_selector(TILE)
        .unwrap()
        .expect("the tile under test must render")
}

async fn go_silent(mount: &web_sys::Element) {
    mount
        .query_selector(SILENCE_BUTTON)
        .unwrap()
        .expect("the silence button must render")
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap()
        .click();
    yield_now().await;
    yield_now().await;
}

async fn assert_reset_to_token(mount: &web_sys::Element, settings: &AppearanceSettings) {
    let tile = tile_of(mount);
    let (r, g, b) = settings.glow_color.to_rgb();

    assert!(
        computed(&tile, "border-top-color").contains(&format!("{r}, {g}, {b}")),
        "positive control: the lit style must paint the glow colour, got {}",
        computed(&tile, "border-top-color")
    );

    go_silent(mount).await;

    for side in ["top", "right", "bottom", "left"] {
        assert_eq!(
            computed(&tile, &format!("border-{side}-color")),
            BORDER_TOKEN,
            "the silent style must reset border-{side}-color; inline style is {:?}",
            tile.get_attribute("style")
        );
    }
}

fn mount_with_token() -> web_sys::Element {
    let mount = create_mount_point();
    mount
        .set_attribute("style", &format!("--grid-item-border: {BORDER_TOKEN}"))
        .unwrap();
    mount
}

#[wasm_bindgen_test]
async fn silent_style_clears_the_speaking_border() {
    let settings = AppearanceSettings {
        glow_decay: 0.0,
        ..AppearanceSettings::default()
    };
    seed(
        speak_style(0.8, true, &settings),
        speak_style(0.0, false, &settings),
    );

    let suppressor = suppress_transitions();
    let mount = mount_with_token();
    render_into(&mount, TileUnderTest);
    yield_now().await;

    assert_reset_to_token(&mount, &settings).await;

    cleanup(&mount);
    suppressor.remove();
}

#[wasm_bindgen_test]
async fn disabling_the_glow_clears_the_speaking_border() {
    let on = AppearanceSettings {
        glow_decay: 0.0,
        ..AppearanceSettings::default()
    };
    let off = AppearanceSettings {
        glow_enabled: false,
        ..on
    };
    // Still speaking at full level: only the `!glow_enabled` branch can return.
    seed(speak_style(0.8, true, &on), speak_style(0.8, true, &off));

    let suppressor = suppress_transitions();
    let mount = mount_with_token();
    render_into(&mount, TileUnderTest);
    yield_now().await;

    assert_reset_to_token(&mount, &on).await;

    cleanup(&mount);
    suppressor.remove();
}

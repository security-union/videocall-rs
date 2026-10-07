// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use dioxus::prelude::*;
use dioxus_ui::components::preferences_settings_panel::PreferencesSettingsPanel;
use dioxus_ui::context::{
    load_decode_budget_override, AppearanceSettings, AppearanceSettingsCtx, DecodeBudgetCtx,
    DecodeBudgetOverride,
};
use support::{cleanup, create_mount_point, render_into, yield_now};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const STORAGE_KEY: &str = "vc_decode_budget_override";
const TOGGLE: &str = "[data-testid='decode-budget-show-all-persistent']";
const AUTO_OPTION: &str = "[data-testid='decode-budget-auto']";
const SIX_OPTION: &str = "[data-testid='decode-budget-6']";

fn storage() -> web_sys::Storage {
    web_sys::window().unwrap().local_storage().unwrap().unwrap()
}

fn clear_storage() {
    let _ = storage().remove_item(STORAGE_KEY);
}

fn stored() -> Option<String> {
    storage().get_item(STORAGE_KEY).unwrap()
}

/// Seeds the override from localStorage, as `AttendantsComponent` does at mount.
#[allow(non_snake_case)]
fn PrefsPanel() -> Element {
    let decode_budget = use_signal(load_decode_budget_override);
    use_context_provider(|| DecodeBudgetCtx(decode_budget));
    let appearance = use_signal(AppearanceSettings::default);
    use_context_provider(|| AppearanceSettingsCtx(appearance));
    rsx! { PreferencesSettingsPanel {} }
}

async fn mount_panel() -> web_sys::Element {
    let mount = create_mount_point();
    render_into(&mount, PrefsPanel);
    yield_now().await;
    mount
}

fn element(mount: &web_sys::Element, selector: &str) -> web_sys::HtmlElement {
    mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("no element matches {selector}"))
        .dyn_into()
        .unwrap()
}

fn toggle_label(mount: &web_sys::Element) -> String {
    element(mount, TOGGLE).text_content().unwrap_or_default()
}

async fn click(mount: &web_sys::Element, selector: &str) {
    element(mount, selector).click();
    yield_now().await;
}

#[wasm_bindgen_test]
async fn show_all_toggle_flips_auto_to_all_and_back() {
    clear_storage();
    let mount = mount_panel().await;

    assert_eq!(toggle_label(&mount), "Show all videos");
    assert_eq!(element(&mount, TOGGLE).tag_name(), "BUTTON");

    click(&mount, TOGGLE).await;
    assert_eq!(toggle_label(&mount), "Back to automatic");
    assert_eq!(stored().as_deref(), Some("all"));
    assert_eq!(load_decode_budget_override(), DecodeBudgetOverride::All);
    assert_eq!(
        element(&mount, AUTO_OPTION)
            .get_attribute("aria-checked")
            .as_deref(),
        Some("false")
    );

    click(&mount, TOGGLE).await;
    assert_eq!(toggle_label(&mount), "Show all videos");
    assert_eq!(stored().as_deref(), Some("auto"));
    assert_eq!(
        element(&mount, AUTO_OPTION)
            .get_attribute("aria-checked")
            .as_deref(),
        Some("true")
    );

    cleanup(&mount);
    clear_storage();
}

#[wasm_bindgen_test]
async fn persisted_all_override_remounts_offering_back_to_automatic() {
    clear_storage();
    let mount = mount_panel().await;
    click(&mount, TOGGLE).await;
    cleanup(&mount);

    let mount = mount_panel().await;
    assert_eq!(toggle_label(&mount), "Back to automatic");

    cleanup(&mount);
    clear_storage();
}

#[wasm_bindgen_test]
async fn show_all_toggle_recovers_from_a_fixed_cap_to_auto() {
    clear_storage();
    let mount = mount_panel().await;

    click(&mount, SIX_OPTION).await;
    assert_eq!(stored().as_deref(), Some("6"));
    assert_eq!(toggle_label(&mount), "Back to automatic");

    click(&mount, TOGGLE).await;
    assert_eq!(stored().as_deref(), Some("auto"));
    assert_eq!(toggle_label(&mount), "Show all videos");
    assert_eq!(
        element(&mount, AUTO_OPTION)
            .get_attribute("aria-checked")
            .as_deref(),
        Some("true")
    );

    cleanup(&mount);
    clear_storage();
}

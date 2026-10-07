// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// `diagnosticsPacketsEnabled` parsed from a real `window.__APP_CONFIG`, and
// `?diag_packets=` read from the real page URL by the production snapshot.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use dioxus_ui::constants::{
    diagnostics_packets_config_value, diagnostics_packets_resolution,
    snapshot_diagnostics_packets_url_param, DiagnosticsPacketsSource,
};
use wasm_bindgen_test::*;

mod support;
use support::{inject_app_config, inject_app_config_with_diagnostics_packets, remove_app_config};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn absent_key_resolves_to_enabled_by_default() {
    inject_app_config();
    assert_eq!(diagnostics_packets_config_value(), None);
    assert_eq!(
        diagnostics_packets_resolution(),
        (true, DiagnosticsPacketsSource::Default)
    );
    remove_app_config();
}

#[wasm_bindgen_test]
fn config_key_disables_sending() {
    inject_app_config_with_diagnostics_packets("false");
    assert_eq!(diagnostics_packets_config_value().as_deref(), Some("false"));
    assert_eq!(
        diagnostics_packets_resolution(),
        (false, DiagnosticsPacketsSource::Config)
    );
    remove_app_config();
}

#[wasm_bindgen_test]
fn unrecognised_config_value_falls_back_to_enabled() {
    inject_app_config_with_diagnostics_packets("maybe");
    assert_eq!(
        diagnostics_packets_resolution(),
        (true, DiagnosticsPacketsSource::Default)
    );
    remove_app_config();
}

fn replace_url(url: &str) {
    gloo_utils::window()
        .history()
        .unwrap()
        .replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(url))
        .unwrap();
}

struct UrlRestore(String);

impl Drop for UrlRestore {
    fn drop(&mut self) {
        let _ = gloo_utils::window().history().and_then(|h| {
            h.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&self.0))
        });
        snapshot_diagnostics_packets_url_param();
    }
}

fn load_with_search(search: &str) -> UrlRestore {
    let location = gloo_utils::window().location();
    let original = format!(
        "{}{}{}",
        location.pathname().unwrap(),
        location.search().unwrap(),
        location.hash().unwrap()
    );
    let restore = UrlRestore(original);
    replace_url(search);
    snapshot_diagnostics_packets_url_param();
    replace_url(&restore.0);
    restore
}

#[wasm_bindgen_test]
fn url_param_disables_under_permissive_config_via_page_url_snapshot() {
    inject_app_config_with_diagnostics_packets("true");
    let _restore = load_with_search("?diag_packets=0");

    assert_eq!(
        diagnostics_packets_resolution(),
        (false, DiagnosticsPacketsSource::Url)
    );
    remove_app_config();
}

#[wasm_bindgen_test]
fn url_param_cannot_re_enable_a_disabling_config_via_page_url_snapshot() {
    inject_app_config_with_diagnostics_packets("false");
    let _restore = load_with_search("?diag_packets=1");

    assert_eq!(
        diagnostics_packets_resolution(),
        (false, DiagnosticsPacketsSource::Config)
    );
    remove_app_config();
}

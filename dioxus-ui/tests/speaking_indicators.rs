// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Tests for speaking indicator features (Dioxus):
// - VAD threshold runtime configuration
// - PeerListItem speaking/muted CSS classes

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use support::{
    cleanup, create_mount_point, inject_app_config, inject_app_config_with_vad_threshold,
    remove_app_config, render_into, yield_now,
};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

use dioxus::prelude::*;
use dioxus_ui::components::peer_list_item::PeerListItem;
use dioxus_ui::constants::vad_threshold;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// VAD threshold configuration tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn vad_threshold_returns_default_value() {
    inject_app_config();
    let threshold = vad_threshold().expect("vad_threshold should succeed");
    assert!(
        (threshold - 0.02).abs() < f32::EPSILON,
        "default VAD threshold should be 0.02, got {threshold}"
    );
    remove_app_config();
}

#[wasm_bindgen_test]
async fn vad_threshold_returns_custom_value() {
    inject_app_config_with_vad_threshold(0.05);
    let threshold = vad_threshold().expect("vad_threshold should succeed");
    assert!(
        (threshold - 0.05).abs() < f32::EPSILON,
        "custom VAD threshold should be 0.05, got {threshold}"
    );
    remove_app_config();
}

#[wasm_bindgen_test]
async fn vad_threshold_errors_without_config() {
    remove_app_config();
    assert!(
        vad_threshold().is_err(),
        "vad_threshold should error when __APP_CONFIG is missing"
    );
}

// ---------------------------------------------------------------------------
// PeerListItem speaking indicator tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn peer_list_item_speaking_adds_speaking_class() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { PeerListItem { name: "Alice".to_string(), speaking: true, muted: false } }
    }
    let _app = render_into(&mount, wrapper);
    yield_now().await;

    let mic_div = mount.query_selector(".peer_item_mic").unwrap().unwrap();
    assert!(
        mic_div.class_list().contains("speaking"),
        "mic div should have 'speaking' class when speaking=true"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn peer_list_item_not_speaking_lacks_speaking_class() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { PeerListItem { name: "Bob".to_string(), speaking: false, muted: false } }
    }
    let _app = render_into(&mount, wrapper);
    yield_now().await;

    let mic_div = mount.query_selector(".peer_item_mic").unwrap().unwrap();
    assert!(
        !mic_div.class_list().contains("speaking"),
        "mic div should NOT have 'speaking' class when speaking=false"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn peer_list_item_muted_renders_mic_icon() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { PeerListItem { name: "Charlie".to_string(), muted: true, speaking: false } }
    }
    let _app = render_into(&mount, wrapper);
    yield_now().await;

    let mic_div = mount.query_selector(".peer_item_mic").unwrap();
    assert!(mic_div.is_some(), "peer_item_mic div should be rendered");

    let svg = mount.query_selector(".peer_item_mic svg").unwrap();
    assert!(svg.is_some(), "mic SVG icon should be rendered");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn peer_list_item_speaking_and_muted_has_speaking_class() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { PeerListItem { name: "Diana".to_string(), speaking: true, muted: true } }
    }
    let _app = render_into(&mount, wrapper);
    yield_now().await;

    let mic_div = mount.query_selector(".peer_item_mic").unwrap().unwrap();
    assert!(
        mic_div.class_list().contains("speaking"),
        "mic div should have 'speaking' class even when muted (speaking prop is true)"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn peer_list_item_host_with_speaking_shows_crown_and_speaking() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { PeerListItem { name: "Host".to_string(), is_host: true, speaking: true, muted: false } }
    }
    let _app = render_into(&mount, wrapper);
    yield_now().await;

    // Host indicator label should be rendered for host
    let indicator = mount.query_selector(".peer_item_text .badge").unwrap();
    assert!(indicator.is_some(), "host should have indicator label");
    let text = indicator.unwrap().text_content().unwrap_or_default();
    assert!(text.contains("Host"), "indicator should contain 'Host'");

    // Speaking class should still work
    let mic_div = mount.query_selector(".peer_item_mic").unwrap().unwrap();
    assert!(
        mic_div.class_list().contains("speaking"),
        "host mic div should have 'speaking' class"
    );

    cleanup(&mount);
}

// ---------------------------------------------------------------------------
// Attendants rows: one per session, even when two sessions share a user id
// (the same account in a browser tab and in videocall-cli on a Pi).
// ---------------------------------------------------------------------------

fn two_sessions_one_user() -> Vec<dioxus_ui::components::peer_list::PeerRow> {
    use std::collections::HashMap;
    let sessions = vec!["101".to_string(), "202".to_string(), "303".to_string()];
    let user_of = |sid: &str| match sid {
        "101" | "202" => Some("alice@example.com".to_string()),
        "303" => Some("bob@example.com".to_string()),
        _ => None,
    };
    let name_of = |sid: &str| match sid {
        "101" | "202" => Some("Alice".to_string()),
        "303" => Some("Bob".to_string()),
        _ => None,
    };
    // 101: unmuted and speaking. 202: muted, silent. 303: unmuted, silent.
    let audio: HashMap<String, bool> = [("101", true), ("202", false), ("303", true)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let speaking: HashMap<String, bool> = [("101", true), ("202", false), ("303", false)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    dioxus_ui::components::peer_list::peer_rows(
        &sessions,
        user_of,
        name_of,
        Some("alice@example.com"),
        &audio,
        &speaking,
        "",
    )
}

#[wasm_bindgen_test]
fn peer_rows_keep_each_session_of_one_user_separate() {
    let rows = two_sessions_one_user();
    assert_eq!(rows.len(), 3);
    let (a, b, c) = (&rows[0], &rows[1], &rows[2]);
    assert_eq!(
        (a.session_id.as_str(), a.user_id.as_str()),
        ("101", "alice@example.com")
    );
    assert_eq!(
        (b.session_id.as_str(), b.user_id.as_str()),
        ("202", "alice@example.com")
    );
    assert!(!a.muted && a.speaking, "session 101 state: {a:?}");
    assert!(b.muted && !b.speaking, "session 202 state: {b:?}");
    assert!(
        a.is_host && b.is_host,
        "both sessions belong to the host account"
    );
    assert!(!c.is_host && !c.muted && !c.speaking);
}

#[wasm_bindgen_test]
fn peer_rows_search_matches_user_id_or_name() {
    use std::collections::HashMap;
    let sessions = vec!["1".to_string(), "2".to_string()];
    let rows = dioxus_ui::components::peer_list::peer_rows(
        &sessions,
        |sid| Some(if sid == "1" { "alice@x" } else { "bob@x" }.to_string()),
        |sid| Some(if sid == "1" { "Alice" } else { "Robert" }.to_string()),
        None,
        &HashMap::new(),
        &HashMap::new(),
        "ROB",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, "2");
    // Unknown sessions default to muted and not speaking.
    assert!(rows[0].muted && !rows[0].speaking);
}

#[wasm_bindgen_test]
async fn peer_rows_render_two_rows_for_one_user_with_their_own_state() {
    use dioxus_ui::components::peer_list::PeerRows;
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { ul { PeerRows { rows: two_sessions_one_user() } } }
    }
    let _app = render_into(&mount, wrapper);
    yield_now().await;

    let items = mount.query_selector_all(".peer_item").unwrap();
    assert_eq!(
        items.length(),
        3,
        "one row per session: {}",
        mount.inner_html()
    );
    let row = |i: u32| -> web_sys::Element { items.item(i).unwrap().dyn_into().unwrap() };
    let mic = |i: u32| row(i).query_selector(".peer_item_mic").unwrap().unwrap();

    // Both Alice rows are shown, each with its own session's state.
    for i in [0, 1] {
        assert!(row(i).text_content().unwrap().contains("Alice"));
        assert_eq!(
            row(i).get_attribute("title").as_deref(),
            Some("Host: alice@example.com")
        );
    }
    assert!(mic(0).class_list().contains("speaking"));
    assert!(!mic(1).class_list().contains("speaking"));
    // The mic icon differs: MIC for the live session, MIC_OFF for the muted one.
    assert_ne!(mic(0).inner_html(), mic(1).inner_html());
    assert!(row(2).text_content().unwrap().contains("Bob"));

    cleanup(&mount);
}

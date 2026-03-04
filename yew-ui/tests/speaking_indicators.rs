// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Tests for speaking indicator features:
// - VAD threshold runtime configuration
// - PeerListItem speaking/muted CSS classes

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::time::Duration;

use support::{
    cleanup, create_mount_point, inject_app_config, inject_app_config_with_vad_threshold,
    remove_app_config,
};
use wasm_bindgen_test::*;
use yew::platform::time::sleep;
use yew::prelude::*;

use videocall_ui::components::peer_list_item::PeerListItem;
use videocall_ui::constants::vad_threshold;

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
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! { <PeerListItem name={"Alice".to_string()} speaking={true} muted={false} /> }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    let mic_div = mount.query_selector(".peer_item_mic").unwrap().unwrap();
    assert!(
        mic_div.class_list().contains("speaking"),
        "mic div should have 'speaking' class when speaking=true"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn peer_list_item_not_speaking_lacks_speaking_class() {
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! { <PeerListItem name={"Bob".to_string()} speaking={false} muted={false} /> }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    let mic_div = mount.query_selector(".peer_item_mic").unwrap().unwrap();
    assert!(
        !mic_div.class_list().contains("speaking"),
        "mic div should NOT have 'speaking' class when speaking=false"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn peer_list_item_muted_renders_mic_icon() {
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! { <PeerListItem name={"Charlie".to_string()} muted={true} speaking={false} /> }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    // The mic icon should always be rendered regardless of muted state
    let mic_div = mount.query_selector(".peer_item_mic").unwrap();
    assert!(mic_div.is_some(), "peer_item_mic div should be rendered");

    // SVG mic icon should be present
    let svg = mount.query_selector(".peer_item_mic svg").unwrap();
    assert!(svg.is_some(), "mic SVG icon should be rendered");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn peer_list_item_speaking_and_muted_has_speaking_class() {
    // Even if muted, speaking class should reflect the speaking prop
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! { <PeerListItem name={"Diana".to_string()} speaking={true} muted={true} /> }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    let mic_div = mount.query_selector(".peer_item_mic").unwrap().unwrap();
    assert!(
        mic_div.class_list().contains("speaking"),
        "mic div should have 'speaking' class even when muted (speaking prop is true)"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn peer_list_item_host_with_speaking_shows_crown_and_speaking() {
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! { <PeerListItem name={"Host".to_string()} is_host={true} speaking={true} muted={false} /> }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    // Crown icon should be rendered for host
    let crown = mount.query_selector(".peer_item_text svg").unwrap();
    assert!(crown.is_some(), "host should have crown icon");

    // Speaking class should still work
    let mic_div = mount.query_selector(".peer_item_mic").unwrap().unwrap();
    assert!(
        mic_div.class_list().contains("speaking"),
        "host mic div should have 'speaking' class"
    );

    cleanup(&mount);
}

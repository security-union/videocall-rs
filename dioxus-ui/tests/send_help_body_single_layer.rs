// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue 2620: the `HelpPopover` renders OUTSIDE `SendLayerCell`'s
// `if !single_layer`, so the video copy must branch on the same predicate.
//
// Two panels mount in this one binary; every assertion is markup presence or the
// popover's own text, never a readout a second panel's global driver can write.
// A new binary needs its own `--test` step in `pr-check-dioxus-ui-hcl.yaml`.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use dioxus::prelude::*;
use dioxus_ui::components::performance_settings::receive::ReceivePreference;
use dioxus_ui::components::performance_settings::{
    send_layer_labels, PerformancePreference, PerformanceSettingsPanel,
};
use videocall_client::PrefMediaKind;
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

mod support;
use support::{cleanup, create_mount_point, inject_app_config, render_into, wait_for_selector};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

fn clamped_panel() -> Element {
    rsx! {
        PerformanceSettingsPanel {
            pref: PerformancePreference::default(),
            on_change: move |_| {},
            receive_pref: ReceivePreference::default(),
            on_receive_change: move |_| {},
            video_layer_max: 1,
            screen_layer_max: 3,
        }
    }
}

fn two_layer_panel() -> Element {
    rsx! {
        PerformanceSettingsPanel {
            pref: PerformancePreference::default(),
            on_change: move |_| {},
            receive_pref: ReceivePreference::default(),
            on_receive_change: move |_| {},
            video_layer_max: 2,
            screen_layer_max: 3,
        }
    }
}

async fn open_help_text(mount: &web_sys::Element, key_id: &str) -> String {
    let btn = mount
        .query_selector(&format!("[data-testid='{key_id}-help']"))
        .unwrap()
        .unwrap_or_else(|| panic!("{key_id} must render its help button"))
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap();
    btn.click();
    let popover = format!("#{key_id}-help-popover");
    assert!(
        wait_for_selector(mount, &popover, 5_000).await,
        "clicking {key_id}-help must open {popover}"
    );
    mount
        .query_selector(&format!("{popover} .perf-help-popover-text"))
        .unwrap()
        .unwrap_or_else(|| panic!("{popover} must carry a body paragraph"))
        .text_content()
        .unwrap_or_default()
}

#[wasm_bindgen_test]
async fn one_layer_video_help_promises_no_handle_and_no_reset() {
    inject_app_config();
    let mount = create_mount_point();
    render_into(&mount, clamped_panel);

    assert!(
        wait_for_selector(&mount, "#perf-vu-audio-readout", 5_000).await,
        "the panel must mount, or every assertion below is vacuous"
    );
    assert_eq!(
        send_layer_labels(PrefMediaKind::Video, 1).len(),
        1,
        "this fixture only exercises the branch while video's ladder is one layer deep"
    );
    assert!(
        mount
            .query_selector("[data-testid='perf-video-range-max']")
            .unwrap()
            .is_none(),
        "a one-layer video ladder renders no ceiling handle"
    );
    assert!(
        mount
            .query_selector("[data-testid='perf-video-auto']")
            .unwrap()
            .is_none(),
        "a one-layer video ladder renders no Reset"
    );

    let body = open_help_text(&mount, "perf-video").await;
    for promise in ["handle", "Reset", "raise it", "Lower it", "device"] {
        assert!(
            !body.contains(promise),
            "the one-layer video help must not promise `{promise}`; got: {body}"
        );
    }
    // The not-contains set alone is satisfied by empty copy.
    assert!(
        body.contains("one video layer") && body.contains("nothing to set"),
        "the one-layer video help must say only one video layer is available and \
         that there is nothing to set; got: {body}"
    );

    let intro = open_help_text(&mount, "perf-intro").await;
    for promise in [
        "the right handle sets",
        "what you SEND",
        "saves your upload",
        "device",
    ] {
        assert!(
            !intro.contains(promise),
            "the clamped panel intro must not promise `{promise}`; got: {intro}"
        );
    }
    assert!(
        intro.contains("the two handles"),
        "the receive columns keep their handles, so the intro must still describe \
         them; got: {intro}"
    );

    cleanup(&mount);
}

/// Brackets the threshold from above: `max_simulcast_layers` returns exactly 2 for
/// 6-to-9-core devices, so a `<= 2` predicate would strand that whole population.
#[wasm_bindgen_test]
async fn two_layer_video_keeps_its_control_and_laddered_help() {
    inject_app_config();
    let mount = create_mount_point();
    render_into(&mount, two_layer_panel);

    assert!(
        wait_for_selector(&mount, "[data-testid='perf-video-range-max']", 5_000).await,
        "a two-layer video ladder has a choice to make and must keep its handle"
    );
    assert!(
        mount
            .query_selector("[data-testid='perf-video-send-rungs']")
            .unwrap()
            .is_some(),
        "a two-layer video ladder must keep its rung strip"
    );

    let body = open_help_text(&mount, "perf-video").await;
    assert!(
        body.contains("handle") && body.contains("Reset"),
        "the laddered video help names the handle and Reset it still renders; got: {body}"
    );
    assert!(
        !body.contains("device"),
        "the laddered video help must not attribute the ceiling to the device; got: {body}"
    );

    let intro = open_help_text(&mount, "perf-intro").await;
    assert!(
        intro.contains("the right handle sets"),
        "the panel intro must still describe the send handle; got: {intro}"
    );

    cleanup(&mount);
}

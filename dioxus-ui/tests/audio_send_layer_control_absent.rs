// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue 2620: a SEND ladder with one layer renders no layer control. Audio has
// published one layer since issue 2279 and screen since issue 2343, so both cells
// must drop the slider, the rung strip and the position caption while keeping the
// live meter and the summary line.
//
// This lives in its OWN binary rather than in `send_pinned_floor_valuetext.rs`
// because that file's `cleanup` leaves the panel's `Interval` and rAF loop running
// against DOCUMENT-GLOBAL element ids; a second mount in the same binary would let
// two panels write each other's readouts and go FALSE GREEN.
//
// A new test binary is only COMPILED by `cargo test --no-run` in
// `pr-check-dioxus-ui-hcl.yaml`; it needs its own `--test` run step there or it
// never executes.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use dioxus::prelude::*;
use dioxus_ui::components::performance_settings::receive::ReceivePreference;
use dioxus_ui::components::performance_settings::{
    send_layer_labels_with_top, PerformancePreference, PerformanceSettingsPanel,
};
use dioxus_ui::constants::audio_published_layer_count;
use videocall_client::PrefMediaKind;
use wasm_bindgen_test::*;

mod support;
use support::{cleanup, create_mount_point, inject_app_config, render_into, wait_for_selector};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

/// Video is forced to THREE rungs so the suppression is proven LENGTH-driven: a
/// blanket removal of the control would also take video's slider, which this
/// fixture still requires to render.
fn panel() -> Element {
    rsx! {
        PerformanceSettingsPanel {
            pref: PerformancePreference::default(),
            on_change: move |_| {},
            receive_pref: ReceivePreference::default(),
            on_receive_change: move |_| {},
            video_layer_max: 3,
            screen_layer_max: 3,
        }
    }
}

#[wasm_bindgen_test]
async fn one_layer_send_ladders_render_no_layer_control() {
    inject_app_config();
    let mount = create_mount_point();
    render_into(&mount, panel);

    // The audio bitrate readout is issue 2620's must-survive acceptance criterion
    // AND this test's mount proof: every assertion below is an absence, and an
    // absence is vacuous on a panel that never mounted.
    let readout_found = wait_for_selector(&mount, "#perf-vu-audio-readout", 5_000).await;
    assert!(
        readout_found,
        "the live audio bitrate readout must survive the control removal (and \
         without it the absence assertions below prove nothing)"
    );

    for (kind, rungs) in [
        (
            "audio",
            send_layer_labels_with_top(
                PrefMediaKind::Audio,
                audio_published_layer_count() as usize,
                "720p",
            ),
        ),
        (
            "screen",
            send_layer_labels_with_top(PrefMediaKind::Screen, 3, "720p"),
        ),
    ] {
        assert_eq!(
            rungs.len(),
            1,
            "{kind} publishes ONE layer; at a deeper ladder the control is supposed \
             to render and these absences would be asserting a regression"
        );
        for suffix in ["range-min", "range-max", "send-rungs", "range-value"] {
            assert!(
                mount
                    .query_selector(&format!("[data-testid='perf-{kind}-{suffix}']"))
                    .unwrap()
                    .is_none(),
                "a one-layer {kind} ladder must render no `perf-{kind}-{suffix}`"
            );
        }
        let summary = mount
            .query_selector(&format!("[data-testid='perf-{kind}-send-summary']"))
            .unwrap()
            .unwrap_or_else(|| panic!("{kind} keeps its summary line as its state readout"));
        assert!(
            !summary.text_content().unwrap_or_default().trim().is_empty(),
            "the {kind} summary line is now the cell's only state text and must not \
             be empty"
        );
    }

    // VIDEO still has three layers, so its control must survive untouched.
    assert!(
        mount
            .query_selector("[data-testid='perf-video-range-min']")
            .unwrap()
            .is_some(),
        "video publishes three layers and must keep its layer control; suppressing \
         it here means the guard is not length-driven"
    );

    // The help popover renders OUTSIDE the suppressed block.
    for kind in ["audio", "screen"] {
        let help = mount
            .query_selector(&format!("[data-testid='perf-{kind}-help']"))
            .unwrap();
        assert!(help.is_some(), "{kind} keeps its help affordance");
    }

    cleanup(&mount);
}

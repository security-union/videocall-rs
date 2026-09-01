// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// The microphone encoder publishes ONE audio layer, and that count is not a function
// of `experimentalSimulcastMaxLayers`.
//
// Browser-only: the decoupling is observable only against a real parsed
// `RuntimeConfig`, where the omitted key takes its serde default of 3.

use dioxus_ui::constants::{audio_published_layer_count, experimental_simulcast_max_layers};
use videocall_client::{max_layers_for_kind, PrefMediaKind};
use wasm_bindgen_test::*;

mod support;
use support::inject_app_config;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn audio_publish_count_is_one_and_ignores_the_simulcast_flag() {
    inject_app_config();

    assert_eq!(
        experimental_simulcast_max_layers(),
        3,
        "premise: the flag must be at its deployed default, or the two numbers \
         cannot be shown to disagree"
    );
    assert_eq!(audio_published_layer_count(), 1);

    // The receiver ladder stays 3 deep; the publish count must not track it either.
    assert!(
        audio_published_layer_count() < max_layers_for_kind(PrefMediaKind::Audio),
        "the publish count must be independent of the receive ladder depth"
    );
}

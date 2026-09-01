// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue #2524. Host tests cover `update_reception` and `render_reception`; neither can
// see that `Diagnostics` WIRES them to the DOM, and the Playwright spec that would is
// untagged and #2193-blocked. Browser-only: the bus has no native receiver.

use dioxus::prelude::*;
use dioxus_ui::components::diagnostics::Diagnostics;
use dioxus_ui::components::media_metrics_overlay::MediaMetricsOverlayCtx;
use dioxus_ui::context::TransportPreferenceCtx;
use videocall_client::VideoCallClient;
use videocall_diagnostics::{global_sender, metric, DiagEvent};
use wasm_bindgen_test::*;

mod support;
use support::{cleanup, create_mount_point, inject_app_config, render_into, yield_now};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const PEER: &str = "alice";

/// Not 0: an un-folded `Option` could render 0 too, so it would not discriminate.
const REPORTED_GAP: u64 = 437;

#[allow(non_snake_case)]
fn DrawerParent() -> Element {
    let client = use_hook(|| VideoCallClient::new_for_test("local-user"));
    use_context_provider(|| client.clone());
    let transport = use_signal(Default::default);
    use_context_provider(|| TransportPreferenceCtx(transport));
    let overlay_on = use_signal(|| false);
    use_context_provider(|| MediaMetricsOverlayCtx(overlay_on));

    let noop: EventHandler<()> = use_callback(|_| {});
    let noop_f64: EventHandler<f64> = use_callback(|_: f64| {});
    rsx! {
        Diagnostics {
            is_open: true,
            on_close: noop,
            video_enabled: true,
            mic_enabled: true,
            share_screen: false,
            encoder_settings: None,
            width: 420.0,
            on_resize_start: noop,
            on_resize_move: noop_f64,
            on_resize_end: noop,
        }
    }
}

fn broadcast_reception(max_gap: u64) {
    let _ = global_sender().try_broadcast(DiagEvent {
        subsystem: "video",
        stream_id: None,
        ts_ms: 1_500_000,
        metrics: vec![
            metric!("media_type", "VIDEO".to_string()),
            metric!("from_peer", "local-user".to_string()),
            metric!("to_peer", PEER.to_string()),
            metric!("video_seq_loss_per_sec", 0.0),
            metric!("video_seq_max_gap", max_gap),
            metric!("keyframe_requests_per_sec", 0.0),
        ],
    });
}

#[wasm_bindgen_test]
async fn reception_dump_renders_the_published_max_gap() {
    inject_app_config();
    let mount = create_mount_point();
    render_into(&mount, DrawerParent);
    yield_now().await;

    broadcast_reception(REPORTED_GAP);
    for _ in 0..20 {
        yield_now().await;
    }

    // FIRST block only: Media Status renders an unconditional `<pre>` that would pass.
    let block = mount
        .query_selector(".diag-raw-block")
        .unwrap()
        .expect("Raw stats disclosure must render its first block");
    let heading = block
        .query_selector("h4")
        .unwrap()
        .map(|e| e.text_content().unwrap_or_default())
        .unwrap_or_default();
    assert_eq!(
        heading, "Reception Stats",
        "premise: the first raw block must be Reception, or this asserts the wrong pre"
    );
    let text = block
        .query_selector("pre")
        .unwrap()
        .map(|e| e.text_content().unwrap_or_default())
        .unwrap_or_default();

    assert!(
        text.contains(&format!("Max gap: {REPORTED_GAP} frames")),
        "the dump must render the published gap; got {text:?}"
    );
    assert!(
        text.contains(PEER),
        "and attribute it to the remote peer; got {text:?}"
    );

    cleanup(&mount);
}

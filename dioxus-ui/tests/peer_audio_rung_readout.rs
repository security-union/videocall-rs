// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// A REMOTE peer's audio kbps is not derivable from the layer id it reports, so the
// peer tile's received-audio field renders the em-dash even when `peer_status`
// carries a rung.
//
// Browser-only: the assertion is on rendered DOM text, and the `peer_status` event
// travels the `videocall-diagnostics` broadcast bus, whose native `SENDER` drops
// its receiver.

use dioxus::prelude::*;
use dioxus_ui::components::canvas_generator::PinnedTile;
use dioxus_ui::components::media_metrics_overlay::MediaMetricsOverlayCtx;
use dioxus_ui::components::peer_tile::PeerTile;
use dioxus_ui::context::{
    AppearanceSettings, AppearanceSettingsCtx, MeetingTime, PeerSignalHistoryMap,
    SignalPopupStateMap,
};
use std::collections::HashMap;
use videocall_client::VideoCallClient;
use videocall_diagnostics::{global_sender, metric, DiagEvent};
use wasm_bindgen_test::*;

mod support;
use support::{cleanup, create_mount_point, inject_app_config, render_into, yield_now};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const PEER: &str = "alice";

/// The rung the test reports over `peer_status`. Deliberately the TOP rung: its
/// ladder nominal is the largest, so a reintroduced mapping is unmistakable in the
/// rendered text.
const REPORTED_RUNG: u64 = 2;

#[allow(non_snake_case)]
fn TileParent() -> Element {
    // Every context `PeerTile` requires, plus `MediaMetricsOverlayCtx` ENABLED —
    // the overlay subtree is gated on it, and the other in-crate tile tests leave
    // it absent precisely to take the no-overlay path.
    let client = use_hook(|| VideoCallClient::new_for_test("local-user"));
    use_context_provider(|| client.clone());
    let history_map: PeerSignalHistoryMap = use_signal(HashMap::new);
    use_context_provider(|| history_map);
    let popup_map: SignalPopupStateMap = use_signal(HashMap::new);
    use_context_provider(|| popup_map);
    let appearance = use_signal(AppearanceSettings::default);
    use_context_provider(|| AppearanceSettingsCtx(appearance));
    let meeting_time = use_signal(MeetingTime::default);
    use_context_provider(|| meeting_time);
    let overlay_on = use_signal(|| true);
    use_context_provider(|| MediaMetricsOverlayCtx(overlay_on));

    let pin: EventHandler<PinnedTile> = use_callback(|_| {});
    let decode: EventHandler<String> = use_callback(|_: String| {});
    rsx! {
        PeerTile {
            peer_id: PEER.to_string(),
            on_toggle_pin: pin,
            on_request_decode: decode,
        }
    }
}

/// Broadcast the `peer_status` shape the decode path emits for a peer whose audio
/// is on and whose selected rung is arriving.
fn broadcast_peer_status(rung: u64) {
    let _ = global_sender().try_broadcast(DiagEvent {
        subsystem: "peer_status",
        stream_id: None,
        ts_ms: 0,
        metrics: vec![
            metric!("to_peer", PEER.to_string()),
            metric!("audio_enabled", 1u64),
            metric!(
                videocall_client::decode::peer_decode_manager::METRIC_SELECTED_AUDIO_LAYER,
                rung
            ),
        ],
    });
}

#[wasm_bindgen_test]
async fn peer_tile_readout_em_dashes_a_reported_audio_rung() {
    inject_app_config();
    let mount = create_mount_point();
    render_into(&mount, TileParent);
    yield_now().await;

    broadcast_peer_status(REPORTED_RUNG);
    // The tile's diagnostics subscriber is async; give it turns to land the signal
    // write and re-render.
    for _ in 0..20 {
        yield_now().await;
    }

    let overlay = mount
        .query_selector("[data-testid='media-metrics-overlay-peer']")
        .unwrap();
    let text = overlay
        .map(|e| e.text_content().unwrap_or_default())
        .unwrap_or_default();

    assert!(
        text.contains("\u{2014}k"),
        "the received-audio field must render the em-dash even with a rung \
         reported; got {text:?}"
    );
    // Every rung nominal, read from the production ladder so a retune moves with it.
    for rung in 0..3u32 {
        let nominal =
            videocall_client::decode::layer_chooser::audio_layer_kbps(rung).expect("on-ladder");
        assert!(
            !text.contains(&format!("{nominal}k")),
            "readout must carry no ladder bitrate; found {nominal}k in {text:?}"
        );
    }

    cleanup(&mount);
}

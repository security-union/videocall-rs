// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// A REMOTE peer's tile renders the audio bitrate its publisher stamped, and the
// em-dash when it stamped none, even when `peer_status` carries a rung.
//
// Browser-only: the assertion is on rendered DOM text, and the `peer_status` event
// travels the `videocall-diagnostics` broadcast bus, whose native `SENDER` drops
// its receiver.

use dioxus::prelude::*;
use dioxus_ui::components::canvas_generator::PinnedTile;
use dioxus_ui::components::media_metrics_overlay::MediaMetricsOverlayCtx;
use dioxus_ui::components::peer_tile::PeerTile;
use dioxus_ui::context::{
    AppearanceSettings, AppearanceSettingsCtx, MeetingTime, PeerAudioLivenessMap,
    PeerSignalHistoryMap, SignalPopupStateMap,
};
use std::collections::HashMap;
use videocall_client::VideoCallClient;
use videocall_diagnostics::{global_sender, metric, DiagEvent};
use wasm_bindgen_test::*;

mod support;
use support::{cleanup, create_mount_point, inject_app_config, render_into, yield_now};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const UNSTAMPED_SID: u64 = 3171;
const STAMPED_SID: u64 = 3172;
const STAMPED_KBPS: u32 = 8;

/// The rung the test reports over `peer_status`. Deliberately the TOP rung: its
/// ladder nominal is the largest, so a reintroduced mapping is unmistakable in the
/// rendered text.
const REPORTED_RUNG: u64 = 2;

/// Every context `PeerTile` requires, plus `MediaMetricsOverlayCtx` ENABLED — the
/// overlay subtree is gated on it.
fn tile_for(session_id: u64, stamped_kbps: u32) -> Element {
    let client = use_hook(|| {
        let client = VideoCallClient::new_for_test("local-user");
        client.insert_peer_for_test(session_id, "remote@example.com");
        client.decode_audio_packet_for_test(session_id, stamped_kbps);
        client
    });
    use_context_provider(|| client.clone());
    let history_map: PeerSignalHistoryMap = use_signal(HashMap::new);
    use_context_provider(|| history_map);
    let liveness_map: PeerAudioLivenessMap = use_signal(HashMap::new);
    use_context_provider(|| liveness_map);
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
            peer_id: session_id.to_string(),
            on_toggle_pin: pin,
            on_request_decode: decode,
        }
    }
}

#[allow(non_snake_case)]
fn UnstampedTileParent() -> Element {
    tile_for(UNSTAMPED_SID, 0)
}

#[allow(non_snake_case)]
fn StampedTileParent() -> Element {
    tile_for(STAMPED_SID, STAMPED_KBPS)
}

/// Broadcast the `peer_status` shape the decode path emits for a peer whose audio
/// is on and whose selected rung is arriving.
fn broadcast_peer_status(session_id: u64, rung: u64) {
    let _ = global_sender().try_broadcast(DiagEvent {
        subsystem: "peer_status",
        stream_id: None,
        ts_ms: 0,
        metrics: vec![
            metric!("to_peer", session_id.to_string()),
            metric!("audio_enabled", 1u64),
            metric!(
                videocall_client::decode::peer_decode_manager::METRIC_SELECTED_AUDIO_LAYER,
                rung
            ),
        ],
    });
}

async fn rendered_overlay_text(parent: fn() -> Element, session_id: u64) -> String {
    inject_app_config();
    let mount = create_mount_point();
    render_into(&mount, parent);
    yield_now().await;

    broadcast_peer_status(session_id, REPORTED_RUNG);
    // The tile's diagnostics subscriber is async; give it turns to land the signal
    // write and re-render.
    for _ in 0..20 {
        yield_now().await;
    }

    let text = mount
        .query_selector("[data-testid='media-metrics-overlay-peer']")
        .unwrap()
        .map(|e| e.text_content().unwrap_or_default())
        .unwrap_or_default();
    cleanup(&mount);
    text
}

#[wasm_bindgen_test]
async fn peer_tile_readout_em_dashes_an_unstamped_audio_bitrate() {
    let text = rendered_overlay_text(UnstampedTileParent, UNSTAMPED_SID).await;

    assert!(
        text.contains("\u{2014}k"),
        "an unstamped audio field must render the em-dash even with a rung \
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
}

#[wasm_bindgen_test]
async fn peer_tile_readout_renders_the_stamped_audio_bitrate() {
    let text = rendered_overlay_text(StampedTileParent, STAMPED_SID).await;

    assert!(
        text.ends_with(&format!("\u{b7} {STAMPED_KBPS}k")),
        "the received-audio field must render the publisher's stamp; got {text:?}"
    );
}

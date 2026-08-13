// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue #2132: the peer tile's received-audio readout must report the audio rung
// the receiver is actually decoding, not the layer-0 nominal.
//
// WHY THIS FILE EXISTS. The host unit tests are mutation-sensitive on each link
// of the chain — `overlay_audio_kbps_from_status` (peer_status -> kbps),
// `overlay_audio_kbps_display` (kbps -> Option, the em-dash gate), and
// `format_media_metrics_line` (Option -> text). What none of them can see is
// whether `PeerTile` WIRES them together: replacing its call with
// `overlay_audio_kbps(a, None)` reinstates the bug and leaves every host test
// green, because each helper is tested with explicit arguments.
//
// The Playwright spec `e2e/tests/media-metrics-overlay.spec.ts` does assert this
// surface with real peers, and this PR extends it. But it cannot currently run:
// issue #2193 means its 2-peer `setupTwoUserMeeting` helper times out on
// `.grid-item:has(canvas)` before reaching any assertion — verified on this head,
// where the two PRE-EXISTING tests in that file fail identically. It is also
// untagged, so `pr-check-e2e-smoke-hcl.yaml` (`--project=bvt1`) would not run it
// even once #2193 lands.
//
// So without this file, reverting the tile's wiring leaves every per-PR CI job
// green. Same seam #2170 was faulted for, applied here because the reviewer
// asked for exactly this link.
//
// It runs in a browser (`wasm_bindgen_test`) for two reasons: the assertion is on
// rendered DOM text, and the `peer_status` event that carries the rung travels the
// `videocall-diagnostics` broadcast bus, whose native `SENDER` drops its receiver
// — so a host test cannot deliver the event at all.
//
// COVERAGE SPLIT — what this deliberately does NOT observe:
//   * the EMISSION side (`Peer::displayed_audio_layer` and the availability
//     window). This injects a `peer_status` event directly; the emission is
//     covered by `peer_status_carries_the_arriving_audio_rung_and_omits_a_stale_one`
//     in `videocall-client`.
//   * the real decode path that sets `selected_audio_layer`. Client-side host
//     tests pin the chooser; this file starts from "a rung was reported."

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

/// The rung the test reports over `peer_status`. Deliberately NOT rung 0: rung 0's
/// nominal is what the pre-fix code always rendered, so it cannot discriminate.
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
async fn peer_tile_readout_reports_the_rung_from_peer_status() {
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

    let expected = videocall_client::decode::layer_chooser::audio_layer_kbps(REPORTED_RUNG as u32)
        .expect("reported rung must be on the ladder");
    let base = videocall_client::decode::layer_chooser::audio_layer_kbps(0).unwrap();
    assert_ne!(
        expected, base,
        "premise: the reported rung's nominal must differ from the base, or this \
         assertion cannot discriminate the fix"
    );
    assert!(
        text.contains(&format!("{expected}k")),
        "readout must report the rung carried on peer_status ({expected}k); got {text:?}"
    );
    assert!(
        !text.contains(&format!("{base}k")),
        "readout must NOT fall back to the base nominal ({base}k) — that is the \
         #2132 bug; got {text:?}"
    );

    cleanup(&mount);
}

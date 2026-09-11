// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue 2660: a peer's audio-liveness stamp must OUTLIVE the `PeerTile` scope.
// Any participant toggling a screen share remounts every tile, and a per-scope
// stamp reseeded to the `audio_path_never_heard` sentinel, which EXEMPTS a
// latched heartbeat from every veto and leaves the speaking border lit forever.
// MUTATION: revert `audio_buffer_stamp` to `use_hook(|| Rc::new(Cell::new(0.0)))`
// and the post-remount assertion fails while its pre-remount twin still passes.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use dioxus::prelude::*;
use dioxus_ui::components::canvas_generator::PinnedTile;
use dioxus_ui::components::peer_tile::PeerTile;
use dioxus_ui::context::{
    AppearanceSettings, AppearanceSettingsCtx, MeetingTime, PeerAudioLivenessMap,
    PeerSignalHistoryMap, SignalPopupStateMap,
};
use std::collections::HashMap;
use videocall_client::adaptive_quality_constants::HEARTBEAT_KEEPALIVE_INTERVAL_MS;
use videocall_client::VideoCallClient;
use videocall_diagnostics::{global_sender, metric, DiagEvent};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

mod support;
use support::{cleanup, create_mount_point, inject_app_config, render_into, yield_now};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const PEER: &str = "alice";

/// An order of magnitude clear of `records_live_audio`'s 20 ms stall residue.
const LIVE_BUFFER_MS: u64 = 200;

const TILE: &str = ".grid-item";
const GLOW: &str = ".speaking-tile";
const TOGGLE: &str = "[data-testid='tile-mount-toggle']";

/// The contexts `PeerTile` requires, plus a button that remounts the tile.
#[allow(non_snake_case)]
fn RemountParent() -> Element {
    let client = use_hook(|| VideoCallClient::new_for_test("local-user"));
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

    let mut mounted = use_signal(|| true);
    let pin: EventHandler<PinnedTile> = use_callback(|_: PinnedTile| {});
    let decode: EventHandler<String> = use_callback(|_: String| {});
    rsx! {
        button {
            "data-testid": "tile-mount-toggle",
            onclick: move |_| {
                let next = !*mounted.peek();
                mounted.set(next);
            },
            "toggle"
        }
        if mounted() {
            PeerTile {
                peer_id: PEER.to_string(),
                on_toggle_pin: pin,
                on_request_decode: decode,
            }
        }
    }
}

fn broadcast_live_audio_sample() {
    let _ = global_sender().try_broadcast(DiagEvent {
        subsystem: "neteq",
        stream_id: None,
        ts_ms: 0,
        metrics: vec![
            metric!("target_peer", PEER.to_string()),
            metric!("audio_buffer_ms", LIVE_BUFFER_MS),
        ],
    });
}

/// An unmuted peer whose sender VAD claims speech; `audio_level` is always 0.0.
fn broadcast_speaking_heartbeat() {
    let _ = global_sender().try_broadcast(DiagEvent {
        subsystem: "peer_status",
        stream_id: None,
        ts_ms: 0,
        metrics: vec![
            metric!("to_peer", PEER.to_string()),
            metric!("audio_enabled", 1u64),
            metric!("is_speaking", 1u64),
            metric!("audio_level", 0.0_f64),
        ],
    });
}

/// The decoder fast path: a measured level, not subject to the heartbeat veto.
fn broadcast_decoded_speech() {
    let _ = global_sender().try_broadcast(DiagEvent {
        subsystem: "peer_speaking",
        stream_id: None,
        ts_ms: 0,
        metrics: vec![
            metric!("to_peer", PEER.to_string()),
            metric!("speaking", 1u64),
            metric!("audio_level", 0.9_f64),
        ],
    });
}

/// Bounded on purpose: settling frames eat the liveness window under measurement.
async fn settle() {
    for _ in 0..8 {
        yield_now().await;
    }
}

fn present(mount: &web_sys::Element, selector: &str) -> bool {
    mount.query_selector(selector).unwrap().is_some()
}

/// Returns as soon as the toggle lands, so a slow runner costs frames instead of
/// the liveness window the measurement sits inside.
async fn wait_for_tile(mount: &web_sys::Element, want: bool) {
    for _ in 0..40 {
        if present(mount, TILE) == want {
            return;
        }
        yield_now().await;
    }
}

fn tile_class(mount: &web_sys::Element) -> String {
    mount
        .query_selector(TILE)
        .unwrap()
        .expect("the tile must be mounted")
        .get_attribute("class")
        .unwrap_or_default()
}

fn click_toggle(mount: &web_sys::Element) {
    mount
        .query_selector(TOGGLE)
        .unwrap()
        .expect("the harness toggle must be rendered")
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap()
        .click();
}

#[wasm_bindgen_test]
async fn the_liveness_stamp_survives_a_tile_remount() {
    inject_app_config();
    let mount = create_mount_point();
    render_into(&mount, RemountParent);
    settle().await;
    assert!(
        present(&mount, TILE),
        "positive control: the tile must have mounted, or every assertion below is vacuous"
    );

    let stamped_at = js_sys::Date::now();
    broadcast_live_audio_sample();
    settle().await;

    broadcast_speaking_heartbeat();
    settle().await;
    assert!(
        !present(&mount, GLOW),
        "a heartbeat claiming speech must not light a tile whose decoded audio is arriving; \
         tile class was {:?}",
        tile_class(&mount)
    );

    click_toggle(&mount);
    wait_for_tile(&mount, false).await;
    assert!(
        !present(&mount, TILE),
        "positive control: the tile scope must actually be destroyed"
    );
    click_toggle(&mount);
    wait_for_tile(&mount, true).await;
    assert!(
        present(&mount, TILE),
        "positive control: the tile scope must actually be recreated"
    );

    let elapsed = js_sys::Date::now() - stamped_at;
    assert!(
        elapsed < f64::from(HEARTBEAT_KEEPALIVE_INTERVAL_MS),
        "inconclusive, not a regression: {elapsed}ms since the stamp overran this test's \
         budget, which sits strictly inside the production liveness window"
    );

    // THE MEASUREMENT. Same claim, same evidence, new scope.
    broadcast_speaking_heartbeat();
    settle().await;
    assert!(
        !present(&mount, GLOW),
        "issue 2660: the remounted tile lost the audio-liveness stamp, so the never-heard \
         sentinel exempted a latched heartbeat and relit the speaking border; tile class \
         was {:?}, and the identical pre-remount assertion above passed",
        tile_class(&mount)
    );

    broadcast_decoded_speech();
    settle().await;
    assert!(
        present(&mount, GLOW),
        "positive control: the remounted tile is subscribed and CAN glow, so the assertion \
         above measured the veto rather than a dead tile"
    );

    cleanup(&mount);
}

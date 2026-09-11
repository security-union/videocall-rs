// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue 2673: `MediaMetricsOverlayCtx` gates the PEER disc, the badge and the
// popup, which used to render unconditionally. The SELF disc is exempt.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::RefCell;
use std::collections::HashMap;

use dioxus::prelude::*;
use dioxus_ui::components::canvas_generator::{PinnedTile, TileMode};
use dioxus_ui::components::connection_quality_indicator::ConnectionQualityIndicator;
use dioxus_ui::components::media_metrics_overlay::MediaMetricsOverlayCtx;
use dioxus_ui::components::peer_tile::PeerTile;
use dioxus_ui::context::{
    AppearanceSettings, AppearanceSettingsCtx, MeetingTime, PeerAudioLivenessMap,
    PeerSignalHistoryMap, SignalPopupStateMap,
};
use support::{
    cleanup, create_mount_point, inject_app_config, inject_app_config_transport_badge_on,
    render_into, yield_now,
};
use videocall_client::VideoCallClient;
use videocall_diagnostics::{global_sender, metric, DiagEvent};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const TILE: &str = ".grid-item";
const SPLIT_TILE: &str = ".split-peer-tile";
const DISC: &str = "button.signal-indicator[data-testid='peer-signal-indicator']";
const SELF_DISC: &str = "button.signal-indicator[data-testid='self-signal-indicator']";
const BADGE: &str = ".transport-badge";
const POPUP: &str = ".signal-quality-popup";
const TOGGLE: &str = "[data-testid='diag-toggle']";

thread_local! {
    /// Per-case, so a finished case's orphaned sampler cannot write into it.
    static PEER_ID: RefCell<String> = const { RefCell::new(String::new()) };
    static MODE: RefCell<TileMode> = const { RefCell::new(TileMode::Full) };
}

fn peer_id() -> String {
    PEER_ID.with(|p| p.borrow().clone())
}

fn mode() -> TileMode {
    MODE.with(|m| m.borrow().clone())
}

#[allow(non_snake_case)]
fn GatedTileParent() -> Element {
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
    let mut diagnostics_on = use_signal(|| false);
    use_context_provider(|| MediaMetricsOverlayCtx(diagnostics_on));

    let pin: EventHandler<PinnedTile> = use_callback(|_: PinnedTile| {});
    let decode: EventHandler<String> = use_callback(|_: String| {});
    rsx! {
        button {
            "data-testid": "diag-toggle",
            onclick: move |_| {
                let next = !*diagnostics_on.peek();
                diagnostics_on.set(next);
            },
            "toggle"
        }
        PeerTile {
            peer_id: peer_id(),
            render_mode: mode(),
            on_toggle_pin: pin,
            on_request_decode: decode,
        }
    }
}

#[allow(non_snake_case)]
fn GatedSelfDiscParent() -> Element {
    let mut diagnostics_on = use_signal(|| false);
    use_context_provider(|| MediaMetricsOverlayCtx(diagnostics_on));
    let open: EventHandler<()> = use_callback(|_: ()| {});
    rsx! {
        button {
            "data-testid": "diag-toggle",
            onclick: move |_| {
                let next = !*diagnostics_on.peek();
                diagnostics_on.set(next);
            },
            "toggle"
        }
        ConnectionQualityIndicator { on_open_diagnostics: open }
    }
}

fn broadcast_peer_transport() {
    let _ = global_sender().try_broadcast(DiagEvent {
        subsystem: "peer_status",
        stream_id: None,
        ts_ms: 0,
        metrics: vec![
            metric!("to_peer", peer_id()),
            metric!("audio_enabled", 1u64),
            metric!("peer_transport", "websocket".to_string()),
        ],
    });
}

fn broadcast_self_rtt_at(ts_ms: u64, rtt_ms: f64) {
    let _ = global_sender().try_broadcast(DiagEvent {
        subsystem: "connection_manager",
        stream_id: None,
        ts_ms,
        metrics: vec![metric!("active_server_rtt", rtt_ms)],
    });
}

async fn settle() {
    for _ in 0..8 {
        yield_now().await;
    }
}

fn announce_text(mount: &web_sys::Element) -> String {
    mount
        .query_selector("[role='status']")
        .unwrap()
        .map(|n| n.text_content().unwrap_or_default())
        .unwrap_or_default()
}

fn present(mount: &web_sys::Element, selector: &str) -> bool {
    mount.query_selector(selector).unwrap().is_some()
}

fn click(mount: &web_sys::Element, selector: &str) {
    mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} must be rendered to be clicked"))
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap()
        .click();
}

fn start(peer: &str, tile_mode: TileMode) -> web_sys::Element {
    PEER_ID.with(|p| *p.borrow_mut() = peer.to_string());
    MODE.with(|m| *m.borrow_mut() = tile_mode);
    let mount = create_mount_point();
    render_into(&mount, GatedTileParent);
    mount
}

#[wasm_bindgen_test]
async fn the_peer_disc_and_badge_follow_the_diagnostics_checkbox() {
    inject_app_config_transport_badge_on();
    let mount = start("gate-peer-both", TileMode::Full);
    settle().await;
    assert!(
        present(&mount, TILE),
        "positive control: the tile must mount, or every assertion below is vacuous"
    );

    broadcast_peer_transport();
    settle().await;

    assert!(
        !present(&mount, DISC),
        "checkbox off: the signal disc must not be in the DOM"
    );
    assert!(
        !present(&mount, BADGE),
        "checkbox off: the transport badge must not be in the DOM"
    );

    click(&mount, TOGGLE);
    settle().await;
    assert!(
        present(&mount, DISC),
        "checkbox on: the signal disc must appear"
    );
    assert!(
        present(&mount, BADGE),
        "checkbox on (deploy flag on, transport known): the badge must appear"
    );

    click(&mount, TOGGLE);
    settle().await;
    assert!(
        !present(&mount, DISC),
        "un-ticking must remove the disc again, not just skip the first paint"
    );
    assert!(!present(&mount, BADGE), "un-ticking must remove the badge");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_split_peer_video_arm_is_gated_too() {
    inject_app_config_transport_badge_on();
    let mount = start("gate-peer-split", TileMode::VideoOnly);
    settle().await;
    assert!(
        present(&mount, SPLIT_TILE),
        "positive control: the split arm must be the one that rendered"
    );

    broadcast_peer_transport();
    settle().await;
    assert!(
        !present(&mount, DISC),
        "checkbox off: no disc in split view"
    );
    assert!(
        !present(&mount, BADGE),
        "checkbox off: no badge in split view"
    );

    click(&mount, TOGGLE);
    settle().await;
    assert!(present(&mount, DISC), "checkbox on: the split disc appears");
    assert!(
        present(&mount, BADGE),
        "checkbox on: the split badge appears"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_deploy_flag_still_gates_the_badge_with_the_checkbox_on() {
    inject_app_config();
    let mount = start("gate-peer-flagoff", TileMode::Full);
    settle().await;
    broadcast_peer_transport();
    click(&mount, TOGGLE);
    settle().await;

    assert!(
        present(&mount, DISC),
        "positive control: the checkbox is on, so the disc proves the tile re-rendered"
    );
    assert!(
        !present(&mount, BADGE),
        "transportBadgeEnabled is off: the checkbox must not override the ops kill switch"
    );

    cleanup(&mount);
}

/// The popup's X and the disc's second click are its ONLY dismissal paths, so
/// un-ticking hides it without dismissing it and re-ticking must bring it back.
#[wasm_bindgen_test]
async fn the_open_popup_hides_with_the_disc_and_returns_with_it() {
    inject_app_config();
    let mount = start("gate-peer-popup", TileMode::Full);
    settle().await;
    click(&mount, TOGGLE);
    settle().await;

    click(&mount, DISC);
    settle().await;
    assert!(
        present(&mount, POPUP),
        "positive control: clicking the disc must open its popup"
    );

    click(&mount, TOGGLE);
    settle().await;
    assert!(
        !present(&mount, POPUP),
        "the popup outlived the disc that dismisses it, so nothing could close it"
    );

    click(&mount, TOGGLE);
    settle().await;
    assert!(
        present(&mount, POPUP),
        "hiding is not dismissing: the popup the viewer never closed must come back"
    );

    cleanup(&mount);
}

/// The self disc is EXEMPT (product call): its `role="status"` region is the
/// product's only emitter of "Your connection is poor.", so gating it would
/// leave the default path with no degraded-connection cue, spoken or visual.
#[wasm_bindgen_test]
async fn the_self_disc_and_its_announcement_are_exempt_from_the_checkbox() {
    let mount = create_mount_point();
    render_into(&mount, GatedSelfDiscParent);
    settle().await;
    assert!(
        present(&mount, SELF_DISC),
        "checkbox off is the DEFAULT path: the self disc must still render"
    );

    for i in 1..=5u64 {
        broadcast_self_rtt_at(900_000 + i * 1_000, 600.0);
    }
    settle().await;
    let disc = mount.query_selector(SELF_DISC).unwrap().expect("disc");
    assert_eq!(
        (
            disc.get_attribute("data-signal-state").as_deref(),
            disc.get_attribute("data-signal-level").as_deref(),
        ),
        (Some("measured"), Some("1")),
        "checkbox off: the self disc must still measure and report a bad link"
    );
    assert!(
        announce_text(&mount).contains("Your connection is poor"),
        "the product's only degraded-connection announcement must survive; got {:?}",
        announce_text(&mount)
    );

    click(&mount, TOGGLE);
    settle().await;
    assert!(
        present(&mount, SELF_DISC),
        "ticking must not disturb an exempt element either"
    );

    cleanup(&mount);
}

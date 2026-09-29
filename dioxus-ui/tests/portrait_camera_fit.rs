// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0

// Issue 2783: a portrait camera feed is letterboxed on the stage and filled in
// the grid until the viewer picks a fit, driven by the decoder's real
// `video_resolution` event through a mounted `PeerTile`.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::Cell;
use std::collections::HashMap;

use dioxus::prelude::*;
use dioxus_ui::components::canvas_generator::{PinnedTile, TileMode};
use dioxus_ui::components::peer_tile::PeerTile;
use dioxus_ui::context::{
    AppearanceSettings, AppearanceSettingsCtx, CroppedTilesCtx, MeetingTime, PeerAudioLivenessMap,
    PeerSignalHistoryMap, SignalPopupStateMap,
};
use support::{cleanup, create_mount_point, inject_app_config, render_into, yield_now};
use videocall_client::VideoCallClient;
use videocall_diagnostics::{global_sender, metric, DiagEvent};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

thread_local! {
    static PEER: Cell<u64> = const { Cell::new(0) };
    static FULL_BLEED: Cell<bool> = const { Cell::new(false) };
}

#[allow(non_snake_case)]
fn CameraPeer() -> Element {
    let peer = PEER.with(Cell::get);
    let client = use_hook(|| {
        let client = VideoCallClient::new_for_test("local-user");
        client.insert_peer_for_test(peer, "carol");
        client.set_peer_media_for_test(peer, true, false);
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
    let cropped = use_signal(HashMap::new);
    use_context_provider(|| CroppedTilesCtx(cropped));
    let mut stage = use_signal(|| FULL_BLEED.with(Cell::get));
    let pin: EventHandler<PinnedTile> = use_callback(|_: PinnedTile| {});
    let decode: EventHandler<String> = use_callback(|_: String| {});
    rsx! {
        button {
            "data-testid": "stage-toggle",
            onclick: move |_| {
                let next = !*stage.peek();
                stage.set(next);
            },
            "stage"
        }
        PeerTile {
            peer_id: peer.to_string(),
            full_bleed: stage(),
            render_mode: TileMode::Full,
            on_toggle_pin: pin,
            on_request_decode: decode,
        }
    }
}

fn start(peer: u64, full_bleed: bool) -> web_sys::Element {
    inject_app_config();
    PEER.with(|p| p.set(peer));
    FULL_BLEED.with(|f| f.set(full_bleed));
    let mount = create_mount_point();
    render_into(&mount, CameraPeer);
    mount
}

fn announce_resolution(peer: u64, width: u64, height: u64) {
    let _ = global_sender().try_broadcast(DiagEvent {
        subsystem: "video_resolution",
        stream_id: None,
        ts_ms: 0,
        metrics: vec![
            metric!("resolution_width", width),
            metric!("resolution_height", height),
            metric!("from_peer", "local".to_string()),
            metric!("to_peer", peer.to_string()),
            metric!("media_type", "VIDEO".to_string()),
        ],
    });
}

async fn settle() {
    for _ in 0..4 {
        yield_now().await;
    }
}

fn class_of(mount: &web_sys::Element, selector: &str) -> String {
    mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} must be rendered"))
        .class_name()
}

fn canvas(peer: u64) -> String {
    format!("#peer-video-{peer}-div canvas")
}

fn crop_button(peer: u64) -> String {
    format!("#peer-video-{peer}-div .crop-icon")
}

fn install_shipped_stylesheets() -> web_sys::Element {
    let style = gloo_utils::document().create_element("style").unwrap();
    style.set_text_content(Some(&format!(
        "{}{}",
        include_str!("../static/style.css"),
        include_str!("../static/global.css"),
    )));
    gloo_utils::document()
        .head()
        .unwrap()
        .append_child(&style)
        .unwrap();
    style
}

fn background_of(mount: &web_sys::Element, selector: &str) -> String {
    let element = mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} must be rendered"));
    web_sys::window()
        .unwrap()
        .get_computed_style(&element)
        .unwrap()
        .unwrap()
        .get_property_value("background-color")
        .unwrap()
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

#[wasm_bindgen_test]
async fn a_portrait_feed_is_letterboxed_on_the_stage_until_the_viewer_picks_fill() {
    let peer = 27831;
    let mount = start(peer, true);
    settle().await;

    announce_resolution(peer, 640, 480);
    settle().await;
    assert_eq!(
        class_of(&mount, &canvas(peer)),
        "cropped",
        "a landscape feed keeps filling the stage"
    );

    announce_resolution(peer, 480, 640);
    settle().await;
    assert_eq!(class_of(&mount, &canvas(peer)), "uncropped");
    assert_eq!(
        class_of(&mount, &crop_button(peer)),
        "crop-icon",
        "the crop button must report the letterbox the viewer sees"
    );

    click(&mount, &crop_button(peer));
    settle().await;
    assert_eq!(
        class_of(&mount, &canvas(peer)),
        "cropped",
        "the first click on a letterboxed tile must switch it to fill"
    );
    assert_eq!(class_of(&mount, &crop_button(peer)), "crop-icon active");

    announce_resolution(peer, 640, 480);
    settle().await;
    announce_resolution(peer, 480, 640);
    settle().await;
    assert_eq!(
        class_of(&mount, &canvas(peer)),
        "cropped",
        "the viewer's choice must survive the sender rotating"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_portrait_feed_keeps_filling_a_grid_tile() {
    let peer = 27832;
    let mount = start(peer, false);
    settle().await;

    announce_resolution(peer, 480, 640);
    settle().await;
    assert_eq!(class_of(&mount, &canvas(peer)), "cropped");

    click(&mount, "[data-testid='stage-toggle']");
    settle().await;
    assert_eq!(
        class_of(&mount, &canvas(peer)),
        "uncropped",
        "the same portrait tile promoted to the stage must letterbox"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_letterbox_bars_are_video_black_in_the_light_theme() {
    let html = gloo_utils::document().document_element().unwrap();
    html.set_attribute("data-theme", "light").unwrap();
    let style = install_shipped_stylesheets();
    let peer = 27833;
    let mount = start(peer, true);
    mount
        .set_attribute("style", "position: relative; width: 960px; height: 540px;")
        .unwrap();
    settle().await;

    announce_resolution(peer, 640, 480);
    settle().await;
    let tile = format!("#peer-video-{peer}-div");
    assert_ne!(
        background_of(&mount, &tile),
        "rgb(0, 0, 0)",
        "precondition: the light theme's stage must not already be black"
    );
    assert_eq!(
        background_of(&mount, &canvas(peer)),
        "rgba(0, 0, 0, 0)",
        "a filled canvas has no bars to paint"
    );

    let video = mount.query_selector(&canvas(peer)).unwrap().unwrap();
    video.set_attribute("width", "480").unwrap();
    video.set_attribute("height", "640").unwrap();
    announce_resolution(peer, 480, 640);
    settle().await;
    assert_eq!(class_of(&mount, &canvas(peer)), "uncropped");
    assert_eq!(background_of(&mount, &canvas(peer)), "rgb(0, 0, 0)");

    let frame = mount
        .query_selector(&format!("{tile} .canvas-container"))
        .unwrap()
        .unwrap()
        .get_bounding_client_rect();
    let painted = video.get_bounding_client_rect();
    assert!(
        frame.width() > painted.height() * 480.0 / 640.0 + 1.0,
        "precondition: the stage must be wider than the portrait picture, so there are bars"
    );
    assert_eq!(
        (painted.width(), painted.height()),
        (frame.width(), frame.height()),
        "the canvas box must span the bars for its background to paint them"
    );

    cleanup(&mount);
    style.remove();
    html.remove_attribute("data-theme").unwrap();
}

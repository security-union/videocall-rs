/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use crate::components::icons::crop::CropIcon;
use crate::components::icons::crown::CrownIcon;
use crate::components::icons::mic::MicIcon;
use crate::components::icons::peer::PeerIcon;
use crate::components::icons::push_pin::PushPinIcon;
use crate::constants::users_allowed_to_stream;
use crate::context::VideoCallClientCtx;
use dioxus::prelude::*;
use std::rc::Rc;
use videocall_client::VideoCallClient;
use web_sys::{window, HtmlCanvasElement};

/// Render a single peer tile. If `full_bleed` is true and the peer is not screen sharing,
/// the video tile will occupy the full grid area. If `host_display_name` matches `key`, a crown
/// icon is displayed next to the name.
pub fn generate_for_peer(
    client: &VideoCallClient,
    key: &String,
    full_bleed: bool,
    host_display_name: Option<&str>,
) -> Element {
    let is_host = host_display_name.map(|h| h == key).unwrap_or(false);
    let allowed = users_allowed_to_stream().unwrap_or_default();
    if !allowed.is_empty() && !allowed.iter().any(|host| host == key) {
        return rsx! {};
    }

    let is_video_enabled_for_peer = client.is_video_enabled_for_peer(key);
    let is_audio_enabled_for_peer = client.is_audio_enabled_for_peer(key);
    let is_screen_share_enabled_for_peer = client.is_screen_share_enabled_for_peer(key);

    // Full-bleed single peer (no screen share)
    if full_bleed && !is_screen_share_enabled_for_peer {
        let peer_video_div_id = Rc::new(format!("peer-video-{}-div", &key));
        let div_id_mobile = (*peer_video_div_id).clone();
        let div_id_pin = (*peer_video_div_id).clone();
        let canvas_id_crop = key.clone();
        let key_clone = key.clone();
        let title = if is_host {
            format!("Host: {key}")
        } else {
            key.clone()
        };
        return rsx! {
            div {
                class: "grid-item full-bleed",
                id: "{peer_video_div_id}",
                div {
                    class: if is_video_enabled_for_peer { "canvas-container video-on" } else { "canvas-container" },
                    onclick: move |_| {
                        if is_mobile_viewport() {
                            toggle_pinned_div(&div_id_mobile);
                        }
                    },
                    if is_video_enabled_for_peer {
                        UserVideo { id: key_clone.clone(), hidden: false }
                    } else {
                        div {
                            class: "",
                            div {
                                class: "placeholder-content",
                                PeerIcon {}
                                span { class: "placeholder-text", "Camera Off" }
                            }
                        }
                    }
                    h4 {
                        class: "floating-name",
                        title: "{title}",
                        dir: "auto",
                        "{key}"
                        if is_host {
                            CrownIcon {}
                        }
                    }
                    div { class: "audio-indicator",
                        MicIcon { muted: !is_audio_enabled_for_peer }
                    }
                    button {
                        onclick: move |_| toggle_canvas_crop(&canvas_id_crop),
                        class: "crop-icon",
                        CropIcon {}
                    }
                    button {
                        onclick: move |_| toggle_pinned_div(&div_id_pin),
                        class: "pin-icon",
                        PushPinIcon {}
                    }
                }
            }
        };
    }

    // Regular grid tile, optionally with screen share tile
    let screen_share_css = if client.is_awaiting_peer_screen_frame(key) {
        "grid-item hidden"
    } else {
        "grid-item"
    };
    let screen_share_div_id = Rc::new(format!("screen-share-{}-div", &key));
    let peer_video_div_id = Rc::new(format!("peer-video-{}-div", &key));

    let ss_div_mobile = (*screen_share_div_id).clone();
    let ss_div_pin = (*screen_share_div_id).clone();
    let ss_canvas_crop = format!("screen-share-{}", key);
    let ss_name = format!("{}-screen", key);

    let pv_div_mobile = (*peer_video_div_id).clone();
    let pv_div_pin = (*peer_video_div_id).clone();
    let pv_canvas_crop = key.clone();
    let key_clone = key.clone();
    let title = if is_host {
        format!("Host: {key}")
    } else {
        key.clone()
    };

    rsx! {
        // Canvas for Screen share.
        if is_screen_share_enabled_for_peer {
            div {
                class: "{screen_share_css}",
                id: "{screen_share_div_id}",
                div {
                    class: "canvas-container video-on",
                    onclick: move |_| {
                        if is_mobile_viewport() {
                            toggle_pinned_div(&ss_div_mobile);
                        }
                    },
                    ScreenCanvas { peer_id: key.clone() }
                    h4 {
                        class: "floating-name",
                        title: "{ss_name}",
                        dir: "auto",
                        "{ss_name}"
                    }
                    button {
                        onclick: move |_| toggle_canvas_crop(&ss_canvas_crop),
                        class: "crop-icon",
                        CropIcon {}
                    }
                    button {
                        onclick: move |_| toggle_pinned_div(&ss_div_pin),
                        class: "pin-icon",
                        PushPinIcon {}
                    }
                }
            }
        }
        div {
            class: "grid-item",
            id: "{peer_video_div_id}",
            // One canvas for the User Video
            div {
                class: if is_video_enabled_for_peer { "canvas-container video-on" } else { "canvas-container" },
                onclick: move |_| {
                    if is_mobile_viewport() {
                        toggle_pinned_div(&pv_div_mobile);
                    }
                },
                if is_video_enabled_for_peer {
                    UserVideo { id: key_clone.clone(), hidden: false }
                } else {
                    div { class: "placeholder-content",
                        PeerIcon {}
                        span { class: "placeholder-text", "Video Disabled" }
                    }
                }
                h4 {
                    class: "floating-name",
                    title: "{title}",
                    dir: "auto",
                    "{key}"
                    if is_host {
                        CrownIcon {}
                    }
                }
                div { class: "audio-indicator",
                    MicIcon { muted: !is_audio_enabled_for_peer }
                }
                button {
                    onclick: move |_| toggle_canvas_crop(&pv_canvas_crop),
                    class: "crop-icon",
                    CropIcon {}
                }
                button {
                    onclick: move |_| toggle_pinned_div(&pv_div_pin),
                    class: "pin-icon",
                    PushPinIcon {}
                }
            }
        }
    }
}

#[component]
fn UserVideo(id: String, hidden: bool) -> Element {
    let client = use_context::<VideoCallClientCtx>();
    let id_for_effect = id.clone();

    use_effect(move || {
        use wasm_bindgen::JsCast;
        if let Some(elem) = gloo_utils::document().get_element_by_id(&id_for_effect) {
            if let Ok(canvas) = elem.dyn_into::<HtmlCanvasElement>() {
                let client = client.clone();
                let id = id_for_effect.clone();
                if let Err(e) = client.set_peer_video_canvas(&id, canvas) {
                    log::debug!("Canvas not yet ready for peer {id}: {e:?}");
                }
            }
        }
    });

    rsx! {
        canvas {
            id: "{id}",
            hidden: hidden,
            class: "uncropped",
        }
    }
}

#[component]
fn ScreenCanvas(peer_id: String) -> Element {
    let client = use_context::<VideoCallClientCtx>();
    let canvas_id = format!("screen-share-{}", peer_id);
    let canvas_id_for_effect = canvas_id.clone();
    let peer_id_for_effect = peer_id.clone();

    use_effect(move || {
        use wasm_bindgen::JsCast;
        if let Some(elem) = gloo_utils::document().get_element_by_id(&canvas_id_for_effect) {
            if let Ok(canvas) = elem.dyn_into::<HtmlCanvasElement>() {
                let client = client.clone();
                let peer_id = peer_id_for_effect.clone();
                if let Err(e) = client.set_peer_screen_canvas(&peer_id, canvas) {
                    log::debug!("Screen canvas not yet ready for peer {peer_id}: {e:?}");
                }
            }
        }
    });

    rsx! {
        canvas {
            id: "{canvas_id}",
            class: "uncropped",
        }
    }
}

fn toggle_pinned_div(div_id: &str) {
    if let Some(div) = window()
        .and_then(|w| w.document())
        .and_then(|doc| doc.get_element_by_id(div_id))
    {
        if !div.class_list().contains("grid-item-pinned") {
            div.class_list().add_1("grid-item-pinned").unwrap();
        } else {
            div.class_list().remove_1("grid-item-pinned").unwrap();
        }
    }
}

fn is_mobile_viewport() -> bool {
    if let Some(win) = window() {
        if let Ok(width) = win.inner_width() {
            let px = width.as_f64().unwrap_or(1024.0);
            return px < 768.0;
        }
    }
    false
}

fn toggle_canvas_crop(canvas_id: &str) {
    if let Some(canvas) = window()
        .and_then(|w| w.document())
        .and_then(|doc| doc.get_element_by_id(canvas_id))
    {
        let class_list = canvas.class_list();
        let is_cropped = class_list.contains("cropped");
        if is_cropped {
            let _ = class_list.remove_1("cropped");
            let _ = class_list.add_1("uncropped");
        } else {
            let _ = class_list.remove_1("uncropped");
            let _ = class_list.add_1("cropped");
        }
    }
}

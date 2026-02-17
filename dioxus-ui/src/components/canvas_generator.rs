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

use dioxus::prelude::*;
use videocall_client::VideoCallClient;
use web_sys::{window, HtmlCanvasElement};

use crate::components::icons::crop::CropIcon;
use crate::components::icons::crown::CrownIcon;
use crate::components::icons::mic::MicIcon;
use crate::components::icons::peer::PeerIcon;
use crate::components::icons::push_pin::PushPinIcon;
use crate::constants::users_allowed_to_stream;
use crate::context::VideoCallClientCtx;

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
        let peer_video_div_id = format!("peer-video-{}-div", key);
        let key_clone = key.clone();
        let peer_video_div_id_clone = peer_video_div_id.clone();
        let peer_video_div_id_clone2 = peer_video_div_id.clone();
        let title = if is_host {
            format!("Host: {key}")
        } else {
            key.clone()
        };

        return rsx! {
            div { class: "grid-item full-bleed", id: "{peer_video_div_id}",
                div {
                    class: if is_video_enabled_for_peer { "canvas-container video-on" } else { "canvas-container" },
                    onclick: move |_| {
                        if is_mobile_viewport() {
                            toggle_pinned_div(&peer_video_div_id_clone);
                        }
                    },
                    if is_video_enabled_for_peer {
                        UserVideo { id: key_clone.clone(), hidden: false }
                    } else {
                        div { class: "",
                            div { class: "placeholder-content",
                                PeerIcon {}
                                span { class: "placeholder-text", "Camera Off" }
                            }
                        }
                    }
                    h4 { class: "floating-name", title: "{title}", dir: "auto",
                        "{key}"
                        if is_host {
                            CrownIcon {}
                        }
                    }
                    div { class: "audio-indicator",
                        MicIcon { muted: !is_audio_enabled_for_peer }
                    }
                    button {
                        class: "crop-icon",
                        onclick: move |_| toggle_canvas_crop(&key_clone),
                        CropIcon {}
                    }
                    button {
                        class: "pin-icon",
                        onclick: move |_| toggle_pinned_div(&peer_video_div_id_clone2),
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
    let screen_share_div_id = format!("screen-share-{}-div", key);
    let peer_video_div_id = format!("peer-video-{}-div", key);

    let key_clone = key.clone();
    let key_for_screen = key.clone();
    let key_for_crop = key.clone();
    let screen_share_div_id_clone = screen_share_div_id.clone();
    let screen_share_div_id_clone2 = screen_share_div_id.clone();
    let peer_video_div_id_clone = peer_video_div_id.clone();
    let peer_video_div_id_clone2 = peer_video_div_id.clone();

    let title = if is_host {
        format!("Host: {key}")
    } else {
        key.clone()
    };

    rsx! {
        // Canvas for Screen share.
        if is_screen_share_enabled_for_peer {
            div { class: "{screen_share_css}", id: "{screen_share_div_id}",
                div {
                    class: "canvas-container video-on",
                    onclick: move |_| {
                        if is_mobile_viewport() {
                            toggle_pinned_div(&screen_share_div_id_clone);
                        }
                    },
                    ScreenCanvas { peer_id: key_for_screen.clone() }
                    h4 { class: "floating-name", title: "{key}-screen", dir: "auto",
                        "{key}-screen"
                    }
                    button {
                        class: "crop-icon",
                        onclick: move |_| toggle_canvas_crop(&format!("screen-share-{}", key_for_screen)),
                        CropIcon {}
                    }
                    button {
                        class: "pin-icon",
                        onclick: move |_| toggle_pinned_div(&screen_share_div_id_clone2),
                        PushPinIcon {}
                    }
                }
            }
        }
        div { class: "grid-item", id: "{peer_video_div_id}",
            // One canvas for the User Video
            div {
                class: if is_video_enabled_for_peer { "canvas-container video-on" } else { "canvas-container" },
                onclick: move |_| {
                    if is_mobile_viewport() {
                        toggle_pinned_div(&peer_video_div_id_clone);
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
                h4 { class: "floating-name", title: "{title}", dir: "auto",
                    "{key}"
                    if is_host {
                        CrownIcon {}
                    }
                }
                div { class: "audio-indicator",
                    MicIcon { muted: !is_audio_enabled_for_peer }
                }
                button {
                    class: "crop-icon",
                    onclick: move |_| toggle_canvas_crop(&key_for_crop),
                    CropIcon {}
                }
                button {
                    class: "pin-icon",
                    onclick: move |_| toggle_pinned_div(&peer_video_div_id_clone2),
                    PushPinIcon {}
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct UserVideoProps {
    pub id: String,
    pub hidden: bool,
}

#[component]
fn UserVideo(props: UserVideoProps) -> Element {
    let client = use_context::<VideoCallClientCtx>();

    // Pass canvas reference to client when mounted
    let peer_id = props.id.clone();
    use_effect(move || {
        if let Some(client) = &client {
            if let Some(canvas) = window()
                .and_then(|w| w.document())
                .and_then(|doc| doc.get_element_by_id(&peer_id))
                .and_then(|el| el.dyn_into::<HtmlCanvasElement>().ok())
            {
                if let Err(e) = client.set_peer_video_canvas(&peer_id, canvas) {
                    log::debug!("Canvas not yet ready for peer {peer_id}: {e:?}");
                }
            }
        }
    });

    rsx! {
        canvas {
            id: "{props.id}",
            hidden: props.hidden,
            class: "uncropped"
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ScreenCanvasProps {
    pub peer_id: String,
}

#[component]
fn ScreenCanvas(props: ScreenCanvasProps) -> Element {
    let client = use_context::<VideoCallClientCtx>();
    let canvas_id = format!("screen-share-{}", props.peer_id);

    // Pass canvas reference to client when mounted
    let peer_id = props.peer_id.clone();
    let canvas_id_clone = canvas_id.clone();
    use_effect(move || {
        if let Some(client) = &client {
            if let Some(canvas) = window()
                .and_then(|w| w.document())
                .and_then(|doc| doc.get_element_by_id(&canvas_id_clone))
                .and_then(|el| el.dyn_into::<HtmlCanvasElement>().ok())
            {
                if let Err(e) = client.set_peer_screen_canvas(&peer_id, canvas) {
                    log::debug!("Screen canvas not yet ready for peer {peer_id}: {e:?}");
                }
            }
        }
    });

    rsx! {
        canvas { id: "{canvas_id}", class: "uncropped" }
    }
}

fn toggle_pinned_div(div_id: &str) {
    if let Some(div) = window()
        .and_then(|w| w.document())
        .and_then(|doc| doc.get_element_by_id(div_id))
    {
        if !div.class_list().contains("grid-item-pinned") {
            let _ = div.class_list().add_1("grid-item-pinned");
        } else {
            let _ = div.class_list().remove_1("grid-item-pinned");
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

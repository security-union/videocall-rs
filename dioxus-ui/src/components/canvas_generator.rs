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

/// Compute the inline CSS for the speaking glow on the canvas container.
/// Always returns explicit values so the glow is fully self-contained in the
/// inline style with zero dependency on CSS classes.
fn speak_style(audio_level: f32) -> String {
    if audio_level <= 0.0 {
        // Explicitly force off — no reliance on CSS class removal
        return "border: 1.5px solid transparent; box-shadow: none; transition: border 1.5s ease-out, box-shadow 1.5s ease-out;".to_string();
    }
    let i = audio_level.clamp(0.0, 1.0);
    // More dramatic glow that scales aggressively with intensity.
    // Uses full `border` shorthand because the glow overlay div does not
    // inherit the container's border — it needs its own.
    format!(
        "border: 1.5px solid rgba(0, 255, 65, {:.2}); \
         box-shadow: inset 0 0 {:.0}px {:.0}px rgba(0, 255, 65, {:.2}), \
                     0 0 {:.0}px {:.0}px rgba(0, 255, 65, {:.2}); \
         transition: border 0.15s ease-in, box-shadow 0.15s ease-in;",
        0.4 + i * 0.6,   // border alpha: 0.4–1.0 (more visible)
        15.0 + i * 25.0, // inset blur: 15–40 (bigger glow)
        5.0 + i * 10.0,  // inset spread: 5–15 (wider)
        0.3 + i * 0.5,   // inset alpha: 0.3–0.8 (brighter)
        15.0 + i * 35.0, // outer blur: 15–50 (much bigger halo)
        3.0 + i * 10.0,  // outer spread: 3–13 (wider)
        0.2 + i * 0.4    // outer alpha: 0.2–0.6 (brighter)
    )
}

/// Compute the inline CSS for the mic icon glow.
/// Always returns explicit values — no reliance on CSS class for glow reset.
fn mic_style(audio_level: f32) -> String {
    if audio_level <= 0.0 {
        return "color: inherit; filter: none; transition: color 1.5s ease-out, filter 1.5s ease-out;".to_string();
    }
    let i = audio_level.clamp(0.0, 1.0);
    format!(
        "color: #00ff41; filter: drop-shadow(0 0 {:.0}px rgba(0, 255, 65, {:.2})); \
         transition: color 0.15s ease-in, filter 0.15s ease-in;",
        6.0 + i * 10.0, // drop-shadow radius: 6–16
        0.6 + i * 0.4   // drop-shadow alpha: 0.6–1.0
    )
}

/// Render a single peer tile. If `full_bleed` is true and the peer is not screen sharing,
/// the video tile will occupy the full grid area. The `audio_level` parameter (0.0–1.0) drives
/// a glow whose intensity scales with voice volume.
/// If `host_user_id` matches the peer's authenticated user_id, a crown icon is displayed next to the name.
pub fn generate_for_peer(
    client: &VideoCallClient,
    key: &String,
    full_bleed: bool,
    audio_level: f32,
    host_user_id: Option<&str>,
) -> Element {
    let peer_user_id = client.get_peer_user_id(key).unwrap_or_else(|| key.clone());
    let peer_display_name = client
        .get_peer_display_name(key)
        .unwrap_or_else(|| peer_user_id.clone());
    // Compare authenticated user_id (from JWT/DB) instead of user-chosen display name
    // to prevent spoofing the host crown icon.
    let is_host = host_user_id.map(|h| h == peer_user_id).unwrap_or(false);
    let allowed = users_allowed_to_stream().unwrap_or_default();
    if !allowed.is_empty() && !allowed.contains(&peer_user_id) {
        return rsx! {};
    }

    let is_video_enabled_for_peer = client.is_video_enabled_for_peer(key);
    let is_audio_enabled_for_peer = client.is_audio_enabled_for_peer(key);
    let is_screen_share_enabled_for_peer = client.is_screen_share_enabled_for_peer(key);

    let is_speaking = audio_level > 0.0;

    let audio_speaking_class = if is_speaking {
        "audio-indicator speaking"
    } else {
        "audio-indicator"
    };

    // Compute inline styles that scale with audio_level
    let tile_style = speak_style(audio_level);
    let mic_inline_style = mic_style(audio_level);

    // Full-bleed single peer (no screen share)
    if full_bleed && !is_screen_share_enabled_for_peer {
        let peer_video_div_id = Rc::new(format!("peer-video-{}-div", &key));
        let div_id_mobile = (*peer_video_div_id).clone();
        let div_id_pin = (*peer_video_div_id).clone();
        let canvas_id_crop = key.clone();
        let key_clone = key.clone();
        let peer_display_name_fb = peer_display_name.clone();
        let title = if is_host {
            format!("Host: {peer_user_id}")
        } else {
            peer_user_id.clone()
        };
        let full_bleed_class = if is_video_enabled_for_peer {
            "canvas-container video-on"
        } else {
            "canvas-container"
        };
        return rsx! {
            div {
                class: "grid-item full-bleed",
                id: "{peer_video_div_id}",
                div {
                    class: "{full_bleed_class}",
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
                        "{peer_display_name_fb}"
                        if is_host {
                            CrownIcon {}
                        }
                    }
                    div {
                        class: "{audio_speaking_class}",
                        style: "{mic_inline_style}",
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
                    // Glow overlay renders ON TOP of video content so the
                    // inset box-shadow is not hidden behind the canvas element.
                    div {
                        style: "{tile_style}",
                        class: "glow-overlay",
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
    let ss_name = format!("{}-screen", peer_display_name);

    let pv_div_mobile = (*peer_video_div_id).clone();
    let pv_div_pin = (*peer_video_div_id).clone();
    let pv_canvas_crop = key.clone();
    let key_clone = key.clone();
    let peer_display_name_grid = peer_display_name.clone();
    let title_grid = if is_host {
        format!("Host: {peer_user_id}")
    } else {
        peer_user_id.clone()
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
        {
            let grid_class = if is_video_enabled_for_peer {
                "canvas-container video-on"
            } else {
                "canvas-container"
            };
            let grid_tile_style = tile_style.clone();
            let grid_mic_style = mic_inline_style.clone();
            rsx! {
                div {
                    class: "grid-item",
                    id: "{peer_video_div_id}",
                    // One canvas for the User Video
                    div {
                        class: "{grid_class}",
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
                            title: "{title_grid}",
                            dir: "auto",
                            "{peer_display_name_grid}"
                            if is_host {
                                CrownIcon {}
                            }
                        }
                        div {
                            class: "{audio_speaking_class}",
                            style: "{grid_mic_style}",
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
                        // Glow overlay renders ON TOP of video content
                        div {
                            style: "{grid_tile_style}",
                            class: "glow-overlay",
                        }
                    }
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

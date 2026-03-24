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
use std::rc::Rc;
use videocall_client::VideoCallClient;
use web_sys::{window, HtmlCanvasElement};
use yew::prelude::*;
use yew::{html, Html};

/// Compute the inline CSS for the speaking glow on the canvas container.
/// Always returns explicit values so the glow is fully self-contained in the
/// inline style with zero dependency on CSS classes.
pub(crate) fn speak_style(audio_level: f32) -> String {
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
///
/// Two separate signals control different visual properties:
/// - `mic_audio_level` (held 1s after silence) controls the icon COLOR (green)
/// - `glow_audio_level` (raw, same as border) controls the drop-shadow GLOW
///
/// This way the icon stays green briefly after speech stops (via the held signal)
/// while the drop-shadow glow tracks the border glow exactly.
fn mic_style(mic_audio_level: f32, glow_audio_level: f32) -> String {
    if mic_audio_level <= 0.0 && glow_audio_level <= 0.0 {
        // Fully silent: fade out both color and filter
        return "color: inherit; filter: none; transition: color 5.0s ease-out, filter 1.5s ease-out;".to_string();
    }
    // Unreachable in practice: the mic hold timer guarantees mic_audio_level
    // stays positive at least as long as glow_audio_level. Handle defensively
    // by showing only the glow without the green icon color.
    if mic_audio_level <= 0.0 && glow_audio_level > 0.0 {
        let clamped = glow_audio_level.clamp(0.0, 1.0);
        let glow_i = clamped.sqrt();
        return format!(
            "color: inherit; \
             filter: drop-shadow(0 0 {:.0}px rgba(0, 255, 65, {:.2})) \
                     drop-shadow(0 0 {:.0}px rgba(0, 255, 65, {:.2})); \
             transition: color 5.0s ease-out, filter 0.15s ease-in;",
            8.0 + glow_i * 16.0,
            0.7 + glow_i * 0.3,
            3.0 + glow_i * 8.0,
            0.8 + glow_i * 0.2,
        );
    }
    if mic_audio_level > 0.0 && glow_audio_level <= 0.0 {
        // Held color (still green) but raw glow has faded — no drop-shadow
        return "color: #00ff41; filter: none; transition: color 0.05s ease-in, filter 1.5s ease-out;".to_string();
    }
    // Both positive: green color + scaled drop-shadow glow
    let clamped = glow_audio_level.clamp(0.0, 1.0);
    let glow_i = clamped.sqrt();
    format!(
        "color: #00ff41; \
         filter: drop-shadow(0 0 {:.0}px rgba(0, 255, 65, {:.2})) \
                 drop-shadow(0 0 {:.0}px rgba(0, 255, 65, {:.2})); \
         transition: color 0.05s ease-in, filter 0.15s ease-in;",
        8.0 + glow_i * 16.0, // primary drop-shadow blur: 8–24px
        0.7 + glow_i * 0.3,  // primary drop-shadow alpha: 0.7–1.0
        3.0 + glow_i * 8.0,  // secondary (tight) glow blur: 3–11px
        0.8 + glow_i * 0.2,  // secondary glow alpha: 0.8–1.0
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
    mic_audio_level: f32,
    host_user_id: Option<&str>,
) -> Html {
    let peer_user_id = client.get_peer_user_id(key).unwrap_or_else(|| key.clone());
    let peer_display_name = client
        .get_peer_display_name(key)
        .unwrap_or_else(|| peer_user_id.clone());

    // Compare authenticated user_id (from JWT/DB) instead of user-chosen display name
    // to prevent spoofing the host crown icon.
    let is_host = host_user_id.map(|h| h == peer_user_id).unwrap_or(false);
    let allowed = users_allowed_to_stream().unwrap_or_default();
    if !allowed.is_empty() && !allowed.contains(&peer_user_id) {
        return html! {};
    }

    let is_video_enabled_for_peer = client.is_video_enabled_for_peer(key);
    let is_audio_enabled_for_peer = client.is_audio_enabled_for_peer(key);
    let is_screen_share_enabled_for_peer = client.is_screen_share_enabled_for_peer(key);

    let is_speaking = mic_audio_level > 0.0;

    // Compute inline styles: border glow uses raw audio_level,
    // mic icon uses mic_audio_level (held for 1s after silence in Rust)
    let tile_style = speak_style(audio_level);
    let mic_inline_style = mic_style(mic_audio_level, audio_level);

    // Full-bleed single peer (no screen share)
    if full_bleed && !is_screen_share_enabled_for_peer {
        let peer_video_div_id = Rc::new(format!("peer-video-{}-div", &key));

        return html! {
            <div class="grid-item full-bleed" id={(*peer_video_div_id).clone()}>
                <div class={classes!("canvas-container", if is_video_enabled_for_peer { "video-on" } else { "" })}
                    onclick={Callback::from({
                        let div_id = (*peer_video_div_id).clone();
                        move |_| { if is_mobile_viewport() { toggle_pinned_div(&div_id) } }
                    })}
                >
                    { if is_video_enabled_for_peer { html!{ <UserVideo id={key.clone()} hidden={false}/> } } else { html!{ <div class=""><div class="placeholder-content"><PeerIcon/><span class="placeholder-text">{"Camera Off"}</span></div></div> } } }
                    <h4 class="floating-name" title={if is_host { format!("Host: {peer_user_id}") } else { peer_user_id.clone() }} dir={"auto"}>
                        {peer_display_name.clone()}
                        if is_host { <CrownIcon /> }
                    </h4>
                    <div class={classes!("audio-indicator", if is_speaking { "speaking" } else { "" })} style={mic_inline_style.clone()}><MicIcon muted={!is_audio_enabled_for_peer}/></div>
                    <button onclick={Callback::from({ let canvas_id = key.clone(); move |_| toggle_canvas_crop(&canvas_id) })} class="crop-icon"><CropIcon/></button>
                    <button onclick={Callback::from(move |_| { toggle_pinned_div(&(*peer_video_div_id).clone()); })} class="pin-icon"><PushPinIcon/></button>
                    // Glow overlay renders ON TOP of video content
                    <div class="glow-overlay" style={tile_style.clone()}></div>
                </div>
            </div>
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

    html! {
        <>
            // Canvas for Screen share.
            if is_screen_share_enabled_for_peer {
                <div class={screen_share_css} id={(*screen_share_div_id).clone()}>
                    <div class={classes!("canvas-container", "video-on")} onclick={Callback::from({
                        let div_id = (*screen_share_div_id).clone();
                        move |_| { if is_mobile_viewport() { toggle_pinned_div(&div_id) } }
                    })}>
                        <ScreenCanvas peer_id={key.clone()} />
                        <h4 class="floating-name" title={format!("{}-screen", &peer_display_name)} dir={"auto"}>{format!("{}-screen", &peer_display_name)}</h4>
                        <button onclick={Callback::from({ let canvas_id = format!("screen-share-{}", key.clone()); move |_| toggle_canvas_crop(&canvas_id) })} class="crop-icon">
                            <CropIcon/>
                        </button>
                        <button onclick={Callback::from(move |_| { toggle_pinned_div(&(*screen_share_div_id).clone()); })} class="pin-icon">
                            <PushPinIcon/>
                        </button>
                    </div>
                </div>
            } else {
                <></>
            }
            <div class="grid-item" id={(*peer_video_div_id).clone()}>
                // One canvas for the User Video
                <div class={classes!("canvas-container", if is_video_enabled_for_peer { "video-on" } else { "" })}
                    onclick={Callback::from({
                        let div_id = (*peer_video_div_id).clone();
                        move |_| { if is_mobile_viewport() { toggle_pinned_div(&div_id) } }
                    })}
                >
                    if is_video_enabled_for_peer {
                        <UserVideo id={key.clone()} hidden={false}></UserVideo>
                    } else {
                        <div class="placeholder-content">
                            <PeerIcon/>
                            <span class="placeholder-text">{"Video Disabled"}</span>
                        </div>
                    }
                    <h4 class="floating-name" title={if is_host { format!("Host: {peer_user_id}") } else { peer_user_id.clone() }} dir={"auto"}>
                        {peer_display_name.clone()}
                        if is_host { <CrownIcon /> }
                    </h4>
                    <div class={classes!("audio-indicator", if is_speaking { "speaking" } else { "" })} style={mic_inline_style}>
                        <MicIcon muted={!is_audio_enabled_for_peer}/>
                    </div>
                    <button onclick={Callback::from({ let canvas_id = key.clone(); move |_| toggle_canvas_crop(&canvas_id) })} class="crop-icon">
                        <CropIcon/>
                    </button>
                    <button onclick={Callback::from(move |_| { toggle_pinned_div(&(*peer_video_div_id).clone()); })} class="pin-icon">
                        <PushPinIcon/>
                    </button>
                    // Glow overlay renders ON TOP of video content
                    <div class="glow-overlay" style={tile_style}></div>
                </div>
            </div>
        </>
    }
}

// props for the video component
#[derive(Properties, Debug, PartialEq)]
struct UserVideoProps {
    pub id: String,
    pub hidden: bool,
}

// user video functional component
#[function_component(UserVideo)]
fn user_video(props: &UserVideoProps) -> Html {
    let video_ref = use_node_ref();
    let client = use_context::<VideoCallClientCtx>().expect("VideoCallClient context missing");

    // Pass canvas reference to client when mounted
    {
        let video_ref = video_ref.clone();
        let peer_id = props.id.clone();
        let client = client.clone();

        use_effect_with(video_ref.clone(), move |_| {
            if let Some(canvas) = video_ref.cast::<HtmlCanvasElement>() {
                if let Err(e) = client.set_peer_video_canvas(&peer_id, canvas) {
                    log::debug!("Canvas not yet ready for peer {peer_id}: {e:?}");
                }
            }
            || ()
        });
    }

    html! {
        <canvas ref={video_ref} id={props.id.clone()} hidden={props.hidden} class={classes!("uncropped")}></canvas>
    }
}

// Screen canvas component
#[derive(Properties, Debug, PartialEq)]
struct ScreenCanvasProps {
    pub peer_id: String,
}

#[function_component(ScreenCanvas)]
fn screen_canvas(props: &ScreenCanvasProps) -> Html {
    let canvas_ref = use_node_ref();
    let client = use_context::<VideoCallClientCtx>().expect("VideoCallClient context missing");

    // Pass canvas reference to client when mounted
    {
        let canvas_ref = canvas_ref.clone();
        let peer_id = props.peer_id.clone();
        let client = client.clone();

        use_effect_with(canvas_ref.clone(), move |_| {
            if let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() {
                if let Err(e) = client.set_peer_screen_canvas(&peer_id, canvas) {
                    log::debug!("Screen canvas not yet ready for peer {peer_id}: {e:?}");
                }
            }
            || ()
        });
    }

    html! {
        <canvas ref={canvas_ref} id={format!("screen-share-{}", props.peer_id)} class={classes!("uncropped")}></canvas>
    }
}

fn toggle_pinned_div(div_id: &str) {
    if let Some(div) = window()
        .and_then(|w| w.document())
        .and_then(|doc| doc.get_element_by_id(div_id))
    {
        // if the div does not have the grid-item-pinned css class, add it to it
        if !div.class_list().contains("grid-item-pinned") {
            div.class_list().add_1("grid-item-pinned").unwrap();
        } else {
            // else remove it
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

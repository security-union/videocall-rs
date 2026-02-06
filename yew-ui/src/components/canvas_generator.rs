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

pub fn generate_for_peer(client: &VideoCallClient, key: &String, full_bleed: bool, is_speaking: bool) -> Html {
    let allowed = users_allowed_to_stream().unwrap_or_default();
    if !allowed.is_empty() && !allowed.iter().any(|host| host == key) {
        return html! {};
    }

    let is_video_enabled_for_peer = client.is_video_enabled_for_peer(key);
    let is_audio_enabled_for_peer = client.is_audio_enabled_for_peer(key);
    let is_screen_share_enabled_for_peer = client.is_screen_share_enabled_for_peer(key);
    log::info!("🟢 UI8: peer {} is_speaking={}", key, is_speaking);

    let border_style = if is_speaking {
        "border: 3px solid orange; border-radius: 8px; transition: all 0.2s;"
    } else {
        ""
    };

    // Full-bleed single peer (no screen share)
    if full_bleed && !is_screen_share_enabled_for_peer {
        let peer_video_div_id = Rc::new(format!("peer-video-{}-div", &key));

        return html! {
            <div class="grid-item full-bleed" id={(*peer_video_div_id).clone()}>
                <div class={classes!("canvas-container", if is_video_enabled_for_peer { "video-on" } else { "" })}
                    style={border_style}
                    onclick={Callback::from({
                        let div_id = (*peer_video_div_id).clone();
                        move |_| { if is_mobile_viewport() { toggle_pinned_div(&div_id) } }
                    })}
                >
                    { if is_video_enabled_for_peer { html!{ <UserVideo id={key.clone()} hidden={false}/> } } else { html!{ <div class=""><div class="placeholder-content"><PeerIcon/><span class="placeholder-text">{"Camera Off"}</span></div></div> } } }
                    <h4 class="floating-name" title={key.clone()} dir={"auto"}>{key.clone()}</h4>
                    <div class="audio-indicator"><MicIcon muted={!is_audio_enabled_for_peer}/></div>
                    <button onclick={Callback::from({ let canvas_id = key.clone(); move |_| toggle_canvas_crop(&canvas_id) })} class="crop-icon"><CropIcon/></button>
                    <button onclick={Callback::from(move |_| { toggle_pinned_div(&(*peer_video_div_id).clone()); })} class="pin-icon"><PushPinIcon/></button>
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
                        <h4 class="floating-name" title={format!("{}-screen", &key)} dir={"auto"}>{format!("{}-screen", &key)}</h4>
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
                    style={border_style}
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
                    <h4 class="floating-name" title={key.clone()} dir={"auto"}>{key.clone()}</h4>
                    <div class="audio-indicator">
                        <MicIcon muted={!is_audio_enabled_for_peer}/>
                    </div>
                    <button onclick={Callback::from({ let canvas_id = key.clone(); move |_| toggle_canvas_crop(&canvas_id) })} class="crop-icon">
                        <CropIcon/>
                    </button>
                    <button onclick={Callback::from(move |_| { toggle_pinned_div(&(*peer_video_div_id).clone()); })} class="pin-icon">
                        <PushPinIcon/>
                    </button>
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
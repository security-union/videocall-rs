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

use crate::components::icons::mic::MicIcon;
use crate::components::icons::peer::PeerIcon;
use crate::components::icons::push_pin::PushPinIcon;
use crate::constants::USERS_ALLOWED_TO_STREAM;
use std::rc::Rc;
use videocall_client::VideoCallClient;
use wasm_bindgen::JsCast;
use web_sys::{window, CanvasRenderingContext2d, HtmlCanvasElement};
use yew::prelude::*;
use yew::virtual_dom::VNode;
use yew::{html, Html};

pub fn generate(client: &VideoCallClient, peers: Vec<String>) -> Vec<VNode> {
    log::info!("Generating canvas for peers: {peers:?}");
    let peers_clone = peers.clone();
    peers_clone
        .into_iter()
        .map(move |key| {
            if !USERS_ALLOWED_TO_STREAM.is_empty() && !USERS_ALLOWED_TO_STREAM.contains(&key) {
                return html! {};
            }
            let screen_share_css = if client.is_awaiting_peer_screen_frame(&key) {
                "grid-item hidden"
            } else {
                "grid-item"
            };
            let is_video_enabled_for_peer = client.is_video_enabled_for_peer(&key);
            let is_screen_share_enabled_for_peer = client.is_screen_share_enabled_for_peer(&key);
            let is_audio_enabled_for_peer = client.is_audio_enabled_for_peer(&key);

            let key_clone_for_button = key.clone();
            let screen_share_div_id = Rc::new(format!("screen-share-{}-div", &key));
            let peer_video_div_id = Rc::new(format!("peer-video-{}-div", &key));
            log::info!("Rendering pin icon for peer: {key}");

            html! {
                <>
                    if is_screen_share_enabled_for_peer {
                        <div class={screen_share_css} id={(*screen_share_div_id).clone()}>
                            <div class="canvas-container">
                                <canvas id={format!("screen-share-{}", &key)}></canvas>
                                <h4 class="floating-name">{format!("{}-screen", &key)}</h4>
                                <button onclick={Callback::from(move |_| {
                                    log::info!("Pin button clicked for: {key_clone_for_button}");
                                    toggle_pinned_div(&(*screen_share_div_id).clone());
                                })} class="pin-button">
                                    <div class="pin-icon">
                                        <PushPinIcon />
                                    </div>
                                </button>
                            </div>
                        </div>
                    } else {
                        <>
                        </>
                    }

                    <div class="grid-item" id={(*peer_video_div_id).clone()}>
                        // One canvas for the User Video
                        <div class="canvas-container">
                            if is_video_enabled_for_peer {
                                <UserVideo id={key.clone()} hidden={false}></UserVideo>
                            } else {
                                <div class="video-placeholder">
                                    <div class="placeholder-content">
                                        <PeerIcon/>
                                        <span class="placeholder-text">{"Video Disabled"}</span>
                                    </div>
                                </div>
                            }
                            <h4 class="floating-name">{key.clone()}</h4>
                            <div class="audio-indicator">
                                <MicIcon muted={!is_audio_enabled_for_peer}/>
                            </div>
                            <button onclick={
                                Callback::from(move |_| {
                                toggle_pinned_div(&(*peer_video_div_id).clone());
                            })} class="pin-icon">
                                <PushPinIcon/>
                            </button>
                        </div>
                    </div>
                </>
            }
        })
        .collect()
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
    // create use_effect hook that gets called only once and sets a thumbnail
    // for the user video
    let video_ref = use_state(NodeRef::default);
    let video_ref_clone = video_ref.clone();
    use_effect_with(vec![props.id.clone()], move |_| {
        // Set thumbnail for the video
        let video = (*video_ref_clone).cast::<HtmlCanvasElement>().unwrap();
        let ctx = video
            .get_context("2d")
            .unwrap()
            .unwrap()
            .unchecked_into::<CanvasRenderingContext2d>();
        ctx.clear_rect(0.0, 0.0, video.width() as f64, video.height() as f64);
        || ()
    });

    html! {
        <canvas ref={(*video_ref).clone()} id={props.id.clone()} hidden={props.hidden}></canvas>
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

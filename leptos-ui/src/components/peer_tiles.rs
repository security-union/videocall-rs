// SPDX-License-Identifier: MIT OR Apache-2.0

use leptos::html;
use leptos::prelude::*;
use leptos::web_sys;
use videocall_client::VideoCallClient;
use wasm_bindgen::JsCast;

pub fn generate_for_peer_view(
    client: VideoCallClient,
    peer_id: String,
    full_bleed: bool,
) -> impl IntoView {
    let is_video_enabled_for_peer = client.is_video_enabled_for_peer(&peer_id);
    let is_audio_enabled_for_peer = client.is_audio_enabled_for_peer(&peer_id);
    let is_screen_share_enabled_for_peer = client.is_screen_share_enabled_for_peer(&peer_id);

    if full_bleed && !is_screen_share_enabled_for_peer {
        let peer_video_div_id = format!("peer-video-{}-div", &peer_id);
        let peer_video_div_id_clone = peer_video_div_id.clone();
        view! {
            <div class="grid-item full-bleed" id={peer_video_div_id.clone()}>
                <div class=move || if is_video_enabled_for_peer { "canvas-container video-on" } else { "canvas-container" }
                    on:click=move |_| { if is_mobile_viewport() { toggle_pinned_div(&peer_video_div_id_clone); } }>
                    {move || if is_video_enabled_for_peer {
                        view!{ <UserVideo id=peer_id.clone() hidden=false/> }.into_any()
                    } else {
                        view!{ <div class=""><div class="placeholder-content"><span class="placeholder-text">{"Camera Off"}</span></div></div> }.into_any()
                    }}
                    <h4 class="floating-name" title=peer_id.clone() dir="auto">{peer_id.clone()}</h4>
                    <div class="audio-indicator">{if is_audio_enabled_for_peer { ().into_any() } else { view!{<span>{"🔇"}</span>}.into_any() }}</div>
                    <button on:click=move |_| { toggle_pinned_div(&peer_video_div_id) } class="pin-icon">{"📌"}</button>
                </div>
            </div>
        }.into_any()
    } else {
        let screen_share_div_id = format!("screen-share-{}-div", &peer_id);
        let peer_video_div_id = format!("peer-video-{}-div", &peer_id);
        let screen_share_div_id_for_click = screen_share_div_id.clone();
        let screen_share_div_id_for_btn = screen_share_div_id.clone();
        let peer_video_div_id_for_click = peer_video_div_id.clone();
        let peer_video_div_id_for_btn = peer_video_div_id.clone();
        view! {
            <>
            {move || if is_screen_share_enabled_for_peer { view!{
                <div class={move || if client.is_awaiting_peer_screen_frame(&peer_id) { "grid-item hidden" } else { "grid-item" }} id={screen_share_div_id.clone()}>
                    <div class="canvas-container video-on" on:click=move |_| { if is_mobile_viewport() { toggle_pinned_div(&screen_share_div_id_for_click); } }>
                        <canvas id=format!("screen-share-{}", &peer_id)></canvas>
                        <h4 class="floating-name" title=format!("{}-screen", &peer_id) dir="auto">{format!("{}-screen", &peer_id)}</h4>
                        <button on:click=move |_| { toggle_pinned_div(&screen_share_div_id_for_btn) } class="pin-icon">{"📌"}</button>
                    </div>
                </div>
            }.into_any() } else { ().into_any() }}

            <div class="grid-item" id={peer_video_div_id}>
                <div class=move || if is_video_enabled_for_peer { "canvas-container video-on" } else { "canvas-container" }
                    on:click=move |_| { if is_mobile_viewport() { toggle_pinned_div(&peer_video_div_id_for_click); } }>
                    {move || if is_video_enabled_for_peer {
                        view!{ <UserVideo id=peer_id.clone() hidden=false/> }.into_any()
                    } else {
                        view!{
                            <div class="placeholder-content">
                                <span class="placeholder-text">{"Video Disabled"}</span>
                            </div>
                        }.into_any()
                    }}
                    <h4 class="floating-name" title=peer_id.clone() dir="auto">{peer_id.clone()}</h4>
                    <div class="audio-indicator">{if is_audio_enabled_for_peer { ().into_any() } else { view!{<span>{"🔇"}</span>}.into_any() }}</div>
                    <button on:click=move |_| { toggle_pinned_div(&peer_video_div_id_for_btn) } class="pin-icon">{"📌"}</button>
                </div>
            </div>
            </>
        }.into_any()
    }
}

#[component]
fn UserVideo(id: String, hidden: bool) -> impl IntoView {
    let canvas_ref: NodeRef<html::Canvas> = NodeRef::new();
    Effect::new(move |_| {
        if let Some(canvas) = canvas_ref.get_untracked() {
            let ctx = canvas
                .get_context("2d")
                .ok()
                .flatten()
                .and_then(|c| c.dyn_into::<web_sys::CanvasRenderingContext2d>().ok());
            if let Some(ctx) = ctx {
                ctx.clear_rect(0.0, 0.0, canvas.width() as f64, canvas.height() as f64);
            }
        }
    });

    view! { <canvas node_ref=canvas_ref id=id hidden=hidden></canvas> }
}

fn toggle_pinned_div(div_id: &str) {
    if let Some(div) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|doc| doc.get_element_by_id(div_id))
    {
        let _ = if !div.class_list().contains("grid-item-pinned") {
            div.class_list().add_1("grid-item-pinned")
        } else {
            div.class_list().remove_1("grid-item-pinned")
        };
    }
}

fn is_mobile_viewport() -> bool {
    web_sys::window()
        .and_then(|w| w.inner_width().ok())
        .and_then(|v| v.as_f64())
        .map(|px| px < 768.0)
        .unwrap_or(false)
}

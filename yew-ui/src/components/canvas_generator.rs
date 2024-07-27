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
    peers
        .iter()
        .map(|key| {
            if !USERS_ALLOWED_TO_STREAM.is_empty()
                && !USERS_ALLOWED_TO_STREAM.iter().any(|host| host == key)
            {
                return html! {};
            }
            let screen_share_css = if client.is_awaiting_peer_screen_frame(key) {
                "grid-item hidden"
            } else {
                "grid-item"
            };
            let screen_share_div_id = Rc::new(format!("screen-share-{}-div", &key));
            let peer_video_div_id = Rc::new(format!("peer-video-{}-div", &key));
            html! {
                <>
                    <div class={screen_share_css} id={(*screen_share_div_id).clone()}>
                        // Canvas for Screen share.
                        <div class="canvas-container">
                            <canvas id={format!("screen-share-{}", &key)}></canvas>
                            <h4 class="floating-name">{format!("{}-screen", &key)}</h4>
                            <button onclick={Callback::from(move |_| {
                                toggle_pinned_div(&(*screen_share_div_id).clone());
                            })} class="pin-icon">
                                <PushPinIcon/>
                            </button>
                        </div>
                    </div>
                    <div class="grid-item" id={(*peer_video_div_id).clone()}>
                        // One canvas for the User Video
                        <div class="canvas-container">
                            <UserVideo id={key.clone()}></UserVideo>
                            <h4 class="floating-name">{key.clone()}</h4>
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
        <canvas ref={(*video_ref).clone()} id={props.id.clone()}></canvas>
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

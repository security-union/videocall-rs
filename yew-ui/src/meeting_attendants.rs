use std::collections::HashMap;

use crate::constants::VIDEO_CODEC;
use crate::host::EncodedVideoChunkTypeWrapper;
use crate::host::MediaPacketWrapper;
use crate::{constants::ACTIX_WEBSOCKET, host::Host};
use anyhow::anyhow;
use gloo_console::log;
use js_sys::Uint8Array;
use protobuf::Message;
use types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::*;
use yew::prelude::*;
use yew::virtual_dom::VNode;
use yew::{html, Component, Context, Html};
use yew_websocket::websocket::{WebSocketService, WebSocketStatus, WebSocketTask};

pub enum WsAction {
    Connect,
    Connected,
    Disconnect,
    Lost,
}

pub enum Msg {
    WsAction(WsAction),
    WsReady(MediaPacketWrapper),
    OnFrame(MediaPacket),
}

impl From<WsAction> for Msg {
    fn from(action: WsAction) -> Self {
        Msg::WsAction(action)
    }
}

#[derive(Properties, Debug, PartialEq)]
pub struct AttendandsComponentProps {
    #[prop_or_default]
    pub id: String,

    #[prop_or_default]
    pub media_packet: MediaPacket,

    #[prop_or_default]
    pub email: String,
}

pub struct AttendandsComponent {
    pub ws: Option<WebSocketTask>,
    pub media_packet: MediaPacket,
    pub connected: bool,
    pub connected_peers: HashMap<String, ClientSubscription>,
}

pub struct ClientSubscription {
    pub video_decoder: VideoDecoder,
    pub waiting_for_keyframe: bool,
}

impl Component for AttendandsComponent {
    type Message = Msg;
    type Properties = AttendandsComponentProps;

    fn create(_ctx: &Context<Self>) -> Self {
        let connected_peers: HashMap<String, ClientSubscription> = HashMap::new();
        Self {
            ws: None,
            connected: false,
            media_packet: MediaPacket::default(),
            connected_peers,
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::WsAction(action) => match action {
                WsAction::Connect => {
                    let callback = ctx.link().callback(|data| Msg::WsReady(data));
                    let notification = ctx.link().batch_callback(|status| match status {
                        WebSocketStatus::Opened => Some(WsAction::Connected.into()),
                        WebSocketStatus::Closed | WebSocketStatus::Error => {
                            Some(WsAction::Lost.into())
                        }
                    });
                    let meeting_id = ctx.props().id.clone();
                    let email = ctx.props().email.clone();
                    let url = format!("{}/{}/{}", ACTIX_WEBSOCKET.to_string(), email, meeting_id);
                    let task = WebSocketService::connect(&url, callback, notification).unwrap();
                    self.ws = Some(task);
                    true
                }
                WsAction::Disconnect => {
                    self.ws.take();
                    self.connected = false;
                    true
                }
                WsAction::Connected => {
                    self.connected = true;
                    true
                }
                WsAction::Lost => {
                    self.ws = None;
                    self.connected = false;
                    true
                }
            },
            Msg::WsReady(response) => {
                let data = response.0;
                if data.video.is_empty() {
                    log!("dropping bad video packet");
                    return false;
                }
                let email = data.email.clone();

                if let Some(peer) = self.connected_peers.get_mut(&email.clone()) {
                    let video_data =
                        Uint8Array::new_with_length(data.video.len().try_into().unwrap());
                    let chunk_type = EncodedVideoChunkTypeWrapper::from(data.video_type).0;
                    video_data.copy_from(&data.video.into_boxed_slice());
                    let encoded_video_chunk = EncodedVideoChunk::new(&EncodedVideoChunkInit::new(
                        &video_data,
                        data.video_timestamp,
                        chunk_type,
                    ))
                    .unwrap();
                    if peer.waiting_for_keyframe && chunk_type == EncodedVideoChunkType::Key
                        || !peer.waiting_for_keyframe
                    {
                        peer.video_decoder.decode(&encoded_video_chunk);
                        peer.waiting_for_keyframe = false;
                    } else {
                        log!("dropping frame");
                    }
                    false
                } else {
                    let error_video = Closure::wrap(Box::new(move |e: JsValue| {
                        log!(&e);
                    })
                        as Box<dyn FnMut(JsValue)>);
                    let output = Closure::wrap(Box::new(move |original_chunk: JsValue| {
                        let chunk = Box::new(original_chunk);
                        let video_chunk = chunk.unchecked_into::<VideoFrame>();
                        let width = video_chunk.coded_width();
                        let height = video_chunk.coded_height();
                        let render_canvas = window()
                            .unwrap()
                            .document()
                            .unwrap()
                            .get_element_by_id(&email.clone())
                            .unwrap()
                            .unchecked_into::<HtmlCanvasElement>();
                        render_canvas.set_width(width as u32);
                        render_canvas.set_height(height as u32);
                        let ctx = render_canvas
                            .get_context("2d")
                            .unwrap()
                            .unwrap()
                            .unchecked_into::<CanvasRenderingContext2d>();
                        let video_chunk = video_chunk.unchecked_into::<HtmlImageElement>();
                        if let Err(e) =
                            ctx.draw_image_with_html_image_element(&video_chunk, 0.0, 0.0)
                        {
                            log!("error ", e);
                        }
                        video_chunk.unchecked_into::<VideoFrame>().close();
                    }) as Box<dyn FnMut(JsValue)>);
                    let video_decoder = VideoDecoder::new(&VideoDecoderInit::new(
                        error_video.as_ref().unchecked_ref(),
                        output.as_ref().unchecked_ref(),
                    ))
                    .unwrap();
                    video_decoder.configure(&VideoDecoderConfig::new(&VIDEO_CODEC));
                    self.connected_peers.insert(
                        data.email.clone(),
                        ClientSubscription {
                            video_decoder,
                            waiting_for_keyframe: true,
                        },
                    );
                    error_video.forget();
                    output.forget();
                    true
                }
            }
            Msg::OnFrame(media) => {
                if let Some(ws) = self.ws.as_mut() {
                    if self.connected {
                        let bytes = media.write_to_bytes().map_err(|w| anyhow!("{:?}", w));
                        ws.send_binary(bytes);
                    } else {
                        log!("disconnected");
                    }
                }
                false
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let email = ctx.props().email.clone();
        let on_frame = ctx
            .link()
            .callback(|frame: MediaPacket| Msg::OnFrame(frame));
        let rows: Vec<VNode> = self
            .connected_peers
            .iter()
            .map(|(key, _value)| {
                html! {
                    <div class="grid-item">
                        <canvas id={key.clone()}></canvas>
                        <h4 class="floating-name">{key.clone()}</h4>
                    </div>
                }
            })
            .collect();
        html! {
            <div class="grid-container">
                { rows }
                <nav class="grid-item menu">
                    <div class="controls">
                        <button disabled={self.ws.is_some()}
                                onclick={ctx.link().callback(|_| WsAction::Connect)}>
                            { "Connect" }
                        </button>
                        <button disabled={self.ws.is_none()}
                                onclick={ctx.link().callback(|_| WsAction::Disconnect)}>
                            { "Close" }
                        </button>
                    </div>
                    <Host on_frame={on_frame} email={email.clone()}/>
                    <h4 class="floating-name">{email}</h4>
                </nav>
            </div>
        }
    }
}

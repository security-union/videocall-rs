use crate::meeting_self::MediaPacketWrapper;
use anyhow::anyhow;
use gloo_console::log;
use js_sys::Uint8Array;
use protobuf::Message;
use serde_derive::{Deserialize, Serialize};
use types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::*;
use yew::prelude::*;
use yew::{html, Component, Context, Html};
use yew_websocket::macros::Json;
use yew_websocket::websocket::{Binary, WebSocketService, WebSocketStatus, WebSocketTask};

use crate::constants::{VIDEO_CODEC, VIDEO_HEIGHT, VIDEO_WIDTH};
use crate::meeting_self::EncodedVideoChunkTypeWrapper;
use crate::{constants::ACTIX_WEBSOCKET, meeting_self::HostComponent};

pub enum WsAction {
    Connect,
    SendData(),
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
    pub fetching: bool,
    pub data: Option<u32>,
    pub ws: Option<WebSocketTask>,
    pub media_packet: MediaPacket,
    pub connected: bool,
    pub video_decoder: Option<VideoDecoder>,
    pub waiting_for_keyframe: bool,
}

impl Component for AttendandsComponent {
    type Message = Msg;
    type Properties = AttendandsComponentProps;

    fn create(_ctx: &Context<Self>) -> Self {
        Self {
            fetching: false,
            data: None,
            ws: None,
            connected: false,
            media_packet: MediaPacket::default(),
            video_decoder: None,
            waiting_for_keyframe: true,
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
                WsAction::SendData() => {
                    let media = MediaPacket::default();
                    let bytes = media.write_to_bytes().map_err(|w| anyhow!("{:?}", w));
                    self.ws.as_mut().unwrap().send_binary(bytes);
                    false
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
                }
                log!("video length", data.video.len());

                if let Some(decoder) = &self.video_decoder {
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
                    if self.waiting_for_keyframe && chunk_type == EncodedVideoChunkType::Key
                        || !self.waiting_for_keyframe
                    {
                        decoder.decode(&encoded_video_chunk);
                        self.waiting_for_keyframe = false;
                    } else {
                        log!("dropping frame");
                    }
                } else {
                    let error_video = Closure::wrap(Box::new(move |e: JsValue| {
                        log!(&e);
                    })
                        as Box<dyn FnMut(JsValue)>);
                    let output = Closure::wrap(Box::new(move |original_chunk: JsValue| {
                        log!("decoded video chunk");
                        let chunk = Box::new(original_chunk);
                        let video_chunk = chunk.clone().unchecked_into::<HtmlImageElement>();
                        // let width = Reflect::get(&chunk.clone(), &JsString::from("codedWidth"))
                        //     .unwrap()
                        //     .as_f64()
                        //     .unwrap();
                        // let height = Reflect::get(&chunk.clone(), &JsString::from("codedHeight"))
                        //     .unwrap()
                        //     .as_f64()
                        //     .unwrap();
                        // let render_canvas = window()
                        //     .unwrap()
                        //     .document()
                        //     .unwrap()
                        //     .get_element_by_id("render")
                        //     .unwrap()
                        //     .unchecked_into::<HtmlCanvasElement>();
                        // render_canvas.set_width(width as u32);
                        // render_canvas.set_height(height as u32);
                        // let ctx = render_canvas
                        //     .get_context("2d")
                        //     .unwrap()
                        //     .unwrap()
                        //     .unchecked_into::<CanvasRenderingContext2d>();
                        // ctx.draw_image_with_html_image_element(
                        //     &video_chunk,
                        //     0.0,
                        //     0.0
                        // );
                        // video_chunk.unchecked_into::<VideoFrame>().close();
                    }) as Box<dyn FnMut(JsValue)>);
                    let video_decoder = VideoDecoder::new(&VideoDecoderInit::new(
                        error_video.as_ref().unchecked_ref(),
                        output.as_ref().unchecked_ref(),
                    ))
                    .unwrap();
                    video_decoder.configure(&VideoDecoderConfig::new(&VIDEO_CODEC));
                    self.video_decoder = Some(video_decoder);
                    self.waiting_for_keyframe = true;
                    error_video.forget();
                    output.forget();
                }

                true
            }
            Msg::OnFrame(media) => {
                // Send image to the server.
                if let Some(ws) = self.ws.as_mut() {
                    if self.connected {
                        let bytes = media.write_to_bytes().map_err(|w| anyhow!("{:?}", w));
                        ws.send_binary(bytes);
                    } else {
                        log!("disconnected");
                    }
                } else {
                    // log!("No websocket!!!!");
                }
                false
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let media_packet = ctx.props().media_packet.clone();
        let on_frame = ctx.link().callback(|frame: MediaPacket| {
            // log!("on meeting attendant callback");
            Msg::OnFrame(frame)
        });
        html! {
            <div>
                <nav class="menu">
                    <button disabled={self.ws.is_some()}
                            onclick={ctx.link().callback(|_| WsAction::Connect)}>
                        { "Connect To WebSocket" }
                    </button>
                    <button disabled={self.ws.is_none()}
                            onclick={ctx.link().callback(|_| WsAction::SendData())}>
                        { "Send To WebSocket [binary]" }
                    </button>
                    <button disabled={self.ws.is_none()}
                            onclick={ctx.link().callback(|_| WsAction::Disconnect)}>
                        { "Close WebSocket connection" }
                    </button>
                </nav>
                <HostComponent media_packet={media_packet} on_frame={on_frame}/>
            </div>
        }
    }
}

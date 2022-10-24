use std::collections::HashMap;

use crate::constants::VIDEO_CODEC;
use crate::model::configure_audio_context;
use crate::model::transform_audio_chunk;
use crate::model::AudioSampleFormatWrapper;
use crate::model::EncodedVideoChunkTypeWrapper;
use crate::model::MediaPacketWrapper;
use crate::{components::host::Host, constants::ACTIX_WEBSOCKET};
use anyhow::anyhow;
use gloo_console::log;
use js_sys::*;
use protobuf::Message;
use types::protos::rust::media_packet::media_packet;
use types::protos::rust::media_packet::MediaPacket;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use web_sys::*;
use yew::prelude::*;
use yew::virtual_dom::VNode;
use yew::{html, Component, Context, Html};
use yew_websocket::websocket::{WebSocketService, WebSocketStatus, WebSocketTask};

// This is important https://plnkr.co/edit/1yQd8ozGXlV9bwK6?preview
// https://github.com/WebAudio/web-audio-api-v2/issues/133

#[derive(Debug)]
pub enum WsAction {
    Connect,
    Connected,
    Disconnect,
    Lost,
}

pub enum Msg {
    WsAction(WsAction),
    OnInboundMedia(MediaPacketWrapper),
    OnOutboundVideoPacket(MediaPacket),
    OnOutboundAudioPacket(AudioData),
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
    pub outbound_audio_buffer: [u8; 2000],
}

pub struct ClientSubscription {
    pub video_decoder: VideoDecoder,
    pub audio_output: Box<dyn FnMut(AudioData)>,
    pub waiting_for_video_keyframe: bool,
    pub waiting_for_audio_keyframe: bool,
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
            outbound_audio_buffer: [0; 2000],
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::WsAction(action) => match action {
                WsAction::Connect => {
                    let callback = ctx.link().callback(|data| Msg::OnInboundMedia(data));
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
                    log!("Disconnect");
                    self.ws.take();
                    self.connected = false;
                    true
                }
                WsAction::Connected => {
                    log!("Connected");
                    self.connected = true;
                    true
                }
                WsAction::Lost => {
                    log!("Lost");
                    self.ws = None;
                    self.connected = false;
                    false
                }
            },
            Msg::OnInboundMedia(response) => {
                let data = response.0;
                let email = data.email.clone();
                if let Some(peer) = self.connected_peers.get_mut(&email.clone()) {
                    match data.media_type.unwrap() {
                        media_packet::MediaType::VIDEO => {
                            let video_data =
                                Uint8Array::new_with_length(data.video.len().try_into().unwrap());
                            let chunk_type = EncodedVideoChunkTypeWrapper::from(data.video_type).0;
                            video_data.copy_from(&data.video.into_boxed_slice());
                            let mut video_chunk =
                                EncodedVideoChunkInit::new(&video_data, data.timestamp, chunk_type);
                            video_chunk.duration(data.duration);
                            let encoded_video_chunk = EncodedVideoChunk::new(&video_chunk).unwrap();
                            if peer.waiting_for_video_keyframe
                                && chunk_type == EncodedVideoChunkType::Key
                                || !peer.waiting_for_video_keyframe
                            {
                                peer.video_decoder.decode(&encoded_video_chunk);
                                peer.waiting_for_video_keyframe = false;
                            }
                        }
                        media_packet::MediaType::AUDIO => {
                            let audio_data = data.audio;
                            let audio_data_js: js_sys::Uint8Array =
                                js_sys::Uint8Array::new_with_length(audio_data.len() as u32);
                            audio_data_js.copy_from(&audio_data.as_slice());

                            let audio_data = AudioData::new(&AudioDataInit::new(
                                &audio_data_js.into(),
                                AudioSampleFormatWrapper::from(data.audio_format).0,
                                data.audio_number_of_channels,
                                data.audio_number_of_frames,
                                data.audio_sample_rate,
                                data.timestamp,
                            ))
                            .unwrap();
                            (peer.audio_output)(audio_data);
                        }
                    }
                    false
                } else {
                    let error_video = Closure::wrap(Box::new(move |e: JsValue| {
                        log!(&e);
                    })
                        as Box<dyn FnMut(JsValue)>);
                    let error_audio = Closure::wrap(Box::new(move |e: JsValue| {
                        log!(&e);
                    })
                        as Box<dyn FnMut(JsValue)>);

                    let audio_output = {
                        let audio_stream_generator = MediaStreamTrackGenerator::new(
                            &MediaStreamTrackGeneratorInit::new(&"audio"),
                        )
                        .unwrap();
                        // The audio context is used to reproduce audio.
                        let _audio_context = configure_audio_context(&audio_stream_generator);
                        Box::new(move |audio_data: AudioData| {
                            let writable = audio_stream_generator.writable();
                            if writable.locked() {
                                return;
                            }
                            if let Err(e) = writable.get_writer().map(|writer| {
                                wasm_bindgen_futures::spawn_local(async move {
                                    if let Err(e) = JsFuture::from(writer.ready()).await {
                                        log!("write chunk error ", e);
                                    }
                                    if let Err(e) =
                                        JsFuture::from(writer.write_with_chunk(&audio_data)).await
                                    {
                                        log!("write chunk error ", e);
                                    };
                                    writer.release_lock();
                                });
                            }) {
                                log!("error", e);
                            }
                        }) as Box<dyn FnMut(AudioData)>
                    };
                    let video_output = Closure::wrap(Box::new(move |original_chunk: JsValue| {
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
                    })
                        as Box<dyn FnMut(JsValue)>);
                    let video_decoder = VideoDecoder::new(&VideoDecoderInit::new(
                        error_video.as_ref().unchecked_ref(),
                        video_output.as_ref().unchecked_ref(),
                    ))
                    .unwrap();
                    video_decoder.configure(&VideoDecoderConfig::new(&VIDEO_CODEC));

                    self.connected_peers.insert(
                        data.email.clone(),
                        ClientSubscription {
                            video_decoder,
                            audio_output,
                            waiting_for_video_keyframe: true,
                            waiting_for_audio_keyframe: true,
                        },
                    );
                    error_audio.forget();
                    error_video.forget();
                    video_output.forget();
                    true
                }
            }
            Msg::OnOutboundVideoPacket(media) => {
                if let Some(ws) = self.ws.as_mut() {
                    if self.connected {
                        let bytes = media.write_to_bytes().map_err(|w| anyhow!("{:?}", w));
                        ws.send_binary(bytes);
                    }
                }
                false
            }
            Msg::OnOutboundAudioPacket(audio_frame) => {
                if let Some(ws) = self.ws.as_mut() {
                    if self.connected {
                        let mut buffer = self.outbound_audio_buffer;
                        let email = ctx.props().email.clone();
                        let packet = transform_audio_chunk(&audio_frame, &mut buffer, &email);
                        if self.connected {
                            let bytes = packet.write_to_bytes().map_err(|w| anyhow!("{:?}", w));
                            ws.send_binary(bytes);
                        }
                        audio_frame.close();
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
            .callback(|frame: MediaPacket| Msg::OnOutboundVideoPacket(frame));

        let on_audio = ctx
            .link()
            .callback(|frame: AudioData| Msg::OnOutboundAudioPacket(frame));
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
                    <Host on_frame={on_frame} on_audio={on_audio} email={email.clone()}/>
                    <h4 class="floating-name">{email}</h4>
                </nav>
            </div>
        }
    }
}

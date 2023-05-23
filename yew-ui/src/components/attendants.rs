use std::collections::HashMap;

use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_CODEC;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::constants::VIDEO_CODEC;
use crate::model::configure_audio_context;
use crate::model::EncodedVideoChunkTypeWrapper;
use crate::model::MediaPacketWrapper;
use crate::{components::host::Host, constants::ACTIX_WEBSOCKET};
use gloo_console::log;
use js_sys::*;
use protobuf::Message;
use types::protos::media_packet::media_packet;
use types::protos::media_packet::MediaPacket;
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

#[derive(Debug)]
pub enum MeetingAction {
    ToggleScreenShare,
    ToggleMicMute,
    ToggleVideoOnOff,
}

pub enum Msg {
    WsAction(WsAction),
    MeetingAction(MeetingAction),
    OnInboundMedia(MediaPacketWrapper),
    OnOutboundVideoPacket(MediaPacket),
    OnOutboundAudioPacket(MediaPacket),
}

impl From<WsAction> for Msg {
    fn from(action: WsAction) -> Self {
        Msg::WsAction(action)
    }
}

impl From<MeetingAction> for Msg {
    fn from(action: MeetingAction) -> Self {
        Msg::MeetingAction(action)
    }
}

#[derive(Properties, Debug, PartialEq)]
pub struct AttendantsComponentProps {
    #[prop_or_default]
    pub id: String,

    #[prop_or_default]
    pub media_packet: MediaPacket,

    #[prop_or_default]
    pub email: String,
}

pub struct AttendantsComponent {
    pub ws: Option<WebSocketTask>,
    pub media_packet: MediaPacket,
    pub connected: bool,
    pub connected_peers: HashMap<String, ClientSubscription>,
    pub outbound_audio_buffer: [u8; 2000],
    pub share_screen: bool,
    pub mic_enabled: bool,
    pub video_enabled: bool,
}

pub struct ClientSubscription {
    pub video_decoder: VideoDecoder,
    pub screen_decoder: VideoDecoder,
    pub audio_decoder: AudioDecoder,
    pub waiting_for_video_keyframe: bool,
    pub waiting_for_audio_keyframe: bool,
    pub waiting_for_screen_keyframe: bool,
}

impl Component for AttendantsComponent {
    type Message = Msg;
    type Properties = AttendantsComponentProps;

    fn create(_ctx: &Context<Self>) -> Self {
        let connected_peers: HashMap<String, ClientSubscription> = HashMap::new();
        Self {
            ws: None,
            connected: false,
            media_packet: MediaPacket::default(),
            connected_peers,
            outbound_audio_buffer: [0; 2000],
            share_screen: false,
            mic_enabled: false,
            video_enabled: false,
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
                    let AttendantsComponentProps { id, email, .. } = ctx.props();
                    let url = format!("{}/{}/{}", ACTIX_WEBSOCKET.to_string(), email, id);
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
                let packet = response.0;
                let email = packet.email.clone();
                let screen_canvas_id = { format!("screen-share-{}", &email) };
                if let Some(peer) = self.connected_peers.get_mut(&email.clone()) {
                    match packet.media_type.unwrap() {
                        media_packet::MediaType::VIDEO => {
                            let video_data =
                                Uint8Array::new_with_length(packet.data.len().try_into().unwrap());
                            let chunk_type =
                                EncodedVideoChunkTypeWrapper::from(packet.frame_type).0;
                            video_data.copy_from(&packet.data.into_boxed_slice());
                            let mut video_chunk = EncodedVideoChunkInit::new(
                                &video_data,
                                packet.timestamp,
                                chunk_type,
                            );
                            video_chunk.duration(packet.duration);
                            let encoded_video_chunk = EncodedVideoChunk::new(&video_chunk).unwrap();
                            if peer.waiting_for_video_keyframe
                                && chunk_type == EncodedVideoChunkType::Key
                                || !peer.waiting_for_video_keyframe
                            {
                                if peer.video_decoder.state() == CodecState::Configured {
                                    peer.video_decoder.decode(&encoded_video_chunk);
                                    peer.waiting_for_video_keyframe = false;
                                } else if peer.video_decoder.state() == CodecState::Closed {
                                    // Codec crashed, reconfigure it...
                                    self.connected_peers.remove(&email.clone());
                                }
                            }
                        }
                        media_packet::MediaType::AUDIO => {
                            let audio_data = packet.data;
                            let audio_data_js: js_sys::Uint8Array =
                                js_sys::Uint8Array::new_with_length(audio_data.len() as u32);
                            audio_data_js.copy_from(&audio_data.as_slice());
                            let chunk_type = EncodedAudioChunkType::from_js_value(&JsValue::from(
                                packet.frame_type,
                            ))
                            .unwrap();
                            let mut audio_chunk = EncodedAudioChunkInit::new(
                                &audio_data_js.into(),
                                packet.timestamp,
                                chunk_type,
                            );
                            audio_chunk.duration(packet.duration);
                            let encoded_audio_chunk = EncodedAudioChunk::new(&audio_chunk).unwrap();
                            if peer.waiting_for_audio_keyframe
                                && chunk_type == EncodedAudioChunkType::Key
                                || !peer.waiting_for_audio_keyframe
                            {
                                if peer.audio_decoder.state() == CodecState::Configured {
                                    peer.audio_decoder.decode(&encoded_audio_chunk);
                                    peer.waiting_for_audio_keyframe = false;
                                } else if peer.audio_decoder.state() == CodecState::Closed {
                                    // Codec crashed, reconfigure it...
                                    self.connected_peers.remove(&email.clone());
                                }
                            }
                        }
                        media_packet::MediaType::SCREEN => {
                            let video_data =
                                Uint8Array::new_with_length(packet.data.len().try_into().unwrap());
                            let chunk_type =
                                EncodedVideoChunkTypeWrapper::from(packet.frame_type).0;
                            video_data.copy_from(&packet.data.into_boxed_slice());
                            let mut video_chunk = EncodedVideoChunkInit::new(
                                &video_data,
                                packet.timestamp,
                                chunk_type,
                            );
                            video_chunk.duration(packet.duration);
                            let encoded_video_chunk = EncodedVideoChunk::new(&video_chunk).unwrap();
                            if peer.waiting_for_screen_keyframe
                                && chunk_type == EncodedVideoChunkType::Key
                                || !peer.waiting_for_screen_keyframe
                            {
                                if peer.screen_decoder.state() == CodecState::Configured {
                                    peer.screen_decoder.decode(&encoded_video_chunk);
                                    peer.waiting_for_screen_keyframe = false;
                                    return true;
                                } else if peer.screen_decoder.state() == CodecState::Closed {
                                    // Codec crashed, reconfigure it...
                                    self.connected_peers.remove(&email.clone());
                                    return true;
                                }
                            }
                        }
                        media_packet::MediaType::COMMAND => {
                            match packet.command_type.unwrap() {
                                media_packet::CommandType::MUTE => {
                                    log!("Muting mic from server command");
                                    self.mic_enabled = false;
                                }
                            }
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
                    let audio_stream_generator = MediaStreamTrackGenerator::new(
                        &MediaStreamTrackGeneratorInit::new(&"audio"),
                    )
                    .unwrap();
                    // The audio context is used to reproduce audio.
                    let _audio_context = configure_audio_context(&audio_stream_generator).unwrap();

                    let audio_output = Closure::wrap(Box::new(move |audio_data: AudioData| {
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
                    })
                        as Box<dyn FnMut(AudioData)>);
                    let audio_decoder = AudioDecoder::new(&AudioDecoderInit::new(
                        error_audio.as_ref().unchecked_ref(),
                        audio_output.as_ref().unchecked_ref(),
                    ))
                    .unwrap();
                    audio_decoder.configure(&AudioDecoderConfig::new(
                        &AUDIO_CODEC,
                        AUDIO_CHANNELS as u32,
                        AUDIO_SAMPLE_RATE as u32,
                    ));
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
                    let screen_output = Closure::wrap(Box::new(move |original_chunk: JsValue| {
                        let chunk = Box::new(original_chunk);
                        let video_chunk = chunk.unchecked_into::<VideoFrame>();
                        let width = video_chunk.coded_width();
                        let height = video_chunk.coded_height();
                        let render_canvas = window()
                            .unwrap()
                            .document()
                            .unwrap()
                            .get_element_by_id(&screen_canvas_id.clone())
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

                    let error_screen = Closure::wrap(Box::new(move |e: JsValue| {
                        log!(&e);
                    })
                        as Box<dyn FnMut(JsValue)>);

                    let screen_decoder = VideoDecoder::new(&VideoDecoderInit::new(
                        error_screen.as_ref().unchecked_ref(),
                        screen_output.as_ref().unchecked_ref(),
                    ))
                    .unwrap();
                    screen_decoder.configure(&VideoDecoderConfig::new(&VIDEO_CODEC));

                    self.connected_peers.insert(
                        packet.email.clone(),
                        ClientSubscription {
                            video_decoder,
                            audio_decoder,
                            screen_decoder,
                            waiting_for_video_keyframe: true,
                            waiting_for_audio_keyframe: true,
                            waiting_for_screen_keyframe: true,
                        },
                    );
                    // TODO: These are leaks, store them into the client instead of leaking!!
                    error_audio.forget();
                    error_video.forget();
                    error_screen.forget();
                    audio_output.forget();
                    screen_output.forget();
                    video_output.forget();
                    true
                }
            }
            Msg::OnOutboundVideoPacket(media) => {
                if let Some(ws) = self.ws.as_mut() {
                    if self.connected {
                        match media
                            .write_to_bytes()
                            .map_err(|w| JsValue::from(format!("{:?}", w)))
                        {
                            Ok(bytes) => {
                                // log!("sending video packet: ", bytes.len(), " bytes");
                                ws.send_binary(bytes);
                            }
                            Err(e) => {
                                log!("error sending video packet: {:?}", e);
                            }
                        }
                    }
                }
                false
            }
            Msg::OnOutboundAudioPacket(media) => {
                if let Some(ws) = self.ws.as_mut() {
                    if self.connected {
                        match media
                            .write_to_bytes()
                            .map_err(|w| JsValue::from(format!("{:?}", w)))
                        {
                            Ok(bytes) => {
                                // log!("sending audio packet: ", bytes.len(), " bytes");
                                ws.send_binary(bytes);
                            }
                            Err(e) => {
                                log!("error sending audio packet: {:?}", e);
                            }
                        }
                    }
                }
                false
            }
            Msg::MeetingAction(action) => {
                match action {
                    MeetingAction::ToggleScreenShare => {
                        self.share_screen = !self.share_screen;
                    }
                    MeetingAction::ToggleMicMute => {
                        self.mic_enabled = !self.mic_enabled;
                    }
                    MeetingAction::ToggleVideoOnOff => {
                        self.video_enabled = !self.video_enabled;
                    }
                }
                true
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
            .callback(|frame: MediaPacket| Msg::OnOutboundAudioPacket(frame));
        let rows: Vec<VNode> = self
            .connected_peers
            .iter()
            .map(|(key, value)| {
                let screen_share_css = if value.waiting_for_screen_keyframe {
                    "grid-item hidden"
                } else {
                    "grid-item"
                };
                html! {
                    <>
                        <div class={screen_share_css}>
                            // Canvas for Screen share.
                            <canvas id={format!("screen-share-{}", &key)}></canvas>
                            <h4 class="floating-name">{format!("{}-screen", &key)}</h4>
                        </div>
                        <div class="grid-item">
                            // One canvas for the User Video
                            <canvas id={key.clone()}></canvas>
                            <h4 class="floating-name">{key.clone()}</h4>
                        </div>
                    </>
                }
            })
            .collect();
        html! {
            <div class="grid-container">
                { rows }
                <nav class="host">
                    <div class="controls">
                        <button
                            onclick={ctx.link().callback(|_| MeetingAction::ToggleScreenShare)}>
                            { if self.share_screen { "Stop Screen Share"} else { "Share Screen"} }
                        </button>
                        <button
                            onclick={ctx.link().callback(|_| MeetingAction::ToggleVideoOnOff)}>
                            { if !self.video_enabled { "Start Video"} else { "Stop Video"} }
                        </button>
                        <button
                            onclick={ctx.link().callback(|_| MeetingAction::ToggleMicMute)}>
                            { if !self.mic_enabled { "Unmute"} else { "Mute"} }
                            </button>
                        <button disabled={self.ws.is_some()}
                                onclick={ctx.link().callback(|_| WsAction::Connect)}>
                            { "Connect" }
                        </button>
                        <button disabled={self.ws.is_none()}
                                onclick={ctx.link().callback(|_| WsAction::Disconnect)}>
                            { "Close" }
                        </button>
                    </div>
                    <Host on_frame={on_frame} on_audio={on_audio} email={email.clone()} share_screen={self.share_screen} mic_enabled={self.mic_enabled} video_enabled={self.video_enabled}/>
                    <h4 class="floating-name">{email}</h4>
                </nav>
            </div>
        }
    }
}

use std::collections::HashMap;
use std::sync::Arc;

use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_CODEC;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::constants::VIDEO_CODEC;
use crate::model::configure_audio_context;
use crate::model::EncodedVideoChunkTypeWrapper;
use crate::model::MediaPacketWrapper;
use crate::{components::host::Host, constants::ACTIX_WEBSOCKET};
use gloo::timers::callback::Interval;
use gloo_console::log;
use protobuf::Message;
use types::protos::media_packet::media_packet;
use types::protos::media_packet::media_packet::MediaType;
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

use super::device_permissions::request_permissions;
use super::video_decoder_with_buffer::VideoDecoderWithBuffer;

// This is important https://plnkr.co/edit/1yQd8ozGXlV9bwK6?preview
// https://github.com/WebAudio/web-audio-api-v2/issues/133

#[derive(Debug)]
pub enum WsAction {
    Connect,
    Connected,
    Disconnect,
    Lost,
    RequestMediaPermissions,
    MediaPermissionsGranted,
    MediaPermissionsError(String),
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
    OnOutboundPacket(MediaPacket),
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
    pub sorted_connected_peers_keys: Vec<String>,
    pub outbound_audio_buffer: [u8; 2000],
    pub share_screen: bool,
    pub mic_enabled: bool,
    pub video_enabled: bool,
    pub heartbeat: Option<Interval>,
    pub error: Option<String>,
    pub media_access_granted: bool,
}

pub struct ClientSubscription {
    pub video_decoder: VideoDecoderWithBuffer,
    pub screen_decoder: VideoDecoderWithBuffer,
    pub audio_decoder: AudioDecoder,
    pub waiting_for_video_keyframe: bool,
    pub waiting_for_audio_keyframe: bool,
    pub waiting_for_screen_keyframe: bool,
    pub error_audio: Closure<dyn FnMut(JsValue)>,
    pub error_video: Closure<dyn FnMut(JsValue)>,
    pub error_screen: Closure<dyn FnMut(JsValue)>,
    pub audio_output: Closure<dyn FnMut(web_sys::AudioData)>,
    pub video_output: Closure<dyn FnMut(JsValue)>,
    pub screen_output: Closure<dyn FnMut(JsValue)>,
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
            sorted_connected_peers_keys: vec![],
            outbound_audio_buffer: [0; 2000],
            share_screen: false,
            mic_enabled: false,
            video_enabled: false,
            heartbeat: None,
            error: None,
            media_access_granted: false,
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::WsAction(action) => match action {
                WsAction::Connect => {
                    let callback = ctx.link().callback(Msg::OnInboundMedia);
                    let notification = ctx.link().batch_callback(|status| match status {
                        WebSocketStatus::Opened => Some(WsAction::Connected.into()),
                        WebSocketStatus::Closed | WebSocketStatus::Error => {
                            Some(WsAction::Lost.into())
                        }
                    });
                    let AttendantsComponentProps { id, email, .. } = ctx.props();
                    let url = format!("{}/{}/{}", ACTIX_WEBSOCKET, email, id);
                    log!("Connecting to ", &url);
                    let task = WebSocketService::connect(&url, callback, notification).unwrap();
                    let link = ctx.link().clone();
                    let email = email.clone();
                    self.heartbeat = Some(Interval::new(1000, move || {
                        let media_packet = MediaPacket { media_type: MediaType::HEARTBEAT.into(), email: email.clone(), timestamp: js_sys::Date::now(), ..Default::default() };
                        link.send_message(Msg::OnOutboundPacket(media_packet));
                    }));
                    self.ws = Some(task);
                    true
                }
                WsAction::Disconnect => {
                    log!("Disconnect");
                    self.ws.take();
                    if let Some(heartbeat) = self.heartbeat.take() {
                        heartbeat.cancel();
                    }
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
                    let heartbeat = self.heartbeat.take();
                    match heartbeat {
                        Some(heartbeat) => {
                            heartbeat.cancel();
                        }
                        None => {}
                    }
                    self.connected = false;
                    false
                }
                WsAction::RequestMediaPermissions => {
                    let future = request_permissions();
                    let link = ctx.link().clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        match future.await {
                            Ok(_) => {
                                link.send_message(WsAction::MediaPermissionsGranted);
                            }
                            Err(_) => {
                                link.send_message(WsAction::MediaPermissionsError("Error requesting permissions. Please make sure to allow access to both camera and microphone.".to_string()));
                            }
                        }
                    });
                    false
                }
                WsAction::MediaPermissionsGranted => {
                    self.error = None;
                    self.media_access_granted = true;
                    ctx.link().send_message(WsAction::Connect);
                    true
                }
                WsAction::MediaPermissionsError(error) => {
                    self.error = Some(error);
                    true
                }
            },
            Msg::OnInboundMedia(response) => {
                let packet = Arc::new(response.0);
                let email = packet.email.clone();
                let screen_canvas_id = { format!("screen-share-{}", &email) };
                let frame_type = packet.frame_type.clone();
                if let Some(peer) = self.connected_peers.get_mut(&email.clone()) {
                    match packet.media_type.unwrap() {
                        media_packet::MediaType::VIDEO => {
                            let chunk_type =
                                EncodedVideoChunkTypeWrapper::from(frame_type.as_str()).0;
                            if !peer.waiting_for_video_keyframe || chunk_type == EncodedVideoChunkType::Key {
                                if peer.video_decoder.state() == CodecState::Configured {
                                    peer.video_decoder.decode(packet.clone());
                                    peer.waiting_for_video_keyframe = false;
                                } else if peer.video_decoder.state() == CodecState::Closed {
                                    // Codec crashed, reconfigure it...
                                    self.connected_peers.remove(&email);
                                    // remove email from connected_peers_keys
                                    if let Some(index) = self
                                        .sorted_connected_peers_keys
                                        .iter()
                                        .position(|x| *x == email)
                                    {
                                        self.sorted_connected_peers_keys.remove(index);
                                    }
                                    self.insert_peer(email.clone(), screen_canvas_id);
                                }
                            }
                        }
                        media_packet::MediaType::AUDIO => {
                            let audio_data = &packet.data;
                            let audio_data_js: js_sys::Uint8Array =
                                js_sys::Uint8Array::new_with_length(audio_data.len() as u32);
                            audio_data_js.copy_from(audio_data.as_slice());
                            let chunk_type =
                                EncodedAudioChunkType::from_js_value(&JsValue::from(frame_type))
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
                            {
                                if peer.audio_decoder.state() == CodecState::Configured {
                                    peer.audio_decoder.decode(&encoded_audio_chunk);
                                    peer.waiting_for_audio_keyframe = false;
                                } else if peer.audio_decoder.state() == CodecState::Closed {
                                    // Codec crashed, reconfigure it...
                                    self.connected_peers.remove(&email);
                                }
                            }
                        }
                        media_packet::MediaType::SCREEN => {
                            let chunk_type =
                                EncodedVideoChunkTypeWrapper::from(packet.frame_type.as_str()).0;
                            if peer.waiting_for_screen_keyframe
                                && chunk_type == EncodedVideoChunkType::Key
                            {
                                if peer.screen_decoder.state() == CodecState::Configured {
                                    peer.screen_decoder.decode(packet.clone());
                                    peer.waiting_for_screen_keyframe = false;
                                    return true;
                                } else if peer.screen_decoder.state() == CodecState::Closed {
                                    // Codec crashed, reconfigure it...
                                    self.connected_peers.remove(&email);
                                    return true;
                                }
                            }
                        }
                        media_packet::MediaType::HEARTBEAT => {
                            return false;
                        }
                    }
                    false
                } else {
                    self.insert_peer(email.clone(), screen_canvas_id);
                    true
                }
            }
            Msg::OnOutboundPacket(media) => {
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
                                let packet_type = media.media_type.enum_value().unwrap();
                                log!(
                                    "error sending {} packet: {:?}",
                                    JsValue::from(format!("{}", packet_type)),
                                    e
                                );
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
        let on_packet = ctx.link().callback(Msg::OnOutboundPacket);
        let media_access_granted = self.media_access_granted;
        let rows: Vec<VNode> = self
            .sorted_connected_peers_keys
            .iter()
            .map(|key| {
                let value = self.connected_peers.get(key).unwrap();
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
                            <UserVideo id={key.clone()}></UserVideo>
                            <h4 class="floating-name">{key.clone()}</h4>
                        </div>
                    </>
                }
            })
            .collect();
        html! {
            <div class="grid-container">
                { self.error.as_ref().map(|error| html! { <p>{ error }</p> }) }
                { rows }
                <nav class="host">
                    <div class="controls">
                        <button
                            class="bg-yew-blue p-2 rounded-md text-white"
                            onclick={ctx.link().callback(|_| MeetingAction::ToggleScreenShare)}>
                            { if self.share_screen { "Stop Screen Share"} else { "Share Screen"} }
                        </button>
                        <button
                            class="bg-yew-blue p-2 rounded-md text-white"
                            onclick={ctx.link().callback(|_| MeetingAction::ToggleVideoOnOff)}>
                            { if !self.video_enabled { "Start Video"} else { "Stop Video"} }
                        </button>
                        <button
                            class="bg-yew-blue p-2 rounded-md text-white"
                            onclick={ctx.link().callback(|_| MeetingAction::ToggleMicMute)}>
                            { if !self.mic_enabled { "Unmute"} else { "Mute"} }
                            </button>
                        <button
                            class="bg-yew-blue p-2 rounded-md text-white"
                            disabled={self.ws.is_some()}
                            onclick={ctx.link().callback(|_| WsAction::RequestMediaPermissions)}>
                            { "Connect" }
                        </button>
                        <button
                            class="bg-yew-blue p-2 rounded-md text-white"
                            disabled={self.ws.is_none()}
                                onclick={ctx.link().callback(|_| WsAction::Disconnect)}>
                            { "Close" }
                        </button>
                    </div>
                    {
                        if media_access_granted {
                            html! {<Host on_packet={on_packet} email={email.clone()} share_screen={self.share_screen} mic_enabled={self.mic_enabled} video_enabled={self.video_enabled}/>}
                        } else {
                            html! {<></>}
                        }
                    }
                    <h4 class="floating-name">{email}</h4>
                </nav>
            </div>
        }
    }
}

impl AttendantsComponent {
    fn insert_peer(&mut self, email: String, screen_canvas_id: String) {
        let error_video = Closure::wrap(Box::new(move |e: JsValue| {
            log!(&e);
        }) as Box<dyn FnMut(JsValue)>);
        let error_audio = Closure::wrap(Box::new(move |e: JsValue| {
            log!(&e);
        }) as Box<dyn FnMut(JsValue)>);
        let audio_stream_generator =
            MediaStreamTrackGenerator::new(&MediaStreamTrackGeneratorInit::new("audio")).unwrap();
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
                    if let Err(e) = JsFuture::from(writer.write_with_chunk(&audio_data)).await {
                        log!("write chunk error ", e);
                    };
                    writer.release_lock();
                });
            }) {
                log!("error", e);
            }
        }) as Box<dyn FnMut(AudioData)>);
        let audio_decoder = AudioDecoder::new(&AudioDecoderInit::new(
            error_audio.as_ref().unchecked_ref(),
            audio_output.as_ref().unchecked_ref(),
        ))
        .unwrap();
        audio_decoder.configure(&AudioDecoderConfig::new(
            AUDIO_CODEC,
            AUDIO_CHANNELS,
            AUDIO_SAMPLE_RATE,
        ));
        let canvas_email = email.clone();
        let document = window().unwrap().document().unwrap();
        let video_output = Closure::wrap(Box::new(move |original_chunk: JsValue| {
            let chunk = Box::new(original_chunk);
            let video_chunk = chunk.unchecked_into::<VideoFrame>();
            let width = video_chunk.coded_width();
            let height = video_chunk.coded_height();
            let video_chunk = video_chunk.unchecked_into::<HtmlImageElement>();
            let render_canvas = document
                .get_element_by_id(&canvas_email)
                .unwrap()
                .unchecked_into::<HtmlCanvasElement>();
            let ctx = render_canvas
                .get_context("2d")
                .unwrap()
                .unwrap()
                .unchecked_into::<CanvasRenderingContext2d>();
            render_canvas.set_width(width);
            render_canvas.set_height(height);
            if let Err(e) = ctx.draw_image_with_html_image_element(&video_chunk, 0.0, 0.0) {
                log!("error ", e);
            }
            video_chunk.unchecked_into::<VideoFrame>().close();
        }) as Box<dyn FnMut(JsValue)>);
        let video_decoder = VideoDecoderWithBuffer::new(&VideoDecoderInit::new(
            error_video.as_ref().unchecked_ref(),
            video_output.as_ref().unchecked_ref(),
        ))
        .unwrap();
        video_decoder.configure(&VideoDecoderConfig::new(VIDEO_CODEC));
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
            render_canvas.set_width(width);
            render_canvas.set_height(height);
            let ctx = render_canvas
                .get_context("2d")
                .unwrap()
                .unwrap()
                .unchecked_into::<CanvasRenderingContext2d>();
            let video_chunk = video_chunk.unchecked_into::<HtmlImageElement>();

            if let Err(e) = ctx.draw_image_with_html_image_element(&video_chunk, 0.0, 0.0) {
                log!("error ", e);
            }
            video_chunk.unchecked_into::<VideoFrame>().close();
        }) as Box<dyn FnMut(JsValue)>);

        let error_screen = Closure::wrap(Box::new(move |e: JsValue| {
            log!(&e);
        }) as Box<dyn FnMut(JsValue)>);

        let screen_decoder = VideoDecoderWithBuffer::new(&VideoDecoderInit::new(
            error_screen.as_ref().unchecked_ref(),
            screen_output.as_ref().unchecked_ref(),
        ))
        .unwrap();
        screen_decoder.configure(&VideoDecoderConfig::new(VIDEO_CODEC));

        self.connected_peers.insert(
            email.clone(),
            ClientSubscription {
                video_decoder,
                audio_decoder,
                screen_decoder,
                waiting_for_video_keyframe: true,
                waiting_for_audio_keyframe: true,
                waiting_for_screen_keyframe: true,
                error_audio,
                error_video,
                error_screen,
                audio_output,
                video_output,
                screen_output,
            },
        );
        self.sorted_connected_peers_keys.push(email);
        self.sorted_connected_peers_keys.sort();
    }
}

// props for the video component
#[derive(Properties, Debug, PartialEq)]
pub struct UserVideoProps {
    pub id: String,
}

// user video functional component
#[function_component(UserVideo)]
fn user_video(props: &UserVideoProps) -> Html {
    // create use_effect hook that gets called only once and sets a thumbnail
    // for the user video
    let video_ref = use_state(NodeRef::default);
    let video_ref_clone = video_ref.clone();
    use_effect_with_deps(
        move |_| {
            // Set thumbnail for the video
            let video = (*video_ref_clone).cast::<HtmlCanvasElement>().unwrap();
            let ctx = video
                .get_context("2d")
                .unwrap()
                .unwrap()
                .unchecked_into::<CanvasRenderingContext2d>();
            ctx.clear_rect(0.0, 0.0, video.width() as f64, video.height() as f64);
            || ()
        },
        vec![props.id.clone()],
    );

    html! {
        <canvas ref={(*video_ref).clone()} id={props.id.clone()}></canvas>
    }
}

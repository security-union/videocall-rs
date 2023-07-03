use std::collections::HashMap;
use std::sync::Arc;

use crate::constants::WEBTRANSPORT_ENABLED;
use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_CODEC;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::constants::VIDEO_CODEC;
use crate::constants::WEBTRANSPORT_HOST;
use crate::model::decode::{AudioPeerDecoder, VideoPeerDecoder};
use crate::model::MediaPacketWrapper;
use crate::{components::host::Host, constants::ACTIX_WEBSOCKET};
use gloo::timers::callback::Interval;
use gloo_console::log;
use js_sys::Boolean;
use js_sys::JsString;
use js_sys::Reflect;
use js_sys::Uint8Array;
use protobuf::Message;
use types::protos::media_packet::media_packet;
use types::protos::media_packet::media_packet::MediaType;
use types::protos::media_packet::MediaPacket;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use web_sys::*;
use yew::prelude::*;
use yew::virtual_dom::VNode;
use yew::{html, Component, Context, Html};
use yew_websocket::websocket::{WebSocketService, WebSocketStatus, WebSocketTask};
use yew_webtransport::webtransport::{
    process_binary, WebTransportService, WebTransportStatus, WebTransportTask,
};

use super::device_permissions::request_permissions;

// This is important https://plnkr.co/edit/1yQd8ozGXlV9bwK6?preview
// https://github.com/WebAudio/web-audio-api-v2/issues/133

#[derive(Debug)]
pub enum WsAction {
    Connect(bool),
    Connected,
    Disconnect,
    Lost(Option<JsValue>),
    RequestMediaPermissions,
    MediaPermissionsGranted,
    MediaPermissionsError(String),
    Log(String),
}

#[derive(Debug)]
pub enum Connection {
    WebSocket(WebSocketTask),
    WebTransport(WebTransportTask),
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
    OnDatagram(Vec<u8>),
    OnUniStream(WebTransportReceiveStream),
    OnBidiStream(WebTransportBidirectionalStream),
    OnMessage(Vec<u8>, WebTransportMessageType),
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
    pub email: String,

    pub webtransport_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebTransportMessageType {
    Datagram,
    UnidirectionalStream,
    BidirectionalStream,
    Unknown,
}

pub struct AttendantsComponent {
    pub connection: Option<Connection>,
    pub media_packet: MediaPacket,
    pub connected: bool,
    pub connected_peers: HashMap<String, ClientSubscription>,
    pub sorted_connected_peers_keys: Vec<String>,
    pub outbound_audio_buffer: [u8; 2000],
    pub share_screen: bool,
    pub webtransport_enabled: bool,
    pub mic_enabled: bool,
    pub video_enabled: bool,
    pub heartbeat: Option<Interval>,
    pub error: Option<String>,
    pub media_access_granted: bool,
}

pub struct ClientSubscription {
    pub audio: AudioPeerDecoder,
    pub video: VideoPeerDecoder,
    pub screen: VideoPeerDecoder,
}

pub fn connect_websocket(
    ctx: &Context<AttendantsComponent>,
    email: &str,
    id: &str,
) -> anyhow::Result<WebSocketTask> {
    let callback = ctx.link().callback(Msg::OnInboundMedia);
    let notification = ctx.link().batch_callback(|status| match status {
        WebSocketStatus::Opened => Some(WsAction::Connected.into()),
        WebSocketStatus::Closed | WebSocketStatus::Error => Some(WsAction::Lost(None).into()),
    });
    let url = format!("{}/{}/{}", ACTIX_WEBSOCKET, email, id);
    log!("Connecting to ", &url);
    let task = WebSocketService::connect(&url, callback, notification)?;
    Ok(task)
}

pub fn connect_webtransport(
    ctx: &Context<AttendantsComponent>,
    email: &str,
    id: &str,
) -> anyhow::Result<WebTransportTask> {
    let on_datagram = ctx.link().callback(Msg::OnDatagram);
    let on_unidirectional_stream = ctx.link().callback(Msg::OnUniStream);
    let on_bidirectional_stream = ctx.link().callback(Msg::OnBidiStream);
    let notification = ctx.link().batch_callback(|status| match status {
        WebTransportStatus::Opened => Some(WsAction::Connected.into()),
        WebTransportStatus::Closed(error) | WebTransportStatus::Error(error) => {
            Some(WsAction::Lost(Some(error)).into())
        }
    });
    let url = format!("{}/{}/{}", WEBTRANSPORT_HOST, email, id);
    let task = WebTransportService::connect(
        &url,
        on_datagram,
        on_unidirectional_stream,
        on_bidirectional_stream,
        notification,
    )?;
    Ok(task)
}

impl Component for AttendantsComponent {
    type Message = Msg;
    type Properties = AttendantsComponentProps;

    fn create(ctx: &Context<Self>) -> Self {
        let connected_peers: HashMap<String, ClientSubscription> = HashMap::new();
        let webtransport_enabled = ctx.props().webtransport_enabled;
        Self {
            connection: None,
            connected: false,
            media_packet: MediaPacket::default(),
            connected_peers,
            sorted_connected_peers_keys: vec![],
            outbound_audio_buffer: [0; 2000],
            share_screen: false,
            mic_enabled: false,
            video_enabled: false,
            webtransport_enabled,
            heartbeat: None,
            error: None,
            media_access_granted: false,
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::WsAction(action) => match action {
                WsAction::Connect(webtransport) => {
                    log!("webtransport connect = {}", webtransport);
                    let id = ctx.props().id.clone();
                    let email = ctx.props().email.clone();
                    if !webtransport {
                        if let Ok(task) = connect_websocket(ctx, &email, &id).map_err(|e| {
                            ctx.link().send_message(WsAction::Log(format!(
                                "WebSocket connect failed: {}",
                                e
                            )));
                        }) {
                            self.connection = Some(Connection::WebSocket(task));
                        }
                    } else {
                        let task= connect_webtransport(ctx, &email, &id);
                        match task {
                            Ok(task) => {
                                self.connection = Some(Connection::WebTransport(task));
                            },
                            Err(e) => {
                                log!("WebTransport connect failed:");
                                ctx.link().send_message(WsAction::Connect(false));
                            }
                        }
                    }

                    let link = ctx.link().clone();
                    self.heartbeat = Some(Interval::new(1000, move || {
                        let media_packet = MediaPacket {
                            media_type: MediaType::HEARTBEAT.into(),
                            email: email.clone(),
                            timestamp: js_sys::Date::now(),
                            ..Default::default()
                        };
                        link.send_message(Msg::OnOutboundPacket(media_packet));
                    }));
                    true
                }
                WsAction::Disconnect => {
                    log!("Disconnect");
                    if let Some(connection) = self.connection.take() {
                        match connection {
                            Connection::WebSocket(task) => {

                            }
                            Connection::WebTransport(task) => {
                                log!("close webtransport");
                                task.transport.close();
                            }
                        }
                    }
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
                WsAction::Log(msg) => {
                    log!("{}", msg);
                    false
                }
                WsAction::Lost(reason) => {
                    log!("Lost");
                    if let Some(window) = window() {
                        let _ = window.alert_with_message(&format!(
                            "Connection lost. Please reconnect. Reason: {:?}",
                            reason
                        ));
                    }
                    self.connection.take();
                    if let Some(heartbeat ) = self.heartbeat.take() {
                        heartbeat.cancel();
                    };
                    self.connected = false;
                    true
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
                    ctx.link()
                        .send_message(WsAction::Connect(self.webtransport_enabled));
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
                if let Some(peer) = self.connected_peers.get_mut(&email.clone()) {
                    match packet.media_type.unwrap() {
                        media_packet::MediaType::VIDEO => {
                            if let Err(()) = peer.video.decode(&packet) {
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
                        media_packet::MediaType::AUDIO => {
                            if let Err(()) = peer.audio.decode(&packet) {
                                self.connected_peers.remove(&email);
                            }
                        }
                        media_packet::MediaType::SCREEN => {
                            if let Err(()) = peer.screen.decode(&packet) {
                                // Codec crashed, reconfigure it...
                                self.connected_peers.remove(&email);
                            }
                            // TOFIX: due to a bug, we need to refresh the screen to ensure that the canvas is created.
                            return true;
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
                if let Some(connection) = self.connection.take() {
                    match connection {
                        Connection::WebSocket(mut ws) => {
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
                                self.connection = Some(Connection::WebSocket(ws));
                            }
                        }
                        Connection::WebTransport(wt) => {
                            if self.connected {
                                match media
                                    .write_to_bytes()
                                    .map_err(|w| JsValue::from(format!("{:?}", w)))
                                {
                                    Ok(bytes) => {
                                        // TODO: Investigate why using datagrams causes issues
                                        if bytes.len() > 100 {
                                            WebTransportTask::send_unidirectional_stream(
                                                wt.transport.clone(),
                                                bytes,
                                            );
                                        } else {
                                            WebTransportTask::send_datagram(
                                                wt.transport.clone(),
                                                bytes,
                                            );
                                        }
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
                                self.connection = Some(Connection::WebTransport(wt));
                            }
                        }
                    }
                }
                false
            }
            Msg::OnDatagram(bytes) => {
                let media_packet = MediaPacket::parse_from_bytes(&bytes)
                    .map_err(|e| JsValue::from(format!("{:?}", e)));
                if let Ok(media_packet) = media_packet {
                    ctx.link()
                        .send_message(Msg::OnInboundMedia(MediaPacketWrapper(media_packet)));
                }
                false
            }
            Msg::OnMessage(response, _message_type) => {
                let res = MediaPacket::parse_from_bytes(&response);
                if let Ok(media_packet) = res {
                    ctx.link()
                        .send_message(Msg::OnInboundMedia(MediaPacketWrapper(media_packet)));
                } else {
                    log!("failed to parse media packet");
                }
                false
            }
            Msg::OnUniStream(stream) => {
                if stream.is_undefined() {
                    log!("stream is undefined");
                    return true;
                }
                let incoming_datagrams: ReadableStreamDefaultReader =
                    stream.get_reader().unchecked_into();
                let callback = ctx
                    .link()
                    .callback(|d| Msg::OnMessage(d, WebTransportMessageType::UnidirectionalStream));
                wasm_bindgen_futures::spawn_local(async move {
                    loop {
                        let read_result = JsFuture::from(incoming_datagrams.read()).await;
                        match read_result {
                            Err(e) => {
                                let mut reason = WebTransportCloseInfo::default();
                                reason.reason(
                                    format!("Failed to read incoming datagrams {e:?}").as_str(),
                                );
                                break;
                            }
                            Ok(result) => {
                                let done = Reflect::get(&result, &JsString::from("done"))
                                    .unwrap()
                                    .unchecked_into::<Boolean>();
                                if done.is_truthy() {
                                    break;
                                }
                                let value: Uint8Array =
                                    Reflect::get(&result, &JsString::from("value"))
                                        .unwrap()
                                        .unchecked_into();
                                process_binary(&value, &callback);
                            }
                        }
                    }
                });
                false
            }
            Msg::OnBidiStream(stream) => {
                log!("OnBidiStream: ", &stream);
                if stream.is_undefined() {
                    log!("stream is undefined");
                    return true;
                }
                let readable: ReadableStreamDefaultReader =
                    stream.readable().get_reader().unchecked_into();
                let callback = ctx
                    .link()
                    .callback(|d| Msg::OnMessage(d, WebTransportMessageType::BidirectionalStream));
                wasm_bindgen_futures::spawn_local(async move {
                    loop {
                        log!("reading from stream");
                        let read_result = JsFuture::from(readable.read()).await;
                        match read_result {
                            Err(e) => {
                                let mut reason = WebTransportCloseInfo::default();
                                reason.reason(
                                    format!("Failed to read incoming datagrams {e:?}").as_str(),
                                );
                                break;
                            }
                            Ok(result) => {
                                let done = Reflect::get(&result, &JsString::from("done"))
                                    .unwrap()
                                    .unchecked_into::<Boolean>();
                                if done.is_truthy() {
                                    break;
                                }
                                let value: Uint8Array =
                                    Reflect::get(&result, &JsString::from("value"))
                                        .unwrap()
                                        .unchecked_into();
                                process_binary(&value, &callback);
                            }
                        }
                    }
                    log!("readable stream closed");
                });
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
                let peer = self.connected_peers.get(key).unwrap();
                let screen_share_css = if peer.screen.is_waiting_for_keyframe() {
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
                        {if self.connection.is_none() {
                            html! {<button
                                class="bg-yew-blue p-2 rounded-md text-white"
                                disabled={self.connection.is_some() }
                                onclick={ctx.link().callback(|_| WsAction::RequestMediaPermissions)}>
                                { "Connect" }
                            </button>
                            }
                        } else {
                            html! {}
                        }}
                        <button
                            class="bg-yew-blue p-2 rounded-md text-white"
                            disabled={self.connection.is_none() }
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
        self.connected_peers.insert(
            email.clone(),
            ClientSubscription {
                audio: AudioPeerDecoder::new(),
                video: VideoPeerDecoder::new(&email),
                screen: VideoPeerDecoder::new(&screen_canvas_id),
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

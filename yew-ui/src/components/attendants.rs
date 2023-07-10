use crate::constants::WEBTRANSPORT_HOST;
use crate::model::decode::PeerDecodeManager;
use crate::model::MediaPacketWrapper;
use crate::{components::host::Host, constants::ACTIX_WEBSOCKET};
use gloo::timers::callback::Interval;
use gloo_console::log;
use js_sys::Boolean;
use js_sys::JsString;
use js_sys::Reflect;
use js_sys::Uint8Array;
use protobuf::Message;
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
use yew_webtransport::webtransport::{WebTransportService, WebTransportStatus, WebTransportTask};

use super::device_permissions::request_permissions;

// This is important https://plnkr.co/edit/1yQd8ozGXlV9bwK6?preview
// https://github.com/WebAudio/web-audio-api-v2/issues/133

#[derive(Debug)]
pub enum WsAction {
    Connect(bool),
    Connected,
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
    OnPeerAdded(String),
    OnFirstFrame((String, MediaType)),
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
    pub connecting: bool,
    pub peer_decode_manager: PeerDecodeManager,
    pub outbound_audio_buffer: [u8; 2000],
    pub share_screen: bool,
    pub webtransport_enabled: bool,
    pub mic_enabled: bool,
    pub video_enabled: bool,
    pub heartbeat: Option<Interval>,
    pub error: Option<String>,
    pub media_access_granted: bool,
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
    log!("Connecting to ", &url);
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
        let webtransport_enabled = ctx.props().webtransport_enabled;
        let mut peer_decode_manager = PeerDecodeManager::new();
        let link = ctx.link().clone();
        peer_decode_manager.on_peer_added = Callback::from(move |email| {
            link.send_message(Msg::OnPeerAdded(email));
        });
        let link = ctx.link().clone();
        peer_decode_manager.on_first_frame = Callback::from(move |(email, media_type)| {
            link.send_message(Msg::OnFirstFrame((email, media_type)));
        });
        peer_decode_manager.get_video_canvas_id = Callback::from(|email| email);
        peer_decode_manager.get_screen_canvas_id =
            Callback::from(|email| format!("screen-share-{}", &email));
        Self {
            connection: None,
            connected: false,
            connecting: false,
            media_packet: MediaPacket::default(),
            peer_decode_manager,
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

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        if first_render {
            ctx.link().send_message(WsAction::RequestMediaPermissions);
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::WsAction(action) => match action {
                WsAction::Connect(webtransport) => {
                    if self.connecting {
                        return false;
                    }
                    self.connecting = true;
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
                        let task = connect_webtransport(ctx, &email, &id);
                        match task {
                            Ok(task) => {
                                self.connection = Some(Connection::WebTransport(task));
                            }
                            Err(_e) => {
                                log!("WebTransport connect failed, falling back to WebSocket");
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
                WsAction::Connected => {
                    log!("Connected");
                    self.connecting = false;
                    self.connected = true;
                    true
                }
                WsAction::Log(msg) => {
                    log!("{}", msg);
                    false
                }
                WsAction::Lost(_reason) => {
                    log!("Lost");
                    self.connected = false;
                    self.connecting = false;
                    self.connection.take();
                    if let Some(heartbeat) = self.heartbeat.take() {
                        heartbeat.cancel();
                    };
                    ctx.link()
                        .send_message(WsAction::Connect(self.webtransport_enabled));
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
            Msg::OnPeerAdded(_email) => true,
            Msg::OnFirstFrame((_email, media_type)) => match media_type {
                MediaType::SCREEN => true,
                _ => false,
            },
            Msg::OnInboundMedia(response) => {
                if let Err(e) = self.peer_decode_manager.decode(response) {
                    log!("error decoding packet: {:?}", e);
                }
                return false;
            }
            Msg::OnOutboundPacket(media) => {
                if let Some(connection) = &self.connection {
                    match connection {
                        Connection::WebSocket(ws) => {
                            if self.connected {
                                match media
                                    .write_to_bytes()
                                    .map_err(|w| JsValue::from(format!("{:?}", w)))
                                {
                                    Ok(bytes) => {
                                        ws.send_binary(bytes);
                                    }
                                    Err(e) => {
                                        let packet_type = media.media_type.enum_value_or_default();
                                        log!(
                                            "error sending {} packet: {:?}",
                                            JsValue::from(format!("{}", packet_type)),
                                            e
                                        );
                                    }
                                }
                            }
                        }
                        Connection::WebTransport(wt) => {
                            if self.connected {
                                match media
                                    .write_to_bytes()
                                    .map_err(|w| JsValue::from(format!("{:?}", w)))
                                {
                                    Ok(bytes) => {
                                        WebTransportTask::send_unidirectional_stream(
                                            wt.transport.clone(),
                                            bytes,
                                        );
                                    }
                                    Err(e) => {
                                        let packet_type = media.media_type.enum_value_or_default();
                                        log!(
                                            "error sending {} packet: {:?}",
                                            JsValue::from(format!("{}", packet_type)),
                                            e
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                false
            }
            Msg::OnDatagram(bytes) => {
                let media_packet = MediaPacket::parse_from_bytes(&bytes);
                match media_packet {
                    Ok(media_packet) => {
                        ctx.link()
                            .send_message(Msg::OnInboundMedia(MediaPacketWrapper(media_packet)));
                    }
                    Err(e) => {
                        let e = JsValue::from(format!("{:?}", e));
                        log!("error parsing datagram: {:?}", e);
                    }
                }
                false
            }
            Msg::OnMessage(response, message_type) => {
                let res = MediaPacket::parse_from_bytes(&response);
                if let Ok(media_packet) = res {
                    ctx.link()
                        .send_message(Msg::OnInboundMedia(MediaPacketWrapper(media_packet)));
                } else {
                    let message_type = format!("{:?}", message_type);
                    log!("failed to parse media packet ", message_type);
                }
                false
            }
            Msg::OnUniStream(stream) => {
                if stream.is_undefined() {
                    log!("stream is undefined");
                    return true;
                }
                let incoming_unistreams: ReadableStreamDefaultReader =
                    stream.get_reader().unchecked_into();
                let callback = ctx
                    .link()
                    .callback(|d| Msg::OnMessage(d, WebTransportMessageType::UnidirectionalStream));
                wasm_bindgen_futures::spawn_local(async move {
                    let mut buffer: Vec<u8> = vec![];
                    loop {
                        let read_result = JsFuture::from(incoming_unistreams.read()).await;
                        match read_result {
                            Err(e) => {
                                let mut reason = WebTransportCloseInfo::default();
                                reason.reason(
                                    format!("Failed to read incoming unistream {e:?}").as_str(),
                                );
                                break;
                            }
                            Ok(result) => {
                                let done = Reflect::get(&result, &JsString::from("done"))
                                    .unwrap()
                                    .unchecked_into::<Boolean>();

                                let value =
                                    Reflect::get(&result, &JsString::from("value")).unwrap();
                                if !value.is_undefined() {
                                    let value: Uint8Array = value.unchecked_into();
                                    append_uint8_array_to_vec(&mut buffer, &value);
                                }

                                if done.is_truthy() {
                                    process_binary(buffer, &callback);
                                    break;
                                }
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
                    let mut buffer: Vec<u8> = vec![];
                    loop {
                        log!("reading from stream");
                        let read_result = JsFuture::from(readable.read()).await;

                        match read_result {
                            Err(e) => {
                                let mut reason = WebTransportCloseInfo::default();
                                reason.reason(
                                    format!("Failed to read incoming bidistream {e:?}").as_str(),
                                );
                                break;
                            }
                            Ok(result) => {
                                let done = Reflect::get(&result, &JsString::from("done"))
                                    .unwrap()
                                    .unchecked_into::<Boolean>();
                                let value =
                                    Reflect::get(&result, &JsString::from("value")).unwrap();
                                if !value.is_undefined() {
                                    let value: Uint8Array = value.unchecked_into();
                                    append_uint8_array_to_vec(&mut buffer, &value);
                                }
                                if done.is_truthy() {
                                    process_binary(buffer, &callback);
                                    break;
                                }
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
            .peer_decode_manager
            .sorted_keys()
            .iter()
            .map(|key| {
                let peer = match self.peer_decode_manager.get(key) {
                    Some(peer) => peer,
                    None => return html! {},
                };

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
                    </div>
                    {
                        if media_access_granted {
                            html! {<Host on_packet={on_packet} email={email.clone()} share_screen={self.share_screen} mic_enabled={self.mic_enabled} video_enabled={self.video_enabled}/>}
                        } else {
                            html! {<></>}
                        }
                    }
                    <h4 class="floating-name">{email}</h4>
                    {if !self.connected {
                        html! {<h4>{"Connecting"}</h4>}
                    } else {
                        html! {<h4>{"Connected"}</h4>}
                    }}
                </nav>
            </div>
        }
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

pub fn append_uint8_array_to_vec(rust_vec: &mut Vec<u8>, js_array: &Uint8Array) {
    // Convert the Uint8Array into a Vec<u8>
    let mut temp_vec = vec![0; js_array.length() as usize];
    js_array.copy_to(&mut temp_vec);

    // Append it to the existing Rust Vec<u8>
    rust_vec.append(&mut temp_vec);
}

pub fn process_binary(bytes: Vec<u8>, callback: &Callback<Vec<u8>>) {
    callback.emit(bytes);
}

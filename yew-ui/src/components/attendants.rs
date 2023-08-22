use std::rc::Rc;

use crate::constants::WEBTRANSPORT_HOST;
use crate::model::connection::{ConnectOptions, Connection};
use crate::model::decode::PeerDecodeManager;
use crate::model::media_devices::MediaDeviceAccess;
use crate::model::MediaPacketWrapper;
use crate::{components::host::Host, constants::ACTIX_WEBSOCKET};
use gloo_console::log;
use types::protos::media_packet::media_packet::MediaType;
use types::protos::media_packet::MediaPacket;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

use super::icons::push_pin::PushPinIcon;
use web_sys::*;
use yew::prelude::*;
use yew::virtual_dom::VNode;
use yew::{html, Component, Context, Html};

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

pub struct AttendantsComponent {
    pub connection: Option<Connection>,
    pub peer_decode_manager: PeerDecodeManager,
    pub media_device_access: MediaDeviceAccess,
    pub outbound_audio_buffer: [u8; 2000],
    pub share_screen: bool,
    pub webtransport_enabled: bool,
    pub mic_enabled: bool,
    pub video_enabled: bool,
    pub error: Option<String>,
}

impl AttendantsComponent {
    fn is_connected(&self) -> bool {
        match &self.connection {
            Some(connection) => connection.is_connected(),
            None => false,
        }
    }

    fn create_peer_decoder_manager(ctx: &Context<Self>) -> PeerDecodeManager {
        let mut peer_decode_manager = PeerDecodeManager::new();
        peer_decode_manager.on_peer_added = {
            let link = ctx.link().clone();
            Callback::from(move |email| link.send_message(Msg::OnPeerAdded(email)))
        };
        peer_decode_manager.on_first_frame = {
            let link = ctx.link().clone();
            Callback::from(move |(email, media_type)| {
                link.send_message(Msg::OnFirstFrame((email, media_type)))
            })
        };
        peer_decode_manager.get_video_canvas_id = Callback::from(|email| email);
        peer_decode_manager.get_screen_canvas_id =
            Callback::from(|email| format!("screen-share-{}", &email));
        peer_decode_manager
    }

    fn create_media_device_access(ctx: &Context<Self>) -> MediaDeviceAccess {
        let mut media_device_access = MediaDeviceAccess::new();
        media_device_access.on_granted = {
            let link = ctx.link().clone();
            Callback::from(move |_| link.send_message(WsAction::MediaPermissionsGranted))
        };
        media_device_access.on_denied = {
            let link = ctx.link().clone();
            Callback::from(move |_| {
                link.send_message(WsAction::MediaPermissionsError("Error requesting permissions. Please make sure to allow access to both camera and microphone.".to_string()))
            })
        };
        media_device_access
    }
}

impl Component for AttendantsComponent {
    type Message = Msg;
    type Properties = AttendantsComponentProps;

    fn create(ctx: &Context<Self>) -> Self {
        Self {
            connection: None,
            peer_decode_manager: Self::create_peer_decoder_manager(ctx),
            media_device_access: Self::create_media_device_access(ctx),
            outbound_audio_buffer: [0; 2000],
            share_screen: false,
            mic_enabled: false,
            video_enabled: false,
            webtransport_enabled: ctx.props().webtransport_enabled,
            error: None,
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
                    if self.connection.is_some() {
                        return false;
                    }
                    log!("webtransport connect = {}", webtransport);
                    let id = ctx.props().id.clone();
                    let email = ctx.props().email.clone();
                    let options = ConnectOptions {
                        userid: email.clone(),
                        websocket_url: format!("{ACTIX_WEBSOCKET}/{email}/{id}"),
                        webtransport_url: format!("{WEBTRANSPORT_HOST}/{email}/{id}"),
                        on_inbound_media: ctx.link().callback(Msg::OnInboundMedia),
                        on_connected: ctx.link().callback(|_| Msg::from(WsAction::Connected)),
                        on_connection_lost: ctx
                            .link()
                            .callback(|_| Msg::from(WsAction::Lost(None))),
                    };
                    match Connection::connect(webtransport, options) {
                        Ok(connection) => {
                            self.connection = Some(connection);
                        }
                        Err(e) => {
                            ctx.link()
                                .send_message(WsAction::Log(format!("Connection failed: {e}")));
                        }
                    }

                    true
                }
                WsAction::Connected => {
                    log!("Connected");
                    true
                }
                WsAction::Log(msg) => {
                    log!("{}", msg);
                    false
                }
                WsAction::Lost(_reason) => {
                    log!("Lost");
                    self.connection = None;
                    ctx.link()
                        .send_message(WsAction::Connect(self.webtransport_enabled));
                    true
                }
                WsAction::RequestMediaPermissions => {
                    self.media_device_access.request();
                    false
                }
                WsAction::MediaPermissionsGranted => {
                    self.error = None;
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
                false
            }
            Msg::OnOutboundPacket(media) => {
                if let Some(connection) = &self.connection {
                    connection.send_packet(media);
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
        let media_access_granted = self.media_device_access.is_granted();
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

                    {if !self.is_connected() {
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

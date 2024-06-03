use crate::constants::{USERS_ALLOWED_TO_STREAM, WEBTRANSPORT_HOST};
use crate::{components::host::Host, constants::ACTIX_WEBSOCKET};
use crate::components::{peer_list::PeerList, canvas_generator};
use log::{error, warn};
use types::protos::media_packet::media_packet::MediaType;
use videocall_client::{MediaDeviceAccess, VideoCallClient, VideoCallClientOptions};
use wasm_bindgen::JsValue;
use web_sys::*;
use yew::prelude::*;
use yew::{html, Component, Context, Html};

#[derive(Debug)]
pub enum WsAction {
    Connect,
    Connected,
    Lost(Option<JsValue>),
    RequestMediaPermissions,
    MediaPermissionsGranted,
    MediaPermissionsError(String),
    Log(String),
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
pub enum MeetingAction {
    ToggleScreenShare,
    ToggleMicMute,
    ToggleVideoOnOff,
}

pub enum UserScreenAction {
    TogglePeerList,
}

pub enum Msg {
    WsAction(WsAction),
    MeetingAction(MeetingAction),
    OnPeerAdded(String),
    OnFirstFrame((String, MediaType)),
    UserScreenAction(UserScreenAction),
}

impl From<WsAction> for Msg {
    fn from(action: WsAction) -> Self {
        Msg::WsAction(action)
    }
}

impl From<UserScreenAction> for Msg {
    fn from(action: UserScreenAction) -> Self {
        Msg::UserScreenAction(action)
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

    pub e2ee_enabled: bool,

    pub webtransport_enabled: bool,
}

pub struct AttendantsComponent {
    pub client: VideoCallClient,
    pub media_device_access: MediaDeviceAccess,
    pub share_screen: bool,
    pub mic_enabled: bool,
    pub video_enabled: bool,
    pub peer_list_open: bool,
    pub error: Option<String>,
}

impl AttendantsComponent {
    fn create_video_call_client(ctx: &Context<Self>) -> VideoCallClient {
        let email = ctx.props().email.clone();
        let id = ctx.props().id.clone();
        let opts = VideoCallClientOptions {
            userid: email.clone(),
            websocket_url: format!("{ACTIX_WEBSOCKET}/{email}/{id}"),
            webtransport_url: format!("{WEBTRANSPORT_HOST}/{email}/{id}"),
            enable_e2ee: ctx.props().e2ee_enabled,
            enable_webtransport: ctx.props().webtransport_enabled,
            on_connected: {
                let link = ctx.link().clone();
                Callback::from(move |_| link.send_message(Msg::from(WsAction::Connected)))
            },
            on_connection_lost: {
                let link = ctx.link().clone();
                Callback::from(move |_| link.send_message(Msg::from(WsAction::Lost(None))))
            },
            on_peer_added: {
                let link = ctx.link().clone();
                Callback::from(move |email| link.send_message(Msg::OnPeerAdded(email)))
            },
            on_peer_first_frame: {
                let link = ctx.link().clone();
                Callback::from(move |(email, media_type)| {
                    link.send_message(Msg::OnFirstFrame((email, media_type)))
                })
            },
            get_peer_video_canvas_id: Callback::from(|email| email),
            get_peer_screen_canvas_id: Callback::from(|email| format!("screen-share-{}", &email)),
        };
        VideoCallClient::new(opts)
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
            client: Self::create_video_call_client(ctx),
            media_device_access: Self::create_media_device_access(ctx),
            share_screen: false,
            mic_enabled: false,
            video_enabled: false,
            peer_list_open: false,
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
                WsAction::Connect => {
                    if self.client.is_connected() {
                        return false;
                    }
                    if let Err(e) = self.client.connect() {
                        ctx.link()
                            .send_message(WsAction::Log(format!("Connection failed: {e}")));
                    }
                    true
                }
                WsAction::Connected => true,
                WsAction::Log(msg) => {
                    warn!("{}", msg);
                    false
                }
                WsAction::Lost(_reason) => {
                    warn!("Lost");
                    ctx.link().send_message(WsAction::Connect);
                    true
                }
                WsAction::RequestMediaPermissions => {
                    self.media_device_access.request();
                    ctx.link().send_message(WsAction::Connect);
                    false
                }
                WsAction::MediaPermissionsGranted => {
                    self.error = None;
                    ctx.link().send_message(WsAction::Connect);
                    true
                }
                WsAction::MediaPermissionsError(error) => {
                    self.error = Some(error);
                    true
                }
            },
            Msg::OnPeerAdded(_email) => true,
            Msg::OnFirstFrame((_email, media_type)) => matches!(media_type, MediaType::SCREEN),
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
            },
            Msg::UserScreenAction(action) => {
                match action {
                    UserScreenAction::TogglePeerList => {
                        self.peer_list_open = !self.peer_list_open;
                    },
                }
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let email = ctx.props().email.clone();
        let media_access_granted = self.media_device_access.is_granted();

        let toggle_peer_list = ctx.link().callback(|_| UserScreenAction::TogglePeerList);
        let dummy_peers: Vec<String> = vec!["Mark".to_owned(), "Stephen".to_owned(), "Rustling".to_owned(), "He owes me money".to_owned()];

        let peers = self.client.sorted_peer_keys();
        let rows = canvas_generator::generate(&self.client, peers);

        html! {
            <div id="main-container">
                <div id="grid-container" style={if self.peer_list_open {"width: 80%;"} else {"width: 100%;"}}>
                    { self.error.as_ref().map(|error| html! { <p>{ error }</p> }) }
                    { rows }
                    {
                        if USERS_ALLOWED_TO_STREAM.iter().any(|host| host == &email) || USERS_ALLOWED_TO_STREAM.is_empty() {
                            html! {
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
                                            onclick={toggle_peer_list.clone()}>
                                            { if !self.peer_list_open { "Open Peers"} else { "Close Peers"} }
                                        </button>
                                    </div>
                                    {
                                        if media_access_granted {
                                            html! {<Host client={self.client.clone()} share_screen={self.share_screen} mic_enabled={self.mic_enabled} video_enabled={self.video_enabled} />}
                                        } else {
                                            html! {<></>}
                                        }
                                    }
                                    <h4 class="floating-name">{email}</h4>

                                    {if !self.client.is_connected() {
                                        html! {<h4>{"Connecting"}</h4>}
                                    } else {
                                        html! {<h4>{"Connected"}</h4>}
                                    }}

                                    {if ctx.props().e2ee_enabled {
                                        html! {<h4>{"End to End Encryption Enabled"}</h4>}
                                    } else {
                                        html! {<h4>{"End to End Encryption Disabled"}</h4>}
                                    }}
                                </nav>
                            }
                        } else {
                            error!("User not allowed to stream");
                            error!("allowed users {}", USERS_ALLOWED_TO_STREAM.join(", "));
                            html! {}
                        }
                    }
                </div>
                <div id="peer-list-container" class={if self.peer_list_open {"visible"} else {""}}>
                    <PeerList peers={dummy_peers} onclose={toggle_peer_list} />
                </div>
            </div>
        }
    }
}


use crate::components::{canvas_generator, peer_list::PeerList};
use crate::constants::{CANVAS_LIMIT, USERS_ALLOWED_TO_STREAM, WEBTRANSPORT_HOST};
use crate::{components::host::Host, constants::ACTIX_WEBSOCKET};
use log::{error, warn};
use videocall_client::{MediaDeviceAccess, VideoCallClient, VideoCallClientOptions};
use videocall_types::protos::media_packet::media_packet::MediaType;
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

#[derive(Debug)]
pub enum UserScreenAction {
    TogglePeerList,
}

#[derive(Debug)]
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
    pending_mic_enable: bool,
    pending_video_enable: bool,
    pending_screen_share: bool,
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
            Callback::from(move |e| {
                let complete_error = format!("Error requesting permissions: Please make sure to allow access to both camera and microphone. ({:?})", e);
                error!("{}", complete_error);
                link.send_message(WsAction::MediaPermissionsError(complete_error.to_string()))
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
            pending_mic_enable: false,
            pending_video_enable: false,
            pending_screen_share: false,
        }
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        if first_render {
            ctx.link().send_message(WsAction::Connect);
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        log::info!("AttendantsComponent update: {:?}", msg);
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
                    log::info!("Connected in attendants");
                    true
                }
                WsAction::Connected => true,
                WsAction::Log(msg) => {
                    warn!("{}", msg);
                    false
                }
                WsAction::Lost(reason) => {
                    warn!("Lost with reason {:?}", reason);
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
                    
                    if self.pending_mic_enable {
                        self.mic_enabled = true;
                        self.pending_mic_enable = false;
                    }
                    
                    if self.pending_video_enable {
                        self.video_enabled = true;
                        self.pending_video_enable = false;
                    }
                    
                    if self.pending_screen_share {
                        self.share_screen = true;
                        self.pending_screen_share = false;
                    }
                    
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
                        if !self.share_screen {
                            if self.media_device_access.is_granted() {
                                self.share_screen = true;
                            } else {
                                self.pending_screen_share = true;
                                ctx.link().send_message(WsAction::RequestMediaPermissions);
                            }
                        } else {
                            self.share_screen = false;
                        }
                    }
                    MeetingAction::ToggleMicMute => {
                        if !self.mic_enabled {
                            if self.media_device_access.is_granted() {
                                self.mic_enabled = true;
                            } else {
                                self.pending_mic_enable = true;
                                ctx.link().send_message(WsAction::RequestMediaPermissions);
                            }
                        } else {
                            self.mic_enabled = false;
                        }
                    }
                    MeetingAction::ToggleVideoOnOff => {
                        if !self.video_enabled {
                            if self.media_device_access.is_granted() {
                                self.video_enabled = true;
                            } else {
                                self.pending_video_enable = true;
                                ctx.link().send_message(WsAction::RequestMediaPermissions);
                            }
                        } else {
                            self.video_enabled = false;
                        }
                    }
                }
                true
            }
            Msg::UserScreenAction(action) => {
                match action {
                    UserScreenAction::TogglePeerList => {
                        self.peer_list_open = !self.peer_list_open;
                    }
                }
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let email = ctx.props().email.clone();
        let media_access_granted = self.media_device_access.is_granted();

        let toggle_peer_list = ctx.link().callback(|_| UserScreenAction::TogglePeerList);

        let peers = self.client.sorted_peer_keys();
        let rows = canvas_generator::generate(
            &self.client,
            peers.iter().take(CANVAS_LIMIT).cloned().collect(),
        );

        html! {
            <div id="main-container" class="meeting-page">
                <div id="grid-container" style={if self.peer_list_open {"width: 80%;"} else {"width: 100%;"}}>
                    { 
                        self.error.as_ref().map(|error| html! { 
                            <div class="error-container">
                                <p class="error-message">{ error }</p>
                                <img src="/assets/instructions.gif" alt="Permission instructions" class="instructions-gif" />
                            </div> 
                        }) 
                    }
                    { rows }
                    {
                        if USERS_ALLOWED_TO_STREAM.iter().any(|host| host == &email) || USERS_ALLOWED_TO_STREAM.is_empty() {
                            html! {
                                <nav class="host">
                                    <div class="controls">
                                        <nav class="video-controls-container">
                                            <button
                                                class={classes!("video-control-button", self.mic_enabled.then_some("active"))}
                                                onclick={ctx.link().callback(|_| MeetingAction::ToggleMicMute)}>
                                                {
                                                    if self.mic_enabled {
                                                        html! {
                                                            <>
                                                                <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                                    <path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3z"></path>
                                                                    <path d="M19 10v2a7 7 0 0 1-14 0v-2"></path>
                                                                    <line x1="12" y1="19" x2="12" y2="22"></line>
                                                                </svg>
                                                                <span class="tooltip">{ "Mute" }</span>
                                                            </>
                                                        }
                                                    } else {
                                                        html! {
                                                            <>
                                                                <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                                    <line x1="1" y1="1" x2="23" y2="23"></line>
                                                                    <path d="M9 9v3a3 3 0 0 0 5.12 2.12M15 9.34V5a3 3 0 0 0-5.94-.6"></path>
                                                                    <path d="M17 16.95A7 7 0 0 1 5 12v-2m14 0v2a7 7 0 0 1-.11 1.23"></path>
                                                                    <line x1="12" y1="19" x2="12" y2="22"></line>
                                                                </svg>
                                                                <span class="tooltip">{ "Unmute" }</span>
                                                            </>
                                                        }
                                                    }
                                                }
                                            </button>
                                            <button
                                                class={classes!("video-control-button", self.video_enabled.then_some("active"))}
                                                onclick={ctx.link().callback(|_| MeetingAction::ToggleVideoOnOff)}>
                                                {
                                                    if self.video_enabled {
                                                        html! {
                                                            <>
                                                                <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                                    <polygon points="23 7 16 12 23 17 23 7"></polygon>
                                                                    <rect x="1" y="5" width="15" height="14" rx="2" ry="2"></rect>
                                                                </svg>
                                                                <span class="tooltip">{ "Stop Video" }</span>
                                                            </>
                                                        }
                                                    } else {
                                                        html! {
                                                            <>
                                                                <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                                    <path d="M16 16v1a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2m5.66 0H14a2 2 0 0 1 2 2v3.34l1 1L23 7v10"></path>
                                                                    <line x1="1" y1="1" x2="23" y2="23"></line>
                                                                </svg>
                                                                <span class="tooltip">{ "Start Video" }</span>
                                                            </>
                                                        }
                                                    }
                                                }
                                            </button>
                                            <button
                                                class={classes!("video-control-button", self.share_screen.then_some("active"))}
                                                onclick={ctx.link().callback(|_| MeetingAction::ToggleScreenShare)}>
                                                {
                                                    if self.share_screen {
                                                        html! {
                                                            <>
                                                                <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                                    <rect x="2" y="3" width="20" height="14" rx="2" ry="2"></rect>
                                                                    <line x1="8" y1="21" x2="16" y2="21"></line>
                                                                    <line x1="12" y1="17" x2="12" y2="21"></line>
                                                                </svg>
                                                                <span class="tooltip">{ "Stop Screen Share" }</span>
                                                            </>
                                                        }
                                                    } else {
                                                        html! {
                                                            <>
                                                                <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                                    <path d="M13 3H4a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-3"></path>
                                                                    <polyline points="8 21 12 17 16 21"></polyline>
                                                                    <polyline points="16 7 20 7 20 3"></polyline>
                                                                    <line x1="10" y1="14" x2="21" y2="3"></line>
                                                                </svg>
                                                                <span class="tooltip">{ "Share Screen" }</span>
                                                            </>
                                                        }
                                                    }
                                                }
                                            </button>
                                            <button
                                                class={classes!("video-control-button", self.peer_list_open.then_some("active"))}
                                                onclick={toggle_peer_list.clone()}>
                                                {
                                                    if self.peer_list_open {
                                                        html! {
                                                            <>
                                                                <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                                    <path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"></path>
                                                                    <circle cx="9" cy="7" r="4"></circle>
                                                                    <path d="M23 21v-2a4 4 0 0 0-3-3.87"></path>
                                                                    <path d="M16 3.13a4 4 0 0 1 0 7.75"></path>
                                                                    <line x1="1" y1="1" x2="23" y2="23"></line>
                                                                </svg>
                                                                <span class="tooltip">{ "Close Peers" }</span>
                                                            </>
                                                        }
                                                    } else {
                                                        html! {
                                                            <>
                                                                <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                                    <path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"></path>
                                                                    <circle cx="9" cy="7" r="4"></circle>
                                                                    <path d="M23 21v-2a4 4 0 0 0-3-3.87"></path>
                                                                    <path d="M16 3.13a4 4 0 0 1 0 7.75"></path>
                                                                </svg>
                                                                <span class="tooltip">{ "Open Peers" }</span>
                                                            </>
                                                        }
                                                    }
                                                }
                                            </button>
                                        </nav>
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
                    <PeerList peers={peers} onclose={toggle_peer_list} />
                </div>
            </div>
        }
    }
}

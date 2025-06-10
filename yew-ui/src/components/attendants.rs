use crate::components::{
    browser_compatibility::BrowserCompatibility, canvas_generator, peer_list::PeerList,
};
use crate::constants::{CANVAS_LIMIT, USERS_ALLOWED_TO_STREAM, WEBTRANSPORT_HOST};
use crate::{components::host::Host, constants::ACTIX_WEBSOCKET};
use log::{debug, error, warn};
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
    DiagnosticsUpdated(String),
    SenderStatsUpdated(String),
    EncoderSettingsUpdated(String),
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
    ToggleDiagnostics,
}

#[derive(Debug)]
pub enum Msg {
    WsAction(WsAction),
    MeetingAction(MeetingAction),
    OnPeerAdded(String),
    OnFirstFrame((String, MediaType)),
    UserScreenAction(UserScreenAction),
    #[cfg(feature = "fake-peers")]
    AddFakePeer,
    #[cfg(feature = "fake-peers")]
    RemoveLastFakePeer,
    ToggleForceDesktopGrid,
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
    pub diagnostics_open: bool,
    pub error: Option<String>,
    pub diagnostics_data: Option<String>,
    pub sender_stats: Option<String>,
    pub encoder_settings: Option<String>,
    pending_mic_enable: bool,
    pending_video_enable: bool,
    pending_screen_share: bool,
    pub meeting_joined: bool,
    fake_peer_ids: Vec<String>,
    next_fake_peer_id_counter: usize,
    force_desktop_grid_on_mobile: bool,
    simulation_info_message: Option<String>,
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
            enable_diagnostics: true,
            on_diagnostics_update: Some({
                let link = ctx.link().clone();
                Callback::from(move |stats| {
                    link.send_message(Msg::from(WsAction::DiagnosticsUpdated(stats)))
                })
            }),
            on_sender_stats_update: Some({
                let link = ctx.link().clone();
                Callback::from(move |stats| {
                    link.send_message(Msg::from(WsAction::SenderStatsUpdated(stats)))
                })
            }),
            diagnostics_update_interval_ms: Some(1000),
            on_encoder_settings_update: Some({
                let link = ctx.link().clone();
                Callback::from(move |settings| {
                    link.send_message(Msg::from(WsAction::EncoderSettingsUpdated(settings)))
                })
            }),
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

    #[cfg(feature = "fake-peers")]
    fn view_fake_peer_buttons(&self, ctx: &Context<Self>, add_fake_peer_disabled: bool) -> Html {
        html! {
            <>
                <button
                    class="video-control-button test-button"
                    title="Add Fake Peer"
                    onclick={ctx.link().callback(|_| Msg::AddFakePeer)}
                    disabled={add_fake_peer_disabled}
                    >
                    <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="feather feather-user-plus"><path d="M16 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"></path><circle cx="8.5" cy="7" r="4"></circle><line x1="20" y1="8" x2="20" y2="14"></line><line x1="17" y1="11" x2="23" y2="11"></line></svg>
                    <span class="tooltip">{ "Add Fake Peer" }</span>
                </button>
                <button
                    class="video-control-button test-button"
                    title="Remove Fake Peer"
                    onclick={ctx.link().callback(|_| Msg::RemoveLastFakePeer)}>
                    <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="feather feather-user-minus"><path d="M16 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"></path><circle cx="8.5" cy="7" r="4"></circle><line x1="23" y1="11" x2="17" y2="11"></line></svg>
                    <span class="tooltip">{ "Remove Fake Peer" }</span>
                </button>
            </>
        }
    }

    #[cfg(not(feature = "fake-peers"))]
    fn view_fake_peer_buttons(&self, _ctx: &Context<Self>, _add_fake_peer_disabled: bool) -> Html {
        html! {} // Empty html when feature is not enabled
    }

    #[cfg(feature = "fake-peers")]
    fn view_grid_toggle(&self, ctx: &Context<Self>) -> Html {
        html! {
            <>
                <button
                class={classes!("video-control-button", "test-button", "mobile-only-grid-toggle", self.force_desktop_grid_on_mobile.then_some("active"))}
                title={if self.force_desktop_grid_on_mobile { "Use Mobile Grid (Stack)" } else { "Force Desktop Grid (Multi-column)" }}
                onclick={ctx.link().callback(|_| Msg::ToggleForceDesktopGrid)}>
                {
                    if self.force_desktop_grid_on_mobile {
                        html!{
                            <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="7" height="7"></rect><rect x="14" y="3" width="7" height="7"></rect><rect x="14" y="14" width="7" height="7"></rect><rect x="3" y="14" width="7" height="7"></rect></svg>
                        }
                    } else {
                        html!{
                            <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="8" y1="6" x2="21" y2="6"></line><line x1="8" y1="12" x2="21" y2="12"></line><line x1="8" y1="18" x2="21" y2="18"></line><line x1="3" y1="6" x2="3.01" y2="6"></line><line x1="3" y1="12" x2="3.01" y2="12"></line><line x1="3" y1="18" x2="3.01" y2="18"></line></svg>
                        }
                    }
                }
                <span class="tooltip">{if self.force_desktop_grid_on_mobile { "Use Mobile Grid" } else { "Force Desktop Grid" }}</span>
            </button>
            </>
        }
    }

    #[cfg(not(feature = "fake-peers"))]
    fn view_grid_toggle(&self, _ctx: &Context<Self>) -> Html {
        html! {} // Empty html when feature is not enabled
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
            diagnostics_open: false,
            error: None,
            diagnostics_data: None,
            sender_stats: None,
            pending_mic_enable: false,
            pending_video_enable: false,
            pending_screen_share: false,
            encoder_settings: None,
            meeting_joined: false,
            fake_peer_ids: Vec::new(),
            next_fake_peer_id_counter: 1,
            force_desktop_grid_on_mobile: true,
            simulation_info_message: None,
        }
    }

    fn rendered(&mut self, _ctx: &Context<Self>, first_render: bool) {
        if first_render {
            // Don't auto-connect anymore
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        debug!("AttendantsComponent update: {:?}", msg);
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
                    self.meeting_joined = true;
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
                WsAction::DiagnosticsUpdated(stats) => {
                    self.diagnostics_data = Some(stats);
                    true
                }
                WsAction::SenderStatsUpdated(stats) => {
                    self.sender_stats = Some(stats);
                    true
                }
                WsAction::EncoderSettingsUpdated(settings) => {
                    self.encoder_settings = Some(settings);
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
                        if self.peer_list_open {
                            self.diagnostics_open = false;
                        }
                    }
                    UserScreenAction::ToggleDiagnostics => {
                        self.diagnostics_open = !self.diagnostics_open;
                        if self.diagnostics_open {
                            self.peer_list_open = false;
                        }
                    }
                }
                true
            }
            #[cfg(feature = "fake-peers")]
            Msg::RemoveLastFakePeer => {
                if !self.fake_peer_ids.is_empty() {
                    self.fake_peer_ids.pop();
                }
                self.simulation_info_message = None;
                true
            }
            #[cfg(feature = "fake-peers")]
            Msg::AddFakePeer => {
                let current_total_peers =
                    self.client.sorted_peer_keys().len() + self.fake_peer_ids.len();
                if current_total_peers < CANVAS_LIMIT {
                    let fake_peer_id = format!("fake-peer-{}", self.next_fake_peer_id_counter);
                    self.fake_peer_ids.push(fake_peer_id);
                    self.next_fake_peer_id_counter += 1;
                    self.simulation_info_message = None;
                } else {
                    log::warn!(
                        "Maximum participants ({}) reached. Cannot add more.",
                        CANVAS_LIMIT
                    );
                    self.simulation_info_message =
                        Some(format!("Maximum participants ({}) reached.", CANVAS_LIMIT));
                }
                true // Re-render to update button state or display message
            }
            Msg::ToggleForceDesktopGrid => {
                self.force_desktop_grid_on_mobile = !self.force_desktop_grid_on_mobile;
                self.simulation_info_message = None;
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let email = ctx.props().email.clone();
        let media_access_granted = self.media_device_access.is_granted();

        let toggle_peer_list = ctx.link().callback(|_| UserScreenAction::TogglePeerList);
        let toggle_diagnostics = ctx.link().callback(|_| UserScreenAction::ToggleDiagnostics);

        let real_peers_vec = self.client.sorted_peer_keys();
        let mut display_peers_vec = real_peers_vec.clone();
        display_peers_vec.extend(self.fake_peer_ids.iter().cloned());

        let num_display_peers = display_peers_vec.len();
        // Cap the number of peers used for styling at CANVAS_LIMIT
        let num_peers_for_styling = num_display_peers.min(CANVAS_LIMIT);

        // Determine if the "Add Fake Peer" button should be disabled
        let add_fake_peer_disabled = num_display_peers >= CANVAS_LIMIT;

        // Set default grid to desktop toggle
        let add_fake_peer_disabled = num_display_peers >= CANVAS_LIMIT;

        let rows = canvas_generator::generate(
            &self.client, // canvas_generator is client-aware for real peers' media status
            display_peers_vec
                .iter()
                .take(CANVAS_LIMIT)
                .cloned()
                .collect(),
        );

        let container_style = if self.peer_list_open || self.diagnostics_open {
            // Use num_peers_for_styling (capped at CANVAS_LIMIT) for the CSS variable
            format!("width: 80%; --num-peers: {};", num_peers_for_styling.max(1))
        } else {
            format!(
                "width: 100%; --num-peers: {};",
                num_peers_for_styling.max(1)
            )
        };

        let on_encoder_settings_update = ctx.link().callback(WsAction::EncoderSettingsUpdated);

        let mut grid_container_classes = classes!();
        if self.force_desktop_grid_on_mobile {
            grid_container_classes.push("force-desktop-grid");
        }

        // Show Join Meeting button if user hasn't joined yet
        if !self.meeting_joined {
            return html! {
                <div id="main-container" class="meeting-page">
                    <BrowserCompatibility/>
                    <div id="join-meeting-container" style="display: flex; flex-direction: column; align-items: center; justify-content: center; height: 100vh; background: #1a1a1a;">
                        <div style="text-align: center; color: white; margin-bottom: 2rem;">
                            <h1>{"Ready to join the meeting?"}</h1>
                            <p>{"Click the button below to join and start listening to others."}</p>
                            {if let Some(error) = &self.error {
                                html! { <p style="color: #ff6b6b; margin-top: 1rem;">{error}</p> }
                            } else {
                                html! {}
                            }}
                        </div>
                        <button
                            class="join-meeting-button"
                            style="
                                background: #4CAF50; 
                                color: white; 
                                border: none; 
                                padding: 1rem 2rem; 
                                font-size: 1.2rem; 
                                border-radius: 8px; 
                                cursor: pointer;
                                transition: background 0.3s ease;
                            "
                            onclick={ctx.link().callback(|_| WsAction::Connect)}
                        >
                            {"Join Meeting"}
                        </button>
                    </div>
                </div>
            };
        }

        html! {
            <div id="main-container" class="meeting-page">
                <BrowserCompatibility/>
                <div id="grid-container"
                    class={grid_container_classes}
                    data-peers={num_peers_for_styling.to_string()}
                    style={container_style}>
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
                                            <button
                                                class={classes!("video-control-button", self.diagnostics_open.then_some("active"))}
                                                onclick={toggle_diagnostics.clone()}>
                                                {
                                                    if self.diagnostics_open {
                                                        html! {
                                                            <>
                                                                <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                                    <path d="M2 12h2l3.5-7L12 19l2.5-5H20"></path>
                                                                    <line x1="3" y1="3" x2="21" y2="21"></line>
                                                                </svg>
                                                                <span class="tooltip">{ "Close Diagnostics" }</span>
                                                            </>
                                                        }
                                                    } else {
                                                        html! {
                                                            <>
                                                                <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                                    <path d="M2 12h2l3.5-7L12 19l2.5-5H20"></path>
                                                                </svg>
                                                                <span class="tooltip">{ "Open Diagnostics" }</span>
                                                            </>
                                                        }
                                                    }
                                                }
                                            </button>
                                            { self.view_grid_toggle(ctx) }
                                            { self.view_fake_peer_buttons(ctx, add_fake_peer_disabled) }

                                        </nav>
                                        // Display simulation info message if any
                                        {
                                            if let Some(message) = &self.simulation_info_message {
                                                html!{
                                                    <p class="simulation-info-message">{ message }</p>
                                                }
                                            } else {
                                                html!{}
                                            }
                                        }
                                    </div>
                                    {
                                        if media_access_granted {
                                            html! {<Host client={self.client.clone()} share_screen={self.share_screen} mic_enabled={self.mic_enabled} video_enabled={self.video_enabled} on_encoder_settings_update={on_encoder_settings_update} />}
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
                    <PeerList peers={display_peers_vec} onclose={toggle_peer_list} />
                </div>
                <div id="diagnostics-sidebar" class={if self.diagnostics_open {"visible"} else {""}}>
                    <div class="sidebar-header">
                        <h2>{"Diagnostics"}</h2>
                        <button class="close-button" onclick={toggle_diagnostics}>{"×"}</button>
                    </div>
                    <div class="sidebar-content">
                        <div class="diagnostics-data">
                            <div class="diagnostics-section">
                                <h3>{"Reception Stats"}</h3>
                                if let Some(data) = &self.diagnostics_data {
                                    <pre>{ data }</pre>
                                } else {
                                    <p>{"No reception data available."}</p>
                                }
                            </div>
                            <div class="diagnostics-section">
                                <h3>{"Sending Stats"}</h3>
                                if let Some(data) = &self.sender_stats {
                                    <pre>{ data }</pre>
                                } else {
                                    <p>{"No sending data available."}</p>
                                }
                            </div>
                            <div class="diagnostics-section">
                                <h3>{"Encoder Settings"}</h3>
                                if let Some(data) = &self.encoder_settings {
                                    <pre>{ data }</pre>
                                } else {
                                    <p>{"No encoder settings available."}</p>
                                }
                            </div>
                            <div class="diagnostics-section">
                                <h3>{"Media Status"}</h3>
                                <pre>{format!("Video: {}\nAudio: {}\nScreen Share: {}",
                                    if self.video_enabled { "Enabled" } else { "Disabled" },
                                    if self.mic_enabled { "Enabled" } else { "Disabled" },
                                    if self.share_screen { "Enabled" } else { "Disabled" }
                                )}</pre>
                            </div>
                        </div>
                    </div>
                </div>
            </div>
        }
    }
}

#[function_component(DiagnosticsSidebar)]
fn diagnostics_sidebar(props: &DiagnosticsSidebarProps) -> Html {
    let diagnostics_data = props.diagnostics_data.clone();
    let sender_stats = props.sender_stats.clone();
    let on_close = props.on_close.clone();

    html! {
        <div class="diagnostics-sidebar">
            <div class="diagnostics-header">
                <h2>{"Diagnostics"}</h2>
                <button class="close-button" onclick={on_close}>{"×"}</button>
            </div>
            <div class="diagnostics-data">
                <div class="diagnostics-section">
                    <h3>{"Reception Stats"}</h3>
                    if let Some(data) = diagnostics_data {
                        <pre class="diagnostics-text">{data}</pre>
                    } else {
                        <p>{"No reception data available."}</p>
                    }
                </div>
                <div class="diagnostics-section">
                    <h3>{"Sending Stats"}</h3>
                    if let Some(data) = sender_stats {
                        <pre class="diagnostics-text">{data}</pre>
                    } else {
                        <p>{"No sending data available."}</p>
                    }
                </div>
            </div>
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct DiagnosticsSidebarProps {
    pub diagnostics_data: Option<String>,
    pub sender_stats: Option<String>,
    pub on_close: Callback<MouseEvent>,
}

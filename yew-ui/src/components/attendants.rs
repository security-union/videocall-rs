/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use crate::components::{
    browser_compatibility::BrowserCompatibility,
    diagnostics::Diagnostics,
    host::Host,
    peer_list::PeerList,
    peer_tile::PeerTile,
    video_control_buttons::{
        CameraButton, DeviceSettingsButton, DiagnosticsButton, HangUpButton, MicButton,
        PeerListButton, ScreenShareButton,
    },
};
use crate::constants::actix_websocket_base;
use crate::constants::{
    server_election_period_ms, users_allowed_to_stream, webtransport_host_base, CANVAS_LIMIT,
};
use crate::context::{MeetingTime, MeetingTimeCtx, VideoCallClientCtx};
use gloo_timers::callback::Timeout;
use gloo_utils::window;
use log::{error, warn};
use serde::Deserialize;
use videocall_client::utils::is_ios;
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
    EncoderSettingsUpdated(String),
    MeetingInfoReceived(u64),
    ToggleDropdown,
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
pub enum MeetingAction {
    ToggleScreenShare,
    ToggleMicMute,
    ToggleVideoOnOff,
}

#[derive(Debug)]
pub enum UserScreenToggleAction {
    PeerList,
    Diagnostics,
    DeviceSettings,
    MeetingInfo,
}

#[derive(Debug)]
pub enum Msg {
    WsAction(WsAction),
    MeetingAction(MeetingAction),
    OnPeerAdded(String),
    OnPeerRemoved(String),
    OnFirstFrame((String, MediaType)),
    OnMicrophoneError(String),
    DismissMicError,
    UserScreenAction(UserScreenToggleAction),
    #[cfg(feature = "fake-peers")]
    AddFakePeer,
    #[cfg(feature = "fake-peers")]
    RemoveLastFakePeer,
    #[cfg(feature = "fake-peers")]
    ToggleForceDesktopGrid,
    HangUp,
    ShowCopyToast(bool),
    MeetingEnded(String),
}

impl From<WsAction> for Msg {
    fn from(action: WsAction) -> Self {
        Msg::WsAction(action)
    }
}

impl From<UserScreenToggleAction> for Msg {
    fn from(action: UserScreenToggleAction) -> Self {
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

    #[prop_or_default]
    pub user_name: Option<String>,

    #[prop_or_default]
    pub user_email: Option<String>,

    #[prop_or_default]
    pub on_logout: Option<Callback<()>>,
}

pub struct AttendantsComponent {
    pub client: VideoCallClient,
    pub media_device_access: MediaDeviceAccess,
    pub share_screen: bool,
    pub mic_enabled: bool,
    pub video_enabled: bool,
    pub peer_list_open: bool,
    pub diagnostics_open: bool,
    pub device_settings_open: bool,
    pub error: Option<String>,
    pub encoder_settings: Option<String>,
    pub mic_error: Option<String>,
    pending_mic_enable: bool,
    pending_video_enable: bool,
    pending_screen_share: bool,
    pub meeting_joined: bool,
    fake_peer_ids: Vec<String>,
    #[cfg(feature = "fake-peers")]
    next_fake_peer_id_counter: usize,
    force_desktop_grid_on_mobile: bool,
    simulation_info_message: Option<String>,
    show_copy_toast: bool,
    pub meeting_start_time_server: Option<f64>, //Server-provided meeting start timestamp - the actual meeting time
    pub call_start_time: Option<f64>,           // Track when the call started for a user
    show_dropdown: bool,
    meeting_ended_message: Option<String>,
    meeting_info_open: bool,
}

impl AttendantsComponent {
    fn create_video_call_client(ctx: &Context<Self>) -> VideoCallClient {
        let email = ctx.props().email.clone();
        let id = ctx.props().id.clone();
        let websocket_urls = actix_websocket_base()
            .unwrap_or_default()
            .split(',')
            .map(|s| format!("{s}/lobby/{email}/{id}"))
            .collect::<Vec<String>>();
        let webtransport_urls = webtransport_host_base()
            .unwrap_or_default()
            .split(',')
            .map(|s| format!("{s}/lobby/{email}/{id}"))
            .collect::<Vec<String>>();

        log::info!(
            "YEW-UI: Creating VideoCallClient for {} in meeting {} with webtransport_enabled={}",
            email,
            id,
            ctx.props().webtransport_enabled
        );
        if websocket_urls.is_empty() || webtransport_urls.is_empty() {
            log::error!("Runtime config missing or invalid: wsUrl or webTransportHost not set");
        }
        log::info!("YEW-UI: WebSocket URLs: {websocket_urls:?}");
        log::info!("YEW-UI: WebTransport URLs: {webtransport_urls:?}");

        let opts = VideoCallClientOptions {
            userid: email.clone(),
            meeting_id: id.clone(),
            websocket_urls,
            webtransport_urls,
            enable_e2ee: ctx.props().e2ee_enabled,
            enable_webtransport: ctx.props().webtransport_enabled,
            on_connected: {
                let link = ctx.link().clone();
                let webtransport_enabled = ctx.props().webtransport_enabled;
                Callback::from(move |_| {
                    log::info!(
                        "YEW-UI: Connection established (webtransport_enabled={webtransport_enabled})",
                    );
                    link.send_message(Msg::from(WsAction::Connected))
                })
            },
            on_connection_lost: {
                let link = ctx.link().clone();
                let webtransport_enabled = ctx.props().webtransport_enabled;
                Callback::from(move |_| {
                    log::warn!(
                        "YEW-UI: Connection lost (webtransport_enabled={webtransport_enabled})",
                    );
                    link.send_message(Msg::from(WsAction::Lost(None)))
                })
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
            on_peer_removed: Some({
                let link = ctx.link().clone();
                Callback::from(move |peer_id: String| {
                    log::info!("Peer removed: {peer_id}");
                    link.send_message(Msg::OnPeerRemoved(peer_id));
                })
            }),
            get_peer_video_canvas_id: Callback::from(|email| email),
            get_peer_screen_canvas_id: Callback::from(|email| format!("screen-share-{}", &email)),
            enable_diagnostics: true,
            diagnostics_update_interval_ms: Some(1000),
            enable_health_reporting: true,
            health_reporting_interval_ms: Some(5000),
            on_encoder_settings_update: Some({
                let link = ctx.link().clone();
                Callback::from(move |settings| {
                    link.send_message(Msg::from(WsAction::EncoderSettingsUpdated(settings)))
                })
            }),
            rtt_testing_period_ms: server_election_period_ms().unwrap_or(2000),
            rtt_probe_interval_ms: Some(200),
            on_meeting_info: Some({
                let link = ctx.link().clone();
                Callback::from(move |start_time_ms: f64| {
                    log::info!("Meeting started at Unix timestamp: {start_time_ms}");
                    link.send_message(Msg::WsAction(WsAction::MeetingInfoReceived(
                        start_time_ms as u64,
                    )))
                })
            }),
            on_meeting_ended: Some({
                let link = ctx.link().clone();
                Callback::from(move |(end_time_ms, message): (f64, String)| {
                    log::info!("Meeting ended at Unix timestamp: {end_time_ms}");
                    // link.send_message(Msg::WsAction(WsAction::MeetingInfoReceived(
                    //     end_time_ms as u64,
                    // )));
                    link.send_message(Msg::WsAction(WsAction::MeetingInfoReceived(
                        end_time_ms as u64,
                    )));
                    link.send_message(Msg::MeetingEnded(message));
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
                let complete_error = format!("Error requesting permissions: Please make sure to allow access to both camera and microphone. ({e:?})");
                error!("{complete_error}");
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

    fn play_user_joined() {
        if let Some(_window) = web_sys::window() {
            if let Ok(audio) = HtmlAudioElement::new_with_src("/assets/hi.wav") {
                audio.set_volume(0.4); // Set moderate volume
                if let Err(e) = audio.play() {
                    log::warn!("Failed to play notification sound: {e:?}");
                }
            } else {
                log::warn!("Failed to create audio element for notification sound");
            }
        }
    }

    fn format_meeting_duration(&self) -> String {
        log::info!(
            "format_meeting_duration - meeting_start_time_server: {:?}",
            self.meeting_start_time_server
        );
        if let Some(server_start_ms) = self.meeting_start_time_server {
            let now_ms = js_sys::Date::now();

            log::info!("Server start: {}, Now: {}", server_start_ms, now_ms);
            let elapsed_ms = (now_ms - server_start_ms).max(0.0);

            let elapsed_secs = (elapsed_ms / 1000.0) as u64;

            log::info!("Elapsed seconds: {}", elapsed_secs);
            let hours = elapsed_secs / 3600;
            let minutes = (elapsed_secs % 3600) / 60;
            let seconds = elapsed_secs % 60;

            if hours > 0 {
                format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
            } else {
                format!("{:02}:{:02}", minutes, seconds)
            }
        } else {
            "00:00".to_string()
        }
    }

    pub fn format_user_duration(&self) -> String {
        if let Some(local_start) = self.call_start_time {
            let now_ms = js_sys::Date::now();

            let elapsed_ms = (now_ms - local_start).max(0.0);
            let elapsed_secs = (elapsed_ms / 1000.0) as u64;
            let hours = elapsed_secs / 3600;
            let minutes = (elapsed_secs % 3600) / 60;
            let seconds = elapsed_secs % 60;

            if hours > 0 {
                format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
            } else {
                format!("{:02}:{:02}", minutes, seconds)
            }
        } else {
            "00:00".to_string()
        }
    }
}

impl Component for AttendantsComponent {
    type Message = Msg;
    type Properties = AttendantsComponentProps;

    fn create(ctx: &Context<Self>) -> Self {
        let client = Self::create_video_call_client(ctx);
        let media_device_access = Self::create_media_device_access(ctx);
        let mut self_ = Self {
            client,
            media_device_access,
            share_screen: false,
            mic_enabled: false,
            video_enabled: false,
            peer_list_open: false,
            diagnostics_open: false,
            device_settings_open: false,
            error: None,
            encoder_settings: None,
            mic_error: None,
            pending_mic_enable: false,
            pending_video_enable: false,
            pending_screen_share: false,
            meeting_joined: false,
            fake_peer_ids: Vec::new(),
            #[cfg(feature = "fake-peers")]
            next_fake_peer_id_counter: 1,
            force_desktop_grid_on_mobile: true,
            simulation_info_message: None,
            show_copy_toast: false,
            call_start_time: None,
            meeting_start_time_server: None,
            show_dropdown: false,
            meeting_ended_message: None,
            meeting_info_open: false,
        };
        if let Err(e) = crate::constants::app_config() {
            log::error!("{e:?}");
            self_.error = Some(e);
        }

        self_
    }

    fn rendered(&mut self, _ctx: &Context<Self>, first_render: bool) {
        if first_render {
            // Don't auto-connect anymore
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        log::debug!("YEW-UI: AttendantsComponent update: {msg:?}");
        match msg {
            Msg::WsAction(action) => match action {
                WsAction::Connect => {
                    if self.client.is_connected() {
                        return false;
                    }

                    if let Err(e) = self.client.connect() {
                        ctx.link()
                            .send_message(WsAction::Log(format!("Connection failed: {e:?}")));
                    }
                    log::info!("Connected in attendants");
                    self.meeting_joined = true;
                    true
                }
                WsAction::Connected => {
                    log::info!("YEW-UI: Connection established successfully!");
                    self.call_start_time = Some(js_sys::Date::now());
                    true
                }

                WsAction::Log(msg) => {
                    warn!("{msg}");
                    false
                }
                WsAction::Lost(reason) => {
                    warn!("Lost with reason {reason:?}");
                    ctx.link().send_message(WsAction::Connect);
                    true
                }
                WsAction::RequestMediaPermissions => {
                    self.media_device_access.request();
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
                    self.meeting_joined = false; // Stay on join screen if permissions denied
                    true
                }
                WsAction::EncoderSettingsUpdated(settings) => {
                    self.encoder_settings = Some(settings);
                    true
                }
                WsAction::MeetingInfoReceived(start_time) => {
                    log::info!("Meeting info received, start_time: {start_time:?}");
                    self.meeting_start_time_server = Some(start_time as f64);
                    true
                }
                WsAction::ToggleDropdown => {
                    self.show_dropdown = !self.show_dropdown;
                    true
                }
            },
            Msg::OnPeerAdded(email) => {
                log::info!("New user joined: {email}");
                // Play notification sound when a new user joins the call
                Self::play_user_joined();

                true
            }
            Msg::OnPeerRemoved(_peer_id) => {
                // Trigger a re-render; tiles are rebuilt from current client peer list
                true
            }
            Msg::OnFirstFrame((_email, media_type)) => matches!(media_type, MediaType::SCREEN),
            Msg::OnMicrophoneError(err) => {
                // Disable mic at the top and show UI
                log::error!("Microphone error (full): {err}");
                self.mic_enabled = false;
                self.mic_error = Some(err);
                true
            }
            Msg::DismissMicError => {
                self.mic_error = None;
                true
            }
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
                    UserScreenToggleAction::PeerList => {
                        self.peer_list_open = !self.peer_list_open;
                        if self.peer_list_open {
                            self.diagnostics_open = false;
                        }
                    }
                    UserScreenToggleAction::Diagnostics => {
                        self.diagnostics_open = !self.diagnostics_open;
                        if self.diagnostics_open {
                            self.peer_list_open = false;
                        }
                    }
                    UserScreenToggleAction::DeviceSettings => {
                        self.device_settings_open = !self.device_settings_open;
                        if self.device_settings_open {
                            self.peer_list_open = false;
                            self.diagnostics_open = false;
                        }
                    }

                    UserScreenToggleAction::MeetingInfo => {
                        self.meeting_info_open = !self.meeting_info_open;
                        if self.meeting_info_open {
                            //  self.peer_list_open = false;
                            self.diagnostics_open = false;
                            self.device_settings_open = false;
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
            #[cfg(feature = "fake-peers")]
            Msg::ToggleForceDesktopGrid => {
                self.force_desktop_grid_on_mobile = !self.force_desktop_grid_on_mobile;
                self.simulation_info_message = None;
                true
            }
            Msg::ShowCopyToast(show) => {
                self.show_copy_toast = show;
                if show {
                    let link = ctx.link().clone();
                    Timeout::new(1640, move || {
                        link.send_message(Msg::ShowCopyToast(false));
                    })
                    .forget();
                }
                true
            }
            Msg::HangUp => {
                log::info!("Hanging up - resetting to initial state");

                if self.client.is_connected() {
                    match self.client.disconnect() {
                        Ok(_) => {
                            log::info!("Disconnected from server");
                        }
                        Err(e) => {
                            log::error!("Error disconnecting from server: {e}");
                        }
                    }
                }

                self.meeting_joined = false;
                self.mic_enabled = false;
                self.video_enabled = false;
                self.call_start_time = None;
                self.meeting_start_time_server = None;

                Timeout::new(500, move || {
                    let _ = window().location().set_href("/");
                })
                .forget();

                true
            }

            Msg::MeetingEnded(end_time) => {
                self.meeting_ended_message = Some(end_time);
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let email = ctx.props().email.clone();
        let media_access_granted = self.media_device_access.is_granted();

        let toggle_peer_list = ctx.link().callback(|_| UserScreenToggleAction::PeerList);
        let toggle_diagnostics = ctx.link().callback(|_| UserScreenToggleAction::Diagnostics);
        let close_diagnostics = ctx.link().callback(|_| UserScreenToggleAction::Diagnostics);

        let real_peers_vec = self.client.sorted_peer_keys();
        let mut display_peers_vec = real_peers_vec.clone();
        display_peers_vec.extend(self.fake_peer_ids.iter().cloned());

        let num_display_peers = display_peers_vec.len();
        // Cap the number of peers used for styling at CANVAS_LIMIT
        let num_peers_for_styling = num_display_peers.min(CANVAS_LIMIT);

        // Determine if the "Add Fake Peer" button should be disabled
        let add_fake_peer_disabled = num_display_peers >= CANVAS_LIMIT;

        let rows: Vec<Html> = display_peers_vec
            .iter()
            .take(CANVAS_LIMIT)
            .enumerate()
            .map(|(i, peer_id)| {
                let full_bleed = display_peers_vec.len() == 1
                    && !self.client.is_screen_share_enabled_for_peer(peer_id);
                html!{ <PeerTile key={format!("tile-{}-{}", i, peer_id)} peer_id={peer_id.clone()} full_bleed={full_bleed} /> }
            })
            .collect();

        // Always let the grid take the whole stage; overlays should not shrink the grid
        let container_style = format!(
            "position: absolute; inset: 0; width: 100%; height: 100%; --num-peers: {};",
            num_peers_for_styling.max(1)
        );

        let on_encoder_settings_update = ctx.link().callback(WsAction::EncoderSettingsUpdated);

        // Compute meeting link for invitation overlay
        let meeting_link = {
            let origin_result = window().location().origin();
            // If obtaining origin fails, fallback to empty string
            let origin = origin_result.unwrap_or_else(|_| "".to_string());
            format!("{}/meeting/{}", origin, ctx.props().id)
        };

        // Callback to copy and trigger toast via component state
        let copy_meeting_link = {
            let meeting_link = meeting_link.clone();
            let link = ctx.link().clone();
            Callback::from(move |_| {
                if let Some(clipboard) = web_sys::window().map(|w| w.navigator().clipboard()) {
                    let _ = clipboard.write_text(&meeting_link);
                    link.send_message(Msg::ShowCopyToast(true));
                }
            })
        };

        let mut grid_container_classes = classes!();
        if self.force_desktop_grid_on_mobile {
            grid_container_classes.push("force-desktop-grid");
        }

        // Create the call timer HTML if the call has started
        let call_timer = if self.call_start_time.is_some() {
            html! {
                <div class="call-timer" >
                    { self.format_user_duration() }
                </div>
            }
        } else {
            html! {} // TODO: show a loading spinner
        };

        // In the view method, before the call_timer declaration
        log::info!(
            "Timer debug - meeting_joined: {}, meeting_start_time: {:?}, call_start_time: {:?}",
            self.meeting_joined,
            self.meeting_start_time_server,
            self.call_start_time
        );

        // Create the top-right controls
        let top_right_controls = html! {
            <div class="top-right-controls">
                {call_timer}
                <button
                    class={classes!("control-button", self.diagnostics_open.then_some("active"))}
                    onclick={toggle_diagnostics.clone()}
                >
                    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                        <circle cx="12" cy="12" r="10"></circle>
                        <line x1="12" y1="8" x2="12" y2="12"></line>
                        <line x1="12" y1="16" x2="12.01" y2="16"></line>
                    </svg>
                </button>
            </div>
        };

        // Show Join Meeting button if user hasn't joined yet
        if !self.meeting_joined {
            return html! {
                <ContextProvider<VideoCallClientCtx> context={self.client.clone()}>
                    <div id="main-container" class="meeting-page">
                        <BrowserCompatibility/>
                         {top_right_controls}
                    <div id="join-meeting-container" style="position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000; z-index: 1000;">
                        // Logout dropdown (top-right corner)
                        {
                            if let (Some(name), Some(email), Some(on_logout)) = (&ctx.props().user_name, &ctx.props().user_email, &ctx.props().on_logout) {
                                html! {
                                    <div style="position: absolute; top: 1rem; right: 1rem; z-index: 1001;">
                                        <button
                                            onclick={ctx.link().callback(|_| WsAction::ToggleDropdown)}
                                            class="flex items-center gap-2 px-4 py-2 bg-gray-800 hover:bg-gray-700 rounded-lg text-white text-sm transition-colors"
                                            style="display: flex; align-items: center; gap: 0.5rem; padding: 0.5rem 1rem; background: #1f2937; border-radius: 0.5rem; color: white; font-size: 0.875rem; transition: background 0.2s; border: none; cursor: pointer;"
                                        >
                                            <span>{name}</span>
                                            <svg style="width: 1rem; height: 1rem;" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 9l-7 7-7-7" />
                                            </svg>
                                        </button>

                                        {
                                            if self.show_dropdown {
                                                html! {
                                                    <div style="position: absolute; right: 0; margin-top: 0.5rem; width: 14rem; background: white; border-radius: 0.5rem; box-shadow: 0 10px 15px -3px rgba(0, 0, 0, 0.1); border: 1px solid #e5e7eb; padding: 0.25rem 0;">
                                                        <div style="padding: 0.75rem 1rem; border-bottom: 1px solid #e5e7eb;">
                                                            <p style="font-size: 0.875rem; font-weight: 500; color: #111827; margin: 0;">{name}</p>
                                                            <p style="font-size: 0.75rem; color: #6b7280; margin: 0; overflow: hidden; text-overflow: ellipsis;">{email}</p>
                                                        </div>
                                                        <button
                                                            onclick={on_logout.reform(|_| ())}
                                                            class="logout-button"
                                                            style="width: 100%; text-align: left; padding: 0.5rem 1rem; font-size: 0.875rem; color: #dc2626; background: transparent; border: none; cursor: pointer;"
                                                        >
                                                            {"Sign out"}
                                                        </button>
                                                    </div>
                                                }
                                            } else {
                                                html! {}
                                            }
                                        }
                                    </div>
                                }
                            } else {
                                html! {}
                            }
                        }

                        <div style="text-align: center; color: white; margin-bottom: 2rem;">
                            <h2>{"Ready to join the meeting?"}</h2>
                            <p>{"Click the button below to join and start listening to others."}</p>
                            {if let Some(error) = &self.error {
                                html! { <p style="color: #ff6b6b; margin-top: 1rem;">{error}</p> }
                            } else {
                                html! {}
                            }}
                        </div>
                        <button
                            class="btn-apple btn-primary"
                            onclick={ctx.link().callback(|_| WsAction::RequestMediaPermissions)}
                        >
                            {"Join Meeting"}
                        </button>
                    </div>
                    </div>
                </ContextProvider<VideoCallClientCtx>>
            };
        }

        // Create MeetingTime for context - child components (MeetingInfo) read from this
        let meeting_time = MeetingTime {
            call_start_time: self.call_start_time,
            meeting_start_time: self.meeting_start_time_server,
        };

        html! {
            <ContextProvider<MeetingTimeCtx> context={meeting_time}>
            <ContextProvider<VideoCallClientCtx> context={self.client.clone()}>
                <div id="main-container" class="meeting-page">
                    <BrowserCompatibility/>
                     {top_right_controls.clone()}
                <div id="grid-container"
                    class={grid_container_classes}
                    data-peers={num_peers_for_styling.to_string()}
                    style={container_style}>
                    { rows }

                    { // Invitation overlay when there are no connected peers
                        if num_display_peers == 0 {
                            html! {
                                <div id="invite-overlay" class="card-apple" style="position: fixed; top: 50%; left: 50%; transform: translate(-50%, -50%); width: 90%; max-width: 420px; z-index: 0; text-align: center;">
                                    <h4 style="margin-top:0;">{"Your meeting is ready!"}</h4>
                                    <p style="font-size: 0.9rem; opacity: 0.8;">{"Share this meeting link with others you want in the meeting"}</p>
                                    <div style="display:flex; align-items:center; margin-top: 0.75rem; margin-bottom: 0.75rem;">
                                        <input
                                            id="meeting-link-input"
                                            value={meeting_link.clone()}
                                            readonly=true
                                            class="input-apple" style="flex:1; overflow:hidden; text-overflow: ellipsis;"/>
                                        <button
                                            class={classes!("btn-apple", "btn-primary", "btn-sm", "copy-button", self.show_copy_toast.then_some("btn-pop-animate"))}
                                            style="margin-left: 0.5rem;"
                                            onclick={copy_meeting_link}
                                        >
                                            {"Copy"}
                                            { if self.show_copy_toast {
                                                html!{
                                                    <div class="sparkles" aria-hidden="true">
                                                        <span class="sparkle"></span>
                                                        <span class="sparkle"></span>
                                                        <span class="sparkle"></span>
                                                        <span class="sparkle"></span>
                                                        <span class="sparkle"></span>
                                                        <span class="sparkle"></span>
                                                        <span class="sparkle"></span>
                                                        <span class="sparkle"></span>
                                                    </div>
                                                }
                                            } else { html!{} } }
                                        </button>
                                    </div>
                                    <p style="font-size: 0.8rem; opacity: 0.7;">{"People who use this meeting link must get your permission before they can join."}</p>
                                    <div
                                        class={classes!("copy-toast", self.show_copy_toast.then_some("copy-toast--visible"))}
                                        role="alert"
                                        aria-live="assertive"
                                        aria-hidden={( !self.show_copy_toast ).to_string()}
                                    >
                                        {"Link copied to clipboard"}
                                    </div>
                                </div>
                            }
                        } else { html!{} }
                    }

                    {
                        if users_allowed_to_stream().unwrap_or_default().iter().any(|host| host == &email) || users_allowed_to_stream().unwrap_or_default().is_empty() {
                            html! {
                                <nav class="host">
                                    <div class="controls">
                                        <nav class="video-controls-container">
                                            <MicButton
                                                enabled={self.mic_enabled}
                                                onclick={ctx.link().callback(|_| MeetingAction::ToggleMicMute)}
                                            />
                                            <CameraButton
                                                enabled={self.video_enabled}
                                                onclick={ctx.link().callback(|_| MeetingAction::ToggleVideoOnOff)}
                                            />
                                            // Hide screen share button on Safari/iOS devices
                                            {
                                                if !is_ios() {
                                                    html! {
                                                        <ScreenShareButton
                                                            active={self.share_screen}
                                                            onclick={ctx.link().callback(|_| MeetingAction::ToggleScreenShare)}
                                                        />
                                                    }
                                                } else {
                                                    html! {}
                                                }
                                            }
                                            <PeerListButton
                                                open={self.peer_list_open}
                                                onclick={toggle_peer_list.clone()}
                                            />
                                            <DiagnosticsButton
                                                open={self.diagnostics_open}
                                                onclick={toggle_diagnostics.clone()}
                                            />
                                            <DeviceSettingsButton
                                                open={self.device_settings_open}
                                                onclick={ctx.link().callback(|_| UserScreenToggleAction::DeviceSettings)}
                                            />
                                            <HangUpButton
                                                onclick={ctx.link().callback(|_| Msg::HangUp)}
                                            />
                                            { self.view_grid_toggle(ctx) }
                                            { self.view_fake_peer_buttons(ctx, add_fake_peer_disabled) }
                                        </nav>
                                        { html!{} }
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
                                        if let Some(err) = &self.mic_error {
                                            let displayed: String = err.chars().take(200).collect();
                                            html!{
                                                <div class="glass-backdrop">
                                                    <div class="card-apple" style="width: 380px;">
                                                        <h4 style="margin-top:0;">{"Microphone issue"}</h4>
                                                        <p style="color:#AEAEB2; margin-top:0.25rem;">{"We couldn't start your microphone."}</p>
                                                        <p style="margin-top:0.5rem;">{ displayed }</p>
                                                        <div style="display:flex; gap:8px; justify-content:flex-end; margin-top:12px;">
                                                            <button class="btn-apple btn-secondary btn-sm" onclick={ctx.link().callback(|_| Msg::DismissMicError)}>{"Close"}</button>
                                                            <button class="btn-apple btn-primary btn-sm" onclick={ctx.link().callback(|_| MeetingAction::ToggleMicMute)}>{"Retry"}</button>
                                                        </div>
                                                    </div>
                                                </div>
                                            }
                                        } else { html!{} }
                                    }
                                    {
                                         if media_access_granted {
                                             html! {<Host
                                                 share_screen={self.share_screen}
                                                 mic_enabled={self.mic_enabled}
                                                 video_enabled={self.video_enabled}
                                                 on_encoder_settings_update={on_encoder_settings_update}
                                                 device_settings_open={self.device_settings_open}
                                                 on_device_settings_toggle={ctx.link().callback(|_| UserScreenToggleAction::DeviceSettings)}
                                                 on_microphone_error={ctx.link().callback(Msg::OnMicrophoneError)}
                                             />}
                                         } else {
                                             html! {<></>}
                                         }
                                    }
                                    <div class={classes!("connection-led", if self.client.is_connected() { "connected" } else { "connecting" })} title={if self.client.is_connected() { "Connected" } else { "Connecting" }}></div>

                                </nav>
                            }
                        } else {
                            error!("User not allowed to stream");
                            let allowed = users_allowed_to_stream().unwrap_or_default();
                            error!("allowed users {}", allowed.join(", "));
                            html! {}
                        }
                    }
                </div>
                <div id="peer-list-container" class={if self.peer_list_open {"visible"} else {""}}>
                    {
                        if self.peer_list_open {
                            let toggle_meeting_info = ctx.link().callback(|_| UserScreenToggleAction::MeetingInfo);
                            html! {
                                <PeerList
                                    peers={display_peers_vec.clone()}
                                    onclose={toggle_peer_list}
                                    show_meeting_info={self.meeting_info_open}
                                    room_id={ctx.props().id.clone()}
                                    num_participants={num_display_peers}
                                    is_active={self.meeting_joined && self.meeting_ended_message.is_none()}
                                    on_toggle_meeting_info={toggle_meeting_info}
                                />
                            }
                        } else {
                                html! {}
                        }
                    }
                </div>

                {
                    if let Some(ref message) = self.meeting_ended_message {
                        html! {
                            <div class="glass-backdrop" style="z-index: 9999;">
                                <div class="card-apple" style="width: 420px; text-align: center;">
                                    <svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 24 24" fill="none" stroke="#ff6b6b" stroke-width="2" style="margin: 0 auto 1rem;">
                                        <circle cx="12" cy="12" r="10"></circle>
                                        <line x1="15" y1="9" x2="9" y2="15"></line>
                                        <line x1="9" y1="9" x2="15" y2="15"></line>
                                    </svg>
                                    <h4 style="margin-top:0; margin-bottom: 0.5rem;">{"Meeting Ended"}</h4>
                                    <p style="font-size: 1rem; margin: 1.5rem 0; color: #666;">
                                        {message}
                                    </p>
                                    <button
                                        class="btn-apple btn-primary"
                                        onclick={Callback::from(|_| {
                                            if let Some(window) = web_sys::window() {
                                                let _ = window.location().set_href("/");
                                            }
                                        })}>
                                        {"Return to Home"}
                                    </button>
                                </div>
                            </div>
                        }
                    } else {
                        html! {}
                    }
                }

                {
                    if self.diagnostics_open {
                        html!{
                            <Diagnostics
                                is_open={true}
                                on_close={close_diagnostics}
                                video_enabled={self.video_enabled}
                                mic_enabled={self.mic_enabled}
                                share_screen={self.share_screen}
                            />
                        }
                    } else { html!{} }
                }
                </div>
            </ContextProvider<VideoCallClientCtx>>
            </ContextProvider<MeetingTimeCtx>>
        }
    }
}

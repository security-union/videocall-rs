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
    browser_compatibility::BrowserCompatibility, canvas_generator, peer_list::PeerList,
};
use crate::constants::{CANVAS_LIMIT, USERS_ALLOWED_TO_STREAM, WEBTRANSPORT_HOST};
use crate::{components::host::Host, constants::ACTIX_WEBSOCKET};
use gloo_utils::window;
use log::{debug, error, warn};
use videocall_client::utils::is_ios;
use videocall_client::{MediaDeviceAccess, VideoCallClient, VideoCallClientOptions};
use videocall_diagnostics::{subscribe, MetricValue};
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
pub enum UserScreenToggleAction {
    PeerList,
    Diagnostics,
    DeviceSettings,
}

#[derive(Debug)]
pub enum Msg {
    WsAction(WsAction),
    MeetingAction(MeetingAction),
    OnPeerAdded(String),
    OnFirstFrame((String, MediaType)),
    UserScreenAction(UserScreenToggleAction),
    #[cfg(feature = "fake-peers")]
    AddFakePeer,
    #[cfg(feature = "fake-peers")]
    RemoveLastFakePeer,
    #[cfg(feature = "fake-peers")]
    ToggleForceDesktopGrid,
    NetEqStatsUpdated(String),
    NetEqBufferUpdated(u64),
    NetEqJitterUpdated(u64),
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
    pub diagnostics_data: Option<String>,
    pub sender_stats: Option<String>,
    pub encoder_settings: Option<String>,
    pub neteq_stats: Option<String>,
    pub neteq_buffer_history: Vec<u64>,
    pub neteq_jitter_history: Vec<u64>,
    pending_mic_enable: bool,
    pending_video_enable: bool,
    pending_screen_share: bool,
    pub meeting_joined: bool,
    fake_peer_ids: Vec<String>,
    #[cfg(feature = "fake-peers")]
    next_fake_peer_id_counter: usize,
    force_desktop_grid_on_mobile: bool,
    simulation_info_message: Option<String>,
}

impl AttendantsComponent {
    fn render_buffer_chart(&self) -> Html {
        let max = *self.neteq_buffer_history.iter().max().unwrap_or(&1) as f64;
        let points: String = self
            .neteq_buffer_history
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let x = 25.0 + (i as f64 / 49.0 * 110.0);
                let y = 55.0 - (*v as f64 / if max == 0.0 { 1.0 } else { max } * 50.0);
                format!("{:.1},{:.1}", x, y)
            })
            .collect::<Vec<_>>()
            .join(" ");

        let max_val = *self.neteq_buffer_history.iter().max().unwrap_or(&0);
        let span = self.neteq_buffer_history.len().saturating_sub(1);

        html! {
            <svg width="140" height="60" viewBox="0 0 140 60" preserveAspectRatio="none">
                <line x1="25" y1="5" x2="25" y2="55" stroke="#666" stroke-width="1" />
                <line x1="25" y1="55" x2="135" y2="55" stroke="#666" stroke-width="1" />
                <polyline points={points} fill="none" stroke="#8ef" stroke-width="2" />
                <text x="0" y="10" fill="#888" font-size="8">{ max_val }</text>
                <text x="0" y="55" fill="#888" font-size="8">{"0"}</text>
                <text x="25" y="59" fill="#888" font-size="8">{"0s"}</text>
                <text x="115" y="59" fill="#888" font-size="8">{ format!("{}s", span) }</text>
            </svg>
        }
    }

    fn render_jitter_chart(&self) -> Html {
        let max = *self.neteq_jitter_history.iter().max().unwrap_or(&1) as f64;
        let points: String = self
            .neteq_jitter_history
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let x = 25.0 + (i as f64 / 49.0 * 110.0);
                let y = 55.0 - (*v as f64 / if max == 0.0 { 1.0 } else { max } * 50.0);
                format!("{:.1},{:.1}", x, y)
            })
            .collect::<Vec<_>>()
            .join(" ");

        html! {
            <svg width="140" height="60" viewBox="0 0 140 60" preserveAspectRatio="none">
                <line x1="25" y1="5" x2="25" y2="55" stroke="#666" stroke-width="1" />
                <line x1="25" y1="55" x2="135" y2="55" stroke="#666" stroke-width="1" />
                <polyline points={points} fill="none" stroke="#ff8" stroke-width="2" />
            </svg>
        }
    }

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
        let client = Self::create_video_call_client(ctx);
        let media_device_access = Self::create_media_device_access(ctx);
        let self_ = Self {
            client,
            media_device_access,
            share_screen: false,
            mic_enabled: false,
            video_enabled: false,
            peer_list_open: false,
            diagnostics_open: false,
            device_settings_open: false,
            error: None,
            diagnostics_data: None,
            sender_stats: None,
            encoder_settings: None,
            neteq_stats: None,
            neteq_buffer_history: Vec::new(),
            neteq_jitter_history: Vec::new(),
            pending_mic_enable: false,
            pending_video_enable: false,
            pending_screen_share: false,
            meeting_joined: false,
            fake_peer_ids: Vec::new(),
            #[cfg(feature = "fake-peers")]
            next_fake_peer_id_counter: 1,
            force_desktop_grid_on_mobile: true,
            simulation_info_message: None,
        };
        {
            let link = ctx.link().clone();
            wasm_bindgen_futures::spawn_local(async move {
                let rx = subscribe();
                while let Ok(evt) = rx.recv_async().await {
                    if evt.subsystem == "neteq" {
                        for m in &evt.metrics {
                            if m.name == "stats_json" {
                                if let MetricValue::Text(json) = &m.value {
                                    link.send_message(Msg::NetEqStatsUpdated(json.clone()));
                                }
                            } else if m.name == "current_buffer_size_ms" {
                                if let MetricValue::U64(v) = &m.value {
                                    link.send_message(Msg::NetEqBufferUpdated(*v as u64));
                                }
                            } else if m.name == "jitter_buffer_delay_ms" {
                                if let MetricValue::U64(v) = &m.value {
                                    link.send_message(Msg::NetEqJitterUpdated(*v as u64));
                                }
                            }
                        }
                    }
                }
            });
        }
        self_
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
            Msg::NetEqStatsUpdated(s) => {
                self.neteq_stats = Some(s);
                true
            }
            Msg::NetEqBufferUpdated(v) => {
                self.neteq_buffer_history.push(v);
                if self.neteq_buffer_history.len() > 50 {
                    self.neteq_buffer_history.remove(0);
                } // keep last 50
                false
            }
            Msg::NetEqJitterUpdated(v) => {
                self.neteq_jitter_history.push(v);
                if self.neteq_jitter_history.len() > 50 {
                    self.neteq_jitter_history.remove(0);
                }
                false
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let email = ctx.props().email.clone();
        let media_access_granted = self.media_device_access.is_granted();

        let toggle_peer_list = ctx.link().callback(|_| UserScreenToggleAction::PeerList);
        let toggle_diagnostics = ctx.link().callback(|_| UserScreenToggleAction::Diagnostics);

        let real_peers_vec = self.client.sorted_peer_keys();
        let mut display_peers_vec = real_peers_vec.clone();
        display_peers_vec.extend(self.fake_peer_ids.iter().cloned());

        let num_display_peers = display_peers_vec.len();
        // Cap the number of peers used for styling at CANVAS_LIMIT
        let num_peers_for_styling = num_display_peers.min(CANVAS_LIMIT);

        // Determine if the "Add Fake Peer" button should be disabled
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

        // Compute meeting link for invitation overlay
        let meeting_link = {
            let origin_result = window().location().origin();
            // If obtaining origin fails, fallback to empty string
            let origin = origin_result.unwrap_or_else(|_| "".to_string());
            format!("{}/meeting/{}", origin, ctx.props().id)
        };

        // Callback to copy the meeting link to clipboard
        let copy_meeting_link = {
            let meeting_link = meeting_link.clone();
            Callback::from(move |_| {
                if let Some(clipboard) = web_sys::window().map(|w| w.navigator().clipboard()) {
                    // Try to write text; ignore potential JS promise errors for now
                    let _ = clipboard.write_text(&meeting_link);
                }
            })
        };

        let mut grid_container_classes = classes!();
        if self.force_desktop_grid_on_mobile {
            grid_container_classes.push("force-desktop-grid");
        }

        // Show Join Meeting button if user hasn't joined yet
        if !self.meeting_joined {
            return html! {
                <div id="main-container" class="meeting-page">
                    <BrowserCompatibility/>
                    <div id="join-meeting-container" style="position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #1a1a1a; z-index: 1000;">
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
                            onclick={ctx.link().callback(|_| WsAction::RequestMediaPermissions)}
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

                    { // Invitation overlay when there are no connected peers
                        if num_display_peers == 0 {
                            html! {
                                <div id="invite-overlay" style="position: fixed; top: 50%; left: 50%; transform: translate(-50%, -50%); background: rgba(0,0,0,0.9); padding: 1.5rem 2rem; border-radius: 8px; width: 90%; max-width: 420px; z-index: 3000; color: white; box-shadow: 0 4px 12px rgba(0,0,0,0.3); text-align: center;">
                                    <h3 style="margin-top:0;">{"Your meeting's ready"}</h3>
                                    <p style="font-size: 0.9rem; opacity: 0.8;">{"Share this meeting link with others you want in the meeting"}</p>
                                    <div style="display:flex; align-items:center; margin-top: 0.75rem; margin-bottom: 0.75rem;">
                                        <input
                                            id="meeting-link-input"
                                            value={meeting_link.clone()}
                                            readonly=true
                                            style="flex:1; padding: 0.5rem; border: none; border-radius: 4px; background: #333; color: white; font-size: 0.9rem; overflow:hidden; text-overflow: ellipsis;"/>
                                        <button
                                            class="copy-link-button"
                                            style="margin-left: 0.5rem; background: #4CAF50; color: white; border: none; padding: 0.5rem 0.75rem; border-radius: 4px; cursor: pointer;"
                                            onclick={copy_meeting_link}
                                        >
                                            {"Copy"}
                                        </button>
                                    </div>
                                    <p style="font-size: 0.8rem; opacity: 0.7;">{"People who use this meeting link must get your permission before they can join."}</p>
                                </div>
                            }
                        } else { html!{} }
                    }

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

                                            // Hide screen share button on Safari/iOS devices
                                            {
                                                if !is_ios() {
                                                    html! {
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
                                                    }
                                                } else {
                                                    html! {}
                                                }
                                            }
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
                                            <button
                                                class={classes!("video-control-button", "mobile-only-device-settings", self.device_settings_open.then_some("active"))}
                                                onclick={ctx.link().callback(|_| UserScreenToggleAction::DeviceSettings)}>
                                                {
                                                    if self.device_settings_open {
                                                        html! {
                                                            <>
                                                                <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                                    <circle cx="12" cy="12" r="3"></circle>
                                                                    <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06-.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1 1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l.06-.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"></path>
                                                                </svg>
                                                                <span class="tooltip">{ "Close Settings" }</span>
                                                            </>
                                                        }
                                                    } else {
                                                        html! {
                                                            <>
                                                                <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                                    <circle cx="12" cy="12" r="3"></circle>
                                                                    <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06-.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1 1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l.06-.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"></path>
                                                                </svg>
                                                                <span class="tooltip">{ "Device Settings" }</span>
                                                            </>
                                                        }
                                                    }
                                                }
                                            </button>
                                            { self.view_grid_toggle(ctx) }
                                            { self.view_fake_peer_buttons(ctx, add_fake_peer_disabled) }

                                        </nav>
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
                                            html! {<Host
                                                client={self.client.clone()}
                                                share_screen={self.share_screen}
                                                mic_enabled={self.mic_enabled}
                                                video_enabled={self.video_enabled}
                                                on_encoder_settings_update={on_encoder_settings_update}
                                                device_settings_open={self.device_settings_open}
                                                on_device_settings_toggle={ctx.link().callback(|_| UserScreenToggleAction::DeviceSettings)}
                                            />}
                                        } else {
                                            html! {<></>}
                                        }
                                    }
                                    <h4 class="floating-name">{email}</h4>

                                    <div class={classes!("connection-led", if self.client.is_connected() { "connected" } else { "connecting" })} title={if self.client.is_connected() { "Connected" } else { "Connecting" }}></div>

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
                                {
                                    if let Some(data) = &self.diagnostics_data {
                                        html! { <pre>{ data }</pre> }
                                    } else {
                                        html! { <p>{"No reception data available."}</p> }
                                    }
                                }
                            </div>
                            <div class="diagnostics-section">
                                <h3>{"Sending Stats"}</h3>
                                {
                                    if let Some(data) = &self.sender_stats {
                                        html! { <pre>{ data }</pre> }
                                    } else {
                                        html! { <p>{"No sending data available."}</p> }
                                    }
                                }
                            </div>
                            <div class="diagnostics-section">
                                <h3>{"Encoder Settings"}</h3>
                                {
                                    if let Some(data) = &self.encoder_settings {
                                        html! { <pre>{ data }</pre> }
                                    } else {
                                        html! { <p>{"No encoder settings available."}</p> }
                                    }
                                }
                            </div>
                            <div class="diagnostics-section">
                                <h3>{"NetEQ Stats"}</h3>
                                {
                                    if let Some(data) = &self.neteq_stats {
                                        html! { <pre>{ data }</pre> }
                                    } else {
                                        html! { <p>{"No NetEQ stats available."}</p> }
                                    }
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
                            <div class="diagnostics-section">
                                <h3>{"NetEQ Buffer / Jitter History"}</h3>
                                <div style="display:flex; gap:12px; align-items:center;">
                                    { self.render_buffer_chart() }
                                    { self.render_jitter_chart() }
                                </div>
                            </div>
                        </div>
                    </div>
                </div>
            </div>
        }
    }
}

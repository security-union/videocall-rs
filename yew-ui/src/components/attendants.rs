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
    host_controls::HostControls,
    meeting_ended_overlay::MeetingEndedOverlay,
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
use std::collections::HashMap;
use videocall_client::Callback as VcCallback;
use videocall_client::{
    utils::is_ios, MediaAccessKind, MediaPermission, MediaPermissionsErrorState, PermissionState,
};
use videocall_client::{
    MediaDeviceAccess, ScreenShareEvent, VideoCallClient, VideoCallClientOptions,
};
use videocall_types::protos::media_packet::media_packet::MediaType;
use wasm_bindgen::{prelude::*, JsCast, JsValue};
use web_sys::Event;
use web_sys::*;
use yew::prelude::*;
use yew::{html, Component, Context, Html};

#[derive(Clone, Debug, PartialEq)]
pub enum ScreenShareState {
    /// No screen share in progress.
    Idle,
    /// User clicked the button; browser picker is open, awaiting selection.
    Requesting,
    /// A screen is actively being shared and encoded.
    Active,
}

pub enum MediaErrorState {
    NoDevice,
    PermissionDenied,
    Other,
}

impl ScreenShareState {
    /// Returns `true` when the encoder should be running (Requesting or Active).
    /// Use this to derive the boolean prop that `Host` needs.
    pub fn is_sharing(&self) -> bool {
        !matches!(self, ScreenShareState::Idle)
    }
}

#[derive(Debug)]
pub enum WsAction {
    Connect,
    Connected,
    Lost(Option<JsValue>),
    RequestMediaPermissions,
    MediaPermissionsUpdated(MediaPermission),
    WindowFocused,
    ReloadDevices,
    Log(String),
    EncoderSettingsUpdated(String),
    MeetingInfoReceived(u64),
    CloseDeviceWarning,
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
    OnSpeakingChanged(bool),
    OnAudioLevelChanged(f32),
    OnMicrophoneError(String),
    OnCameraError(String),
    DismissUserError,
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
    ScreenShareStateChange(ScreenShareEvent),
    /// A fresh room access token was obtained from the meeting API.
    TokenRefreshed(String),
    /// Token refresh failed (session expired, kicked from meeting, network error).
    TokenRefreshFailed(String),
    /// The waiting room participant list changed (push notification from server).
    WaitingRoomUpdated,
    /// A remote participant left the meeting. Carries (display_name, user_id).
    OnPeerLeft((String, String)),
    /// A remote participant joined the meeting. Carries (display_name, user_id).
    OnPeerJoined((String, String)),
    /// Remove an expired toast by its monotonic ID.
    RemovePeerToast(u64),
    /// Play the leave sound only if the toast still exists (not cancelled by a join).
    PlayLeftSoundIfStillActive(u64),
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
    pub display_name: String,

    pub e2ee_enabled: bool,

    pub webtransport_enabled: bool,

    #[prop_or_default]
    pub user_name: Option<String>,

    #[prop_or_default]
    pub user_id: Option<String>,

    #[prop_or_default]
    pub on_logout: Option<Callback<()>>,

    /// Display name (username) of the meeting host/owner (for displaying crown icon)
    #[prop_or_default]
    pub host_display_name: Option<String>,

    /// Authenticated user_id of the meeting host (for host identity comparison).
    /// Compared against each peer's user_id to prevent display-name spoofing.
    #[prop_or_default]
    pub host_user_id: Option<String>,

    /// If true, automatically join the meeting without showing the "Join Meeting" button.
    /// Used when user was admitted from the waiting room.
    #[prop_or_default]
    pub auto_join: bool,

    /// If true, the current user is the owner of the meeting.
    #[prop_or_default]
    pub is_owner: bool,

    /// Signed JWT room access token for connecting to the media server.
    /// Obtained from the meeting API when the participant is admitted.
    #[prop_or_default]
    pub room_token: String,
}

pub struct AttendantsComponent {
    pub client: VideoCallClient,
    pub media_device_access: MediaDeviceAccess,
    pub screen_share_state: ScreenShareState,
    pub mic_enabled: bool,
    pub mic_error: Option<MediaErrorState>,
    pub video_enabled: bool,
    pub video_error: Option<MediaErrorState>,
    pub peer_list_open: bool,
    pub diagnostics_open: bool,
    pub device_settings_open: bool,
    pub error: Option<String>,
    pub encoder_settings: Option<String>,
    /// Generic user-visible error message shown in a dialog
    pub user_error: Option<String>,
    pending_mic_enable: bool,
    pending_video_enable: bool,
    pub meeting_joined: bool,
    fake_peer_ids: Vec<String>,
    #[cfg(feature = "fake-peers")]
    next_fake_peer_id_counter: usize,
    force_desktop_grid_on_mobile: bool,
    simulation_info_message: Option<String>,
    show_copy_toast: bool,
    pub meeting_start_time_server: Option<f64>,
    pub call_start_time: Option<f64>,
    meeting_ended_message: Option<String>,
    meeting_info_open: bool,
    local_speaking: bool,
    local_audio_level: f32,
    /// Monotonically increasing counter bumped when a `WaitingRoomUpdated`
    /// push notification arrives. Passed as a prop to `HostControls` so it
    /// knows to re-fetch the waiting list.
    waiting_room_version: u32,
    /// Tracks the current reconnection attempt number for exponential backoff.
    /// Only meaningful in the JWT auth path (`media-server-jwt-auth` feature);
    /// the non-JWT path manages its own attempt counter inside the recursive
    /// `schedule_reconnect_no_jwt` closure chain.
    reconnect_attempt: u32,
    /// Active "participant join/leave" toast messages: (toast_id, display_name, user_id, is_joined).
    peer_toasts: Vec<(u64, String, String, bool)>,
    /// Monotonic counter for toast IDs.
    toast_counter: u64,
    show_device_warning: bool,
    reload_devices_counter: u32,
    device_was_denied: bool,
    session_loaded: bool,
}

impl AttendantsComponent {
    /// Build the WebSocket and WebTransport lobby URLs for the media server.
    ///
    /// When `media-server-jwt-auth` is enabled, the token is embedded as a query
    /// parameter. When disabled, the legacy `/{user_id}/{room}` path is used.
    #[allow(unused_variables)]
    fn build_lobby_urls(token: &str, user_id: &str, id: &str) -> (Vec<String>, Vec<String>) {
        #[cfg(feature = "media-server-jwt-auth")]
        let lobby_url = |base: &str| format!("{base}/lobby?token={token}");

        #[cfg(not(feature = "media-server-jwt-auth"))]
        let lobby_url = |base: &str| format!("{base}/lobby/{user_id}/{id}");

        let websocket_urls = actix_websocket_base()
            .unwrap_or_default()
            .split(',')
            .map(lobby_url)
            .collect::<Vec<String>>();
        let webtransport_urls = webtransport_host_base()
            .unwrap_or_default()
            .split(',')
            .map(lobby_url)
            .collect::<Vec<String>>();

        (websocket_urls, webtransport_urls)
    }

    fn create_video_call_client(ctx: &Context<Self>) -> VideoCallClient {
        let display_name = ctx.props().display_name.clone();
        let id = ctx.props().id.clone();

        #[cfg(feature = "media-server-jwt-auth")]
        let token = {
            let t = ctx.props().room_token.clone();
            assert!(
                !t.is_empty(),
                "media-server-jwt-auth is enabled but room_token is empty — \
                 cannot connect to the media server without a signed JWT"
            );
            t
        };

        #[cfg(not(feature = "media-server-jwt-auth"))]
        let token = String::new();

        let (websocket_urls, webtransport_urls) =
            Self::build_lobby_urls(&token, &display_name, &id);

        log::info!(
            "YEW-UI: Creating VideoCallClient for {} in meeting {} with webtransport_enabled={}, jwt_auth={}",
            display_name,
            id,
            ctx.props().webtransport_enabled,
            cfg!(feature = "media-server-jwt-auth"),
        );
        if websocket_urls.is_empty() || webtransport_urls.is_empty() {
            log::error!("Runtime config missing or invalid: wsUrl or webTransportHost not set");
        }
        log::info!("YEW-UI: WebSocket URLs: {websocket_urls:?}");
        log::info!("YEW-UI: WebTransport URLs: {webtransport_urls:?}");

        let opts = VideoCallClientOptions {
            user_id: ctx
                .props()
                .user_id
                .clone()
                .unwrap_or_else(|| display_name.clone()),
            meeting_id: id.clone(),
            websocket_urls,
            webtransport_urls,
            enable_e2ee: ctx.props().e2ee_enabled,
            enable_webtransport: ctx.props().webtransport_enabled,
            on_connected: {
                let link = ctx.link().clone();
                let webtransport_enabled = ctx.props().webtransport_enabled;
                VcCallback::from(move |_| {
                    log::info!(
                        "YEW-UI: Connection established (webtransport_enabled={webtransport_enabled})",
                    );
                    link.send_message(Msg::from(WsAction::Connected))
                })
            },
            on_connection_lost: {
                let link = ctx.link().clone();
                let webtransport_enabled = ctx.props().webtransport_enabled;
                VcCallback::from(move |_| {
                    log::warn!(
                        "YEW-UI: Connection lost (webtransport_enabled={webtransport_enabled})",
                    );
                    link.send_message(Msg::from(WsAction::Lost(None)))
                })
            },
            on_peer_added: {
                let link = ctx.link().clone();
                VcCallback::from(move |peer_id| link.send_message(Msg::OnPeerAdded(peer_id)))
            },
            on_peer_first_frame: {
                let link = ctx.link().clone();
                VcCallback::from(move |(peer_id, media_type)| {
                    link.send_message(Msg::OnFirstFrame((peer_id, media_type)))
                })
            },
            on_peer_removed: Some({
                let link = ctx.link().clone();
                VcCallback::from(move |peer_id: String| {
                    log::info!("Peer removed: {peer_id}");
                    link.send_message(Msg::OnPeerRemoved(peer_id));
                })
            }),
            get_peer_video_canvas_id: VcCallback::from(|session_id| session_id),
            get_peer_screen_canvas_id: VcCallback::from(|session_id| {
                format!("screen-share-{}", &session_id)
            }),
            enable_diagnostics: true,
            diagnostics_update_interval_ms: Some(1000),
            enable_health_reporting: true,
            health_reporting_interval_ms: Some(5000),
            on_encoder_settings_update: Some({
                let link = ctx.link().clone();
                VcCallback::from(move |settings| {
                    link.send_message(Msg::from(WsAction::EncoderSettingsUpdated(settings)))
                })
            }),
            rtt_testing_period_ms: server_election_period_ms().unwrap_or(2000),
            rtt_probe_interval_ms: Some(200),
            on_meeting_info: Some({
                let link = ctx.link().clone();
                VcCallback::from(move |start_time_ms: f64| {
                    log::info!("Meeting started at Unix timestamp: {start_time_ms}");
                    link.send_message(Msg::WsAction(WsAction::MeetingInfoReceived(
                        start_time_ms as u64,
                    )))
                })
            }),
            on_meeting_ended: Some({
                let link = ctx.link().clone();
                VcCallback::from(move |(end_time_ms, message): (f64, String)| {
                    log::info!("Meeting ended at Unix timestamp: {end_time_ms}");
                    link.send_message(Msg::WsAction(WsAction::MeetingInfoReceived(
                        end_time_ms as u64,
                    )));
                    link.send_message(Msg::MeetingEnded(message));
                })
            }),
            on_speaking_changed: Some({
                let link = ctx.link().clone();
                VcCallback::from(move |speaking: bool| {
                    link.send_message(Msg::OnSpeakingChanged(speaking));
                })
            }),
            on_audio_level_changed: Some({
                let link = ctx.link().clone();
                VcCallback::from(move |level: f32| {
                    link.send_message(Msg::OnAudioLevelChanged(level));
                })
            }),
            vad_threshold: crate::constants::vad_threshold().ok(),
            on_meeting_activated: None,
            on_participant_admitted: None,
            on_participant_rejected: None,
            on_waiting_room_updated: Some({
                let link = ctx.link().clone();
                VcCallback::from(move |_| {
                    log::info!("Waiting room updated via push notification");
                    link.send_message(Msg::WaitingRoomUpdated);
                })
            }),
            on_peer_left: Some({
                let link = ctx.link().clone();
                VcCallback::from(move |pair: (String, String)| {
                    link.send_message(Msg::OnPeerLeft(pair));
                })
            }),
            on_peer_joined: Some({
                let link = ctx.link().clone();
                VcCallback::from(move |pair: (String, String)| {
                    link.send_message(Msg::OnPeerJoined(pair));
                })
            }),
            on_display_name_changed: None,
        };

        VideoCallClient::new(opts)
    }

    fn create_media_device_access(ctx: &Context<Self>) -> MediaDeviceAccess {
        let mut media_device_access = MediaDeviceAccess::new();
        let link = ctx.link().clone();
        media_device_access.on_result = {
            VcCallback::from(move |permission: MediaPermission| {
                link.send_message(WsAction::MediaPermissionsUpdated(permission))
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
        html! {}
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
        html! {}
    }

    /// Maximum number of reconnection attempts before giving up.
    const MAX_RECONNECT_ATTEMPTS: u32 = 10;

    /// Compute the reconnect delay for the given attempt using exponential
    /// backoff with ±25% random jitter.  Returns `None` when all attempts are
    /// exhausted.
    fn reconnect_delay_ms(attempt: u32) -> Option<u32> {
        const MAX_DELAY_MS: u32 = 16_000;
        if attempt >= Self::MAX_RECONNECT_ATTEMPTS {
            return None;
        }
        let base = (1000u32.saturating_mul(2u32.saturating_pow(attempt))).min(MAX_DELAY_MS);
        let jitter = (js_sys::Math::random() * 0.5 - 0.25) * base as f64;
        Some((base as f64 + jitter).max(500.0) as u32)
    }

    /// Schedule a token refresh attempt with exponential backoff and jitter.
    ///
    /// Retries with increasing delay (1s -> 2s -> 4s -> 8s -> 16s cap) plus +/-25%
    /// random jitter.  Gives up after 10 attempts.  If the meeting has ended
    /// (`MeetingNotActive`), sends `MeetingEnded` instead of retrying.
    #[cfg(feature = "media-server-jwt-auth")]
    fn schedule_token_refresh(link: yew::html::Scope<Self>, meeting_id: String, attempt: u32) {
        let delay_ms = match Self::reconnect_delay_ms(attempt) {
            Some(d) => d,
            None => {
                link.send_message(Msg::TokenRefreshFailed(
                    "Unable to reconnect after multiple attempts. Please refresh the page.".into(),
                ));
                return;
            }
        };

        log::info!(
            "Scheduling token refresh attempt {}/{} in {}ms",
            attempt + 1,
            Self::MAX_RECONNECT_ATTEMPTS,
            delay_ms
        );

        Timeout::new(delay_ms, move || {
            wasm_bindgen_futures::spawn_local(async move {
                match crate::meeting_api::refresh_room_token(&meeting_id).await {
                    Ok(token) => link.send_message(Msg::TokenRefreshed(token)),
                    Err(crate::meeting_api::JoinError::MeetingNotActive) => {
                        link.send_message(Msg::MeetingEnded("The meeting has ended.".to_string()));
                    }
                    Err(e) => {
                        link.send_message(Msg::TokenRefreshFailed(e.to_string()));
                    }
                }
            });
        })
        .forget();
    }

    /// Schedule a reconnection attempt with exponential backoff (non-JWT path).
    ///
    /// Retries with increasing delay (1s -> 2s -> 4s -> 8s -> 16s cap) plus +/-25%
    /// random jitter.  Gives up after 10 attempts.
    #[cfg(not(feature = "media-server-jwt-auth"))]
    fn schedule_reconnect_no_jwt(link: yew::html::Scope<Self>, attempt: u32) {
        let delay_ms = match Self::reconnect_delay_ms(attempt) {
            Some(d) => d,
            None => {
                link.send_message(Msg::TokenRefreshFailed(
                    "Unable to reconnect after multiple attempts. Please refresh the page.".into(),
                ));
                return;
            }
        };

        log::info!(
            "Scheduling reconnect attempt {}/{} in {}ms",
            attempt + 1,
            Self::MAX_RECONNECT_ATTEMPTS,
            delay_ms
        );

        Timeout::new(delay_ms, move || {
            link.send_message(WsAction::Connect);
        })
        .forget();
    }

    fn play_user_joined() {
        // Ascending two-tone chime: C5 -> E5 (pleasant, welcoming)
        Self::play_tone_pair(523.25, 659.25, 0.12, 0.35);
    }

    fn play_user_left() {
        // Descending two-tone: E5 -> A4 (subtle, muted)
        Self::play_tone_pair(659.25, 440.0, 0.12, 0.25);
    }

    /// Play two short sine-wave tones in sequence using the Web Audio API.
    fn play_tone_pair(freq1: f64, freq2: f64, duration: f64, volume: f64) {
        let Some(_window) = web_sys::window() else {
            return;
        };
        let ctx = match web_sys::AudioContext::new() {
            Ok(ctx) => ctx,
            Err(_) => return,
        };
        let now = ctx.current_time();

        // First tone
        if let (Ok(osc), Ok(gain)) = (ctx.create_oscillator(), ctx.create_gain()) {
            let _ = osc.connect_with_audio_node(&gain);
            let _ = gain.connect_with_audio_node(&ctx.destination());
            osc.set_type(web_sys::OscillatorType::Triangle);
            let _ = osc.frequency().set_value_at_time(freq1 as f32, now);
            let _ = gain.gain().set_value_at_time(volume as f32, now);
            let _ = gain
                .gain()
                .exponential_ramp_to_value_at_time(0.01, now + duration);
            let _ = osc.start();
            let _ = osc.stop_with_when(now + duration);
        }

        // Second tone (starts after first ends)
        if let (Ok(osc), Ok(gain)) = (ctx.create_oscillator(), ctx.create_gain()) {
            let _ = osc.connect_with_audio_node(&gain);
            let _ = gain.connect_with_audio_node(&ctx.destination());
            osc.set_type(web_sys::OscillatorType::Triangle);
            let _ = osc
                .frequency()
                .set_value_at_time(freq2 as f32, now + duration);
            let _ = gain.gain().set_value_at_time(volume as f32, now + duration);
            let _ = gain
                .gain()
                .exponential_ramp_to_value_at_time(0.01, now + duration * 2.0);
            let _ = osc.start_with_when(now + duration);
            let _ = osc.stop_with_when(now + duration * 2.0);
        }

        // Close the AudioContext after playback completes to avoid resource leaks.
        let duration_ms = (duration * 2.0 * 1000.0) as u32 + 100;
        Timeout::new(duration_ms, move || {
            let _ = ctx.close();
        })
        .forget();
    }

    fn render_device_error(&self) -> Html {
        let mut messages = vec![];

        if let Some(err) = &self.mic_error {
            messages.push(self.render_single_error("Microphone", err));
        }

        if let Some(err) = &self.video_error {
            messages.push(self.render_single_error("Camera", err));
        }

        html! { for messages}
    }

    fn render_single_error(&self, device: &str, error: &MediaErrorState) -> Html {
        match error {
            MediaErrorState::NoDevice => html! {
                <p>{ format!(" {} not found on this device.",device)}</p>
            },
            MediaErrorState::Other => html! {
                <p>{ format!(" {} has an unexpected problem.",device)}</p>
            },
            MediaErrorState::PermissionDenied => html! {
                <>
                   <p>{ format!(" {} is blocked in your browser.",device)}</p>
                   <p style="front-size: 0.9rem; opacity: 0.8;">
                      {"Please click the lock icon in your browser's address bar and allow access if you want to use it."}
                   </p>
                </>
            },
        }
    }
}

impl Component for AttendantsComponent {
    type Message = Msg;
    type Properties = AttendantsComponentProps;

    fn create(ctx: &Context<Self>) -> Self {
        let link = ctx.link().clone();
        let client = Self::create_video_call_client(ctx);
        let media_device_access = Self::create_media_device_access(ctx);

        let window = web_sys::window().expect("no global window exist");
        let closure = Closure::wrap(Box::new(move |_event: Event| {
            link.send_message(WsAction::WindowFocused);
        }) as Box<dyn FnMut(_)>);

        window
            .add_event_listener_with_callback("focus", closure.as_ref().unchecked_ref())
            .expect("failed to add focus listener");

        closure.forget();

        let mut self_ = Self {
            client,
            media_device_access,
            screen_share_state: ScreenShareState::Idle,
            mic_enabled: false,
            mic_error: None,
            video_enabled: false,
            video_error: None,
            peer_list_open: false,
            diagnostics_open: false,
            device_settings_open: false,
            error: None,
            encoder_settings: None,
            user_error: None,
            pending_mic_enable: false,
            pending_video_enable: false,
            meeting_joined: false,
            fake_peer_ids: Vec::new(),
            #[cfg(feature = "fake-peers")]
            next_fake_peer_id_counter: 1,
            force_desktop_grid_on_mobile: true,
            simulation_info_message: None,
            show_copy_toast: false,
            call_start_time: None,
            meeting_start_time_server: None,
            meeting_ended_message: None,
            meeting_info_open: false,
            local_speaking: false,
            local_audio_level: 0.0,
            waiting_room_version: 0,
            reconnect_attempt: 0,
            peer_toasts: Vec::new(),
            toast_counter: 0,
            show_device_warning: false,
            reload_devices_counter: 0,
            device_was_denied: false,
            session_loaded: false,
        };
        if let Err(e) = crate::constants::app_config() {
            log::error!("{e:?}");
            self_.error = Some(e);
        }

        self_
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        if first_render && ctx.props().auto_join {
            // Auto-join: request media permissions which will trigger connection
            ctx.link().send_message(WsAction::RequestMediaPermissions);
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        log::debug!("YEW-UI: AttendantsComponent update: {msg:?}");
        match msg {
            Msg::OnSpeakingChanged(speaking) => {
                log::trace!("LOCAL Speaking state changed to: {}", speaking);
                self.local_speaking = speaking;
                true
            }
            Msg::OnAudioLevelChanged(level) => {
                self.local_audio_level = level;
                true
            }
            Msg::WsAction(action) => match action {
                WsAction::Connect => {
                    if self.client.is_connected() {
                        return false;
                    }

                    if let Err(e) = self.client.connect() {
                        ctx.link()
                            .send_message(WsAction::Log(format!("Connection failed: {e:?}")));
                    }
                    self.meeting_joined = true;
                    true
                }
                WsAction::Connected => {
                    log::info!("YEW-UI: Connection established successfully!");
                    self.error = None;
                    self.reconnect_attempt = 0;
                    self.call_start_time = Some(js_sys::Date::now());
                    self.session_loaded = true;
                    true
                }

                WsAction::Log(msg) => {
                    warn!("{msg}");
                    false
                }
                WsAction::Lost(reason) => {
                    warn!("Connection lost (reason: {reason:?})");
                    self.error = Some("Connection lost, reconnecting...".to_string());

                    #[cfg(feature = "media-server-jwt-auth")]
                    {
                        self.reconnect_attempt = 0;
                        let link = ctx.link().clone();
                        let meeting_id = ctx.props().id.clone();
                        Self::schedule_token_refresh(link, meeting_id, 0);
                    }

                    #[cfg(not(feature = "media-server-jwt-auth"))]
                    {
                        self.reconnect_attempt = 0;
                        let link = ctx.link().clone();
                        Self::schedule_reconnect_no_jwt(link, 0);
                    }

                    true
                }
                WsAction::RequestMediaPermissions => {
                    self.media_device_access.request();
                    false
                }
                WsAction::WindowFocused => {
                    if matches!(self.mic_error, Some(MediaErrorState::PermissionDenied))
                        || matches!(self.video_error, Some(MediaErrorState::PermissionDenied))
                    {
                        ctx.link().send_message(WsAction::ReloadDevices);
                    } else {
                        ctx.link().send_message(WsAction::RequestMediaPermissions);
                    }
                    false
                }
                WsAction::ReloadDevices => {
                    self.device_was_denied = true;
                    self.media_device_access.request();
                    true
                }
                WsAction::MediaPermissionsUpdated(permit) => {
                    self.error = None;
                    self.mic_error = None;
                    self.video_error = None;

                    if let PermissionState::Granted = &permit.audio {
                        if self.pending_mic_enable {
                            self.mic_enabled = true;
                            self.pending_mic_enable = false;
                        }
                    };

                    if let PermissionState::Granted = &permit.video {
                        if self.pending_video_enable {
                            self.video_enabled = true;
                            self.pending_video_enable = false;
                        }
                    }

                    if let PermissionState::Denied(MediaPermissionsErrorState::Other(_)) =
                        &permit.audio
                    {
                        self.mic_error = Some(MediaErrorState::Other);
                    }

                    if let PermissionState::Denied(MediaPermissionsErrorState::Other(_)) =
                        &permit.video
                    {
                        self.video_error = Some(MediaErrorState::Other);
                    }

                    if permit.audio == PermissionState::Denied(MediaPermissionsErrorState::NoDevice)
                    {
                        self.mic_error = Some(MediaErrorState::NoDevice);
                    }

                    if permit.video == PermissionState::Denied(MediaPermissionsErrorState::NoDevice)
                    {
                        self.video_error = Some(MediaErrorState::NoDevice);
                    }

                    if permit.audio
                        == PermissionState::Denied(MediaPermissionsErrorState::PermissionDenied)
                    {
                        self.mic_error = Some(MediaErrorState::PermissionDenied);
                    }

                    if permit.video
                        == PermissionState::Denied(MediaPermissionsErrorState::PermissionDenied)
                    {
                        self.video_error = Some(MediaErrorState::PermissionDenied);
                    }

                    if self.session_loaded {
                        if self.mic_error.is_some() {
                            self.mic_enabled = false;
                            self.pending_mic_enable = false;
                        }
                        if self.video_error.is_some() {
                            self.video_enabled = false;
                            self.pending_video_enable = false;
                        }
                    } else if self.mic_error.is_some() || self.video_error.is_some() {
                        self.show_device_warning = true;
                    } else {
                        ctx.link().send_message(WsAction::Connect);
                    }

                    if self.device_was_denied {
                        self.device_was_denied = false;
                        self.reload_devices_counter += 1;
                    }

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
                WsAction::CloseDeviceWarning => {
                    self.show_device_warning = false;
                    ctx.link().send_message(WsAction::Connect);
                    true
                }
            },
            Msg::OnPeerAdded(peer_id) => {
                log::info!("New user joined: {peer_id}");
                // Sound is played by OnPeerJoined which has display name context.
                true
            }
            Msg::OnPeerRemoved(_peer_id) => true,
            Msg::OnFirstFrame((_peer_id, media_type)) => matches!(media_type, MediaType::SCREEN),
            Msg::OnMicrophoneError(err) => {
                log::error!("Microphone error (full): {err}");
                self.mic_enabled = false;
                self.user_error = Some(format!("Microphone error: {err}"));
                true
            }
            Msg::OnCameraError(err) => {
                log::error!("Camera error (full): {err}");
                self.video_enabled = false;
                self.user_error = Some(format!("Camera error: {err}"));
                true
            }
            Msg::DismissUserError => {
                self.user_error = None;
                true
            }
            Msg::MeetingAction(action) => {
                match action {
                    MeetingAction::ToggleScreenShare => {
                        // No getUserMedia permission check needed here: getDisplayMedia()
                        // has its own browser-native permission prompt, independent of
                        // camera/mic permissions.
                        // https://developer.mozilla.org/en-US/docs/Web/API/MediaDevices/getDisplayMedia
                        if matches!(self.screen_share_state, ScreenShareState::Idle) {
                            self.screen_share_state = ScreenShareState::Requesting;
                        } else {
                            self.screen_share_state = ScreenShareState::Idle;
                        }
                    }
                    MeetingAction::ToggleMicMute => {
                        if !self.mic_enabled {
                            if self
                                .media_device_access
                                .is_granted(MediaAccessKind::AudioCheck)
                            {
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
                            if self
                                .media_device_access
                                .is_granted(MediaAccessKind::VideoCheck)
                            {
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
                true
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

                // Call leave_meeting API to update participant status in database
                let meeting_id = ctx.props().id.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(e) = crate::meeting_api::leave_meeting(&meeting_id).await {
                        log::error!("Error leaving meeting: {e}");
                    }
                    // Redirect to home after API call completes
                    let _ = window().location().set_href("/");
                });

                true
            }

            Msg::MeetingEnded(end_time) => {
                self.meeting_ended_message = Some(end_time);
                true
            }
            Msg::ScreenShareStateChange(event) => {
                log::info!("Screen share state changed: {event:?}");
                match event {
                    ScreenShareEvent::Started => {
                        self.screen_share_state = ScreenShareState::Active;
                    }
                    ScreenShareEvent::Cancelled | ScreenShareEvent::Stopped => {
                        self.screen_share_state = ScreenShareState::Idle;
                    }
                    ScreenShareEvent::Failed(ref msg) => {
                        log::error!("Screen share failed: {msg}");
                        self.screen_share_state = ScreenShareState::Idle;
                        self.user_error = Some(format!("Screen share failed: {msg}"));
                    }
                }
                true
            }
            Msg::TokenRefreshed(new_token) => {
                log::info!("Room token refreshed, reconnecting with new token");
                self.error = None;
                self.reconnect_attempt = 0;
                let (ws_urls, wt_urls) =
                    Self::build_lobby_urls(&new_token, &ctx.props().display_name, &ctx.props().id);
                self.client.update_server_urls(ws_urls, wt_urls);
                if let Err(e) = self.client.connect() {
                    ctx.link().send_message(WsAction::Log(format!(
                        "Reconnection with refreshed token failed: {e:?}"
                    )));
                }
                true
            }
            Msg::TokenRefreshFailed(err) => {
                warn!("Token refresh failed: {err}");
                self.error = Some(format!("Connection lost, retrying... ({err})"));

                #[cfg(feature = "media-server-jwt-auth")]
                {
                    self.reconnect_attempt += 1;
                    let link = ctx.link().clone();
                    let meeting_id = ctx.props().id.clone();
                    Self::schedule_token_refresh(link, meeting_id, self.reconnect_attempt);
                }

                true
            }
            Msg::WaitingRoomUpdated => {
                self.waiting_room_version = self.waiting_room_version.wrapping_add(1);
                true
            }
            Msg::OnPeerLeft((display_name, user_id)) => {
                log::debug!("TOAST-RX: peer left: {} ({})", display_name, user_id);
                // Don't play sound immediately -- defer it so a rapid
                // join event (waiting-room admission) can cancel it.
                let id = self.toast_counter;
                self.toast_counter += 1;
                self.peer_toasts.push((id, display_name, user_id, false));
                // Defer the leave sound: only play if the toast still exists
                // after 500ms (i.e. no join event cancelled it).
                let link_sound = ctx.link().clone();
                Timeout::new(500, move || {
                    link_sound.send_message(Msg::PlayLeftSoundIfStillActive(id));
                })
                .forget();
                // Schedule toast removal after 8 seconds.
                let link = ctx.link().clone();
                Timeout::new(8_000, move || {
                    link.send_message(Msg::RemovePeerToast(id));
                })
                .forget();
                true
            }
            Msg::OnPeerJoined((display_name, user_id)) => {
                log::debug!("TOAST-RX: peer joined: {} ({})", display_name, user_id);
                // When an observer is admitted from the waiting room, the
                // observer connection closes (PARTICIPANT_LEFT) and the user
                // reconnects as a real participant (PARTICIPANT_JOINED). If
                // there is a pending "left" toast for this user, remove it
                // (the deferred leave sound will also be suppressed because
                // it checks whether the toast still exists).
                self.peer_toasts
                    .retain(|(_, _, uid, is_joined)| !(!is_joined && uid == &user_id));

                // Always show the join toast and play the join sound.
                Self::play_user_joined();
                let id = self.toast_counter;
                self.toast_counter += 1;
                self.peer_toasts.push((id, display_name, user_id, true));
                let link = ctx.link().clone();
                Timeout::new(8_000, move || {
                    link.send_message(Msg::RemovePeerToast(id));
                })
                .forget();
                true
            }
            Msg::RemovePeerToast(toast_id) => {
                let before = self.peer_toasts.len();
                self.peer_toasts.retain(|(id, _, _, _)| *id != toast_id);
                self.peer_toasts.len() != before
            }
            Msg::PlayLeftSoundIfStillActive(toast_id) => {
                if self.peer_toasts.iter().any(|(id, _, _, _)| *id == toast_id) {
                    Self::play_user_left();
                }
                false // no re-render needed
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let display_name = ctx.props().display_name.clone();
        let effective_user_id = ctx
            .props()
            .user_id
            .as_deref()
            .unwrap_or(&display_name)
            .to_string();
        let is_allowed = users_allowed_to_stream().unwrap_or_default();
        let can_stream =
            is_allowed.is_empty() || is_allowed.iter().any(|host| host == &effective_user_id);
        let media_access_granted = self
            .media_device_access
            .is_granted(MediaAccessKind::BothCheck);

        let toggle_peer_list = ctx.link().callback(|_| UserScreenToggleAction::PeerList);
        let toggle_diagnostics = ctx.link().callback(|_| UserScreenToggleAction::Diagnostics);
        let close_diagnostics = ctx.link().callback(|_| UserScreenToggleAction::Diagnostics);

        let real_peers_vec = self.client.sorted_peer_keys();
        // Keep session_id for both PeerTile and PeerList (both need session_id for audio/speaking state matching)
        let mut display_peers_vec = real_peers_vec.clone();
        display_peers_vec.extend(self.fake_peer_ids.iter().cloned());

        let num_display_peers = display_peers_vec.len();
        let num_peers_for_styling = num_display_peers.min(CANVAS_LIMIT);

        let add_fake_peer_disabled = num_display_peers >= CANVAS_LIMIT;

        let rows: Vec<Html> = display_peers_vec
            .iter()
            .take(CANVAS_LIMIT)
            .enumerate()
            .map(|(i, peer_id)| {
                let full_bleed = display_peers_vec.len() == 1
                    && !self.client.is_screen_share_enabled_for_peer(peer_id);
                html!{ <PeerTile key={format!("tile-{}-{}", i, peer_id)} peer_id={peer_id.clone()} full_bleed={full_bleed} host_user_id={ctx.props().host_user_id.clone()} /> }
            })
            .collect();

        let container_style = format!(
            "position: absolute; inset: 0; width: 100%; height: 100%; --num-peers: {};",
            num_peers_for_styling.max(1)
        );

        let on_encoder_settings_update = ctx.link().callback(WsAction::EncoderSettingsUpdated);

        let meeting_link = {
            let origin_result = window().location().origin();
            let origin = origin_result.unwrap_or_else(|_| "".to_string());
            format!("{}/meeting/{}", origin, ctx.props().id)
        };

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

        if !self.meeting_joined {
            return html! {
                <ContextProvider<VideoCallClientCtx> context={self.client.clone()}>
                    <div id="main-container" class="meeting-page">
                        <BrowserCompatibility/>
                    <div id="join-meeting-container" style="position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000; z-index: 1000;">
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
                            { if ctx.props().is_owner { "Start Meeting" } else { "Join Meeting" } }
                        </button>
                        {
                            if self.show_device_warning {
                                html! {
                                   <div class="modal-overlay">
                                      <div class="modal-content">
                                           <h3>{"Device access problem"}</h3>

                                           { self.render_device_error() }

                                          <button
                                              class="btn-apple btn-primary"
                                              onclick={ctx.link().callback(|_| WsAction::CloseDeviceWarning)}
                                              style="margin-top: 1.5rem;"
                                          >
                                              {"Ok"}
                                          </button>
                                      </div>
                                    </div>
                                }
                            } else {
                                html! {}
                            }
                        }
                    </div>
                    </div>
                </ContextProvider<VideoCallClientCtx>>
            };
        }

        let meeting_time = MeetingTime {
            call_start_time: self.call_start_time,
            meeting_start_time: self.meeting_start_time_server,
        };

        html! {
            <ContextProvider<MeetingTimeCtx> context={meeting_time}>
            <ContextProvider<VideoCallClientCtx> context={self.client.clone()}>
                <div id="main-container" class="meeting-page">
                    <BrowserCompatibility/>

                    // "participant joined/left" toast notifications
                    if !self.peer_toasts.is_empty() {
                        <div class="peer-toasts">
                            { for self.peer_toasts.iter().map(|(id, display_name, _uid, is_joined)| {
                                let key = id.to_string();
                                let is_joined = *is_joined;
                                let variant_class = if is_joined {
                                    "peer-toast toast-joined"
                                } else {
                                    "peer-toast toast-left"
                                };
                                let action_text = if is_joined { "joined the meeting" } else { "left the meeting" };
                                let icon_svg = if is_joined {
                                    html! {
                                        <svg width="16" height="16" viewBox="0 0 24 24" fill="none"
                                             stroke="currentColor" stroke-width="2"
                                             stroke-linecap="round" stroke-linejoin="round">
                                            <path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/>
                                            <circle cx="9" cy="7" r="4"/>
                                            <line x1="19" y1="8" x2="19" y2="14"/>
                                            <line x1="22" y1="11" x2="16" y2="11"/>
                                        </svg>
                                    }
                                } else {
                                    html! {
                                        <svg width="16" height="16" viewBox="0 0 24 24" fill="none"
                                             stroke="currentColor" stroke-width="2"
                                             stroke-linecap="round" stroke-linejoin="round">
                                            <path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/>
                                            <circle cx="9" cy="7" r="4"/>
                                            <line x1="22" y1="11" x2="16" y2="11"/>
                                        </svg>
                                    }
                                };
                                html! {
                                    <div {key} class={variant_class}>
                                        <span class="toast-icon">{ icon_svg }</span>
                                        <span class="toast-text">
                                            <span class="toast-name">{ display_name.clone() }</span>
                                            <br/>
                                            <span class="toast-action">{ action_text }</span>
                                        </span>
                                    </div>
                                }
                            })}
                        </div>
                    }

                <div id="grid-container"
                    class={grid_container_classes}
                    data-peers={num_peers_for_styling.to_string()}
                    style={container_style}>
                    { rows }

                    {
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
                        if can_stream {
                            let mic_available = matches!(self.mic_error, None);
                            let video_available = matches!(self.video_error, None);
                            html! {
                                <nav class="host">
                                    <div class="controls">
                                        <nav class="video-controls-container">
                                            <MicButton
                                                enabled={self.mic_enabled}
                                                available={mic_available}
                                                onclick={ctx.link().callback(|_| MeetingAction::ToggleMicMute)}
                                            />
                                            <CameraButton
                                                enabled={self.video_enabled}
                                                available={video_available}
                                                onclick={ctx.link().callback(|_| MeetingAction::ToggleVideoOnOff)}
                                            />
                                            {
                                                if !is_ios() {
                                                    let is_active = matches!(self.screen_share_state, ScreenShareState::Active);
                                                    let is_disabled = matches!(self.screen_share_state, ScreenShareState::Requesting);
                                                    html! {
                                                        <ScreenShareButton
                                                            active={is_active}
                                                            disabled={is_disabled}
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
                                        if let Some(err) = &self.user_error {
                                            let displayed: String = err.chars().take(200).collect();
                                            html!{
                                                <div class="glass-backdrop">
                                                    <div class="card-apple" style="width: 380px;">
                                                        <h4 style="margin-top:0;">{"Error"}</h4>
                                                        <p style="margin-top:0.5rem;">{ displayed }</p>
                                                        <div style="display:flex; gap:8px; justify-content:flex-end; margin-top:12px;">
                                                            <button class="btn-apple btn-primary btn-sm" onclick={ctx.link().callback(|_| Msg::DismissUserError)}>{"OK"}</button>
                                                        </div>
                                                    </div>
                                                </div>
                                            }
                                        } else { html!{} }
                                    }
                                    {
                                         if media_access_granted {
                                             html! {<Host
                                                 share_screen={self.screen_share_state.is_sharing()}
                                                 mic_enabled={self.mic_enabled}
                                                 video_enabled={self.video_enabled}
                                                 audio_level={self.local_audio_level}
                                                 on_encoder_settings_update={on_encoder_settings_update}
                                                 device_settings_open={self.device_settings_open}
                                                 on_device_settings_toggle={ctx.link().callback(|_| UserScreenToggleAction::DeviceSettings)}
                                                 on_microphone_error={ctx.link().callback(Msg::OnMicrophoneError)}
                                                 on_camera_error={ctx.link().callback(Msg::OnCameraError)}
                                                 on_screen_share_state={ctx.link().callback(Msg::ScreenShareStateChange)}
                                                 reload_devices_counter = {self.reload_devices_counter}
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
                            let peer_audio_states: HashMap<String, bool> = display_peers_vec
                                .iter()
                                .map(|peer_id| (peer_id.clone(), self.client.is_audio_enabled_for_peer(peer_id)))
                                .collect();
                            html! {
                                <PeerList
                                    peers={display_peers_vec.clone()}
                                    onclose={toggle_peer_list}
                                    peer_audio_states={peer_audio_states}
                                    self_muted={!self.mic_enabled}
                                    self_speaking={self.local_speaking}
                                    show_meeting_info={self.meeting_info_open}
                                    room_id={ctx.props().id.clone()}
                                    num_participants={num_display_peers}
                                    is_active={self.meeting_joined && self.meeting_ended_message.is_none()}
                                    on_toggle_meeting_info={toggle_meeting_info}
                                    host_display_name={ctx.props().host_display_name.clone()}
                                    host_user_id={ctx.props().host_user_id.clone()}
                                />
                            }
                        } else {
                                html! {}
                        }
                    }
                </div>

                // Waiting room controls - all admitted participants can manage waiting room
                <HostControls
                    meeting_id={ctx.props().id.clone()}
                    is_admitted={true}
                    waiting_room_version={self.waiting_room_version}
                />

                {
                    if let Some(ref message) = self.meeting_ended_message {
                        html! { <MeetingEndedOverlay message={message.clone()} /> }
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
                                share_screen={self.screen_share_state.is_sharing()}
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

// ---------------------------------------------------------------------------
// Tests for reconnect_delay_ms
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    /// attempt=0 should return Some(d) where d is in the base range.
    /// base = 1000, jitter in [-25%, +25%), so delay in [750, 1250).
    #[wasm_bindgen_test]
    fn reconnect_delay_attempt_0_returns_value_in_expected_range() {
        for _ in 0..50 {
            let delay = AttendantsComponent::reconnect_delay_ms(0);
            assert!(delay.is_some(), "attempt 0 should return Some");
            let d = delay.unwrap();
            assert!(d >= 750, "attempt 0 delay {d} should be >= 750");
            assert!(d <= 1250, "attempt 0 delay {d} should be <= 1250");
        }
    }

    /// attempt=9 should return Some(d) with base capped at MAX_DELAY_MS (16000).
    /// jitter in [-25%, +25%), so delay in [12000, 20000).
    #[wasm_bindgen_test]
    fn reconnect_delay_attempt_9_returns_capped_value() {
        for _ in 0..50 {
            let delay = AttendantsComponent::reconnect_delay_ms(9);
            assert!(delay.is_some(), "attempt 9 should return Some");
            let d = delay.unwrap();
            assert!(d >= 12000, "attempt 9 delay {d} should be >= 12000");
            assert!(d <= 20000, "attempt 9 delay {d} should be <= 20000");
        }
    }

    /// attempt=10 exceeds MAX_RECONNECT_ATTEMPTS and should return None.
    #[wasm_bindgen_test]
    fn reconnect_delay_attempt_10_returns_none() {
        assert!(
            AttendantsComponent::reconnect_delay_ms(10).is_none(),
            "attempt 10 should return None"
        );
    }

    /// Attempts beyond 10 should also return None.
    #[wasm_bindgen_test]
    fn reconnect_delay_attempt_beyond_max_returns_none() {
        assert!(AttendantsComponent::reconnect_delay_ms(11).is_none());
        assert!(AttendantsComponent::reconnect_delay_ms(100).is_none());
        assert!(AttendantsComponent::reconnect_delay_ms(u32::MAX).is_none());
    }

    /// Backoff should roughly double each attempt (accounting for jitter).
    /// We compare the averages of many samples to the expected base values.
    #[wasm_bindgen_test]
    fn reconnect_delay_backoff_roughly_doubles() {
        let samples = 200;
        for attempt in 0..4u32 {
            let expected_base =
                (1000u32.saturating_mul(2u32.saturating_pow(attempt))).min(16_000) as f64;
            let sum: f64 = (0..samples)
                .map(|_| AttendantsComponent::reconnect_delay_ms(attempt).unwrap() as f64)
                .sum();
            let avg = sum / samples as f64;
            // Average should be close to the base (jitter is symmetric around 0).
            // Allow 15% tolerance for randomness.
            let tolerance = expected_base * 0.15;
            assert!(
                (avg - expected_base).abs() < tolerance,
                "attempt {attempt}: avg {avg:.0} should be near expected base {expected_base:.0} (tolerance {tolerance:.0})"
            );
        }
    }

    /// The minimum possible return value is 500 (enforced by .max(500.0)).
    /// Verify no value goes below 500 for any valid attempt.
    #[wasm_bindgen_test]
    fn reconnect_delay_never_below_500() {
        for attempt in 0..10u32 {
            for _ in 0..20 {
                let d = AttendantsComponent::reconnect_delay_ms(attempt).unwrap();
                assert!(d >= 500, "attempt {attempt}: delay {d} must be >= 500");
            }
        }
    }
}

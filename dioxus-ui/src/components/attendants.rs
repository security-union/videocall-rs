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
use crate::context::MeetingTime;
use dioxus::prelude::Element as DioxusElement;
use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use gloo_utils::window;
use log::error;
use std::cell::RefCell;
use std::rc::Rc;
use videocall_client::utils::is_ios;
use videocall_client::Callback as VcCallback;
use videocall_client::{
    MediaDeviceAccess, ScreenShareEvent, VideoCallClient, VideoCallClientOptions,
};
use web_sys::HtmlAudioElement;

#[derive(Clone, Debug, PartialEq)]
pub enum ScreenShareState {
    Idle,
    Requesting,
    Active,
}

impl ScreenShareState {
    pub fn is_sharing(&self) -> bool {
        !matches!(self, ScreenShareState::Idle)
    }
}

/// Build the WebSocket and WebTransport lobby URLs for the media server.
#[allow(unused_variables)]
fn build_lobby_urls(token: &str, email: &str, id: &str) -> (Vec<String>, Vec<String>) {
    #[cfg(feature = "media-server-jwt-auth")]
    let lobby_url = |base: &str| format!("{base}/lobby?token={token}");

    #[cfg(not(feature = "media-server-jwt-auth"))]
    let lobby_url = |base: &str| format!("{base}/lobby/{email}/{id}");

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

fn play_user_joined() {
    if let Some(_window) = web_sys::window() {
        if let Ok(audio) = HtmlAudioElement::new_with_src("/assets/hi.wav") {
            audio.set_volume(0.4);
            if let Err(e) = audio.play() {
                log::warn!("Failed to play notification sound: {e:?}");
            }
        }
    }
}

/// Schedule a reconnection attempt after 1 second (JWT auth path).
///
/// Refreshes the room token, rebuilds lobby URLs, updates the client, and
/// reconnects.  On failure it retries by scheduling itself again — matching
/// yew-ui's `schedule_token_refresh` retry behaviour.
#[cfg(feature = "media-server-jwt-auth")]
fn schedule_reconnect(
    client_cell: Rc<RefCell<Option<VideoCallClient>>>,
    meeting_id: String,
    email: String,
    mut connection_error: Signal<Option<String>>,
    mut meeting_ended_message: Signal<Option<String>>,
) {
    Timeout::new(1_000, move || {
        wasm_bindgen_futures::spawn_local(async move {
            match crate::meeting_api::refresh_room_token(&meeting_id).await {
                Ok(new_token) => {
                    log::info!("Room token refreshed, reconnecting with new token");
                    let (ws, wt) = build_lobby_urls(&new_token, &email, &meeting_id);
                    if let Some(client) = client_cell.borrow_mut().as_mut() {
                        client.update_server_urls(ws, wt);
                        if let Err(e) = client.connect() {
                            log::error!("Reconnection with refreshed token failed: {e:?}");
                        }
                    }
                    connection_error.set(None);
                }
                Err(crate::meeting_api::JoinError::MeetingNotActive) => {
                    meeting_ended_message.set(Some("The meeting has ended.".to_string()));
                }
                Err(e) => {
                    connection_error.set(Some(format!("Connection lost, retrying... ({e})")));
                    // Retry after another second
                    schedule_reconnect(
                        client_cell,
                        meeting_id,
                        email,
                        connection_error,
                        meeting_ended_message,
                    );
                }
            }
        });
    })
    .forget();
}

#[component]
pub fn AttendantsComponent(
    #[props(default)] id: String,
    #[props(default)] email: String,
    e2ee_enabled: bool,
    webtransport_enabled: bool,
    #[props(default)] user_name: Option<String>,
    #[props(default)] user_email: Option<String>,
    #[props(default)] on_logout: Option<EventHandler<()>>,
    #[props(default)] host_display_name: Option<String>,
    #[props(default)] auto_join: bool,
    #[props(default)] is_owner: bool,
    #[props(default)] room_token: String,
    #[props(default = true)] waiting_room_enabled: bool,
) -> DioxusElement {
    // Clone props that will be used in multiple closures
    let id_for_peer_list = id.clone();

    // --- State signals ---
    let mut screen_share_state = use_signal(|| ScreenShareState::Idle);
    let mut mic_enabled = use_signal(|| false);
    let mut video_enabled = use_signal(|| false);
    let mut peer_list_open = use_signal(|| false);
    let mut diagnostics_open = use_signal(|| false);
    let mut device_settings_open = use_signal(|| false);
    let mut connection_error = use_signal(|| None::<String>);
    let mut user_error = use_signal(|| None::<String>);
    let mut meeting_joined = use_signal(|| false);
    let mut show_copy_toast = use_signal(|| false);
    let mut meeting_start_time_server = use_signal(|| None::<f64>);
    let mut call_start_time = use_signal(|| None::<f64>);
    let mut show_dropdown = use_signal(|| false);
    let meeting_ended_message = use_signal(|| None::<String>);
    let mut meeting_info_open = use_signal(|| false);
    let peer_list_version = use_signal(|| 0u32);
    let media_access_granted = use_signal(|| false);
    let mut pending_mic_enable = use_signal(|| false);
    let mut pending_video_enable = use_signal(|| false);
    let mut waiting_room_toggle = use_signal(move || waiting_room_enabled);

    // Create VideoCallClient and MediaDeviceAccess once.
    // We use an Rc<RefCell<Option<VideoCallClient>>> so the on_connection_lost
    // callback can access the client for reconnection. The cell is populated
    // right after VideoCallClient::new().
    let client = use_hook(|| {
        #[cfg(feature = "media-server-jwt-auth")]
        let token = {
            let t = room_token.clone();
            assert!(
                !t.is_empty(),
                "media-server-jwt-auth is enabled but room_token is empty"
            );
            t
        };
        #[cfg(not(feature = "media-server-jwt-auth"))]
        let token = String::new();

        let (websocket_urls, webtransport_urls) = build_lobby_urls(&token, &email, &id);

        log::info!(
            "DIOXUS-UI: Creating VideoCallClient for {} in meeting {}",
            email,
            id
        );

        let client_for_reconnect: Rc<RefCell<Option<VideoCallClient>>> =
            Rc::new(RefCell::new(None));

        let opts = VideoCallClientOptions {
            userid: email.clone(),
            meeting_id: id.clone(),
            websocket_urls,
            webtransport_urls,
            enable_e2ee: e2ee_enabled,
            enable_webtransport: webtransport_enabled,
            on_connected: VcCallback::from(move |_| {
                log::info!("DIOXUS-UI: Connection established");
                let mut connection_error = connection_error;
                let mut call_start_time = call_start_time;
                connection_error.set(None);
                call_start_time.set(Some(js_sys::Date::now()));
            }),
            on_connection_lost: {
                let id = id.clone();
                let email = email.clone();
                let client_cell = client_for_reconnect.clone();
                VcCallback::from(move |_| {
                    log::warn!("DIOXUS-UI: Connection lost");
                    let mut connection_error = connection_error;
                    let meeting_ended_message = meeting_ended_message;
                    connection_error.set(Some("Connection lost, reconnecting...".to_string()));

                    #[cfg(feature = "media-server-jwt-auth")]
                    {
                        let client_cell = client_cell.clone();
                        let meeting_id = id.clone();
                        let email = email.clone();
                        schedule_reconnect(
                            client_cell,
                            meeting_id,
                            email,
                            connection_error,
                            meeting_ended_message,
                        );
                    }

                    #[cfg(not(feature = "media-server-jwt-auth"))]
                    {
                        let client_cell = client_cell.clone();
                        Timeout::new(1_000, move || {
                            if let Some(client) = client_cell.borrow_mut().as_mut() {
                                if let Err(e) = client.connect() {
                                    log::error!("Reconnection failed: {e:?}");
                                }
                            }
                        })
                        .forget();
                    }
                })
            },
            on_peer_added: VcCallback::from(move |session_id: String| {
                log::info!("New user joined: {session_id}");
                play_user_joined();
                let mut v = peer_list_version;
                v.set(v() + 1);
            }),
            on_peer_first_frame: VcCallback::noop(),
            on_peer_removed: Some(VcCallback::from(move |peer_id: String| {
                log::info!("Peer removed: {peer_id}");
                let mut v = peer_list_version;
                v.set(v() + 1);
            })),
            get_peer_video_canvas_id: VcCallback::from(|email| email),
            get_peer_screen_canvas_id: VcCallback::from(|email| format!("screen-share-{}", &email)),
            enable_diagnostics: true,
            diagnostics_update_interval_ms: Some(1000),
            enable_health_reporting: true,
            health_reporting_interval_ms: Some(5000),
            on_encoder_settings_update: None,
            rtt_testing_period_ms: server_election_period_ms().unwrap_or(2000),
            rtt_probe_interval_ms: Some(200),
            on_meeting_info: Some(VcCallback::from(move |start_time_ms: f64| {
                log::info!("Meeting started at Unix timestamp: {start_time_ms}");
                let mut meeting_start_time_server = meeting_start_time_server;
                meeting_start_time_server.set(Some(start_time_ms));
            })),
            on_meeting_ended: Some(VcCallback::from(
                move |(end_time_ms, message): (f64, String)| {
                    log::info!("Meeting ended at Unix timestamp: {end_time_ms}");
                    let mut meeting_start_time_server = meeting_start_time_server;
                    let mut meeting_ended_message = meeting_ended_message;
                    meeting_start_time_server.set(Some(end_time_ms));
                    meeting_ended_message.set(Some(message));
                },
            )),
        };

        let client = VideoCallClient::new(opts);
        *client_for_reconnect.borrow_mut() = Some(client.clone());
        client
    });

    let mda = use_hook(|| {
        let mut mda = MediaDeviceAccess::new();
        let client_cell = RefCell::new(client.clone());
        mda.on_granted = VcCallback::from(move |_| {
            let mut media_access_granted = media_access_granted;
            let mut meeting_joined = meeting_joined;
            let mut mic_enabled = mic_enabled;
            let mut video_enabled = video_enabled;
            let mut pending_mic_enable = pending_mic_enable;
            let mut pending_video_enable = pending_video_enable;
            media_access_granted.set(true);

            // Fulfil any pending mic/camera enables that triggered the permission request.
            if pending_mic_enable() {
                mic_enabled.set(true);
                pending_mic_enable.set(false);
            }
            if pending_video_enable() {
                video_enabled.set(true);
                pending_video_enable.set(false);
            }

            // Connect after permissions granted
            if let Err(e) = client_cell.borrow_mut().connect() {
                log::error!("Connection failed: {e:?}");
            }
            meeting_joined.set(true);
        });
        mda.on_denied = VcCallback::from(move |e| {
            let mut connection_error = connection_error;
            let mut meeting_joined = meeting_joined;
            let complete_error = format!("Error requesting permissions: Please make sure to allow access to both camera and microphone. ({e:?})");
            error!("{complete_error}");
            connection_error.set(Some(complete_error));
            meeting_joined.set(false);
        });
        Rc::new(RefCell::new(mda))
    });

    // Provide contexts for child components
    use_context_provider(|| client.clone());
    let mut meeting_time_signal = use_signal(MeetingTime::default);
    use_context_provider(|| meeting_time_signal);

    // Check for config errors
    use_effect(move || {
        if let Err(e) = crate::constants::app_config() {
            log::error!("{e:?}");
            connection_error.set(Some(e));
        }
    });

    // Auto-join on first render if requested
    {
        let mda = mda.clone();
        use_effect(move || {
            if auto_join {
                mda.borrow().request();
            }
        });
    }

    // --- Derived values ---
    let _ = peer_list_version(); // subscribe to trigger re-renders when peers change
    let display_peers = client.sorted_peer_keys();
    let peers_for_display: Vec<String> = display_peers
        .iter()
        .map(|session_id| {
            client
                .get_peer_email(session_id)
                .unwrap_or_else(|| session_id.clone())
        })
        .collect();
    let num_display_peers = display_peers.len();
    let num_peers_for_styling = num_display_peers.min(CANVAS_LIMIT);

    let container_style = format!(
        "position: absolute; inset: 0; width: 100%; height: 100%; --num-peers: {};",
        num_peers_for_styling.max(1)
    );

    let meeting_link = {
        let origin = window().location().origin().unwrap_or_default();
        format!("{}/meeting/{}", origin, id)
    };

    let is_allowed = users_allowed_to_stream().unwrap_or_default();
    let can_stream = is_allowed.is_empty() || is_allowed.iter().any(|host| host == &email);

    // --- Pre-join screen ---
    if !meeting_joined() {
        return rsx! {
            div {
                id: "main-container",
                class: "meeting-page",
                BrowserCompatibility {}
                div {
                    id: "join-meeting-container",
                    style: "position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000; z-index: 1000;",

                    // Logout dropdown
                    if let (Some(name), Some(u_email), Some(on_logout_handler)) = (user_name.as_deref(), user_email.as_deref(), on_logout) {
                        div { style: "position: absolute; top: 1rem; right: 1rem; z-index: 1001;",
                            button {
                                onclick: move |_| show_dropdown.set(!show_dropdown()),
                                style: "display: flex; align-items: center; gap: 0.5rem; padding: 0.5rem 1rem; background: #1f2937; border-radius: 0.5rem; color: white; font-size: 0.875rem; border: none; cursor: pointer;",
                                span { "{name}" }
                                svg { style: "width: 1rem; height: 1rem;", fill: "none", stroke: "currentColor", view_box: "0 0 24 24",
                                    path { stroke_linecap: "round", stroke_linejoin: "round", stroke_width: "2", d: "M19 9l-7 7-7-7" }
                                }
                            }
                            if show_dropdown() {
                                div { style: "position: absolute; right: 0; margin-top: 0.5rem; width: 14rem; background: white; border-radius: 0.5rem; box-shadow: 0 10px 15px -3px rgba(0, 0, 0, 0.1); border: 1px solid #e5e7eb; padding: 0.25rem 0;",
                                    div { style: "padding: 0.75rem 1rem; border-bottom: 1px solid #e5e7eb;",
                                        p { style: "font-size: 0.875rem; font-weight: 500; color: #111827; margin: 0;", "{name}" }
                                        p { style: "font-size: 0.75rem; color: #6b7280; margin: 0; overflow: hidden; text-overflow: ellipsis;", "{u_email}" }
                                    }
                                    button {
                                        onclick: move |_| on_logout_handler.call(()),
                                        style: "width: 100%; text-align: left; padding: 0.5rem 1rem; font-size: 0.875rem; color: #dc2626; background: transparent; border: none; cursor: pointer;",
                                        "Sign out"
                                    }
                                }
                            }
                        }
                    }

                    div { style: "text-align: center; color: white; margin-bottom: 2rem;",
                        h2 { "Ready to join the meeting?" }
                        p { "Click the button below to join and start listening to others." }
                        if let Some(err) = connection_error() {
                            p { style: "color: #ff6b6b; margin-top: 1rem;", "{err}" }
                        }
                    }
                    if is_owner {
                        {
                            let meeting_id_for_toggle = id.clone();
                            rsx! {
                                div { style: "display: flex; align-items: center; justify-content: center; gap: 0.75rem; margin-bottom: 1.5rem; color: white;",
                                    span { style: "font-size: 0.9rem;", "Waiting Room" }
                                    button {
                                        r#type: "button",
                                        style: if waiting_room_toggle() {
                                            "position: relative; width: 44px; height: 24px; border-radius: 12px; border: none; cursor: pointer; background: #34c759; transition: background 0.2s;"
                                        } else {
                                            "position: relative; width: 44px; height: 24px; border-radius: 12px; border: none; cursor: pointer; background: #636366; transition: background 0.2s;"
                                        },
                                        onclick: {
                                            let meeting_id = meeting_id_for_toggle.clone();
                                            move |_| {
                                                let new_val = !waiting_room_toggle();
                                                waiting_room_toggle.set(new_val);
                                                let meeting_id = meeting_id.clone();
                                                wasm_bindgen_futures::spawn_local(async move {
                                                    if let Err(e) = crate::meeting_api::update_meeting(&meeting_id, new_val).await {
                                                        log::error!("Failed to update waiting room setting: {e}");
                                                    }
                                                });
                                            }
                                        },
                                        div {
                                            style: if waiting_room_toggle() {
                                                "position: absolute; top: 2px; left: 22px; width: 20px; height: 20px; border-radius: 50%; background: white; transition: left 0.2s;"
                                            } else {
                                                "position: absolute; top: 2px; left: 2px; width: 20px; height: 20px; border-radius: 50%; background: white; transition: left 0.2s;"
                                            },
                                        }
                                    }
                                }
                                p { style: "text-align: center; color: rgba(255,255,255,0.6); font-size: 0.8rem; margin-bottom: 1.5rem; margin-top: -0.75rem;",
                                    if waiting_room_toggle() {
                                        "Participants will wait for your approval before joining"
                                    } else {
                                        "Participants will join the meeting directly"
                                    }
                                }
                            }
                        }
                    }
                    button {
                        class: "btn-apple btn-primary",
                        onclick: move |_| {
                            mda.borrow().request();
                        },
                        if is_owner { "Start Meeting" } else { "Join Meeting" }
                    }
                }
            }
        };
    }

    // --- Meeting view ---
    // Update the meeting time context signal
    meeting_time_signal.set(MeetingTime {
        call_start_time: call_start_time(),
        meeting_start_time: meeting_start_time_server(),
    });

    rsx! {
        div {
            // Provide MeetingTime context
            // Provide VideoCallClient context
            div {
                id: "main-container",
                class: "meeting-page",
                BrowserCompatibility {}
                div {
                    id: "grid-container",
                    "data-peers": "{num_peers_for_styling}",
                    style: "{container_style}",

                    // Peer tiles
                    for (i, peer_id) in display_peers.iter().take(CANVAS_LIMIT).enumerate() {
                        {
                            let full_bleed = display_peers.len() == 1
                                && !client.is_screen_share_enabled_for_peer(peer_id);
                            rsx! {
                                PeerTile {
                                    key: "tile-{i}-{peer_id}",
                                    peer_id: peer_id.clone(),
                                    full_bleed: full_bleed,
                                    host_display_name: host_display_name.clone(),
                                }
                            }
                        }
                    }

                    // Invitation overlay when no peers
                    if num_display_peers == 0 {
                        div {
                            id: "invite-overlay",
                            class: "card-apple",
                            style: "position: fixed; top: 50%; left: 50%; transform: translate(-50%, -50%); width: 90%; max-width: 420px; z-index: 0; text-align: center;",
                            h4 { style: "margin-top:0;", "Your meeting is ready!" }
                            p { style: "font-size: 0.9rem; opacity: 0.8;", "Share this meeting link with others you want in the meeting" }
                            div { style: "display:flex; align-items:center; margin-top: 0.75rem; margin-bottom: 0.75rem;",
                                input {
                                    id: "meeting-link-input",
                                    value: "{meeting_link}",
                                    readonly: true,
                                    class: "input-apple",
                                    style: "flex:1; overflow:hidden; text-overflow: ellipsis;",
                                }
                                button {
                                    class: if show_copy_toast() { "btn-apple btn-primary btn-sm copy-button btn-pop-animate" } else { "btn-apple btn-primary btn-sm copy-button" },
                                    style: "margin-left: 0.5rem;",
                                    onclick: {
                                        let meeting_link = meeting_link.clone();
                                        move |_| {
                                            if let Some(clipboard) = web_sys::window().map(|w| w.navigator().clipboard()) {
                                                let _ = clipboard.write_text(&meeting_link);
                                                show_copy_toast.set(true);
                                                Timeout::new(1640, move || {
                                                    show_copy_toast.set(false);
                                                }).forget();
                                            }
                                        }
                                    },
                                    "Copy"
                                    if show_copy_toast() {
                                        div { class: "sparkles", "aria-hidden": "true",
                                            span { class: "sparkle" }
                                            span { class: "sparkle" }
                                            span { class: "sparkle" }
                                            span { class: "sparkle" }
                                            span { class: "sparkle" }
                                            span { class: "sparkle" }
                                            span { class: "sparkle" }
                                            span { class: "sparkle" }
                                        }
                                    }
                                }
                            }
                            p { style: "font-size: 0.8rem; opacity: 0.7;", "People who use this meeting link must get your permission before they can join." }
                            div {
                                class: if show_copy_toast() { "copy-toast copy-toast--visible" } else { "copy-toast" },
                                role: "alert",
                                "aria-live": "assertive",
                                "Link copied to clipboard"
                            }
                        }
                    }

                    // Controls nav
                    if can_stream {
                        nav { class: "host",
                            div { class: "controls",
                                nav { class: "video-controls-container",
                                    {
                                        let mda_mic = mda.clone();
                                        rsx! {
                                            MicButton {
                                                enabled: mic_enabled(),
                                                onclick: move |_| {
                                                    if !mic_enabled() {
                                                        if media_access_granted() {
                                                            mic_enabled.set(true);
                                                        } else {
                                                            pending_mic_enable.set(true);
                                                            mda_mic.borrow().request();
                                                        }
                                                    } else {
                                                        mic_enabled.set(false);
                                                    }
                                                },
                                            }
                                        }
                                    }
                                    {
                                        let mda_cam = mda.clone();
                                        rsx! {
                                            CameraButton {
                                                enabled: video_enabled(),
                                                onclick: move |_| {
                                                    if !video_enabled() {
                                                        if media_access_granted() {
                                                            video_enabled.set(true);
                                                            // "Warm up" the video element in this user-gesture
                                                            // call stack.  Safari blocks play() outside user
                                                            // gestures; calling it here marks the element as
                                                            // user-activated so later srcObject + autoplay works.
                                                            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                                                                if let Some(elem) = doc.get_element_by_id("webcam") {
                                                                    use wasm_bindgen::JsCast;
                                                                    if let Ok(v) = elem.dyn_into::<web_sys::HtmlVideoElement>() {
                                                                        let _ = v.play();
                                                                    }
                                                                }
                                                            }
                                                        } else {
                                                            pending_video_enable.set(true);
                                                            mda_cam.borrow().request();
                                                        }
                                                    } else {
                                                        video_enabled.set(false);
                                                    }
                                                },
                                            }
                                        }
                                    }
                                    if !is_ios() {
                                        {
                                            let is_active = matches!(screen_share_state(), ScreenShareState::Active);
                                            let is_disabled = matches!(screen_share_state(), ScreenShareState::Requesting);
                                            rsx! {
                                                ScreenShareButton {
                                                    active: is_active,
                                                    disabled: is_disabled,
                                                    onclick: move |_| {
                                                        if matches!(screen_share_state(), ScreenShareState::Idle) {
                                                            screen_share_state.set(ScreenShareState::Requesting);
                                                        } else {
                                                            screen_share_state.set(ScreenShareState::Idle);
                                                        }
                                                    },
                                                }
                                            }
                                        }
                                    }
                                    PeerListButton {
                                        open: peer_list_open(),
                                        onclick: move |_| {
                                            peer_list_open.set(!peer_list_open());
                                            if peer_list_open() {
                                                diagnostics_open.set(false);
                                            }
                                        },
                                    }
                                    DiagnosticsButton {
                                        open: diagnostics_open(),
                                        onclick: move |_| {
                                            diagnostics_open.set(!diagnostics_open());
                                            if diagnostics_open() {
                                                peer_list_open.set(false);
                                            }
                                        },
                                    }
                                    DeviceSettingsButton {
                                        open: device_settings_open(),
                                        onclick: move |_| {
                                            device_settings_open.set(!device_settings_open());
                                            if device_settings_open() {
                                                peer_list_open.set(false);
                                                diagnostics_open.set(false);
                                            }
                                        },
                                    }
                                    {
                                        let hangup_client = client.clone();
                                        let hangup_id = id.clone();
                                        rsx! {
                                            HangUpButton {
                                                onclick: move |_| {
                                                    log::info!("Hanging up - resetting to initial state");
                                                    if hangup_client.is_connected() {
                                                        if let Err(e) = hangup_client.disconnect() {
                                                            log::error!("Error disconnecting: {e}");
                                                        }
                                                    }
                                                    meeting_joined.set(false);
                                                    mic_enabled.set(false);
                                                    video_enabled.set(false);
                                                    pending_mic_enable.set(false);
                                                    pending_video_enable.set(false);
                                                    call_start_time.set(None);
                                                    meeting_start_time_server.set(None);

                                                    let meeting_id = hangup_id.clone();
                                                    wasm_bindgen_futures::spawn_local(async move {
                                                        if let Err(e) = crate::meeting_api::leave_meeting(&meeting_id).await {
                                                            log::error!("Error leaving meeting: {e}");
                                                        }
                                                        let _ = window().location().set_href("/");
                                                    });
                                                },
                                            }
                                        }
                                    }
                                }
                            }
                            // User error dialog
                            if let Some(err) = user_error() {
                                {
                                    let displayed: String = err.chars().take(200).collect();
                                    rsx! {
                                        div { class: "glass-backdrop",
                                            div { class: "card-apple", style: "width: 380px;",
                                                h4 { style: "margin-top:0;", "Error" }
                                                p { style: "margin-top:0.5rem;", "{displayed}" }
                                                div { style: "display:flex; gap:8px; justify-content:flex-end; margin-top:12px;",
                                                    button {
                                                        class: "btn-apple btn-primary btn-sm",
                                                        onclick: move |_| user_error.set(None),
                                                        "OK"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            // Host component (encoders)
                            if media_access_granted() {
                                Host {
                                    share_screen: screen_share_state().is_sharing(),
                                    mic_enabled: mic_enabled(),
                                    video_enabled: video_enabled(),
                                    on_encoder_settings_update: move |_s: String| {},
                                    device_settings_open: device_settings_open(),
                                    on_device_settings_toggle: move |_| {
                                        device_settings_open.set(!device_settings_open());
                                    },
                                    on_microphone_error: move |err: String| {
                                        log::error!("Microphone error: {err}");
                                        mic_enabled.set(false);
                                        user_error.set(Some(format!("Microphone error: {err}")));
                                    },
                                    on_camera_error: move |err: String| {
                                        log::error!("Camera error: {err}");
                                        video_enabled.set(false);
                                        user_error.set(Some(format!("Camera error: {err}")));
                                    },
                                    on_screen_share_state: move |event: ScreenShareEvent| {
                                        log::info!("Screen share state changed: {event:?}");
                                        match event {
                                            ScreenShareEvent::Started => screen_share_state.set(ScreenShareState::Active),
                                            ScreenShareEvent::Cancelled | ScreenShareEvent::Stopped => screen_share_state.set(ScreenShareState::Idle),
                                            ScreenShareEvent::Failed(ref msg) => {
                                                log::error!("Screen share failed: {msg}");
                                                screen_share_state.set(ScreenShareState::Idle);
                                                user_error.set(Some(format!("Screen share failed: {msg}")));
                                            }
                                        }
                                    },
                                }
                            }
                            {
                                let status_client = client.clone();
                                rsx! {
                                    div {
                                        class: if status_client.is_connected() { "connection-led connected" } else { "connection-led connecting" },
                                        title: if status_client.is_connected() { "Connected" } else { "Connecting" },
                                    }
                                }
                            }
                        }
                    }
                }

                // Peer list sidebar
                div {
                    id: "peer-list-container",
                    class: if peer_list_open() { "visible" } else { "" },
                    if peer_list_open() {
                        PeerList {
                            peers: peers_for_display.clone(),
                            onclose: move |_| peer_list_open.set(false),
                            show_meeting_info: meeting_info_open(),
                            room_id: id_for_peer_list.clone(),
                            num_participants: num_display_peers,
                            is_active: meeting_joined() && meeting_ended_message().is_none(),
                            on_toggle_meeting_info: move |_| {
                                meeting_info_open.set(!meeting_info_open());
                                if meeting_info_open() {
                                    diagnostics_open.set(false);
                                    device_settings_open.set(false);
                                }
                            },
                            host_display_name: host_display_name.clone(),
                        }
                    }
                }

                // Waiting room controls (host only)
                if is_owner {
                    HostControls {
                        meeting_id: id.clone(),
                        is_admitted: true,
                    }
                }

                // Meeting ended overlay
                if let Some(message) = meeting_ended_message() {
                    MeetingEndedOverlay { message: message }
                }

                // Diagnostics sidebar
                if diagnostics_open() {
                    Diagnostics {
                        is_open: true,
                        on_close: move |_| diagnostics_open.set(false),
                        video_enabled: video_enabled(),
                        mic_enabled: mic_enabled(),
                        share_screen: screen_share_state().is_sharing(),
                    }
                }
            }
        }
    }
}

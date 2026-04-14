// SPDX-License-Identifier: MIT OR Apache-2.0

//! Guest join page — allows unauthenticated users to join a meeting
//! without signing in with an OAuth provider. The guest only needs
//! to provide a display name.

use crate::components::attendants::AttendantsComponent;
use crate::components::browser_compatibility::BrowserCompatibility;
use crate::components::waiting_room::WaitingRoom;
use crate::constants::{
    actix_websocket_base, e2ee_enabled, webtransport_enabled, webtransport_host_base,
};
use crate::context::{
    resolve_transport_config, save_display_name_to_storage, validate_display_name, DisplayNameCtx,
    TransportPreferenceCtx, DISPLAY_NAME_MAX_LEN,
};
use crate::meeting_api::{join_meeting_as_guest, JoinMeetingResponse};
use dioxus::prelude::*;
use videocall_client::Callback as VcCallback;
use videocall_client::{VideoCallClient, VideoCallClientOptions};

const TEXT_INPUT_CLASSES: &str = "input-apple";

/// Guest participant status — mirrors the authenticated MeetingStatus but
/// scoped to guest-specific transitions.
#[derive(Clone, PartialEq, Debug)]
enum GuestStatus {
    NotJoined,
    Joining,
    WaitingForMeeting {
        observer_token: String,
    },
    Waiting {
        observer_token: String,
    },
    Admitted {
        host_display_name: Option<String>,
        host_user_id: Option<String>,
        room_token: String,
        waiting_room_enabled: bool,
        admitted_can_admit: bool,
        allow_guests: bool,
    },
    Rejected,
    Error(String),
}

#[component]
pub fn GuestJoinPage(id: String) -> Element {
    let mut display_name_ctx = use_context::<DisplayNameCtx>();
    let mut guest_status = use_signal(|| GuestStatus::NotJoined);
    let mut host_display_name = use_signal(|| None::<String>);
    let mut host_user_id = use_signal(|| None::<String>);
    let mut current_user_id = use_signal(|| None::<String>);
    let mut input_value = use_signal(String::new);
    let mut input_error = use_signal(|| None::<String>);
    let mut observer_token_signal = use_signal(|| None::<String>);
    let mut came_from_waiting_room = use_signal(|| false);

    // When WaitingForMeeting, create an observer WebSocket client that receives
    // a push notification when the host activates the meeting, matching the
    // authenticated flow in meeting.rs.
    let mut observer_client = use_signal(|| None::<VideoCallClient>);
    let transport_pref_ctx = use_context::<TransportPreferenceCtx>();
    {
        let meeting_id = id.clone();
        use_effect(move || {
            let observer_token = match observer_token_signal() {
                Some(t) if !t.is_empty() => t,
                _ => {
                    if let Some(client) = observer_client.write().take() {
                        let _ = client.disconnect();
                    }
                    return;
                }
            };

            let meeting_id = meeting_id.clone();
            let display_name = input_value();

            let lobby_url = |base: &str| format!("{base}/lobby?token={observer_token}");
            let websocket_urls: Vec<String> = actix_websocket_base()
                .unwrap_or_default()
                .split(',')
                .map(&lobby_url)
                .collect();
            let webtransport_urls: Vec<String> = webtransport_host_base()
                .unwrap_or_default()
                .split(',')
                .map(&lobby_url)
                .collect();

            let (effective_wt_enabled, websocket_urls, webtransport_urls) =
                resolve_transport_config(
                    (transport_pref_ctx.0)(),
                    webtransport_enabled().unwrap_or(false),
                    websocket_urls,
                    webtransport_urls,
                );

            let user_id_for_client = current_user_id().unwrap_or_else(|| display_name.clone());

            let opts = VideoCallClientOptions {
                user_id: user_id_for_client,
                display_name: display_name.clone(),
                meeting_id: meeting_id.clone(),
                websocket_urls,
                webtransport_urls,
                enable_e2ee: false,
                enable_webtransport: effective_wt_enabled,
                on_connected: VcCallback::from(move |_| {
                    log::info!("Guest observer connection established (waiting for meeting)");
                }),
                on_connection_lost: VcCallback::from(move |_| {
                    log::warn!("Guest observer connection lost (waiting for meeting)");
                }),
                on_peer_added: VcCallback::noop(),
                on_peer_first_frame: VcCallback::noop(),
                on_peer_removed: None,
                get_peer_video_canvas_id: VcCallback::from(|id| id),
                get_peer_screen_canvas_id: VcCallback::from(|id| id),
                enable_diagnostics: false,
                diagnostics_update_interval_ms: None,
                enable_health_reporting: false,
                health_reporting_interval_ms: None,
                on_encoder_settings_update: None,
                rtt_testing_period_ms: 3000,
                rtt_probe_interval_ms: None,
                on_meeting_info: None,
                on_meeting_ended: None,
                on_meeting_activated: Some(VcCallback::from({
                    let meeting_id = meeting_id.clone();
                    move |_| {
                        log::info!("Guest: Meeting activated push received, re-joining...");
                        let meeting_id = meeting_id.clone();
                        let display_name = display_name.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            observer_token_signal.set(None);

                            match join_meeting_as_guest(&meeting_id, &display_name).await {
                                Ok(response) => {
                                    current_user_id.set(Some(response.user_id.clone()));
                                    let determined_host = response.host_display_name.clone();
                                    let determined_host_uid = response.host_user_id.clone();
                                    let wr_enabled = response.waiting_room_enabled.unwrap_or(true);
                                    let aca = response.admitted_can_admit.unwrap_or(false);
                                    let ag = response.allow_guests.unwrap_or(false);
                                    host_display_name.set(determined_host.clone());
                                    host_user_id.set(determined_host_uid.clone());
                                    match response.status.as_str() {
                                        "admitted" => {
                                            if let Some(token) = response.room_token {
                                                guest_status.set(GuestStatus::Admitted {
                                                    host_display_name: determined_host,
                                                    host_user_id: determined_host_uid,
                                                    room_token: token,
                                                    waiting_room_enabled: wr_enabled,
                                                    admitted_can_admit: aca,
                                                    allow_guests: ag,
                                                });
                                            } else {
                                                guest_status.set(GuestStatus::Error(
                                                    "Admitted but no room token".to_string(),
                                                ));
                                            }
                                        }
                                        "waiting" => {
                                            let obs_token =
                                                response.observer_token.unwrap_or_default();
                                            came_from_waiting_room.set(true);
                                            observer_token_signal.set(Some(obs_token.clone()));
                                            guest_status.set(GuestStatus::Waiting {
                                                observer_token: obs_token,
                                            });
                                        }
                                        "rejected" => {
                                            guest_status.set(GuestStatus::Rejected);
                                        }
                                        _ => {
                                            guest_status.set(GuestStatus::Error(format!(
                                                "Unknown status: {}",
                                                response.status
                                            )));
                                        }
                                    }
                                }
                                Err(e) => {
                                    guest_status.set(GuestStatus::Error(e.to_string()));
                                }
                            }
                        });
                    }
                })),
                on_participant_admitted: None,
                on_participant_rejected: None,
                on_waiting_room_updated: None,
                on_speaking_changed: None,
                on_audio_level_changed: None,
                vad_threshold: None,
                on_peer_left: None,
                on_peer_joined: None,
                on_display_name_changed: None,
                decode_media: false,
            };

            let mut client = VideoCallClient::new(opts);
            if let Err(e) = client.connect() {
                log::error!("Failed to connect guest observer client: {e}");
            }
            observer_client.set(Some(client));
        });
    }

    // Join as guest handler
    let on_join_guest = {
        let meeting_id = id.clone();
        move || {
            let meeting_id = meeting_id.clone();
            let display_name = input_value();
            guest_status.set(GuestStatus::Joining);

            wasm_bindgen_futures::spawn_local(async move {
                match join_meeting_as_guest(&meeting_id, &display_name).await {
                    Ok(response) => {
                        let effective_user_id = response.user_id.clone();
                        current_user_id.set(Some(effective_user_id));

                        let determined_host = response.host_display_name.clone();
                        let determined_host_uid = response.host_user_id.clone();
                        let wr_enabled = response.waiting_room_enabled.unwrap_or(true);
                        let aca = response.admitted_can_admit.unwrap_or(false);
                        let ag = response.allow_guests.unwrap_or(false);
                        host_display_name.set(determined_host.clone());
                        host_user_id.set(determined_host_uid.clone());

                        match response.status.as_str() {
                            "admitted" => {
                                observer_token_signal.set(None);
                                if let Some(token) = response.room_token {
                                    guest_status.set(GuestStatus::Admitted {
                                        host_display_name: determined_host,
                                        host_user_id: determined_host_uid,
                                        room_token: token,
                                        waiting_room_enabled: wr_enabled,
                                        admitted_can_admit: aca,
                                        allow_guests: ag,
                                    });
                                } else {
                                    guest_status.set(GuestStatus::Error(
                                        "Admitted but no room token".to_string(),
                                    ));
                                }
                            }
                            "waiting_for_meeting" => {
                                let obs_token = response.observer_token.unwrap_or_default();
                                observer_token_signal.set(Some(obs_token.clone()));
                                guest_status.set(GuestStatus::WaitingForMeeting {
                                    observer_token: obs_token,
                                });
                            }
                            "waiting" => {
                                let obs_token = response.observer_token.unwrap_or_default();
                                observer_token_signal.set(Some(obs_token.clone()));
                                came_from_waiting_room.set(true);
                                guest_status.set(GuestStatus::Waiting {
                                    observer_token: obs_token,
                                });
                            }
                            "rejected" => {
                                observer_token_signal.set(None);
                                guest_status.set(GuestStatus::Rejected);
                            }
                            _ => {
                                observer_token_signal.set(None);
                                guest_status.set(GuestStatus::Error(format!(
                                    "Unknown status: {}",
                                    response.status
                                )));
                            }
                        }
                    }
                    Err(e) => {
                        observer_token_signal.set(None);
                        let msg = e.to_string();
                        if msg.contains("GUESTS_NOT_ALLOWED") {
                            guest_status
                                .set(GuestStatus::Error("Guests are not allowed in this meeting. The meeting host must enable guest access.".to_string()));
                        } else {
                            guest_status.set(GuestStatus::Error(msg));
                        }
                    }
                }
            });
        }
    };

    // Handle waiting room admission
    let on_admitted = {
        move |status: JoinMeetingResponse| {
            let determined_host = status.host_display_name.clone();
            let determined_host_uid = status.host_user_id.clone();
            let wr_enabled = status.waiting_room_enabled.unwrap_or(true);
            let aca = status.admitted_can_admit.unwrap_or(false);
            let ag = status.allow_guests.unwrap_or(false);
            let token = status.room_token.unwrap_or_default();
            host_display_name.set(determined_host.clone());
            host_user_id.set(determined_host_uid.clone());
            observer_token_signal.set(None);
            guest_status.set(GuestStatus::Admitted {
                host_display_name: determined_host,
                host_user_id: determined_host_uid,
                room_token: token,
                waiting_room_enabled: wr_enabled,
                admitted_can_admit: aca,
                allow_guests: ag,
            });
        }
    };

    let on_rejected = move |_| {
        observer_token_signal.set(None);
        crate::auth::clear_guest_session_id();
        guest_status.set(GuestStatus::Rejected);
    };

    let on_cancel_waiting = {
        let meeting_id = id.clone();
        move |_| {
            let meeting_id = meeting_id.clone();
            let token = observer_token_signal().unwrap_or_default();
            wasm_bindgen_futures::spawn_local(async move {
                let _ = crate::meeting_api::leave_meeting_as_guest(&meeting_id, &token).await;
                if let Some(w) = web_sys::window() {
                    let _ = w.location().set_href("/");
                }
            });
        }
    };

    let current_guest_status = guest_status();
    let should_auto_join = came_from_waiting_room();
    let display_name_for_render = input_value();

    rsx! {
        match &current_guest_status {
            // Admitted — show the meeting
            GuestStatus::Admitted { host_display_name, host_user_id, room_token, waiting_room_enabled, admitted_can_admit, allow_guests } => rsx! {
                AttendantsComponent {
                    display_name: display_name_for_render.clone(),
                    id: id.clone(),
                    e2ee_enabled: e2ee_enabled().unwrap_or(false),
                    user_id: current_user_id(),
                    host_display_name: host_display_name.clone(),
                    host_user_id: host_user_id.clone(),
                    auto_join: should_auto_join,
                    is_owner: false,
                    is_guest: true,
                    room_token: room_token.clone(),
                    waiting_room_enabled: *waiting_room_enabled,
                    admitted_can_admit: *admitted_can_admit,
                    allow_guests: *allow_guests,
                }
            },

            // Waiting room
            GuestStatus::Waiting { observer_token } => rsx! {
                WaitingRoom {
                    meeting_id: id.clone(),
                    user_id: current_user_id().unwrap_or_default(),
                    display_name: display_name_for_render.clone(),
                    observer_token: observer_token.clone(),
                    is_guest: true,
                    on_admitted: on_admitted,
                    on_rejected: on_rejected,
                    on_cancel: on_cancel_waiting,
                }
            },

            // Waiting for host to start
            GuestStatus::WaitingForMeeting { .. } => rsx! {
                div { class: "waiting-room-container",
                    div { class: "waiting-room-card card-apple",
                        div { class: "waiting-room-icon",
                            div { class: "loading-spinner", style: "width: 48px; height: 48px;" }
                        }
                        h2 { "Waiting for meeting to start" }
                        p { class: "waiting-room-message",
                            "The host hasn't started this meeting yet. You'll automatically join once the meeting begins."
                        }
                        button {
                            class: "btn-apple btn-secondary",
                            onclick: move |_| {
                                if let Some(w) = web_sys::window() {
                                    let _ = w.location().set_href("/");
                                }
                            },
                            "Leave"
                        }
                    }
                }
            },

            // Rejected
            GuestStatus::Rejected => rsx! {
                div { class: "rejected-container",
                    div { class: "rejected-card card-apple",
                        svg { xmlns: "http://www.w3.org/2000/svg", width: "64", height: "64", view_box: "0 0 24 24", fill: "none", stroke: "#ff6b6b", stroke_width: "1.5",
                            circle { cx: "12", cy: "12", r: "10" }
                            line { x1: "15", y1: "9", x2: "9", y2: "15" }
                            line { x1: "9", y1: "9", x2: "15", y2: "15" }
                        }
                        h2 { "Entry denied" }
                        p { "The meeting host has denied your request to join." }
                        button {
                            class: "btn-apple btn-primary",
                            onclick: move |_| {
                                if let Some(w) = web_sys::window() { let _ = w.location().set_href("/"); }
                            },
                            "Return to Home"
                        }
                    }
                }
            },

            // Error
            GuestStatus::Error(error) => rsx! {
                div { class: "error-container",
                    div { class: "error-card card-apple",
                        svg { xmlns: "http://www.w3.org/2000/svg", width: "64", height: "64", view_box: "0 0 24 24", fill: "none", stroke: "#ff9800", stroke_width: "1.5",
                            circle { cx: "12", cy: "12", r: "10" }
                            line { x1: "12", y1: "8", x2: "12", y2: "12" }
                            line { x1: "12", y1: "16", x2: "12.01", y2: "16" }
                        }
                        h2 { "Unable to join" }
                        p { "{error}" }
                        button {
                            class: "btn-apple btn-primary",
                            onclick: move |_| {
                                if let Some(w) = web_sys::window() { let _ = w.location().set_href("/"); }
                            },
                            "Return to Home"
                        }
                    }
                }
            },

            // Joining in progress
            GuestStatus::Joining => {
                let name = input_value();
                rsx! {
                    div { style: "position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000;",
                        div { class: "loading-spinner", style: "width: 40px; height: 40px; margin-bottom: 1rem;" }
                        p { style: "color: white; font-size: 1rem;",
                            "Joining as guest: "
                            strong { "{name}" }
                            "..."
                        }
                    }
                }
            },

            // Not yet joined — show the guest join form
            GuestStatus::NotJoined => {
                let mut on_join = on_join_guest.clone();
                rsx! {
                    div { class: "hero-container",
                        BrowserCompatibility {}
                        div { class: "floating-element floating-element-1" }
                        div { class: "floating-element floating-element-2" }
                        div { class: "floating-element floating-element-3" }
                        div { class: "hero-content",
                            h1 { class: "hero-title text-center", "Join as Guest" }
                            div { class: "content-separator" }
                            div { class: "w-full mb-8 card-apple p-8",
                                form {
                                    onsubmit: move |e| {
                                        e.prevent_default();
                                        input_error.set(None);
                                        let raw = input_value();
                                        match validate_display_name(&raw) {
                                            Ok(valid_name) => {
                                                input_value.set(valid_name.clone());
                                                save_display_name_to_storage(&valid_name);
                                                display_name_ctx.0.set(Some(valid_name));
                                                on_join();
                                            }
                                            Err(msg) => {
                                                input_error.set(Some(msg));
                                            }
                                        }
                                    },
                                    h3 { class: "text-center text-xl font-semibold mb-6 text-white/90",
                                        "Join Meeting as Guest"
                                    }
                                    div { class: "space-y-6",
                                        div {
                                            label {
                                                class: "block text-white/80 text-sm font-medium mb-2 ml-1",
                                                "Meeting ID"
                                            }
                                            div {
                                                class: "input-apple",
                                                style: "opacity: 0.7; cursor: default; user-select: all; display: flex; align-items: center;",
                                                "{id}"
                                            }
                                        }
                                        div {
                                            label {
                                                r#for: "guest-name",
                                                class: "block text-white/80 text-sm font-medium mb-2 ml-1",
                                                "Your Name"
                                            }
                                            input {
                                                id: "guest-name",
                                                class: TEXT_INPUT_CLASSES,
                                                r#type: "text",
                                                placeholder: "Enter your display name",
                                                required: true,
                                                autofocus: true,
                                                maxlength: DISPLAY_NAME_MAX_LEN as i64,
                                                value: "{input_value}",
                                                oninput: move |e: Event<FormData>| {
                                                    input_value.set(e.value());
                                                    input_error.set(None);
                                                },
                                            }
                                            p { class: "text-sm text-foreground-subtle mt-2 ml-1",
                                                "Allowed: letters, numbers, spaces, hyphens, underscores, apostrophes"
                                            }
                                            if let Some(err) = input_error() {
                                                p {
                                                    class: "text-sm mt-2 ml-1",
                                                    style: "color: #ff6b6b;",
                                                    "{err}"
                                                }
                                            }
                                        }
                                        div { class: "mt-4",
                                            button {
                                                r#type: "submit",
                                                class: "btn-apple btn-primary w-full",
                                                disabled: input_value().trim().is_empty(),
                                                span { class: "text-lg", "Join as Guest" }
                                            }
                                        }
                                        p { class: "text-sm text-foreground-subtle text-center mt-4",
                                            "You are joining without an account. "
                                            "Some features may be limited."
                                        }
                                    }
                                }
                            }
                            div { class: "content-separator" }
                        }
                    }
                }
            },
        }
    }
}

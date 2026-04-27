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
    TransportPreference, TransportPreferenceCtx, DISPLAY_NAME_MAX_LEN,
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
        status_observer_token: String,
        waiting_room_enabled: bool,
        admitted_can_admit: bool,
        allow_guests: bool,
    },
    Rejected,
    Error(String),
}

// ---------------------------------------------------------------------------
// Extracted helpers
// ---------------------------------------------------------------------------

/// Map a join/status API response to a [`GuestStatus`] variant.
fn guest_status_from_join_response(
    response: &JoinMeetingResponse,
    fallback_status_observer_token: Option<String>,
) -> GuestStatus {
    let host_display_name = response.host_display_name.clone();
    let host_user_id = response.host_user_id.clone();
    let wr_enabled = response.waiting_room_enabled.unwrap_or(true);
    let aca = response.admitted_can_admit.unwrap_or(false);
    let ag = response.allow_guests.unwrap_or(false);

    match response.status.as_str() {
        "admitted" => {
            if let Some(token) = response.room_token.clone() {
                GuestStatus::Admitted {
                    host_display_name,
                    host_user_id,
                    room_token: token,
                    status_observer_token: resolve_status_observer_token(
                        response.observer_token.clone(),
                        fallback_status_observer_token,
                    ),
                    waiting_room_enabled: wr_enabled,
                    admitted_can_admit: aca,
                    allow_guests: ag,
                }
            } else {
                GuestStatus::Error("Admitted but no room token".to_string())
            }
        }
        "waiting_for_meeting" => GuestStatus::WaitingForMeeting {
            observer_token: response.observer_token.clone().unwrap_or_default(),
        },
        "waiting" => GuestStatus::Waiting {
            observer_token: response.observer_token.clone().unwrap_or_default(),
        },
        "rejected" => GuestStatus::Rejected,
        _ => GuestStatus::Error(format!("Unknown status: {}", response.status)),
    }
}

fn resolve_status_observer_token(
    observer_token: Option<String>,
    fallback_status_observer_token: Option<String>,
) -> String {
    observer_token
        .filter(|t| !t.is_empty())
        .or_else(|| fallback_status_observer_token.filter(|t| !t.is_empty()))
        .unwrap_or_default()
}

/// Apply a [`JoinMeetingResponse`] that is already known to represent admission
/// (e.g. from the waiting-room push). Sets host metadata signals and transitions
/// to [`GuestStatus::Admitted`].
fn handle_admitted(
    response: JoinMeetingResponse,
    fallback_status_observer_token: String,
    mut guest_status: Signal<GuestStatus>,
    mut host_display_name: Signal<Option<String>>,
    mut host_user_id: Signal<Option<String>>,
    mut observer_token_signal: Signal<Option<String>>,
) {
    let determined_host = response.host_display_name.clone();
    let determined_host_uid = response.host_user_id.clone();
    let wr_enabled = response.waiting_room_enabled.unwrap_or(true);
    let aca = response.admitted_can_admit.unwrap_or(false);
    let ag = response.allow_guests.unwrap_or(false);
    let token = response.room_token.unwrap_or_default();
    let status_observer_token = resolve_status_observer_token(
        response.observer_token,
        Some(fallback_status_observer_token),
    );
    host_display_name.set(determined_host.clone());
    host_user_id.set(determined_host_uid.clone());
    observer_token_signal.set(None);
    guest_status.set(GuestStatus::Admitted {
        host_display_name: determined_host,
        host_user_id: determined_host_uid,
        room_token: token,
        status_observer_token,
        waiting_room_enabled: wr_enabled,
        admitted_can_admit: aca,
        allow_guests: ag,
    });
}

/// Build and connect the observer [`VideoCallClient`] used while waiting for
/// the host to activate the meeting. Factored out of the component
/// `use_effect` to keep the reactive hook body minimal.
#[allow(clippy::too_many_arguments)]
fn start_observer_connection(
    observer_token: String,
    meeting_id: String,
    display_name: String,
    user_id: String,
    transport_pref: TransportPreference,
    mut observer_client: Signal<Option<VideoCallClient>>,
    mut guest_status: Signal<GuestStatus>,
    mut current_user_id: Signal<Option<String>>,
    mut host_display_name: Signal<Option<String>>,
    mut host_user_id: Signal<Option<String>>,
    mut observer_token_signal: Signal<Option<String>>,
    mut came_from_waiting_room: Signal<bool>,
) {
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

    let (effective_wt_enabled, websocket_urls, webtransport_urls) = resolve_transport_config(
        transport_pref,
        webtransport_enabled().unwrap_or(false),
        websocket_urls,
        webtransport_urls,
    );

    let opts = VideoCallClientOptions {
        user_id,
        display_name: display_name.clone(),
        is_guest: true,
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
            let fallback_status_observer_token = observer_token.clone();
            move |_| {
                log::info!("Guest: Meeting activated push received, re-joining...");
                let meeting_id = meeting_id.clone();
                let display_name = display_name.clone();
                let fallback_status_observer_token = fallback_status_observer_token.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    observer_token_signal.set(None);

                    match join_meeting_as_guest(&meeting_id, &display_name).await {
                        Ok(response) => {
                            current_user_id.set(Some(response.user_id.clone()));
                            host_display_name.set(response.host_display_name.clone());
                            host_user_id.set(response.host_user_id.clone());
                            let status = guest_status_from_join_response(
                                &response,
                                Some(fallback_status_observer_token),
                            );
                            if matches!(&status, GuestStatus::Waiting { .. }) {
                                came_from_waiting_room.set(true);
                            }
                            guest_status.set(status);
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
        on_meeting_settings_updated: None,
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
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

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

            let user_id_for_client =
                current_user_id().unwrap_or_else(|| input_value.read().clone());

            start_observer_connection(
                observer_token,
                meeting_id.clone(),
                input_value(),
                user_id_for_client,
                (transport_pref_ctx.0)(),
                observer_client,
                guest_status,
                current_user_id,
                host_display_name,
                host_user_id,
                observer_token_signal,
                came_from_waiting_room,
            );
        });
    }

    // Join as guest handler
    let on_join_guest = {
        let meeting_id = id.clone();
        move || {
            let meeting_id = meeting_id.clone();
            let display_name = input_value();
            let fallback_status_observer_token = match guest_status() {
                GuestStatus::Waiting { observer_token }
                | GuestStatus::WaitingForMeeting { observer_token } => Some(observer_token),
                GuestStatus::Admitted {
                    status_observer_token,
                    ..
                } => Some(status_observer_token),
                _ => None,
            };
            guest_status.set(GuestStatus::Joining);

            wasm_bindgen_futures::spawn_local(async move {
                match join_meeting_as_guest(&meeting_id, &display_name).await {
                    Ok(response) => {
                        current_user_id.set(Some(response.user_id.clone()));
                        host_display_name.set(response.host_display_name.clone());
                        host_user_id.set(response.host_user_id.clone());
                        let status = guest_status_from_join_response(
                            &response,
                            fallback_status_observer_token,
                        );
                        match &status {
                            GuestStatus::WaitingForMeeting { observer_token } => {
                                observer_token_signal.set(Some(observer_token.clone()));
                            }
                            GuestStatus::Waiting { .. } => {
                                observer_token_signal.set(None);
                                came_from_waiting_room.set(true);
                            }
                            _ => {
                                observer_token_signal.set(None);
                            }
                        }
                        guest_status.set(status);
                    }
                    Err(e) => {
                        observer_token_signal.set(None);
                        guest_status.set(GuestStatus::Error(e.to_string()));
                    }
                }
            });
        }
    };

    // Handle waiting room admission
    let on_admitted = {
        move |status: JoinMeetingResponse| {
            let fallback_status_observer_token = match guest_status() {
                GuestStatus::Waiting { observer_token }
                | GuestStatus::WaitingForMeeting { observer_token } => observer_token,
                GuestStatus::Admitted {
                    status_observer_token,
                    ..
                } => status_observer_token,
                _ => String::new(),
            };
            handle_admitted(
                status,
                fallback_status_observer_token,
                guest_status,
                host_display_name,
                host_user_id,
                observer_token_signal,
            );
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
            GuestStatus::Admitted { host_display_name, host_user_id, room_token, status_observer_token, waiting_room_enabled, admitted_can_admit, allow_guests } => rsx! {
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
                    status_observer_token: status_observer_token.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_response(
        status: &str,
        room_token: Option<&str>,
        observer_token: Option<&str>,
    ) -> JoinMeetingResponse {
        JoinMeetingResponse {
            user_id: "guest-1".to_string(),
            display_name: Some("Guest".to_string()),
            status: status.to_string(),
            is_host: false,
            is_guest: true,
            joined_at: 0,
            admitted_at: None,
            room_token: room_token.map(ToString::to_string),
            observer_token: observer_token.map(ToString::to_string),
            waiting_room_enabled: Some(true),
            admitted_can_admit: Some(false),
            end_on_host_leave: Some(true),
            host_display_name: Some("Host".to_string()),
            host_user_id: Some("host-1".to_string()),
            allow_guests: Some(true),
        }
    }

    #[test]
    fn admitted_mapping_uses_fallback_when_response_observer_missing() {
        let response = make_response("admitted", Some("room-token"), None);

        let status = guest_status_from_join_response(&response, Some("fallback-token".to_string()));

        match status {
            GuestStatus::Admitted {
                status_observer_token,
                ..
            } => {
                assert_eq!(status_observer_token, "fallback-token");
            }
            other => panic!("expected Admitted, got {other:?}"),
        }
    }

    #[test]
    fn admitted_mapping_uses_fallback_when_response_observer_empty() {
        let response = make_response("admitted", Some("room-token"), Some(""));

        let status = guest_status_from_join_response(&response, Some("fallback-token".to_string()));

        match status {
            GuestStatus::Admitted {
                status_observer_token,
                ..
            } => {
                assert_eq!(status_observer_token, "fallback-token");
            }
            other => panic!("expected Admitted, got {other:?}"),
        }
    }

    #[test]
    fn admitted_mapping_prefers_non_empty_response_observer_token() {
        let response = make_response("admitted", Some("room-token"), Some("response-token"));

        let status = guest_status_from_join_response(&response, Some("fallback-token".to_string()));

        match status {
            GuestStatus::Admitted {
                status_observer_token,
                ..
            } => {
                assert_eq!(status_observer_token, "response-token");
            }
            other => panic!("expected Admitted, got {other:?}"),
        }
    }
}

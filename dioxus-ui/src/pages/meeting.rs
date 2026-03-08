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
 */

use crate::components::attendants::AttendantsComponent;
use crate::components::waiting_room::WaitingRoom;
use crate::constants::{
    actix_websocket_base, e2ee_enabled, oauth_enabled, webtransport_enabled,
    webtransport_host_base,
};
use crate::context::{get_or_create_local_user_id, load_display_name_from_storage, save_display_name_to_storage, validate_display_name, DisplayNameCtx};
use crate::meeting_api::{join_meeting, JoinError, JoinMeetingResponse};
use dioxus::prelude::*;
use videocall_client::Callback as VcCallback;
use videocall_client::{VideoCallClient, VideoCallClientOptions};
use web_sys::window;

use crate::auth::{check_session, get_user_profile, logout, UserProfile};
use crate::routing::Route;

/// Meeting participant status from the API
#[derive(Clone, PartialEq, Debug)]
pub enum MeetingStatus {
    NotJoined,
    Joining,
    WaitingForMeeting {
        observer_token: String,
    },
    Waiting {
        observer_token: String,
    },
    Admitted {
        is_host: bool,
        host_display_name: Option<String>,
        room_token: String,
        waiting_room_enabled: bool,
    },
    Rejected,
    Error(String),
}

#[component]
pub fn MeetingPage(id: String) -> Element {
    let mut display_name_ctx = use_context::<DisplayNameCtx>();
    let mut auth_checked = use_signal(|| false);
    let navigator = use_navigator();
    let mut user_profile = use_signal(|| None::<UserProfile>);
    let mut meeting_status = use_signal(|| MeetingStatus::NotJoined);
    let mut host_display_name = use_signal(|| None::<String>);
    let mut current_user_id = use_signal(|| None::<String>);
    let mut came_from_waiting_room = use_signal(|| false);

    // Separate signal that tracks only the observer token for the WaitingForMeeting
    // state. The observer `use_effect` subscribes to THIS signal instead of
    // `meeting_status`, breaking the circular dependency that caused a
    // `RefCell already borrowed` panic in dioxus-core when `on_meeting_activated`
    // set `meeting_status` and the effect tried to re-run synchronously.
    let mut observer_token_signal = use_signal(|| None::<String>);

    let initial_username: String = if let Some(name) = (display_name_ctx.0)() {
        name
    } else {
        load_display_name_from_storage().unwrap_or_default()
    };
    let mut input_value_state = use_signal(|| initial_username);

    // Auth check effect
    use_effect(move || {
        if oauth_enabled().unwrap_or(false) {
            wasm_bindgen_futures::spawn_local(async move {
                match check_session().await {
                    Ok(_) => auth_checked.set(true),
                    Err(_) => {
                        if let Some(win) = window() {
                            if let Ok(current_url) = win.location().href() {
                                // Store the return URL in sessionStorage before
                                // navigating to /login. Dioxus 0.7's router strips
                                // unrecognized query params via history.replaceState,
                                // so we cannot rely on ?returnTo= surviving in the URL.
                                match win.session_storage() {
                                    Ok(Some(storage)) => {
                                        if storage
                                            .set_item("vc_oauth_return_to", &current_url)
                                            .is_err()
                                        {
                                            log::warn!("Failed to write vc_oauth_return_to to sessionStorage — post-login redirect will fall back to app root");
                                        }
                                    }
                                    _ => {
                                        log::warn!("sessionStorage unavailable — post-login redirect will fall back to app root");
                                    }
                                }
                                let _ = win.location().set_href("/login");
                            }
                        }
                    }
                }
            });
        } else {
            auth_checked.set(true);
        }
    });

    // Fetch user profile
    use_effect(move || {
        let auth_done = auth_checked();
        if auth_done && oauth_enabled().unwrap_or(false) {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(profile) = get_user_profile().await {
                    user_profile.set(Some(profile));
                }
            });
        }
    });

    // When WaitingForMeeting, create an observer WebSocket client that receives
    // a push notification when the host activates the meeting, replacing the
    // old 2-second polling loop.
    //
    // IMPORTANT: This effect subscribes to `observer_token_signal` (NOT
    // `meeting_status`) to avoid a circular reactive dependency. The
    // `on_meeting_activated` callback sets `meeting_status` to Admitted (or
    // another variant) and clears `observer_token_signal` to None. Because the
    // effect only watches `observer_token_signal`, writing to `meeting_status`
    // does not cause re-entrant execution of this effect.
    let mut observer_client = use_signal(|| None::<VideoCallClient>);
    {
        let meeting_id = id.clone();
        use_effect(move || {
            let observer_token = match observer_token_signal() {
                Some(t) if !t.is_empty() => t,
                _ => {
                    // Clean up observer client when leaving WaitingForMeeting
                    if let Some(client) = observer_client.write().take() {
                        let _ = client.disconnect();
                    }
                    return;
                }
            };

            let meeting_id = meeting_id.clone();
            let display_name = input_value_state();

            // Build observer lobby URLs using the observer token
            let lobby_url =
                |base: &str| format!("{base}/lobby?token={observer_token}");
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

            // Use the user's ID so the server can match
            // push-notification `target_user_id` to this observer client.
            let user_id_for_client = current_user_id()
                .unwrap_or_else(|| display_name.clone());

            let opts = VideoCallClientOptions {
                user_id: user_id_for_client,
                meeting_id: meeting_id.clone(),
                websocket_urls,
                webtransport_urls,
                enable_e2ee: false,
                enable_webtransport: webtransport_enabled().unwrap_or(false),
                on_connected: VcCallback::from(move |_| {
                    log::info!("Observer connection established (waiting for meeting)");
                }),
                on_connection_lost: VcCallback::from(move |_| {
                    log::warn!("Observer connection lost (waiting for meeting)");
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
                        log::info!("Meeting activated push received, re-joining...");
                        let meeting_id = meeting_id.clone();
                        let display_name = display_name.clone();
                        // Use spawn_local instead of dioxus::spawn because
                        // this callback fires from a WebSocket message
                        // handler (Inner::on_inbound_media) which runs
                        // outside any Dioxus runtime context. Calling
                        // dioxus::spawn() here would panic.
                        wasm_bindgen_futures::spawn_local(async move {
                            // Clear the observer token FIRST so the observer
                            // effect tears down the client without re-entering.
                            observer_token_signal.set(None);

                            match join_meeting(&meeting_id, Some(&display_name)).await {
                                Ok(response) => {
                                    let effective_user_id = if response.user_id.is_empty() {
                                        get_or_create_local_user_id()
                                    } else {
                                        response.user_id.clone()
                                    };
                                    current_user_id.set(Some(effective_user_id));
                                    let determined_host = response.host_display_name.clone();
                                    let wr_enabled =
                                        response.waiting_room_enabled.unwrap_or(true);
                                    host_display_name.set(determined_host.clone());
                                    match response.status.as_str() {
                                        "admitted" => {
                                            if let Some(token) = response.room_token {
                                                meeting_status.set(MeetingStatus::Admitted {
                                                    is_host: response.is_host,
                                                    host_display_name: determined_host,
                                                    room_token: token,
                                                    waiting_room_enabled: wr_enabled,
                                                });
                                            } else {
                                                meeting_status.set(MeetingStatus::Error(
                                                    "Admitted but no room token".to_string(),
                                                ));
                                            }
                                        }
                                        "waiting" => {
                                            let obs_token = response
                                                .observer_token
                                                .unwrap_or_default();
                                            came_from_waiting_room.set(true);
                                            // Also set observer_token_signal for
                                            // the Waiting state (observer stays alive).
                                            observer_token_signal
                                                .set(Some(obs_token.clone()));
                                            meeting_status.set(MeetingStatus::Waiting {
                                                observer_token: obs_token,
                                            });
                                        }
                                        "rejected" => {
                                            meeting_status.set(MeetingStatus::Rejected);
                                        }
                                        _ => meeting_status.set(MeetingStatus::Error(
                                            format!(
                                                "Unknown status: {}",
                                                response.status
                                            ),
                                        )),
                                    }
                                }
                                Err(e) => {
                                    meeting_status
                                        .set(MeetingStatus::Error(e.to_string()));
                                }
                            }
                        });
                    }
                })),
                on_participant_admitted: None,
                on_participant_rejected: None,
                on_waiting_room_updated: None,
                on_speaking_changed: None,
                vad_threshold: None,
            };

            let mut client = VideoCallClient::new(opts);
            if let Err(e) = client.connect() {
                log::error!("Failed to connect observer client: {e}");
            }
            observer_client.set(Some(client));
        });
    }

    // Logout handler
    let on_logout = move |_| {
        let navigator = navigator;
        wasm_bindgen_futures::spawn_local(async move {
            let _ = logout().await;
            navigator.push(Route::Login {});
        });
    };

    // Early return for auth check
    if !auth_checked() && oauth_enabled().unwrap_or(false) {
        return rsx! {
            div { style: "position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000;",
                p { style: "color: white; font-size: 1rem;", "Checking authentication..." }
            }
        };
    }

    // Join meeting handler
    let on_join_meeting = {
        let meeting_id = id.clone();
        move || {
            let meeting_id = meeting_id.clone();
            let display_name = input_value_state();
            meeting_status.set(MeetingStatus::Joining);

            wasm_bindgen_futures::spawn_local(async move {
                match join_meeting(&meeting_id, Some(&display_name)).await {
                    Ok(response) => {
                        // Use the API-provided user_id when available;
                        // fall back to a locally-generated stable UUID
                        // so non-OAuth users still get a persistent identity.
                        let effective_user_id = if response.user_id.is_empty() {
                            get_or_create_local_user_id()
                        } else {
                            response.user_id.clone()
                        };
                        current_user_id.set(Some(effective_user_id));
                        let determined_host = if response.is_host {
                            Some(display_name.clone())
                        } else {
                            response.host_display_name.clone()
                        };
                        let wr_enabled = response.waiting_room_enabled.unwrap_or(true);
                        host_display_name.set(determined_host.clone());
                        match response.status.as_str() {
                            "admitted" => {
                                observer_token_signal.set(None);
                                if let Some(token) = response.room_token {
                                    meeting_status.set(MeetingStatus::Admitted {
                                        is_host: response.is_host,
                                        host_display_name: determined_host,
                                        room_token: token,
                                        waiting_room_enabled: wr_enabled,
                                    });
                                } else {
                                    meeting_status.set(MeetingStatus::Error(
                                        "Admitted but no room token".to_string(),
                                    ));
                                }
                            }
                            "waiting_for_meeting" => {
                                let obs_token =
                                    response.observer_token.unwrap_or_default();
                                observer_token_signal
                                    .set(Some(obs_token.clone()));
                                meeting_status.set(MeetingStatus::WaitingForMeeting {
                                    observer_token: obs_token,
                                });
                            }
                            "waiting" => {
                                let obs_token =
                                    response.observer_token.unwrap_or_default();
                                observer_token_signal
                                    .set(Some(obs_token.clone()));
                                came_from_waiting_room.set(true);
                                meeting_status.set(MeetingStatus::Waiting {
                                    observer_token: obs_token,
                                });
                            }
                            "rejected" => {
                                observer_token_signal.set(None);
                                meeting_status.set(MeetingStatus::Rejected);
                            }
                            _ => {
                                observer_token_signal.set(None);
                                meeting_status.set(MeetingStatus::Error(format!(
                                    "Unknown status: {}",
                                    response.status
                                )));
                            }
                        }
                    }
                    Err(JoinError::MeetingNotActive) => {
                        // Fallback for older server versions that still return 400
                        observer_token_signal.set(Some(String::new()));
                        meeting_status.set(MeetingStatus::WaitingForMeeting {
                            observer_token: String::new(),
                        });
                    }
                    Err(e) => {
                        observer_token_signal.set(None);
                        meeting_status.set(MeetingStatus::Error(e.to_string()));
                    }
                }
            });
        }
    };

    // Handle waiting room admission
    let on_admitted = {
        move |status: JoinMeetingResponse| {
            let determined_host = status.host_display_name.clone();
            let wr_enabled = status.waiting_room_enabled.unwrap_or(true);
            let token = status.room_token.unwrap_or_default();
            host_display_name.set(determined_host.clone());
            observer_token_signal.set(None);
            meeting_status.set(MeetingStatus::Admitted {
                is_host: false,
                host_display_name: determined_host,
                room_token: token,
                waiting_room_enabled: wr_enabled,
            });
        }
    };

    let on_rejected = move |_| {
        observer_token_signal.set(None);
        meeting_status.set(MeetingStatus::Rejected);
    };

    let on_cancel_waiting = {
        let meeting_id = id.clone();
        move |_| {
            let meeting_id = meeting_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let _ = crate::meeting_api::leave_meeting(&meeting_id).await;
                if let Some(w) = web_sys::window() {
                    let _ = w.location().set_href("/");
                }
            });
        }
    };

    // Auto-join: when the username is already set and the meeting status is
    // NotJoined, trigger the join flow automatically so the user does not
    // have to interact with a redundant form.
    {
        let mut on_join = on_join_meeting.clone();
        let mut auto_join_attempted = use_signal(|| false);
        use_effect(move || {
            let has_username = (display_name_ctx.0)().is_some();
            let is_not_joined = matches!(meeting_status(), MeetingStatus::NotJoined);
            if has_username && is_not_joined && !auto_join_attempted() {
                auto_join_attempted.set(true);
                on_join();
            }
        });
    }

    let maybe_username = (display_name_ctx.0)();
    let current_meeting_status = meeting_status();
    let should_auto_join = came_from_waiting_room();

    rsx! {
        match (&maybe_username, &current_meeting_status) {
            // User is admitted - show the meeting
            (Some(username), MeetingStatus::Admitted { is_host, host_display_name, room_token, waiting_room_enabled }) => rsx! {
                AttendantsComponent {
                    display_name: username.clone(),
                    id: id.clone(),
                    webtransport_enabled: webtransport_enabled().unwrap_or(false),
                    e2ee_enabled: e2ee_enabled().unwrap_or(false),
                    user_name: user_profile().as_ref().map(|p| p.name.clone()),
                    user_id: current_user_id()
                        .or_else(|| user_profile().as_ref().map(|p| p.user_id.clone()))
                        .or_else(|| Some(get_or_create_local_user_id())),
                    on_logout: Some(EventHandler::new(on_logout)),
                    host_display_name: host_display_name.clone(),
                    auto_join: should_auto_join,
                    is_owner: *is_host,
                    room_token: room_token.clone(),
                    waiting_room_enabled: *waiting_room_enabled,
                }
            },

            // Waiting room
            (Some(_), MeetingStatus::Waiting { observer_token }) => rsx! {
                WaitingRoom {
                    meeting_id: id.clone(),
                    user_id: current_user_id().unwrap_or_default(),
                    observer_token: observer_token.clone(),
                    on_admitted: on_admitted,
                    on_rejected: on_rejected,
                    on_cancel: on_cancel_waiting,
                }
            },

            // Waiting for host to start
            (Some(_), MeetingStatus::WaitingForMeeting { .. }) => rsx! {
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
            (Some(_), MeetingStatus::Rejected) => rsx! {
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
            (Some(_), MeetingStatus::Error(error)) => rsx! {
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
            (Some(_), MeetingStatus::Joining) => {
                let display_name = maybe_username.as_deref().unwrap_or("...");
                rsx! {
                    div { style: "position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000;",
                        div { class: "loading-spinner", style: "width: 40px; height: 40px; margin-bottom: 1rem;" }
                        p { style: "color: white; font-size: 1rem;",
                            "Joining as "
                            strong { "{display_name}" }
                            "..."
                        }
                    }
                }
            },

            // No username set, or waiting for auto-join to fire
            _ => {
                if maybe_username.is_none() {
                    // Show inline display name prompt instead of redirecting
                    rsx! {
                        div {
                            style: "position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000;",
                            div {
                                class: "card-apple p-8",
                                style: "max-width: 400px; width: 90%;",
                                h2 {
                                    style: "color: white; text-align: center; margin-bottom: 0.5rem;",
                                    "Enter your display name"
                                }
                                p {
                                    style: "color: rgba(255,255,255,0.6); text-align: center; font-size: 0.875rem; margin-bottom: 1.5rem;",
                                    "Choose a name to join the meeting"
                                }
                                form {
                                    onsubmit: move |e| {
                                        e.prevent_default();
                                        let raw = input_value_state();
                                        match validate_display_name(&raw) {
                                            Ok(valid_name) => {
                                                save_display_name_to_storage(&valid_name);
                                                (display_name_ctx.0).set(Some(valid_name));
                                            }
                                            Err(msg) => {
                                                if let Some(w) = web_sys::window() {
                                                    let _ = w.alert_with_message(&msg);
                                                }
                                            }
                                        }
                                    },
                                    input {
                                        class: "input-apple",
                                        r#type: "text",
                                        placeholder: "Enter your display name",
                                        required: true,
                                        autofocus: true,
                                        value: "{input_value_state}",
                                        oninput: move |e: Event<FormData>| {
                                            input_value_state.set(e.value());
                                        },
                                    }
                                    button {
                                        r#type: "submit",
                                        class: "btn-apple btn-primary w-full",
                                        style: "margin-top: 1rem;",
                                        "Join Meeting"
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // Username is set; the auto-join effect will fire momentarily
                    let display_name = maybe_username.as_deref().unwrap_or("Unknown");
                    rsx! {
                        div { style: "position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000;",
                            div { class: "loading-spinner", style: "width: 40px; height: 40px; margin-bottom: 1rem;" }
                            p { style: "color: white; font-size: 1rem;",
                                "Joining as "
                                strong { "{display_name}" }
                                "..."
                            }
                        }
                    }
                }
            }
        }
    }
}

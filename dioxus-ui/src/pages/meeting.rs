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

use crate::auth::{check_session, get_user_profile, logout, UserProfile};
use crate::components::attendants::AttendantsComponent;
use crate::components::waiting_room::WaitingRoom;
use crate::constants::{e2ee_enabled, oauth_enabled, webtransport_enabled};
use crate::context::{
    is_valid_username, load_username_from_storage, save_username_to_storage, UsernameCtx,
};
use crate::meeting_api::{get_meeting_info, join_meeting, JoinError};
use crate::routing::Route;
use dioxus::prelude::*;
use gloo_timers::callback::Interval;
use std::cell::RefCell;
use std::rc::Rc;

/// Meeting participant status from the API
#[derive(Clone, PartialEq, Debug)]
pub enum MeetingStatus {
    /// Initial state - haven't joined yet
    NotJoined,
    /// Joining in progress
    Joining,
    /// Waiting for the host to start the meeting
    WaitingForMeeting,
    /// In the waiting room, pending host admission
    Waiting,
    /// Admitted to the meeting
    Admitted {
        is_host: bool,
        host_display_name: Option<String>,
        /// Signed JWT room access token for connecting to the media server
        room_token: String,
    },
    /// Rejected by the host
    Rejected,
    /// Error occurred
    Error(String),
}

#[component]
pub fn MeetingPage(id: String) -> Element {
    let nav = navigator();
    let username_ctx: Option<Signal<Option<String>>> = try_use_context::<UsernameCtx>();

    // State
    let mut auth_checked = use_signal(|| !oauth_enabled().unwrap_or(false));
    let mut user_profile = use_signal(|| None::<UserProfile>);
    let mut show_dropdown = use_signal(|| false);
    let mut meeting_status = use_signal(|| MeetingStatus::NotJoined);
    let mut host_display_name = use_signal(|| None::<String>);
    let mut current_user_email = use_signal(|| None::<String>);
    let mut came_from_waiting_room = use_signal(|| false);
    let mut error_state = use_signal(|| None::<String>);

    // Initialize input with stored username
    let initial_username = username_ctx
        .and_then(|ctx| ctx.read().clone())
        .unwrap_or_else(|| load_username_from_storage().unwrap_or_default());
    let mut input_value = use_signal(|| initial_username);

    // Auth check effect
    let meeting_id = id.clone();
    use_effect(move || {
        if oauth_enabled().unwrap_or(false) {
            wasm_bindgen_futures::spawn_local(async move {
                match check_session().await {
                    Ok(_) => {
                        log::info!("Session check passed");
                        auth_checked.set(true);

                        // Fetch user profile
                        if let Ok(profile) = get_user_profile().await {
                            user_profile.set(Some(profile));
                        }
                    }
                    Err(e) => {
                        log::warn!("No active session: {e:?}");
                        if let Some(win) = web_sys::window() {
                            if let Ok(current_url) = win.location().href() {
                                let login_url = format!(
                                    "/login?returnTo={}",
                                    urlencoding::encode(&current_url)
                                );
                                let _ = win.location().set_href(&login_url);
                            }
                        }
                    }
                }
            });
        }
    });

    // Poll for meeting activation when in WaitingForMeeting state
    let meeting_id_for_poll = id.clone();
    use_effect(move || {
        // Use peek to avoid subscribing to meeting_status (writes happen in the interval callbacks)
        let current_status = meeting_status.peek().clone();
        if current_status != MeetingStatus::WaitingForMeeting {
            return;
        }

        let meeting_id = meeting_id_for_poll.clone();
        let display_name = input_value.read().clone();

        let interval = Interval::new(2000, move || {
            let meeting_id = meeting_id.clone();
            let display_name = display_name.clone();

            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(info) = get_meeting_info(&meeting_id).await {
                    if info.state == "active" {
                        match join_meeting(&meeting_id, Some(&display_name)).await {
                            Ok(response) => {
                                current_user_email.set(Some(response.email.clone()));
                                let determined_host_display_name = info.host_display_name.clone();
                                host_display_name.set(determined_host_display_name.clone());

                                match response.status.as_str() {
                                    "admitted" => {
                                        if let Some(token) = response.room_token {
                                            meeting_status.set(MeetingStatus::Admitted {
                                                is_host: response.is_host,
                                                host_display_name: determined_host_display_name,
                                                room_token: token,
                                            });
                                        }
                                    }
                                    "waiting" => {
                                        came_from_waiting_room.set(true);
                                        meeting_status.set(MeetingStatus::Waiting);
                                    }
                                    "rejected" => meeting_status.set(MeetingStatus::Rejected),
                                    _ => {}
                                }
                            }
                            Err(JoinError::MeetingNotActive) => {}
                            Err(e) => meeting_status.set(MeetingStatus::Error(e.to_string())),
                        }
                    }
                }
            });
        });

        // Keep interval alive using Rc<RefCell>
        let interval_holder = Rc::new(RefCell::new(Some(interval)));
        let cleanup_holder = interval_holder.clone();

        // Cleanup on effect end - use a closure that will run when component unmounts
        // Note: Dioxus 0.6 effects don't have a return cleanup mechanism like React
        // The interval will be dropped when the Rc goes out of scope
    });

    // Early return for auth check
    if !*auth_checked.read() && oauth_enabled().unwrap_or(false) {
        return rsx! {
            div {
                style: "position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000;",
                p { style: "color: white; font-size: 1rem;", "Checking authentication..." }
            }
        };
    }

    // Handlers
    let on_logout = {
        let nav = nav.clone();
        move |_| {
            let nav = nav.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let _ = logout().await;
                nav.push(Route::Login {});
            });
        }
    };

    let on_join_meeting = {
        let meeting_id = id.clone();
        move |_| {
            let meeting_id = meeting_id.clone();
            let display_name = input_value.read().clone();

            meeting_status.set(MeetingStatus::Joining);

            wasm_bindgen_futures::spawn_local(async move {
                match join_meeting(&meeting_id, Some(&display_name)).await {
                    Ok(response) => {
                        current_user_email.set(Some(response.email.clone()));

                        let determined_host_display_name = if response.is_host {
                            Some(display_name.clone())
                        } else {
                            match get_meeting_info(&meeting_id).await {
                                Ok(info) => info.host_display_name,
                                Err(_) => None,
                            }
                        };
                        host_display_name.set(determined_host_display_name.clone());

                        match response.status.as_str() {
                            "admitted" => {
                                if let Some(token) = response.room_token {
                                    meeting_status.set(MeetingStatus::Admitted {
                                        is_host: response.is_host,
                                        host_display_name: determined_host_display_name,
                                        room_token: token,
                                    });
                                } else {
                                    meeting_status.set(MeetingStatus::Error(
                                        "Admitted but no room token received".to_string(),
                                    ));
                                }
                            }
                            "waiting" => {
                                came_from_waiting_room.set(true);
                                meeting_status.set(MeetingStatus::Waiting);
                            }
                            "rejected" => meeting_status.set(MeetingStatus::Rejected),
                            _ => {
                                meeting_status.set(MeetingStatus::Error(format!(
                                    "Unknown status: {}",
                                    response.status
                                )));
                            }
                        }
                    }
                    Err(JoinError::MeetingNotActive) => {
                        meeting_status.set(MeetingStatus::WaitingForMeeting);
                    }
                    Err(e) => meeting_status.set(MeetingStatus::Error(e.to_string())),
                }
            });
        }
    };

    let on_admitted = {
        let meeting_id = id.clone();
        move |room_token: String| {
            let meeting_id = meeting_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let determined = match get_meeting_info(&meeting_id).await {
                    Ok(info) => info.host_display_name,
                    Err(_) => None,
                };
                host_display_name.set(determined.clone());
                meeting_status.set(MeetingStatus::Admitted {
                    is_host: false,
                    host_display_name: determined,
                    room_token,
                });
            });
        }
    };

    let mut on_rejected = move |_| meeting_status.set(MeetingStatus::Rejected);

    let on_cancel_waiting = {
        let meeting_id = id.clone();
        move |_| {
            let meeting_id = meeting_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let _ = crate::meeting_api::leave_meeting(&meeting_id).await;
                if let Some(window) = web_sys::window() {
                    let _ = window.location().set_href("/");
                }
            });
        }
    };

    let on_submit = {
        let mut on_join = on_join_meeting.clone();
        move |evt: Event<FormData>| {
            evt.prevent_default();
            let value = input_value.read().clone();
            if is_valid_username(&value) {
                save_username_to_storage(&value);
                if let Some(mut ctx) = username_ctx {
                    ctx.set(Some(value));
                }
                error_state.set(None);
                on_join(());
            } else {
                error_state.set(Some(
                    "Please enter a valid username (letters, numbers, underscore).".to_string(),
                ));
            }
        }
    };

    let maybe_username: Option<String> = username_ctx
        .and_then(|ctx| ctx.read().clone());
    let current_status = meeting_status.read().clone();
    let should_auto_join = *came_from_waiting_room.read();

    match (&maybe_username, &current_status) {
        // User is admitted - show the meeting
        (Some(username), MeetingStatus::Admitted { is_host, host_display_name: hdname, room_token }) => {
            rsx! {
                AttendantsComponent {
                    email: username.clone(),
                    id: id.clone(),
                    webtransport_enabled: webtransport_enabled().unwrap_or(false),
                    e2ee_enabled: e2ee_enabled().unwrap_or(false),
                    user_name: user_profile.read().as_ref().map(|p| p.name.clone()),
                    user_email: current_user_email.read().clone().or_else(|| user_profile.read().as_ref().map(|p| p.email.clone())),
                    on_logout: move |_| on_logout(()),
                    host_display_name: hdname.clone(),
                    auto_join: should_auto_join,
                    is_owner: *is_host,
                    room_token: room_token.clone()
                }
            }
        }

        // User is waiting in the waiting room
        (Some(_), MeetingStatus::Waiting) => {
            rsx! {
                WaitingRoom {
                    meeting_id: id.clone(),
                    on_admitted: move |token| on_admitted(token),
                    on_rejected: move |_| on_rejected(()),
                    on_cancel: move |_| on_cancel_waiting(())
                }
            }
        }

        // Waiting for host to start the meeting
        (Some(_), MeetingStatus::WaitingForMeeting) => {
            rsx! {
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
                                if let Some(window) = web_sys::window() {
                                    let _ = window.location().set_href("/");
                                }
                            },
                            "Leave"
                        }
                    }
                }
            }
        }

        // User was rejected
        (Some(_), MeetingStatus::Rejected) => {
            rsx! {
                div { class: "rejected-container",
                    div { class: "rejected-card card-apple",
                        svg {
                            xmlns: "http://www.w3.org/2000/svg",
                            width: "64",
                            height: "64",
                            view_box: "0 0 24 24",
                            fill: "none",
                            stroke: "#ff6b6b",
                            stroke_width: "1.5",
                            circle { cx: "12", cy: "12", r: "10" }
                            line { x1: "15", y1: "9", x2: "9", y2: "15" }
                            line { x1: "9", y1: "9", x2: "15", y2: "15" }
                        }
                        h2 { "Entry denied" }
                        p { "The meeting host has denied your request to join." }
                        button {
                            class: "btn-apple btn-primary",
                            onclick: move |_| {
                                if let Some(window) = web_sys::window() {
                                    let _ = window.location().set_href("/");
                                }
                            },
                            "Return to Home"
                        }
                    }
                }
            }
        }

        // Error state
        (Some(_), MeetingStatus::Error(error)) => {
            rsx! {
                div { class: "error-container",
                    div { class: "error-card card-apple",
                        svg {
                            xmlns: "http://www.w3.org/2000/svg",
                            width: "64",
                            height: "64",
                            view_box: "0 0 24 24",
                            fill: "none",
                            stroke: "#ff9800",
                            stroke_width: "1.5",
                            circle { cx: "12", cy: "12", r: "10" }
                            line { x1: "12", y1: "8", x2: "12", y2: "12" }
                            line { x1: "12", y1: "16", x2: "12.01", y2: "16" }
                        }
                        h2 { "Unable to join" }
                        p { "{error}" }
                        button {
                            class: "btn-apple btn-primary",
                            onclick: move |_| {
                                if let Some(window) = web_sys::window() {
                                    let _ = window.location().set_href("/");
                                }
                            },
                            "Return to Home"
                        }
                    }
                }
            }
        }

        // Joining in progress
        (Some(_), MeetingStatus::Joining) => {
            rsx! {
                div {
                    style: "position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000;",
                    div { class: "loading-spinner", style: "width: 40px; height: 40px; margin-bottom: 1rem;" }
                    p { style: "color: white; font-size: 1rem;", "Joining meeting..." }
                }
            }
        }

        // Username prompt view (not joined yet or no username)
        _ => {
            rsx! {
                div { id: "username-prompt", class: "username-prompt-container relative",
                    // User profile dropdown
                    if oauth_enabled().unwrap_or(false) {
                        if let Some(profile) = user_profile.read().clone() {
                            div { class: "absolute top-4 right-4 z-50",
                                button {
                                    onclick: move |_| {
                                        let current = *show_dropdown.read();
                                        show_dropdown.set(!current);
                                    },
                                    class: "flex items-center gap-2 px-4 py-2 bg-gray-800 hover:bg-gray-700 rounded-lg text-white text-sm transition-colors",
                                    span { "{profile.name}" }
                                    svg {
                                        class: "w-4 h-4",
                                        fill: "none",
                                        stroke: "currentColor",
                                        view_box: "0 0 24 24",
                                        path {
                                            stroke_linecap: "round",
                                            stroke_linejoin: "round",
                                            stroke_width: "2",
                                            d: "M19 9l-7 7-7-7"
                                        }
                                    }
                                }

                                if *show_dropdown.read() {
                                    div { class: "absolute right-0 mt-2 w-56 bg-white rounded-lg shadow-lg border border-gray-200 py-1",
                                        div { class: "px-4 py-3 border-b border-gray-200",
                                            p { class: "text-sm font-medium text-gray-900", "{profile.name}" }
                                            p { class: "text-xs text-gray-500 truncate", "{profile.email}" }
                                        }
                                        button {
                                            onclick: move |_| on_logout(()),
                                            class: "w-full text-left px-4 py-2 text-sm text-red-600 hover:bg-red-50 transition-colors",
                                            "Sign out"
                                        }
                                    }
                                }
                            }
                        }
                    }

                    form {
                        class: "username-form",
                        onsubmit: on_submit,
                        h1 { "Choose a display name" }
                        input {
                            class: "username-input",
                            placeholder: "Your name",
                            pattern: "^[a-zA-Z0-9_]*$",
                            required: true,
                            autofocus: true,
                            value: "{input_value}",
                            oninput: move |evt| input_value.set(evt.value())
                        }
                        if let Some(err) = error_state.read().clone() {
                            p { class: "error", "{err}" }
                        }
                        button { class: "cta-button", r#type: "submit", "Continue" }
                    }
                }
            }
        }
    }
}

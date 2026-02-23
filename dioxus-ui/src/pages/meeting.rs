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
use crate::constants::{e2ee_enabled, oauth_enabled, webtransport_enabled};
use crate::context::{
    is_valid_username, load_username_from_storage, save_username_to_storage, UsernameCtx,
};
use crate::meeting_api::{get_meeting_info, join_meeting, JoinError};
use dioxus::prelude::*;
use gloo_timers::callback::Interval;
use std::cell::RefCell;
use std::rc::Rc;
use web_sys::window;

use crate::auth::{check_session, get_user_profile, logout, UserProfile};
use crate::routing::Route;

/// Meeting participant status from the API
#[derive(Clone, PartialEq, Debug)]
pub enum MeetingStatus {
    NotJoined,
    Joining,
    WaitingForMeeting,
    Waiting,
    Admitted {
        is_host: bool,
        host_display_name: Option<String>,
        room_token: String,
    },
    Rejected,
    Error(String),
}

#[component]
pub fn MeetingPage(id: String) -> Element {
    let mut username_ctx = use_context::<UsernameCtx>();
    let mut auth_checked = use_signal(|| false);
    let navigator = use_navigator();
    let mut user_profile = use_signal(|| None::<UserProfile>);
    let mut show_dropdown = use_signal(|| false);
    let mut meeting_status = use_signal(|| MeetingStatus::NotJoined);
    let mut host_display_name = use_signal(|| None::<String>);
    let mut current_user_email = use_signal(|| None::<String>);
    let mut came_from_waiting_room = use_signal(|| false);
    let mut error_state = use_signal(|| None::<String>);

    let initial_username: String = if let Some(name) = (username_ctx.0)() {
        name
    } else {
        load_username_from_storage().unwrap_or_default()
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
                                let login_url = format!("/login?returnTo={}", urlencoding::encode(&current_url));
                                let _ = win.location().set_href(&login_url);
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
    {
        let auth_done = auth_checked();
        use_effect(move || {
            if auth_done && oauth_enabled().unwrap_or(false) {
                wasm_bindgen_futures::spawn_local(async move {
                    if let Ok(profile) = get_user_profile().await {
                        user_profile.set(Some(profile));
                    }
                });
            }
        });
    }

    // Poll for meeting activation when WaitingForMeeting
    let _meeting_poll_interval: Rc<RefCell<Option<Interval>>> = use_hook(|| Rc::new(RefCell::new(None)));
    {
        let meeting_id = id.clone();
        let current_status = meeting_status();
        let poll_interval = _meeting_poll_interval.clone();
        use_effect(move || {
            // Drop previous interval
            poll_interval.borrow_mut().take();

            if current_status != MeetingStatus::WaitingForMeeting {
                return;
            }
            let meeting_id = meeting_id.clone();
            let display_name = input_value_state();

            let interval = Interval::new(2000, move || {
                let meeting_id = meeting_id.clone();
                let display_name = display_name.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    match get_meeting_info(&meeting_id).await {
                        Ok(info) => {
                            if info.state == "active" {
                                match join_meeting(&meeting_id, Some(&display_name)).await {
                                    Ok(response) => {
                                        current_user_email.set(Some(response.email.clone()));
                                        host_display_name.set(info.host_display_name.clone());
                                        match response.status.as_str() {
                                            "admitted" => {
                                                if let Some(token) = response.room_token {
                                                    meeting_status.set(MeetingStatus::Admitted {
                                                        is_host: response.is_host,
                                                        host_display_name: info.host_display_name,
                                                        room_token: token,
                                                    });
                                                } else {
                                                    meeting_status.set(MeetingStatus::Error("Admitted but no room token".to_string()));
                                                }
                                            }
                                            "waiting" => {
                                                came_from_waiting_room.set(true);
                                                meeting_status.set(MeetingStatus::Waiting);
                                            }
                                            "rejected" => meeting_status.set(MeetingStatus::Rejected),
                                            _ => meeting_status.set(MeetingStatus::Error(format!("Unknown status: {}", response.status))),
                                        }
                                    }
                                    Err(JoinError::MeetingNotActive) => {}
                                    Err(e) => meeting_status.set(MeetingStatus::Error(e.to_string())),
                                }
                            }
                        }
                        Err(_) => {}
                    }
                });
            });

            *poll_interval.borrow_mut() = Some(interval);
        });
    }

    // Logout handler
    let on_logout = move |_| {
        let navigator = navigator.clone();
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
                        current_user_email.set(Some(response.email.clone()));
                        let determined_host = if response.is_host {
                            Some(display_name.clone())
                        } else {
                            match crate::meeting_api::get_meeting_info(&meeting_id).await {
                                Ok(info) => info.host_display_name,
                                Err(_) => None,
                            }
                        };
                        host_display_name.set(determined_host.clone());
                        match response.status.as_str() {
                            "admitted" => {
                                if let Some(token) = response.room_token {
                                    meeting_status.set(MeetingStatus::Admitted {
                                        is_host: response.is_host,
                                        host_display_name: determined_host,
                                        room_token: token,
                                    });
                                } else {
                                    meeting_status.set(MeetingStatus::Error("Admitted but no room token".to_string()));
                                }
                            }
                            "waiting" => {
                                came_from_waiting_room.set(true);
                                meeting_status.set(MeetingStatus::Waiting);
                            }
                            "rejected" => meeting_status.set(MeetingStatus::Rejected),
                            _ => meeting_status.set(MeetingStatus::Error(format!("Unknown status: {}", response.status))),
                        }
                    }
                    Err(JoinError::MeetingNotActive) => meeting_status.set(MeetingStatus::WaitingForMeeting),
                    Err(e) => meeting_status.set(MeetingStatus::Error(e.to_string())),
                }
            });
        }
    };

    // Handle waiting room admission
    let on_admitted = {
        let meeting_id = id.clone();
        move |room_token: String| {
            let meeting_id = meeting_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let determined_host = match crate::meeting_api::get_meeting_info(&meeting_id).await {
                    Ok(info) => info.host_display_name,
                    Err(_) => None,
                };
                host_display_name.set(determined_host.clone());
                meeting_status.set(MeetingStatus::Admitted {
                    is_host: false,
                    host_display_name: determined_host,
                    room_token,
                });
            });
        }
    };

    let on_rejected = move |_| {
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

    // Submit handler
    let on_submit = {
        let mut on_join = on_join_meeting.clone();
        move |e: FormEvent| {
            e.prevent_default();
            let value = input_value_state();
            if is_valid_username(&value) {
                save_username_to_storage(&value);

                if let Some(storage) = window().and_then(|w| w.local_storage().ok().flatten()) {
                    if let Ok(Some(flag)) = storage.get_item("vc_username_reset") {
                        if flag == "1" {
                            let _ = storage.remove_item("vc_username_reset");
                            if let Some(win) = window() {
                                let _ = win.location().reload();
                            }
                            return;
                        }
                    }
                }

                (username_ctx.0).set(Some(value));
                error_state.set(None);
                on_join();
            } else {
                error_state.set(Some("Please enter a valid username (letters, numbers, underscore).".to_string()));
            }
        }
    };

    let maybe_username = (username_ctx.0)();
    let current_meeting_status = meeting_status();
    let should_auto_join = came_from_waiting_room();

    rsx! {
        match (&maybe_username, &current_meeting_status) {
            // User is admitted - show the meeting
            (Some(username), MeetingStatus::Admitted { is_host, host_display_name, room_token }) => rsx! {
                AttendantsComponent {
                    email: username.clone(),
                    id: id.clone(),
                    webtransport_enabled: webtransport_enabled().unwrap_or(false),
                    e2ee_enabled: e2ee_enabled().unwrap_or(false),
                    user_name: user_profile().as_ref().map(|p| p.name.clone()),
                    user_email: current_user_email().or_else(|| user_profile().as_ref().map(|p| p.email.clone())),
                    on_logout: Some(EventHandler::new(on_logout)),
                    host_display_name: host_display_name.clone(),
                    auto_join: should_auto_join,
                    is_owner: *is_host,
                    room_token: room_token.clone(),
                }
            },

            // Waiting room
            (Some(_), MeetingStatus::Waiting) => rsx! {
                WaitingRoom {
                    meeting_id: id.clone(),
                    on_admitted: move |token| on_admitted(token),
                    on_rejected: on_rejected,
                    on_cancel: on_cancel_waiting,
                }
            },

            // Waiting for host to start
            (Some(_), MeetingStatus::WaitingForMeeting) => rsx! {
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
            (Some(_), MeetingStatus::Joining) => rsx! {
                div { style: "position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000;",
                    div { class: "loading-spinner", style: "width: 40px; height: 40px; margin-bottom: 1rem;" }
                    p { style: "color: white; font-size: 1rem;", "Joining meeting..." }
                }
            },

            // Username prompt (not joined or no username)
            _ => {
                let mut on_join = on_join_meeting.clone();
                rsx! {
                    div { id: "username-prompt", class: "username-prompt-container relative",
                        // User profile dropdown (OAuth)
                        if oauth_enabled().unwrap_or(false) {
                            if let Some(profile) = user_profile() {
                                div { class: "fixed top-4 right-4 z-50",
                                    button {
                                        r#type: "button",
                                        onclick: move |_| show_dropdown.set(!show_dropdown()),
                                        class: "flex items-center gap-2 px-4 py-2 bg-gray-800 hover:bg-gray-700 rounded-lg text-white text-sm transition-colors",
                                        span { "{profile.name}" }
                                        svg { class: "w-4 h-4", fill: "none", stroke: "currentColor", view_box: "0 0 24 24",
                                            path { stroke_linecap: "round", stroke_linejoin: "round", stroke_width: "2", d: "M19 9l-7 7-7-7" }
                                        }
                                    }
                                    if show_dropdown() {
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
                                oninput: move |e: Event<FormData>| {
                                    input_value_state.set(e.value());
                                },
                                onkeydown: move |e: Event<KeyboardData>| {
                                    if e.key() == Key::Enter {
                                        let value = input_value_state();
                                        if is_valid_username(&value) {
                                            save_username_to_storage(&value);
                                            if let Some(storage) = window().and_then(|w| w.local_storage().ok().flatten()) {
                                                if let Ok(Some(flag)) = storage.get_item("vc_username_reset") {
                                                    if flag == "1" {
                                                        let _ = storage.remove_item("vc_username_reset");
                                                        if let Some(win) = window() { let _ = win.location().reload(); }
                                                        e.prevent_default();
                                                        return;
                                                    }
                                                }
                                            }
                                            (username_ctx.0).set(Some(value));
                                            error_state.set(None);
                                            on_join();
                                        } else {
                                            error_state.set(Some("Please enter a valid username (letters, numbers, underscore).".to_string()));
                                        }
                                        e.prevent_default();
                                    }
                                },
                                value: "{input_value_state}",
                            }
                            if let Some(err) = error_state() {
                                p { class: "error", "{err}" }
                            }
                            button { class: "cta-button", r#type: "submit", "Continue" }
                        }
                    }
                }
            }
        }
    }
}

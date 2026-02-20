/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use crate::components::login::{do_login, ProviderButton};
use crate::constants::meeting_api_client;
use crate::routing::Route;
use dioxus::prelude::*;
use videocall_meeting_types::responses::{ListMeetingsResponse, MeetingSummary};

enum FetchMeetingsError {
    Unauthenticated,
    Other(String),
}

#[component]
pub fn MeetingsList(
    on_select_meeting: Option<EventHandler<String>>,
    user_email: Option<String>,
) -> Element {
    let mut meetings = use_signal(Vec::<MeetingSummary>::new);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| None::<String>);
    let mut unauthenticated = use_signal(|| false);
    let mut expanded = use_signal(|| true);
    let mut total = use_signal(|| 0i64);

    #[allow(unused_mut)]
    let mut fetch_meetings = move || {
        loading.set(true);
        error.set(None);
        unauthenticated.set(false);

        spawn(async move {
            match do_fetch_meetings().await {
                Ok(response) => {
                    meetings.set(response.meetings);
                    total.set(response.total);
                    loading.set(false);
                    error.set(None);
                }
                Err(FetchMeetingsError::Unauthenticated) => {
                    loading.set(false);
                    unauthenticated.set(true);
                }
                Err(FetchMeetingsError::Other(e)) => {
                    loading.set(false);
                    error.set(Some(e));
                }
            }
        });
    };

    // Fetch on mount
    use_effect({
        let mut fetch_meetings = fetch_meetings.clone();
        move || {
            fetch_meetings();
        }
    });

    let toggle_expanded = {
        let mut fetch_meetings = fetch_meetings.clone();
        move |_| {
            let new_expanded = !expanded();
            expanded.set(new_expanded);
            if new_expanded {
                fetch_meetings();
            }
        }
    };

    let refresh = {
        let mut fetch_meetings = fetch_meetings.clone();
        move |_| {
            fetch_meetings();
        }
    };

    rsx! {
        div { class: "meetings-list-container",
            button {
                class: "meetings-list-toggle",
                onclick: toggle_expanded,
                r#type: "button",
                svg {
                    class: if expanded() { "chevron-icon expanded" } else { "chevron-icon" },
                    xmlns: "http://www.w3.org/2000/svg",
                    width: "20",
                    height: "20",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    polyline { points: "6 9 12 15 18 9" }
                }
                span { "My Meetings" }
                span { class: "meeting-count", "({total()})" }
            }

            if expanded() {
                div { class: "meetings-list-content",
                    if loading() {
                        div { class: "meetings-loading",
                            span { class: "loading-spinner" }
                            "Loading meetings..."
                        }
                    } else if unauthenticated() {
                        div { class: "meetings-auth-prompt",
                            p { class: "meetings-auth-text", "Sign in to see your meetings" }
                            ProviderButton { onclick: move |_| do_login() }
                        }
                    } else if let Some(err) = error() {
                        div { class: "meetings-error",
                            span { "Error: {err}" }
                            button { onclick: refresh, class: "retry-btn", "Retry" }
                        }
                    } else if meetings().is_empty() {
                        div { class: "meetings-empty", "No meetings yet" }
                    } else {
                        ul { class: "meetings-list",
                            for meeting in meetings().iter() {
                                MeetingItem {
                                    key: "{meeting.meeting_id}",
                                    meeting: meeting.clone(),
                                    on_select_meeting: on_select_meeting,
                                    on_delete: {
                                        #[allow(unused_mut)]
                                        let mut fetch_meetings = fetch_meetings.clone();
                                        move |meeting_id: String| {
                                            // Optimistic removal
                                            meetings.write().retain(|m| m.meeting_id != meeting_id);
                                            total.set(total().saturating_sub(1));

                                            let meeting_id = meeting_id.clone();
                                            let mut fetch_meetings = fetch_meetings.clone();
                                            spawn(async move {
                                                match do_delete_meeting(&meeting_id).await {
                                                    Ok(_) => fetch_meetings(),
                                                    Err(e) => {
                                                        error.set(Some(e));
                                                        fetch_meetings();
                                                    }
                                                }
                                            });
                                        }
                                    },
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn MeetingItem(
    meeting: MeetingSummary,
    on_select_meeting: Option<EventHandler<String>>,
    on_delete: EventHandler<String>,
) -> Element {
    let nav = use_navigator();
    let is_active = meeting.state == "active";
    let is_ended = meeting.state == "ended";

    let state_class = match meeting.state.as_str() {
        "active" => "state-active",
        "idle" => "state-idle",
        _ => "state-ended",
    };

    let duration_ms = meeting
        .ended_at
        .map(|ended_at| ended_at - meeting.started_at)
        .unwrap_or(0);

    let meeting_id = meeting.meeting_id.clone();
    let meeting_id_click = meeting_id.clone();
    let meeting_id_delete = meeting_id.clone();

    let on_click = move |_| {
        if let Some(ref callback) = on_select_meeting {
            callback.call(meeting_id_click.clone());
        } else {
            nav.push(Route::Meeting {
                id: meeting_id_click.clone(),
            });
        }
    };

    let on_delete_click = move |e: MouseEvent| {
        e.stop_propagation();
        if web_sys::window()
            .and_then(|w| {
                w.confirm_with_message("Are you sure you want to delete this meeting?")
                    .ok()
            })
            .unwrap_or(false)
        {
            on_delete.call(meeting_id_delete.clone());
        }
    };

    rsx! {
        li { class: if is_ended { "meeting-item meeting-ended" } else { "meeting-item" },
            div { class: "meeting-item-content", onclick: on_click,
                div { class: "meeting-info",
                    span { class: "meeting-id", "{meeting.meeting_id}" }
                    span { class: "meeting-state {state_class}", "{meeting.state}" }
                }
                div { class: "meeting-details",
                    if is_active {
                        span { class: "meeting-participants", title: "Participants in meeting",
                            svg {
                                xmlns: "http://www.w3.org/2000/svg", width: "14", height: "14",
                                view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                                stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                                path { d: "M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" }
                                circle { cx: "9", cy: "7", r: "4" }
                                path { d: "M23 21v-2a4 4 0 0 0-3-3.87" }
                                path { d: "M16 3.13a4 4 0 0 1 0 7.75" }
                            }
                            "{meeting.participant_count} joined"
                        }
                        if meeting.waiting_count > 0 {
                            span { class: "meeting-waiting", title: "Waiting to join",
                                svg {
                                    xmlns: "http://www.w3.org/2000/svg", width: "14", height: "14",
                                    view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                                    stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                                    circle { cx: "12", cy: "12", r: "10" }
                                    line { x1: "12", y1: "8", x2: "12", y2: "12" }
                                    line { x1: "12", y1: "16", x2: "12.01", y2: "16" }
                                }
                                "{meeting.waiting_count} waiting"
                            }
                        }
                    }
                    if is_ended {
                        span { class: "meeting-duration", title: "Total duration",
                            svg {
                                xmlns: "http://www.w3.org/2000/svg", width: "14", height: "14",
                                view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                                stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                                circle { cx: "12", cy: "12", r: "10" }
                                polyline { points: "12 6 12 12 16 14" }
                            }
                            "{format_duration(duration_ms)}"
                        }
                        span { class: "meeting-time", title: "Started at {format_time(meeting.started_at)}",
                            "{format_time(meeting.started_at)}"
                        }
                        span { class: "meeting-time-separator", "-" }
                        if let Some(ended_at) = meeting.ended_at {
                            span { class: "meeting-time", title: "Ended at {format_time(ended_at)}",
                                "{format_time(ended_at)}"
                            }
                        }
                    }
                    if !is_active && !is_ended {
                        span { class: "meeting-participants", title: "Participants",
                            svg {
                                xmlns: "http://www.w3.org/2000/svg", width: "14", height: "14",
                                view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                                stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                                path { d: "M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" }
                                circle { cx: "9", cy: "7", r: "4" }
                                path { d: "M23 21v-2a4 4 0 0 0-3-3.87" }
                                path { d: "M16 3.13a4 4 0 0 1 0 7.75" }
                            }
                            "{meeting.participant_count}"
                        }
                    }
                    if meeting.has_password {
                        span { class: "meeting-password", title: "Password protected",
                            svg {
                                xmlns: "http://www.w3.org/2000/svg", width: "14", height: "14",
                                view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                                stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                                rect { x: "3", y: "11", width: "18", height: "11", rx: "2", ry: "2" }
                                path { d: "M7 11V7a5 5 0 0 1 10 0v4" }
                            }
                        }
                    }
                }
            }
            button {
                class: if is_ended { "meeting-delete-btn meeting-delete-btn-ended" } else { "meeting-delete-btn" },
                onclick: on_delete_click,
                title: "Delete meeting",
                svg {
                    xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                    view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                    stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                    polyline { points: "3 6 5 6 21 6" }
                    path { d: "M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" }
                    line { x1: "10", y1: "11", x2: "10", y2: "17" }
                    line { x1: "14", y1: "11", x2: "14", y2: "17" }
                }
            }
        }
    }
}

async fn do_fetch_meetings() -> Result<ListMeetingsResponse, FetchMeetingsError> {
    let client = meeting_api_client()
        .map_err(|e| FetchMeetingsError::Other(format!("Config error: {e}")))?;
    client.list_meetings(20, 0).await.map_err(|e| match e {
        videocall_meeting_client::ApiError::NotAuthenticated => FetchMeetingsError::Unauthenticated,
        other => FetchMeetingsError::Other(format!("{other}")),
    })
}

async fn do_delete_meeting(meeting_id: &str) -> Result<(), String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    client
        .delete_meeting(meeting_id)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}

fn format_duration(duration_ms: i64) -> String {
    let total_seconds = duration_ms / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_time(timestamp_ms: i64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(timestamp_ms as f64));
    let hours = date.get_hours();
    let minutes = date.get_minutes();
    let am_pm = if hours >= 12 { "PM" } else { "AM" };
    let hours_12 = if hours == 0 {
        12
    } else if hours > 12 {
        hours - 12
    } else {
        hours
    };
    format!("{hours_12}:{minutes:02} {am_pm}")
}

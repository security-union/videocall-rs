/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use crate::components::login::{do_login, ProviderButton};
use crate::constants::meeting_api_client;
use crate::routing::Route;
use dioxus::prelude::*;
use videocall_meeting_types::responses::{ListMeetingsResponse, MeetingSummary};
use videocall_theme::{
    icons, Badge, BadgeTone, Button, ButtonSize, Icon, IconButton, IconButtonSize, ListGroup,
    ListRow, ListRowLabel, Spinner,
};

enum FetchMeetingsError {
    Unauthenticated,
    Other(String),
}

#[component]
pub fn MeetingsList(
    on_select_meeting: Option<EventHandler<String>>,
    user_id: Option<String>,
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
        let mut fetch_meetings = fetch_meetings;
        move || {
            fetch_meetings();
        }
    });

    let toggle_expanded = {
        let mut fetch_meetings = fetch_meetings;
        move |_| {
            let new_expanded = !expanded();
            expanded.set(new_expanded);
            if new_expanded {
                fetch_meetings();
            }
        }
    };

    let refresh = {
        let mut fetch_meetings = fetch_meetings;
        move |_| {
            fetch_meetings();
        }
    };

    rsx! {
        div { class: "meetings-list-container",
            ListRow {
                class: "meetings-list-toggle",
                onclick: toggle_expanded,
                aria_expanded: expanded(),
                Icon { glyph: &icons::CHEVRON_DOWN, size: "20", class: if expanded() { "chevron-icon expanded" } else { "chevron-icon" } }
                ListRowLabel { "My Meetings" }
                Badge { "aria-label": "{total()} meetings", "{total()}" }
            }

            if expanded() {
                div { class: "meetings-list-content",
                    if loading() {
                        div { class: "meetings-loading text-footnote text-secondary",
                            Spinner {}
                            "Loading meetings..."
                        }
                    } else if unauthenticated() {
                        div { class: "meetings-auth-prompt",
                            p { class: "text-subheadline text-secondary", "Sign in to see your meetings" }
                            ProviderButton { onclick: move |_| do_login() }
                        }
                    } else if let Some(err) = error() {
                        div { class: "meetings-error text-footnote text-danger",
                            span { "Error: {err}" }
                            Button { size: ButtonSize::Small, onclick: refresh, "Retry" }
                        }
                    } else if meetings().is_empty() {
                        div { class: "meetings-empty text-footnote text-secondary", "No meetings yet" }
                    } else {
                        ListGroup { list: true, class: "meetings-list",
                            for meeting in meetings().iter() {
                                MeetingItem {
                                    key: "{meeting.meeting_id}",
                                    meeting: meeting.clone(),
                                    on_select_meeting: on_select_meeting,
                                    on_delete: {
                                        #[allow(unused_mut)]
                                        let mut fetch_meetings = fetch_meetings;
                                        move |meeting_id: String| {
                                            // Optimistic removal
                                            meetings.write().retain(|m| m.meeting_id != meeting_id);
                                            total.set(total().saturating_sub(1));

                                            let meeting_id = meeting_id.clone();
                                            let mut fetch_meetings = fetch_meetings;
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
    let state_tone = match meeting.state.as_str() {
        "active" => BadgeTone::Success,
        "idle" => BadgeTone::Warning,
        _ => BadgeTone::Neutral,
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

    let meeting_id_edit = meeting_id.clone();

    let on_edit_click = move |e: MouseEvent| {
        e.stop_propagation();
        nav.push(Route::MeetingSettings {
            id: meeting_id_edit.clone(),
        });
    };

    rsx! {
        ListRow { list_item: true, class: if is_ended { "meeting-item meeting-ended" } else { "meeting-item" },
            div { class: "meeting-item-content", onclick: on_click,
                div { class: "meeting-info",
                    span { class: "meeting-id text-headline", "{meeting.meeting_id}" }
                    Badge { tone: state_tone, "{meeting.state}" }
                }
                div { class: "meeting-details text-footnote text-secondary",
                    if is_active {
                        span { class: "meeting-participants", title: "Participants in meeting",
                            Icon { glyph: &icons::PEOPLE, size: "14" }
                            "{meeting.participant_count} joined"
                        }
                        if meeting.waiting_count > 0 {
                            span { class: "meeting-waiting", title: "Waiting to join",
                                Icon { glyph: &icons::ALERT, size: "14" }
                                "{meeting.waiting_count} waiting"
                            }
                        }
                    }
                    if is_ended {
                        span { class: "meeting-duration", title: "Total duration",
                            Icon { glyph: &icons::CLOCK, size: "14" }
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
                            Icon { glyph: &icons::PEOPLE, size: "14" }
                            "{meeting.participant_count}"
                        }
                    }
                    if meeting.has_password {
                        span { class: "meeting-password", title: "Password protected",
                            Icon { glyph: &icons::LOCK, size: "14" }
                        }
                    }
                }
            }
            IconButton {
                label: "Meeting settings",
                size: IconButtonSize::Small,
                onclick: on_edit_click,
                title: "Meeting settings",
                Icon { glyph: &icons::PENCIL, size: "16" }
            }
            IconButton {
                label: "Delete meeting",
                size: IconButtonSize::Small,
                class: "meeting-delete-btn",
                onclick: on_delete_click,
                title: "Delete meeting",
                Icon { glyph: &icons::TRASH, size: "16" }
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

/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! "Previously Joined" meetings list — sibling component to [`MeetingsList`].
//!
//! Renders the last N meetings the authenticated user has been admitted into,
//! ordered by most recent admission. Includes both meetings the user owns and
//! meetings they joined as a non-owner.
//!
//! Visually mirrors `MeetingsList` (chevron toggle, state pills, participant /
//! waiting / password indicators, click-to-navigate). The only differences:
//! - An "Owner" badge is rendered next to the state pill when the row's
//!   `is_owner == true`.
//! - No edit/delete management actions — users manage their owned meetings
//!   from the existing "My Meetings" section.

use crate::components::login::{do_login, ProviderButton};
use crate::constants::meeting_api_client;
use crate::local_storage::{load_bool, save_bool};
use crate::routing::Route;
use dioxus::prelude::*;
use videocall_meeting_types::responses::{JoinedMeetingSummary, ListJoinedMeetingsResponse};
use wasm_bindgen::JsCast;

/// Number of joined meetings to fetch and display. The backend default is 5,
/// but we pass it explicitly so the UI does not depend on the default.
const JOINED_LIMIT: u32 = 5;

/// `localStorage` key for the "Previously Joined" section's expand/collapse
/// state. Defaults to expanded (`true`) on first load.
const EXPANDED_STORAGE_KEY: &str = "home.previously-joined.expanded";

enum FetchJoinedError {
    Unauthenticated,
    Other(String),
}

#[component]
pub fn JoinedMeetingsList(on_select_meeting: Option<EventHandler<String>>) -> Element {
    let mut meetings = use_signal(Vec::<JoinedMeetingSummary>::new);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| None::<String>);
    let mut unauthenticated = use_signal(|| false);
    let mut expanded = use_signal(|| load_bool(EXPANDED_STORAGE_KEY, true));
    let mut total = use_signal(|| 0i64);

    #[allow(unused_mut)]
    let mut fetch_meetings = move || {
        loading.set(true);
        error.set(None);
        unauthenticated.set(false);

        spawn(async move {
            match do_fetch_joined_meetings().await {
                Ok(response) => {
                    meetings.set(response.meetings);
                    total.set(response.total);
                    loading.set(false);
                    error.set(None);
                }
                Err(FetchJoinedError::Unauthenticated) => {
                    loading.set(false);
                    unauthenticated.set(true);
                }
                Err(FetchJoinedError::Other(e)) => {
                    loading.set(false);
                    error.set(Some(e));
                }
            }
        });
    };

    // Fetch on mount.
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
            save_bool(EXPANDED_STORAGE_KEY, new_expanded);
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
        div { class: "meetings-list-container joined-meetings-list-container",
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
                span { "Previously Joined" }
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
                        div { class: "meetings-empty", "No previously joined meetings" }
                    } else {
                        ul { class: "meetings-list",
                            for meeting in meetings().iter() {
                                JoinedMeetingItem {
                                    key: "{meeting.meeting_id}",
                                    meeting: meeting.clone(),
                                    on_select_meeting: on_select_meeting,
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
fn JoinedMeetingItem(
    meeting: JoinedMeetingSummary,
    on_select_meeting: Option<EventHandler<String>>,
) -> Element {
    let nav = use_navigator();
    let is_active = meeting.state == "active";
    let is_ended = meeting.state == "ended";
    let state_class = match meeting.state.as_str() {
        "active" => "state-active",
        "idle" => "state-idle",
        _ => "state-ended",
    };

    let duration_ms = if is_active {
        let now_ms = js_sys::Date::now() as i64;
        (now_ms - meeting.started_at).max(0)
    } else {
        meeting
            .ended_at
            .map(|ended_at| ended_at - meeting.started_at)
            .unwrap_or(0)
    };

    let meeting_id = meeting.meeting_id.clone();
    let meeting_id_click = meeting_id.clone();

    let on_click = move |_| {
        if let Some(ref callback) = on_select_meeting {
            callback.call(meeting_id_click.clone());
        } else {
            nav.push(Route::Meeting {
                id: meeting_id_click.clone(),
            });
        }
    };

    let tooltip_html = build_joined_tooltip_html(&meeting, is_active, is_ended, duration_ms);
    let tooltip_html_for_show = tooltip_html.clone();

    rsx! {
        li {
            class: if is_ended { "meeting-item meeting-ended" } else { "meeting-item" },
            onmouseenter: move |e: MouseEvent| {
                let coords = e.client_coordinates();
                show_joined_info_tooltip(coords.x, coords.y, &tooltip_html_for_show);
            },
            onmousemove: move |e: MouseEvent| {
                let coords = e.client_coordinates();
                position_joined_info_tooltip(coords.x, coords.y);
            },
            onmouseleave: move |_| hide_joined_info_tooltip(),
            div { class: "meeting-item-content", onclick: on_click,
                div { class: "meeting-info",
                    span { class: "meeting-id", "{meeting.meeting_id}" }
                    {
                        let state_label = {
                            let s = &meeting.state;
                            let mut c = s.chars();
                            match c.next() {
                                None => String::new(),
                                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                            }
                        };
                        rsx! {
                            span { class: "meeting-state {state_class}", "{state_label}" }
                        }
                    }
                    if meeting.is_owner {
                        span {
                            class: "meeting-owner-badge",
                            title: "You own this meeting",
                            svg {
                                xmlns: "http://www.w3.org/2000/svg", width: "10", height: "10",
                                view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                                stroke_width: "2.5", stroke_linecap: "round", stroke_linejoin: "round",
                                path { d: "M12 2l2.39 7.36H22l-6.18 4.49L18.21 22 12 17.27 5.79 22l2.39-8.15L2 9.36h7.61L12 2z" }
                            }
                            "Owner"
                        }
                    }
                }
            }
        }
    }
}

async fn do_fetch_joined_meetings() -> Result<ListJoinedMeetingsResponse, FetchJoinedError> {
    let client =
        meeting_api_client().map_err(|e| FetchJoinedError::Other(format!("Config error: {e}")))?;
    client
        .list_joined_meetings(JOINED_LIMIT)
        .await
        .map_err(|e| match e {
            videocall_meeting_client::ApiError::NotAuthenticated => {
                FetchJoinedError::Unauthenticated
            }
            other => FetchJoinedError::Other(format!("{other}")),
        })
}

fn build_joined_tooltip_html(
    meeting: &JoinedMeetingSummary,
    is_active: bool,
    is_ended: bool,
    duration_ms: i64,
) -> String {
    use crate::components::meeting_format::{format_datetime, format_duration};
    let mut rows: Vec<String> = Vec::new();
    if is_active {
        rows.push(tooltip_row(
            "Started on",
            &format_datetime(meeting.started_at),
        ));
        rows.push(tooltip_row("Duration", &format_duration(duration_ms)));
        rows.push(tooltip_row(
            "Attendees",
            &meeting.participant_count.to_string(),
        ));
        if meeting.waiting_count > 0 {
            rows.push(tooltip_row("Waiting", &meeting.waiting_count.to_string()));
        }
    } else if is_ended {
        if meeting.is_owner {
            rows.push(tooltip_row(
                "Created on",
                &format_datetime(meeting.created_at),
            ));
        }
        rows.push(tooltip_row(
            "Last active on",
            &format_datetime(meeting.started_at),
        ));
        rows.push(tooltip_row("Duration", &format_duration(duration_ms)));
    } else {
        // idle branch
        if meeting.is_owner {
            rows.push(tooltip_row(
                "Created on",
                &format_datetime(meeting.created_at),
            ));
        }
        rows.push(tooltip_row(
            "Last active on",
            &format_datetime(meeting.started_at),
        ));
    }
    if meeting.has_password {
        rows.push(tooltip_row("Password", "Protected"));
    }
    rows.join("")
}

fn tooltip_row(label: &str, value: &str) -> String {
    format!(
        "<div class=\"meeting-info-tooltip-row\">\
         <span class=\"meeting-info-tooltip-label\">{label}</span>\
         <span class=\"meeting-info-tooltip-value\">{value}</span>\
         </div>"
    )
}

fn get_or_create_joined_tooltip_el() -> web_sys::HtmlElement {
    let doc = gloo_utils::document();
    if let Some(el) = doc.get_element_by_id("meeting-info-tooltip-global") {
        el.unchecked_into()
    } else {
        let el = doc.create_element("div").unwrap();
        el.set_id("meeting-info-tooltip-global");
        el.set_class_name("meeting-info-tooltip-portal");
        let html_el: web_sys::HtmlElement = el.unchecked_into();
        doc.body().unwrap().append_child(&html_el).unwrap();
        html_el
    }
}

fn show_joined_info_tooltip(x: f64, y: f64, html: &str) {
    let el = get_or_create_joined_tooltip_el();
    el.set_inner_html(html);
    position_joined_info_tooltip(x, y);
    let _ = el.class_list().add_1("is-visible");
}

fn position_joined_info_tooltip(x: f64, y: f64) {
    if let Some(el) = gloo_utils::document().get_element_by_id("meeting-info-tooltip-global") {
        let html_el: web_sys::HtmlElement = el.unchecked_into();
        let rect = html_el.get_bounding_client_rect();
        let win = gloo_utils::window();
        let vw = win
            .inner_width()
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let vh = win
            .inner_height()
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let offset = 16.0;
        let edge_margin = 8.0;
        let tooltip_w = rect.width().max(192.0);
        let tooltip_h = rect.height().max(40.0);
        let mut left = x + offset;
        let mut top = y + offset;
        if left + tooltip_w + edge_margin > vw {
            left = (x - tooltip_w - offset).max(edge_margin);
        }
        if top + tooltip_h + edge_margin > vh {
            top = (y - tooltip_h - offset).max(edge_margin);
        }
        let style = html_el.style();
        style.set_property("left", &format!("{left:.0}px")).unwrap();
        style.set_property("top", &format!("{top:.0}px")).unwrap();
    }
}

fn hide_joined_info_tooltip() {
    if let Some(el) = gloo_utils::document().get_element_by_id("meeting-info-tooltip-global") {
        let html_el: web_sys::HtmlElement = el.unchecked_into();
        let _ = html_el.class_list().remove_1("is-visible");
    }
}

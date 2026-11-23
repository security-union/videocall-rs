// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dedicated meeting settings page — the full management hub for a meeting.

use crate::auth::check_session;
use crate::components::toggle_switch::ToggleSwitch;
use crate::constants::oauth_enabled;
use crate::meeting_api::{
    delete_meeting, end_meeting, get_meeting_info, update_meeting, MeetingInfo,
};
use crate::routing::Route;
use dioxus::prelude::*;
use videocall_theme::{
    Badge, BadgeTone, Button, ButtonVariant, Card, IconButton, ListFooter, ListGroup, ListHeader,
    ListRow, ListRowLabel, ListRowValue, Spinner, Toolbar,
};
use web_sys::window;

/// Shared page shell: a grouped settings screen that follows the system
/// appearance. All early-return states use this wrapper so the page always
/// looks consistent.
fn page_shell(inner: Element) -> Element {
    rsx! {
        div { class: "surface-grouped settings-page",
            div { class: "settings-container", {inner} }
        }
    }
}

#[component]
pub fn MeetingSettingsPage(id: String) -> Element {
    let navigator = use_navigator();
    let mut auth_checked = use_signal(|| false);
    let mut meeting = use_signal(|| None::<MeetingInfo>);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| None::<String>);
    let mut waiting_room_toggle = use_signal(|| false);
    let mut saving = use_signal(|| false);
    let mut toggle_error = use_signal(|| None::<String>);
    let mut ending = use_signal(|| false);
    let mut deleting = use_signal(|| false);

    // Auth check
    use_effect(move || {
        if oauth_enabled().unwrap_or(false) {
            wasm_bindgen_futures::spawn_local(async move {
                match check_session().await {
                    Ok(_) => auth_checked.set(true),
                    Err(_) => {
                        if let Some(win) = window() {
                            let _ = win.location().set_href("/login");
                        }
                    }
                }
            });
        } else {
            auth_checked.set(true);
        }
    });

    // Fetch meeting info once auth is confirmed
    {
        let meeting_id = id.clone();
        use_effect(move || {
            if !auth_checked() {
                return;
            }
            let meeting_id = meeting_id.clone();
            spawn(async move {
                match get_meeting_info(&meeting_id).await {
                    Ok(info) => {
                        waiting_room_toggle.set(info.waiting_room_enabled);
                        meeting.set(Some(info));
                        loading.set(false);
                    }
                    Err(e) => {
                        loading.set(false);
                        error.set(Some(format!("{e}")));
                    }
                }
            });
        });
    }

    // Auth loading
    if !auth_checked() && oauth_enabled().unwrap_or(false) {
        return page_shell(rsx! {
            div { class: "settings-state",
                Spinner { large: true, label: None }
                span { "Checking authentication..." }
            }
        });
    }

    // Data loading
    if loading() {
        return page_shell(rsx! {
            div { class: "settings-state",
                Spinner { large: true, label: None }
                span { "Loading meeting..." }
            }
        });
    }

    // Error
    if let Some(err) = error() {
        return page_shell(rsx! {
            Card { class: "settings-state",
                svg {
                    xmlns: "http://www.w3.org/2000/svg", width: "48", height: "48",
                    view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                    class: "settings-state-icon",
                    stroke_width: "1.5", stroke_linecap: "round", stroke_linejoin: "round",
                    circle { cx: "12", cy: "12", r: "10" }
                    line { x1: "12", y1: "8", x2: "12", y2: "12" }
                    line { x1: "12", y1: "16", x2: "12.01", y2: "16" }
                }
                h2 { "Unable to load meeting" }
                p { "{err}" }
                Button {
                    onclick: move |_| { navigator.push(Route::Home {}); },
                    "Back to Home"
                }
            }
        });
    }

    // Not found
    let Some(info) = meeting() else {
        return page_shell(rsx! {
            Card { class: "settings-state",
                h2 { "Meeting not found" }
                Button {
                    onclick: move |_| { navigator.push(Route::Home {}); },
                    "Back to Home"
                }
            }
        });
    };

    let state_tone = match info.state.as_str() {
        "active" => BadgeTone::Success,
        "idle" => BadgeTone::Warning,
        _ => BadgeTone::Neutral,
    };
    let is_ended = info.state == "ended";
    let is_active = info.state == "active";

    // Compute display strings for stats
    let duration_str = info
        .ended_at
        .map(|ended| format_duration(ended - info.started_at));
    let started_str = format_time(info.started_at);
    let ended_str = info.ended_at.map(format_time);
    let participant_count = info.participant_count;
    let waiting_count = info.waiting_count;

    let meeting_id_toggle = id.clone();
    let meeting_id_join = id.clone();
    let meeting_id_end = id.clone();
    let meeting_id_delete = id.clone();

    let on_toggle_waiting_room = move |new_val: bool| {
        if saving() {
            return;
        }
        toggle_error.set(None);
        waiting_room_toggle.set(new_val);
        saving.set(true);
        let meeting_id = meeting_id_toggle.clone();
        spawn(async move {
            match update_meeting(&meeting_id, new_val).await {
                Ok(updated) => {
                    waiting_room_toggle.set(updated.waiting_room_enabled);
                    saving.set(false);
                }
                Err(e) => {
                    log::error!("Failed to update waiting room: {e}");
                    waiting_room_toggle.set(!new_val);
                    saving.set(false);
                    toggle_error.set(Some(format!("Failed to update setting: {e}")));
                }
            }
        });
    };

    let on_join = move |_| {
        navigator.push(Route::Meeting {
            id: meeting_id_join.clone(),
        });
    };

    let on_end_meeting = move |_| {
        if ending() {
            return;
        }
        let confirmed = window()
            .and_then(|w| {
                w.confirm_with_message("End this meeting for all participants?")
                    .ok()
            })
            .unwrap_or(false);
        if !confirmed {
            return;
        }
        ending.set(true);
        let meeting_id = meeting_id_end.clone();
        spawn(async move {
            match end_meeting(&meeting_id).await {
                Ok(updated) => {
                    meeting.set(Some(updated));
                    ending.set(false);
                }
                Err(e) => {
                    log::error!("Failed to end meeting: {e}");
                    ending.set(false);
                    error.set(Some(format!("Failed to end meeting: {e}")));
                }
            }
        });
    };

    let on_delete = move |_| {
        if deleting() {
            return;
        }
        let confirmed = window()
            .and_then(|w| {
                w.confirm_with_message(
                    "Are you sure you want to delete this meeting? This cannot be undone.",
                )
                .ok()
            })
            .unwrap_or(false);
        if !confirmed {
            return;
        }
        deleting.set(true);
        let meeting_id = meeting_id_delete.clone();
        spawn(async move {
            match delete_meeting(&meeting_id).await {
                Ok(_) => {
                    navigator.push(Route::Home {});
                }
                Err(e) => {
                    log::error!("Failed to delete meeting: {e}");
                    deleting.set(false);
                    error.set(Some(format!("Failed to delete: {e}")));
                }
            }
        });
    };

    page_shell(rsx! {
        // Back navigation and title
        Toolbar { header: true, class: "settings-toolbar",
            IconButton {
                label: "Back to home",
                onclick: move |_| { navigator.push(Route::Home {}); },
                title: "Back to home",
                svg {
                    xmlns: "http://www.w3.org/2000/svg", width: "20", height: "20",
                    view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                    stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                    polyline { points: "15 18 9 12 15 6" }
                }
            }
            h1 { class: "toolbar-title", "Meeting Settings" }
            // Keeps the title centered against the back button.
            span { class: "settings-toolbar-spacer", "aria-hidden": "true" }
        }

        // Details
        section { class: "settings-group",
            ListHeader { "Details" }
            ListGroup {
                ListRow {
                    ListRowLabel { "Meeting ID" }
                    ListRowValue { class: "text-mono", "{info.meeting_id}" }
                }
                ListRow {
                    ListRowLabel { "Status" }
                    ListRowValue {
                        Badge { tone: state_tone, "{info.state}" }
                    }
                }
                if let Some(host) = &info.host_display_name {
                    ListRow {
                        ListRowLabel { "Host" }
                        ListRowValue { "{host}" }
                    }
                }
            }
        }

        // Activity
        section { class: "settings-group",
            ListHeader { "Activity" }
            ListGroup {
                ListRow {
                    svg {
                        xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                        view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                        stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                        path { d: "M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" }
                        circle { cx: "9", cy: "7", r: "4" }
                        path { d: "M23 21v-2a4 4 0 0 0-3-3.87" }
                        path { d: "M16 3.13a4 4 0 0 1 0 7.75" }
                    }
                    ListRowLabel { "Participants" }
                    ListRowValue { "{participant_count}" }
                }

                if is_active && waiting_count > 0 {
                    ListRow { class: "settings-row-warning",
                        svg {
                            xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                            view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                            stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                            circle { cx: "12", cy: "12", r: "10" }
                            line { x1: "12", y1: "8", x2: "12", y2: "12" }
                            line { x1: "12", y1: "16", x2: "12.01", y2: "16" }
                        }
                        ListRowLabel { "Waiting" }
                        ListRowValue { "{waiting_count}" }
                    }
                }

                if let Some(ref dur) = duration_str {
                    ListRow {
                        svg {
                            xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                            view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                            stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                            circle { cx: "12", cy: "12", r: "10" }
                            polyline { points: "12 6 12 12 16 14" }
                        }
                        ListRowLabel { "Duration" }
                        ListRowValue { "{dur}" }
                    }
                }

                ListRow {
                    svg {
                        xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                        view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                        stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                        rect { x: "3", y: "4", width: "18", height: "18", rx: "2", ry: "2" }
                        line { x1: "16", y1: "2", x2: "16", y2: "6" }
                        line { x1: "8", y1: "2", x2: "8", y2: "6" }
                        line { x1: "3", y1: "10", x2: "21", y2: "10" }
                    }
                    ListRowLabel { if is_ended { "Time" } else { "Started" } }
                    ListRowValue {
                        "{started_str}"
                        if let Some(ref ended) = ended_str {
                            span { " – {ended}" }
                        }
                    }
                }
            }
        }

        // Options
        section { class: "settings-group",
            ListHeader { "Options" }
            ListGroup {
                ListRow {
                    ListRowLabel { id: "settings-waiting-room-label", "Waiting Room" }
                    ListRowValue { class: "settings-option-controls",
                        span {
                            class: "settings-info-icon",
                            title: "Participants must be admitted by the host before joining",
                            svg {
                                xmlns: "http://www.w3.org/2000/svg", width: "15", height: "15",
                                view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                                stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                                circle { cx: "12", cy: "12", r: "10" }
                                line { x1: "12", y1: "16", x2: "12", y2: "12" }
                                line { x1: "12", y1: "8", x2: "12.01", y2: "8" }
                            }
                        }
                        ToggleSwitch {
                            enabled: waiting_room_toggle(),
                            on_toggle: on_toggle_waiting_room,
                            disabled: saving(),
                            labelled_by: "settings-waiting-room-label",
                        }
                    }
                }
            }
            if let Some(err) = toggle_error() {
                ListFooter { class: "settings-footer-error", role: "alert", "{err}" }
            }
        }

        // Actions
        section { class: "settings-group",
            ListHeader { "Actions" }
            div { class: "settings-actions",
                // Start (idle/ended) or Join (active)
                Button {
                    variant: ButtonVariant::Prominent,
                    block: true,
                    onclick: on_join,
                    if info.state == "active" {
                        svg {
                            xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                            view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                            stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                            path { d: "M15 10l5 5-5 5" }
                            path { d: "M20 15H8" }
                            path { d: "M4 4v16" }
                        }
                        span { "Join Meeting" }
                    } else {
                        svg {
                            xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                            view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                            stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                            polygon { points: "5 3 19 12 5 21 5 3" }
                        }
                        span { "Start Meeting" }
                    }
                }

                // End Meeting: only when meeting is active or idle
                if !is_ended {
                    Button {
                        variant: ButtonVariant::Tinted,
                        block: true,
                        class: "settings-btn-warning",
                        disabled: ending(),
                        onclick: on_end_meeting,
                        if ending() {
                            Spinner { label: None }
                            span { "Ending..." }
                        } else {
                            svg {
                                xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                                view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                                stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                                rect { x: "3", y: "3", width: "18", height: "18", rx: "2" }
                            }
                            span { "End Meeting" }
                        }
                    }
                }
            }
        }

        // Danger zone: its own group, always available
        section { class: "settings-group",
            if !is_ended {
                ListHeader { "Danger Zone" }
            }
            div { class: "settings-actions",
                Button {
                    variant: ButtonVariant::Destructive,
                    block: true,
                    disabled: deleting(),
                    onclick: on_delete,
                    if deleting() {
                        Spinner { label: None }
                        span { "Deleting..." }
                    } else {
                        svg {
                            xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                            view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                            stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                            polyline { points: "3 6 5 6 21 6" }
                            path { d: "M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" }
                            line { x1: "10", y1: "11", x2: "10", y2: "17" }
                            line { x1: "14", y1: "11", x2: "14", y2: "17" }
                        }
                        span { "Delete Meeting" }
                    }
                }
            }
        }
    })
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

// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dedicated meeting settings page — the full management hub for a meeting.

use crate::auth::{check_session, redirect_to_login};
use crate::components::toggle_switch::ToggleSwitch;
use crate::constants::oauth_enabled;
use crate::meeting_api::{
    delete_meeting, end_meeting, get_meeting_info, update_meeting, MeetingInfo,
};
use crate::routing::Route;
use dioxus::prelude::*;
use web_sys::window;

/// Shared page shell — hero-container with floating gradient orbs, matching
/// the homepage layout.  All early-return states use this wrapper so the page
/// always looks consistent.
fn page_shell(inner: Element) -> Element {
    rsx! {
        div { class: "hero-container",
            div { class: "floating-element floating-element-1" }
            div { class: "floating-element floating-element-2" }
            div { class: "floating-element floating-element-3" }
            div { class: "hero-content", {inner} }
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
                        // Encode the current settings page URL as `returnTo`
                        // so the user is sent directly back here after sign-in.
                        redirect_to_login();
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
            div { class: "settings-loading-state",
                span { class: "loading-spinner" }
                span { "Checking authentication..." }
            }
        });
    }

    // Data loading
    if loading() {
        return page_shell(rsx! {
            div { class: "settings-loading-state",
                span { class: "loading-spinner" }
                span { "Loading meeting..." }
            }
        });
    }

    // Error
    if let Some(err) = error() {
        return page_shell(rsx! {
            div { class: "settings-empty-state",
                svg {
                    xmlns: "http://www.w3.org/2000/svg", width: "48", height: "48",
                    view_box: "0 0 24 24", fill: "none", stroke: "var(--warning)",
                    stroke_width: "1.5", stroke_linecap: "round", stroke_linejoin: "round",
                    circle { cx: "12", cy: "12", r: "10" }
                    line { x1: "12", y1: "8", x2: "12", y2: "12" }
                    line { x1: "12", y1: "16", x2: "12.01", y2: "16" }
                }
                h2 { "Unable to load meeting" }
                p { "{err}" }
                button {
                    class: "btn-apple btn-secondary",
                    onclick: move |_| { navigator.push(Route::Home {}); },
                    "Back to Home"
                }
            }
        });
    }

    // Not found
    let Some(info) = meeting() else {
        return page_shell(rsx! {
            div { class: "settings-empty-state",
                h2 { "Meeting not found" }
                button {
                    class: "btn-apple btn-secondary",
                    onclick: move |_| { navigator.push(Route::Home {}); },
                    "Back to Home"
                }
            }
        });
    };

    let state_class = match info.state.as_str() {
        "active" => "state-active",
        "idle" => "state-idle",
        _ => "state-ended",
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
        // Back navigation
        div { class: "settings-header",
            button {
                class: "settings-back-btn",
                onclick: move |_| { navigator.push(Route::Home {}); },
                title: "Back to home",
                svg {
                    xmlns: "http://www.w3.org/2000/svg", width: "20", height: "20",
                    view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                    stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                    polyline { points: "15 18 9 12 15 6" }
                }
                span { "Back" }
            }
        }

        h1 { class: "hero-title text-center", "Meeting Settings" }

        div { class: "content-separator" }

        // ── Details card (compact) ──
        div { class: "settings-card settings-card-compact",
            h3 { class: "settings-card-title", "Details" }

            div { class: "settings-field-compact",
                span { class: "settings-field-label", "Meeting ID" }
                span { class: "settings-field-value settings-field-mono", "{info.meeting_id}" }
            }

            div { class: "settings-field-compact",
                span { class: "settings-field-label", "Status" }
                span { class: "meeting-state {state_class}", "{info.state}" }
            }

            if let Some(host) = &info.host_display_name {
                div { class: "settings-field-compact",
                    span { class: "settings-field-label", "Host" }
                    span { class: "settings-field-value", "{host}" }
                }
            }
        }

        // ── Activity card (compact rows) ──
        div { class: "settings-card settings-card-compact",
            h3 { class: "settings-card-title", "Activity" }

            div { class: "settings-stat-row",
                svg {
                    xmlns: "http://www.w3.org/2000/svg", width: "14", height: "14",
                    view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                    stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                    path { d: "M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" }
                    circle { cx: "9", cy: "7", r: "4" }
                    path { d: "M23 21v-2a4 4 0 0 0-3-3.87" }
                    path { d: "M16 3.13a4 4 0 0 1 0 7.75" }
                }
                span { class: "settings-stat-label", "Participants" }
                span { class: "settings-stat-value", "{participant_count}" }
            }

            if is_active && waiting_count > 0 {
                div { class: "settings-stat-row settings-stat-warning",
                    svg {
                        xmlns: "http://www.w3.org/2000/svg", width: "14", height: "14",
                        view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                        stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                        circle { cx: "12", cy: "12", r: "10" }
                        line { x1: "12", y1: "8", x2: "12", y2: "12" }
                        line { x1: "12", y1: "16", x2: "12.01", y2: "16" }
                    }
                    span { class: "settings-stat-label", "Waiting" }
                    span { class: "settings-stat-value", "{waiting_count}" }
                }
            }

            if let Some(ref dur) = duration_str {
                div { class: "settings-stat-row",
                    svg {
                        xmlns: "http://www.w3.org/2000/svg", width: "14", height: "14",
                        view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                        stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                        circle { cx: "12", cy: "12", r: "10" }
                        polyline { points: "12 6 12 12 16 14" }
                    }
                    span { class: "settings-stat-label", "Duration" }
                    span { class: "settings-stat-value", "{dur}" }
                }
            }

            div { class: "settings-stat-row",
                svg {
                    xmlns: "http://www.w3.org/2000/svg", width: "14", height: "14",
                    view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                    stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                    rect { x: "3", y: "4", width: "18", height: "18", rx: "2", ry: "2" }
                    line { x1: "16", y1: "2", x2: "16", y2: "6" }
                    line { x1: "8", y1: "2", x2: "8", y2: "6" }
                    line { x1: "3", y1: "10", x2: "21", y2: "10" }
                }
                span { class: "settings-stat-label", if is_ended { "Time" } else { "Started" } }
                span { class: "settings-stat-value",
                    "{started_str}"
                    if let Some(ref ended) = ended_str {
                        span { class: "settings-stat-separator", " – {ended}" }
                    }
                }
            }
        }

        // ── Options card ──
        div { class: "settings-card",
            h3 { class: "settings-card-title", "Options" }

            div { class: "settings-option-row",
                span { class: "settings-option-label", "Waiting Room" }
                div { class: "settings-option-controls",
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
                    }
                }
            }

            if let Some(err) = toggle_error() {
                p { class: "toggle-error",
                    "{err}"
                }
            }
        }

        // ── Actions card ──
        div { class: "settings-card",
            h3 { class: "settings-card-title", "Actions" }

            // Start (idle/ended) or Join (active)
            div { class: "settings-action-row",
                button {
                    class: "btn-apple btn-primary settings-action-btn",
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
            }

            // End Meeting — only when meeting is active or idle
            if !is_ended {
                div { class: "settings-action-row",
                    button {
                        class: "btn-apple btn-warning settings-action-btn",
                        disabled: ending(),
                        onclick: on_end_meeting,
                        if ending() {
                            span { class: "loading-spinner" }
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

            // Danger zone divider — only show when there are actions above it
            if !is_ended {
                div { class: "settings-danger-zone",
                    div { class: "settings-danger-divider" }
                    span { class: "settings-danger-label", "Danger Zone" }
                    div { class: "settings-danger-divider" }
                }
            }

            // Delete — always available, but visually separated
            div { class: "settings-action-row",
                button {
                    class: "btn-apple btn-danger settings-action-btn",
                    disabled: deleting(),
                    onclick: on_delete,
                    if deleting() {
                        span { class: "loading-spinner" }
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

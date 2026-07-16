// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dedicated meeting settings page — the full management hub for a meeting.

use crate::auth::{check_session, redirect_to_login};
use crate::components::meeting_format::{
    format_datetime_zoned, format_duration, meeting_activity_duration_ms,
};
use crate::constants::oauth_enabled;
use crate::meeting_api::{delete_meeting, end_meeting, get_meeting_info, MeetingInfo};
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
    let mut admitted_can_admit_toggle = use_signal(|| false);
    let mut end_on_host_leave_toggle = use_signal(|| true);
    let mut allow_guests_toggle = use_signal(|| false);
    let saving = use_signal(|| false);
    let toggle_error = use_signal(|| None::<String>);
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
                        admitted_can_admit_toggle.set(info.admitted_can_admit);
                        end_on_host_leave_toggle.set(info.end_on_host_leave);
                        allow_guests_toggle.set(info.allow_guests);
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

    // Periodic refresh of the live read-only stats (participant_count,
    // waiting_count, state, duration). The backend now reports the ACTUAL
    // currently-present participant count, but the page would otherwise show
    // whatever it was at load time. A `use_future` poll keeps it current.
    //
    // Lifecycle / cleanup: `use_future` ties this loop to the component scope.
    // When the user navigates away the scope unmounts, Dioxus drops the future,
    // and the in-flight `TimeoutFuture::new(...).await` is cancelled — so the
    // loop self-terminates. No manual timer handle to leak (mirrors the
    // self-cancelling poll in `decode_budget_banner.rs`).
    //
    // Interval: 12s. This is a low-traffic settings page, not the media hot
    // path; 12s keeps the count fresh without hammering the meeting-api.
    //
    // Stomping safety: we ONLY rewrite the read-only stat fields onto a clone
    // of the current `meeting`. We deliberately do NOT touch the toggle-backed
    // signals (waiting_room_toggle / admitted_can_admit_toggle /
    // end_on_host_leave_toggle / allow_guests_toggle) — those are the live
    // source of truth for the optimistic toggle UI, and re-syncing them here
    // would clobber an in-flight edit. We also skip the refresh entirely while
    // a toggle save is in flight (`saving()`), and stop polling once the
    // meeting has ended (its count is final).
    {
        let meeting_id = id.clone();
        use_future(move || {
            let meeting_id = meeting_id.clone();
            async move {
                // 12s cadence — sensible for a low-traffic settings page.
                const REFRESH_INTERVAL_MS: u32 = 12_000;
                loop {
                    gloo_timers::future::TimeoutFuture::new(REFRESH_INTERVAL_MS).await;

                    // Don't poll before the first load resolves, while an error
                    // is showing, or while a toggle save is in flight (avoid
                    // racing the optimistic toggle UI). Use `.peek()` so these
                    // reads inside the long-lived poll don't create reactive
                    // subscriptions (mirrors `decode_budget_banner.rs`).
                    if *loading.peek() || error.peek().is_some() || *saving.peek() {
                        continue;
                    }

                    // Stop polling once the meeting has ended — its stats are
                    // final and won't change.
                    if meeting.peek().as_ref().map(|m| m.state == "ended") == Some(true) {
                        break;
                    }

                    match get_meeting_info(&meeting_id).await {
                        Ok(fresh) => {
                            // Re-check guards after the await: a save may have
                            // started, or the user may have ended the meeting,
                            // while the request was in flight.
                            if *saving.peek() {
                                continue;
                            }
                            // Update ONLY the read-only stat fields on a clone
                            // of the current meeting. This leaves the
                            // toggle-backed booleans (and their signals)
                            // untouched, so an in-flight optimistic toggle is
                            // never stomped by a stale server snapshot.
                            let current = meeting.peek().clone();
                            if let Some(mut current) = current {
                                current.state = fresh.state;
                                current.participant_count = fresh.participant_count;
                                current.waiting_count = fresh.waiting_count;
                                current.started_at = fresh.started_at;
                                current.ended_at = fresh.ended_at;
                                current.host_display_name = fresh.host_display_name;
                                meeting.set(Some(current));
                            }
                        }
                        Err(e) => {
                            // A transient refresh failure is non-fatal: keep the
                            // last-known stats on screen and try again next tick.
                            // Do NOT surface it as a page error (that would hide
                            // the whole settings UI behind the error state).
                            log::warn!("meeting stats refresh failed: {e}");
                        }
                    }
                }
            }
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

    // Compute display strings for stats.
    // issue 1672: duration is shown for EVERY state — running (now - started)
    // while the meeting is open, final (ended - started) once it has ended —
    // so it is always a String rather than an Option.
    let now_ms = js_sys::Date::now() as i64;
    let duration_str = format_duration(meeting_activity_duration_ms(
        info.started_at,
        info.ended_at,
        now_ms,
    ));
    let started_str = format_datetime_zoned(info.started_at);
    let ended_str = info.ended_at.map(format_datetime_zoned);
    let participant_count = info.participant_count;
    let waiting_count = info.waiting_count;

    let meeting_id_join = id.clone();
    let meeting_id_end = id.clone();
    let meeting_id_delete = id.clone();
    let meeting_id_guest_link = id.clone();
    let meeting_id_options = id.clone();

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

            // issue 1672: three separate labeled field-lines (Started, then Ended
            // for ended meetings, then Duration) replace the old single-line
            // "started – ended" range that overflowed the dialog at narrow widths.
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
                span { class: "settings-stat-label", "Started" }
                span { class: "settings-stat-value settings-stat-value--time", "{started_str}" }
            }

            if let Some(ref ended) = ended_str {
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
                    span { class: "settings-stat-label", "Ended" }
                    span { class: "settings-stat-value settings-stat-value--time", "{ended}" }
                }
            }

            div { class: "settings-stat-row",
                svg {
                    xmlns: "http://www.w3.org/2000/svg", width: "14", height: "14",
                    view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                    stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                    circle { cx: "12", cy: "12", r: "10" }
                    polyline { points: "12 6 12 12 16 14" }
                }
                span { class: "settings-stat-label", "Duration" }
                span { class: "settings-stat-value", "{duration_str}" }
            }
        }

        // ── Options card ──
        div { class: "settings-card",
            h3 { class: "settings-card-title", "Options" }

            crate::components::meeting_options_controls::MeetingOptionsControls {
                meeting_id: meeting_id_options.clone(),
                waiting_room_toggle,
                admitted_can_admit_toggle,
                end_on_host_leave_toggle,
                allow_guests_toggle,
                saving,
                toggle_error,
            }

            if allow_guests_toggle() {
                div { class: "settings-option-row",
                    style: "flex-direction: column; align-items: flex-start; gap: var(--space-1);",
                    span {
                        class: "settings-option-label",
                        // @token-exempt: 0.8rem falls between --fs-3 (12px) and --fs-4 (13px)
                        style: "font-size: 0.8rem; color: var(--text-subtle, rgba(255,255,255,0.5));",
                        "Guest join link:"
                    }
                    {
                        let guest_link = window()
                            .and_then(|w| w.location().origin().ok())
                            .map(|origin| format!("{origin}/meeting/{meeting_id_guest_link}/guest"))
                            .unwrap_or_default();
                        rsx! {
                            span {
                                class: "settings-field-value settings-field-mono settings-guest-link",
                                style: "user-select: all;",
                                "{guest_link}"
                            }
                        }
                    }
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

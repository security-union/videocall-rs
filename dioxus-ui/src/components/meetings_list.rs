/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Home-page "Meetings" section.
//!
//! Renders the union of meetings the authenticated user owns or has been
//! admitted into, ordered server-side by `last_active_at DESC`. Backed by
//! `GET /api/v1/meetings/feed`, which returns the
//! [`MeetingFeedSummary`] type with a server-computed `is_owner` flag.
//!
//! The `is_owner` flag is the **only** authoritative ownership signal in the
//! UI — it gates the inline gold star, the edit and delete buttons, and the
//! "Owner" tooltip line. Inferring ownership from any other field (host
//! email, etc.) is forbidden because two distinct authenticated identities
//! can both legitimately appear in the same row of their respective feeds
//! (one as owner, the other as a previously-admitted guest).
//!
//! See `videocall-meeting-types/src/responses.rs` for full wire-shape docs
//! and `meeting-api/tests/list_feed_tests.rs::test_two_identities_disjoint_is_owner_for_same_meeting`
//! for the regression test that motivated the merger.

use crate::components::login::{do_login, ProviderButton};
use crate::components::meeting_format::format_meeting_state_label;
use crate::constants::meeting_api_client;
use crate::local_storage::{load_bool, save_bool};
use crate::routing::Route;
use dioxus::prelude::*;
use videocall_meeting_types::responses::{ListFeedResponse, MeetingFeedSummary};
use wasm_bindgen::JsCast;

/// `localStorage` key for the merged "Meetings" section's expand/collapse state.
/// Defaults to expanded (`true`) on first load.
const EXPANDED_STORAGE_KEY: &str = "home.meetings.expanded";

/// Legacy key from when the home page rendered "My Meetings" and "Previously
/// Joined" as two separate sections. Read once on first load to migrate the
/// user's existing preference; never written.
const LEGACY_MY_MEETINGS_EXPANDED_KEY: &str = "home.my-meetings.expanded";

enum FetchMeetingsError {
    Unauthenticated,
    Other(String),
}

/// Read the merged section's expand state, falling back to the legacy
/// "My Meetings" key for migration. Defaults to expanded (`true`).
fn load_merged_expanded_default() -> bool {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        if let Ok(Some(_)) = storage.get_item(EXPANDED_STORAGE_KEY) {
            // New key set — defer to load_bool with the standard default.
            return load_bool(EXPANDED_STORAGE_KEY, true);
        }
        // No new key — try the legacy "My Meetings" key for migration.
        if let Ok(Some(v)) = storage.get_item(LEGACY_MY_MEETINGS_EXPANDED_KEY) {
            return matches!(v.as_str(), "true");
        }
    }
    // Storage unavailable or no preference set — default to expanded.
    true
}

#[component]
pub fn MeetingsList(on_select_meeting: Option<EventHandler<String>>) -> Element {
    let mut meetings = use_signal(Vec::<MeetingFeedSummary>::new);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| None::<String>);
    let mut unauthenticated = use_signal(|| false);
    let mut expanded = use_signal(load_merged_expanded_default);

    #[allow(unused_mut)]
    let mut fetch_meetings = move || {
        loading.set(true);
        error.set(None);
        unauthenticated.set(false);

        spawn(async move {
            match do_fetch_feed().await {
                Ok(response) => {
                    meetings.set(response.meetings);
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
                span { "Meetings" }
                span { class: "meeting-count", "({meetings().len()})" }
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
                                        let mut fetch_meetings = fetch_meetings;
                                        move |meeting_id: String| {
                                            // Optimistic removal so the row disappears immediately
                                            // even though the next refetch should not include it.
                                            // The count badge in the header derives from
                                            // `meetings().len()` and updates automatically.
                                            meetings.write().retain(|m| m.meeting_id != meeting_id);

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
    meeting: MeetingFeedSummary,
    on_select_meeting: Option<EventHandler<String>>,
    on_delete: EventHandler<String>,
) -> Element {
    let nav = use_navigator();
    let is_active = meeting.state == "active";
    let is_ended = meeting.state == "ended";
    let is_owner = meeting.is_owner;
    let state_class = match meeting.state.as_str() {
        "active" => "state-active",
        "idle" => "state-idle",
        _ => "state-ended",
    };

    let duration_ms = compute_duration_ms(&meeting, is_active);

    let meeting_id = meeting.meeting_id.clone();
    let meeting_id_click = meeting_id.clone();
    let meeting_id_delete = meeting_id.clone();

    // The hover tooltip is portaled to <body>, so it survives MeetingItem
    // unmounts. Every click handler that mutates application state, navigates,
    // or opens a modal must dismiss the tooltip first — otherwise it lingers
    // on top of whatever the user is doing next. The unmount cleanup below
    // (`use_drop`) covers external navigations that bypass these handlers.
    let on_click = move |_| {
        hide_meeting_info_tooltip();
        if let Some(ref callback) = on_select_meeting {
            callback.call(meeting_id_click.clone());
        } else {
            nav.push(Route::Meeting {
                id: meeting_id_click.clone(),
            });
        }
    };

    let on_delete_click = move |e: MouseEvent| {
        // Hide the body-portaled tooltip BEFORE the confirm() dialog so it
        // doesn't sit behind the OS-style modal. Done before
        // `stop_propagation()` so dismissal happens even if propagation fails.
        hide_meeting_info_tooltip();
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
        // Hide the body-portaled tooltip BEFORE navigating away.
        hide_meeting_info_tooltip();
        e.stop_propagation();
        nav.push(Route::MeetingSettings {
            id: meeting_id_edit.clone(),
        });
    };

    // Catches navigations that bypass the row's own click handlers — e.g. the
    // user opens the search modal, clicks a header link, signs out, or hits
    // the browser back button. When MeetingItem unmounts, the body-level
    // tooltip portal stays in the DOM with `is-visible` set; this drop hook
    // strips that class so the tooltip vanishes with the row.
    //
    // Layer 3 (window-level pagehide/visibilitychange listeners) was
    // considered but skipped — Layer 2 already fires on every component
    // teardown including route changes, so the marginal coverage of a
    // browser-back inside the same SPA route isn't worth the global handler.
    use_drop(hide_meeting_info_tooltip);

    let tooltip_html = build_meeting_tooltip_html(&meeting, is_active, is_ended, duration_ms);
    let tooltip_html_for_show = tooltip_html.clone();

    rsx! {
        li {
            class: if is_ended { "meeting-item meeting-ended" } else { "meeting-item" },
            onmouseenter: move |e: MouseEvent| {
                let coords = e.client_coordinates();
                show_meeting_info_tooltip(coords.x, coords.y, &tooltip_html_for_show);
            },
            onmousemove: move |e: MouseEvent| {
                let coords = e.client_coordinates();
                position_meeting_info_tooltip(coords.x, coords.y);
            },
            onmouseleave: move |_| hide_meeting_info_tooltip(),
            div { class: "meeting-item-content", onclick: on_click,
                div { class: "meeting-info",
                    span { class: "meeting-id", "{meeting.meeting_id}" }
                    if is_owner {
                        span {
                            class: "meeting-owner-icon",
                            aria_label: "Owner",
                            title: "Owner",
                            svg {
                                xmlns: "http://www.w3.org/2000/svg",
                                width: "14",
                                height: "14",
                                view_box: "0 0 24 24",
                                fill: "currentColor",
                                stroke: "none",
                                "aria-hidden": "true",
                                "focusable": "false",
                                path { d: "M12 2l2.39 7.36H22l-6.18 4.49L18.21 22 12 17.27 5.79 22l2.39-8.15L2 9.36h7.61L12 2z" }
                            }
                        }
                    }
                    {
                        let state_label = format_meeting_state_label(&meeting.state);
                        rsx! {
                            span { class: "meeting-state {state_class}", "{state_label}" }
                        }
                    }
                }
            }
            // Edit / delete affordances are gated on the server-computed
            // `is_owner` flag — we never infer ownership from any other field.
            if is_owner {
                button {
                    class: "meeting-edit-btn",
                    onclick: on_edit_click,
                    title: "Meeting settings",
                    svg {
                        xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                        view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                        stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                        path { d: "M12 20h9" }
                        path { d: "M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z" }
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
}

/// Compute the elapsed/total duration of the meeting in milliseconds.
///
/// Public-but-internal helper kept testable: `started_at` is `Option<i64>` on
/// `MeetingFeedSummary`, so we have to decide what to render for idle
/// meetings that have never been activated. We mirror the prior
/// `JoinedMeetingSummary` semantics: when `started_at` is missing we report
/// zero (no duration to display).
fn compute_duration_ms(meeting: &MeetingFeedSummary, is_active: bool) -> i64 {
    let Some(started_at) = meeting.started_at else {
        return 0;
    };
    if is_active {
        let now_ms = js_sys::Date::now() as i64;
        (now_ms - started_at).max(0)
    } else {
        meeting
            .ended_at
            .map(|ended_at| ended_at - started_at)
            .unwrap_or(0)
    }
}

async fn do_fetch_feed() -> Result<ListFeedResponse, FetchMeetingsError> {
    let client = meeting_api_client()
        .map_err(|e| FetchMeetingsError::Other(format!("Config error: {e}")))?;
    // Pass `None` so the server applies its default cap (200). Bumping that
    // cap is intentionally a server-side decision, not a client query knob.
    client.list_meeting_feed(None).await.map_err(|e| match e {
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

/// Build the inner HTML for the meeting-info hover tooltip.
///
/// When `meeting.is_owner` is true an "Owner" line is injected at the very
/// top of the metadata table (gold-tinted to match the inline star icon).
/// Non-owner rows skip that line entirely, so the tooltip starts with the
/// usual created/started/duration metadata.
///
/// SECURITY INVARIANT: The HTML body produced here MUST NOT contain any
/// caller-controlled or server-supplied string content (user display names,
/// meeting titles, OAuth profile names, etc). Currently the only interpolated
/// values are static labels ("Owner", "Created on", etc.) and outputs of
/// `format_datetime` / `format_duration`, both of which produce bounded
/// numeric/date strings. If you add a row that includes a user-supplied value,
/// HTML-escape it (e.g. via `web_sys::Document::create_text_node` + structured
/// DOM construction) instead of injecting via `set_inner_html`.
pub(crate) fn build_meeting_tooltip_html(
    meeting: &MeetingFeedSummary,
    is_active: bool,
    is_ended: bool,
    duration_ms: i64,
) -> String {
    use crate::components::meeting_format::{format_datetime, format_duration};
    let mut rows: Vec<String> = Vec::new();
    if meeting.is_owner {
        // The "Owner" tooltip row mirrors the visual treatment of the inline
        // star icon — same gold tint, same star glyph — and sits at the very
        // top of the metadata table so it's the first signal users see.
        rows.push(owner_tooltip_row());
    }
    rows.push(tooltip_row(
        "Created on",
        &format_datetime(meeting.created_at),
    ));
    if is_active {
        if let Some(started_at) = meeting.started_at {
            rows.push(tooltip_row("Started on", &format_datetime(started_at)));
        }
        rows.push(tooltip_row("Duration", &format_duration(duration_ms)));
        rows.push(tooltip_row(
            "Attendees",
            &meeting.participant_count.to_string(),
        ));
        if meeting.waiting_count > 0 {
            rows.push(tooltip_row("Waiting", &meeting.waiting_count.to_string()));
        }
    } else if is_ended {
        if let Some(started_at) = meeting.started_at {
            rows.push(tooltip_row("Last active on", &format_datetime(started_at)));
        }
        rows.push(tooltip_row("Duration", &format_duration(duration_ms)));
    } else if let Some(started_at) = meeting.started_at {
        rows.push(tooltip_row("Last active on", &format_datetime(started_at)));
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

/// Owner row at the top of the tooltip. Static SVG star + the literal "Owner"
/// label, gold-tinted via the `meeting-info-tooltip-row--owner` modifier.
fn owner_tooltip_row() -> String {
    "<div class=\"meeting-info-tooltip-row meeting-info-tooltip-row--owner\">\
     <span class=\"meeting-info-tooltip-label meeting-info-tooltip-label--owner\">\
     <svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\" viewBox=\"0 0 24 24\" fill=\"currentColor\" stroke=\"none\" aria-hidden=\"true\" focusable=\"false\">\
     <path d=\"M12 2l2.39 7.36H22l-6.18 4.49L18.21 22 12 17.27 5.79 22l2.39-8.15L2 9.36h7.61L12 2z\"/>\
     </svg>\
     Owner\
     </span>\
     </div>"
        .to_string()
}

/// Get-or-create the body-level tooltip element. Mirrors the pattern in `signal_quality.rs`.
fn get_or_create_meeting_tooltip_el() -> web_sys::HtmlElement {
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

fn show_meeting_info_tooltip(x: f64, y: f64, html: &str) {
    let el = get_or_create_meeting_tooltip_el();
    el.set_inner_html(html);
    position_meeting_info_tooltip(x, y);
    let _ = el.class_list().add_1("is-visible");
}

fn position_meeting_info_tooltip(x: f64, y: f64) {
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

fn hide_meeting_info_tooltip() {
    if let Some(el) = gloo_utils::document().get_element_by_id("meeting-info-tooltip-global") {
        let html_el: web_sys::HtmlElement = el.unchecked_into();
        let _ = html_el.class_list().remove_1("is-visible");
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the tooltip-HTML builder.
    //!
    //! `build_meeting_tooltip_html` calls into `meeting_format::format_datetime`,
    //! which is implemented on top of `js_sys::Date` and therefore only works
    //! under wasm. We register the tests with `wasm_bindgen_test` so they
    //! compile on the host target (the macro emits host-target stubs that
    //! never actually execute) and run for real under wasm-pack /
    //! wasm-bindgen-test-runner.
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    fn sample_meeting(is_owner: bool) -> MeetingFeedSummary {
        MeetingFeedSummary {
            meeting_id: "demo-meeting".to_string(),
            state: "active".to_string(),
            last_active_at: 1_714_323_600_000,
            created_at: 1_714_323_000_000,
            started_at: Some(1_714_323_500_000),
            ended_at: None,
            host: Some("alice@example.com".to_string()),
            is_owner,
            participant_count: 2,
            waiting_count: 0,
            has_password: false,
            allow_guests: false,
            waiting_room_enabled: true,
            admitted_can_admit: false,
            end_on_host_leave: true,
        }
    }

    #[wasm_bindgen_test]
    fn tooltip_includes_owner_row_when_owner() {
        let meeting = sample_meeting(true);
        // Active branch — duration is computed against `started_at`, but the
        // exact value is irrelevant for the Owner row check.
        let html = build_meeting_tooltip_html(&meeting, true, false, 60_000);
        assert!(
            html.contains("meeting-info-tooltip-row--owner"),
            "tooltip should include the owner-row marker class for an owned meeting; html: {html}"
        );
        assert!(
            html.contains("Owner"),
            "tooltip should contain the 'Owner' label for an owned meeting; html: {html}"
        );
        // The owner row must appear before any of the standard rows.
        let owner_idx = html
            .find("meeting-info-tooltip-row--owner")
            .expect("owner row must be present");
        let created_idx = html
            .find("Created on")
            .expect("created-on row must be present");
        assert!(
            owner_idx < created_idx,
            "owner row must precede the created-on row"
        );
    }

    #[wasm_bindgen_test]
    fn tooltip_omits_owner_row_when_not_owner() {
        let meeting = sample_meeting(false);
        let html = build_meeting_tooltip_html(&meeting, true, false, 60_000);
        assert!(
            !html.contains("meeting-info-tooltip-row--owner"),
            "tooltip must not include the owner-row marker class for a non-owned meeting; html: {html}"
        );
        assert!(
            !html.contains(">Owner<"),
            "tooltip must not contain the 'Owner' label for a non-owned meeting; html: {html}"
        );
    }

    /// Show then hide the body-portaled tooltip. The bug we fixed was that
    /// click handlers / unmounts didn't dismiss the tooltip — this test
    /// pins the helper contract those handlers rely on.
    ///
    /// We can't drive `mouseenter`/`click` against a fully-mounted Dioxus
    /// `MeetingItem` from this in-file unit test without standing up the
    /// whole feed-fetch harness, so we exercise the same primitives the
    /// component handlers exercise. The integration coverage of the rendered
    /// `MeetingsList` lives in `tests/meetings_list_owner_gating.rs`.
    #[wasm_bindgen_test]
    fn show_then_hide_toggles_is_visible_on_global_portal() {
        // Make sure we start clean — a previous test in the same browser
        // run may have left the portal in place.
        if let Some(prev) = gloo_utils::document().get_element_by_id("meeting-info-tooltip-global")
        {
            prev.parent_node().and_then(|p| p.remove_child(&prev).ok());
        }

        let meeting = sample_meeting(true);
        let html = build_meeting_tooltip_html(&meeting, true, false, 60_000);

        show_meeting_info_tooltip(10.0, 10.0, &html);

        let el = gloo_utils::document()
            .get_element_by_id("meeting-info-tooltip-global")
            .expect("portal element must exist after show()");
        assert!(
            el.class_list().contains("is-visible"),
            "show_meeting_info_tooltip must add the is-visible marker class"
        );

        hide_meeting_info_tooltip();

        let el = gloo_utils::document()
            .get_element_by_id("meeting-info-tooltip-global")
            .expect("portal element must still exist after hide() (only the class flips)");
        assert!(
            !el.class_list().contains("is-visible"),
            "hide_meeting_info_tooltip must strip the is-visible marker class"
        );
    }
}

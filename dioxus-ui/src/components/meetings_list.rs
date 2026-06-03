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
use crate::components::meetings_filter::{
    filter_and_sort_meetings, AttendedWithin, FilterState, SortDir, SortKey, SortState,
};
use crate::constants::meeting_api_client;
use crate::local_storage::{load_bool, load_json, save_bool, save_json};
use crate::routing::Route;
use dioxus::prelude::*;
use videocall_meeting_types::responses::{ListFeedResponse, MeetingFeedSummary};
use wasm_bindgen::JsCast;

/// `localStorage` key for the merged "Meetings" section's expand/collapse state.
/// Defaults to expanded (`true`) on first load.
const EXPANDED_STORAGE_KEY: &str = "home.meetings.expanded";

/// `localStorage` key for the persisted meetings-list filter selections
/// (JSON-serialised [`FilterState`]). Resilient to absent/corrupt values —
/// see [`crate::local_storage::load_json`].
const FILTER_STORAGE_KEY: &str = "home.meetings.filter";

/// `localStorage` key for the persisted meetings-list sort selection
/// (JSON-serialised [`SortState`]).
const SORT_STORAGE_KEY: &str = "home.meetings.sort";

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

    // Filter / sort selections, restored from localStorage on mount and
    // persisted on every change. `load_json` falls back to `Default` for
    // absent/corrupt stored values, so a garbled key can never wedge the UI.
    let mut filter =
        use_signal(|| load_json::<FilterState>(FILTER_STORAGE_KEY, FilterState::default()));
    let mut sort = use_signal(|| load_json::<SortState>(SORT_STORAGE_KEY, SortState::default()));

    // Whether the filter popover / sort dropdown are open. Only one of the two
    // floating panels is open at a time (opening one closes the other).
    let mut filter_open = use_signal(|| false);
    let mut sort_open = use_signal(|| false);

    // Derived list: filter + sort over the in-memory feed. Recomputed only when
    // (meetings, filter, sort) change — NOT on every unrelated re-render. `now`
    // is read once per recompute via the browser clock; the actual filtering is
    // the pure `filter_and_sort_meetings` (host-testable with `now` injected).
    let visible_meetings = use_memo(move || {
        let now_ms = js_sys::Date::now() as i64;
        filter_and_sort_meetings(&meetings(), &filter(), &sort(), now_ms)
    });

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

    // Persist + apply a new filter. Centralised so every checkbox/radio writes
    // through the same path.
    let mut set_filter = move |next: FilterState| {
        filter.set(next);
        save_json(FILTER_STORAGE_KEY, &next);
    };

    // Persist + apply a new sort state.
    let set_sort = move |next: SortState| {
        sort.set(next);
        save_json(SORT_STORAGE_KEY, &next);
    };

    let clear_filters = move |_| {
        set_filter(FilterState::default());
        filter_open.set(false);
    };

    let active_filter_count = filter().active_count();
    // Bind the derived list ONCE per render and reuse it for both the
    // filtered-empty check and the row loop, so the ≤200-row Vec isn't cloned
    // twice on every render (including on popover open/close).
    let visible = visible_meetings();
    // Distinguish the two empty states: the generic "no meetings yet" (feed is
    // genuinely empty) vs. "no meetings match your filters" (feed has rows but
    // the active filter excludes them all). Only the latter is reachable when
    // a filter is active.
    let feed_empty = meetings().is_empty();
    let filtered_empty = !feed_empty && visible.is_empty();

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
                    } else if feed_empty {
                        div { class: "meetings-empty", "No meetings yet" }
                    } else {
                        // Filter + sort toolbar. Always shown when the feed has
                        // rows so the user can refine even a long list.
                        MeetingsToolbar {
                            filter: filter(),
                            sort: sort(),
                            active_filter_count,
                            filter_open: filter_open(),
                            sort_open: sort_open(),
                            on_filter_change: set_filter,
                            on_sort_change: set_sort,
                            on_toggle_filter: move |_| {
                                let open = !filter_open();
                                filter_open.set(open);
                                if open { sort_open.set(false); }
                            },
                            on_toggle_sort: move |_| {
                                let open = !sort_open();
                                sort_open.set(open);
                                if open { filter_open.set(false); }
                            },
                            on_close_filter: move |_| filter_open.set(false),
                            on_close_sort: move |_| sort_open.set(false),
                        }

                        if filtered_empty {
                            div { class: "meetings-empty meetings-empty-filtered",
                                span { "No meetings match your filters" }
                                button {
                                    class: "meetings-clear-filters-btn",
                                    r#type: "button",
                                    onclick: clear_filters,
                                    "Clear filters"
                                }
                            }
                        } else {
                        ul { class: "meetings-list",
                            for meeting in visible.iter() {
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

/// Filter + sort toolbar shown above the meetings list whenever the feed has
/// rows. Owns no business state — it renders the current `filter`/`sort` props
/// and bubbles every change up to `MeetingsList`, which persists it. Splitting
/// it out keeps `MeetingsList`'s render tree small and lets the popover /
/// dropdown manage their own open state via props.
///
/// Accessibility: both floating panels are dismissible by Escape (keydown on
/// the panel) and by clicking the transparent full-viewport backdrop
/// (outside-click). The trigger buttons carry `aria-haspopup`/`aria-expanded`;
/// checkboxes and radios are wrapped in `<label>`s so the text is clickable and
/// announced. The direction toggle exposes an `aria-label` describing the
/// current direction.
#[component]
#[allow(clippy::too_many_arguments)]
fn MeetingsToolbar(
    filter: FilterState,
    sort: SortState,
    active_filter_count: usize,
    filter_open: bool,
    sort_open: bool,
    on_filter_change: EventHandler<FilterState>,
    on_sort_change: EventHandler<SortState>,
    on_toggle_filter: EventHandler<()>,
    on_toggle_sort: EventHandler<()>,
    on_close_filter: EventHandler<()>,
    on_close_sort: EventHandler<()>,
) -> Element {
    let dir_is_desc = sort.dir == SortDir::Desc;
    let dir_label = if dir_is_desc {
        "Sort direction: descending. Activate to sort ascending."
    } else {
        "Sort direction: ascending. Activate to sort descending."
    };
    let filters_active = active_filter_count > 0;

    // Stable ids on the trigger buttons so the fixed-position popovers can
    // anchor to their bounding rects and focus can return to them on close.
    const FILTER_BTN_ID: &str = "meetings-filter-trigger";
    const SORT_BTN_ID: &str = "meetings-sort-trigger";
    // Keep these in sync with `.meetings-filter-popover` / `.meetings-sort-popover`
    // `min-width` in global.css — they drive the right-alignment math.
    const FILTER_POPOVER_W: f64 = 220.0;
    const SORT_POPOVER_W: f64 = 180.0;

    // Anchor coordinates, recomputed whenever an open flag flips. The trigger
    // buttons always render, so their rects are available before the popover
    // mounts.
    let filter_anchor = if filter_open {
        anchor_below_trigger(FILTER_BTN_ID, FILTER_POPOVER_W)
    } else {
        AnchorPos::default()
    };
    let sort_anchor = if sort_open {
        anchor_below_trigger(SORT_BTN_ID, SORT_POPOVER_W)
    } else {
        AnchorPos::default()
    };

    rsx! {
        div { class: "meetings-toolbar",
            // ---- Filter popover ------------------------------------------
            div { class: "meetings-toolbar-group",
                button {
                    id: FILTER_BTN_ID,
                    class: if filters_active { "meetings-icon-btn is-active" } else { "meetings-icon-btn" },
                    r#type: "button",
                    aria_haspopup: "true",
                    aria_expanded: filter_open,
                    title: "Filter meetings",
                    aria_label: if filters_active {
                        format!("Filter meetings ({active_filter_count} active)")
                    } else {
                        "Filter meetings".to_string()
                    },
                    onclick: move |_| on_toggle_filter.call(()),
                    svg {
                        xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                        view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                        stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                        "aria-hidden": "true",
                        polygon { points: "22 3 2 3 10 12.46 10 19 14 21 14 12.46 22 3" }
                    }
                    if filters_active {
                        span { class: "meetings-filter-badge", "aria-hidden": "true", "{active_filter_count}" }
                    }
                }

                if filter_open {
                    // Transparent backdrop: clicking anywhere outside the panel
                    // closes the popover (outside-click dismissal). Returns
                    // focus to the trigger so keyboard focus isn't stranded.
                    div {
                        class: "meetings-popover-backdrop",
                        onclick: move |_| {
                            on_close_filter.call(());
                            focus_element_by_id(FILTER_BTN_ID);
                        },
                    }
                    div {
                        class: "meetings-popover meetings-filter-popover",
                        role: "dialog",
                        aria_label: "Filter meetings",
                        // `position: fixed` (set in CSS) + JS-anchored coords so
                        // the panel escapes the card's clipped scroll region.
                        style: "left: {filter_anchor.left}px; top: {filter_anchor.top}px;",
                        // Escape closes; stop propagation so a click inside the
                        // panel never reaches the backdrop.
                        onkeydown: move |e: KeyboardEvent| {
                            if e.key() == Key::Escape {
                                e.stop_propagation();
                                on_close_filter.call(());
                                focus_element_by_id(FILTER_BTN_ID);
                            }
                        },
                        onclick: move |e: MouseEvent| e.stop_propagation(),

                        fieldset { class: "meetings-filter-group",
                            legend { class: "meetings-filter-legend", "Ownership" }
                            label { class: "meetings-filter-option",
                                input {
                                    r#type: "checkbox",
                                    checked: filter.own_owned,
                                    onchange: move |e| {
                                        on_filter_change.call(FilterState { own_owned: e.checked(), ..filter });
                                    },
                                }
                                span { "I own" }
                            }
                            label { class: "meetings-filter-option",
                                input {
                                    r#type: "checkbox",
                                    checked: filter.own_not_owned,
                                    onchange: move |e| {
                                        on_filter_change.call(FilterState { own_not_owned: e.checked(), ..filter });
                                    },
                                }
                                span { "I don't own" }
                            }
                        }

                        fieldset { class: "meetings-filter-group",
                            legend { class: "meetings-filter-legend", "Status" }
                            label { class: "meetings-filter-option",
                                input {
                                    r#type: "checkbox",
                                    checked: filter.status_active,
                                    onchange: move |e| {
                                        on_filter_change.call(FilterState { status_active: e.checked(), ..filter });
                                    },
                                }
                                span { "Active" }
                            }
                            label { class: "meetings-filter-option",
                                input {
                                    r#type: "checkbox",
                                    checked: filter.status_ended,
                                    onchange: move |e| {
                                        on_filter_change.call(FilterState { status_ended: e.checked(), ..filter });
                                    },
                                }
                                span { "Ended" }
                            }
                        }

                        fieldset { class: "meetings-filter-group",
                            legend { class: "meetings-filter-legend", "Attended within" }
                            for window in [
                                AttendedWithin::Last7Days,
                                AttendedWithin::Last30Days,
                                AttendedWithin::Last6Months,
                                AttendedWithin::Any,
                            ] {
                                label { class: "meetings-filter-option", key: "{window.as_id()}",
                                    input {
                                        r#type: "radio",
                                        name: "meetings-attended-within",
                                        value: window.as_id(),
                                        checked: filter.attended_within == window,
                                        onchange: move |_| {
                                            on_filter_change.call(FilterState { attended_within: window, ..filter });
                                        },
                                    }
                                    span { "{window.label()}" }
                                }
                            }
                        }
                    }
                }
            }

            // ---- Sort dropdown -------------------------------------------
            div { class: "meetings-toolbar-group",
                button {
                    id: SORT_BTN_ID,
                    class: "meetings-sort-btn",
                    r#type: "button",
                    aria_haspopup: "true",
                    aria_expanded: sort_open,
                    title: "Sort meetings",
                    onclick: move |_| on_toggle_sort.call(()),
                    svg {
                        xmlns: "http://www.w3.org/2000/svg", width: "14", height: "14",
                        view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                        stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                        "aria-hidden": "true",
                        line { x1: "3", y1: "6", x2: "13", y2: "6" }
                        line { x1: "3", y1: "12", x2: "10", y2: "12" }
                        line { x1: "3", y1: "18", x2: "7", y2: "18" }
                    }
                    span { class: "meetings-sort-label", "{sort.key.label()}" }
                }

                // Direction toggle, always visible next to the sort button.
                button {
                    class: "meetings-icon-btn meetings-sort-dir-btn",
                    r#type: "button",
                    aria_label: dir_label,
                    title: dir_label,
                    onclick: move |_| {
                        on_sort_change.call(SortState { dir: sort.dir.flipped(), ..sort });
                    },
                    svg {
                        class: if dir_is_desc { "meetings-sort-dir-icon is-desc" } else { "meetings-sort-dir-icon" },
                        xmlns: "http://www.w3.org/2000/svg", width: "14", height: "14",
                        view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                        stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                        "aria-hidden": "true",
                        line { x1: "12", y1: "5", x2: "12", y2: "19" }
                        polyline { points: "19 12 12 19 5 12" }
                    }
                }

                if sort_open {
                    div {
                        class: "meetings-popover-backdrop",
                        onclick: move |_| {
                            on_close_sort.call(());
                            focus_element_by_id(SORT_BTN_ID);
                        },
                    }
                    // Plain menu of buttons (no role="listbox"/"option"): there
                    // is no arrow-key roving to back full listbox semantics, so
                    // the markup deliberately under-promises. Tab + Escape is
                    // the supported keyboard model; a roving-tabindex listbox is
                    // a deferred follow-up. `aria_current` marks the active sort
                    // without implying single-select listbox behaviour.
                    div {
                        class: "meetings-popover meetings-sort-popover",
                        aria_label: "Sort by",
                        style: "left: {sort_anchor.left}px; top: {sort_anchor.top}px;",
                        onkeydown: move |e: KeyboardEvent| {
                            if e.key() == Key::Escape {
                                e.stop_propagation();
                                on_close_sort.call(());
                                focus_element_by_id(SORT_BTN_ID);
                            }
                        },
                        onclick: move |e: MouseEvent| e.stop_propagation(),
                        for key in SortKey::all() {
                            button {
                                class: if sort.key == key { "meetings-sort-option is-selected" } else { "meetings-sort-option" },
                                r#type: "button",
                                aria_current: if sort.key == key { "true" } else { "false" },
                                key: "{key.as_id()}",
                                onclick: move |_| {
                                    on_sort_change.call(SortState { key, ..sort });
                                    on_close_sort.call(());
                                    focus_element_by_id(SORT_BTN_ID);
                                },
                                "{key.label()}"
                            }
                        }
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
/// Non-owner rows skip that line entirely and instead render two Host rows
/// (display name + full user_id) at the top so the user knows who
/// created the meeting and how to contact them. See HCL issue 579. The
/// Host ID value is rendered in full — long IDs wrap inside the tooltip
/// (see `.meeting-info-tooltip-row__value--breakable` in `global.css`)
/// rather than being truncated with an ellipsis.
///
/// SECURITY INVARIANT: The HTML body produced here MUST NOT contain any
/// caller-controlled or server-supplied string content unless it is passed
/// through `escape_html_text` first. The host identity strings
/// (`host_display_name`, `host_user_id`) come from server-supplied fields
/// populated from the OAuth provider's id_token, so they are escaped
/// before being interpolated into the tooltip's `set_inner_html` payload.
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
    } else {
        rows.push(host_tooltip_row(meeting.host_display_name.as_deref()));
        rows.push(host_id_tooltip_row(meeting.host_user_id.as_deref()));
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

/// "Host: <display name>" row for non-owner meetings. Falls back to
/// "(unknown)" when the backend has not yet cached a display name (no
/// participant has joined the meeting yet).
fn host_tooltip_row(host_display_name: Option<&str>) -> String {
    let value_escaped = match host_display_name {
        Some(name) if !name.is_empty() => escape_html_text(name),
        _ => "(unknown)".to_string(),
    };
    format!(
        "<div class=\"meeting-info-tooltip-row\">\
         <span class=\"meeting-info-tooltip-label\">Host</span>\
         <span class=\"meeting-info-tooltip-value\">{value_escaped}</span>\
         </div>"
    )
}

/// "Host ID: <user_id>" row for non-owner meetings. The full user_id is
/// rendered inline (no truncation) so the value is copyable from the
/// tooltip's text content. CSS in `global.css` allows long IDs to wrap
/// inside a bounded tooltip width — see
/// `.meeting-info-tooltip-row__value--breakable`.
fn host_id_tooltip_row(host_user_id: Option<&str>) -> String {
    let raw = host_user_id.unwrap_or("");
    if raw.is_empty() {
        return "<div class=\"meeting-info-tooltip-row\">\
                <span class=\"meeting-info-tooltip-label\">Host ID</span>\
                <span class=\"meeting-info-tooltip-value\">(unknown)</span>\
                </div>"
            .to_string();
    }
    let value_escaped = escape_html_text(raw);
    format!(
        "<div class=\"meeting-info-tooltip-row\">\
         <span class=\"meeting-info-tooltip-label\">Host ID</span>\
         <span class=\"meeting-info-tooltip-value meeting-info-tooltip-value--breakable\">{value_escaped}</span>\
         </div>"
    )
}

/// Escape a string for safe interpolation into HTML text content. Covers
/// the five characters that change parser state inside element bodies and
/// double-quoted attribute values.
fn escape_html_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
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

/// Viewport-anchored coordinates for a `position: fixed` popover.
///
/// `position: fixed` is used (rather than a body portal or `position:
/// absolute`) so the popover escapes the meetings card's clipped scroll region
/// — `.meetings-list-container { overflow: hidden }` and
/// `.meetings-list-content { overflow-y: auto; max-height: 260px }` would
/// otherwise crop an absolutely-positioned dropdown. Fixed elements are laid
/// out relative to the viewport and are not clipped by ancestor `overflow`
/// (the meetings card's ancestry establishes no transform/filter containing
/// block), so the dropdown floats above the page.
#[derive(Clone, Copy, PartialEq, Default)]
pub(crate) struct AnchorPos {
    pub left: f64,
    pub top: f64,
}

/// Read a trigger button's bounding rect (by element id) and compute the
/// top-left for a popover that drops below it and is right-aligned to its right
/// edge. Falls back to `(0,0)` if the element isn't in the DOM yet. The final
/// viewport clamp (so the panel never overflows narrow screens) is applied in
/// CSS via `max-width`/`max-height` plus a JS left-edge guard here.
fn anchor_below_trigger(trigger_id: &str, popover_min_width: f64) -> AnchorPos {
    let doc = gloo_utils::document();
    let Some(el) = doc.get_element_by_id(trigger_id) else {
        return AnchorPos::default();
    };
    let rect = el.get_bounding_client_rect();
    let win = gloo_utils::window();
    let vw = win
        .inner_width()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let gap = 6.0;
    let edge_margin = 8.0;
    let top = rect.bottom() + gap;
    // Right-align the popover to the trigger's right edge.
    let mut left = rect.right() - popover_min_width;
    // Clamp to the viewport on narrow screens so it never spills off-screen.
    if left < edge_margin {
        left = edge_margin;
    }
    if left + popover_min_width + edge_margin > vw {
        left = (vw - popover_min_width - edge_margin).max(edge_margin);
    }
    AnchorPos { left, top }
}

/// Return keyboard focus to a trigger button by id. Called when a popover
/// closes (Escape, outside-click, or a selection that dismisses it) so focus
/// doesn't get stranded on the now-removed panel.
fn focus_element_by_id(id: &str) {
    if let Some(el) = gloo_utils::document().get_element_by_id(id) {
        let html_el: web_sys::HtmlElement = el.unchecked_into();
        let _ = html_el.focus();
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
            host_display_name: Some("Alice Anderson".to_string()),
            host_user_id: Some("alice@example.com".to_string()),
            is_owner,
            participant_count: 2,
            waiting_count: 0,
            has_password: false,
            allow_guests: false,
            waiting_room_enabled: true,
            admitted_can_admit: false,
            end_on_host_leave: true,
            user_last_attended_at: Some(1_714_323_600_000),
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

    #[wasm_bindgen_test]
    fn tooltip_includes_host_rows_for_non_owner() {
        let meeting = sample_meeting(false);
        let html = build_meeting_tooltip_html(&meeting, true, false, 60_000);
        assert!(
            html.contains(">Host<"),
            "non-owner tooltip must include a 'Host' label; html: {html}"
        );
        assert!(
            html.contains(">Host ID<"),
            "non-owner tooltip must include a 'Host ID' label; html: {html}"
        );
        assert!(
            html.contains("Alice Anderson"),
            "non-owner tooltip must show the host display name; html: {html}"
        );
        let host_idx = html.find(">Host<").expect("Host row must be present");
        let created_idx = html
            .find("Created on")
            .expect("Created on row must be present");
        assert!(
            host_idx < created_idx,
            "Host rows must precede the created-on row"
        );
    }

    #[wasm_bindgen_test]
    fn tooltip_omits_host_rows_when_owner() {
        let meeting = sample_meeting(true);
        let html = build_meeting_tooltip_html(&meeting, true, false, 60_000);
        assert!(
            !html.contains(">Host<"),
            "owner tooltip must not include a 'Host' label (Owner line conveys it); html: {html}"
        );
        assert!(
            !html.contains(">Host ID<"),
            "owner tooltip must not include a 'Host ID' label; html: {html}"
        );
    }

    #[wasm_bindgen_test]
    fn tooltip_renders_full_host_user_id_without_truncation() {
        // Regression guard for the UX tweak that dropped the 12-char +
        // ellipsis truncation from the Host ID row. The full value must
        // appear inline (no `\u{2026}` ellipsis, no `title="…"` fallback)
        // so it is selectable / copyable from the tooltip text.
        let mut meeting = sample_meeting(false);
        let full_id = "verylonghostuserid@example.com";
        meeting.host_user_id = Some(full_id.to_string());
        let html = build_meeting_tooltip_html(&meeting, true, false, 60_000);
        assert!(
            html.contains(full_id),
            "full host_user_id must render inline in the tooltip; html: {html}"
        );
        assert!(
            !html.contains('\u{2026}'),
            "tooltip must not contain a horizontal-ellipsis truncation marker; html: {html}"
        );
        // The breakable-value class lets CSS wrap long IDs inside the
        // bounded tooltip width — guard it so we don't lose layout if the
        // CSS rule is renamed in the future.
        assert!(
            html.contains("meeting-info-tooltip-value--breakable"),
            "Host ID row must carry the breakable-value class so long IDs wrap; html: {html}"
        );
    }

    #[wasm_bindgen_test]
    fn tooltip_falls_back_to_unknown_for_missing_host_identity() {
        let mut meeting = sample_meeting(false);
        meeting.host_display_name = None;
        meeting.host_user_id = None;
        let html = build_meeting_tooltip_html(&meeting, true, false, 60_000);
        assert!(
            html.contains("(unknown)"),
            "missing host identity must render '(unknown)'; html: {html}"
        );
    }

    #[wasm_bindgen_test]
    fn tooltip_escapes_html_in_host_fields() {
        let mut meeting = sample_meeting(false);
        meeting.host_display_name = Some("<script>alert(1)</script>".to_string());
        meeting.host_user_id = Some("a&b\"c".to_string());
        let html = build_meeting_tooltip_html(&meeting, true, false, 60_000);
        assert!(
            !html.contains("<script>alert(1)</script>"),
            "raw <script> from host_display_name must not appear in tooltip html; html: {html}"
        );
        assert!(
            html.contains("&lt;script&gt;"),
            "host_display_name must be HTML-escaped; html: {html}"
        );
        assert!(
            html.contains("a&amp;b&quot;c"),
            "host_user_id must be HTML-escaped in the tooltip's text content; html: {html}"
        );
        assert!(
            !html.contains("a&b\"c"),
            "raw host_user_id characters must not appear unescaped in tooltip html; html: {html}"
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

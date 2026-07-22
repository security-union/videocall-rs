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
use crate::constants::{meeting_api_base_url, meeting_api_client};
use crate::local_storage::{load_bool, load_json, save_bool, save_json};
use crate::routing::Route;
use dioxus::prelude::*;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use std::cell::RefCell;
use std::rc::Rc;
use videocall_meeting_types::responses::{ListFeedResponse, MeetingFeedSummary};
use wasm_bindgen::closure::Closure;
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

/// Named SSE `event:` the meeting-feed change stream uses (issue #1081). The
/// server emits `event: feed-changed`, NOT the default `message` event, so the
/// client must register a listener for THIS name — a `message` handler alone
/// would never fire. Keep in lockstep with the backend constant of the same
/// value in `meeting-api/src/routes/feed_stream.rs`.
const FEED_CHANGED_EVENT: &str = "feed-changed";

/// Debounce window (ms) applied to SSE `feed-changed` nudges before re-fetching
/// the feed. The nudge payload is advisory-only; on ANY nudge we re-fetch
/// `GET /api/v1/meetings/feed`. Bursts of nudges (admit-all storms, reconnection
/// waves, many joins/leaves at once) are coalesced into a single re-fetch: the
/// first nudge opens a fixed 400ms window, and any further nudges that arrive
/// during it are drained before one re-fetch fires. 400ms sits inside the
/// server-documented 300–500ms guidance: long enough to absorb a burst, short
/// enough to feel live.
const FEED_SSE_DEBOUNCE_MS: u32 = 400;

/// Build the meeting-feed SSE stream URL from the meeting-api base URL.
///
/// Pure (host-testable) so the path contract is pinned without a browser. The
/// base comes from the SAME source the existing feed fetch uses
/// ([`meeting_api_base_url`] → `MeetingApiClient`), and the existing feed fetch
/// hits `{base}/api/v1/meetings/feed`, so the live stream is that path plus
/// `/stream` — matching the server route registered in
/// `meeting-api/src/routes/mod.rs`. The base is trimmed of a trailing slash so
/// a configured `https://api.example.com/` and `https://api.example.com` both
/// yield the same URL (mirrors `MeetingApiClient::new`, which `trim_end_matches`es).
fn feed_stream_url(base_url: &str) -> String {
    format!(
        "{}/api/v1/meetings/feed/stream",
        base_url.trim_end_matches('/')
    )
}

enum FetchMeetingsError {
    Unauthenticated,
    Other(String),
}

/// Decide whether the low-frequency fallback poll should issue a feed
/// re-fetch on this tick. We skip while the initial mount fetch is still in
/// flight (`loading` — the spinner is showing; a silent poll on top would be
/// wasteful and could double-fetch), while `unauthenticated` (don't hammer
/// a 401 the user must resolve by signing in; the SSE error-budget already
/// closed the stream in that case, and the mount/refresh paths re-arm auth),
/// and while the browser tab is `hidden` (issue #1628 follow-up — a
/// backgrounded tab is wasting mobile data + radio wakeups on a list the user
/// isn't looking at; the next foreground tick / SSE nudge re-fetches anyway).
/// Pure + host-testable so the guard truth table is pinned without a browser;
/// the live `document.hidden` read happens at the single call site.
fn should_refetch_on_tick(loading: bool, unauthenticated: bool, hidden: bool) -> bool {
    !loading && !unauthenticated && !hidden
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
    //
    // issue 498 (memo-level Rc form): wrap each row in `Rc` here so passing a
    // row into `MeetingItem` is a cheap pointer clone instead of a deep clone
    // of the whole `MeetingFeedSummary` on every parent re-render.
    let visible_meetings = use_memo(move || {
        let now_ms = js_sys::Date::now() as i64;
        filter_and_sort_meetings(&meetings(), &filter(), &sort(), now_ms)
            .into_iter()
            .map(Rc::new)
            .collect::<Vec<_>>()
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

    // SSE → coroutine bridge (issue #1671). The raw browser `EventSource`
    // callbacks fire on the bare event-loop stack with NO Dioxus runtime / scope
    // present, so calling `spawn`/`Signal::set` directly from them panics inside
    // dioxus-core (Option::unwrap on the runtime, or a RefCell-already-borrowed
    // re-entry mid-diff). We therefore bridge the nudge across a stable
    // `futures` channel to a `use_coroutine` task: the coroutine's async body
    // runs INSIDE the component scope (it is built on `use_future`, which Dioxus
    // polls under the runtime), so `meetings.set(...)` etc. there are safe.
    //
    // The channel pair is created with `use_hook` so it is STABLE across renders
    // (one channel for the lifetime of the mount). The sender is cloned into the
    // SSE `on_message` closure; the receiver lives in an `Rc<RefCell<Option<…>>>`
    // holder and is taken ONCE into the coroutine body below — so the closure and
    // the coroutine share the SAME channel pair. We default to our OWN channel
    // rather than the coroutine's internal `tx()` because reading that handle is
    // not guaranteed runtime-free. (`use_hook` requires its stored value to be
    // `Clone`; an `UnboundedReceiver` is not, so we hide it behind an `Rc` cell —
    // the sender already is `Clone`, hence the tuple is.)
    let (feed_nudge_tx, feed_nudge_rx_holder) = use_hook(|| {
        let (tx, rx) = futures::channel::mpsc::unbounded::<()>();
        (tx, Rc::new(RefCell::new(Some(rx))))
    });

    // The coroutine OWNS the silent background re-fetch that used to live in the
    // `background_refetch` closure: it reuses the EXACT same feed fetch path
    // (`do_fetch_feed`) but does NOT flip `loading` — a live nudge updates the
    // list in place rather than blanking it to the spinner, so the homepage
    // doesn't blink on every remote join/leave. Auth/error transitions are still
    // honored so the list self-heals (an expired session surfaces the
    // unauthenticated prompt; a later valid re-fetch clears it; a transient
    // error is shown without wiping the currently-displayed rows), and the
    // `meetings.set` is equality-skipped so an unchanged feed costs no recompute.
    //
    // Debounce now lives HERE (not in a raw `gloo_timers::callback::Timeout`):
    // we await one nudge, sleep `FEED_SSE_DEBOUNCE_MS`, drain any nudges that
    // queued during the sleep, then do ONE refetch — so a burst of nudges
    // (admit-all storms, reconnection waves) coalesces into a single re-fetch.
    // Note this is a LEADING fixed window (the first nudge opens a 400ms window
    // that absorbs the burst), not the old trailing reset-on-each-nudge timer;
    // it still coalesces a burst to one refetch and is marginally more prompt.
    // Behavioral note (issue #1671 review): under a SUSTAINED stream of nudges
    // spaced <400ms apart indefinitely (a long reconnection-wave tail), the
    // leading window refetches at a steady-state ceiling of ~1 fetch / 400ms
    // (≈2.5/sec) so the list converges. The old trailing timer reset on every
    // nudge and so STARVED under that same stream (≈0 fetches until it subsided,
    // leaving the list stale). We deliberately prefer convergence over freezing;
    // the ceiling is bounded and far below a tight poll, and each fetch is the
    // same server-capped (≤200-row) feed the render already reads.
    // On unmount Dioxus drops the coroutine future, which cancels the in-flight
    // `TimeoutFuture` — that is the new debounce-cancellation path (it replaces
    // dropping the old `Timeout`). The `use_coroutine` closure receives its own
    // internal receiver, which we ignore (`_`) in favor of our stable `rx`.
    use_coroutine(move |_: UnboundedReceiver<()>| {
        // Move our OWN receiver into the task on first build. `use_coroutine`'s
        // `init` runs once, so the `.take()` yields the real receiver here.
        let rx = feed_nudge_rx_holder.borrow_mut().take();
        async move {
            let Some(mut rx) = rx else {
                // No receiver (would only happen on an impossible double-build).
                // Nothing to drive — exit cleanly rather than busy-spin.
                return;
            };
            loop {
                // Block until a nudge arrives. `None` => all senders dropped
                // (component torn down): exit the loop, do not busy-spin.
                if rx.next().await.is_none() {
                    break;
                }
                // Debounce window: absorb a burst into one refetch.
                gloo_timers::future::TimeoutFuture::new(FEED_SSE_DEBOUNCE_MS).await;
                // Non-blockingly drain any nudges that queued during the sleep so
                // the whole burst collapses to a single refetch. `try_next` is
                // `Ok(Some(()))` while items remain, then `Err` (empty-but-open)
                // or `Ok(None)` (closed) — either non-`Ok(Some)` ends the drain.
                while let Ok(Some(())) = rx.try_next() {}
                // ── single silent refetch (subsumes `background_refetch`) ──
                match do_fetch_feed().await {
                    Ok(response) => {
                        // Equality-skip: `Signal::set` does NOT short-circuit on
                        // equality (dioxus-signals 0.7.3) — it unconditionally
                        // dirties subscribers. When an SSE nudge re-fetch returns
                        // an unchanged feed, an unguarded `set` would still force
                        // the `visible_meetings` memo to recompute
                        // `filter_and_sort_meetings` over up to 200 rows + 200
                        // `Rc::new` allocations for nothing. `.peek()` so the
                        // compare creates no reactive subscription.
                        if *meetings.peek() != response.meetings {
                            meetings.set(response.meetings);
                        }
                        error.set(None);
                        unauthenticated.set(false);
                    }
                    Err(FetchMeetingsError::Unauthenticated) => {
                        unauthenticated.set(true);
                    }
                    Err(FetchMeetingsError::Other(e)) => {
                        error.set(Some(e));
                    }
                }
            }
        }
    });

    // Fetch on mount
    use_effect({
        let mut fetch_meetings = fetch_meetings;
        move || {
            fetch_meetings();
        }
    });

    // Subscribe the list to the server's live feed-change stream (issue #1081):
    // an `EventSource` over `…/api/v1/meetings/feed/stream`. On each named
    // `feed-changed` nudge the raw callback does a runtime-free
    // `notify_feed_changed(&tx)` channel send (issue #1671); the
    // `use_coroutine` task above then debounces (`FEED_SSE_DEBOUNCE_MS`) and runs
    // ONE silent refetch. This is a SUBSCRIPTION, not a poll — we add no
    // interval; updates are pushed by the server. The mount fetch, this
    // SSE-driven refresh, and the manual refresh button are the only update
    // paths, and the list degrades gracefully to the first and last of those if
    // SSE never connects (see below). We pass a clone of the nudge sender so the
    // closure can hand off across the channel without touching the Dioxus runtime.
    install_feed_sse(feed_nudge_tx.clone());

    // Low-frequency fallback poll (issue #1628). SSE (above) is the PRIMARY
    // live-update path; this poll exists ONLY for when SSE never connects or
    // its consecutive-error budget closes the stream (e.g. a cross-origin
    // cookie handshake that drops the stream, or a persistent stream 401).
    // Without it, the list silently stops reflecting idle/active/ended +
    // participant_count transitions until a manual refresh — the exact parity
    // gap vs. the edit screen, which already polls (see meeting_settings.rs).
    //
    // Lifecycle / cleanup: `use_future` ties this loop to the component scope.
    // On navigate-away the scope unmounts and Dioxus drops the future, which
    // stops the loop and clears the pending `TimeoutFuture` timer — so there is
    // no timer handle to leak (mirrors meeting_settings.rs). Note: a fetch
    // already in flight at the `do_fetch_feed().await` point is NOT actively
    // aborted (no `AbortController` is wired to the dropped future); it simply
    // completes and its result is discarded, since the future is gone.
    //
    // Silent + non-stomping: reuses `do_fetch_feed()` (the EXACT endpoint the
    // render reads) and the SSE nudge coroutine's update shape (equality-skipped
    // meetings.set + clear error/unauthenticated) WITHOUT flipping `loading` — so
    // a fallback refresh updates rows in place and never blanks to the spinner.
    // It writes ONLY `meetings`/`error`/`unauthenticated`; it never touches the
    // filter/sort/expanded/popover signals, so a fallback tick cannot stomp an
    // open filter popover or the user's sort selection.
    //
    // Interval: 25s. Rationale: SSE is primary and handles the live case; this
    // is a backstop, NOT the live path. The edit screen polls one meeting on a
    // low-traffic settings page every 12s; the homepage list is the high-traffic
    // landing page and re-fetches the user's ENTIRE feed (server-capped at 200
    // rows) each tick, so a faster cadence multiplies meeting-api load across
    // every landing. 25s keeps a degraded (SSE-down) list converging within
    // half a minute — fast enough that a user rarely sees stale state — while
    // costing at most ~2.4 feed fetches/minute per open homepage. Guard reads
    // use `.peek()` so this long-lived poll creates NO reactive subscriptions.
    use_future(move || async move {
        const FALLBACK_POLL_INTERVAL_MS: u32 = 25_000;
        loop {
            gloo_timers::future::TimeoutFuture::new(FALLBACK_POLL_INTERVAL_MS).await;

            // Skip while the mount fetch is in flight (spinner showing), while
            // unauthenticated (don't hammer a 401), or while the tab is hidden
            // (issue #1628: don't burn mobile data/radio on a backgrounded list
            // — the next foreground tick / SSE nudge re-fetches anyway).
            // `document.hidden` is read live here (the pure guard stays
            // host-testable); if the document is unavailable we default to NOT
            // hidden so a missing API can never permanently wedge updates.
            // `.peek()` → no reactive subscriptions on this long-lived poll.
            let hidden = web_sys::window()
                .and_then(|w| w.document())
                .map(|d| d.hidden())
                .unwrap_or(false);
            if !should_refetch_on_tick(*loading.peek(), *unauthenticated.peek(), hidden) {
                continue;
            }

            match do_fetch_feed().await {
                Ok(response) => {
                    // Re-check guards AFTER the await: the user may have hit
                    // refresh (loading flipped) or the session may have expired
                    // while the request was in flight. Mirror the SSE nudge
                    // coroutine: silent in-place update, clear transient error/auth.
                    if *loading.peek() {
                        continue;
                    }
                    // Equality-skip: `Signal::set` does NOT short-circuit on
                    // equality (dioxus-signals 0.7.3) — it unconditionally
                    // dirties subscribers. On the no-change tick (the dominant
                    // case for a backstop poll) an unguarded `set` would force
                    // the `visible_meetings` memo to recompute
                    // `filter_and_sort_meetings` over up to 200 rows + 200
                    // `Rc::new` allocations for nothing. `.peek()` so the
                    // compare creates no reactive subscription.
                    if *meetings.peek() != response.meetings {
                        meetings.set(response.meetings);
                    }
                    error.set(None);
                    unauthenticated.set(false);
                }
                Err(FetchMeetingsError::Unauthenticated) => {
                    unauthenticated.set(true);
                }
                Err(FetchMeetingsError::Other(e)) => {
                    error.set(Some(e));
                }
            }
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
    // The filter/sort controls live on the header row (issue #1), so they must
    // only render once the feed is expanded, loaded, authenticated, error-free,
    // and actually has rows to refine — matching when the list body shows rows.
    let show_controls =
        expanded() && !loading() && !unauthenticated() && error().is_none() && !feed_empty;

    rsx! {
        div { class: "meetings-list-container",
            // Header row: collapsible "Meetings" title on the left, the
            // filter/sort controls on the right (space-between).
            div { class: "meetings-list-header",
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

                if show_controls {
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
                }
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
    // issue 498: shared via `Rc` so the parent hands us a cheap pointer clone
    // instead of deep-cloning the whole `MeetingFeedSummary` per row on every
    // re-render. We only read fields here; we never mutate the row in place.
    meeting: Rc<MeetingFeedSummary>,
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

    // issue 497: defer the tooltip HTML build to hover. A dedicated `Rc` clone
    // (cheap pointer clone) is captured by the `FnMut` hover handler so it can
    // rebuild the HTML on each `mouseenter`; the scalars `is_active`/`is_ended`/
    // `duration_ms` are `Copy`. This avoids building HTML for every row on every
    // render — it now runs only when a row is actually hovered.
    let meeting_for_tooltip = meeting.clone();

    rsx! {
        li {
            class: if is_ended { "meeting-item meeting-ended" } else { "meeting-item" },
            // issue 497: tooltip HTML is built lazily here on hover (not eagerly
            // per render); content is byte-for-byte identical to the prior eager
            // build (same helper, same args).
            onmouseenter: move |e: MouseEvent| {
                let coords = e.client_coordinates();
                let html = build_meeting_tooltip_html(
                    &meeting_for_tooltip,
                    is_active,
                    is_ended,
                    duration_ms,
                );
                show_meeting_info_tooltip(coords.x, coords.y, &html);
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

/// Filter + sort toolbar rendered on the right of the "Meetings" header row
/// (same line as the collapsible title) whenever the feed has rows. Owns no
/// business state — it renders the current `filter`/`sort` props
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

    // Stable ids on the trigger buttons so focus can return to them on close.
    // Popover positioning is now pure CSS (absolute, anchored to the
    // `.meetings-toolbar-group` wrapper) — no JS rect math.
    const FILTER_BTN_ID: &str = "meetings-filter-trigger";
    const SORT_BTN_ID: &str = "meetings-sort-trigger";

    rsx! {
        div { class: "meetings-toolbar",
            // ---- Filter popover ------------------------------------------
            // Escape is handled at the GROUP level (not on the popover div) so a
            // keydown from the focused trigger button — the common case, and
            // what Playwright's `.press('Escape')` produces — bubbles up and
            // closes the popover regardless of where focus sits while open.
            div {
                class: "meetings-toolbar-group",
                onkeydown: move |e: KeyboardEvent| {
                    if filter_open && e.key() == Key::Escape {
                        e.stop_propagation();
                        on_close_filter.call(());
                        focus_element_by_id(FILTER_BTN_ID);
                    }
                },
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
                        // Position is pure CSS (absolute, anchored to the
                        // toolbar-group wrapper) — see `.meetings-popover`.
                        // Escape is handled on the enclosing group (above);
                        // stop click propagation so a click inside the panel
                        // never reaches the backdrop.
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
                                span { "Owned" }
                            }
                            label { class: "meetings-filter-option",
                                input {
                                    r#type: "checkbox",
                                    checked: filter.own_not_owned,
                                    onchange: move |e| {
                                        on_filter_change.call(FilterState { own_not_owned: e.checked(), ..filter });
                                    },
                                }
                                span { "Not Owned" }
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
            // Group-level Escape (see filter group comment) so a keydown from
            // the focused sort trigger closes the popover.
            div {
                class: "meetings-toolbar-group",
                onkeydown: move |e: KeyboardEvent| {
                    if sort_open && e.key() == Key::Escape {
                        e.stop_propagation();
                        on_close_sort.call(());
                        focus_element_by_id(SORT_BTN_ID);
                    }
                },
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
                        // Escape handled on the enclosing group (above); stop
                        // click propagation so an in-panel click never reaches
                        // the backdrop.
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

/// After this many CONSECUTIVE `EventSource` `error` events with no intervening
/// successful message, we give up on the live stream and close it. EventSource
/// auto-reconnects on transient drops (browser-native backoff), so a handful of
/// errors is normal and we must NOT close on the first one. But a persistent
/// failure — most importantly a `401` on the stream (expired/absent session),
/// which EventSource would otherwise retry forever — must not hot-loop: we close
/// and fall back to the still-working fetch-on-mount / manual-refresh paths.
/// Any successful nudge resets the counter, so only an UNBROKEN run of failures
/// trips it.
const FEED_SSE_MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// Mutable state backing the live feed `EventSource`, owned for the lifetime of
/// the mounted `MeetingsList` and torn down in `use_drop`. Held behind
/// `Rc<RefCell<Option<…>>>` so the message/error closures and the drop hook
/// share it. Every field is dropped explicitly on unmount so no listener,
/// socket, or timer leaks across a mount/unmount cycle (the component remounts
/// as the user navigates).
struct FeedSseState {
    /// The open stream. `close()` is idempotent; we call it on unmount and after
    /// the consecutive-error threshold trips.
    source: web_sys::EventSource,
    /// Listener for the named `feed-changed` event. Kept alive here (NOT
    /// `forget()`-ed) so we can `remove_event_listener_with_callback` it on
    /// unmount — a forgotten closure would leak across remounts.
    on_message: Option<Closure<dyn FnMut(web_sys::MessageEvent)>>,
    /// Listener for the `error` event (transient drops + terminal failures).
    on_error: Option<Closure<dyn FnMut(web_sys::Event)>>,
    /// Listener for the `open` event, fired on every successful (re)connection.
    /// Resets the error budget so a stream that flaps but keeps reconnecting is
    /// never closed — only an UNBROKEN run of failed connects trips the cap.
    on_open: Option<Closure<dyn FnMut(web_sys::Event)>>,
    /// Count of consecutive `error`s since the last successful (re)connection or
    /// nudge. Reset to 0 on every `open` and every `feed-changed`; closes the
    /// stream once it reaches [`FEED_SSE_MAX_CONSECUTIVE_ERRORS`]. Because a
    /// healthy reconnect resets it, only a persistent failure (e.g. a stream
    /// `401` that never opens) accumulates to the cap.
    consecutive_errors: u32,
}

/// Runtime-free handoff of an SSE `feed-changed` nudge from the raw browser
/// `EventSource` callback to the Dioxus-driven coroutine (issue #1671).
///
/// The `on_message` `Closure` fires on the bare event-loop stack with NO Dioxus
/// runtime/scope present, so it must NOT touch `spawn`/`Signal::set`/`peek`/the
/// `Coroutine` handle (any of those panics in dioxus-core). All it may do is push
/// a `()` across the stable `futures` channel; the coroutine — which IS polled
/// under the runtime — wakes, debounces, and performs the actual refetch.
///
/// `unbounded_send` is non-blocking and never touches the runtime, so it is safe
/// from a raw callback. We deliberately swallow the `Err` it returns when the
/// receiver has been dropped (the component unmounted): a late nudge arriving
/// after navigate-away is a no-op, NOT a panic. Kept as a separate fn so the
/// host regression test can exercise this exact handoff with no runtime present.
fn notify_feed_changed(tx: &futures::channel::mpsc::UnboundedSender<()>) {
    let _ = tx.unbounded_send(());
}

/// Subscribe the mounted `MeetingsList` to the server's live meeting-feed change
/// stream (issue #1081) via a browser `EventSource`.
///
/// `nudge_tx` is the sender half of the stable `futures` channel that feeds the
/// component's nudge coroutine (issue #1671). The raw `feed-changed` callback
/// hands off across this channel via [`notify_feed_changed`] instead of calling
/// into the Dioxus runtime directly (which would panic from a bare browser
/// callback); the coroutine owns the debounce + silent refetch.
///
/// # Connection & data flow
///
/// 1. Open `EventSource("{meeting_api_base}/api/v1/meetings/feed/stream")` with
///    `withCredentials = true`. The existing feed fetch authenticates in Cookie
///    mode (`credentials: 'include'`), and the meeting-api is commonly a
///    DIFFERENT origin than the page, so the session cookie only rides along on
///    the SSE handshake if `withCredentials` is set. It is a harmless no-op when
///    the stream is same-origin, so this matches the fetch's auth either way.
/// 2. On each named `feed-changed` event, send one `()` across `nudge_tx`
///    (`notify_feed_changed`). The coroutine debounces `FEED_SSE_DEBOUNCE_MS` and
///    coalesces a burst of nudges into a single feed re-fetch. The payload is
///    advisory-only and is intentionally ignored: the re-fetch is what enforces
///    per-user visibility.
/// 3. On `error`, increment a consecutive-error counter; EventSource reconnects
///    itself on transient drops, so we tolerate a few. A successful (re)connect
///    (`open`) or any `feed-changed` nudge resets the counter, so only an UNBROKEN
///    run of failed connects accumulates. After `FEED_SSE_MAX_CONSECUTIVE_ERRORS`
///    such errors (e.g. a stream `401` that never opens and would otherwise retry
///    forever) we `close()` and stop — the list still updates via the on-mount
///    fetch and the manual refresh button.
///
/// # Degrade-on-failure
///
/// If `EventSource` construction fails (feature disabled, bad URL) or the config
/// lookup errors, we simply don't subscribe; the component's existing
/// fetch-on-mount + manual-refresh behavior is untouched and fully functional.
fn install_feed_sse(nudge_tx: futures::channel::mpsc::UnboundedSender<()>) {
    let state: Rc<RefCell<Option<FeedSseState>>> = use_hook(|| Rc::new(RefCell::new(None)));

    {
        let state_for_init = state.clone();
        // One-shot: `use_hook` runs exactly once per mount, so we open at most one
        // EventSource per mounted component (no duplicate streams on re-render).
        use_hook(move || {
            // Resolve the stream URL from the SAME base the feed fetch uses. On a
            // config error we silently skip SSE — the fetch paths still work.
            let base = match meeting_api_base_url() {
                Ok(base) => base,
                Err(_) => return,
            };
            let url = feed_stream_url(&base);

            // `withCredentials = true` so the session cookie is sent on a
            // cross-origin stream (no-op when same-origin) — mirrors the feed
            // fetch's `credentials: 'include'`.
            let init = web_sys::EventSourceInit::new();
            init.set_with_credentials(true);
            let source = match web_sys::EventSource::new_with_event_source_init_dict(&url, &init) {
                Ok(src) => src,
                // Construction can only fail on a malformed URL or the feature
                // being unavailable; either way we degrade to the fetch paths.
                Err(_) => return,
            };

            // ── named `feed-changed` listener → coroutine handoff (issue #1671) ──
            // The server emits a NAMED event, so a `message` (default) handler
            // would never fire; we must listen for `feed-changed` specifically.
            // This fires on the bare browser event-loop stack with NO Dioxus
            // runtime present, so it does ONLY runtime-free work: bump the error
            // budget (touches the `Rc<RefCell<…>>` state, not the runtime) and
            // push a nudge across the channel. The Dioxus-driven coroutine, which
            // IS polled under the runtime, debounces and does the actual refetch.
            let on_message: Closure<dyn FnMut(web_sys::MessageEvent)> = Closure::new({
                let state = state_for_init.clone();
                let nudge_tx = nudge_tx.clone();
                move |_ev: web_sys::MessageEvent| {
                    if let Some(s) = state.borrow_mut().as_mut() {
                        // A successful nudge means the stream is healthy — reset
                        // the error budget.
                        s.consecutive_errors = 0;
                        // Runtime-free handoff: push one `()` to the nudge
                        // coroutine, which owns the debounce + silent refetch. A
                        // closed channel (post-unmount) makes this a no-op, never
                        // a panic. Inside the same `Some(s)` guard as the reset: a
                        // transiently-`None` state means the stream is tearing
                        // down anyway, so skipping the send is correct.
                        notify_feed_changed(&nudge_tx);
                    }
                }
            });
            let _ = source.add_event_listener_with_callback(
                FEED_CHANGED_EVENT,
                on_message.as_ref().unchecked_ref(),
            );

            // ── `open` listener → reset the error budget on a healthy connect ──
            // EventSource fires `open` on every successful (re)connection. A
            // stream can reconnect and sit idle (only keep-alive comments, which
            // fire no JS event), so we must reset here and not rely solely on a
            // `feed-changed` nudge — otherwise a flaky-but-recovering connection
            // with no feed activity could wrongly accumulate to the close cap.
            let on_open: Closure<dyn FnMut(web_sys::Event)> = Closure::new({
                let state = state_for_init.clone();
                move |_ev: web_sys::Event| {
                    if let Some(s) = state.borrow_mut().as_mut() {
                        s.consecutive_errors = 0;
                    }
                }
            });
            let _ =
                source.add_event_listener_with_callback("open", on_open.as_ref().unchecked_ref());

            // ── `error` listener → tolerate transient drops, bail on persistent ──
            let on_error: Closure<dyn FnMut(web_sys::Event)> = Closure::new({
                let state = state_for_init.clone();
                move |_ev: web_sys::Event| {
                    if let Some(s) = state.borrow_mut().as_mut() {
                        s.consecutive_errors += 1;
                        if s.consecutive_errors >= FEED_SSE_MAX_CONSECUTIVE_ERRORS {
                            // Persistent failure (e.g. a stream 401 that never
                            // opens). Stop the native retry loop; the fetch paths
                            // still work.
                            s.source.close();
                        }
                    }
                }
            });
            let _ =
                source.add_event_listener_with_callback("error", on_error.as_ref().unchecked_ref());

            *state_for_init.borrow_mut() = Some(FeedSseState {
                source,
                on_message: Some(on_message),
                on_error: Some(on_error),
                on_open: Some(on_open),
                consecutive_errors: 0,
            });
        });
    }

    // Clean teardown on unmount: detach all three listeners, drop the closures,
    // and close the socket. Removing the listeners (rather than `forget()`-ing
    // the closures) is what prevents a leak across the component's mount/unmount
    // cycles during navigation. The pending debounce no longer lives here — the
    // coroutine future is dropped by Dioxus on unmount, which cancels the
    // in-flight `TimeoutFuture` (issue #1671).
    use_drop({
        let state = state.clone();
        move || {
            if let Some(mut s) = state.borrow_mut().take() {
                if let Some(cb) = s.on_message.take() {
                    let _ = s.source.remove_event_listener_with_callback(
                        FEED_CHANGED_EVENT,
                        cb.as_ref().unchecked_ref(),
                    );
                }
                if let Some(cb) = s.on_error.take() {
                    let _ = s
                        .source
                        .remove_event_listener_with_callback("error", cb.as_ref().unchecked_ref());
                }
                if let Some(cb) = s.on_open.take() {
                    let _ = s
                        .source
                        .remove_event_listener_with_callback("open", cb.as_ref().unchecked_ref());
                }
                s.source.close();
            }
        }
    });
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
            recording_allowed_for_all: false,
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

    // ── Host-testable pure helpers (issue #1081) ─────────────────────────────
    //
    // `feed_stream_url` is pure Rust (no `js_sys`/`web_sys`), so unlike the
    // tooltip tests above these run for real on the HOST target under
    // `cargo test --lib`. The EventSource WIRING itself (listener registration,
    // debounce timer, withCredentials, teardown) is inherently `web_sys`-bound
    // and browser-only — there is no host harness for `EventSource`, so it is
    // not host-tested here. What we CAN pin without a browser is the stream URL
    // the wiring opens, which is the load-bearing contract against the server
    // route.

    /// The SSE stream URL must be exactly the feed-fetch path plus `/stream`,
    /// matching the server route `GET /api/v1/meetings/feed/stream`. If the
    /// path drifts (e.g. someone "fixes" it to `/meetings/stream`), the live
    /// updates silently stop — this guards the literal contract.
    #[test]
    fn feed_stream_url_appends_stream_to_the_feed_path() {
        assert_eq!(
            feed_stream_url("https://api.example.com"),
            "https://api.example.com/api/v1/meetings/feed/stream"
        );
    }

    /// A configured base with a trailing slash must not produce a doubled
    /// slash — mirrors `MeetingApiClient::new`, which trims the trailing slash
    /// so the feed fetch and the stream resolve to the same origin+path shape.
    #[test]
    fn feed_stream_url_trims_trailing_slash() {
        assert_eq!(
            feed_stream_url("https://api.example.com/"),
            "https://api.example.com/api/v1/meetings/feed/stream"
        );
        // Same-origin/relative-style base (the empty-config fallback resolves to
        // the page origin upstream, but the trim must hold for any base).
        assert_eq!(
            feed_stream_url("http://localhost:8081"),
            "http://localhost:8081/api/v1/meetings/feed/stream"
        );
    }

    /// Regression guard for issue #1671. The raw SSE `feed-changed` callback
    /// fires on the bare browser event-loop stack with NO Dioxus runtime/scope
    /// present, so it MUST NOT touch the runtime — it only hands the nudge across
    /// the `futures` channel via the production [`notify_feed_changed`]. This
    /// test exercises that exact production fn with no runtime installed:
    ///
    /// 1. a live nudge enqueues exactly one `()` (not zero, not two) and does not
    ///    panic — proving the runtime-free handoff works; and
    /// 2. after the receiver is dropped (the navigate-away/unmount path), a LATE
    ///    nudge is swallowed (`Err` ignored) and still does not panic.
    ///
    /// It calls the REAL `super::notify_feed_changed` — not an inline copy — so a
    /// regression that re-introduces a runtime touch on the send path (the
    /// original #1671 panic) breaks THIS test. (Verified fails-on-revert by
    /// temporarily inserting `dioxus::prelude::spawn(async {})` before the send,
    /// which panics on the host with no runtime; the test went red.)
    #[test]
    fn notify_feed_changed_enqueues_one_nudge_without_runtime() {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();

        // (1) One nudge → exactly one `()` queued, no runtime present, no panic.
        super::notify_feed_changed(&tx);
        assert!(
            matches!(rx.try_next(), Ok(Some(()))),
            "notify_feed_changed must enqueue exactly one () on the channel"
        );
        // A second drain must be `Err` (empty-but-OPEN: `tx` is still alive), NOT
        // `Ok(Some(_))`. `Ok(Some)` here would mean a double-send; `Ok(None)`
        // would mean the channel was closed (it is not). `Err` proves exactly one
        // item was sent. (futures-channel: `Ok(None)` is closed-and-empty,
        // `Err(_)` is empty-but-still-open — verified against the crate docs.)
        assert!(
            rx.try_next().is_err(),
            "channel must be empty-but-open after draining the single nudge \
             (a second item would mean a double-send)"
        );

        // (2) Navigate-away path: drop the receiver, then a late nudge must be a
        // no-op (its `Err` is swallowed) and must NOT panic.
        drop(rx);
        super::notify_feed_changed(&tx);
    }

    /// The fallback poll must re-fetch ONLY when the list is loaded, the user
    /// is authenticated, AND the tab is visible. It must skip while the mount
    /// fetch is in flight (spinner showing), while unauthenticated (don't
    /// hammer a 401), and while the tab is hidden (issue #1628: don't burn
    /// mobile data on a backgrounded list). This pins the full truth table so
    /// an inverted/broken guard — including dropping the `!hidden` term —
    /// fails loudly; the poll calls THIS production fn, so a regression here is
    /// a real bug.
    #[test]
    fn should_refetch_on_tick_only_when_loaded_and_authed() {
        // loaded + authed + visible → poll
        assert!(
            super::should_refetch_on_tick(false, false, false),
            "loaded + authed + visible → poll"
        );
        assert!(
            !super::should_refetch_on_tick(true, false, false),
            "still loading → skip"
        );
        assert!(
            !super::should_refetch_on_tick(false, true, false),
            "unauthenticated → skip"
        );
        // hidden tab → skip even when loaded + authed. This row FAILS if the
        // `!hidden` term is dropped from the guard, pinning Fix B (#1628).
        assert!(
            !super::should_refetch_on_tick(false, false, true),
            "hidden tab → skip"
        );
        assert!(
            !super::should_refetch_on_tick(true, true, true),
            "loading + unauth + hidden → skip"
        );
    }
}

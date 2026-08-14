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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use crate::components::meeting_info::MeetingInfo;
use crate::components::peer_list_item::PeerListItem;
use crate::constants::meeting_api_client;
use crate::context::{HostSetCtx, RaisedHandsCtx, RecordingSetCtx, VideoCallClientCtx};
use dioxus::prelude::*;
use futures::future::{AbortHandle, Abortable};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use videocall_diagnostics::{recv_loop_action, subscribe, DiagEvent, MetricValue, RecvLoopAction};

/// One row in the peer-list sidebar.
///
/// Keyed by `session_id` so multiple sessions belonging to the same
/// authenticated `user_id` render as separate rows — one per tab.
#[derive(Clone, PartialEq, Debug)]
pub struct PeerListEntry {
    pub session_id: String,
    /// Authenticated user id. Multiple entries may share the same `user_id`
    /// when one user is connected from multiple tabs. Host actions
    /// (mute / disable video) apply at the `user_id` level — every session
    /// of a muted user gets muted server-side.
    pub user_id: String,
}

/// What Escape should do while focus is in the peer-list search box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchEscapeAction {
    /// Non-empty query: Escape clears the query and is swallowed (stop +
    /// prevent-default) so the panel stays OPEN — the standard search-field
    /// Escape idiom: first Escape clears, only a second Escape dismisses.
    ClearOnly,
    /// Empty query: Escape is left to bubble so the panel's own background
    /// light-dismiss (issue #1790, handled on `#main-container`) closes the
    /// peer list.
    Bubble,
}

/// Decide what Escape does in the search box from whether the query is empty.
/// Extracted as a pure fn so the clear-before-close precedence is unit-tested
/// without a DOM.
fn search_escape_action(query_empty: bool) -> SearchEscapeAction {
    if query_empty {
        SearchEscapeAction::Bubble
    } else {
        SearchEscapeAction::ClearOnly
    }
}

#[component]
pub fn PeerList(
    peers: Vec<PeerListEntry>,
    onclose: EventHandler<MouseEvent>,
    #[props(default = true)] self_muted: bool,
    #[props(default = false)] self_speaking: bool,
    show_meeting_info: bool,
    room_id: String,
    num_participants: usize,
    is_active: bool,
    on_toggle_meeting_info: EventHandler<()>,
    #[props(default)] host_display_name: Option<String>,
    #[props(default)] host_user_id: Option<String>,
    #[props(default)] local_user_display_name: String,
    #[props(default)] on_edit_self_name: EventHandler<()>,
) -> Element {
    let mut search_query = use_signal(String::new);
    let mut show_context_menu = use_signal(|| false);
    let mut show_incall_menu = use_signal(|| false);
    let mut is_muting_all = use_signal(|| false);
    let mut is_disabling_video_all = use_signal(|| false);

    // Track peer audio, video, and speaking states from diagnostics.
    // These maps are keyed by session_id — the `to_peer` metric emitted by
    // `broadcast_peer_status` / `peer_speaking` carries `sid_str` (see
    // `videocall-client/src/decode/peer_decode_manager.rs` and
    // `videocall-client/src/decode/neteq_audio_decoder.rs`).
    let mut peer_audio_states = use_signal(HashMap::<String, bool>::new);
    let mut peer_video_states = use_signal(HashMap::<String, bool>::new);
    let mut peer_speaking_states = use_signal(HashMap::<String, bool>::new);

    // Subscribe to diagnostics for peer_status and peer_speaking updates
    let _client = use_context::<VideoCallClientCtx>();
    let prev_abort_handle = use_hook(|| Rc::new(RefCell::new(None::<AbortHandle>)));
    use_effect(move || {
        if let Some(h) = prev_abort_handle.borrow_mut().take() {
            h.abort();
        }
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        *prev_abort_handle.borrow_mut() = Some(abort_handle);

        let fut = async move {
            let mut rx = subscribe();
            loop {
                // Issue 2174: a bare `while let Ok(..)` here died permanently on
                // the first `Overflowed`, which is recoverable — see
                // `videocall_diagnostics::recv_loop_action`. The roster's mic /
                // camera / speaking dots then froze at their last value.
                let evt = match rx.recv().await {
                    Ok(evt) => evt,
                    Err(e) => match recv_loop_action(&e) {
                        RecvLoopAction::Continue => continue,
                        RecvLoopAction::Break => break,
                    },
                };
                handle_peer_list_diagnostics(
                    &evt,
                    &mut peer_audio_states,
                    &mut peer_video_states,
                    &mut peer_speaking_states,
                );
            }
        };
        let abortable = Abortable::new(fut, abort_reg);
        wasm_bindgen_futures::spawn_local(async move {
            let _ = abortable.await;
        });
    });

    // Get client from context to resolve per-session display names.
    let client_ctx = use_context::<VideoCallClientCtx>();
    let audio_states = peer_audio_states();
    let video_states = peer_video_states();
    let speaking_states = peer_speaking_states();

    // Filter by search query against session_id, user_id, and display_name.
    // Iterate the entry vec directly — no HashMap dedup step, which is what
    // collapsed same-user sessions in the pre-fix code.
    let filtered_peers: Vec<PeerListEntry> =
        filter_peers_for_search(&peers, &search_query(), |sid| {
            client_ctx.get_peer_display_name(sid)
        });

    // Use the local_user_display_name passed as prop (reactive, updates on rename)
    let display_name = local_user_display_name.clone();

    // Check if current user is host by comparing authenticated user_ids
    // (not display names, which are user-chosen and spoofable).
    // We need the current user's user_id from the client context.
    let current_user_id_val = client_ctx.user_id().clone();
    // Local session id for the self-row's per-recorder indicator. Recording is a
    // per-session action (see `RecordingSetCtx`), so the self-row keys on THIS
    // session, not the user_id — a sibling tab of the same account records
    // independently. Empty when the session has not been assigned yet.
    let current_session_id = client_ctx.get_own_session_id().unwrap_or_default();
    // Single-host model: the current host comes from the reactive `HostSetCtx`
    // (updated live on HOST_GRANTED/HOST_REVOKED), with a fallback to the
    // `host_user_id` prop when the context is absent.
    let host_set = try_use_context::<HostSetCtx>();
    let is_host_uid = move |uid: &str| -> bool {
        match host_set.as_ref() {
            Some(hs) => hs.is_host(uid),
            None => host_user_id.as_deref() == Some(uid),
        }
    };
    let is_current_user_host = is_host_uid(&current_user_id_val);
    // Per-recorder indicator. Unlike `is_host_uid` (per-account role, keyed by
    // user_id), recording is a per-SESSION action, so this keys on `session_id`.
    // Sourced only from the reactive `RecordingSetCtx` (no persisted server state
    // to fall back to); a missing provider means "nobody is recording".
    let recording_set = try_use_context::<RecordingSetCtx>();
    let is_recording_session = move |session_id: &str| -> bool {
        recording_set
            .as_ref()
            .map(|rs| rs.is_recording(session_id))
            .unwrap_or(false)
    };
    // Issue 2135: the roster's raised-hand badge + its "position P of N" queue
    // slot, keyed on `session_id` like recording (a hand belongs to a tab, not an
    // account). The roster is the ONE surface that can show every raised hand at
    // once, so it carries the ordinal — the banner collapses past the third name
    // and a tile badge shows no order at all.
    //
    // The ordinal lives HERE and nowhere else, deliberately. This is a single
    // component rendering all rows, so the closure's reads cost ONE subscription
    // to the roster signal no matter how many rows exist. Resolving the same
    // ordinal inside each `PeerTile` would instead cost N subscriptions to a
    // value that moves for everyone whenever anyone lowers — the fan-out the
    // #2135 perf review flagged.
    let raised_hands_ctx = try_use_context::<RaisedHandsCtx>();
    let hand_slot_of = move |session_id: &str| -> Option<(usize, usize)> {
        raised_hands_ctx
            .as_ref()
            .and_then(|rh| rh.queue_slot(session_id))
    };

    rsx! {
        div {
            // Show meeting information at the top when enabled
            if show_meeting_info {
                MeetingInfo {
                    is_open: true,
                    onclose: move |_| on_toggle_meeting_info.call(()),
                    room_id: room_id.clone(),
                    num_participants: num_participants,
                    is_active: is_active,
                }
            }

            div { class: "sidebar-header",
                h2 { "Attendants" }
                div { class: "header-actions",
                    button {
                        class: "menu-button",
                        onclick: move |e: MouseEvent| {
                            e.stop_propagation();
                            show_context_menu.set(!show_context_menu());
                        },
                        aria_label: "More options",
                        svg {
                            xmlns: "http://www.w3.org/2000/svg",
                            width: "20",
                            height: "20",
                            view_box: "0 0 24 24",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            circle { cx: "12", cy: "12", r: "1" }
                            circle { cx: "12", cy: "5", r: "1" }
                            circle { cx: "12", cy: "19", r: "1" }
                        }
                    }
                    button {
                        class: "close-button",
                        onclick: move |e| onclose.call(e),
                        "\u{00d7}"
                    }
                    if show_context_menu() {
                        div { class: "context-menu",
                            button {
                                class: "context-menu-item",
                                onclick: move |_| on_toggle_meeting_info.call(()),
                                svg {
                                    xmlns: "http://www.w3.org/2000/svg",
                                    width: "16",
                                    height: "16",
                                    view_box: "0 0 24 24",
                                    fill: "none",
                                    stroke: "currentColor",
                                    stroke_width: "2",
                                    stroke_linecap: "round",
                                    stroke_linejoin: "round",
                                    circle { cx: "12", cy: "12", r: "10" }
                                    line { x1: "12", y1: "16", x2: "12", y2: "12" }
                                    line { x1: "12", y1: "8", x2: "12.01", y2: "8" }
                                }
                                if show_meeting_info { "Hide Meeting Info" } else { "Show Meeting Info" }
                            }
                        }
                    }
                }
            }

            // Sidebar content
            div { class: "sidebar-content",
                div { class: "search-container",
                    input {
                        r#type: "text",
                        placeholder: "Search attendants...",
                        value: "{search_query}",
                        oninput: move |e: Event<FormData>| {
                            search_query.set(e.value());
                        },
                        onkeydown: move |e: Event<KeyboardData>| {
                            // Escape in the search box: with a non-empty query,
                            // clear it and swallow the key so the panel stays open
                            // (first Escape clears). With an empty query, let Escape
                            // bubble so the panel's #1790 light-dismiss closes it.
                            if e.key() == Key::Escape {
                                match search_escape_action(search_query().is_empty()) {
                                    SearchEscapeAction::ClearOnly => {
                                        e.stop_propagation();
                                        e.prevent_default();
                                        search_query.set(String::new());
                                    }
                                    SearchEscapeAction::Bubble => {}
                                }
                            }
                        },
                        class: "search-input",
                    }
                }

                div { class: "attendants-section",
                    div { class: "in-call-header",
                        h3 { "In call" }
                        if is_current_user_host {
                            {
                                let room_id_for_mute = room_id.clone();
                                let room_id_for_disable_video_all = room_id.clone();
                                rsx! {
                                    div { class: "in-call-menu-wrapper",
                                        button {
                                            class: "menu-button",
                                            title: "Host actions",
                                            aria_label: "Host actions",
                                            onclick: move |e: MouseEvent| {
                                                e.stop_propagation();
                                                show_incall_menu.set(!show_incall_menu());
                                            },
                                            svg {
                                                xmlns: "http://www.w3.org/2000/svg",
                                                width: "16",
                                                height: "16",
                                                view_box: "0 0 24 24",
                                                fill: "none",
                                                stroke: "currentColor",
                                                stroke_width: "2",
                                                stroke_linecap: "round",
                                                stroke_linejoin: "round",
                                                circle { cx: "12", cy: "12", r: "1" }
                                                circle { cx: "12", cy: "5", r: "1" }
                                                circle { cx: "12", cy: "19", r: "1" }
                                            }
                                        }
                                        if show_incall_menu() {
                                            div {
                                                style: "position: fixed; inset: 0; z-index: 999;",
                                                onclick: move |_| show_incall_menu.set(false),
                                            }
                                            div { class: "context-menu",
                                                button {
                                                    class: "context-menu-item",
                                                    disabled: is_muting_all(),
                                                    onclick: move |_| {
                                                        if is_muting_all() { return; }
                                                        is_muting_all.set(true);
                                                        show_incall_menu.set(false);
                                                        let meeting_id = room_id_for_mute.clone();
                                                        spawn(async move {
                                                            match meeting_api_client() {
                                                                Ok(client) => {
                                                                    if let Err(e) = client.mute_all(&meeting_id).await {
                                                                        log::warn!("mute_all failed: {e}");
                                                                    }
                                                                }
                                                                Err(e) => log::warn!("meeting_api_client error: {e}"),
                                                            }
                                                            is_muting_all.set(false);
                                                        });
                                                    },
                                                    svg {
                                                        xmlns: "http://www.w3.org/2000/svg",
                                                        width: "16",
                                                        height: "16",
                                                        view_box: "0 0 24 24",
                                                        fill: "none",
                                                        stroke: "currentColor",
                                                        stroke_width: "2",
                                                        stroke_linecap: "round",
                                                        stroke_linejoin: "round",
                                                        line { x1: "1", y1: "1", x2: "23", y2: "23" }
                                                        path { d: "M9 9v3a3 3 0 0 0 5.12 2.12M15 9.34V4a3 3 0 0 0-5.94-.6" }
                                                        path { d: "M17 16.95A7 7 0 0 1 5 12v-2m14 0v2a7 7 0 0 1-.11 1.23" }
                                                        line { x1: "12", y1: "19", x2: "12", y2: "23" }
                                                        line { x1: "8", y1: "23", x2: "16", y2: "23" }
                                                    }
                                                    if is_muting_all() { "Muting..." } else { "Mute all" }
                                                }
                                                button {
                                                    class: "context-menu-item",
                                                    disabled: is_disabling_video_all(),
                                                    onclick: move |_| {
                                                        if is_disabling_video_all() { return; }
                                                        is_disabling_video_all.set(true);
                                                        show_incall_menu.set(false);
                                                        let meeting_id = room_id_for_disable_video_all.clone();
                                                        spawn(async move {
                                                            match meeting_api_client() {
                                                                Ok(client) => {
                                                                    if let Err(e) = client.disable_video_all(&meeting_id).await {
                                                                        log::warn!("disable_video_all failed: {e}");
                                                                    }
                                                                }
                                                                Err(e) => log::warn!("meeting_api_client error: {e}"),
                                                            }
                                                            is_disabling_video_all.set(false);
                                                        });
                                                    },
                                                    svg {
                                                        xmlns: "http://www.w3.org/2000/svg",
                                                        width: "16",
                                                        height: "16",
                                                        view_box: "0 0 24 24",
                                                        fill: "none",
                                                        stroke: "currentColor",
                                                        stroke_width: "2",
                                                        stroke_linecap: "round",
                                                        stroke_linejoin: "round",
                                                        path { d: "M16 16v1a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2m5.66 0H14a2 2 0 0 1 2 2v3.34l1 1L23 7v10" }
                                                        line { x1: "1", y1: "1", x2: "23", y2: "23" }
                                                    }
                                                    if is_disabling_video_all() { "Disabling video..." } else { "Disable video for all" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    div { class: "peer-list",
                        ul {
                            // show self as the first item with actual username
                            li { PeerListItem { name: display_name.clone(), is_host: is_current_user_host, is_recording: is_recording_session(&current_session_id), is_self: true, is_guest: client_ctx.is_local_guest().unwrap_or(false), muted: self_muted, speaking: self_speaking, hand_slot: hand_slot_of(&current_session_id), on_edit_name: on_edit_self_name } }

                            for peer in filtered_peers.iter() {
                                {
                                    // Each entry is one session — look up per-session
                                    // state (display name, guest flag, audio/video/
                                    // speaking) by session_id. Multiple entries may
                                    // share `peer.user_id` when one user is connected
                                    // from several tabs; each tab gets its own row.
                                    let sid = peer.session_id.as_str();
                                    let user_id = peer.user_id.clone();
                                    let peer_display_name = client_ctx
                                        .get_peer_display_name(sid)
                                        .unwrap_or_else(|| user_id.clone());
                                    let peer_is_guest = client_ctx
                                        .get_peer_is_guest(sid)
                                        .unwrap_or(false);
                                    // Compare using authenticated user_id, not display name.
                                    // Host is a per-user role, not per-session, so every
                                    // session of the host's user_id renders with the host
                                    // indicator. Read from `is_host_uid`.
                                    let is_peer_host = is_host_uid(&user_id);
                                    // Issue 2174 follow-up: an absent map entry
                                    // means "no heartbeat yet", not "off" —
                                    // resolve it from the client's live
                                    // snapshot so a peer who joined, or a panel
                                    // opened, less than one heartbeat ago does
                                    // not render as muted with video off AND
                                    // does not withhold the host's mute /
                                    // disable-video controls, which are gated
                                    // on these two flags below.
                                    let muted = !resolve_roster_media_flag(
                                        audio_states.get(sid).copied(),
                                        || client_ctx.is_audio_enabled_for_peer(sid),
                                    );
                                    let video_disabled = !resolve_roster_media_flag(
                                        video_states.get(sid).copied(),
                                        || client_ctx.is_video_enabled_for_peer(sid),
                                    );
                                    let speaking = speaking_states
                                        .get(sid)
                                        .copied()
                                        .unwrap_or(false);
                                    // Host actions are per-user: muting any row of a
                                    // multi-session user mutes all their sessions.
                                    let on_mute = if is_current_user_host && !muted && user_id != current_user_id_val {
                                        let meeting_id = room_id.clone();
                                        let peer_user_id = user_id.clone();
                                        Some(EventHandler::new(move |_| {
                                            let meeting_id = meeting_id.clone();
                                            let peer_user_id = peer_user_id.clone();
                                            spawn(async move {
                                                match meeting_api_client() {
                                                    Ok(client) => {
                                                        if let Err(e) = client
                                                            .mute_participant(
                                                                &meeting_id,
                                                                &peer_user_id,
                                                            )
                                                            .await
                                                        {
                                                            log::warn!(
                                                                "mute_participant failed: {e}"
                                                            );
                                                        }
                                                    }
                                                    Err(e) => {
                                                        log::warn!(
                                                            "meeting_api_client error: {e}"
                                                        );
                                                    }
                                                }
                                            });
                                        }))
                                    } else {
                                        None
                                    };
                                    // Provide a disable-video callback when the
                                    // local user is the host and the peer's
                                    // camera is currently on. Same per-user
                                    // contract as mute above.
                                    let on_disable_video = if is_current_user_host && !video_disabled && user_id != current_user_id_val {
                                        let meeting_id = room_id.clone();
                                        let peer_user_id = user_id.clone();
                                        Some(EventHandler::new(move |_| {
                                            let meeting_id = meeting_id.clone();
                                            let peer_user_id = peer_user_id.clone();
                                            spawn(async move {
                                                match meeting_api_client() {
                                                    Ok(client) => {
                                                        if let Err(e) = client
                                                            .disable_video_participant(
                                                                &meeting_id,
                                                                &peer_user_id,
                                                            )
                                                            .await
                                                        {
                                                            log::warn!(
                                                                "disable_video_participant failed: {e}"
                                                            );
                                                        }
                                                    }
                                                    Err(e) => {
                                                        log::warn!(
                                                            "meeting_api_client error: {e}"
                                                        );
                                                    }
                                                }
                                            });
                                        }))
                                    } else {
                                        None
                                    };
                                    // Remove from meeting: shown whenever the local user is host.
                                    let on_kick = if is_current_user_host && user_id != current_user_id_val {
                                        let meeting_id = room_id.clone();
                                        let peer_user_id = user_id.clone();
                                        Some(EventHandler::new(move |_| {
                                            let meeting_id = meeting_id.clone();
                                            let peer_user_id = peer_user_id.clone();
                                            spawn(async move {
                                                match meeting_api_client() {
                                                    Ok(client) => {
                                                        if let Err(e) = client
                                                            .kick_participant(
                                                                &meeting_id,
                                                                &peer_user_id,
                                                            )
                                                            .await
                                                        {
                                                            log::warn!(
                                                                "kick_participant failed: {e}"
                                                            );
                                                        }
                                                    }
                                                    Err(e) => {
                                                        log::warn!(
                                                            "meeting_api_client error: {e}"
                                                        );
                                                    }
                                                }
                                            });
                                        }))
                                    } else {
                                        None
                                    };
                                    // Transfer host action is shown when the local user is the host, the peer is not the local user, and the peer is not a guest.
                                    let on_transfer_host = if is_current_user_host
                                        && user_id != current_user_id_val
                                        && !peer_is_guest
                                    {
                                        let meeting_id = room_id.clone();
                                        let peer_user_id = user_id.clone();
                                        Some(EventHandler::new(move |_| {
                                            let meeting_id = meeting_id.clone();
                                            let peer_user_id = peer_user_id.clone();
                                            spawn(async move {
                                                match meeting_api_client() {
                                                    Ok(client) => {
                                                        if let Err(e) = client.transfer_host(&meeting_id, &peer_user_id).await {
                                                            log::warn!("transfer_host failed: {e}");
                                                        }
                                                    }
                                                    Err(e) => log::warn!("meeting_api_client error: {e}"),
                                                }
                                            });
                                        }))
                                    } else {
                                        None
                                    };
                                    let row_key = peer.session_id.clone();
                                    let tooltip_user_id = user_id.clone();
                                    rsx! {
                                        li {
                                            key: "{row_key}",
                                            PeerListItem {
                                                name: peer_display_name,
                                                tooltip: tooltip_user_id,
                                                is_host: is_peer_host,
                                                is_recording: is_recording_session(sid),
                                                hand_slot: hand_slot_of(sid),
                                                is_guest: peer_is_guest,
                                                muted: muted,
                                                video_disabled: video_disabled,
                                                speaking: speaking,
                                                on_mute: on_mute,
                                                on_disable_video: on_disable_video,
                                                on_kick: on_kick,
                                                on_transfer_host: on_transfer_host,
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
    }
}

/// Filter a session-keyed peer list against the search box.
///
/// Matches if the query is a substring (case-insensitive) of display name,
/// user_id, or session_id.
fn filter_peers_for_search<F>(
    peers: &[PeerListEntry],
    query: &str,
    get_display_name: F,
) -> Vec<PeerListEntry>
where
    F: Fn(&str) -> Option<String>,
{
    let q = query.to_lowercase();
    peers
        .iter()
        .filter(|p| {
            let display_name = get_display_name(&p.session_id).unwrap_or_default();
            p.session_id.to_lowercase().contains(&q)
                || p.user_id.to_lowercase().contains(&q)
                || display_name.to_lowercase().contains(&q)
        })
        .cloned()
        .collect()
}

/// Resolve a roster media flag (audio-on / video-on) for one peer: the
/// diagnostics map wins whenever it holds an entry, otherwise fall back to the
/// client's live snapshot.
///
/// Issue 2174 follow-up: these maps start empty and are only ever filled by
/// `peer_status`, so defaulting an absent peer to "off" mis-rendered live
/// participants for up to one 5 s keepalive after the panel was opened — and
/// not only cosmetically. The host's per-row controls are gated on these same
/// two flags (`on_mute` on `!muted`, `on_disable_video` on `!video_disabled`),
/// so an unreported peer offered the host NO mute and NO disable-video button
/// at all — precisely when a host is most likely to reach for them, in the
/// first seconds after opening the roster. The snapshot reads the same
/// `peer_decode_manager` fields the heartbeat is built from
/// (`is_audio_enabled_for_peer` / `is_video_enabled_for_peer`), so the fallback
/// cannot disagree with the event that later replaces it.
///
/// Both client calls themselves return `false` when the peer is absent from the
/// decode manager or its `inner` is momentarily borrowed, so a genuinely
/// unknown peer still degrades to the previous "off" rendering. The fallback
/// upgrades the *known-live* case; it does not invent state.
///
/// The map still owns reactivity — it is the signal whose writes re-render the
/// row. The snapshot only supplies a better value than "off" while the map has
/// nothing to say, which is also why it is taken lazily: in the steady state
/// every peer has an entry and the client is never consulted.
fn resolve_roster_media_flag(known: Option<bool>, client_snapshot: impl FnOnce() -> bool) -> bool {
    known.unwrap_or_else(client_snapshot)
}

/// Resolve a `peer_speaking` claim for the roster against the last known
/// `audio_enabled` for that peer.
///
/// Issue 2174 follow-up: the roster's two speaking sources are asymmetric. The
/// `is_speaking` flag on a `peer_status` heartbeat is already audio-gated at
/// the producer (`self.is_speaking = metadata.is_speaking && resolved_audio` in
/// `peer_decode_manager.rs`), but a `peer_speaking` event carries the decoder's
/// raw VAD result with no `audio_enabled` field at all — nothing in the event
/// says whether the peer is still allowed to be speaking.
///
/// This veto is defense-in-depth at the STATE layer; it is NOT what prevents a
/// visible contradiction. `PeerListItem` builds both `mic_class` and
/// `mic_style` under `speaking && !muted`, and `muted` derives from the same
/// `peer_audio_states` map this fn consults, so a row whose glyph reads muted
/// structurally cannot show a lit dot with or without the veto. What the veto
/// buys is that `peer_speaking_states` itself stays honest: a stale `true` for
/// a muted peer is what a future consumer — or any relaxation of that render
/// guard — would trust, and nothing else would correct the entry until the next
/// real VAD transition.
///
/// On today's code paths it never actually fires, which is worth stating rather
/// than implying otherwise. The producer gate added alongside it suppresses the
/// input: `VadState::observe` in `neteq_audio_decoder.rs` early-returns while
/// `suppressed`, which `set_muted(true)` sets, so no `speaking: 1` is emitted
/// after a mute at all. And unlike the tile, the roster has no optimistic local
/// write that could run ahead of that gate — `peer_audio_states` moves only in
/// the `peer_status` arm below — so `Some(false)` here always implies the local
/// decoder is already suppressed. The veto is kept for the producer that does
/// not carry that suppression.
///
/// Consequence for testing, which is a trap for the next author: the roster
/// half is NOT e2e-observable. Because the render guard already couples the dot
/// to `!muted`, every case emits identical DOM with and without the veto. The
/// unit test below is therefore the only guard this rule can have, and must not
/// be swapped for a Playwright spec.
///
/// `None` means the roster has not seen a `peer_status` for this peer yet, and
/// must NOT veto. That state stays reachable here — [`resolve_roster_media_flag`]
/// improves what the row *renders* for an unreported peer, it does not populate
/// this map — so a genuinely unmuted peer speaking before their first heartbeat
/// reaches this fn as `None`, passes through, and now really does light the dot
/// because the row no longer resolves them as muted.
///
/// Note this is the same rule as `effective_level` at the FUNCTION level (only
/// an explicit `Some(false)` vetoes) but the OPPOSITE default at the SYSTEM
/// level, deliberately. The tile passes `Some(*audio_enabled.peek())` off a
/// `Signal<bool>` that has no unknown state, so its unknown has already
/// collapsed to `false` at the seed and fails CLOSED — it suppresses. The
/// roster keeps a real tri-state and fails OPEN. Each is the conservative
/// choice for its own consumer: the tile is guarding a full-tile glow it can
/// re-light within one heartbeat, while the roster would otherwise blank the
/// only speaking indicator the host has for a peer it simply has not heard
/// about yet.
fn resolve_roster_speaking(speaking: bool, audio_enabled: Option<bool>) -> bool {
    if audio_enabled == Some(false) {
        return false;
    }
    speaking
}

fn handle_peer_list_diagnostics(
    evt: &DiagEvent,
    peer_audio_states: &mut Signal<HashMap<String, bool>>,
    peer_video_states: &mut Signal<HashMap<String, bool>>,
    peer_speaking_states: &mut Signal<HashMap<String, bool>>,
) {
    match evt.subsystem {
        "peer_status" => {
            let mut to_peer: Option<String> = None;
            let mut audio_enabled: Option<bool> = None;
            let mut video_enabled: Option<bool> = None;
            let mut is_speaking: Option<bool> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.to_string()),
                    ("audio_enabled", MetricValue::U64(v)) => audio_enabled = Some(*v != 0),
                    ("video_enabled", MetricValue::U64(v)) => video_enabled = Some(*v != 0),
                    ("is_speaking", MetricValue::U64(v)) => is_speaking = Some(*v != 0),
                    _ => {}
                }
            }
            if let Some(peer) = to_peer {
                if let Some(audio) = audio_enabled {
                    let current = match peer_audio_states.try_peek() {
                        Ok(map) => map.get(&peer).copied(),
                        Err(_) => return,
                    };
                    if current != Some(audio) {
                        peer_audio_states.write().insert(peer.clone(), audio);
                    }
                }
                if let Some(video) = video_enabled {
                    let current = match peer_video_states.try_peek() {
                        Ok(map) => map.get(&peer).copied(),
                        Err(_) => return,
                    };
                    if current != Some(video) {
                        peer_video_states.write().insert(peer.clone(), video);
                    }
                }
                if let Some(speaking) = is_speaking {
                    let current = match peer_speaking_states.try_peek() {
                        Ok(map) => map.get(&peer).copied(),
                        Err(_) => return,
                    };
                    if current != Some(speaking) {
                        peer_speaking_states.write().insert(peer, speaking);
                    }
                }
            }
        }
        "peer_speaking" => {
            // Borrow the peer id rather than allocating: `peer_speaking` is by
            // far the highest-rate subsystem on this bus (the decoder VAD emits
            // on every level change > 0.02 while anyone talks) and the vast
            // majority of events end at the unchanged-state check below. Only
            // the rare edge that actually writes the map needs an owned key.
            let mut to_peer: Option<&str> = None;
            let mut speaking: Option<bool> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.as_ref()),
                    ("speaking", MetricValue::U64(v)) => speaking = Some(*v != 0),
                    _ => {}
                }
            }
            if let (Some(peer), Some(speaking_val)) = (to_peer, speaking) {
                // Issue 2174 follow-up: veto the raw VAD claim against the last
                // known audio state, mirroring `effective_level` in
                // `peer_tile.rs`. Without this a straggler from the decoder
                // pipeline lights the roster's speaking dot on a row whose mic
                // glyph already reads muted.
                let audio_known = match peer_audio_states.try_peek() {
                    Ok(map) => map.get(peer).copied(),
                    Err(_) => return,
                };
                let speaking_val = resolve_roster_speaking(speaking_val, audio_known);
                let current = match peer_speaking_states.try_peek() {
                    Ok(map) => map.get(peer).copied(),
                    Err(_) => return,
                };
                if current != Some(speaking_val) {
                    peer_speaking_states
                        .write()
                        .insert(peer.to_string(), speaking_val);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Issue 2174 follow-up: a peer with no `peer_status` yet must render from
    /// the client's live snapshot rather than defaulting to off, which is what
    /// made a freshly-opened roster show live participants as muted with their
    /// camera off for up to one 5 s heartbeat. Once diagnostics has reported
    /// the peer the map wins — it is the fresher of the two from then on.
    ///
    /// Mutation sensitivity: dropping the fallback (`known.unwrap_or(false)`)
    /// fails the unknown-but-live case; ignoring the map and always taking the
    /// snapshot fails the two map-wins cases.
    #[test]
    fn an_unknown_roster_peer_falls_back_to_the_client_snapshot() {
        assert!(
            resolve_roster_media_flag(None, || true),
            "an unreported peer that the client says is live must render as live"
        );
        assert!(
            !resolve_roster_media_flag(None, || false),
            "an unreported peer the client says is off stays off"
        );
        assert!(
            resolve_roster_media_flag(Some(true), || false),
            "a peer the map reports as on ignores a stale off snapshot"
        );
        assert!(
            !resolve_roster_media_flag(Some(false), || true),
            "a peer the map reports as off ignores a stale on snapshot"
        );
    }

    /// Issue 2174 follow-up: a straggler `peer_speaking` from the decoder
    /// pipeline must not leave `peer_speaking_states` latched at `true` for a
    /// muted peer. `PeerListItem` gates the rendered dot on `speaking &&
    /// !muted`, so this is map hygiene rather than a pixel guard — the stale
    /// entry would otherwise survive until the next real VAD transition.
    ///
    /// Do NOT retire this in favour of an E2E spec: the render guard makes the
    /// roster emit identical DOM with and without the veto, so this unit test
    /// is the only guard the rule can have. See `resolve_roster_speaking`.
    ///
    /// Mutation sensitivity: dropping the `Some(false)` guard from
    /// `resolve_roster_speaking` returns `true` for the first case and fails
    /// this test.
    #[test]
    fn a_muted_peer_cannot_latch_the_roster_speaking_state() {
        assert!(
            !resolve_roster_speaking(true, Some(false)),
            "a muted peer's speaking claim must be suppressed"
        );
        assert!(
            !resolve_roster_speaking(false, Some(false)),
            "a muted peer that is not speaking stays dark"
        );
    }

    /// The veto must never suppress a legitimate speaker. `None` is "no
    /// heartbeat seen yet", and thanks to the snapshot fallback in
    /// `resolve_roster_media_flag` such a peer no longer renders as muted — so
    /// a peer already talking when the panel opens really can light the dot,
    /// and the veto must let the claim through.
    ///
    /// Mutation sensitivity: widening the guard to `!= Some(true)` (treating
    /// unknown as muted) fails the `None` case.
    #[test]
    fn an_unmuted_or_unknown_peer_keeps_its_speaking_claim() {
        assert!(
            resolve_roster_speaking(true, Some(true)),
            "an audio-enabled peer's speaking claim passes through"
        );
        assert!(
            resolve_roster_speaking(true, None),
            "an unknown audio state must not suppress a speaking claim"
        );
        assert!(!resolve_roster_speaking(false, Some(true)));
        assert!(!resolve_roster_speaking(false, None));
    }

    /// Escape in the search box clears a non-empty query first (staying open),
    /// and only bubbles to close the panel once the query is empty. A regression
    /// that dropped the clear step (Escape always bubbles) would flip the
    /// non-empty case to `Bubble` and fail here — this pins the #1790
    /// clear-before-close precedence.
    #[test]
    fn search_escape_clears_before_closing() {
        assert_eq!(search_escape_action(false), SearchEscapeAction::ClearOnly);
        assert_eq!(search_escape_action(true), SearchEscapeAction::Bubble);
    }

    /// Build a display-name lookup closure backed by a `HashMap<sid, name>`,
    /// mirroring the shape of `VideoCallClientCtx::get_peer_display_name`.
    fn display_name_lookup(map: HashMap<String, String>) -> impl Fn(&str) -> Option<String> {
        move |sid: &str| map.get(sid).cloned()
    }

    /// Three sessions of the SAME user_id must survive the filter as three
    /// distinct rows, each with their own display name.
    #[test]
    fn filter_peers_keeps_all_same_user_sessions() {
        let peers = vec![
            PeerListEntry {
                session_id: "sid-a".into(),
                user_id: "shared-user".into(),
            },
            PeerListEntry {
                session_id: "sid-b".into(),
                user_id: "shared-user".into(),
            },
            PeerListEntry {
                session_id: "sid-c".into(),
                user_id: "shared-user".into(),
            },
        ];
        let mut names = HashMap::new();
        names.insert("sid-a".to_string(), "Tab A".to_string());
        names.insert("sid-b".to_string(), "Tab B".to_string());
        names.insert("sid-c".to_string(), "Tab C".to_string());
        let lookup = display_name_lookup(names);

        let filtered = filter_peers_for_search(&peers, "", lookup);

        // Three rows — one per session, NOT one per user_id.
        assert_eq!(
            filtered.len(),
            3,
            "all three same-user sessions must survive the filter"
        );

        // Each row is keyed on its own session_id (not user_id), so the
        // render loop can look up the per-session display name correctly.
        let sids: Vec<String> = filtered.iter().map(|p| p.session_id.clone()).collect();
        assert!(sids.contains(&"sid-a".to_string()));
        assert!(sids.contains(&"sid-b".to_string()));
        assert!(sids.contains(&"sid-c".to_string()));

        // All three rows share the same user_id — that is the multi-session
        // condition we are testing.
        assert!(filtered.iter().all(|p| p.user_id == "shared-user"));

        // Simulating the render-time per-row display-name resolution
        // (`client_ctx.get_peer_display_name(sid)`), each row resolves to its
        // own session-specific name. The pre-fix code instead looked up
        // names through a `HashMap<user_id, session_id>` collected from the
        // peer keys — which collapsed all three same-user rows to the same
        // session_id, so all three rows would show the SAME (last-inserted)
        // display name. The session-keyed entry shape proves this collapse
        // can no longer happen.
        let names_lookup: HashMap<String, String> =
            [("sid-a", "Tab A"), ("sid-b", "Tab B"), ("sid-c", "Tab C")]
                .iter()
                .map(|(s, n)| (s.to_string(), n.to_string()))
                .collect();
        let resolved_names: Vec<String> = filtered
            .iter()
            .map(|p| {
                names_lookup
                    .get(&p.session_id)
                    .cloned()
                    .unwrap_or_else(|| p.user_id.clone())
            })
            .collect();
        let unique_names: std::collections::HashSet<String> =
            resolved_names.iter().cloned().collect();
        assert_eq!(
            unique_names.len(),
            3,
            "each same-user session row must resolve to a distinct display name, got {resolved_names:?}"
        );
    }

    /// Search by display name matches the right session even when multiple
    /// rows share a user_id. With user-id-only filtering the search query
    /// "Tab B" would match nothing because the user_id is "shared-user".
    #[test]
    fn filter_peers_search_matches_display_name() {
        let peers = vec![
            PeerListEntry {
                session_id: "sid-a".into(),
                user_id: "shared-user".into(),
            },
            PeerListEntry {
                session_id: "sid-b".into(),
                user_id: "shared-user".into(),
            },
        ];
        let mut names = HashMap::new();
        names.insert("sid-a".to_string(), "Tab A".to_string());
        names.insert("sid-b".to_string(), "Tab B".to_string());
        let lookup = display_name_lookup(names);

        let filtered = filter_peers_for_search(&peers, "tab b", lookup);

        assert_eq!(
            filtered.len(),
            1,
            "search should match only sid-b's display name"
        );
        assert_eq!(filtered[0].session_id, "sid-b");
    }

    /// Search by session_id matches that one row, even when display name is
    /// absent. Covers the case where peer_display_name has not yet been
    /// populated from PARTICIPANT_JOINED.
    #[test]
    fn filter_peers_search_matches_session_id() {
        let peers = vec![
            PeerListEntry {
                session_id: "sid-alpha".into(),
                user_id: "uid-1".into(),
            },
            PeerListEntry {
                session_id: "sid-beta".into(),
                user_id: "uid-2".into(),
            },
        ];
        // No display names populated.
        let lookup = display_name_lookup(HashMap::new());

        let filtered = filter_peers_for_search(&peers, "alpha", lookup);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].session_id, "sid-alpha");
    }
}

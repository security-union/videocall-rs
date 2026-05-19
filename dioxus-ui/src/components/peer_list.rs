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
use crate::context::VideoCallClientCtx;
use dioxus::prelude::*;
use futures::future::{AbortHandle, Abortable};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use videocall_diagnostics::{subscribe, DiagEvent, MetricValue};

/// One row in the peer-list sidebar.
///
/// Keyed by `session_id` (unique per browser tab / WebTransport or WebSocket
/// connection), so multiple sessions belonging to the same authenticated
/// `user_id` render as multiple rows — one per tab. Pre-dating this struct
/// the prop was `Vec<user_id>`, which collapsed N same-user sessions into a
/// single row whose display name was non-deterministic (the HashMap collect
/// kept whichever entry inserted last). See HCL #828 follow-up.
#[derive(Clone, PartialEq, Debug)]
pub struct PeerListEntry {
    /// The peer's session_id — the unique per-connection key used to look up
    /// per-session state (display name, audio/video/speaking maps).
    pub session_id: String,
    /// The peer's authenticated `user_id`. Multiple entries may share the
    /// same `user_id` when one user is connected from multiple tabs. Host
    /// actions (mute / disable video) still apply at the `user_id` level by
    /// design — see PR #556.
    pub user_id: String,
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
            while let Ok(evt) = rx.recv().await {
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
    let is_current_user_host = host_user_id
        .as_ref()
        .map(|h| h == &current_user_id_val)
        .unwrap_or(false);

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
                            li { PeerListItem { name: display_name.clone(), is_host: is_current_user_host, is_self: true, is_guest: client_ctx.is_local_guest().unwrap_or(false), muted: self_muted, speaking: self_speaking, on_edit_name: on_edit_self_name } }

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
                                    // indicator.
                                    let is_peer_host = host_user_id
                                        .as_ref()
                                        .map(|h| h == &user_id)
                                        .unwrap_or(false);
                                    let muted = audio_states
                                        .get(sid)
                                        .copied()
                                        .map(|enabled| !enabled)
                                        .unwrap_or(true);
                                    let video_disabled = video_states
                                        .get(sid)
                                        .copied()
                                        .map(|enabled| !enabled)
                                        .unwrap_or(true);
                                    let speaking = speaking_states
                                        .get(sid)
                                        .copied()
                                        .unwrap_or(false);
                                    // Host actions (mute / disable video) apply per
                                    // user_id by design — clicking on any row of a
                                    // multi-session user mutes all of their sessions
                                    // server-side (PR #556's HOST_MUTE_PARTICIPANT
                                    // contract).
                                    let on_mute = if is_current_user_host && !muted {
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
                                    let on_disable_video = if is_current_user_host && !video_disabled {
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
                                    let row_key = peer.session_id.clone();
                                    let tooltip_user_id = user_id.clone();
                                    rsx! {
                                        li {
                                            key: "{row_key}",
                                            PeerListItem {
                                                name: peer_display_name,
                                                tooltip: tooltip_user_id,
                                                is_host: is_peer_host,
                                                is_guest: peer_is_guest,
                                                muted: muted,
                                                video_disabled: video_disabled,
                                                speaking: speaking,
                                                on_mute: on_mute,
                                                on_disable_video: on_disable_video,
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
/// Pure helper extracted so the dedup-collapsing-bug regression can be
/// unit-tested without spinning up a full `VideoCallClientCtx` (which would
/// require a WebTransport / WebSocket connection). Matches if the query is
/// a substring (case-insensitive) of any of: display name, user_id, or
/// session_id.
///
/// Pre-fix this filtering step was keyed by user_id and built a
/// `HashMap<user_id, session_id>` whose `collect` collapsed N same-user
/// sessions into one entry. By taking a `&[PeerListEntry]` and iterating
/// without dedup, every session survives — N tabs of one user produce N
/// rows. See HCL #828 follow-up.
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
                    ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
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
            let mut to_peer: Option<String> = None;
            let mut speaking: Option<bool> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
                    ("speaking", MetricValue::U64(v)) => speaking = Some(*v != 0),
                    _ => {}
                }
            }
            if let (Some(peer), Some(speaking_val)) = (to_peer, speaking) {
                let current = match peer_speaking_states.try_peek() {
                    Ok(map) => map.get(&peer).copied(),
                    Err(_) => return,
                };
                if current != Some(speaking_val) {
                    peer_speaking_states.write().insert(peer, speaking_val);
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

    /// Build a display-name lookup closure backed by a `HashMap<sid, name>`,
    /// mirroring the shape of `VideoCallClientCtx::get_peer_display_name`.
    fn display_name_lookup(map: HashMap<String, String>) -> impl Fn(&str) -> Option<String> {
        move |sid: &str| map.get(sid).cloned()
    }

    /// HCL #828 follow-up regression test: three sessions of the SAME user_id
    /// must survive the filter step as three distinct rows, each with their
    /// own display name. The pre-fix code collected user_id -> session_id
    /// into a HashMap before filtering, which dropped two of the three
    /// sessions because HashMap::collect on duplicate keys keeps the last
    /// value. With the new session-keyed entry shape this is a non-issue
    /// and the filter is a straight `Vec` walk.
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

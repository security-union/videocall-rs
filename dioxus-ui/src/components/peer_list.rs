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

#[component]
pub fn PeerList(
    peers: Vec<String>,
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

    // Track peer audio, video, and speaking states from diagnostics
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

    // Get client from context to convert session_id to user_id for display
    let client_ctx = use_context::<VideoCallClientCtx>();
    let audio_states = peer_audio_states();
    let video_states = peer_video_states();
    let speaking_states = peer_speaking_states();

    // Build reverse lookup (user_id -> session_id) once, to avoid O(N^2) scanning inside the loop.
    let user_id_to_sid: HashMap<String, String> = client_ctx
        .sorted_peer_keys()
        .into_iter()
        .filter_map(|sid| client_ctx.get_peer_user_id(&sid).map(|uid| (uid, sid)))
        .collect();

    let filtered_peers: Vec<String> = peers
        .iter()
        .filter(|peer| {
            let peer_display_name = user_id_to_sid
                .get(peer.as_str())
                .and_then(|sid| client_ctx.get_peer_display_name(sid))
                .unwrap_or_else(|| peer.to_string());
            let query = search_query().to_lowercase();
            peer.to_lowercase().contains(&query)
                || peer_display_name.to_lowercase().contains(&query)
        })
        .cloned()
        .collect();

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
                                    // peer is the display user_id; we need the session_id to look up states.
                                    // Use the pre-built reverse map for O(1) lookup instead of scanning all peers.
                                    let peer_session_id = user_id_to_sid.get(peer.as_str());
                                    let peer_display_name = peer_session_id
                                        .and_then(|sid| client_ctx.get_peer_display_name(sid))
                                        .unwrap_or_else(|| peer.clone());
                                    let peer_is_guest = peer_session_id.and_then(|sid| client_ctx.get_peer_is_guest(sid)).unwrap_or(false);
                                    // Compare using authenticated user_id, not display name
                                    let is_peer_host = host_user_id
                                        .as_ref()
                                        .map(|h| h == peer)
                                        .unwrap_or(false);
                                    let muted = peer_session_id
                                        .and_then(|sid| audio_states.get(sid).copied())
                                        .map(|enabled| !enabled)
                                        .unwrap_or(true);
                                    let video_disabled = peer_session_id
                                        .and_then(|sid| video_states.get(sid).copied())
                                        .map(|enabled| !enabled)
                                        .unwrap_or(true);
                                    let speaking = peer_session_id
                                        .and_then(|sid| speaking_states.get(sid).copied())
                                        .unwrap_or(false);
                                    // Provide a mute callback when the local user is
                                    // the host and the peer's mic is currently on.
                                    let on_mute = if is_current_user_host && !muted {
                                        let meeting_id = room_id.clone();
                                        let peer_user_id = peer.clone();
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
                                    // camera is currently on.
                                    let on_disable_video = if is_current_user_host && !video_disabled {
                                        let meeting_id = room_id.clone();
                                        let peer_user_id = peer.clone();
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
                                    rsx! {
                                        li {
                                            key: "{peer}",
                                            PeerListItem {
                                                name: peer_display_name,
                                                tooltip: peer.clone(),
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

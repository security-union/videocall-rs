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
use crate::context::{DisplayNameCtx, VideoCallClientCtx};
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
) -> Element {
    let mut search_query = use_signal(String::new);
    let mut show_context_menu = use_signal(|| false);

    // Track peer audio and speaking states from diagnostics
    let mut peer_audio_states = use_signal(HashMap::<String, bool>::new);
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

    // Get username from context and append (You)
    let display_name_ctx = use_context::<DisplayNameCtx>();
    let current_user_name: Option<String> = (display_name_ctx.0)().clone();

    let display_name = current_user_name.clone().unwrap_or_default();

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
                    h3 { "In call" }
                    div { class: "peer-list",
                        ul {
                            // show self as the first item with actual username
                            li { PeerListItem { name: display_name.clone(), is_host: is_current_user_host, is_self: true, muted: self_muted, speaking: self_speaking } }

                            for peer in filtered_peers.iter() {
                                {
                                    // peer is the display user_id; we need the session_id to look up states.
                                    // Use the pre-built reverse map for O(1) lookup instead of scanning all peers.
                                    let peer_session_id = user_id_to_sid.get(peer.as_str());
                                    let peer_display_name = peer_session_id
                                        .and_then(|sid| client_ctx.get_peer_display_name(sid))
                                        .unwrap_or_else(|| peer.clone());
                                    // Compare using authenticated user_id, not display name
                                    let is_peer_host = host_user_id
                                        .as_ref()
                                        .map(|h| h == peer)
                                        .unwrap_or(false);
                                    let muted = peer_session_id
                                        .and_then(|sid| audio_states.get(sid).copied())
                                        .map(|enabled| !enabled)
                                        .unwrap_or(true);
                                    let speaking = peer_session_id
                                        .and_then(|sid| speaking_states.get(sid).copied())
                                        .unwrap_or(false);
                                    rsx! {
                                        li {
                                            key: "{peer}",
                                            PeerListItem {
                                                name: peer_display_name.clone(),
                                                tooltip: peer.clone(),
                                                is_host: is_peer_host,
                                                muted: muted,
                                                speaking: speaking,
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
    peer_speaking_states: &mut Signal<HashMap<String, bool>>,
) {
    match evt.subsystem {
        "peer_status" => {
            let mut to_peer: Option<String> = None;
            let mut audio_enabled: Option<bool> = None;
            let mut is_speaking: Option<bool> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
                    ("audio_enabled", MetricValue::U64(v)) => audio_enabled = Some(*v != 0),
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

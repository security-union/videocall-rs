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
use videocall_theme::{icons, Icon, IconButton, ListGroup, ListHeader, Menu, MenuItem, Toolbar};

/// One remote row in the attendants list.
///
/// Rows are per session, not per user: the same account can be in a meeting
/// twice (a browser tab and `videocall-cli` on another machine), and each
/// session has its own mute and speaking state.
#[derive(Clone, PartialEq, Debug)]
pub struct PeerRow {
    pub session_id: String,
    pub user_id: String,
    pub display_name: String,
    pub is_host: bool,
    pub muted: bool,
    pub speaking: bool,
}

/// Build the remote rows for `session_ids`, in order, keeping those whose
/// user id or display name contains `query` (case-insensitive).
///
/// `user_id_of` and `display_name_of` look a session up in the client; a
/// session with no known user id falls back to its session id, and one with
/// no display name to its user id. Audio and speaking state are keyed by
/// session id, as the diagnostics `peer_status` events report them.
pub fn peer_rows(
    session_ids: &[String],
    user_id_of: impl Fn(&str) -> Option<String>,
    display_name_of: impl Fn(&str) -> Option<String>,
    host_user_id: Option<&str>,
    audio_states: &HashMap<String, bool>,
    speaking_states: &HashMap<String, bool>,
    query: &str,
) -> Vec<PeerRow> {
    let query = query.to_lowercase();
    session_ids
        .iter()
        .map(|sid| {
            let user_id = user_id_of(sid).unwrap_or_else(|| sid.clone());
            let display_name = display_name_of(sid).unwrap_or_else(|| user_id.clone());
            PeerRow {
                session_id: sid.clone(),
                // Compare authenticated user ids, not display names.
                is_host: host_user_id == Some(user_id.as_str()),
                muted: audio_states
                    .get(sid)
                    .map(|enabled| !enabled)
                    .unwrap_or(true),
                speaking: speaking_states.get(sid).copied().unwrap_or(false),
                user_id,
                display_name,
            }
        })
        .filter(|row| {
            row.user_id.to_lowercase().contains(&query)
                || row.display_name.to_lowercase().contains(&query)
        })
        .collect()
}

/// The remote rows of the attendants list, keyed by session id.
#[component]
pub fn PeerRows(rows: Vec<PeerRow>) -> Element {
    rsx! {
        for row in rows.into_iter() {
            PeerListItem {
                key: "{row.session_id}",
                name: row.display_name,
                tooltip: row.user_id,
                is_host: row.is_host,
                muted: row.muted,
                speaking: row.speaking,
            }
        }
    }
}

#[component]
pub fn PeerList(
    /// Session ids of the remote peers, in display order.
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

    // Get client from context to resolve each session to a user and name.
    let client_ctx = use_context::<VideoCallClientCtx>();
    let rows = peer_rows(
        &peers,
        |sid| client_ctx.get_peer_user_id(sid),
        |sid| client_ctx.get_peer_display_name(sid),
        host_user_id.as_deref(),
        &peer_audio_states(),
        &peer_speaking_states(),
        &search_query(),
    );

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
        div { class: "sidebar-panel",
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

            Toolbar { class: "sidebar-header",
                h2 { class: "toolbar-title", "Attendants" }
                div { class: "header-actions",
                    IconButton {
                        label: "More options",
                        onclick: move |e: MouseEvent| {
                            e.stop_propagation();
                            show_context_menu.set(!show_context_menu());
                        },
                        Icon { glyph: &icons::MORE_VERTICAL, size: "20" }
                    }
                    IconButton {
                        label: "Close",
                        class: "close-button",
                        onclick: move |e| onclose.call(e),
                        "\u{00d7}"
                    }
                    if show_context_menu() {
                        // No menu roles today; `None` keeps it that way.
                        Menu { role: None::<&str>,
                            MenuItem {
                                role: None::<&str>,
                                onclick: move |_| on_toggle_meeting_info.call(()),
                                Icon { glyph: &icons::INFO, size: "16" }
                                if show_meeting_info { "Hide Meeting Info" } else { "Show Meeting Info" }
                            }
                        }
                    }
                }
            }

            // Sidebar content
            div { class: "sidebar-content",
                div { class: "search-field",
                    Icon { glyph: &icons::SEARCH, "aria-hidden": "true" }
                    input {
                        r#type: "text",
                        placeholder: "Search attendants...",
                        value: "{search_query}",
                        oninput: move |e: Event<FormData>| {
                            search_query.set(e.value());
                        },
                    }
                }

                div { class: "attendants-section",
                    ListHeader { "In call" }
                    div { class: "peer-list",
                        // The rows stay raw `li`s (PeerListItem): one per peer.
                        ListGroup { list: true,
                            // show self as the first item with actual username
                            PeerListItem { name: display_name.clone(), is_host: is_current_user_host, is_self: true, muted: self_muted, speaking: self_speaking }

                            PeerRows { rows }
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

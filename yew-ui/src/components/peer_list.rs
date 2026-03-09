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
use futures::future::{AbortHandle, Abortable};
use std::collections::HashMap;
use videocall_diagnostics::{subscribe, DiagEvent, MetricValue};
use web_sys::HtmlInputElement;
use yew::prelude::*;
use yew::{html, Component, Context};

pub struct PeerList {
    search_query: String,
    show_context_menu: bool,
    peer_audio_states: HashMap<String, bool>,
    peer_speaking_states: HashMap<String, bool>,
    abort_handle: Option<AbortHandle>,
    local_speaking: bool,
}

#[derive(Properties, Clone, PartialEq)]
pub struct PeerListProperties {
    pub peers: Vec<String>,
    pub onclose: yew::Callback<yew::MouseEvent>,

    // audio states
    #[prop_or_default]
    pub peer_audio_states: HashMap<String, bool>,
    #[prop_or(true)]
    pub self_muted: bool,

    // speaking state for local user
    #[prop_or(false)]
    pub self_speaking: bool,

    // meeting info
    pub show_meeting_info: bool,
    pub room_id: String,
    pub num_participants: usize,
    pub is_active: bool,
    pub on_toggle_meeting_info: yew::Callback<()>,

    /// Display name (username) of the meeting host (for displaying crown icon)
    #[prop_or_default]
    pub host_display_name: Option<String>,
}

pub enum PeerListMsg {
    UpdateSearchQuery(String),
    ToggleContextMenu,
    Diagnostics(DiagEvent),
}

impl Component for PeerList {
    type Message = PeerListMsg;

    type Properties = PeerListProperties;

    fn create(ctx: &Context<Self>) -> Self {
        // Initialize with audio states from props
        PeerList {
            search_query: String::new(),
            show_context_menu: false,
            peer_audio_states: ctx.props().peer_audio_states.clone(),
            peer_speaking_states: HashMap::new(),
            abort_handle: None,
            local_speaking: ctx.props().self_speaking,
        }
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        if first_render {
            // Subscribe to global diagnostics for peer_status updates
            let link = ctx.link().clone();
            let (abort_handle, abort_reg) = AbortHandle::new_pair();
            let fut = async move {
                let mut rx = subscribe();
                while let Ok(evt) = rx.recv().await {
                    link.send_message(PeerListMsg::Diagnostics(evt));
                }
            };
            let abortable = Abortable::new(fut, abort_reg);
            self.abort_handle = Some(abort_handle);
            wasm_bindgen_futures::spawn_local(async move {
                let _ = abortable.await;
            });
        }
    }

    fn update(&mut self, _ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            PeerListMsg::UpdateSearchQuery(query) => {
                self.search_query = query;
                true
            }
            PeerListMsg::ToggleContextMenu => {
                self.show_context_menu = !self.show_context_menu;
                true
            }
            PeerListMsg::Diagnostics(evt) => {
                match evt.subsystem {
                    "peer_status" => {
                        // Parse peer_status metrics for audio enabled state AND speaking state
                        let mut to_peer: Option<String> = None;
                        let mut audio_enabled: Option<bool> = None;
                        let mut is_speaking: Option<bool> = None;
                        for m in &evt.metrics {
                            match (m.name, &m.value) {
                                ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
                                ("audio_enabled", MetricValue::U64(v)) => {
                                    audio_enabled = Some(*v != 0)
                                }
                                ("is_speaking", MetricValue::U64(v)) => is_speaking = Some(*v != 0),
                                _ => {}
                            }
                        }

                        let mut updated = false;

                        if let (Some(peer), Some(audio)) = (to_peer.as_ref(), audio_enabled) {
                            let current = self.peer_audio_states.get(peer).copied();
                            if current != Some(audio) {
                                self.peer_audio_states.insert(peer.clone(), audio);
                                updated = true;
                            }
                        }

                        if let (Some(peer), Some(speaking)) = (to_peer, is_speaking) {
                            let current = self.peer_speaking_states.get(&peer).copied();
                            if current != Some(speaking) {
                                self.peer_speaking_states.insert(peer, speaking);
                                updated = true;
                            }
                        }

                        updated
                    }
                    "peer_speaking" => {
                        // Fast-path speaking updates from decoded audio frames
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
                            let current = self.peer_speaking_states.get(&peer).copied();
                            if current != Some(speaking_val) {
                                self.peer_speaking_states.insert(peer, speaking_val);
                                return true;
                            }
                        }
                        false
                    }
                    _ => false,
                }
            }
        }
    }

    fn changed(&mut self, ctx: &Context<Self>, _old_props: &Self::Properties) -> bool {
        // Merge new peer audio states from props (for newly joined peers)
        for (peer, audio) in &ctx.props().peer_audio_states {
            if !self.peer_audio_states.contains_key(peer) {
                self.peer_audio_states.insert(peer.clone(), *audio);
            }
        }
        // Update local speaking state
        self.local_speaking = ctx.props().self_speaking;
        true
    }

    fn destroy(&mut self, _ctx: &Context<Self>) {
        if let Some(handle) = self.abort_handle.take() {
            handle.abort();
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        // Get VideoCallClient from context to convert session_id to email for display
        let client_ctx = ctx
            .link()
            .context::<VideoCallClientCtx>(Callback::noop())
            .map(|(client, _)| client);

        let filtered_peers: Vec<_> = ctx
            .props()
            .peers
            .iter()
            .filter(|peer| {
                // Resolve session_id to display name for search filtering
                let display_name = if let Some(ref client) = client_ctx {
                    client
                        .get_peer_user_id(peer)
                        .unwrap_or_else(|| (*peer).clone())
                } else {
                    (*peer).clone()
                };
                display_name
                    .to_lowercase()
                    .contains(&self.search_query.to_lowercase())
            })
            .cloned()
            .collect();

        let search_peers = ctx.link().callback(|e: InputEvent| {
            let input: HtmlInputElement = e.target_unchecked_into();
            PeerListMsg::UpdateSearchQuery(input.value())
        });

        let toggle_context_menu = ctx.link().callback(|e: MouseEvent| {
            e.stop_propagation();
            PeerListMsg::ToggleContextMenu
        });

        // Get username from context and append (You)
        let current_user_name: Option<String> = ctx
            .link()
            .context::<DisplayNameCtx>(Callback::noop())
            .and_then(|(state, _handle)| state.as_ref().cloned());

        let display_name = current_user_name
            .clone()
            .map(|name| format!("{name} (You)"))
            .unwrap_or_else(|| "(You)".to_string());

        // Check if current user is host by comparing display names
        let host_display_name = ctx.props().host_display_name.clone();
        let is_current_user_host = host_display_name
            .as_ref()
            .map(|h| current_user_name.as_ref().map(|c| h == c).unwrap_or(false))
            .unwrap_or(false);

        html! {
            <div>

                {
                    // Show meeting information at the top when enabled
                    // MeetingInfo reads timing from MeetingTimeContext directly
                    if ctx.props().show_meeting_info {
                        html! {
                            <MeetingInfo
                                is_open={true}
                                onclose={ctx.props().on_toggle_meeting_info.clone()}
                                room_id={ctx.props().room_id.clone()}
                                num_participants={ctx.props().num_participants}
                                is_active={ctx.props().is_active}
                            />
                        }
                    } else {
                        html! {}
                    }
                }

                <div class="sidebar-header">
                    <h2>{ "Attendants" }</h2>

                    <div class="header-actions">
                        <button
                            class="menu-button"
                            onclick={toggle_context_menu}
                            aria-label="More options">
                            <svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                <circle cx="12" cy="12" r="1"></circle>
                                <circle cx="12" cy="5" r="1"></circle>
                                <circle cx="12" cy="19" r="1"></circle>
                            </svg>
                        </button>
                        <button class="close-button" onclick={ctx.props().onclose.clone()}>{"×"}</button>

                        {
                            if self.show_context_menu {
                                html! {
                                    <div class="context-menu">
                                        <button
                                            class="context-menu-item"
                                            onclick={ctx.props().on_toggle_meeting_info.reform(|_| ())}>
                                            <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                <circle cx="12" cy="12" r="10"></circle>
                                                <line x1="12" y1="16" x2="12" y2="12"></line>
                                                <line x1="12" y1="8" x2="12.01" y2="8"></line>
                                            </svg>
                                            {if ctx.props().show_meeting_info { "Hide Meeting Info" } else { "Show Meeting Info" }}
                                        </button>

                                    </div>
                                }
                            } else {
                                html! {}
                            }
                        }
                    </div>
                </div>


                // Sidebar content
                <div class="sidebar-content">
                    <div class="search-container">
                        <input
                            type="text"
                            placeholder="Search attendants..."
                            value={self.search_query.clone()}
                            oninput={search_peers}
                            class="search-input"
                        />
                    </div>


                    <div class="attendants-section">
                        <h3>{ "In call" }</h3>
                        <div class="peer-list">
                            <ul>
                                // show self as the first item with actual username
                                <li><PeerListItem name={display_name.clone()} is_host={is_current_user_host} muted={ctx.props().self_muted} speaking={self.local_speaking} /></li>

                                { for filtered_peers.iter().map(|peer_id| {
                                    // peer_id is session_id, get email for display
                                    let display_name = if let Some(ref client) = client_ctx {
                                        client.get_peer_user_id(peer_id).unwrap_or_else(|| peer_id.clone())
                                    } else {
                                        peer_id.clone()
                                    };

                                    let is_peer_host = host_display_name.as_ref()
                                        .map(|h| h == &display_name)
                                        .unwrap_or(false);
                                    let muted = !self.peer_audio_states.get(peer_id).copied().unwrap_or(false);
                                    let speaking = self.peer_speaking_states.get(peer_id).copied().unwrap_or(false);
                                    html!{
                                        <li><PeerListItem name={display_name} is_host={is_peer_host} muted={muted} speaking={speaking} /></li>
                                    }
                                })}
                            </ul>
                        </div>
                    </div>
                </div>
            </div>
        }
    }
}

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
use crate::context::UsernameCtx;
use web_sys::HtmlInputElement;
use yew::prelude::*;
use yew::{html, Component, Context};

pub struct PeerList {
    search_query: String,
    show_context_menu: bool,
}

#[derive(Properties, Clone, PartialEq)]
pub struct PeerListProperties {
    pub peers: Vec<String>,
    pub onclose: yew::Callback<yew::MouseEvent>,

    // meeting info
    pub show_meeting_info: bool,
    pub room_id: String,
    pub num_participants: usize,
    pub meeting_duration: String,
    pub user_meeting_duration: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub is_active: bool,
    pub on_toggle_meeting_info: yew::Callback<()>,
}

pub enum PeerListMsg {
    UpdateSearchQuery(String),
    ToggleContextMenu,
}

impl Component for PeerList {
    type Message = PeerListMsg;

    type Properties = PeerListProperties;

    fn create(_ctx: &Context<Self>) -> Self {
        PeerList {
            search_query: String::new(),
            show_context_menu: false,
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
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let filtered_peers: Vec<_> = ctx
            .props()
            .peers
            .iter()
            .filter(|peer| {
                peer.to_lowercase()
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
        let display_name: String = ctx
            .link()
            .context::<UsernameCtx>(Callback::noop())
            .and_then(|(state, _handle)| state.as_ref().cloned())
            .map(|name| format!("{name} (You)"))
            .unwrap_or_else(|| "(You)".to_string());

        html! {
            <div>

                {
                    // Show meeting information at the top when enabled
                    if ctx.props().show_meeting_info {
                        html! {
                            <MeetingInfo
                                is_open={true}
                                onclose={ctx.props().on_toggle_meeting_info.clone()}
                                room_id={ctx.props().room_id.clone()}
                                num_participants={ctx.props().num_participants}
                                meeting_duration={ctx.props().meeting_duration.clone()}
                                user_meeting_duration={ctx.props().user_meeting_duration.clone()}
                                started_at={ctx.props().started_at.clone()}
                                ended_at={ctx.props().ended_at.clone()}
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
                                <li><PeerListItem name={display_name.clone()} /></li>

                                { for filtered_peers.iter().map(|peer|
                                    html!{
                                        <li><PeerListItem name={peer.clone()}/></li>
                                    })
                                }
                            </ul>
                        </div>
                    </div>
                </div>
            </div>
        }
    }
}

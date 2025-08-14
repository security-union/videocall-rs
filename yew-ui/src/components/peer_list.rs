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

use crate::components::peer_list_item::PeerListItem;
use crate::context::UsernameCtx;
use web_sys::HtmlInputElement;
use yew::prelude::*;
use yew::{html, Component, Context};

pub struct PeerList {
    search_query: String,
}

#[derive(Properties, Clone, PartialEq)]
pub struct PeerListProperties {
    pub peers: Vec<String>,
    pub onclose: yew::Callback<yew::MouseEvent>,
}

pub enum PeerListMsg {
    UpdateSearchQuery(String),
}

impl Component for PeerList {
    type Message = PeerListMsg;

    type Properties = PeerListProperties;

    fn create(_ctx: &Context<Self>) -> Self {
        PeerList {
            search_query: String::new(),
        }
    }

    fn update(&mut self, _ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            PeerListMsg::UpdateSearchQuery(query) => {
                self.search_query = query;
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

        // Get username from context and append (SELF)
        let display_name: String = format!(
            "{} (You)",
            ctx.link()
                .context::<UsernameCtx>(Callback::noop())
                .and_then(|(state, _handle)| state.as_ref().cloned())
                .unwrap_or_else(|| "(You)".to_string())
        );

        html! {
            <>
                <div class="sidebar-header">
                    <h2>{ "Attendants" }</h2>
                    <button class="close-button" onclick={ctx.props().onclose.clone()}>{"×"}</button>
                </div>
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
            </>
        }
    }
}

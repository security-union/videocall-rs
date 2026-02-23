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

use crate::components::icons::peer::PeerIcon;
use yew::prelude::*;
use yew::{html, Component, Html};

pub struct PeerListItem {}

#[derive(Properties, Clone, PartialEq)]
pub struct PeerListItemProps {
    pub name: String,
    #[prop_or_default]
    pub is_host: bool,
    #[prop_or_default]
    pub is_you: bool,
}

impl Component for PeerListItem {
    type Message = ();

    type Properties = PeerListItemProps;

    fn create(_ctx: &Context<Self>) -> Self {
        Self {}
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let name = ctx.props().name.clone();
        let is_host = ctx.props().is_host;
        let is_you = ctx.props().is_you;
        let title = if is_host {
            format!("Host: {name}")
        } else {
            name.clone()
        };

        let indicator = match (is_you, is_host) {
            (true, true) => Some("(You/Host)"),
            (true, false) => Some("(You)"),
            (false, true) => Some("(Host)"),
            (false, false) => None,
        };

        html! {
            <div class="peer_item" title={title}>
                <div class="peer_item_icon">
                    <PeerIcon />
                </div>
                <div class="peer_item_text">
                    {name}
                    if let Some(label) = indicator {
                        <span class="host-indicator" style="color: #888; font-size: 0.85em; margin-left: 4px;">
                            {label}
                        </span>
                    }
                </div>
            </div>
        }
    }
}

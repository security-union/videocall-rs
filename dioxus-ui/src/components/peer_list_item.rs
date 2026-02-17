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

use dioxus::prelude::*;

use crate::components::icons::crown::CrownIcon;
use crate::components::icons::peer::PeerIcon;

#[derive(Props, Clone, PartialEq)]
pub struct PeerListItemProps {
    pub name: String,
    #[props(default = false)]
    pub is_host: bool,
}

#[component]
pub fn PeerListItem(props: PeerListItemProps) -> Element {
    let name = props.name.clone();
    let is_host = props.is_host;
    let title = if is_host {
        format!("Host: {name}")
    } else {
        name.clone()
    };

    rsx! {
        div { class: "peer_item", title: "{title}",
            div { class: "peer_item_icon",
                PeerIcon {}
            }
            div { class: "peer_item_text",
                "{name}"
                if is_host {
                    CrownIcon {}
                }
            }
        }
    }
}

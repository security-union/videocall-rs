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

use crate::components::icons::mic::MicIcon;
use crate::components::icons::peer::PeerIcon;
use dioxus::prelude::*;

#[component]
pub fn PeerListItem(
    name: String,
    #[props(default)] is_host: bool,
    #[props(default)] is_self: bool,
    #[props(default = true)] muted: bool,
    #[props(default = false)] speaking: bool,
) -> Element {
    let title = if is_host {
        format!("Host: {name}")
    } else {
        name.clone()
    };

    let mic_class = if speaking {
        "peer_item_mic speaking"
    } else {
        "peer_item_mic"
    };

    let indicator = match (is_self, is_host) {
        (true, true) => Some("(You/Host)"),
        (true, false) => Some("(You)"),
        (false, true) => Some("(Host)"),
        (false, false) => None,
    };

    rsx! {
        div { class: "peer_item", title,
            div { class: "peer_item_icon",
                PeerIcon {}
            }
            div { class: "peer_item_text",
                "{name}"
                if let Some(label) = indicator {
                    span { class: "peer-indicator", "{label}" }
                }
            }
            div { class: "{mic_class}",
                MicIcon { muted: muted }
            }
        }
    }
}

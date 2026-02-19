// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

#[component]
pub fn CrownIcon() -> Element {
    rsx! {
        span {
            class: "host-indicator",
            style: "color: #888; font-size: 0.85em; margin-left: 4px;",
            "(Host)"
        }
    }
}

// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

#[component]
pub fn CrownIcon() -> Element {
    rsx! {
        span {
            class: "host-indicator",
            // @token-exempt: subtle host indicator, #888 has no matching token
            style: "color: #888; font-size: 0.85em; margin-left: var(--space-1);",
            "(Host)"
        }
    }
}

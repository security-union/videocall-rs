// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

#[component]
pub fn HeroOrbs() -> Element {
    rsx! {
        div { class: "hero-orbs", aria_hidden: "true",
            div { class: "floating-element floating-element-1" }
            div { class: "floating-element floating-element-2" }
            div { class: "floating-element floating-element-3" }
        }
    }
}

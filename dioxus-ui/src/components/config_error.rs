/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use dioxus::prelude::*;

#[component]
pub fn ConfigError(message: String) -> Element {
    rsx! {
        div { class: "error-container",
            p { class: "error-message", "{message}" }
            p {
                "See setup and configuration docs: "
                a {
                    href: "https://github.com/security-union/videocall-rs",
                    target: "_blank",
                    rel: "noopener noreferrer",
                    "security-union/videocall-rs"
                }
            }
            img {
                src: "/assets/street_fighter.gif",
                alt: "Permission instructions",
                class: "instructions-gif",
            }
        }
    }
}

/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use dioxus::prelude::*;
use videocall_theme::Card;

#[component]
pub fn ConfigError(message: String) -> Element {
    rsx! {
        div { class: "error-container screen-center surface-grouped",
            Card { class: "status-card",
            p { class: "text-headline", "{message}" }
            p { class: "text-footnote text-secondary",
                "See setup and configuration docs: "
                a {
                    class: "link",
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
}

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

use crate::components::icons::{
    digital_ocean::DigitalOceanIcon, discord::DiscordIcon, youtube::YoutubeIcon,
};
use dioxus::prelude::*;
use videocall_theme::{Bar, Toolbar};

#[component]
pub fn TopBar() -> Element {
    // A toolbar row over the call: the links are one Clear glass group and
    // the credit line is its own, since text and icons do not share a
    // background. Layout is in styles/features/call.css.
    rsx! {
        Toolbar {
            class: "call-top-bar",
            Bar {
                clear: true,
                class: "call-top-bar-links",
                a {
                    href: "https://github.com/security-union/videocall-rs",
                    class: "call-top-bar-link",
                    target: "_blank",
                    rel: "noopener noreferrer",
                    img {
                        src: "https://img.shields.io/github/stars/security-union/videocall-rs?style=social",
                        class: "call-top-bar-badge",
                        alt: "GitHub stars",
                    }
                }
                a {
                    href: "https://www.youtube.com/@SecurityUnion",
                    class: "call-top-bar-link",
                    target: "_blank",
                    rel: "noopener noreferrer",
                    "aria-label": "YouTube",
                    YoutubeIcon {}
                }
                a {
                    href: "https://discord.gg/JP38NRe4CJ",
                    class: "call-top-bar-link",
                    target: "_blank",
                    rel: "noopener noreferrer",
                    "aria-label": "Discord",
                    DiscordIcon {}
                }
                a {
                    href: "https://m.do.co/c/6de4e19c5193",
                    class: "call-top-bar-link call-top-bar-link-wide",
                    target: "_blank",
                    rel: "noopener noreferrer",
                    "aria-label": "DigitalOcean",
                    DigitalOceanIcon {}
                }
            }
            // Raw: a span, where Bar renders a div.
            span {
                class: "bar bar-clear call-top-bar-credit text-caption-1",
                "Made with \u{2764}\u{FE0F} by awesome developers from all over the world \u{1F30F}, hosted by Security Union \u{1F6E1}\u{FE0F}."
            }
        }
    }
}

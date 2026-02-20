// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

#[component]
pub fn MicIcon(#[props(default = false)] muted: bool) -> Element {
    if muted {
        rsx! {
            svg {
                xmlns: "http://www.w3.org/2000/svg",
                view_box: "0 0 24 24",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "2",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                line { x1: "1", y1: "1", x2: "23", y2: "23" }
                path { d: "M9 9v3a3 3 0 0 0 5.12 2.12M15 9.34V5a3 3 0 0 0-5.94-.6" }
                path { d: "M17 16.95A7 7 0 0 1 5 12v-2m14 0v2a7 7 0 0 1-.11 1.23" }
                line { x1: "12", y1: "19", x2: "12", y2: "22" }
            }
        }
    } else {
        rsx! {
            svg {
                xmlns: "http://www.w3.org/2000/svg",
                view_box: "0 0 24 24",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "2",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                path { d: "M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3z" }
                path { d: "M19 10v2a7 7 0 0 1-14 0v-2" }
                line { x1: "12", y1: "19", x2: "12", y2: "22" }
            }
        }
    }
}

// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

/// Crop glyph. Takes `currentColor`; the parent button sizes it.
#[component]
pub fn CropIcon() -> Element {
    rsx! {
        svg {
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            path { d: "M6 3v12a3 3 0 003 3h12" }
            path { d: "M18 21V9a3 3 0 00-3-3H3" }
        }
    }
}

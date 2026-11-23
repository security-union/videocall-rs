// SPDX-License-Identifier: MIT OR Apache-2.0

//! Every icon in `videocall_theme::icons`, by name. dioxus-ui draws the same
//! glyphs with the same `Icon` component.

use dioxus::prelude::*;
use videocall_theme::*;

use super::{Cell, Section};

#[component]
pub fn Icons() -> Element {
    rsx! {
        Section {
            id: "icons",
            title: "Icons",
            lede: "One set, owned by the theme and drawn by one Icon component, in the current text color. The app and this page use the same glyphs, so a control can't show one picture here and another in a call. Outlined at a stroke of 2 unless a glyph is solid. Brand marks are not icons and stay in the app.",
            div { class: "gallery-cells",
                for glyph in icons::ALL.iter().copied() {
                    Cell { key: "{glyph.name}", caption: glyph.name,
                        Icon { glyph, size: "24", "aria-hidden": "true" }
                    }
                }
            }
        }
    }
}

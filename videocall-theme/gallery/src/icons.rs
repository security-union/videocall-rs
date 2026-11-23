// SPDX-License-Identifier: MIT OR Apache-2.0

//! Line icons for the gallery, drawn in the current text color so they
//! follow every appearance. Simple shapes on a 24-unit grid.

use dioxus::prelude::*;

/// Draws the given path data as a 24x24 stroked icon. Decorative: the
/// control around it carries the accessible name.
#[component]
pub fn Icon(paths: &'static [&'static str]) -> Element {
    rsx! {
        svg {
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            for d in paths {
                path { d: *d }
            }
        }
    }
}

pub const CLOSE: &[&str] = &["M6 6l12 12", "M18 6L6 18"];
pub const CHECK: &[&str] = &["M5 12.5l4.5 4.5L19 7.5"];
pub const PLUS: &[&str] = &["M12 5v14", "M5 12h14"];
pub const CHEVRON_RIGHT: &[&str] = &["M9 6l6 6-6 6"];
pub const BACK: &[&str] = &["M15 6l-6 6 6 6"];
pub const MORE: &[&str] = &["M5 12h.01", "M12 12h.01", "M19 12h.01"];
pub const ACCESSIBILITY: &[&str] = &[
    "M12 6.5a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3z",
    "M5 9l7 1 7-1",
    "M12 10v4",
    "M9 20l3-6 3 6",
];
pub const SHARE: &[&str] = &["M12 15V4", "M8 8l4-4 4 4", "M5 13v6h14v-6"];
pub const TRASH: &[&str] = &["M4 7h16", "M9 7V4h6v3", "M6 7l1 13h10l1-13"];
pub const PEOPLE: &[&str] = &[
    "M9 11a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7z",
    "M2.5 20a6.5 6.5 0 0 1 13 0",
    "M16 4.5a3.5 3.5 0 0 1 0 6.5",
    "M18.5 14a6.5 6.5 0 0 1 3 6",
];
pub const CHAT: &[&str] = &["M4 5h16v11H9l-5 4z"];
pub const MIC: &[&str] = &[
    "M12 3a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V6a3 3 0 0 0-3-3z",
    "M19 11a7 7 0 0 1-14 0",
    "M12 18v3",
];
pub const MIC_OFF: &[&str] = &[
    "M12 3a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V6a3 3 0 0 0-3-3z",
    "M19 11a7 7 0 0 1-14 0",
    "M12 18v3",
    "M4 4l16 16",
];
pub const CAMERA: &[&str] = &["M3 7h12v10H3z", "M15 10.5l6-3.5v10l-6-3.5"];
pub const CAMERA_OFF: &[&str] = &["M3 7h12v10H3z", "M15 10.5l6-3.5v10l-6-3.5", "M2 4l18 16"];
pub const SCREEN: &[&str] = &["M3 5h18v11H3z", "M8 20h8", "M12 16v4"];
pub const SETTINGS: &[&str] = &[
    "M4 7h9", "M17 7h3", "M15 5v4", "M4 17h3", "M11 17h9", "M9 15v4",
];
pub const HANG_UP: &[&str] =
    &["M3 14.5c5-5 13-5 18 0l-2.5 2.5-3.5-1.5v-2.5a9 9 0 0 0-6 0v2.5L5.5 17z"];

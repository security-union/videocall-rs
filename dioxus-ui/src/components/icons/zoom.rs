// SPDX-License-Identifier: MIT OR Apache-2.0

//! Issue 1175: icons for the received-shared-content zoom / detach controls.
//! Stroke-based, 24x24 viewBox, `currentColor` — matching the existing tile
//! overlay icons (e.g. `crop.rs`) so they inherit the control-bar color.

use dioxus::prelude::*;

/// Magnifying glass with a `+` — zoom in.
#[component]
pub fn ZoomInIcon() -> Element {
    rsx! {
        svg {
            class: "w-8",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            circle { cx: "11", cy: "11", r: "7" }
            path { d: "M21 21l-4.35-4.35" }
            path { d: "M11 8v6" }
            path { d: "M8 11h6" }
        }
    }
}

/// Magnifying glass with a `−` — zoom out.
#[component]
pub fn ZoomOutIcon() -> Element {
    rsx! {
        svg {
            class: "w-8",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            circle { cx: "11", cy: "11", r: "7" }
            path { d: "M21 21l-4.35-4.35" }
            path { d: "M8 11h6" }
        }
    }
}

/// Circular arrow — reset zoom to 100% (fit).
#[component]
pub fn ZoomResetIcon() -> Element {
    rsx! {
        svg {
            class: "w-8",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            path { d: "M3 12a9 9 0 1 0 3-6.7" }
            path { d: "M3 4v4h4" }
        }
    }
}

/// Issue 1821: actual-size (1:1) — a rounded rectangle framing a centered "1:1"
/// glyph. The frame is stroked like its siblings; the "1:1" is FILLED with
/// `currentColor` (a stroked glyph this small reads as a smudge), so it inherits
/// the control-bar color the same way.
#[component]
pub fn ActualSizeIcon() -> Element {
    rsx! {
        svg {
            class: "w-8",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            rect { x: "3", y: "5", width: "18", height: "14", rx: "2" }
            text {
                x: "12",
                y: "12",
                fill: "currentColor",
                stroke: "none",
                font_size: "9",
                font_weight: "700",
                text_anchor: "middle",
                dominant_baseline: "central",
                font_family: "system-ui, sans-serif",
                "1:1"
            }
        }
    }
}

/// Box with an outward arrow — open shared content in a separate window. Reused
/// (with a rotated meaning) for the reattach affordance in the placeholder.
#[component]
pub fn DetachIcon() -> Element {
    rsx! {
        svg {
            class: "w-8",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            path { d: "M15 3h6v6" }
            path { d: "M10 14 21 3" }
            path { d: "M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" }
        }
    }
}

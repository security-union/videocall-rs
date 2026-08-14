// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

/// The raised-hand glyph (issue 2135), used by the action-bar toggle, the video
/// tile badge, the roster row badge, and the persistent banner.
///
/// An inline SVG rather than the ✋ emoji: the same three surfaces already mix a
/// stroked-icon vocabulary (crown, mic, signal bars), an emoji would inherit an
/// unpredictable per-platform colour that no theme token can tint, and — unlike
/// `RecordingIcon`'s red dot, whose whole meaning IS the colour — this glyph must
/// read as `currentColor` so it can sit on a dark tile overlay, a light roster
/// row, and an accent-filled pressed button without three separate treatments.
///
/// `decorative` picks the a11y treatment, which differs by surface and is easy to
/// get wrong in both directions:
///   * `true` — the icon sits INSIDE a control or element that already carries
///     its own accessible name (the action-bar button, the banner). The name
///     would then be announced twice, so the `<svg>` is `aria-hidden`.
///   * `false` — the icon IS the only representation of the state (the tile and
///     roster badges), so it needs `role="img"` plus a `label`. `role="img"` is
///     required for the label to be reliably surfaced; a bare labelled `<svg>` is
///     announced inconsistently across screen readers.
#[component]
pub fn RaisedHandIcon(
    #[props(default = true)] decorative: bool,
    #[props(default)] label: String,
    /// Extra classes for surface-specific sizing/positioning.
    #[props(default)]
    class: String,
) -> Element {
    let effective_label = if label.is_empty() {
        "Hand raised".to_string()
    } else {
        label
    };
    rsx! {
        svg {
            class: if class.is_empty() { None } else { Some(class) },
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            role: if decorative { None } else { Some("img") },
            "aria-hidden": if decorative { Some("true") } else { None },
            "aria-label": if decorative { None } else { Some(effective_label.clone()) },
            // `<title>` FIRST. SVG 1.1 requires the title to be the first child
            // of its parent for it to be treated as that element's name, and
            // some AT/browser pairs skip a title that appears after the drawing
            // content — which would silently drop the only label this glyph has
            // on the badge surfaces.
            if !decorative {
                title { "{effective_label}" }
            }
            // An open palm with the index finger extended upward — the
            // conventional "raise hand" mark in conferencing UIs.
            path { d: "M18 11V6a2 2 0 0 0-2-2a2 2 0 0 0-2 2" }
            path { d: "M14 10V4a2 2 0 0 0-2-2a2 2 0 0 0-2 2v2" }
            path { d: "M10 10.5V6a2 2 0 0 0-2-2a2 2 0 0 0-2 2v8" }
            path { d: "M18 8a2 2 0 1 1 4 0v6a8 8 0 0 1-8 8h-2c-2.8 0-4.5-.86-5.99-2.34l-3.6-3.6a2 2 0 0 1 2.83-2.82L7 15" }
        }
    }
}

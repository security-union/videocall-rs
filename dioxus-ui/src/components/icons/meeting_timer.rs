// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

/// The meeting-timer glyph (issue 2136): a stopwatch, used by the host's
/// action-bar control and its popover.
///
/// An inline SVG rather than the ⏱ emoji, for the same reason `RaisedHandIcon`
/// gives: the action bar already speaks a stroked-icon vocabulary, and an emoji
/// carries a per-platform colour that no theme token can tint. This glyph must
/// render as `currentColor` so it reads correctly both at rest and on the
/// accent-filled pressed state of its button.
///
/// A STOPWATCH, deliberately, not an hourglass or a bare clock. A clock face is
/// the app's "meeting time" (elapsed) idiom already — `CallTimer` in the meeting
/// info panel — and reusing it here would conflate elapsed time with the host's
/// countdown. The stopwatch crown and side button read as "a timer someone
/// started", which is exactly what this is.
///
/// `decorative` picks the a11y treatment, matching the convention established by
/// `RaisedHandIcon`:
///   * `true` — the icon sits inside a control that already has an accessible
///     name, so the `<svg>` is `aria-hidden` and the name is not announced twice.
///   * `false` — the icon is the only representation of the thing, so it takes
///     `role="img"` plus a label. `role="img"` is required for the label to be
///     surfaced reliably; a bare labelled `<svg>` is announced inconsistently.
#[component]
pub fn MeetingTimerIcon(
    #[props(default = true)] decorative: bool,
    #[props(default)] label: String,
    /// Extra classes for surface-specific sizing/positioning.
    #[props(default)]
    class: String,
) -> Element {
    let effective_label = if label.is_empty() {
        "Meeting timer".to_string()
    } else {
        label
    };
    rsx! {
        svg {
            class: if class.is_empty() { None } else { Some(class) },
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            width: "18",
            height: "18",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            role: if decorative { None } else { Some("img") },
            "aria-hidden": if decorative { Some("true") } else { None },
            "aria-label": if decorative { None } else { Some(effective_label.clone()) },
            // SVG 1.1 requires <title> to be the FIRST child: some AT/browser
            // pairs skip a title that appears after the drawing content.
            if !decorative {
                title { "{effective_label}" }
            }
            // Stopwatch crown + side button, then the dial and the hand.
            path { d: "M10 2h4" }
            path { d: "M12 2v2" }
            path { d: "M19.5 5.5 18 7" }
            circle { cx: "12", cy: "14", r: "8" }
            path { d: "M12 11v3l2 2" }
        }
    }
}

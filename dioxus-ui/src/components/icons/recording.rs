// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

/// Indicator shown next to a participant's name (in their video tile and in the
/// peer list) while that participant is recording the meeting. One is rendered
/// per active recorder, driven by [`crate::context::RecordingSetCtx`].
#[component]
pub fn RecordingIcon() -> Element {
    rsx! {
        span {
            class: "recording-indicator",
            // `role="img"` makes `aria-label` authoritative for screen
            // readers; on a default generic-role span the label is
            // unreliably surfaced (some readers announce the emoji's own
            // Unicode name, "large red circle", or nothing instead).
            role: "img",
            "aria-label": "Recording",
            title: "Recording",
            "🔴"
        }
    }
}

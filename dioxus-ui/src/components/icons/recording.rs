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
            "aria-label": "Recording",
            title: "Recording",
            "🔴"
        }
    }
}

// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

/// The "(Host)" label next to a participant's name. It is a trust indicator:
/// callers render it only when the peer's authenticated user id matches the
/// meeting host. Styling (a `.badge`) is in `styles/features/call.css`.
#[component]
pub fn CrownIcon() -> Element {
    rsx! {
        span { class: "host-indicator badge", "(Host)" }
    }
}

/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 */

//! Full-screen overlay shown when a meeting has ended.
//!
//! Renders a centered card with an X icon, a configurable message,
//! and a "Return to Home" button that navigates to `/`.

use dioxus::prelude::*;
use videocall_theme::{Alert, AlertActions, AlertMessage, Button, ButtonVariant, Scrim};

/// A scrim + alert that tells the user the meeting has ended and offers a
/// button to return to the home page.
///
/// `glass-backdrop`, `meeting-ended-overlay`, `meeting-ended-message` and
/// `meeting-ended-home-btn` are kept as hooks: tests select them.
#[component]
pub fn MeetingEndedOverlay(
    /// The message to display (e.g. "The host has ended the meeting.").
    message: String,
) -> Element {
    rsx! {
        Scrim {
            class: "glass-backdrop meeting-ended-overlay",
            // No dialog role today; `None` keeps it that way.
            Alert {
                role: None::<&str>,
                class: "meeting-ended-alert",
                svg {
                    class: "meeting-ended-icon",
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    "aria-hidden": "true",
                    circle { cx: "12", cy: "12", r: "10" }
                    line { x1: "15", y1: "9", x2: "9", y2: "15" }
                    line { x1: "9", y1: "9", x2: "15", y2: "15" }
                }
                h4 { class: "alert-title", "Meeting Ended" }
                AlertMessage { class: "meeting-ended-message", "{message}" }
                AlertActions {
                    Button {
                        variant: ButtonVariant::Prominent,
                        class: "meeting-ended-home-btn",
                        onclick: move |_| {
                            if let Some(window) = web_sys::window() {
                                let _ = window.location().set_href("/");
                            }
                        },
                        "Return to Home"
                    }
                }
            }
        }
    }
}

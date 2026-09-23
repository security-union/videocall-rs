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

use crate::components::meeting_footer::trap_tab_in_dialog;
use dioxus::prelude::*;

const MEETING_ENDED_CARD_ID: &str = "meeting-ended-card";

/// A glass-backdrop overlay that tells the user the meeting has ended
/// and offers a button to return to the home page.
#[component]
pub fn MeetingEndedOverlay(
    /// The message to display (e.g. "The host has ended the meeting.").
    message: String,
) -> Element {
    rsx! {
        div {
            class: "glass-backdrop meeting-ended-overlay",
            style: "z-index: 9999;",
            onkeydown: move |e: Event<KeyboardData>| e.stop_propagation(),
            div {
                id: MEETING_ENDED_CARD_ID,
                class: "card-apple meeting-ended-card",
                style: "width: 420px; text-align: center;",
                role: "alertdialog",
                "aria-modal": "true",
                "aria-labelledby": "meeting-ended-title",
                "aria-describedby": "meeting-ended-message",
                tabindex: "-1",
                onkeydown: move |e: Event<KeyboardData>| {
                    if e.key() == Key::Tab
                        && trap_tab_in_dialog(MEETING_ENDED_CARD_ID, e.modifiers().shift())
                    {
                        e.prevent_default();
                    }
                },
                onmounted: move |element| {
                    let element = element.data();
                    spawn(async move {
                        let _ = element.set_focus(true).await;
                    });
                },
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    width: "64",
                    height: "64",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "#ff6b6b",
                    stroke_width: "2",
                    style: "margin: 0 auto var(--space-4);",
                    circle { cx: "12", cy: "12", r: "10" }
                    line { x1: "15", y1: "9", x2: "9", y2: "15" }
                    line { x1: "9", y1: "9", x2: "15", y2: "15" }
                }
                h4 {
                    id: "meeting-ended-title",
                    style: "margin-top:0; margin-bottom: var(--space-2);",
                    "Meeting Ended"
                }
                p {
                    id: "meeting-ended-message",
                    class: "meeting-ended-message",
                    style: "font-size: var(--fs-7); margin: 1.5rem 0; color: var(--text-secondary);",
                    "{message}"
                }
                button {
                    class: "btn-apple btn-primary meeting-ended-home-btn",
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

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

use yew::prelude::*;

#[derive(Properties, Debug, PartialEq)]
pub struct MeetingEndedOverlayProps {
    /// The message to display (e.g. "The host has ended the meeting.").
    pub message: String,
}

/// A glass-backdrop overlay that tells the user the meeting has ended
/// and offers a button to return to the home page.
#[function_component(MeetingEndedOverlay)]
pub fn meeting_ended_overlay(props: &MeetingEndedOverlayProps) -> Html {
    let on_return_home = Callback::from(|_: MouseEvent| {
        if let Some(window) = web_sys::window() {
            let _ = window.location().set_href("/");
        }
    });

    html! {
        <div class="glass-backdrop meeting-ended-overlay" style="z-index: 9999;">
            <div class="card-apple" style="width: 420px; text-align: center;">
                <svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"
                     viewBox="0 0 24 24" fill="none" stroke="#ff6b6b"
                     stroke-width="2" style="margin: 0 auto 1rem;">
                    <circle cx="12" cy="12" r="10"></circle>
                    <line x1="15" y1="9" x2="9" y2="15"></line>
                    <line x1="9" y1="9" x2="15" y2="15"></line>
                </svg>
                <h4 style="margin-top:0; margin-bottom: 0.5rem;">{"Meeting Ended"}</h4>
                <p class="meeting-ended-message"
                   style="font-size: 1rem; margin: 1.5rem 0; color: #666;">
                    { &props.message }
                </p>
                <button
                    class="btn-apple btn-primary meeting-ended-home-btn"
                    onclick={on_return_home}>
                    {"Return to Home"}
                </button>
            </div>
        </div>
    }
}

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

//! Okta-branded "Sign in with Okta" button with the Okta logo and matching
//! dark-theme styling consistent with the Google GSI button appearance.

use dioxus::prelude::*;

/// Okta-branded sign-in button — dark theme, styled to match the Google GSI
/// Material button dimensions and feel.
#[component]
pub fn OktaSignInButton(onclick: EventHandler<MouseEvent>) -> Element {
    rsx! {
        button {
            class: "okta-sign-in-button",
            onclick: move |evt| onclick.call(evt),
            div { class: "okta-sign-in-button-content",
                div { class: "okta-sign-in-button-icon",
                    svg {
                        width: "20",
                        height: "20",
                        view_box: "0 0 24 24",
                        xmlns: "http://www.w3.org/2000/svg",
                        style: "display: block;",
                        circle { cx: "12", cy: "12", r: "12", fill: "#007DC1" }
                        circle { cx: "12", cy: "12", r: "5", fill: "#FFFFFF" }
                    }
                }
                span { class: "okta-sign-in-button-label", "Sign in with Okta" }
            }
        }
    }
}

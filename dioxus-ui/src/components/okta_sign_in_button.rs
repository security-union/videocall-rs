/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Okta-branded "Sign in with Okta" button with the Okta logo and matching
//! dark-theme styling consistent with the Google GSI button appearance.

use dioxus::prelude::*;

#[component]
pub fn OktaSignInButton(onclick: EventHandler<MouseEvent>) -> Element {
    rsx! {
        button { class: "okta-sign-in-button", onclick: move |e| onclick.call(e),
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

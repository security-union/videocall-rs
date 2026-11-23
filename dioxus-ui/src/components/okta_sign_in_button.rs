/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Okta-branded "Sign in with Okta" button with the Okta logo and matching
//! dark-theme styling consistent with the Google GSI button appearance.

use dioxus::prelude::*;
use videocall_theme::{Button, ButtonSize};

#[component]
pub fn OktaSignInButton(onclick: EventHandler<MouseEvent>) -> Element {
    rsx! {
        Button {
            size: ButtonSize::Large,
            block: true,
            class: "okta-sign-in-button",
            onclick: move |e| onclick.call(e),
            span { class: "okta-sign-in-button-icon",
                svg {
                    view_box: "0 0 24 24",
                    xmlns: "http://www.w3.org/2000/svg",
                    "aria-hidden": "true",
                    circle { cx: "12", cy: "12", r: "12", fill: "#007DC1" }
                    circle { cx: "12", cy: "12", r: "5", fill: "#FFFFFF" }
                }
            }
            span { class: "okta-sign-in-button-label", "Sign in with Okta" }
        }
    }
}

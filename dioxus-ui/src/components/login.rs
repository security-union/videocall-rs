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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Login page component.
//!
//! Renders a provider-branded sign-in button based on the `oauthProvider`
//! value in `window.__APP_CONFIG`. Delegates to dedicated button components
//! for each supported provider (Google, Okta) so branding stays isolated.

use dioxus::prelude::*;
use gloo_utils::window;

use crate::components::google_sign_in_button::GoogleSignInButton;
use crate::components::okta_sign_in_button::OktaSignInButton;
use crate::constants::{login_url, oauth_provider};

/// Handle login button click - redirect to backend OAuth endpoint
fn handle_login() {
    match login_url() {
        Ok(mut url) => {
            if let Ok(search) = window().location().search() {
                if !search.is_empty() {
                    url = format!("{url}{search}");
                }
            }
            let _ = window().location().set_href(&url);
        }
        Err(e) => log::error!("Failed to get login URL: {e:?}"),
    }
}

#[component]
pub fn Login() -> Element {
    let provider = oauth_provider();

    rsx! {
        div { class: "login-container",
            div { class: "login-card",
                h1 { class: "login-title", "videocall.rs" }

                match provider.as_deref() {
                    Some("google") => rsx! {
                        GoogleSignInButton {
                            onclick: move |_| handle_login()
                        }
                    },
                    Some("okta") => rsx! {
                        OktaSignInButton {
                            onclick: move |_| handle_login()
                        }
                    },
                    _ => rsx! {
                        button {
                            class: "generic-sign-in-button",
                            onclick: move |_| handle_login(),
                            "Sign in"
                        }
                    }
                }

                p { class: "login-footer",
                    "By signing in, you agree to our "
                    a { href: "https://github.com/security-union/videocall-rs", "Terms of Service" }
                    " and "
                    a { href: "https://github.com/security-union/videocall-rs", "Privacy Policy" }
                }
            }
        }
    }
}

/// Build the login callback that redirects to the backend OAuth endpoint,
/// forwarding any `returnTo` query parameter from the current URL.
pub fn build_login_callback() -> impl Fn() {
    move || handle_login()
}

/// Render the sign-in button for the configured OAuth provider.
#[component]
pub fn ProviderButton(onclick: EventHandler<MouseEvent>) -> Element {
    match oauth_provider().as_deref() {
        Some("google") => rsx! {
            GoogleSignInButton { onclick: onclick }
        },
        Some("okta") => rsx! {
            OktaSignInButton { onclick: onclick }
        },
        _ => rsx! {
            button {
                class: "generic-sign-in-button",
                onclick: move |evt| onclick.call(evt),
                "Sign in"
            }
        },
    }
}

/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
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

/// Build the login callback that redirects to the backend OAuth endpoint,
/// forwarding any `returnTo` query parameter from the current URL.
pub fn do_login() {
    match login_url() {
        Ok(mut url) => {
            // Prefer sessionStorage — the meeting page stores the return URL there
            // before navigating to /login, because Dioxus 0.7's router strips
            // unrecognized query params via history.replaceState on boot.
            let stored_return_to = window().session_storage().ok().flatten().and_then(|s| {
                let val = s.get_item("vc_oauth_return_to").ok().flatten();
                if val.is_some() {
                    let _ = s.remove_item("vc_oauth_return_to");
                }
                val
            });

            if let Some(return_to) = stored_return_to {
                url = format!("{url}?returnTo={}", urlencoding::encode(&return_to));
            } else if let Ok(search) = window().location().search() {
                if !search.is_empty() {
                    // Fallback: returnTo still in query string (direct /login?returnTo= navigation).
                    url = format!("{url}{search}");
                } else if let Ok(origin) = window().location().origin() {
                    // No returnTo anywhere — tell the backend which frontend to return to.
                    let value = format!("{origin}/");
                    let encoded = urlencoding::encode(&value);
                    url = format!("{url}?returnTo={encoded}");
                }
            }
            let _ = window().location().set_href(&url);
        }
        Err(e) => log::error!("Failed to get login URL: {e:?}"),
    }
}

/// Render the sign-in button for the configured OAuth provider.
#[component]
pub fn ProviderButton(onclick: EventHandler<MouseEvent>) -> Element {
    match oauth_provider().as_deref() {
        Some("google") => rsx! { GoogleSignInButton { onclick } },
        Some("okta") => rsx! { OktaSignInButton { onclick } },
        _ => rsx! {
            button { class: "generic-sign-in-button", onclick: move |e| onclick.call(e),
                "Sign in"
            }
        },
    }
}

#[component]
pub fn Login() -> Element {
    rsx! {
        div { class: "login-container",
            div { class: "login-card",
                h1 { class: "login-title", "videocall.rs" }

                ProviderButton { onclick: move |_| do_login() }

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

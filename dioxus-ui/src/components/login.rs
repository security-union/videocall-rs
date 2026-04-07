/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Login page component and `do_login` helper.
//!
//! ## Behaviour when OAuth is configured (`oauthEnabled = true`)
//!
//! The `Login` component **immediately starts the PKCE OIDC flow** on mount
//! via a `use_effect`.  No button click is required.  A minimal loading screen
//! is rendered while the browser navigates to the provider.
//!
//! [`do_login`] is the thin public entry point for starting the flow from
//! other components (e.g. the home-page sign-in button, the meetings list
//! unauthenticated prompt).  It delegates to [`crate::auth::do_login`].
//!
//! ## Behaviour when OAuth is **not** configured
//!
//! Renders a generic or provider-branded sign-in button.

use dioxus::prelude::*;

use crate::components::google_sign_in_button::GoogleSignInButton;
use crate::components::okta_sign_in_button::OktaSignInButton;
use crate::constants::{oauth_enabled, oauth_provider};

/// Start the OAuth / PKCE login flow.
///
/// This is a thin re-export of [`crate::auth::do_login`] kept in this module
/// so callers that already import from `crate::components::login` don't need
/// a new import path.
pub fn do_login() {
    crate::auth::do_login();
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

/// The `/login` route component.
///
/// When `oauthEnabled` is `true` this component **immediately** starts the
/// PKCE OIDC flow on mount (via `use_effect`) so the user never sees an
/// intermediate button page.
///
/// When OAuth is not configured the traditional sign-in button UI is rendered.
#[component]
pub fn Login() -> Element {
    use_effect(move || {
        if oauth_enabled().unwrap_or(false) {
            do_login();
        }
    });

    if oauth_enabled().unwrap_or(false) {
        rsx! {
            div { class: "login-container",
                div { class: "login-card",
                    h1 { class: "login-title", "videocall.rs" }
                    div {
                        style: "display: flex; flex-direction: column; \
                                align-items: center; gap: 1rem; padding: 1rem 0;",
                        div {
                            style: "width: 36px; height: 36px; \
                                    border: 3px solid rgba(255,255,255,0.2); \
                                    border-top-color: #7928CA; border-radius: 50%; \
                                    animation: spin 0.8s linear infinite;",
                        }
                        p {
                            style: "color: rgba(255,255,255,0.65); \
                                    font-size: 0.95rem; margin: 0;",
                            "Redirecting to sign-in\u{2026}"
                        }
                        style {
                            "@keyframes spin {{ to {{ transform: rotate(360deg); }} }}"
                        }
                    }
                }
            }
        }
    } else {
        rsx! {
            div { class: "login-container",
                div { class: "login-card",
                    h1 { class: "login-title", "videocall.rs" }

                    ProviderButton { onclick: move |_| do_login() }

                    p { class: "login-footer",
                        "By signing in, you agree to our "
                        a {
                            href: "https://github.com/security-union/videocall-rs",
                            "Terms of Service"
                        }
                        " and "
                        a {
                            href: "https://github.com/security-union/videocall-rs",
                            "Privacy Policy"
                        }
                    }
                }
            }
        }
    }
}

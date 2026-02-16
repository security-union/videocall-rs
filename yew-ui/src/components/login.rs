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

use gloo_utils::window;
use yew::prelude::*;

use crate::components::google_sign_in_button::GoogleSignInButton;
use crate::components::okta_sign_in_button::OktaSignInButton;
use crate::constants::{login_url, oauth_provider};

/// Build the login callback that redirects to the backend OAuth endpoint,
/// forwarding any `returnTo` query parameter from the current URL.
fn build_login_callback() -> Callback<MouseEvent> {
    Callback::from(|_: MouseEvent| match login_url() {
        Ok(mut url) => {
            if let Ok(search) = window().location().search() {
                if !search.is_empty() {
                    url = format!("{url}{search}");
                }
            }
            let _ = window().location().set_href(&url);
        }
        Err(e) => log::error!("Failed to get login URL: {e:?}"),
    })
}

/// Render the sign-in button for the configured OAuth provider.
fn render_provider_button(onclick: Callback<MouseEvent>) -> Html {
    match oauth_provider().as_deref() {
        Some("google") => html! { <GoogleSignInButton {onclick} /> },
        Some("okta") => html! { <OktaSignInButton {onclick} /> },
        _ => html! {
            <button class="generic-sign-in-button" {onclick}>
                {"Sign in"}
            </button>
        },
    }
}

#[function_component(Login)]
pub fn login() -> Html {
    let login = build_login_callback();

    html! {
        <div class="login-container">
            <div class="login-card">
                <h1 class="login-title">{"videocall.rs"}</h1>

                { render_provider_button(login) }

                <p class="login-footer">
                    {"By signing in, you agree to our "}
                    <a href="https://github.com/security-union/videocall-rs">{"Terms of Service"}</a>
                    {" and "}
                    <a href="https://github.com/security-union/videocall-rs">{"Privacy Policy"}</a>
                </p>
            </div>
        </div>
    }
}

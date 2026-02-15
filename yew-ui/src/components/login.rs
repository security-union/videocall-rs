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

use gloo_utils::window;
use yew::prelude::*;

use crate::constants::{login_url, oauth_provider};

/// Render the Google "G" logo as an inline SVG.
fn google_logo() -> Html {
    html! {
        <svg class="oauth-provider-logo" width="20" height="20" viewBox="0 0 48 48" xmlns="http://www.w3.org/2000/svg">
            <path fill="#EA4335" d="M24 9.5c3.54 0 6.71 1.22 9.21 3.6l6.85-6.85C35.9 2.38 30.47 0 24 0 14.62 0 6.51 5.38 2.56 13.22l7.98 6.19C12.43 13.72 17.74 9.5 24 9.5z"/>
            <path fill="#4285F4" d="M46.98 24.55c0-1.57-.15-3.09-.38-4.55H24v9.02h12.94c-.58 2.96-2.26 5.48-4.78 7.18l7.73 6c4.51-4.18 7.09-10.36 7.09-17.65z"/>
            <path fill="#FBBC05" d="M10.53 28.59a14.5 14.5 0 0 1 0-9.18l-7.98-6.19a24.01 24.01 0 0 0 0 21.56l7.98-6.19z"/>
            <path fill="#34A853" d="M24 48c6.48 0 11.93-2.13 15.89-5.81l-7.73-6c-2.15 1.45-4.92 2.3-8.16 2.3-6.26 0-11.57-4.22-13.47-9.91l-7.98 6.19C6.51 42.62 14.62 48 24 48z"/>
        </svg>
    }
}

/// Render the Okta logo as an inline SVG.
fn okta_logo() -> Html {
    html! {
        <svg class="oauth-provider-logo" width="20" height="20" viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
            <circle cx="12" cy="12" r="12" fill="#007DC1"/>
            <circle cx="12" cy="12" r="5" fill="#FFFFFF"/>
        </svg>
    }
}

/// Return provider-specific (logo, button_text, css_class) for the sign-in button.
fn provider_branding() -> (Option<Html>, &'static str, &'static str) {
    match oauth_provider().as_deref() {
        Some("google") => (
            Some(google_logo()),
            "Sign in with Google",
            "oauth-btn-google",
        ),
        Some("okta") => (Some(okta_logo()), "Sign in with Okta", "oauth-btn-okta"),
        _ => (None, "Sign in", "oauth-btn-generic"),
    }
}

#[function_component(Login)]
pub fn login() -> Html {
    let login = Callback::from(|_: MouseEvent| match login_url() {
        Ok(mut url) => {
            // Check if there's a returnTo parameter in the current URL
            if let Ok(win) = window().location().search() {
                if !win.is_empty() {
                    // Append the query parameters from the current URL to the backend login URL
                    url = format!("{url}{win}");
                }
            }
            let _ = window().location().set_href(&url);
        }
        Err(e) => log::error!("Failed to get login URL: {e:?}"),
    });

    let (logo, button_text, btn_class) = provider_branding();

    html! {
        <div style="position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000; -webkit-font-smoothing: antialiased; -moz-osx-font-smoothing: grayscale;">
            <div class="flex flex-col items-center px-6 py-12">
                // Logo/Brand - Large, sleek, Apple-style
                <div style="margin-bottom: 4rem;">
                    <h1 style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; font-size: 5.5rem; font-weight: 300; letter-spacing: -0.03em; color: #ffffff; margin: 0;">{"videocall.rs"}</h1>
                </div>

                // Sign in box
                <div class="flex flex-col items-center">

                    // OAuth Sign-in button — provider-branded when configured
                    <button
                        onclick={login}
                        class={classes!(
                            "oauth-sign-in-btn",
                            btn_class,
                            "transition-transform",
                            "hover:scale-[1.02]",
                            "active:scale-[0.98]",
                        )}
                        style="background: #0a84ff; border: none; padding: 12px 32px; cursor: pointer; border-radius: 8px; font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; font-size: 1rem; font-weight: 500; color: #ffffff; letter-spacing: 0.01em; display: flex; align-items: center; gap: 8px;"
                    >
                        { for logo }
                        <span class="oauth-btn-label">{ button_text }</span>
                    </button>

                    <p style="margin-top: 2rem; text-align: center; font-size: 0.75rem; color: #86868b; font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;">
                        {"By signing in, you agree to our "}
                        <a href="https://github.com/security-union/videocall-rs" style="color: #0a84ff; text-decoration: none;" class="hover:underline">{"Terms of Service"}</a>
                        {" and "}
                        <a href="https://github.com/security-union/videocall-rs" style="color: #0a84ff; text-decoration: none;" class="hover:underline">{"Privacy Policy"}</a>
                    </p>
                </div>
            </div>
        </div>
    }
}

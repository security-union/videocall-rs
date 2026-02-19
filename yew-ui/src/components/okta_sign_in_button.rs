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

use yew::prelude::*;

/// Properties for the Okta sign-in button.
#[derive(Properties, PartialEq)]
pub struct OktaSignInButtonProps {
    pub onclick: Callback<MouseEvent>,
}

/// Okta-branded sign-in button — dark theme, styled to match the Google GSI
/// Material button dimensions and feel.
#[function_component(OktaSignInButton)]
pub fn okta_sign_in_button(props: &OktaSignInButtonProps) -> Html {
    html! {
        <button class="okta-sign-in-button" onclick={props.onclick.clone()}>
            <div class="okta-sign-in-button-content">
                <div class="okta-sign-in-button-icon">
                    <svg
                        width="20"
                        height="20"
                        viewBox="0 0 24 24"
                        xmlns="http://www.w3.org/2000/svg"
                        style="display: block;"
                    >
                        <circle cx="12" cy="12" r="12" fill="#007DC1"/>
                        <circle cx="12" cy="12" r="5" fill="#FFFFFF"/>
                    </svg>
                </div>
                <span class="okta-sign-in-button-label">{"Sign in with Okta"}</span>
            </div>
        </button>
    }
}

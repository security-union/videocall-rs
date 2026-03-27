// SPDX-License-Identifier: MIT OR Apache-2.0

//! Authentication module
//!
//! Handles OAuth session validation, user profile fetching, and logout.
//!
//! ## Logout strategy
//!
//! [`logout`] performs a **browser navigation** to the meeting-api `/logout`
//! endpoint rather than a `fetch()` request.  This is necessary because the
//! server may respond with a `303 See Other` redirect to the OIDC provider's
//! `end_session_endpoint` (RP-initiated logout).  A `fetch()` call follows
//! redirects internally and the browser never navigates to the provider —
//! destroying the provider session requires the browser to actually visit that
//! URL.  By setting `window.location.href` the browser follows the full
//! redirect chain as a top-level navigation, so both the local session cookie
//! *and* the provider session are terminated.

use crate::constants::{login_url, logout_url, meeting_api_client};
use anyhow::anyhow;
use gloo_utils::window;
use videocall_meeting_types::responses::ProfileResponse;

pub type UserProfile = ProfileResponse;

/// Check whether the current session JWT (cookie) is still valid.
pub async fn check_session() -> anyhow::Result<()> {
    let client = meeting_api_client().map_err(|e| anyhow!("Config error: {e}"))?;
    client.check_session().await.map_err(|e| anyhow!("{e}"))
}

/// Fetch the authenticated user's display name and user ID from the session.
pub async fn get_user_profile() -> anyhow::Result<UserProfile> {
    let client = meeting_api_client().map_err(|e| anyhow!("Config error: {e}"))?;
    client.get_profile().await.map_err(|e| anyhow!("{e}"))
}

/// Redirect the browser directly to the backend OAuth login endpoint.
///
/// Encodes the current page URL as the `returnTo` query parameter so the
/// server can bounce the user back to the meeting after successful
/// authentication. This skips the intermediate `/login` SPA page entirely,
/// giving unauthenticated users an immediate redirect to the identity provider
/// without requiring any additional click.
pub fn redirect_to_login() {
    match login_url() {
        Ok(base_url) => {
            let url = window()
                .location()
                .href()
                .ok()
                .filter(|s| !s.is_empty())
                .map(|rt| format!("{base_url}?returnTo={}", urlencoding::encode(&rt)))
                .unwrap_or(base_url);
            if let Err(e) = window().location().set_href(&url) {
                log::error!("redirect_to_login navigation failed: {e:?}");
            }
        }
        Err(e) => log::error!("Failed to get login URL for redirect: {e:?}"),
    }
}

/// Navigate the browser to the meeting-api `/logout` endpoint.
///
/// Using a browser navigation instead of `fetch` ensures the server's
/// `Set-Cookie: Max-Age=0` header is honoured for the top-level document,
/// and that any `303` redirect to the OIDC provider's `end_session_endpoint`
/// is followed as a real page load — terminating both the local session cookie
/// *and* the provider session in one round-trip.
///
/// Callers should perform any local state clean-up (clear cached profile,
/// localStorage, etc.) **before** calling this function, because the browser
/// will begin unloading the page as soon as `set_href` is called.
pub fn logout() -> Result<(), String> {
    let url = logout_url()?;
    window()
        .location()
        .set_href(&url)
        .map_err(|e| format!("Navigation to logout URL failed: {e:?}"))
}

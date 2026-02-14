// SPDX-License-Identifier: MIT OR Apache-2.0

//! Authentication module
//!
//! Handles OAuth session validation and user profile fetching

use crate::constants::meeting_api_base_url;
use anyhow::anyhow;
use reqwasm::http::{Request, RequestCredentials};
use videocall_meeting_types::responses::{APIResponse, ProfileResponse};

/// Re-export `ProfileResponse` as `UserProfile` for use across the UI.
pub type UserProfile = ProfileResponse;

/// Check if there is an active session by calling the backend /session endpoint
/// Returns Ok(()) if session is valid, Err if unauthorized (401) or other error
pub async fn check_session() -> anyhow::Result<()> {
    let base_url = meeting_api_base_url().map_err(|e| anyhow!("Config error: {e:?}"))?;
    let session_url = format!("{}/session", base_url);

    log::info!("Checking session at: {session_url}");

    let fetched_response = Request::get(&session_url)
        .credentials(RequestCredentials::Include)
        .send()
        .await?;

    log::info!(
        "Session check response status: {}",
        fetched_response.status()
    );

    match fetched_response.status() {
        401 => Err(anyhow!("unauthorized")),
        200..=299 => Ok(()),
        status => Err(anyhow!("Session check failed with status: {status}")),
    }
}

/// Get the current user's profile from the backend
pub async fn get_user_profile() -> anyhow::Result<UserProfile> {
    let base_url = meeting_api_base_url().map_err(|e| anyhow!("Config error: {e:?}"))?;
    let profile_url = format!("{}/profile", base_url);

    let fetched_response = Request::get(&profile_url)
        .credentials(RequestCredentials::Include)
        .send()
        .await?;

    let wrapper: APIResponse<UserProfile> = fetched_response.json().await?;
    Ok(wrapper.result)
}

/// Logout - clears session cookies on the backend
pub async fn logout() -> anyhow::Result<()> {
    let base_url = meeting_api_base_url().map_err(|e| anyhow!("Config error: {e:?}"))?;
    let logout_url = format!("{}/logout", base_url);

    Request::get(&logout_url)
        .credentials(RequestCredentials::Include)
        .send()
        .await?;

    Ok(())
}

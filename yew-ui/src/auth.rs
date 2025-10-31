// SPDX-License-Identifier: MIT OR Apache-2.0

//! Authentication module
//!
//! Handles OAuth session validation and user profile fetching

use crate::constants::app_config;
use anyhow::anyhow;
use reqwasm::http::{Request, RequestCredentials};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct UserProfile {
    pub email: String,
    pub name: String,
}

/// Check if there is an active session by calling the backend /session endpoint
/// Returns Ok(()) if session is valid, Err if unauthorized (401) or other error
pub async fn check_session() -> anyhow::Result<()> {
    let config = app_config().map_err(|e| anyhow!("Config error: {}", e))?;
    let session_url = format!("{}/session", config.api_base_url);

    log::info!("Checking session at: {}", session_url);

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
        status => Err(anyhow!("Session check failed with status: {}", status)),
    }
}

/// Get the current user's profile from the backend
pub async fn get_user_profile() -> anyhow::Result<UserProfile> {
    let config = app_config().map_err(|e| anyhow!("Config error: {}", e))?;
    let profile_url = format!("{}/profile", config.api_base_url);

    let fetched_response = Request::get(&profile_url)
        .credentials(RequestCredentials::Include)
        .send()
        .await?;

    let response: UserProfile = fetched_response.json().await?;
    Ok(response)
}

/// Logout - clears session cookies on the backend
pub async fn logout() -> anyhow::Result<()> {
    let config = app_config().map_err(|e| anyhow!("Config error: {}", e))?;
    let logout_url = format!("{}/logout", config.api_base_url);

    Request::get(&logout_url)
        .credentials(RequestCredentials::Include)
        .send()
        .await?;

    Ok(())
}

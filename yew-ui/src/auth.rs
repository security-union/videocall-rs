// SPDX-License-Identifier: MIT OR Apache-2.0

//! Authentication module
//!
//! Handles OAuth session validation and user profile fetching via
//! [`videocall_meeting_client`].

use crate::constants::meeting_api_client;
use anyhow::anyhow;
use videocall_meeting_types::responses::ProfileResponse;

/// Re-export `ProfileResponse` as `UserProfile` for use across the UI.
pub type UserProfile = ProfileResponse;

/// Check if there is an active session by calling the backend /session endpoint.
/// Returns Ok(()) if session is valid, Err if unauthorized (401) or other error.
pub async fn check_session() -> anyhow::Result<()> {
    let client = meeting_api_client().map_err(|e| anyhow!("Config error: {e}"))?;
    client.check_session().await.map_err(|e| anyhow!("{e}"))
}

/// Get the current user's profile from the backend.
pub async fn get_user_profile() -> anyhow::Result<UserProfile> {
    let client = meeting_api_client().map_err(|e| anyhow!("Config error: {e}"))?;
    client.get_profile().await.map_err(|e| anyhow!("{e}"))
}

/// Logout - clears session cookies on the backend.
pub async fn logout() -> anyhow::Result<()> {
    let client = meeting_api_client().map_err(|e| anyhow!("Config error: {e}"))?;
    client.logout().await.map_err(|e| anyhow!("{e}"))
}

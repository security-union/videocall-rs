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

//! Authentication and session endpoints: `/session`, `/profile`, `/logout`.

use videocall_meeting_types::responses::ProfileResponse;

use crate::error::ApiError;
use crate::{parse_api_response, parse_status_only, MeetingApiClient};

impl MeetingApiClient {
    /// Check whether the current session is valid.
    ///
    /// Calls `GET /session`. Returns `Ok(())` if authenticated, or
    /// [`ApiError::NotAuthenticated`] if the session JWT is missing/expired.
    pub async fn check_session(&self) -> Result<(), ApiError> {
        let response = self.get("/session").send().await?;
        parse_status_only(response).await
    }

    /// Get the authenticated user's profile.
    ///
    /// Calls `GET /profile` and returns the email and display name from the
    /// session JWT claims.
    pub async fn get_profile(&self) -> Result<ProfileResponse, ApiError> {
        let response = self.get("/profile").send().await?;
        parse_api_response(response).await
    }

    /// Log out by clearing the session cookie on the server.
    ///
    /// Calls `GET /logout`. After this, subsequent requests will return 401.
    pub async fn logout(&self) -> Result<(), ApiError> {
        let response = self.get("/logout").send().await?;
        parse_status_only(response).await
    }
}

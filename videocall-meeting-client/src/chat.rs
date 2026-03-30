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

//! Chat token exchange endpoint: `/api/v1/chat/token`.

use videocall_meeting_types::{requests::ChatTokenRequest, responses::ChatTokenResponse};

use crate::error::ApiError;
use crate::{parse_api_response, MeetingApiClient};

impl MeetingApiClient {
    /// Exchange the current videocall session for a chat-service bearer token
    /// scoped to the given meeting's chat room.
    ///
    /// Calls `POST /api/v1/chat/token`.
    ///
    /// Returns [`ChatTokenResponse`] containing the chat token, derived room
    /// ID, and optional expiry timestamp.
    ///
    /// # Errors
    ///
    /// - [`ApiError::NotAuthenticated`] — session is missing or expired.
    /// - [`ApiError::NotFound`] — chat service is not configured on the server.
    /// - [`ApiError::ServerError`] — chat service is unreachable or returned an error.
    pub async fn get_chat_token(&self, meeting_id: &str) -> Result<ChatTokenResponse, ApiError> {
        let body = ChatTokenRequest {
            meeting_id: meeting_id.to_string(),
        };
        let response = self.post("/api/v1/chat/token").json(&body).send().await?;
        parse_api_response(response).await
    }
}

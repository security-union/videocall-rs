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

//! Participant action endpoints: join, leave, status, refresh token.

use videocall_meeting_types::{requests::JoinMeetingRequest, responses::ParticipantStatusResponse};

use crate::error::ApiError;
use crate::{parse_api_response, MeetingApiClient};

impl MeetingApiClient {
    /// Join a meeting. If the meeting does not exist, it is auto-created with
    /// the joining user as host.
    ///
    /// Calls `POST /api/v1/meetings/{meeting_id}/join`.
    ///
    /// - **Hosts** are auto-admitted and receive a `room_token` immediately.
    /// - **Attendees** enter the waiting room (`room_token` is `None`).
    pub async fn join_meeting(
        &self,
        meeting_id: &str,
        display_name: Option<&str>,
    ) -> Result<ParticipantStatusResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/join");
        let body = JoinMeetingRequest {
            display_name: display_name.map(|s| s.to_string()),
        };
        let response = self.post(&path).json(&body).send().await?;
        parse_api_response(response).await
    }

    /// Check your current status in a meeting. This is the primary polling
    /// endpoint for attendees in the waiting room.
    ///
    /// Calls `GET /api/v1/meetings/{meeting_id}/status`.
    ///
    /// When `status` is `"admitted"`, the response includes a `room_token`.
    pub async fn get_status(
        &self,
        meeting_id: &str,
    ) -> Result<ParticipantStatusResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/status");
        let response = self.get(&path).send().await?;
        parse_api_response(response).await
    }

    /// Fetch a fresh room access token for the given meeting.
    ///
    /// Convenience wrapper around [`get_status`](Self::get_status) that
    /// extracts and returns just the `room_token` string. Returns an error
    /// if the participant is not admitted or the token is missing.
    pub async fn refresh_room_token(&self, meeting_id: &str) -> Result<String, ApiError> {
        let status = self.get_status(meeting_id).await?;
        status.room_token.ok_or_else(|| ApiError::ServerError {
            status: 200,
            body: "Admitted but no room token in response".to_string(),
        })
    }

    /// Leave a meeting. Updates participant status to `"left"`.
    ///
    /// Calls `POST /api/v1/meetings/{meeting_id}/leave`.
    ///
    /// The meeting ends automatically when the host leaves or when all
    /// admitted participants have left.
    pub async fn leave_meeting(
        &self,
        meeting_id: &str,
    ) -> Result<ParticipantStatusResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/leave");
        let response = self.post(&path).send().await?;
        parse_api_response(response).await
    }

    /// List all admitted participants in a meeting.
    ///
    /// Calls `GET /api/v1/meetings/{meeting_id}/participants`.
    ///
    /// Only participants who are themselves in the meeting can call this.
    pub async fn list_participants(
        &self,
        meeting_id: &str,
    ) -> Result<Vec<ParticipantStatusResponse>, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/participants");
        let response = self.get(&path).send().await?;
        parse_api_response(response).await
    }
}

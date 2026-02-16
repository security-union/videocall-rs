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

//! Meeting CRUD endpoints: create, list, get, delete.

use videocall_meeting_types::{
    requests::CreateMeetingRequest,
    responses::{
        CreateMeetingResponse, DeleteMeetingResponse, ListMeetingsResponse, MeetingInfoResponse,
    },
};

use crate::error::ApiError;
use crate::{parse_api_response, MeetingApiClient};

impl MeetingApiClient {
    /// Create a new meeting. The authenticated user becomes the host.
    ///
    /// Calls `POST /api/v1/meetings`.
    pub async fn create_meeting(
        &self,
        request: &CreateMeetingRequest,
    ) -> Result<CreateMeetingResponse, ApiError> {
        let response = self.post("/api/v1/meetings").json(request).send().await?;
        parse_api_response(response).await
    }

    /// List meetings owned by the authenticated user.
    ///
    /// Calls `GET /api/v1/meetings?limit={limit}&offset={offset}`.
    pub async fn list_meetings(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<ListMeetingsResponse, ApiError> {
        let response = self
            .get("/api/v1/meetings")
            .query(&[("limit", limit), ("offset", offset)])
            .send()
            .await?;
        parse_api_response(response).await
    }

    /// Get information about a specific meeting.
    ///
    /// Calls `GET /api/v1/meetings/{meeting_id}`.
    pub async fn get_meeting(&self, meeting_id: &str) -> Result<MeetingInfoResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}");
        let response = self.get(&path).send().await?;
        parse_api_response(response).await
    }

    /// Delete a meeting (soft-delete, owner only).
    ///
    /// Calls `DELETE /api/v1/meetings/{meeting_id}`.
    pub async fn delete_meeting(
        &self,
        meeting_id: &str,
    ) -> Result<DeleteMeetingResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}");
        let response = self.delete(&path).send().await?;
        parse_api_response(response).await
    }
}

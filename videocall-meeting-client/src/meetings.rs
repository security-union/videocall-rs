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
    requests::{CreateMeetingRequest, UpdateMeetingRequest},
    responses::{
        CreateMeetingResponse, DeleteMeetingResponse, ListMeetingsResponse,
        MeetingGuestInfoResponse, MeetingInfoResponse,
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

    /// List meetings owned by the authenticated user, optionally filtered by query.
    ///
    /// Calls `GET /api/v1/meetings?limit={limit}&offset={offset}[&q={q}]`.
    pub async fn list_meetings(
        &self,
        limit: i64,
        offset: i64,
        q: Option<&str>,
    ) -> Result<ListMeetingsResponse, ApiError> {
        let mut query = vec![("limit", limit.to_string()), ("offset", offset.to_string())];
        if let Some(query_str) = q {
            query.push(("q", query_str.to_string()));
        }

        let response = self
            .get("/api/v1/meetings")
            .query(&query)
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

    /// End a meeting (owner only).
    ///
    /// Calls `POST /api/v1/meetings/{meeting_id}/end`.
    pub async fn end_meeting(&self, meeting_id: &str) -> Result<MeetingInfoResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/end");
        let response = self.post(&path).send().await?;
        parse_api_response(response).await
    }

    /// Update meeting settings (owner only).
    ///
    /// Calls `PATCH /api/v1/meetings/{meeting_id}`.
    pub async fn update_meeting(
        &self,
        meeting_id: &str,
        request: &UpdateMeetingRequest,
    ) -> Result<MeetingInfoResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}");
        let response = self.patch(&path).json(request).send().await?;
        parse_api_response(response).await
    }

    /// Get public guest info for a meeting (no authentication required).
    ///
    /// Calls `GET /api/v1/meetings/{meeting_id}/guest-info`.
    pub async fn get_meeting_guest_info(
        &self,
        meeting_id: &str,
    ) -> Result<MeetingGuestInfoResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/guest-info");
        let response = self.get(&path).send().await?;
        parse_api_response(response).await
    }
}

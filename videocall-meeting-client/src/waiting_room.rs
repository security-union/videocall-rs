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

//! Waiting room management endpoints: list waiting, admit, admit-all, reject.

use videocall_meeting_types::{
    requests::AdmitRequest,
    responses::{AdmitAllResponse, ParticipantStatusResponse, WaitingRoomResponse},
};

use crate::error::ApiError;
use crate::{parse_api_response, MeetingApiClient};

impl MeetingApiClient {
    /// List participants currently in the waiting room.
    ///
    /// Calls `GET /api/v1/meetings/{meeting_id}/waiting`.
    ///
    /// Only admitted participants can view the waiting room.
    pub async fn get_waiting_room(
        &self,
        meeting_id: &str,
    ) -> Result<WaitingRoomResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/waiting");
        let response = self.get(&path).send().await?;
        parse_api_response(response).await
    }

    /// Admit a participant from the waiting room.
    ///
    /// Calls `POST /api/v1/meetings/{meeting_id}/admit`.
    ///
    /// The admitted participant picks up their `room_token` on their next
    /// `GET /status` poll.
    pub async fn admit_participant(
        &self,
        meeting_id: &str,
        email: &str,
    ) -> Result<ParticipantStatusResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/admit");
        let body = AdmitRequest {
            email: email.to_string(),
        };
        let response = self.post(&path).json(&body).send().await?;
        parse_api_response(response).await
    }

    /// Admit all participants currently in the waiting room.
    ///
    /// Calls `POST /api/v1/meetings/{meeting_id}/admit-all`.
    pub async fn admit_all(&self, meeting_id: &str) -> Result<AdmitAllResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/admit-all");
        let response = self.post(&path).send().await?;
        parse_api_response(response).await
    }

    /// Reject a participant from the waiting room.
    ///
    /// Calls `POST /api/v1/meetings/{meeting_id}/reject`.
    pub async fn reject_participant(
        &self,
        meeting_id: &str,
        email: &str,
    ) -> Result<ParticipantStatusResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/reject");
        let body = AdmitRequest {
            email: email.to_string(),
        };
        let response = self.post(&path).json(&body).send().await?;
        parse_api_response(response).await
    }
}

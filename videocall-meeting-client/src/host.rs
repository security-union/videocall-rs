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

//! Host-only meeting controls: mute a single participant or mute all.

use videocall_meeting_types::requests::MuteParticipantRequest;

use crate::error::ApiError;
use crate::{parse_status_only, MeetingApiClient};

impl MeetingApiClient {
    /// Ask a single participant to mute their microphone.
    ///
    /// Calls `POST /api/v1/meetings/{meeting_id}/mute`.
    ///
    /// Only the meeting host may call this endpoint. Mute is soft (a NATS
    /// event is broadcast; the client honors it locally). The host cannot
    /// force-unmute a participant.
    pub async fn mute_participant(&self, meeting_id: &str, user_id: &str) -> Result<(), ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/mute");
        let body = MuteParticipantRequest {
            user_id: user_id.to_string(),
        };
        let response = self.post(&path).json(&body).send().await?;
        parse_status_only(response).await
    }

    /// Ask every participant to mute their microphone.
    ///
    /// Calls `POST /api/v1/meetings/{meeting_id}/mute-all`.
    ///
    /// Only the meeting host may call this endpoint.
    pub async fn mute_all(&self, meeting_id: &str) -> Result<(), ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/mute-all");
        let response = self.post(&path).send().await?;
        parse_status_only(response).await
    }
}

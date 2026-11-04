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

use videocall_meeting_types::{
    requests::{GuestJoinRequest, JoinMeetingRequest, UpdateDisplayNameRequest},
    responses::ParticipantStatusResponse,
};

use crate::error::ApiError;
use crate::{parse_api_response, parse_status_only, MeetingApiClient};

impl MeetingApiClient {
    /// Join a meeting. If the meeting does not exist, it is auto-created with
    /// the joining user as host.
    ///
    /// Calls `POST /api/v1/meetings/{meeting_id}/join`.
    ///
    /// - **Hosts** are auto-admitted and receive a `room_token` immediately.
    /// - **Attendees** enter the waiting room (`room_token` is `None`).
    /// - If the meeting is not yet active, `status` will be
    ///   `"waiting_for_meeting"` and `observer_token` will contain a JWT for
    ///   receiving push notifications.
    ///
    /// `password` is the meeting password (issue #1613). Pass `None` for
    /// meetings whose `has_password` is `false`, and for the meeting owner —
    /// the server exempts `creator_id` from the check. Every other caller
    /// joining a `has_password: true` meeting must supply it or the server
    /// answers [`ApiError::MeetingPasswordRequired`]; a wrong one answers
    /// [`ApiError::InvalidMeetingPassword`].
    pub async fn join_meeting(
        &self,
        meeting_id: &str,
        display_name: Option<&str>,
        password: Option<&str>,
    ) -> Result<ParticipantStatusResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/join");
        let body = JoinMeetingRequest {
            display_name: display_name.map(|s| s.to_string()),
            password: password.map(|s| s.to_string()),
        };
        let response = self.post(&path).json(&body).send().await?;
        parse_api_response(response).await
    }

    /// Join a meeting as a guest (unauthenticated).
    ///
    /// Calls `POST /api/v1/meetings/{meeting_id}/join-guest`.
    ///
    /// The meeting must have `allow_guests` enabled. `display_name` is required.
    /// Guests are never hosts and cannot auto-create meetings.
    ///
    /// `password` is the meeting password (issue #1613). A guest is never the
    /// meeting owner, so it is required whenever the meeting has one; see
    /// [`Self::join_meeting`] for the error codes.
    pub async fn join_meeting_as_guest(
        &self,
        meeting_id: &str,
        display_name: &str,
        guest_session_id: Option<&str>,
        password: Option<&str>,
    ) -> Result<ParticipantStatusResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/join-guest");
        let body = GuestJoinRequest {
            display_name: display_name.to_string(),
            guest_session_id: guest_session_id.map(|s| s.to_string()),
            password: password.map(|s| s.to_string()),
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

    /// Check a guest participant's current status in a meeting.
    ///
    /// Calls `GET /api/v1/meetings/{meeting_id}/guest-status`.
    ///
    /// The client must be configured with `AuthMode::Bearer(observer_token)`
    /// to pass the `GuestObserver` extractor on the server.
    ///
    /// When `status` is `"admitted"`, the response includes a `room_token`.
    pub async fn get_guest_status(
        &self,
        meeting_id: &str,
    ) -> Result<ParticipantStatusResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/guest-status");
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

    /// Leave a meeting as an unauthenticated guest.
    ///
    /// Calls `POST /api/v1/meetings/{meeting_id}/leave-guest`.
    pub async fn leave_meeting_as_guest(
        &self,
        meeting_id: &str,
    ) -> Result<ParticipantStatusResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/leave-guest");
        let response = self.post(&path).send().await?;
        parse_api_response(response).await
    }

    /// Renew your own presence lease from the waiting room or the manual pre-join lobby (call every ~30s).
    pub async fn presence_keepalive(&self, meeting_id: &str) -> Result<(), ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/presence/keepalive");
        let response = self.post(&path).send().await?;
        parse_status_only(response).await
    }

    /// Guest counterpart of [`Self::presence_keepalive`].
    pub async fn presence_keepalive_guest(&self, meeting_id: &str) -> Result<(), ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/presence/keepalive-guest");
        let response = self.post(&path).send().await?;
        parse_status_only(response).await
    }

    /// Sends `POST /api/v1/meetings/{meeting_id}/captions` with
    /// `{"enabled", "renew"}` and the room token as bearer: enable
    /// (`renew = false`), renew (`renew = true`) or revoke (`enabled = false`)
    /// the meeting's live-captions lease.
    ///
    /// `renew = true` without `enabled = true` is contradictory and returns
    /// [`ApiError::Config`] unsent.
    pub async fn set_captions(
        &self,
        meeting_id: &str,
        enabled: bool,
        renew: bool,
    ) -> Result<(), ApiError> {
        let response = self
            .captions_request(meeting_id, enabled, renew)?
            .send()
            .await?;
        parse_status_only(response).await
    }

    fn captions_request(
        &self,
        meeting_id: &str,
        enabled: bool,
        renew: bool,
    ) -> Result<reqwest::RequestBuilder, ApiError> {
        if renew && !enabled {
            return Err(ApiError::Config(
                "set_captions: renew requires enabled".to_string(),
            ));
        }
        let path = format!("/api/v1/meetings/{meeting_id}/captions");
        Ok(self
            .post(&path)
            .json(&serde_json::json!({ "enabled": enabled, "renew": renew })))
    }

    /// Update your display name during an active meeting.
    ///
    /// Calls `PUT /api/v1/meetings/{meeting_id}/display-name`.
    ///
    /// `session_id` identifies the specific tab/session performing the rename.
    /// When provided, the server scopes the rename and its broadcast to that
    /// single session — sibling tabs of the same authenticated user keep their
    /// own names (HCL issue #828 follow-up). Pass `None` for legacy/global
    /// renames that should apply to every session sharing the caller's
    /// `user_id`.
    pub async fn update_display_name(
        &self,
        meeting_id: &str,
        display_name: &str,
        session_id: Option<u64>,
    ) -> Result<ParticipantStatusResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/display-name");
        let body = UpdateDisplayNameRequest {
            display_name: display_name.to_string(),
            session_id,
        };
        let response = self.put(&path).json(&body).send().await?;
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

#[cfg(test)]
mod tests {
    use crate::{AuthMode, MeetingApiClient};

    fn captions_body(enabled: bool, renew: bool) -> (reqwest::Request, serde_json::Value) {
        let client = MeetingApiClient::new("http://api.test/", AuthMode::Bearer("tok".into()));
        let request = client
            .captions_request("standup", enabled, renew)
            .unwrap()
            .build()
            .unwrap();
        let body = request.body().and_then(|b| b.as_bytes()).unwrap();
        let json = serde_json::from_slice(body).unwrap();
        (request, json)
    }

    #[test]
    fn captions_request_posts_enabled_and_renew_to_the_meeting() {
        let (request, body) = captions_body(true, false);
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(
            request.url().as_str(),
            "http://api.test/api/v1/meetings/standup/captions"
        );
        assert_eq!(
            request.headers()[reqwest::header::AUTHORIZATION],
            "Bearer tok"
        );
        assert_eq!(body, serde_json::json!({"enabled": true, "renew": false}));
    }

    #[test]
    fn captions_request_carries_renewal_and_revoke() {
        assert_eq!(
            captions_body(true, true).1,
            serde_json::json!({"enabled": true, "renew": true})
        );
        assert_eq!(
            captions_body(false, false).1,
            serde_json::json!({"enabled": false, "renew": false})
        );
    }

    #[test]
    fn captions_request_rejects_renewing_a_revoke() {
        let client = MeetingApiClient::new("http://api.test/", AuthMode::Bearer("tok".into()));
        assert!(matches!(
            client.captions_request("standup", false, true),
            Err(crate::ApiError::Config(_))
        ));
    }
}

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
        CreateMeetingResponse, DeleteMeetingResponse, ListFeedResponse, ListJoinedMeetingsResponse,
        ListMeetingsResponse, MeetingGuestInfoResponse, MeetingInfoResponse,
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

        let response = self.get("/api/v1/meetings").query(&query).send().await?;
        parse_api_response(response).await
    }

    /// List meetings the authenticated user has previously been admitted into,
    /// ordered by most recent admission time (descending). Includes both
    /// meetings the user owns and meetings they joined as a non-owner.
    ///
    /// Calls `GET /api/v1/meetings/joined?limit={limit}`.
    pub async fn list_joined_meetings(
        &self,
        limit: u32,
    ) -> Result<ListJoinedMeetingsResponse, ApiError> {
        let query = [("limit", limit.to_string())];
        let response = self
            .get("/api/v1/meetings/joined")
            .query(&query)
            .send()
            .await?;
        parse_api_response(response).await
    }

    /// Fetch the deduplicated home-page meeting feed: every meeting the
    /// authenticated user owns or has been admitted into, ordered by
    /// `last_active_at` descending. Each row carries an authoritative,
    /// server-computed `is_owner` flag.
    ///
    /// Calls `GET /api/v1/meetings/feed[?limit={limit}]`.
    ///
    /// `limit` is optional. When `None`, the server applies its default of
    /// 200 rows. Callers that pass a value larger than 200 will be clamped
    /// silently by the server.
    pub async fn list_meeting_feed(
        &self,
        limit: Option<u32>,
    ) -> Result<ListFeedResponse, ApiError> {
        let mut req = self.get("/api/v1/meetings/feed");
        if let Some(l) = limit {
            req = req.query(&[("limit", l.to_string())]);
        }
        let response = req.send().await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use videocall_meeting_types::responses::APIResponse;

    /// Parsing-only sanity check for the merged-feed wire shape: a payload
    /// with one owned and one not-owned meeting must round-trip through
    /// serde with the `is_owner` flag preserved per row.
    ///
    /// The end-to-end HTTP path is exercised by the backend's
    /// `meeting-api/tests/list_feed_tests.rs` suite — this test exists so
    /// a breaking change to either the type definition or `serde` derive
    /// fails fast in the client crate too.
    #[test]
    fn list_feed_response_parses_mixed_ownership() {
        let body = r#"{
            "success": true,
            "result": {
                "meetings": [
                    {
                        "meeting_id": "owned-1",
                        "state": "active",
                        "last_active_at": 1714323600000,
                        "created_at": 1714323000000,
                        "started_at": 1714323500000,
                        "host": "alice@example.com",
                        "is_owner": true,
                        "participant_count": 2,
                        "waiting_count": 0,
                        "has_password": false,
                        "allow_guests": false,
                        "waiting_room_enabled": true,
                        "admitted_can_admit": false,
                        "end_on_host_leave": true
                    },
                    {
                        "meeting_id": "joined-1",
                        "state": "ended",
                        "last_active_at": 1714323200000,
                        "created_at": 1714323000000,
                        "started_at": 1714323100000,
                        "ended_at": 1714323200000,
                        "host": "bob@example.com",
                        "is_owner": false,
                        "participant_count": 4,
                        "waiting_count": 0,
                        "has_password": true,
                        "allow_guests": false,
                        "waiting_room_enabled": false,
                        "admitted_can_admit": true,
                        "end_on_host_leave": false
                    }
                ]
            }
        }"#;

        let parsed: APIResponse<ListFeedResponse> =
            serde_json::from_str(body).expect("envelope must parse");
        assert!(parsed.success);
        let feed = parsed.result;
        assert_eq!(feed.meetings.len(), 2);

        let owned = &feed.meetings[0];
        assert_eq!(owned.meeting_id, "owned-1");
        assert!(owned.is_owner);
        assert_eq!(owned.started_at, Some(1_714_323_500_000));
        assert!(owned.ended_at.is_none());

        let joined = &feed.meetings[1];
        assert_eq!(joined.meeting_id, "joined-1");
        assert!(!joined.is_owner);
        assert!(joined.has_password);
        assert_eq!(joined.ended_at, Some(1_714_323_200_000));
    }

    /// `started_at` is `Option<i64>` on `MeetingFeedSummary` because idle
    /// meetings that have never been activated have no started timestamp.
    /// Verify the type honours that — a missing key must deserialise as
    /// `None`, not as a parse error.
    #[test]
    fn list_feed_response_handles_missing_optional_fields() {
        let body = r#"{
            "success": true,
            "result": {
                "meetings": [
                    {
                        "meeting_id": "idle-never-started",
                        "state": "idle",
                        "last_active_at": 1714323000000,
                        "created_at": 1714323000000,
                        "is_owner": true,
                        "participant_count": 0,
                        "waiting_count": 0,
                        "has_password": false,
                        "allow_guests": false,
                        "waiting_room_enabled": true,
                        "admitted_can_admit": false,
                        "end_on_host_leave": true
                    }
                ]
            }
        }"#;

        let parsed: APIResponse<ListFeedResponse> =
            serde_json::from_str(body).expect("envelope must parse");
        let m = &parsed.result.meetings[0];
        assert!(m.started_at.is_none());
        assert!(m.ended_at.is_none());
        assert!(m.host.is_none());
        assert!(m.is_owner);
    }
}

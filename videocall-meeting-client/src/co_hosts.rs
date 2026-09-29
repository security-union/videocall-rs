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

//! Co-host management: list, grant, and revoke co-hosts.

use videocall_meeting_types::{
    requests::{GrantCoHostRequest, RevokeCoHostRequest},
    responses::ListCoHostsResponse,
};

use crate::error::ApiError;
use crate::{parse_api_response, MeetingApiClient};

impl MeetingApiClient {
    pub async fn list_co_hosts(&self, meeting_id: &str) -> Result<ListCoHostsResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/co-hosts");
        let response = self.get(&path).send().await?;
        parse_api_response(response).await
    }

    pub async fn grant_co_host(
        &self,
        meeting_id: &str,
        user_id: &str,
        persist: impl Into<Option<bool>>,
    ) -> Result<ListCoHostsResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/co-hosts");
        let body = GrantCoHostRequest {
            user_id: user_id.to_string(),
            persist: persist.into(),
        };
        let response = self.post(&path).json(&body).send().await?;
        parse_api_response(response).await
    }

    pub async fn revoke_co_host(
        &self,
        meeting_id: &str,
        user_id: &str,
    ) -> Result<ListCoHostsResponse, ApiError> {
        let path = format!("/api/v1/meetings/{meeting_id}/co-hosts/revoke");
        let body = RevokeCoHostRequest {
            user_id: user_id.to_string(),
        };
        let response = self.post(&path).json(&body).send().await?;
        parse_api_response(response).await
    }
}

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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Meeting API client for joining meetings and checking status

use crate::constants::app_config;
use reqwasm::http::{Request, RequestCredentials};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct JoinMeetingResponse {
    pub email: String,
    pub status: String,
    pub is_host: bool,
    pub joined_at: i64,
    pub admitted_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MeetingInfo {
    pub meeting_id: String,
    pub state: String,
    pub host: String,
    pub host_display_name: Option<String>,
    pub has_password: bool,
    pub your_status: Option<JoinMeetingResponse>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JoinMeetingRequest {
    pub display_name: Option<String>,
}

#[derive(Debug, Clone)]
pub enum JoinError {
    NotAuthenticated,
    MeetingNotActive,
    NetworkError(String),
    ServerError(u16, String),
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JoinError::NotAuthenticated => write!(f, "Not authenticated. Please log in."),
            JoinError::MeetingNotActive => write!(f, "Meeting is not active. The host must join first."),
            JoinError::NetworkError(e) => write!(f, "Network error: {}", e),
            JoinError::ServerError(code, msg) => write!(f, "Server error ({}): {}", code, msg),
        }
    }
}

/// Join a meeting via the API
/// Returns the participant status which indicates if they are admitted, waiting, etc.
pub async fn join_meeting(meeting_id: &str, display_name: Option<&str>) -> Result<JoinMeetingResponse, JoinError> {
    let config = app_config().map_err(|e| JoinError::NetworkError(e))?;
    let url = format!("{}/api/v1/meetings/{}/join", config.api_base_url, meeting_id);

    log::info!("Joining meeting via API: {} (display_name: {:?})", url, display_name);

    let body = JoinMeetingRequest {
        display_name: display_name.map(|s| s.to_string()),
    };
    let body_json = serde_json::to_string(&body)
        .map_err(|e| JoinError::NetworkError(format!("Failed to serialize request: {e}")))?;

    let response = Request::post(&url)
        .credentials(RequestCredentials::Include)
        .header("Content-Type", "application/json")
        .body(body_json)
        .send()
        .await
        .map_err(|e| JoinError::NetworkError(format!("{e}")))?;

    let status = response.status();
    log::info!("Join meeting response status: {}", status);

    match status {
        200 => {
            let data: JoinMeetingResponse = response
                .json()
                .await
                .map_err(|e| JoinError::NetworkError(format!("Failed to parse response: {e}")))?;
            log::info!("Join response: status={}, is_host={}", data.status, data.is_host);
            Ok(data)
        }
        401 => Err(JoinError::NotAuthenticated),
        400 => {
            // Check if it's "meeting not active"
            let text = response.text().await.unwrap_or_default();
            if text.contains("MEETING_NOT_ACTIVE") {
                Err(JoinError::MeetingNotActive)
            } else {
                Err(JoinError::ServerError(400, text))
            }
        }
        _ => {
            let text = response.text().await.unwrap_or_default();
            Err(JoinError::ServerError(status, text))
        }
    }
}

/// Get meeting info including host email
pub async fn get_meeting_info(meeting_id: &str) -> Result<MeetingInfo, JoinError> {
    let config = app_config().map_err(|e| JoinError::NetworkError(e))?;
    let url = format!("{}/api/v1/meetings/{}", config.api_base_url, meeting_id);

    let response = Request::get(&url)
        .credentials(RequestCredentials::Include)
        .send()
        .await
        .map_err(|e| JoinError::NetworkError(format!("{e}")))?;

    match response.status() {
        200 => {
            let data: MeetingInfo = response
                .json()
                .await
                .map_err(|e| JoinError::NetworkError(format!("Failed to parse response: {e}")))?;
            Ok(data)
        }
        401 => Err(JoinError::NotAuthenticated),
        404 => Err(JoinError::ServerError(404, "Meeting not found".to_string())),
        status => {
            let text = response.text().await.unwrap_or_default();
            Err(JoinError::ServerError(status, text))
        }
    }
}

/// Check participant status in a meeting
pub async fn check_status(meeting_id: &str) -> Result<JoinMeetingResponse, JoinError> {
    let config = app_config().map_err(|e| JoinError::NetworkError(e))?;
    let url = format!("{}/api/v1/meetings/{}/status", config.api_base_url, meeting_id);

    let response = Request::get(&url)
        .credentials(RequestCredentials::Include)
        .send()
        .await
        .map_err(|e| JoinError::NetworkError(format!("{e}")))?;

    match response.status() {
        200 => {
            let data: JoinMeetingResponse = response
                .json()
                .await
                .map_err(|e| JoinError::NetworkError(format!("Failed to parse response: {e}")))?;
            Ok(data)
        }
        401 => Err(JoinError::NotAuthenticated),
        404 => Err(JoinError::ServerError(404, "Not in meeting".to_string())),
        status => {
            let text = response.text().await.unwrap_or_default();
            Err(JoinError::ServerError(status, text))
        }
    }
}

/// Leave a meeting - updates participant status to 'left' in database
pub async fn leave_meeting(meeting_id: &str) -> Result<(), JoinError> {
    let config = app_config().map_err(|e| JoinError::NetworkError(e))?;
    let url = format!("{}/api/v1/meetings/{}/leave", config.api_base_url, meeting_id);

    log::info!("Leaving meeting via API: {}", url);

    let response = Request::post(&url)
        .credentials(RequestCredentials::Include)
        .send()
        .await
        .map_err(|e| JoinError::NetworkError(format!("{e}")))?;

    match response.status() {
        200 => {
            log::info!("Successfully left meeting {}", meeting_id);
            Ok(())
        }
        401 => Err(JoinError::NotAuthenticated),
        404 => {
            // Not in meeting is fine - just means we weren't tracked
            log::warn!("Not in meeting {} when trying to leave", meeting_id);
            Ok(())
        }
        status => {
            let text = response.text().await.unwrap_or_default();
            Err(JoinError::ServerError(status, text))
        }
    }
}

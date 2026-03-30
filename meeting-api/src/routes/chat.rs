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

//! Chat token exchange endpoint.
//!
//! `POST /api/v1/chat/token` validates the caller's videocall session and
//! exchanges it for a chat-service-specific bearer token by calling the
//! external chat service's token API with server-side credentials.
//!
//! This endpoint is only available when the chat service is configured via
//! the `CHAT_SERVICE_URL` and `CHAT_SERVICE_API_KEY` environment variables.

use axum::{extract::State, Json};
use videocall_meeting_types::{
    requests::ChatTokenRequest,
    responses::{APIResponse, ChatTokenResponse},
};

use crate::auth::AuthUser;
use crate::error::AppError;
use crate::state::AppState;

/// Response shape expected from the external chat service's token endpoint.
///
/// The chat service is expected to return at least a `token` field.
/// `expires_at` is optional (Unix timestamp in seconds).
#[derive(serde::Deserialize)]
struct ExternalTokenResponse {
    token: String,
    #[serde(default)]
    expires_at: Option<i64>,
}

/// POST /api/v1/chat/token
///
/// Exchange a valid videocall session for a chat-service bearer token scoped
/// to the meeting's chat room.
///
/// # Request body
///
/// ```json
/// { "meeting_id": "standup-2024" }
/// ```
///
/// # Successful response (200)
///
/// ```json
/// {
///   "success": true,
///   "result": {
///     "token": "eyJ...",
///     "room_id": "videocall-standup-2024",
///     "expires_at": 1234567890
///   }
/// }
/// ```
///
/// # Errors
///
/// - **401** — Missing or invalid session.
/// - **404** — Chat service is not configured (`CHAT_SERVICE_URL` not set).
/// - **400** — Empty `meeting_id`.
/// - **502** — The external chat service returned an error or is unreachable.
pub async fn get_chat_token(
    State(state): State<AppState>,
    AuthUser { user_id, name }: AuthUser,
    Json(body): Json<ChatTokenRequest>,
) -> Result<Json<APIResponse<ChatTokenResponse>>, AppError> {
    // Validate that chat integration is configured.
    let chat_url = state.chat_service_url.as_deref().ok_or_else(|| {
        AppError::new(
            axum::http::StatusCode::NOT_FOUND,
            videocall_meeting_types::APIError {
                code: "CHAT_NOT_CONFIGURED".to_string(),
                message: "Chat service is not configured on this server.".to_string(),
                engineering_error: None,
            },
        )
    })?;

    let api_key = state.chat_service_api_key.as_deref().unwrap_or_default();

    // Validate meeting_id is not empty.
    let meeting_id = body.meeting_id.trim();
    if meeting_id.is_empty() {
        return Err(AppError::new(
            axum::http::StatusCode::BAD_REQUEST,
            videocall_meeting_types::APIError {
                code: "INVALID_MEETING_ID".to_string(),
                message: "meeting_id must not be empty.".to_string(),
                engineering_error: None,
            },
        ));
    }

    // Derive the chat room ID.
    let room_id = format!("{}{}", state.chat_room_prefix, meeting_id);

    // Call the external chat service's token endpoint.
    let token_url = format!("{}/auth/token", chat_url.trim_end_matches('/'));

    let external_response = state
        .http_client
        .post(&token_url)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "user_id": user_id,
            "display_name": name,
            "room_id": room_id,
        }))
        .send()
        .await
        .map_err(|e| {
            tracing::error!("Chat service request failed: {e}");
            AppError::new(
                axum::http::StatusCode::BAD_GATEWAY,
                videocall_meeting_types::APIError {
                    code: "CHAT_SERVICE_ERROR".to_string(),
                    message: "Failed to reach the chat service.".to_string(),
                    engineering_error: Some(e.to_string()),
                },
            )
        })?;

    if !external_response.status().is_success() {
        let status = external_response.status().as_u16();
        let body_text = external_response.text().await.unwrap_or_default();
        tracing::error!("Chat service returned {status}: {body_text}");
        return Err(AppError::new(
            axum::http::StatusCode::BAD_GATEWAY,
            videocall_meeting_types::APIError {
                code: "CHAT_SERVICE_ERROR".to_string(),
                message: format!("Chat service returned HTTP {status}."),
                engineering_error: Some(body_text),
            },
        ));
    }

    let ext: ExternalTokenResponse = external_response.json().await.map_err(|e| {
        tracing::error!("Failed to parse chat service response: {e}");
        AppError::new(
            axum::http::StatusCode::BAD_GATEWAY,
            videocall_meeting_types::APIError {
                code: "CHAT_SERVICE_ERROR".to_string(),
                message: "Invalid response from chat service.".to_string(),
                engineering_error: Some(e.to_string()),
            },
        )
    })?;

    Ok(Json(APIResponse::ok(ChatTokenResponse {
        token: ext.token,
        room_id,
        expires_at: ext.expires_at,
    })))
}

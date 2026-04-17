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

//! Push-based indexing of meeting documents to the external SearchV2 middleware.
//!
//! When `SEARCH_API_URL` is configured, meeting lifecycle events (create, update,
//! end, delete) push document updates to OpenSearch via the middleware's
//! content-push API:
//!   PUT    {SEARCH_API_URL}/contentsources/cs-vc-meetings/documents/{room_id}
//!   DELETE {SEARCH_API_URL}/contentsources/cs-vc-meetings/documents/{room_id}

use crate::db::meetings::MeetingRow;
use crate::state::AppState;
use serde_json::json;

const CONTENT_SOURCE_ID: &str = "cs-vc-meetings";

/// Push a meeting document to SearchV2 after create, update, or end.
///
/// This is fire-and-forget: failures are logged but do not block the API response.
pub async fn push_meeting(state: &AppState, meeting: &MeetingRow) {
    let (base_url, token) = match (&state.search_api_url, &state.search_api_token) {
        (Some(url), Some(tok)) => (url, tok),
        _ => return, // SearchV2 not configured — silently skip
    };

    let doc_id = &meeting.room_id;
    let url = format!(
        "{}/contentsources/{}/documents/{}",
        base_url.trim_end_matches('/'),
        CONTENT_SOURCE_ID,
        doc_id,
    );

    let created_ms = meeting.created_at.timestamp_millis();
    let updated_ms = meeting.updated_at.timestamp_millis();
    let creator = meeting.creator_id.as_deref().unwrap_or("unknown");
    let state_str = meeting.state.as_deref().unwrap_or("created");
    let host = meeting.host_display_name.as_deref().unwrap_or("");

    let body = json!({
        "created": created_ms,
        "updated": updated_ms,
        "title": meeting.room_id,
        "type": "vc-meetings",
        "description": format!("Meeting {} ({})", meeting.room_id, state_str),
        "acls": [format!("user:{}", creator), "public"],
        "tags": ["meeting", state_str],
        "documentObject": {
            "room_id": meeting.room_id,
            "state": state_str,
            "host_display_name": host,
            "creator_id": creator,
            "started_at": meeting.started_at.to_rfc3339(),
            "ended_at": meeting.ended_at.map(|t| t.to_rfc3339()),
            "attendees": meeting.attendees,
            "has_password": meeting.password_hash.is_some(),
            "waiting_room_enabled": meeting.waiting_room_enabled,
            "admitted_can_admit": meeting.admitted_can_admit,
        }
    });

    let result = state
        .http_client
        .put(&url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            tracing::debug!("SearchV2 push OK for meeting {}", doc_id);
        }
        Ok(resp) => {
            tracing::warn!(
                "SearchV2 push failed for meeting {}: HTTP {}",
                doc_id,
                resp.status()
            );
        }
        Err(e) => {
            tracing::warn!("SearchV2 push error for meeting {}: {}", doc_id, e);
        }
    }
}

/// Delete a meeting document from SearchV2 after meeting deletion.
pub async fn delete_meeting_doc(state: &AppState, room_id: &str) {
    let (base_url, token) = match (&state.search_api_url, &state.search_api_token) {
        (Some(url), Some(tok)) => (url, tok),
        _ => return,
    };

    let url = format!(
        "{}/contentsources/{}/documents/{}",
        base_url.trim_end_matches('/'),
        CONTENT_SOURCE_ID,
        room_id,
    );

    let result = state
        .http_client
        .delete(&url)
        .bearer_auth(token)
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 204 => {
            tracing::debug!("SearchV2 delete OK for meeting {}", room_id);
        }
        Ok(resp) => {
            tracing::warn!(
                "SearchV2 delete failed for meeting {}: HTTP {}",
                room_id,
                resp.status()
            );
        }
        Err(e) => {
            tracing::warn!("SearchV2 delete error for meeting {}: {}", room_id, e);
        }
    }
}

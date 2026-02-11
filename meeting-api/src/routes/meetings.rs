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

//! Handlers for meeting CRUD endpoints.

use argon2::PasswordHasher;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use rand::Rng;
use videocall_meeting_types::{
    requests::{CreateMeetingRequest, ListMeetingsQuery},
    responses::{
        APIResponse, CreateMeetingResponse, DeleteMeetingResponse, ListMeetingsResponse,
        MeetingInfoResponse, MeetingSummary,
    },
};

use crate::auth::AuthUser;
use crate::db::{meetings as db_meetings, participants as db_participants};
use crate::error::AppError;
use crate::state::AppState;

const MAX_ATTENDEES: usize = 100;
const VALID_ID_PATTERN: &str = "^[a-zA-Z0-9_-]+$";

fn validate_meeting_id(meeting_id: &str) -> Result<(), AppError> {
    if meeting_id.is_empty() {
        return Err(AppError::invalid_meeting_id("cannot be empty"));
    }
    if meeting_id.len() > 255 {
        return Err(AppError::invalid_meeting_id("cannot exceed 255 characters"));
    }
    let re = regex::Regex::new(VALID_ID_PATTERN).expect("valid regex");
    if !re.is_match(meeting_id) {
        return Err(AppError::invalid_meeting_id(&format!(
            "must match pattern: {VALID_ID_PATTERN}"
        )));
    }
    Ok(())
}

fn generate_meeting_id() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..12)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// POST /api/v1/meetings
pub async fn create_meeting(
    State(state): State<AppState>,
    AuthUser(email): AuthUser,
    Json(body): Json<CreateMeetingRequest>,
) -> Result<(StatusCode, Json<APIResponse<CreateMeetingResponse>>), AppError> {
    let meeting_id = match &body.meeting_id {
        Some(id) => {
            validate_meeting_id(id)?;
            id.clone()
        }
        None => generate_meeting_id(),
    };

    if body.attendees.len() > MAX_ATTENDEES {
        return Err(AppError::too_many_attendees(
            body.attendees.len(),
            MAX_ATTENDEES,
        ));
    }

    let password_hash = match &body.password {
        Some(pw) if !pw.is_empty() => {
            let salt = argon2::password_hash::SaltString::generate(&mut rand::rngs::OsRng);
            let hash = argon2::Argon2::default()
                .hash_password(pw.as_bytes(), &salt)
                .map_err(|e| AppError::internal(&format!("password hash error: {e}")))?
                .to_string();
            Some(hash)
        }
        _ => None,
    };

    let attendees_json =
        serde_json::to_value(&body.attendees).map_err(|e| AppError::internal(&e.to_string()))?;

    let row = db_meetings::create(
        &state.db,
        &meeting_id,
        &email,
        password_hash.as_deref(),
        &attendees_json,
    )
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            AppError::meeting_exists(&meeting_id)
        }
        other => AppError::from(other),
    })?;

    let response = CreateMeetingResponse {
        meeting_id: row.room_id,
        host: email,
        created_at: row.created_at.timestamp(),
        state: row.state.unwrap_or_else(|| "idle".to_string()),
        attendees: body.attendees,
        has_password: password_hash.is_some(),
    };

    Ok((StatusCode::CREATED, Json(APIResponse::ok(response))))
}

/// GET /api/v1/meetings
pub async fn list_meetings(
    State(state): State<AppState>,
    AuthUser(email): AuthUser,
    Query(params): Query<ListMeetingsQuery>,
) -> Result<Json<APIResponse<ListMeetingsResponse>>, AppError> {
    let limit = params.limit.clamp(1, 100);
    let offset = params.offset.max(0);

    let rows = db_meetings::list_by_owner(&state.db, &email, limit, offset).await?;
    let total = db_meetings::count_by_owner(&state.db, &email).await?;

    let mut meetings = Vec::with_capacity(rows.len());
    for row in &rows {
        let participant_count = db_participants::count_admitted(&state.db, row.id).await?;
        let waiting_count = db_participants::count_waiting(&state.db, row.id).await?;

        meetings.push(MeetingSummary {
            meeting_id: row.room_id.clone(),
            host: row.creator_id.clone(),
            state: row.state.clone().unwrap_or_else(|| "idle".to_string()),
            has_password: row.password_hash.is_some(),
            created_at: row.created_at.timestamp(),
            participant_count,
            started_at: row.started_at.timestamp(),
            ended_at: row.ended_at.map(|t| t.timestamp()),
            waiting_count,
        });
    }

    Ok(Json(APIResponse::ok(ListMeetingsResponse {
        meetings,
        total,
        limit,
        offset,
    })))
}

/// GET /api/v1/meetings/{meeting_id}
pub async fn get_meeting(
    State(state): State<AppState>,
    AuthUser(email): AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<MeetingInfoResponse>>, AppError> {
    let row = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    let your_status = db_participants::get_status(&state.db, row.id, &email).await?;
    let your_status = your_status.map(|p| p.into_participant_status(None));

    Ok(Json(APIResponse::ok(MeetingInfoResponse {
        meeting_id: row.room_id,
        state: row.state.unwrap_or_else(|| "idle".to_string()),
        host: row.creator_id.unwrap_or_default(),
        host_display_name: row.host_display_name,
        has_password: row.password_hash.is_some(),
        your_status,
    })))
}

/// DELETE /api/v1/meetings/{meeting_id}
pub async fn delete_meeting(
    State(state): State<AppState>,
    AuthUser(email): AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<DeleteMeetingResponse>>, AppError> {
    // Check the meeting exists first to distinguish 404 from 403.
    let row = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    if row.creator_id.as_deref() != Some(email.as_str()) {
        return Err(AppError::not_owner());
    }

    db_meetings::soft_delete(&state.db, &meeting_id, &email).await?;

    Ok(Json(APIResponse::ok(DeleteMeetingResponse {
        message: format!("Meeting '{meeting_id}' has been deleted"),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_simple_alphanumeric() {
        assert!(validate_meeting_id("standup2024").is_ok());
    }

    #[test]
    fn validate_accepts_hyphens_and_underscores() {
        assert!(validate_meeting_id("my-meeting_123").is_ok());
    }

    #[test]
    fn validate_rejects_empty_id() {
        let err = validate_meeting_id("").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.body.code, "INVALID_MEETING_ID");
    }

    #[test]
    fn validate_rejects_too_long_id() {
        let long_id = "a".repeat(256);
        let err = validate_meeting_id(&long_id).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_rejects_special_characters() {
        let err = validate_meeting_id("room id with spaces").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_rejects_dots_and_slashes() {
        assert!(validate_meeting_id("../etc/passwd").is_err());
        assert!(validate_meeting_id("room.name").is_err());
    }

    #[test]
    fn generate_produces_12_char_lowercase_alphanumeric() {
        let id = generate_meeting_id();
        assert_eq!(id.len(), 12);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn generated_ids_are_unique() {
        let ids: Vec<String> = (0..100).map(|_| generate_meeting_id()).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        // With 36^12 possibilities, collisions in 100 IDs are astronomically unlikely.
        assert_eq!(unique.len(), 100);
    }

    #[test]
    fn generated_ids_pass_validation() {
        for _ in 0..50 {
            let id = generate_meeting_id();
            assert!(
                validate_meeting_id(&id).is_ok(),
                "Generated ID '{id}' should be valid"
            );
        }
    }
}

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

use crate::search;
use argon2::PasswordHasher;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use rand::Rng;
use videocall_meeting_types::{
    requests::{
        CreateMeetingRequest, ListJoinedMeetingsQuery, ListMeetingsQuery, UpdateMeetingRequest,
    },
    responses::{
        APIResponse, CreateMeetingResponse, DeleteMeetingResponse, JoinedMeetingSummary,
        ListJoinedMeetingsResponse, ListMeetingsResponse, MeetingGuestInfoResponse,
        MeetingInfoResponse, MeetingSummary,
    },
};

use crate::auth::AuthUser;
use crate::db::{meetings as db_meetings, participants as db_participants};
use crate::error::AppError;
use crate::nats_events;
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
    AuthUser { user_id, .. }: AuthUser,
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

    let waiting_room_enabled = body.waiting_room_enabled.unwrap_or(true);
    let admitted_can_admit = body.admitted_can_admit.unwrap_or(false);
    let end_on_host_leave = body.end_on_host_leave.unwrap_or(true);
    let allow_guests = body.allow_guests.unwrap_or(false);

    let row = db_meetings::create_with_options(
        &state.db,
        &meeting_id,
        &user_id,
        password_hash.as_deref(),
        &attendees_json,
        waiting_room_enabled,
        admitted_can_admit,
        end_on_host_leave,
        allow_guests,
    )
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            AppError::meeting_exists(&meeting_id)
        }
        other => AppError::from(other),
    })?;

    // Fire-and-forget push to SearchV2.  See `search::spawn_repush` for the
    // full fire-and-forget contract (no-op when disabled, re-fetches the
    // meeting row, loads the participant roster).
    search::spawn_repush(&state, row.id, row.room_id.clone());

    let response = CreateMeetingResponse {
        meeting_id: row.room_id,
        host: user_id,
        created_at: row.created_at.timestamp(),
        state: row.state.unwrap_or_else(|| "idle".to_string()),
        attendees: body.attendees,
        has_password: password_hash.is_some(),
        waiting_room_enabled: row.waiting_room_enabled,
        admitted_can_admit: row.admitted_can_admit,
        end_on_host_leave: row.end_on_host_leave,
        allow_guests: row.allow_guests,
    };

    Ok((StatusCode::CREATED, Json(APIResponse::ok(response))))
}

/// GET /api/v1/meetings
pub async fn list_meetings(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Query(params): Query<ListMeetingsQuery>,
) -> Result<Json<APIResponse<ListMeetingsResponse>>, AppError> {
    let limit = params.limit.clamp(1, 100);
    let offset = params.offset.max(0);

    let (rows, total) = if let Some(q) = &params.q {
        if !q.trim().is_empty() {
            let rows = db_meetings::search_by_owner(&state.db, &user_id, q, limit, offset).await?;
            let total = db_meetings::count_search_by_owner(&state.db, &user_id, q).await?;
            (rows, total)
        } else {
            let rows = db_meetings::list_by_owner(&state.db, &user_id, limit, offset).await?;
            let total = db_meetings::count_by_owner(&state.db, &user_id).await?;
            (rows, total)
        }
    } else {
        let rows = db_meetings::list_by_owner(&state.db, &user_id, limit, offset).await?;
        let total = db_meetings::count_by_owner(&state.db, &user_id).await?;
        (rows, total)
    };

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
            waiting_room_enabled: row.waiting_room_enabled,
            admitted_can_admit: row.admitted_can_admit,
            end_on_host_leave: row.end_on_host_leave,
            allow_guests: row.allow_guests,
        });
    }

    Ok(Json(APIResponse::ok(ListMeetingsResponse {
        meetings,
        total,
        limit,
        offset,
    })))
}

/// GET /api/v1/meetings/joined
///
/// Returns the most recent meetings the authenticated user has been admitted
/// into, ordered by their last admission time descending. Includes meetings
/// the user owns (which they always have a participant row for once they
/// host-join) as well as meetings owned by others.
///
/// Query parameters:
/// - `limit` (default 5, max 50, must be positive): how many rows to return.
pub async fn list_joined_meetings(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Query(params): Query<ListJoinedMeetingsQuery>,
) -> Result<Json<APIResponse<ListJoinedMeetingsResponse>>, AppError> {
    // Reject negatives explicitly (per the API contract). A positive limit
    // greater than 50 is silently clamped — that's the standard "be generous
    // with input" behaviour the existing list endpoint also follows.
    if params.limit < 1 {
        return Err(AppError::invalid_input(
            "limit must be a positive integer between 1 and 50",
        ));
    }
    let limit = params.limit.min(50);

    let rows = db_meetings::list_joined_by_user(&state.db, &user_id, limit).await?;

    let mut meetings = Vec::with_capacity(rows.len());
    for row in &rows {
        let participant_count = db_participants::count_admitted(&state.db, row.id).await?;
        let waiting_count = db_participants::count_waiting(&state.db, row.id).await?;

        meetings.push(JoinedMeetingSummary {
            meeting_id: row.room_id.clone(),
            state: row.state.clone().unwrap_or_else(|| "idle".to_string()),
            started_at: row.started_at.timestamp_millis(),
            ended_at: row.ended_at.map(|t| t.timestamp_millis()),
            participant_count,
            waiting_count,
            has_password: row.password_hash.is_some(),
            is_owner: row.creator_id.as_deref() == Some(user_id.as_str()),
            last_joined_at: row.last_joined_at.timestamp_millis(),
        });
    }

    let total = meetings.len() as i64;
    Ok(Json(APIResponse::ok(ListJoinedMeetingsResponse {
        meetings,
        total,
    })))
}

/// GET /api/v1/meetings/{meeting_id}
pub async fn get_meeting(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<MeetingInfoResponse>>, AppError> {
    let row = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    let your_status = db_participants::get_status(&state.db, row.id, &user_id).await?;
    let your_status = your_status.map(|p| p.into_participant_status(None));

    let participant_count = db_participants::count_admitted(&state.db, row.id).await?;
    let waiting_count = db_participants::count_waiting(&state.db, row.id).await?;

    Ok(Json(APIResponse::ok(MeetingInfoResponse {
        meeting_id: row.room_id,
        state: row.state.unwrap_or_else(|| "idle".to_string()),
        host: row.creator_id.clone().unwrap_or_default(),
        host_display_name: row.host_display_name,
        host_user_id: row.creator_id,
        has_password: row.password_hash.is_some(),
        waiting_room_enabled: row.waiting_room_enabled,
        admitted_can_admit: row.admitted_can_admit,
        end_on_host_leave: row.end_on_host_leave,
        participant_count,
        waiting_count,
        started_at: row.started_at.timestamp_millis(),
        ended_at: row.ended_at.map(|t| t.timestamp_millis()),
        your_status,
        allow_guests: row.allow_guests,
    })))
}

/// DELETE /api/v1/meetings/{meeting_id}
pub async fn delete_meeting(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<DeleteMeetingResponse>>, AppError> {
    // Check the meeting exists first to distinguish 404 from 403.
    let row = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    if row.creator_id.as_deref() != Some(user_id.as_str()) {
        return Err(AppError::not_owner());
    }

    db_meetings::soft_delete(&state.db, &meeting_id, &user_id).await?;

    // Fire-and-forget: remove from SearchV2
    tokio::spawn({
        let state = state.clone();
        let room_id = meeting_id.clone();
        async move {
            search::delete_meeting_doc(state.search.as_ref(), &state.http_client, &room_id).await;
        }
    });

    Ok(Json(APIResponse::ok(DeleteMeetingResponse {
        message: format!("Meeting '{meeting_id}' has been deleted"),
    })))
}

/// POST /api/v1/meetings/{meeting_id}/end
pub async fn end_meeting_handler(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<MeetingInfoResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    if meeting.creator_id.as_deref() != Some(user_id.as_str()) {
        return Err(AppError::not_owner());
    }

    // Idempotent: if already ended, return the current state.
    if meeting.state.as_deref() == Some("ended") {
        let your_status = db_participants::get_status(&state.db, meeting.id, &user_id).await?;
        let your_status = your_status.map(|p| p.into_participant_status(None));

        let participant_count = db_participants::count_admitted(&state.db, meeting.id).await?;
        let waiting_count = db_participants::count_waiting(&state.db, meeting.id).await?;

        return Ok(Json(APIResponse::ok(MeetingInfoResponse {
            meeting_id: meeting.room_id,
            state: "ended".to_string(),
            host: meeting.creator_id.clone().unwrap_or_default(),
            host_display_name: meeting.host_display_name,
            host_user_id: meeting.creator_id,
            has_password: meeting.password_hash.is_some(),
            waiting_room_enabled: meeting.waiting_room_enabled,
            admitted_can_admit: meeting.admitted_can_admit,
            end_on_host_leave: meeting.end_on_host_leave,
            participant_count,
            waiting_count,
            started_at: meeting.started_at.timestamp_millis(),
            ended_at: meeting.ended_at.map(|t| t.timestamp_millis()),
            your_status,
            allow_guests: meeting.allow_guests,
        })));
    }

    db_meetings::end_meeting(&state.db, meeting.id).await?;

    let row = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    // Fire-and-forget push of the ended state so search results mark the
    // meeting as completed promptly.
    search::spawn_repush(&state, row.id, row.room_id.clone());

    let your_status = db_participants::get_status(&state.db, row.id, &user_id).await?;
    let your_status = your_status.map(|p| p.into_participant_status(None));

    let participant_count = db_participants::count_admitted(&state.db, row.id).await?;
    let waiting_count = db_participants::count_waiting(&state.db, row.id).await?;

    Ok(Json(APIResponse::ok(MeetingInfoResponse {
        meeting_id: row.room_id,
        state: row.state.unwrap_or_else(|| "idle".to_string()),
        host: row.creator_id.clone().unwrap_or_default(),
        host_display_name: row.host_display_name,
        host_user_id: row.creator_id,
        has_password: row.password_hash.is_some(),
        waiting_room_enabled: row.waiting_room_enabled,
        admitted_can_admit: row.admitted_can_admit,
        end_on_host_leave: row.end_on_host_leave,
        participant_count,
        waiting_count,
        started_at: row.started_at.timestamp_millis(),
        ended_at: row.ended_at.map(|t| t.timestamp_millis()),
        your_status,
        allow_guests: row.allow_guests,
    })))
}

/// PATCH /api/v1/meetings/{meeting_id}
pub async fn update_meeting(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
    Json(body): Json<UpdateMeetingRequest>,
) -> Result<Json<APIResponse<MeetingInfoResponse>>, AppError> {
    let settings_updated = body.waiting_room_enabled.is_some()
        || body.admitted_can_admit.is_some()
        || body.end_on_host_leave.is_some()
        || body.allow_guests.is_some();

    let row = if settings_updated {
        // Atomically update both settings within a single transaction.
        // The UPDATE … WHERE creator_id = $2 folds in the ownership check,
        // so we only fetch separately on failure to distinguish 404 vs 403.
        match db_meetings::update_meeting_settings(
            &state.db,
            &meeting_id,
            &user_id,
            body.waiting_room_enabled,
            body.admitted_can_admit,
            body.end_on_host_leave,
            body.allow_guests,
        )
        .await?
        {
            Some(row) => row,
            None => {
                return Err(
                    match db_meetings::get_by_room_id(&state.db, &meeting_id).await? {
                        Some(_) => AppError::not_owner(),
                        None => AppError::meeting_not_found(&meeting_id),
                    },
                );
            }
        }
    } else {
        // No updates requested — fetch and verify ownership.
        let row = db_meetings::get_by_room_id(&state.db, &meeting_id)
            .await?
            .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;
        if row.creator_id.as_deref() != Some(user_id.as_str()) {
            return Err(AppError::not_owner());
        }
        row
    };

    // Fire-and-forget push of the updated settings so search results reflect
    // the new waiting-room / admitted_can_admit state quickly.
    search::spawn_repush(&state, row.id, row.room_id.clone());

    if settings_updated {
        nats_events::publish_meeting_settings_updated(state.nats.as_ref(), &row.room_id).await;
    }

    let your_status = db_participants::get_status(&state.db, row.id, &user_id).await?;
    let your_status = your_status.map(|p| p.into_participant_status(None));

    let participant_count = db_participants::count_admitted(&state.db, row.id).await?;
    let waiting_count = db_participants::count_waiting(&state.db, row.id).await?;

    Ok(Json(APIResponse::ok(MeetingInfoResponse {
        meeting_id: row.room_id,
        state: row.state.unwrap_or_else(|| "idle".to_string()),
        host: row.creator_id.clone().unwrap_or_default(),
        host_display_name: row.host_display_name,
        host_user_id: row.creator_id,
        has_password: row.password_hash.is_some(),
        waiting_room_enabled: row.waiting_room_enabled,
        admitted_can_admit: row.admitted_can_admit,
        end_on_host_leave: row.end_on_host_leave,
        participant_count,
        waiting_count,
        started_at: row.started_at.timestamp_millis(),
        ended_at: row.ended_at.map(|t| t.timestamp_millis()),
        your_status,
        allow_guests: row.allow_guests,
    })))
}

/// GET /api/v1/meetings/{meeting_id}/guest-info
///
/// Public (no authentication required). Returns whether guests are allowed
/// to join this meeting. Used by the frontend to decide whether to redirect
/// an unauthenticated user to the guest join page.
///
/// NOTE: intentionally returns `{ allow_guests: false }` for both missing and
/// guest-disabled meetings rather than a 404, to prevent meeting enumeration
/// by unauthenticated callers.
pub async fn get_meeting_guest_info(
    State(state): State<AppState>,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<MeetingGuestInfoResponse>>, AppError> {
    let allow_guests = match db_meetings::get_by_room_id(&state.db, &meeting_id).await? {
        Some(row) => row.allow_guests,
        None => false,
    };
    Ok(Json(APIResponse::ok(MeetingGuestInfoResponse {
        allow_guests,
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

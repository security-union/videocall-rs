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

//! Handlers for waiting room management: list waiting, admit, admit-all, reject.

use axum::{
    extract::{Path, State},
    Json,
};
use videocall_meeting_types::{
    requests::AdmitRequest,
    responses::{APIResponse, AdmitAllResponse, ParticipantStatusResponse, WaitingRoomResponse},
};

use crate::auth::AuthUser;
use crate::db::{meetings as db_meetings, participants as db_participants};
use crate::error::AppError;
use crate::state::AppState;

/// Verify that the requester is an admitted participant (authorization check).
async fn require_admitted(state: &AppState, meeting_id: i32, email: &str) -> Result<(), AppError> {
    let row = db_participants::get_status(&state.db, meeting_id, email)
        .await?
        .ok_or_else(AppError::not_host)?;

    if row.status != "admitted" {
        return Err(AppError::not_host());
    }
    Ok(())
}

/// GET /api/v1/meetings/{meeting_id}/waiting
pub async fn get_waiting_room(
    State(state): State<AppState>,
    AuthUser { email, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<WaitingRoomResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    require_admitted(&state, meeting.id, &email).await?;

    let rows = db_participants::get_waiting(&state.db, meeting.id).await?;
    let waiting: Vec<ParticipantStatusResponse> = rows
        .into_iter()
        .map(|r| r.into_participant_status(None))
        .collect();

    Ok(Json(APIResponse::ok(WaitingRoomResponse {
        meeting_id,
        waiting,
    })))
}

/// POST /api/v1/meetings/{meeting_id}/admit
pub async fn admit_participant(
    State(state): State<AppState>,
    AuthUser { email, .. }: AuthUser,
    Path(meeting_id): Path<String>,
    Json(body): Json<AdmitRequest>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    require_admitted(&state, meeting.id, &email).await?;

    let row = db_participants::admit(&state.db, meeting.id, &body.email)
        .await?
        .ok_or_else(|| AppError::participant_not_found(&body.email))?;

    // Token is null in the admit response -- the participant picks it up via GET /status.
    Ok(Json(APIResponse::ok(row.into_participant_status(None))))
}

/// POST /api/v1/meetings/{meeting_id}/admit-all
pub async fn admit_all(
    State(state): State<AppState>,
    AuthUser { email, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<AdmitAllResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    require_admitted(&state, meeting.id, &email).await?;

    let rows = db_participants::admit_all(&state.db, meeting.id).await?;
    let admitted_count = rows.len();
    let admitted: Vec<ParticipantStatusResponse> = rows
        .into_iter()
        .map(|r| r.into_participant_status(None))
        .collect();

    Ok(Json(APIResponse::ok(AdmitAllResponse {
        admitted_count,
        admitted,
    })))
}

/// POST /api/v1/meetings/{meeting_id}/reject
pub async fn reject_participant(
    State(state): State<AppState>,
    AuthUser { email, .. }: AuthUser,
    Path(meeting_id): Path<String>,
    Json(body): Json<AdmitRequest>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    require_admitted(&state, meeting.id, &email).await?;

    let row = db_participants::reject(&state.db, meeting.id, &body.email)
        .await?
        .ok_or_else(|| AppError::participant_not_found(&body.email))?;

    Ok(Json(APIResponse::ok(row.into_participant_status(None))))
}

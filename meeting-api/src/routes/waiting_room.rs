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
use crate::nats_events;
use crate::search;
use crate::state::AppState;

/// Verify that the requester is the meeting host, or an admitted participant
/// when `admitted_can_admit` is enabled (authorization check).
async fn require_host_or_can_admit(
    state: &AppState,
    meeting_id: i32,
    user_id: &str,
    admitted_can_admit: bool,
) -> Result<(), AppError> {
    let row = db_participants::get_status(&state.db, meeting_id, user_id)
        .await?
        .ok_or_else(AppError::not_host)?;

    if row.status != "admitted" {
        return Err(AppError::not_host());
    }

    // Host can always manage the waiting room
    if row.is_host {
        return Ok(());
    }

    // Non-host admitted participants can manage if the meeting allows it
    if admitted_can_admit {
        return Ok(());
    }

    Err(AppError::not_host())
}

/// GET /api/v1/meetings/{meeting_id}/waiting
pub async fn get_waiting_room(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<WaitingRoomResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    require_host_or_can_admit(&state, meeting.id, &user_id, meeting.admitted_can_admit).await?;

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
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
    Json(body): Json<AdmitRequest>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    require_host_or_can_admit(&state, meeting.id, &user_id, meeting.admitted_can_admit).await?;

    let row = db_participants::admit(&state.db, meeting.id, &body.user_id)
        .await?
        .ok_or_else(|| AppError::participant_not_found(&body.user_id))?;

    // Notify the admitted participant via NATS. The client will fetch its room
    // token via HTTP after receiving this notification.
    nats_events::publish_participant_admitted(state.nats.as_ref(), &meeting_id, &body.user_id)
        .await;

    // Re-push the meeting doc so SearchV2 picks up the new ACL principal.
    search::spawn_repush(&state, meeting.id, meeting_id.clone());

    Ok(Json(APIResponse::ok(row.into_participant_status(None))))
}

/// POST /api/v1/meetings/{meeting_id}/admit-all
pub async fn admit_all(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<AdmitAllResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    require_host_or_can_admit(&state, meeting.id, &user_id, meeting.admitted_can_admit).await?;

    let rows = db_participants::admit_all(&state.db, meeting.id).await?;
    let admitted_count = rows.len();

    // Notify all admitted participants via NATS in parallel. Clients will fetch
    // their room tokens via HTTP after receiving the notification.
    futures::future::join_all(rows.iter().map(|row| {
        nats_events::publish_participant_admitted(state.nats.as_ref(), &meeting_id, &row.user_id)
    }))
    .await;

    let admitted: Vec<ParticipantStatusResponse> = rows
        .into_iter()
        .map(|r| r.into_participant_status(None))
        .collect();

    // Re-push the meeting doc — potentially many new principals at once.
    if admitted_count > 0 {
        search::spawn_repush(&state, meeting.id, meeting_id.clone());
    }

    Ok(Json(APIResponse::ok(AdmitAllResponse {
        admitted_count,
        admitted,
    })))
}

/// POST /api/v1/meetings/{meeting_id}/reject
pub async fn reject_participant(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
    Json(body): Json<AdmitRequest>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    require_host_or_can_admit(&state, meeting.id, &user_id, meeting.admitted_can_admit).await?;

    let row = db_participants::reject(&state.db, meeting.id, &body.user_id)
        .await?
        .ok_or_else(|| AppError::participant_not_found(&body.user_id))?;

    nats_events::publish_participant_rejected(state.nats.as_ref(), &meeting_id, &body.user_id)
        .await;

    // A rejected user is no longer returned by `list_for_search` — re-push so
    // their principal drops out of the ACL set.
    search::spawn_repush(&state, meeting.id, meeting_id.clone());

    Ok(Json(APIResponse::ok(row.into_participant_status(None))))
}

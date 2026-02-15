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

//! Handlers for participant actions: join, leave, status, list participants.

use axum::{
    extract::{Path, State},
    Json,
};
use videocall_meeting_types::{
    requests::JoinMeetingRequest,
    responses::{APIResponse, ParticipantStatusResponse},
};

use crate::auth::AuthUser;
use crate::db::{meetings as db_meetings, participants as db_participants};
use crate::error::AppError;
use crate::state::AppState;
use crate::token::generate_room_token;

/// POST /api/v1/meetings/{meeting_id}/join
///
/// If the meeting doesn't exist, create it with the joining user as host.
/// Hosts are auto-admitted and receive a room_token immediately.
/// Attendees enter the waiting room.
pub async fn join_meeting(
    State(state): State<AppState>,
    AuthUser { email, .. }: AuthUser,
    Path(meeting_id): Path<String>,
    body: Option<Json<JoinMeetingRequest>>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let display_name = body.as_ref().and_then(|b| b.display_name.as_deref());

    let meeting = match db_meetings::get_by_room_id(&state.db, &meeting_id).await? {
        Some(m) => m,
        None => {
            // Auto-create meeting with this user as host.
            let attendees = serde_json::Value::Array(vec![]);
            db_meetings::create(&state.db, &meeting_id, &email, None, &attendees).await?
        }
    };

    let is_host = meeting.creator_id.as_deref() == Some(email.as_str());

    if is_host {
        // Activate the meeting if it's idle or ended.
        let current_state = meeting.state.as_deref().unwrap_or("idle");
        if current_state != "active" {
            db_meetings::activate(&state.db, meeting.id).await?;
        }

        if let Some(dn) = display_name {
            db_meetings::set_host_display_name(&state.db, meeting.id, dn).await?;
        }

        let row = db_participants::upsert_host(&state.db, meeting.id, &email, display_name).await?;

        let token = generate_room_token(
            &state.jwt_secret,
            state.token_ttl_secs,
            &email,
            &meeting_id,
            true,
            display_name.unwrap_or(&email),
        )?;

        Ok(Json(APIResponse::ok(
            row.into_participant_status(Some(token)),
        )))
    } else {
        // Attendee: must wait for admission if meeting is active.
        let current_state = meeting.state.as_deref().unwrap_or("idle");
        if current_state != "active" {
            return Err(AppError::meeting_not_active(&meeting_id));
        }

        let row =
            db_participants::upsert_attendee(&state.db, meeting.id, &email, display_name).await?;

        Ok(Json(APIResponse::ok(row.into_participant_status(None))))
    }
}

/// GET /api/v1/meetings/{meeting_id}/status
///
/// Polling endpoint. When status is 'admitted', the response includes the room_token.
pub async fn get_my_status(
    State(state): State<AppState>,
    AuthUser { email, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    // Refuse to issue tokens for ended meetings.
    if meeting.state.as_deref() == Some("ended") {
        return Err(AppError::meeting_not_active(&meeting_id));
    }

    let row = db_participants::get_status(&state.db, meeting.id, &email)
        .await?
        .ok_or_else(AppError::not_in_meeting)?;

    let token = if row.status == "admitted" {
        Some(generate_room_token(
            &state.jwt_secret,
            state.token_ttl_secs,
            &email,
            &meeting_id,
            row.is_host,
            row.display_name.as_deref().unwrap_or(&email),
        )?)
    } else {
        None
    };

    Ok(Json(APIResponse::ok(row.into_participant_status(token))))
}

/// POST /api/v1/meetings/{meeting_id}/leave
pub async fn leave_meeting(
    State(state): State<AppState>,
    AuthUser { email, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    let row = db_participants::leave(&state.db, meeting.id, &email)
        .await?
        .ok_or_else(AppError::not_in_meeting)?;

    // If host left or no admitted participants remain, end the meeting.
    let is_host = meeting.creator_id.as_deref() == Some(email.as_str());
    if is_host {
        db_meetings::end_meeting(&state.db, meeting.id).await?;
    } else {
        let remaining = db_participants::count_admitted(&state.db, meeting.id).await?;
        if remaining == 0 {
            db_meetings::end_meeting(&state.db, meeting.id).await?;
        }
    }

    Ok(Json(APIResponse::ok(row.into_participant_status(None))))
}

/// GET /api/v1/meetings/{meeting_id}/participants
///
/// Only participants who are themselves in the meeting can list other participants.
pub async fn get_participants(
    State(state): State<AppState>,
    AuthUser { email, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<Vec<ParticipantStatusResponse>>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    // Verify the requester is actually a participant in this meeting.
    db_participants::get_status(&state.db, meeting.id, &email)
        .await?
        .ok_or_else(AppError::not_in_meeting)?;

    let rows = db_participants::get_admitted(&state.db, meeting.id).await?;
    let participants: Vec<ParticipantStatusResponse> = rows
        .into_iter()
        .map(|r| r.into_participant_status(None))
        .collect();

    Ok(Json(APIResponse::ok(participants)))
}

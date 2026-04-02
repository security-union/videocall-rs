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
use chrono::Utc;
use videocall_meeting_types::{
    requests::{JoinMeetingRequest, UpdateDisplayNameRequest},
    responses::{APIResponse, ParticipantStatusResponse},
};

use crate::auth::AuthUser;
use crate::db::{meetings as db_meetings, participants as db_participants};
use crate::error::AppError;
use crate::nats_events;
use crate::state::AppState;
use crate::token::{generate_observer_token, generate_room_token};

/// POST /api/v1/meetings/{meeting_id}/join
///
/// If the meeting doesn't exist, create it with the joining user as host.
/// Hosts are auto-admitted and receive a room_token immediately.
/// Attendees enter the waiting room.
pub async fn join_meeting(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
    body: Option<Json<JoinMeetingRequest>>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let display_name = body.as_ref().and_then(|b| b.display_name.as_deref());

    let meeting = match db_meetings::get_by_room_id(&state.db, &meeting_id).await? {
        Some(m) => m,
        None => {
            // Auto-create meeting with this user as host.
            let attendees = serde_json::Value::Array(vec![]);
            db_meetings::create(&state.db, &meeting_id, &user_id, None, &attendees).await?
        }
    };

    let is_host = meeting.creator_id.as_deref() == Some(user_id.as_str());

    if is_host {
        // Activate the meeting if it's idle or ended.
        let current_state = meeting.state.as_deref().unwrap_or("idle");
        if current_state != "active" {
            db_meetings::activate(&state.db, meeting.id).await?;
            nats_events::publish_meeting_activated(state.nats.as_ref(), &meeting_id).await;
        }

        if let Some(dn) = display_name {
            db_meetings::set_host_display_name(&state.db, meeting.id, dn).await?;
        }

        let row =
            db_participants::upsert_host(&state.db, meeting.id, &user_id, display_name).await?;

        let token = generate_room_token(
            &state.jwt_secret,
            state.token_ttl_secs,
            &user_id,
            &meeting_id,
            true,
            display_name.unwrap_or(&user_id),
        )?;

        let mut resp = row.into_participant_status(Some(token));
        resp.waiting_room_enabled = Some(meeting.waiting_room_enabled);
        resp.admitted_can_admit = Some(meeting.admitted_can_admit);
        resp.host_display_name = display_name.map(String::from).or(meeting.host_display_name);
        resp.host_user_id = meeting.creator_id;
        Ok(Json(APIResponse::ok(resp)))
    } else {
        // Attendee: must wait for admission if meeting is active.
        let current_state = meeting.state.as_deref().unwrap_or("idle");
        if current_state != "active" {
            // Meeting exists but isn't active yet. Return a "waiting_for_meeting"
            // status with an observer token so the client can receive a push
            // notification when the host activates the meeting.
            let dn = display_name.unwrap_or(&user_id);
            let observer = generate_observer_token(&state.jwt_secret, &user_id, &meeting_id, dn)?;
            let resp = ParticipantStatusResponse {
                user_id: user_id.clone(),
                display_name: display_name.map(String::from),
                status: "waiting_for_meeting".to_string(),
                is_host: false,
                joined_at: Utc::now().timestamp(),
                admitted_at: None,
                room_token: None,
                observer_token: Some(observer),
                waiting_room_enabled: Some(meeting.waiting_room_enabled),
                admitted_can_admit: Some(meeting.admitted_can_admit),
                host_display_name: meeting.host_display_name,
                host_user_id: meeting.creator_id,
            };
            return Ok(Json(APIResponse::ok(resp)));
        }

        // Atomically check waiting_room_enabled and insert participant in one
        // transaction, using FOR UPDATE to serialize against concurrent toggles.
        let (auto_admitted, row, waiting_room_enabled) =
            db_participants::join_attendee(&state.db, meeting.id, &user_id, display_name).await?;

        let token = if auto_admitted {
            Some(generate_room_token(
                &state.jwt_secret,
                state.token_ttl_secs,
                &user_id,
                &meeting_id,
                false,
                display_name.unwrap_or(&user_id),
            )?)
        } else {
            None
        };

        let mut resp = row.into_participant_status(token);
        // When the participant is placed in the waiting room (not auto-admitted),
        // include an observer token so they can receive push notifications.
        if !auto_admitted {
            let dn = display_name.unwrap_or(&user_id);
            resp.observer_token = Some(generate_observer_token(
                &state.jwt_secret,
                &user_id,
                &meeting_id,
                dn,
            )?);
            // Notify the host that the waiting room list has changed.
            nats_events::publish_waiting_room_updated(state.nats.as_ref(), &meeting_id).await;
        }
        resp.waiting_room_enabled = Some(waiting_room_enabled);
        resp.admitted_can_admit = Some(meeting.admitted_can_admit);
        resp.host_display_name = meeting.host_display_name;
        resp.host_user_id = meeting.creator_id;
        Ok(Json(APIResponse::ok(resp)))
    }
}

/// POST /api/v1/meetings/{meeting_id}/join_guest
///
/// Allows a guest user (non-authenticated) to join a meeting if guests are allowed.
pub async fn join_meeting_as_guest(
    State(state): State<AppState>,
    Path(meeting_id): Path<String>,
    body: Json<JoinMeetingRequest>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let display_name = body.display_name.as_deref().unwrap_or("Guest");

    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    if !meeting.allow_guests {
        return Err(AppError::guests_not_allowed(&meeting_id));
    }

    // Guests are treated as attendees but without a user_id. Use a special
    // "guest:{uuid}" format for the participant record and tokens.
    let guest_user_id = format!(
        "guest:{}",
        (uuid::Uuid::new_v4().as_u128() & 0xffffffffffffffff) as u64
    );

    // Atomically check waiting_room_enabled and insert participant in one
    // transaction, using FOR UPDATE to serialize against concurrent toggles.
    let (auto_admitted, row, waiting_room_enabled) =
        db_participants::join_attendee(&state.db, meeting.id, &guest_user_id, Some(display_name))
            .await?;

    let token = if auto_admitted {
        Some(generate_room_token(
            &state.jwt_secret,
            state.token_ttl_secs,
            &guest_user_id,
            &meeting_id,
            false,
            display_name,
        )?)
    } else {
        None
    };

    let mut resp = row.into_participant_status(token);
    // When the participant is placed in the waiting room (not auto-admitted),
    // include an observer token so they can receive push notifications.
    if !auto_admitted {
        resp.observer_token = Some(generate_observer_token(
            &state.jwt_secret,
            &guest_user_id,
            &meeting_id,
            display_name,
        )?);
        // Notify the host that the waiting room list has changed.
        nats_events::publish_waiting_room_updated(state.nats.as_ref(), &meeting_id).await;
    }
    resp.waiting_room_enabled = Some(waiting_room_enabled);
    resp.admitted_can_admit = Some(meeting.admitted_can_admit);
    resp.host_display_name = meeting.host_display_name;
    resp.host_user_id = meeting.creator_id;
    Ok(Json(APIResponse::ok(resp)))
}

/// GET /api/v1/meetings/{meeting_id}/status
///
/// Polling endpoint. When status is 'admitted', the response includes the room_token.
pub async fn get_my_status(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    // Refuse to issue tokens for ended meetings.
    if meeting.state.as_deref() == Some("ended") {
        return Err(AppError::meeting_not_active(&meeting_id));
    }

    let row = db_participants::get_status(&state.db, meeting.id, &user_id)
        .await?
        .ok_or_else(AppError::not_in_meeting)?;

    let token = if row.status == "admitted" {
        Some(generate_room_token(
            &state.jwt_secret,
            state.token_ttl_secs,
            &user_id,
            &meeting_id,
            row.is_host,
            row.display_name.as_deref().unwrap_or(&user_id),
        )?)
    } else {
        None
    };

    let mut resp = row.into_participant_status(token);
    resp.waiting_room_enabled = Some(meeting.waiting_room_enabled);
    resp.admitted_can_admit = Some(meeting.admitted_can_admit);
    resp.host_display_name = meeting.host_display_name;
    resp.host_user_id = meeting.creator_id;
    Ok(Json(APIResponse::ok(resp)))
}

/// POST /api/v1/meetings/{meeting_id}/leave
pub async fn leave_meeting(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    let row = db_participants::leave(&state.db, meeting.id, &user_id)
        .await?
        .ok_or_else(AppError::not_in_meeting)?;

    // If host left or no admitted participants remain, end the meeting.
    let is_host = meeting.creator_id.as_deref() == Some(user_id.as_str());
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

/// PUT /api/v1/meetings/{meeting_id}/display-name
///
/// Update the participant's display name during an active meeting.
/// Broadcasts the change to all connected participants via NATS.
pub async fn update_display_name(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
    Json(body): Json<UpdateDisplayNameRequest>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    // Validate the participant exists and is in the meeting
    db_participants::get_status(&state.db, meeting.id, &user_id)
        .await?
        .ok_or_else(AppError::not_in_meeting)?;

    // Update the display name in the database
    let updated_row =
        db_participants::update_display_name(&state.db, meeting.id, &user_id, &body.display_name)
            .await?
            .ok_or_else(AppError::not_in_meeting)?;

    // Broadcast the display name change to all participants in the meeting
    nats_events::publish_participant_display_name_changed(
        state.nats.as_ref(),
        &meeting_id,
        &user_id,
        &body.display_name,
    )
    .await;

    // Return the updated participant status
    Ok(Json(APIResponse::ok(
        updated_row.into_participant_status(None),
    )))
}

/// GET /api/v1/meetings/{meeting_id}/participants
///
/// Only participants who are themselves in the meeting can list other participants.
pub async fn get_participants(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<Vec<ParticipantStatusResponse>>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    // Verify the requester is actually a participant in this meeting.
    db_participants::get_status(&state.db, meeting.id, &user_id)
        .await?
        .ok_or_else(AppError::not_in_meeting)?;

    let rows = db_participants::get_admitted(&state.db, meeting.id).await?;
    let participants: Vec<ParticipantStatusResponse> = rows
        .into_iter()
        .map(|r| r.into_participant_status(None))
        .collect();

    Ok(Json(APIResponse::ok(participants)))
}

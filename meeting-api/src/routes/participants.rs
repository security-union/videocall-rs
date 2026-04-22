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
    requests::{GuestJoinRequest, JoinMeetingRequest, UpdateDisplayNameRequest},
    responses::{APIResponse, ParticipantStatusResponse},
};

use crate::auth::AuthUser;
use crate::auth::GuestObserver;
use crate::db::meetings::MeetingRow;
use crate::db::{meetings as db_meetings, participants as db_participants};
use crate::error::AppError;
use crate::nats_events;
use crate::state::AppState;
use crate::token::{generate_observer_token, generate_room_token};
use videocall_types::validation::validate_display_name;

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
    let display_name = body
        .as_ref()
        .and_then(|b| b.display_name.as_deref())
        .map(|raw| validate_display_name(raw).map_err(|e| AppError::invalid_display_name(&e)))
        .transpose()?;
    let display_name = display_name.as_deref();

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
            meeting.end_on_host_leave,
            false,
        )?;

        let mut resp = row.into_participant_status(Some(token));
        resp.waiting_room_enabled = Some(meeting.waiting_room_enabled);
        resp.admitted_can_admit = Some(meeting.admitted_can_admit);
        resp.end_on_host_leave = Some(meeting.end_on_host_leave);
        resp.allow_guests = Some(meeting.allow_guests);
        resp.host_display_name = display_name.map(String::from).or(meeting.host_display_name);
        resp.host_user_id = meeting.creator_id;
        Ok(Json(APIResponse::ok(resp)))
    } else {
        join_as_attendee(
            &state,
            meeting,
            &user_id,
            &meeting_id,
            display_name,
            &user_id,
            false,
        )
        .await
    }
}

/// Shared attendee join logic used by both [`join_meeting`] (non-host path)
/// and [`join_meeting_as_guest`].
///
/// Handles the "waiting_for_meeting" early-return when the meeting is not yet
/// active, the waiting-room / auto-admit flow, and observer-token generation.
async fn join_as_attendee(
    state: &AppState,
    meeting: MeetingRow,
    user_id: &str,
    meeting_id: &str,
    display_name: Option<&str>,
    fallback_display_name: &str,
    is_guest: bool,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let is_host = meeting.creator_id.as_deref() == Some(user_id);
    let current_state = meeting.state.as_deref().unwrap_or("idle");
    if current_state != "active" {
        if !meeting.waiting_room_enabled {
            // No waiting room: auto-activate the meeting and admit
            // a non-host joiner so they can wait inside the call.
            db_meetings::activate(&state.db, meeting.id).await?;
            nats_events::publish_meeting_activated(state.nats.as_ref(), meeting_id).await;
            let (auto_admitted, row, wr_enabled) =
                db_participants::join_attendee(&state.db, meeting.id, user_id, display_name, None, is_guest)
                    .await?
                    .expect("join_attendee with None host check never returns None");
            let token = if auto_admitted {
                Some(generate_room_token(
                    &state.jwt_secret,
                    state.token_ttl_secs,
                    user_id,
                    meeting_id,
                    is_host,
                    display_name.unwrap_or(fallback_display_name),
                    meeting.end_on_host_leave,
                    is_guest,
                )?)
            } else {
                None
            };
            let mut resp = row.into_participant_status(token);
            if !auto_admitted {
                let dn = display_name.unwrap_or(fallback_display_name);
                resp.observer_token = Some(generate_observer_token(
                    &state.jwt_secret,
                    user_id,
                    meeting_id,
                    dn,
                    is_guest,
                )?);
                nats_events::publish_waiting_room_updated(state.nats.as_ref(), meeting_id).await;
            }
            resp.waiting_room_enabled = Some(wr_enabled);
            resp.admitted_can_admit = Some(meeting.admitted_can_admit);
            resp.end_on_host_leave = Some(meeting.end_on_host_leave);
            resp.allow_guests = Some(meeting.allow_guests);
            resp.host_display_name = meeting.host_display_name;
            resp.host_user_id = meeting.creator_id;
            return Ok(Json(APIResponse::ok(resp)));
        }

        // Meeting exists but isn't active yet. Return a "waiting_for_meeting"
        // status with an observer token so the client can receive a push
        // notification when the host activates the meeting.
        let dn = display_name.unwrap_or(fallback_display_name);
        let observer =
            generate_observer_token(&state.jwt_secret, user_id, meeting_id, dn, is_guest)?;
        let resp = ParticipantStatusResponse {
            is_guest,
            user_id: user_id.to_string(),
            display_name: display_name.map(String::from),
            status: "waiting_for_meeting".to_string(),
            is_host: false,
            joined_at: Utc::now().timestamp(),
            admitted_at: None,
            room_token: None,
            observer_token: Some(observer),
            waiting_room_enabled: Some(meeting.waiting_room_enabled),
            admitted_can_admit: Some(meeting.admitted_can_admit),
            end_on_host_leave: Some(meeting.end_on_host_leave),
            host_display_name: meeting.host_display_name,
            host_user_id: meeting.creator_id,
            allow_guests: Some(meeting.allow_guests),
        };
        return Ok(Json(APIResponse::ok(resp)));
    }

    // Pass creator_id only when the host-gone check must be enforced.
    // Folding this check into the transaction closes the TOCTOU window where
    // concurrent requests could both pass an out-of-transaction host-status read.
    let check_creator = if !meeting.end_on_host_leave && !meeting.admitted_can_admit {
        meeting.creator_id.as_deref()
    } else {
        None
    };

    // Atomically check waiting_room_enabled and insert participant in one
    // transaction, using FOR UPDATE to serialize against concurrent toggles.
    let (auto_admitted, row, waiting_room_enabled) = match db_participants::join_attendee(
        &state.db,
        meeting.id,
        user_id,
        display_name,
        check_creator,
        is_guest,
    )
    .await?
    {
        Some(r) => r,
        None => return Err(AppError::joining_not_allowed()),
    };

    let token = if auto_admitted {
        Some(generate_room_token(
            &state.jwt_secret,
            state.token_ttl_secs,
            user_id,
            meeting_id,
            is_host,
            display_name.unwrap_or(fallback_display_name),
            meeting.end_on_host_leave,
            is_guest,
        )?)
    } else {
        None
    };

    let mut resp = row.into_participant_status(token);
    // When the participant is placed in the waiting room (not auto-admitted),
    // include an observer token so they can receive push notifications.
    if !auto_admitted {
        let dn = display_name.unwrap_or(fallback_display_name);
        resp.observer_token = Some(generate_observer_token(
            &state.jwt_secret,
            user_id,
            meeting_id,
            dn,
            is_guest,
        )?);
        // Notify the host that the waiting room list has changed.
        nats_events::publish_waiting_room_updated(state.nats.as_ref(), meeting_id).await;
    }
    resp.waiting_room_enabled = Some(waiting_room_enabled);
    resp.admitted_can_admit = Some(meeting.admitted_can_admit);
    resp.end_on_host_leave = Some(meeting.end_on_host_leave);
    resp.allow_guests = Some(meeting.allow_guests);
    resp.host_display_name = meeting.host_display_name;
    resp.host_user_id = meeting.creator_id;
    Ok(Json(APIResponse::ok(resp)))
}

/// POST /api/v1/meetings/{meeting_id}/join-guest
///
/// Allows a guest user (non-authenticated) to join a meeting if guests are allowed.
pub async fn join_meeting_as_guest(
    State(state): State<AppState>,
    Path(meeting_id): Path<String>,
    body: Json<GuestJoinRequest>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let display_name =
        validate_display_name(&body.display_name).map_err(|e| AppError::invalid_input(&e))?;
    let display_name = display_name.as_str();

    let meeting = match db_meetings::get_by_room_id(&state.db, &meeting_id).await? {
        Some(m) if m.allow_guests => m,
        _ => return Err(AppError::guests_not_allowed()),
    };

    // Guests are treated as attendees but without a user_id. Use a special
    // "guest:{uuid}" format for the participant record and tokens.
    // If the client provided a stable guest_session_id from a previous join,
    // reuse it.
    let guest_user_id = match body.guest_session_id.as_deref() {
        Some(id)
            if id.starts_with(videocall_meeting_types::GUEST_USER_ID_PREFIX)
                && id
                    .strip_prefix(videocall_meeting_types::GUEST_USER_ID_PREFIX)
                    .and_then(|s| uuid::Uuid::parse_str(s).ok())
                    .is_some() =>
        {
            id.to_string()
        }
        _ => format!(
            "{}{}",
            videocall_meeting_types::GUEST_USER_ID_PREFIX,
            uuid::Uuid::new_v4()
        ),
    };

    join_as_attendee(
        &state,
        meeting,
        &guest_user_id,
        &meeting_id,
        Some(display_name),
        "Guest",
        true,
    )
    .await
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
            meeting.end_on_host_leave,
            false,
        )?)
    } else {
        None
    };

    let mut resp = row.into_participant_status(token);
    resp.waiting_room_enabled = Some(meeting.waiting_room_enabled);
    resp.admitted_can_admit = Some(meeting.admitted_can_admit);
    resp.end_on_host_leave = Some(meeting.end_on_host_leave);
    resp.allow_guests = Some(meeting.allow_guests);
    resp.host_display_name = meeting.host_display_name;
    resp.host_user_id = meeting.creator_id;
    Ok(Json(APIResponse::ok(resp)))
}

/// GET /api/v1/meetings/{meeting_id}/guest-status
///
/// Polling endpoint for guests (unauthenticated users who joined via
/// `join-guest`). Authenticates via an observer JWT Bearer token.
/// When `status == "admitted"` the response includes a fresh `room_token`.
pub async fn get_guest_status(
    State(state): State<AppState>,
    GuestObserver {
        user_id,
        meeting_id: token_meeting_id,
        ..
    }: GuestObserver,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    // Reject cross-meeting token reuse: the observer token must have been
    // issued for exactly this meeting, not a different one.
    if token_meeting_id != meeting_id {
        return Err(AppError::unauthorized_msg(
            "observer token is not valid for this meeting",
        ));
    }

    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

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
            false,
            row.display_name.as_deref().unwrap_or(&user_id),
            meeting.end_on_host_leave,
            true,
        )?)
    } else {
        None
    };

    let mut resp = row.into_participant_status(token);
    resp.waiting_room_enabled = Some(meeting.waiting_room_enabled);
    resp.admitted_can_admit = Some(meeting.admitted_can_admit);
    resp.allow_guests = Some(meeting.allow_guests);
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

    // End the meeting when the host leaves only if end_on_host_leave is set,
    // otherwise continue until the last participant leaves.
    let is_host = meeting.creator_id.as_deref() == Some(user_id.as_str());
    if is_host && meeting.end_on_host_leave {
        db_meetings::end_meeting(&state.db, meeting.id).await?;
    } else {
        let remaining = db_participants::count_admitted(&state.db, meeting.id).await?;
        if remaining == 0 {
            db_meetings::end_meeting(&state.db, meeting.id).await?;
        }
    }

    Ok(Json(APIResponse::ok(row.into_participant_status(None))))
}

/// POST /api/v1/meetings/{meeting_id}/leave-guest
///
/// Allows a guest participant to cleanly leave and remove their row from the
/// waiting room.
pub async fn leave_meeting_as_guest(
    State(state): State<AppState>,
    GuestObserver {
        user_id,
        meeting_id: token_meeting_id,
        ..
    }: GuestObserver,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    if token_meeting_id != meeting_id {
        return Err(AppError::unauthorized_msg(
            "observer token is not valid for this meeting",
        ));
    }

    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    let row = match db_participants::leave(&state.db, meeting.id, &user_id).await? {
        Some(r) => r,
        None => return Err(AppError::not_in_meeting()),
    };

    // Notify the host that the waiting room list changed.
    nats_events::publish_waiting_room_updated(state.nats.as_ref(), &meeting_id).await;

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

    // Validate the display name
    let validated_name = validate_display_name(&body.display_name)
        .map_err(|e| AppError::invalid_display_name(&e))?;

    // Validate the participant exists and is in the meeting
    db_participants::get_status(&state.db, meeting.id, &user_id)
        .await?
        .ok_or_else(AppError::not_in_meeting)?;

    // Update the display name in the database
    let updated_row =
        db_participants::update_display_name(&state.db, meeting.id, &user_id, &validated_name)
            .await?
            .ok_or_else(AppError::not_in_meeting)?;

    // Broadcast the display name change to all participants in the meeting
    nats_events::publish_participant_display_name_changed(
        state.nats.as_ref(),
        &meeting_id,
        &user_id,
        &validated_name,
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

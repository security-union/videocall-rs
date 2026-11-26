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
    requests::JoinMeetingRequest,
    responses::{APIResponse, ParticipantStatusResponse},
};

use crate::auth::{AuthUser, AuthVia, CliOrBrowserUser};
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
///
/// CLI credentials ([`AuthVia::Cli`]) never host: a missing meeting is 404
/// (no auto-create), a non-active meeting is never activated (the creator
/// gets the same `waiting_for_meeting` answer as anyone else), the host
/// display name is never touched, and the room token always has
/// `is_host = false`. A creator joining their own *active* meeting via the
/// CLI is still admitted without the waiting room.
///
/// Display names for CLI callers: the participant row is shared with the
/// same account's browser session, so a CLI never replaces a name already
/// stored there (it only fills an empty one). The name the CLI asked for
/// goes into the room token it gets directly from this call; without one,
/// the account's name is used, never the user id.
pub async fn join_meeting(
    State(state): State<AppState>,
    CliOrBrowserUser { user_id, name, via }: CliOrBrowserUser,
    Path(meeting_id): Path<String>,
    body: Option<Json<JoinMeetingRequest>>,
) -> Result<Json<APIResponse<ParticipantStatusResponse>>, AppError> {
    let display_name = body.as_ref().and_then(|b| b.display_name.as_deref());
    // The name used when nobody chose one: the account name for a CLI
    // device, the user id for a browser (unchanged behaviour).
    let fallback_name = fallback_display_name(via, &user_id, &name);

    let meeting = match db_meetings::get_by_room_id(&state.db, &meeting_id).await? {
        Some(m) => m,
        None if via == AuthVia::Cli => return Err(AppError::meeting_not_found(&meeting_id)),
        None => {
            // Auto-create meeting with this user as host.
            let attendees = serde_json::Value::Array(vec![]);
            db_meetings::create(&state.db, &meeting_id, &user_id, None, &attendees).await?
        }
    };

    let is_creator = meeting.creator_id.as_deref() == Some(user_id.as_str());
    let is_host = is_creator && via == AuthVia::Browser;
    let meeting_active = meeting.state.as_deref() == Some("active");

    if is_creator && via == AuthVia::Cli && meeting_active {
        // Creator on the CLI: admitted without the waiting room, never host.
        let row =
            db_participants::upsert_creator_via_cli(&state.db, meeting.id, &user_id, display_name)
                .await?;
        let token = generate_room_token(
            &state.jwt_secret,
            room_token_ttl(&state, via),
            &user_id,
            &meeting_id,
            false,
            display_name
                .or(row.display_name.as_deref())
                .unwrap_or(fallback_name),
        )?;
        let mut resp = row.into_participant_status(Some(token));
        resp.is_host = false;
        resp.waiting_room_enabled = Some(meeting.waiting_room_enabled);
        resp.host_display_name = meeting.host_display_name;
        resp.host_user_id = meeting.creator_id;
        return Ok(Json(APIResponse::ok(resp)));
    }

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
            let dn = display_name.unwrap_or(fallback_name);
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
                host_display_name: meeting.host_display_name,
                host_user_id: meeting.creator_id,
            };
            return Ok(Json(APIResponse::ok(resp)));
        }

        // Atomically check waiting_room_enabled and insert participant in one
        // transaction, using FOR UPDATE to serialize against concurrent toggles.
        let (auto_admitted, row, waiting_room_enabled) = db_participants::join_attendee(
            &state.db,
            meeting.id,
            &user_id,
            display_name,
            via == AuthVia::Cli,
        )
        .await?;
        // The CLI's requested name wins in its own token; the stored row
        // (shared with the browser) is not renamed.
        let token_name = match via {
            AuthVia::Cli => display_name
                .or(row.display_name.as_deref())
                .unwrap_or(fallback_name),
            AuthVia::Browser => display_name.unwrap_or(fallback_name),
        }
        .to_string();

        let token = if auto_admitted {
            Some(generate_room_token(
                &state.jwt_secret,
                room_token_ttl(&state, via),
                &user_id,
                &meeting_id,
                false,
                &token_name,
            )?)
        } else {
            None
        };

        let mut resp = row.into_participant_status(token);
        // When the participant is placed in the waiting room (not auto-admitted),
        // include an observer token so they can receive push notifications.
        if !auto_admitted {
            let dn = &token_name;
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
        resp.host_display_name = meeting.host_display_name;
        resp.host_user_id = meeting.creator_id;
        Ok(Json(APIResponse::ok(resp)))
    }
}

/// GET /api/v1/meetings/{meeting_id}/status
///
/// Polling endpoint. When status is 'admitted', the response includes the room_token.
///
/// CLI credentials always get a room token with `is_host = false`.
pub async fn get_my_status(
    State(state): State<AppState>,
    CliOrBrowserUser { user_id, name, via }: CliOrBrowserUser,
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
            room_token_ttl(&state, via),
            &user_id,
            &meeting_id,
            row.is_host && via == AuthVia::Browser,
            row.display_name
                .as_deref()
                .unwrap_or(fallback_display_name(via, &user_id, &name)),
        )?)
    } else {
        None
    };

    let mut resp = row.into_participant_status(token);
    if via == AuthVia::Cli {
        resp.is_host = false;
    }
    resp.waiting_room_enabled = Some(meeting.waiting_room_enabled);
    resp.host_display_name = meeting.host_display_name;
    resp.host_user_id = meeting.creator_id;
    Ok(Json(APIResponse::ok(resp)))
}

/// Lifetime of a room token issued to a CLI device. Short so that revoking
/// a device stops it reaching the relay within minutes: the CLI asks for a
/// fresh token on every connect attempt.
pub const CLI_ROOM_TOKEN_TTL_SECS: i64 = 300;

/// Room-token lifetime for this caller: [`CLI_ROOM_TOKEN_TTL_SECS`] for a CLI
/// device, the configured `TOKEN_TTL_SECS` for a browser (unchanged).
fn room_token_ttl(state: &AppState, via: AuthVia) -> i64 {
    match via {
        AuthVia::Cli => CLI_ROOM_TOKEN_TTL_SECS.min(state.token_ttl_secs),
        AuthVia::Browser => state.token_ttl_secs,
    }
}

/// The display name used when none was chosen or stored: a CLI device
/// shows its account's name; a browser keeps the old user-id fallback.
fn fallback_display_name<'a>(via: AuthVia, user_id: &'a str, account_name: &'a str) -> &'a str {
    match via {
        AuthVia::Cli if !account_name.trim().is_empty() => account_name,
        _ => user_id,
    }
}

/// POST /api/v1/meetings/{meeting_id}/leave
///
/// Browser session only: leaving as the creator ends the meeting, and a CLI
/// shares its participant row with the same account's browser session.
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

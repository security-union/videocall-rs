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
use crate::search;
use crate::state::AppState;
use crate::token::{generate_observer_token, generate_room_token};
use videocall_types::validation::validate_display_name;

/// Maximum display-name changes allowed per window.
const MAX_DISPLAY_NAME_RENAMES: u32 = 5;
/// Window duration in seconds for display-name rate limiting.
const DISPLAY_NAME_WINDOW_SECS: u64 = 60;
/// Sweep cadence for stale limiter entries (every N rate-limit checks).
const RATE_LIMIT_SWEEP_EVERY_OPS: u64 = 64;

/// Returns `true` when the new display name differs from the current one
/// (or no current name exists).
fn is_name_changing(current: Option<&str>, new: &str) -> bool {
    current.map(|c| c != new).unwrap_or(true)
}

/// Shared rate-limit check for display-name changes.
/// Evicts stale entries, then enforces `MAX_DISPLAY_NAME_RENAMES` per
/// `DISPLAY_NAME_WINDOW_SECS` per user. Returns `Ok(())` if allowed.
async fn enforce_display_name_rate_limit(state: &AppState, user_id: &str) -> Result<(), AppError> {
    let sweep_tick = state
        .display_name_rate_limiter_ops
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;

    let mut limiter = state
        .display_name_rate_limiter
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if sweep_tick.is_multiple_of(RATE_LIMIT_SWEEP_EVERY_OPS) {
        limiter.retain(|_, (window_start, _)| {
            window_start.elapsed().as_secs() < DISPLAY_NAME_WINDOW_SECS
        });
    }

    let entry = limiter
        .entry(user_id.to_owned())
        .or_insert_with(|| (std::time::Instant::now(), 0));

    // Even when a periodic sweep hasn't run yet, stale per-user windows must
    // reset immediately on access.
    if entry.0.elapsed().as_secs() >= DISPLAY_NAME_WINDOW_SECS {
        *entry = (std::time::Instant::now(), 0);
    }

    if entry.1 >= MAX_DISPLAY_NAME_RENAMES {
        return Err(AppError::rate_limit_exceeded());
    }
    entry.1 += 1;
    Ok(())
}

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
        .map(|raw| validate_display_name(raw).map_err(|_| AppError::invalid_display_name()))
        .transpose()?;
    let display_name = display_name.as_deref();

    // Rate-limit display-name changes via the join path to prevent
    // leave+rejoin bypass of the rename rate limiter.
    if display_name.is_some() {
        enforce_display_name_rate_limit(&state, &user_id).await?;
    }

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

        // Host row inserted/updated — refresh the SearchV2 doc so the
        // creator appears in acls/participants even on a fresh meeting.
        search::spawn_repush(&state, meeting.id, meeting_id.clone());

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
        resp.waiting_room_enabled = meeting.waiting_room_enabled;
        resp.admitted_can_admit = meeting.admitted_can_admit;
        resp.end_on_host_leave = meeting.end_on_host_leave;
        resp.allow_guests = meeting.allow_guests;
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
            let (auto_admitted, row, wr_enabled) = db_participants::join_attendee(
                &state.db,
                meeting.id,
                user_id,
                display_name,
                None,
                is_guest,
            )
            .await?
            .ok_or_else(|| {
                AppError::internal(
                    "join_attendee returned None despite no host-check — internal invariant violated",
                )
            })?;
            // New attendee row added — may be `admitted` or `waiting`,
            // both of which are indexed by `list_for_search`. Re-push.
            search::spawn_repush(state, meeting.id, meeting_id.to_string());
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
            if is_guest || !auto_admitted {
                let dn = display_name.unwrap_or(fallback_display_name);
                resp.observer_token = Some(generate_observer_token(
                    &state.jwt_secret,
                    user_id,
                    meeting_id,
                    dn,
                    is_guest,
                )?);
            }
            if !auto_admitted {
                nats_events::publish_waiting_room_updated(state.nats.as_ref(), meeting_id).await;
            }
            resp.waiting_room_enabled = wr_enabled;
            resp.admitted_can_admit = meeting.admitted_can_admit;
            resp.end_on_host_leave = meeting.end_on_host_leave;
            resp.allow_guests = meeting.allow_guests;
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
            waiting_room_enabled: meeting.waiting_room_enabled,
            admitted_can_admit: meeting.admitted_can_admit,
            end_on_host_leave: meeting.end_on_host_leave,
            host_display_name: meeting.host_display_name,
            host_user_id: meeting.creator_id,
            allow_guests: meeting.allow_guests,
        };
        return Ok(Json(APIResponse::ok(resp)));
    }
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
    // New attendee row added — re-push so search hits reflect the new participant.
    search::spawn_repush(state, meeting.id, meeting_id.to_string());

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
    // Waiting attendees and all guests receive observer tokens for guest-status polling.
    if is_guest || !auto_admitted {
        let dn = display_name.unwrap_or(fallback_display_name);
        resp.observer_token = Some(generate_observer_token(
            &state.jwt_secret,
            user_id,
            meeting_id,
            dn,
            is_guest,
        )?);
    }
    if !auto_admitted {
        // Notify the host that the waiting room list has changed.
        nats_events::publish_waiting_room_updated(state.nats.as_ref(), meeting_id).await;
    }
    resp.waiting_room_enabled = waiting_room_enabled;
    resp.admitted_can_admit = meeting.admitted_can_admit;
    resp.end_on_host_leave = meeting.end_on_host_leave;
    resp.allow_guests = meeting.allow_guests;
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
    let display_name = validate_display_name(&body.display_name)
        .map_err(|_| AppError::invalid_input("Invalid display name."))?;
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
    resp.waiting_room_enabled = meeting.waiting_room_enabled;
    resp.admitted_can_admit = meeting.admitted_can_admit;
    resp.end_on_host_leave = meeting.end_on_host_leave;
    resp.allow_guests = meeting.allow_guests;
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
    resp.waiting_room_enabled = meeting.waiting_room_enabled;
    resp.admitted_can_admit = meeting.admitted_can_admit;
    resp.allow_guests = meeting.allow_guests;
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

    // Participant left — they're still a row in meeting_participants but
    // `list_for_search` filters to `admitted`/`waiting`, so this removal
    // drops their principal from the ACL set on the next push.
    search::spawn_repush(&state, meeting.id, meeting_id.clone());

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
    let validated_name =
        validate_display_name(&body.display_name).map_err(|_| AppError::invalid_display_name())?;

    // Validate the participant exists and is in the meeting, and check if the name is actually changing.
    let current_status = db_participants::get_status(&state.db, meeting.id, &user_id)
        .await?
        .ok_or_else(AppError::not_in_meeting)?;

    // Check if the name is actually changing.
    // This prevents budget burn and broadcast amplification from idempotent retries.
    let name_is_changing =
        is_name_changing(current_status.display_name.as_deref(), &validated_name);

    if !name_is_changing {
        // Name is identical — return current status without DB write, broadcast, or rate limit.
        return Ok(Json(APIResponse::ok(
            current_status.into_participant_status(None),
        )));
    }

    // Name is changing: consume a rate-limit slot.
    enforce_display_name_rate_limit(&state, &user_id).await?;

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

    // Display-name change propagates into `participantNames` on the doc
    // — re-push so search hits show the current name.
    search::spawn_repush(&state, meeting.id, meeting_id.clone());

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

#[cfg(test)]
mod tests {
    use super::{is_name_changing, DISPLAY_NAME_WINDOW_SECS, MAX_DISPLAY_NAME_RENAMES};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    /// Helper that mirrors the shared `enforce_display_name_rate_limit` logic,
    /// returning `true` if the request is allowed.
    fn check_rate_limit(
        limiter: &mut HashMap<String, (Instant, u32)>,
        user_id: &str,
        max_renames: u32,
        window_secs: u64,
    ) -> bool {
        limiter.retain(|_, (ws, _)| ws.elapsed().as_secs() < window_secs);
        let entry = limiter
            .entry(user_id.to_string())
            .or_insert_with(|| (Instant::now(), 0));
        if entry.1 >= max_renames {
            return false;
        }
        entry.1 += 1;
        true
    }

    #[test]
    fn rate_limiter_allows_up_to_max_renames() {
        let mut map = HashMap::new();
        for _ in 0..5 {
            assert!(check_rate_limit(&mut map, "alice", 5, 60));
        }
        // 6th should be rejected
        assert!(!check_rate_limit(&mut map, "alice", 5, 60));
    }

    #[test]
    fn rate_limiter_resets_after_window() {
        let mut map = HashMap::new();
        // Exhaust the limit
        for _ in 0..5 {
            assert!(check_rate_limit(&mut map, "bob", 5, 60));
        }
        assert!(!check_rate_limit(&mut map, "bob", 5, 60));

        // Simulate window expiry by back-dating the entry
        if let Some(entry) = map.get_mut("bob") {
            entry.0 = Instant::now() - Duration::from_secs(61);
        }
        // Should be allowed again (retain evicts, or_insert_with creates fresh)
        assert!(check_rate_limit(&mut map, "bob", 5, 60));
    }

    #[test]
    fn stale_entries_are_evicted() {
        let mut map = HashMap::new();

        // Insert a stale entry for a one-time user
        map.insert(
            "one-timer".to_string(),
            (Instant::now() - Duration::from_secs(120), 1),
        );
        assert_eq!(map.len(), 1);

        // A new user's rate-limit check triggers eviction
        assert!(check_rate_limit(&mut map, "active-user", 5, 60));
        // Stale entry should have been removed
        assert!(!map.contains_key("one-timer"));
        assert_eq!(map.len(), 1); // only "active-user"
    }

    #[test]
    fn active_entries_survive_eviction() {
        let mut map = HashMap::new();

        // Active entry (recent)
        assert!(check_rate_limit(&mut map, "active", 5, 60));
        // Stale entry
        map.insert(
            "stale".to_string(),
            (Instant::now() - Duration::from_secs(120), 3),
        );
        assert_eq!(map.len(), 2);

        // Trigger eviction via another check
        assert!(check_rate_limit(&mut map, "active", 5, 60));
        assert!(map.contains_key("active"));
        assert!(!map.contains_key("stale"));
    }

    #[test]
    fn per_user_isolation() {
        let mut map = HashMap::new();

        // Exhaust user A's limit
        for _ in 0..5 {
            assert!(check_rate_limit(&mut map, "user-a", 5, 60));
        }
        assert!(!check_rate_limit(&mut map, "user-a", 5, 60));

        // User B must still have a full quota
        for _ in 0..5 {
            assert!(check_rate_limit(&mut map, "user-b", 5, 60));
        }
        assert!(!check_rate_limit(&mut map, "user-b", 5, 60));
    }

    /// Simulates the leave+join bypass: a single shared limiter is used by both
    /// the join (with display_name) and update_display_name paths.  Repeated
    /// join-with-name requests should exhaust the budget identically.
    #[test]
    fn join_bypass_shares_rate_limit_budget_with_rename() {
        let mut map = HashMap::new();
        let max = MAX_DISPLAY_NAME_RENAMES;
        let window = DISPLAY_NAME_WINDOW_SECS;

        // Simulate 3 renames via the update path
        for _ in 0..3 {
            assert!(check_rate_limit(&mut map, "attacker", max, window));
        }

        // Simulate 2 more via the join path (same limiter, same user)
        for _ in 0..2 {
            assert!(check_rate_limit(&mut map, "attacker", max, window));
        }

        // 6th attempt (via either path) must be rejected
        assert!(!check_rate_limit(&mut map, "attacker", max, window));
    }

    /// Constants used by the module are sensible.
    #[test]
    fn rate_limit_constants_are_valid() {
        assert_eq!(MAX_DISPLAY_NAME_RENAMES, 5);
        assert_eq!(DISPLAY_NAME_WINDOW_SECS, 60);
    }

    /// Idempotent requests (same name) should not consume a rate-limit slot.
    /// This test verifies that calling check_rate_limit with the same user_id
    /// twice in a row, with the same counter value, behaves correctly if we
    /// skip the second call (simulating name-unchanged logic).
    #[test]
    fn unchanged_name_does_not_consume_budget() {
        let mut map = HashMap::new();

        // First rename consumes a slot
        assert!(check_rate_limit(&mut map, "user", 5, 60));
        assert_eq!(map.get("user").unwrap().1, 1);

        // If the name were unchanged, we wouldn't call check_rate_limit again.
        // Verify that manually: counter stays at 1.
        let counter_before = map.get("user").unwrap().1;
        // (not calling check_rate_limit, simulating the skip)
        assert_eq!(counter_before, 1);

        // Subsequent different rename does consume a slot
        assert!(check_rate_limit(&mut map, "user", 5, 60));
        assert_eq!(map.get("user").unwrap().1, 2);

        // Exhausting the limit still works
        for _ in 0..3 {
            assert!(check_rate_limit(&mut map, "user", 5, 60));
        }
        // Now at 5, next attempt blocked
        assert!(!check_rate_limit(&mut map, "user", 5, 60));
    }

    /// Verify that the name-change detection logic works correctly.
    /// When a participant resubmits their current display name, it should be
    /// detected as unchanged.
    #[test]
    fn name_change_detection() {
        // Case 1: No existing name -> new name = changing
        assert!(
            is_name_changing(None, "Alice"),
            "None -> Some should be changing"
        );

        // Case 2: Same name = not changing
        assert!(
            !is_name_changing(Some("Alice"), "Alice"),
            "Same name should be unchanged"
        );

        // Case 3: Different name = changing
        assert!(
            is_name_changing(Some("Alice"), "Bob"),
            "Different name should be changing"
        );
    }
}

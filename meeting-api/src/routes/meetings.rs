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
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use rand::Rng;
use videocall_meeting_types::{
    requests::{
        CreateMeetingRequest, ListFeedQuery, ListJoinedMeetingsQuery, ListMeetingsQuery,
        UpdateMeetingRequest,
    },
    responses::{
        APIResponse, CreateMeetingResponse, DeleteMeetingResponse, JoinedMeetingSummary,
        ListFeedResponse, ListJoinedMeetingsResponse, ListMeetingsResponse, MeetingFeedSummary,
        MeetingGuestInfoResponse, MeetingInfoResponse, MeetingSummary,
    },
};

use crate::auth::AuthUser;
use crate::db::{
    co_hosts as db_co_hosts, meetings as db_meetings, participants as db_participants,
};
use crate::error::AppError;
use crate::feed_events::{self, FeedChange, FeedChangeReason};
use crate::nats_events;
use crate::password;
use crate::routes::co_hosts;
use crate::routes::valid_meeting_id::ValidMeetingId;
use crate::state::AppState;
use videocall_types::validation::validate_meeting_id;

const MAX_ATTENDEES: usize = 100;

/// Hard cap for `GET /api/v1/meetings/feed`; use the search modal beyond this.
const MAX_FEED_LIMIT: i64 = 200;

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
    let co_hosts = co_hosts::normalize_create_list(&body.co_hosts, &user_id)?;

    let password_hash = match &body.password {
        Some(pw) if !pw.is_empty() => Some(state.password_gate.hash(pw).await?),
        _ => None,
    };

    let attendees_json =
        serde_json::to_value(&body.attendees).map_err(|e| AppError::internal(&e.to_string()))?;

    let waiting_room_enabled = body.waiting_room_enabled.unwrap_or(true);
    let admitted_can_admit = body.admitted_can_admit.unwrap_or(false);
    let end_on_host_leave = body.end_on_host_leave.unwrap_or(true);
    let allow_guests = body.allow_guests.unwrap_or(false);
    let recording_allowed_for_all = body.recording_allowed_for_all.unwrap_or(false);
    let chat_allowed_for_all = body.chat_allowed_for_all.unwrap_or(true);

    let mut tx = state.db.begin().await?;
    let row = db_meetings::create_with_options(
        &mut *tx,
        &meeting_id,
        &user_id,
        password_hash.as_deref(),
        &attendees_json,
        waiting_room_enabled,
        admitted_can_admit,
        end_on_host_leave,
        allow_guests,
        recording_allowed_for_all,
        chat_allowed_for_all,
    )
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            AppError::meeting_exists(&meeting_id)
        }
        other => AppError::from(other),
    })?;
    db_co_hosts::insert_persistent(&mut tx, row.id, &co_hosts, &user_id).await?;
    tx.commit().await?;

    search::spawn_repush(&state, row.id, row.room_id.clone());

    feed_events::publish_feed_change(
        state.nats.as_ref(),
        &state.feed_tx,
        FeedChange::new(row.room_id.clone(), FeedChangeReason::Created),
    )
    .await;

    let response = CreateMeetingResponse {
        meeting_id: row.room_id,
        host: user_id,
        created_at: row.created_at.timestamp_millis(),
        state: row.state.unwrap_or_else(|| "idle".to_string()),
        attendees: body.attendees,
        has_password: password_hash.is_some(),
        waiting_room_enabled: row.waiting_room_enabled,
        admitted_can_admit: row.admitted_can_admit,
        end_on_host_leave: row.end_on_host_leave,
        allow_guests: row.allow_guests,
        recording_allowed_for_all: row.recording_allowed_for_all,
        chat_allowed_for_all: row.chat_allowed_for_all,
        co_hosts,
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

    let healthy = state.presence_healthy().await?;
    let mut meetings = Vec::with_capacity(rows.len());
    for row in &rows {
        let participant_count = db_participants::count_admitted(&state.db, row.id, healthy).await?;
        let waiting_count = db_participants::count_waiting(&state.db, row.id).await?;

        meetings.push(MeetingSummary {
            meeting_id: row.room_id.clone(),
            host: row.creator_id.clone(),
            // Issue #1628: derive the displayed state from live presence so
            // `idle ⟺ zero present`. The raw column can lag at 'idle' while
            // participants are present (transport-only reconnect, column/
            // presence skew); `participant_count` is the same live count shown
            // below, so state and count can never contradict.
            state: db_meetings::display_state(row.state.as_deref(), participant_count),
            has_password: row.password_hash.is_some(),
            created_at: row.created_at.timestamp_millis(),
            participant_count,
            started_at: row.started_at.timestamp_millis(),
            ended_at: row.ended_at.map(|t| t.timestamp_millis()),
            waiting_count,
            waiting_room_enabled: row.waiting_room_enabled,
            admitted_can_admit: row.admitted_can_admit,
            end_on_host_leave: row.end_on_host_leave,
            allow_guests: row.allow_guests,
            recording_allowed_for_all: row.recording_allowed_for_all,
            chat_allowed_for_all: row.chat_allowed_for_all,
        });
    }

    Ok(Json(APIResponse::ok(ListMeetingsResponse {
        meetings,
        total,
        limit,
        offset,
    })))
}

/// GET /api/v1/meetings/joined — meetings admitted into, ordered by last admission. `limit` default 5, max 50.
pub async fn list_joined_meetings(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Query(params): Query<ListJoinedMeetingsQuery>,
) -> Result<Json<APIResponse<ListJoinedMeetingsResponse>>, AppError> {
    if params.limit < 1 {
        return Err(AppError::invalid_input(
            "limit must be a positive integer between 1 and 50",
        ));
    }
    let limit = params.limit.min(50);

    let healthy = state.presence_healthy().await?;
    let rows = db_meetings::list_joined_by_user(&state.db, &user_id, limit, healthy).await?;

    let mut meetings = Vec::with_capacity(rows.len());
    for row in &rows {
        meetings.push(JoinedMeetingSummary {
            meeting_id: row.room_id.clone(),
            // Issue #1628: presence-derived display state (see `list_meetings`).
            state: db_meetings::display_state(row.state.as_deref(), row.participant_count),
            started_at: row.started_at.timestamp_millis(),
            ended_at: row.ended_at.map(|t| t.timestamp_millis()),
            participant_count: row.participant_count,
            waiting_count: row.waiting_count,
            has_password: row.password_hash.is_some(),
            is_owner: row.creator_id.as_deref() == Some(user_id.as_str()),
            created_at: row.created_at.timestamp_millis(),
            last_joined_at: row.last_joined_at.timestamp_millis(),
        });
    }

    let total = meetings.len() as i64;
    Ok(Json(APIResponse::ok(ListJoinedMeetingsResponse {
        meetings,
        total,
    })))
}

/// GET /api/v1/meetings/feed — home-page feed, deduplicated and ordered by `last_active_at` DESC. `limit` default/max 200.
pub async fn list_feed(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Query(params): Query<ListFeedQuery>,
) -> Result<Json<APIResponse<ListFeedResponse>>, AppError> {
    if params.limit < 1 {
        return Err(AppError::invalid_input(
            "limit must be a positive integer between 1 and 200",
        ));
    }
    let limit = params.limit.min(MAX_FEED_LIMIT);

    let healthy = state.presence_healthy().await?;
    let rows = db_meetings::list_feed_for_user(&state.db, &user_id, limit, healthy).await?;

    let meetings = rows
        .into_iter()
        .map(|row| {
            // Issue #1628: derive the displayed state from live presence so
            // `idle ⟺ zero present`. The raw column can lag at 'idle' while
            // participants are present (transport-only reconnect, column/
            // presence skew); `participant_count` is the same live count
            // surfaced below, so state and count can never contradict.
            let state_str = db_meetings::display_state(row.state.as_deref(), row.participant_count);
            // `started_at` is set to NOW() at INSERT even when the meeting is
            // still `idle`, so the raw column is meaningless for never-
            // activated meetings. Surface `Some(started_at)` only when the
            // meeting has actually been activated at some point — i.e. the
            // (now presence-derived) display state is not idle, or the meeting
            // has ended. This mirrors the spec: "None if never activated". A
            // meeting that is currently active by presence (>=1 present) but
            // whose raw column lagged at 'idle' is correctly treated as
            // activated here too.
            let was_activated = state_str != "idle" || row.ended_at.is_some();
            let started_at = if was_activated {
                Some(row.started_at.timestamp_millis())
            } else {
                None
            };

            MeetingFeedSummary {
                meeting_id: row.room_id,
                state: state_str,
                last_active_at: row.last_active_at.timestamp_millis(),
                created_at: row.created_at.timestamp_millis(),
                started_at,
                ended_at: row.ended_at.map(|t| t.timestamp_millis()),
                host: row.creator_id.clone(),
                host_display_name: row.host_display_name,
                host_user_id: row.creator_id.clone(),
                is_owner: row.creator_id.as_deref() == Some(user_id.as_str()),
                is_co_host: row.is_co_host,
                participant_count: row.participant_count,
                waiting_count: row.waiting_count,
                has_password: row.password_hash.is_some(),
                allow_guests: row.allow_guests,
                recording_allowed_for_all: row.recording_allowed_for_all,
                chat_allowed_for_all: row.chat_allowed_for_all,
                waiting_room_enabled: row.waiting_room_enabled,
                admitted_can_admit: row.admitted_can_admit,
                end_on_host_leave: row.end_on_host_leave,
                // Strictly the REQUESTING user's last admission (raw
                // `p.last_admit`, user-scoped MAX(admitted_at)); `None` when
                // they were never admitted. Not `last_active_at`, which is
                // COALESCE'd with meeting-level fallbacks.
                user_last_attended_at: row.last_admit.map(|t| t.timestamp_millis()),
            }
        })
        .collect();

    Ok(Json(APIResponse::ok(ListFeedResponse { meetings })))
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
    let viewer_is_owner = row.creator_id.as_deref() == Some(user_id.as_str());

    let your_status = db_participants::get_status(&state.db, row.id, &user_id).await?;
    let your_status = your_status.map(|p| p.into_participant_status(None));

    let healthy = state.presence_healthy().await?;
    let participant_count = db_participants::count_admitted(&state.db, row.id, healthy).await?;
    let waiting_count = db_participants::count_waiting(&state.db, row.id).await?;
    let viewer_can_edit_options = can_edit_options(&state, &row, &user_id, healthy).await?;

    Ok(Json(APIResponse::ok(MeetingInfoResponse {
        meeting_id: row.room_id,
        // Issue #1628: presence-derived display state (see `list_meetings`).
        state: db_meetings::display_state(row.state.as_deref(), participant_count),
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
        recording_allowed_for_all: row.recording_allowed_for_all,
        chat_allowed_for_all: row.chat_allowed_for_all,
        viewer_is_owner,
        viewer_can_edit_options,
    })))
}

/// Whether a delete should broadcast MEETING_ENDED: only a real (non-race) delete of an active meeting.
pub fn delete_should_broadcast(deleted: &Option<db_meetings::MeetingRow>) -> bool {
    matches!(deleted, Some(m) if m.state.as_deref() == Some("active"))
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

    let deleted = db_meetings::soft_delete(&state.db, &meeting_id, &user_id).await?;

    if delete_should_broadcast(&deleted) {
        nats_events::publish_meeting_ended(
            state.nats.as_ref(),
            &meeting_id,
            nats_events::HOST_LEFT_MESSAGE,
        )
        .await;
    }

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

        let healthy = state.presence_healthy().await?;
        let participant_count =
            db_participants::count_admitted(&state.db, meeting.id, healthy).await?;
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
            recording_allowed_for_all: meeting.recording_allowed_for_all,
            chat_allowed_for_all: meeting.chat_allowed_for_all,
            viewer_is_owner: true,
            viewer_can_edit_options: true,
        })));
    }

    db_meetings::end_meeting(&state.db, meeting.id).await?;

    let row = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    nats_events::publish_meeting_ended(
        state.nats.as_ref(),
        &meeting_id,
        nats_events::HOST_LEFT_MESSAGE,
    )
    .await;

    search::spawn_repush(&state, row.id, row.room_id.clone());

    feed_events::publish_feed_change(
        state.nats.as_ref(),
        &state.feed_tx,
        FeedChange::new(row.room_id.clone(), FeedChangeReason::Ended),
    )
    .await;

    let your_status = db_participants::get_status(&state.db, row.id, &user_id).await?;
    let your_status = your_status.map(|p| p.into_participant_status(None));

    let healthy = state.presence_healthy().await?;
    let participant_count = db_participants::count_admitted(&state.db, row.id, healthy).await?;
    let waiting_count = db_participants::count_waiting(&state.db, row.id).await?;

    Ok(Json(APIResponse::ok(MeetingInfoResponse {
        meeting_id: row.room_id,
        // Just transitioned to `ended`; `display_state` returns `ended`
        // (terminal) regardless of any in-flight roster rows (issue #1628).
        state: db_meetings::display_state(row.state.as_deref(), participant_count),
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
        recording_allowed_for_all: row.recording_allowed_for_all,
        chat_allowed_for_all: row.chat_allowed_for_all,
        // `end_meeting_handler` is owner-only (checked above), on both its
        // idempotent and real-end response.
        viewer_is_owner: true,
        viewer_can_edit_options: true,
    })))
}

/// Whether `user_id` may change meeting OPTIONS or list co-hosts: owner, live co-host, or present host of the active meeting.
pub(crate) async fn can_edit_options(
    state: &AppState,
    meeting: &db_meetings::MeetingRow,
    user_id: &str,
    healthy: bool,
) -> Result<bool, AppError> {
    if meeting.creator_id.as_deref() == Some(user_id) {
        return Ok(true);
    }
    if db_co_hosts::has_live_entry(&state.db, meeting.id, user_id).await? {
        return Ok(true);
    }
    Ok(meeting.state.as_deref() == Some(db_meetings::STATE_ACTIVE)
        && db_participants::is_present_host(&state.db, meeting.id, user_id, healthy).await?)
}

/// PATCH /api/v1/meetings/{meeting_id}
pub async fn update_meeting(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    ValidMeetingId(meeting_id): ValidMeetingId,
    Json(body): Json<UpdateMeetingRequest>,
) -> Result<Json<APIResponse<MeetingInfoResponse>>, AppError> {
    let toggles_updated = body.waiting_room_enabled.is_some()
        || body.admitted_can_admit.is_some()
        || body.end_on_host_leave.is_some()
        || body.allow_guests.is_some()
        || body.recording_allowed_for_all.is_some()
        || body.chat_allowed_for_all.is_some();

    let mut auto_admitted_user_ids: Vec<String> = Vec::new();

    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;
    let is_owner = meeting.creator_id.as_deref() == Some(user_id.as_str());

    let intent = password::parse_password_update(body.password.as_deref(), body.remove_password)?;
    let password_updated = intent.is_change();
    let settings_updated = toggles_updated || password_updated;

    if password_updated && !is_owner {
        return Err(AppError::not_owner());
    }
    let healthy = state.presence_healthy().await?;
    if !can_edit_options(&state, &meeting, &user_id, healthy).await? {
        return Err(AppError::not_host());
    }

    let password_update = state.password_gate.hash_intent(intent).await?;

    let row = if settings_updated {
        match db_meetings::update_meeting_settings(
            &state.db,
            &meeting_id,
            body.waiting_room_enabled,
            body.admitted_can_admit,
            body.end_on_host_leave,
            body.allow_guests,
            body.recording_allowed_for_all,
            body.chat_allowed_for_all,
            &password_update,
        )
        .await?
        {
            Some(update) => {
                auto_admitted_user_ids = update.auto_admitted_user_ids;
                update.row
            }
            // The caller was already authorized above against a row that
            // existed moments ago; only a concurrent delete explains a miss.
            None => return Err(AppError::meeting_not_found(&meeting_id)),
        }
    } else {
        // No updates requested — the row already fetched (and authorized
        // above) is the response.
        meeting
    };

    search::spawn_repush(&state, row.id, row.room_id.clone());

    if settings_updated {
        // The bulk admit above emits no per-participant event, and these
        // clients are listening for PARTICIPANT_ADMITTED (issue #2262).
        for admitted_user_id in &auto_admitted_user_ids {
            nats_events::publish_participant_admitted(
                state.nats.as_ref(),
                &row.room_id,
                admitted_user_id,
            )
            .await;
        }
        if !auto_admitted_user_ids.is_empty() {
            nats_events::publish_waiting_room_updated(state.nats.as_ref(), &row.room_id).await;
        }
        // Notify clients (REST refetch trigger); `has_password` is on every
        // meeting payload, so a password change has to reach them too.
        nats_events::publish_meeting_settings_updated(state.nats.as_ref(), &row.room_id).await;
    }

    if toggles_updated {
        let internal_payload = nats_events::MeetingSettingsUpdatePayload {
            room_id: row.room_id.clone(),
            end_on_host_leave: row.end_on_host_leave,
            admitted_can_admit: row.admitted_can_admit,
            waiting_room_enabled: row.waiting_room_enabled,
            allow_guests: row.allow_guests,
            recording_allowed_for_all: row.recording_allowed_for_all,
        };
        nats_events::publish_internal_meeting_settings_update(
            state.nats.as_ref(),
            &internal_payload,
        )
        .await;

        // NOTE (issue #1081): we intentionally do NOT emit a homepage-feed nudge
        // for a settings PATCH. The live homepage list tracks meeting
        // presence/state/counts (created, joined, idle, ended, left); the
        // host-only policy flags carried on the feed row are already propagated
        // by the dedicated `MEETING_SETTINGS_UPDATED` event above, and the owner
        // who issued the PATCH is the one who triggered the change, so their own
        // client already has the new values. Adding a nudge here would be an
        // inaccurately-labelled, host-only signal outside the live-list scope.
    }

    let your_status = db_participants::get_status(&state.db, row.id, &user_id).await?;
    let your_status = your_status.map(|p| p.into_participant_status(None));

    let participant_count = db_participants::count_admitted(&state.db, row.id, healthy).await?;
    let waiting_count = db_participants::count_waiting(&state.db, row.id).await?;

    Ok(Json(APIResponse::ok(MeetingInfoResponse {
        meeting_id: row.room_id,
        // Issue #1628: presence-derived display state (see `list_meetings`).
        // The settings/edit page reads this field too; keeping the single
        // derivation here makes the edit page and the list agree.
        state: db_meetings::display_state(row.state.as_deref(), participant_count),
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
        recording_allowed_for_all: row.recording_allowed_for_all,
        chat_allowed_for_all: row.chat_allowed_for_all,
        viewer_is_owner: is_owner,
        // Reaching here means `can_edit_options` already authorized this
        // caller above (owner, live co-host, or present host).
        viewer_can_edit_options: true,
    })))
}

/// GET /api/v1/meetings/{meeting_id}/guest-info — public; `false` for both missing and guest-disabled meetings, never 404 (avoids enumeration).
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

    fn app_error_for(meeting_id: &str) -> AppError {
        AppError::from(validate_meeting_id(meeting_id).expect_err("id should be rejected"))
    }

    #[test]
    fn validate_accepts_simple_alphanumeric() {
        assert!(validate_meeting_id("standup2024").is_ok());
    }

    #[test]
    fn validate_accepts_hyphens_underscores_and_tildes() {
        assert!(validate_meeting_id("my-meeting_123").is_ok());
        assert!(validate_meeting_id("a~b").is_ok());
    }

    #[test]
    fn validate_rejects_empty_id() {
        let err = app_error_for("");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.body.code, "INVALID_MEETING_ID");
        assert_eq!(err.body.message, "Invalid meeting ID: cannot be empty");
    }

    #[test]
    fn validate_rejects_too_long_id() {
        let err = app_error_for(&"a".repeat(256));
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.body.code, "INVALID_MEETING_ID");
    }

    #[test]
    fn validate_rejects_special_characters() {
        let err = app_error_for("room id with spaces");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.body.code, "INVALID_MEETING_ID");
        assert!(
            err.body.message.contains("' '"),
            "message should name the rejected character: {}",
            err.body.message
        );
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

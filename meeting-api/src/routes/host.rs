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

//! Host-only meeting controls.
//! Mute, disable video, or kick a single participant; mute/disable-video for all.
//! Only the meeting host may call these endpoints.

use axum::{
    extract::{Path, State},
    Json,
};
use videocall_meeting_types::{
    requests::{
        DisableVideoParticipantRequest, KickParticipantRequest, MuteParticipantRequest,
        TransferHostRequest,
    },
    responses::APIResponse,
    GUEST_USER_ID_PREFIX,
};

use crate::auth::AuthUser;
use crate::db::{meetings as db_meetings, participants as db_participants};
use crate::error::AppError;
use crate::feed_events::{self, FeedChange, FeedChangeReason};
use crate::nats_events;
use crate::state::AppState;

/// Strict host-only authorization. Checks that the user is a participant in the meeting with
/// "admitted" status and `is_host` flag set. Used by all host-only endpoints in this module.
async fn require_host(state: &AppState, meeting_id: i32, user_id: &str) -> Result<(), AppError> {
    let row = db_participants::get_status(&state.db, meeting_id, user_id)
        .await?
        .ok_or_else(AppError::not_host)?;
    if row.status != "admitted" || !row.is_host {
        return Err(AppError::not_host());
    }
    Ok(())
}

/// `POST /api/v1/meetings/{meeting_id}/mute`.
///
/// Host requests that a single participant mute their mic. The server
/// publishes a single `HOST_MUTE_PARTICIPANT` NATS event with the target
/// user ID.
pub async fn mute_participant(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
    Json(body): Json<MuteParticipantRequest>,
) -> Result<Json<APIResponse<()>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;
    if meeting.state.as_deref() == Some("ended") {
        return Err(AppError::meeting_not_found(&meeting_id));
    }
    require_host(&state, meeting.id, &user_id).await?;

    if body.user_id.is_empty() {
        return Err(AppError::bad_request(
            "user_id must not be empty; use /mute-all to mute everyone",
        ));
    }

    const MAX_USER_ID_LEN: usize = 254;

    if body.user_id.len() > MAX_USER_ID_LEN {
        return Err(AppError::bad_request("user_id too long"));
    }
    if body.user_id == user_id {
        return Err(AppError::bad_request(
            "cannot mute yourself via host action",
        ));
    }

    nats_events::publish_host_mute(state.nats.as_ref(), &meeting_id, &body.user_id, &user_id)
        .await
        .map_err(|e| {
            tracing::error!(
                "NATS publish failed for HOST_MUTE_PARTICIPANT in room {meeting_id}: {e}"
            );
            AppError::internal("failed to broadcast mute event")
        })?;
    Ok(Json(APIResponse::ok(())))
}

/// `POST /api/v1/meetings/{meeting_id}/mute-all`.
///
/// Host requests that every participant mute their mic. Implemented as a
/// single NATS broadcast with an empty `target_user_id`.
pub async fn mute_all(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<()>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;
    if meeting.state.as_deref() == Some("ended") {
        return Err(AppError::meeting_not_found(&meeting_id));
    }
    require_host(&state, meeting.id, &user_id).await?;

    nats_events::publish_host_mute(state.nats.as_ref(), &meeting_id, "", &user_id)
        .await
        .map_err(|e| {
            tracing::error!("NATS publish failed for HOST_MUTE_ALL in room {meeting_id}: {e}");
            AppError::internal("failed to broadcast mute event")
        })?;
    Ok(Json(APIResponse::ok(())))
}

/// `POST /api/v1/meetings/{meeting_id}/disable-video`.
///
/// Host requests that a single participant disable their camera. The server
/// publishes a single `HOST_DISABLE_VIDEO` NATS event with the target user ID.
pub async fn disable_video_participant(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
    Json(body): Json<DisableVideoParticipantRequest>,
) -> Result<Json<APIResponse<()>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;
    if meeting.state.as_deref() == Some("ended") {
        return Err(AppError::meeting_not_found(&meeting_id));
    }
    require_host(&state, meeting.id, &user_id).await?;

    if body.user_id.is_empty() {
        return Err(AppError::bad_request(
            "user_id must not be empty; use /disable-video-all to disable video for everyone",
        ));
    }

    const MAX_USER_ID_LEN: usize = 254;

    if body.user_id.len() > MAX_USER_ID_LEN {
        return Err(AppError::bad_request("user_id too long"));
    }
    if body.user_id == user_id {
        return Err(AppError::bad_request(
            "cannot disable your own video via host action",
        ));
    }

    nats_events::publish_host_disable_video(
        state.nats.as_ref(),
        &meeting_id,
        &body.user_id,
        &user_id,
    )
    .await
    .map_err(|e| {
        tracing::error!("NATS publish failed for HOST_DISABLE_VIDEO in room {meeting_id}: {e}");
        AppError::internal("failed to broadcast disable-video event")
    })?;
    Ok(Json(APIResponse::ok(())))
}

/// `POST /api/v1/meetings/{meeting_id}/disable-video-all`.
///
/// Host requests that every participant disable their camera. Implemented as
/// a single NATS broadcast with an empty `target_user_id`.
pub async fn disable_video_all(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
) -> Result<Json<APIResponse<()>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;
    if meeting.state.as_deref() == Some("ended") {
        return Err(AppError::meeting_not_found(&meeting_id));
    }
    require_host(&state, meeting.id, &user_id).await?;

    nats_events::publish_host_disable_video(state.nats.as_ref(), &meeting_id, "", &user_id)
        .await
        .map_err(|e| {
            tracing::error!(
                "NATS publish failed for HOST_DISABLE_VIDEO_ALL in room {meeting_id}: {e}"
            );
            AppError::internal("failed to broadcast disable-video event")
        })?;
    Ok(Json(APIResponse::ok(())))
}

/// `POST /api/v1/meetings/{meeting_id}/kick`.
///
/// Host removes a single participant from the meeting. The server:
///   1. Marks the participant's DB row as `status='kicked'`.
///   2. Publishes a `PARTICIPANT_KICKED` NATS event with the target user ID.
///
/// The kicked participant's client receives the event, shows a toast, and
/// disconnects. They may rejoin by navigating to the meeting URL again (they
/// go through the normal join/waiting-room flow).
pub async fn kick_participant(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
    Json(body): Json<KickParticipantRequest>,
) -> Result<Json<APIResponse<()>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;
    if meeting.state.as_deref() == Some("ended") {
        return Err(AppError::meeting_not_found(&meeting_id));
    }
    require_host(&state, meeting.id, &user_id).await?;

    if body.user_id.is_empty() {
        return Err(AppError::bad_request("user_id must not be empty"));
    }

    const MAX_USER_ID_LEN: usize = 254;
    if body.user_id.len() > MAX_USER_ID_LEN {
        return Err(AppError::bad_request("user_id too long"));
    }
    if body.user_id == user_id {
        return Err(AppError::bad_request("cannot kick yourself"));
    }

    db_participants::kick(&state.db, meeting.id, &body.user_id)
        .await
        .map_err(|e| {
            tracing::error!(
                "DB kick failed for user {} in room {meeting_id}: {e}",
                body.user_id
            );
            AppError::internal("failed to update participant status")
        })?;

    nats_events::publish_host_kick(state.nats.as_ref(), &meeting_id, &body.user_id)
        .await
        .map_err(|e| {
            tracing::error!("NATS publish failed for PARTICIPANT_KICKED in room {meeting_id}: {e}");
            AppError::internal("failed to broadcast kick event")
        })?;

    // Live homepage-feed nudge (issue #1081): a kick removes a present
    // participant (status='kicked'), dropping the count the feed shows.
    feed_events::publish_feed_change(
        state.nats.as_ref(),
        &state.feed_tx,
        FeedChange::new(meeting_id.clone(), FeedChangeReason::ParticipantLeft),
    )
    .await;

    Ok(Json(APIResponse::ok(())))
}

/// Max accepted `user_id` length on host endpoints.
const MAX_USER_ID_LEN: usize = 254;

/// `POST /api/v1/meetings/{meeting_id}/transfer-host`.
///
/// Atomically promotes the target and demotes the issuing host in a single DB
/// transaction — the only sanctioned self-demotion. If the target is not an
/// admitted participant the transaction rolls back and the caller keeps host
/// (so the meeting is never left without a successor).
///
/// Event ordering: `HOST_GRANTED(target)` is published BEFORE
/// `HOST_REVOKED(caller)` so no client ever observes a transient hostless gap.
pub async fn transfer_host(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
    Json(body): Json<TransferHostRequest>,
) -> Result<Json<APIResponse<()>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;
    if meeting.state.as_deref() == Some("ended") {
        return Err(AppError::meeting_not_found(&meeting_id));
    }
    require_host(&state, meeting.id, &user_id).await?;

    if body.user_id.is_empty() {
        return Err(AppError::bad_request("user_id must not be empty"));
    }
    if body.user_id.len() > MAX_USER_ID_LEN {
        return Err(AppError::bad_request("user_id too long"));
    }
    if body.user_id == user_id {
        return Err(AppError::bad_request("cannot transfer host to yourself"));
    }
    if body.user_id.starts_with(GUEST_USER_ID_PREFIX) {
        return Err(AppError::bad_request(
            "cannot transfer host to a guest participant",
        ));
    }

    let transferred =
        db_participants::transfer_host(&state.db, meeting.id, &user_id, &body.user_id)
            .await
            .map_err(|e| {
                tracing::error!(
                    "DB transfer_host failed from {user_id} to {} in room {meeting_id}: {e}",
                    body.user_id
                );
                AppError::internal("failed to transfer host")
            })?;
    if transferred.is_none() {
        return Err(AppError::bad_request(
            "target is not an admitted participant",
        ));
    }

    // Promotion event first so no client observes a transient hostless gap.
    if let Err(e) =
        nats_events::publish_host_granted(state.nats.as_ref(), &meeting_id, &body.user_id, &user_id)
            .await
    {
        tracing::error!(
            "NATS publish failed for HOST_GRANTED (transfer) in room {meeting_id}: {e}"
        );
    }
    if let Err(e) =
        nats_events::publish_host_revoked(state.nats.as_ref(), &meeting_id, &user_id, &user_id)
            .await
    {
        tracing::error!(
            "NATS publish failed for HOST_REVOKED (transfer) in room {meeting_id}: {e}"
        );
    }

    // Two internal fanout events so chat_server updates both users' cached
    // is_host flags across all sessions: target true, caller false.
    nats_events::publish_internal_host_change(
        state.nats.as_ref(),
        &nats_events::MeetingHostChangePayload {
            room_id: meeting_id.clone(),
            user_id: body.user_id.clone(),
            is_host: true,
        },
    )
    .await;
    nats_events::publish_internal_host_change(
        state.nats.as_ref(),
        &nats_events::MeetingHostChangePayload {
            room_id: meeting_id.clone(),
            user_id: user_id.clone(),
            is_host: false,
        },
    )
    .await;

    Ok(Json(APIResponse::ok(())))
}

/// Max accepted `user_id` length on host endpoints.
const MAX_USER_ID_LEN: usize = 254;

/// `POST /api/v1/meetings/{meeting_id}/transfer-host`.
///
/// Atomically promotes the target and demotes the issuing host in a single DB
/// transaction — the only sanctioned self-demotion. If the target is not an
/// admitted participant the transaction rolls back and the caller keeps host
/// (so the meeting is never left without a successor).
///
/// Event ordering: `HOST_GRANTED(target)` is published BEFORE
/// `HOST_REVOKED(caller)` so no client ever observes a transient hostless gap.
pub async fn transfer_host(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(meeting_id): Path<String>,
    Json(body): Json<TransferHostRequest>,
) -> Result<Json<APIResponse<()>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;
    if meeting.state.as_deref() == Some("ended") {
        return Err(AppError::meeting_not_found(&meeting_id));
    }
    require_host(&state, meeting.id, &user_id).await?;

    if body.user_id.is_empty() {
        return Err(AppError::bad_request("user_id must not be empty"));
    }
    if body.user_id.len() > MAX_USER_ID_LEN {
        return Err(AppError::bad_request("user_id too long"));
    }
    if body.user_id == user_id {
        return Err(AppError::bad_request("cannot transfer host to yourself"));
    }
    if body.user_id.starts_with(GUEST_USER_ID_PREFIX) {
        return Err(AppError::bad_request(
            "cannot transfer host to a guest participant",
        ));
    }

    let transferred =
        db_participants::transfer_host(&state.db, meeting.id, &user_id, &body.user_id)
            .await
            .map_err(|e| {
                tracing::error!(
                    "DB transfer_host failed from {user_id} to {} in room {meeting_id}: {e}",
                    body.user_id
                );
                AppError::internal("failed to transfer host")
            })?;
    if transferred.is_none() {
        return Err(AppError::bad_request(
            "target is not an admitted participant",
        ));
    }

    // Promotion event first so no client observes a transient hostless gap.
    if let Err(e) =
        nats_events::publish_host_granted(state.nats.as_ref(), &meeting_id, &body.user_id, &user_id)
            .await
    {
        tracing::error!(
            "NATS publish failed for HOST_GRANTED (transfer) in room {meeting_id}: {e}"
        );
    }
    if let Err(e) =
        nats_events::publish_host_revoked(state.nats.as_ref(), &meeting_id, &user_id, &user_id)
            .await
    {
        tracing::error!(
            "NATS publish failed for HOST_REVOKED (transfer) in room {meeting_id}: {e}"
        );
    }

    // Two internal fanout events so chat_server updates both users' cached
    // is_host flags across all sessions: target true, caller false.
    nats_events::publish_internal_host_change(
        state.nats.as_ref(),
        &nats_events::MeetingHostChangePayload {
            room_id: meeting_id.clone(),
            user_id: body.user_id.clone(),
            is_host: true,
        },
    )
    .await;
    nats_events::publish_internal_host_change(
        state.nats.as_ref(),
        &nats_events::MeetingHostChangePayload {
            room_id: meeting_id.clone(),
            user_id: user_id.clone(),
            is_host: false,
        },
    )
    .await;

    Ok(Json(APIResponse::ok(())))
}

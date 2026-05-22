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
    requests::{DisableVideoParticipantRequest, KickParticipantRequest, MuteParticipantRequest},
    responses::APIResponse,
};

use crate::auth::AuthUser;
use crate::db::{meetings as db_meetings, participants as db_participants};
use crate::error::AppError;
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

    nats_events::publish_host_mute(state.nats.as_ref(), &meeting_id, &body.user_id)
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

    nats_events::publish_host_mute(state.nats.as_ref(), &meeting_id, "")
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

    nats_events::publish_host_disable_video(state.nats.as_ref(), &meeting_id, &body.user_id)
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

    nats_events::publish_host_disable_video(state.nats.as_ref(), &meeting_id, "")
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
    Ok(Json(APIResponse::ok(())))
}

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

//! In-app recording registration (#2856).

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use videocall_meeting_types::responses::APIResponse;
use videocall_types::validation::validate_meeting_id;

use crate::auth::AuthUser;
use crate::db::recordings::{self as db_recordings, RegisterOutcome};
use crate::error::AppError;
use crate::nats_events;
use crate::password::ClientAddr;
use crate::recording::{CookieOrigin, LeaseSecret, Rejection};
use crate::routes::valid_meeting_id::ValidMeetingId;
use crate::state::AppState;

/// Largest accepted stop body; a lease secret is 43 bytes.
pub const MAX_STOP_BODY: usize = 256;

#[derive(Debug, Deserialize)]
pub struct RegisterRecordingRequest {
    pub attempt_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct RegisterRecordingResponse {
    pub recording_id: Uuid,
    pub lease_secret: LeaseSecret,
    pub epoch: u64,
    pub version: i64,
}

/// `POST /api/v1/meetings/{meeting_id}/recordings`.
pub async fn register(
    State(state): State<AppState>,
    _origin: CookieOrigin,
    AuthUser { user_id, .. }: AuthUser,
    ValidMeetingId(meeting_id): ValidMeetingId,
    Json(body): Json<RegisterRecordingRequest>,
) -> Result<impl IntoResponse, AppError> {
    if !state.recording.register_limiter.allow(&user_id) {
        return Err(Rejection::RateLimited.into());
    }
    let secret = LeaseSecret::generate();
    let outcome = db_recordings::register(
        &state.db,
        &meeting_id,
        &user_id,
        body.attempt_id,
        &secret.hash(),
    )
    .await?;
    match outcome {
        RegisterOutcome::MeetingNotFound => Err(AppError::meeting_not_found(&meeting_id)),
        RegisterOutcome::Rejected { reason, snapshot } => {
            if let Some(snapshot) = snapshot {
                nats_events::publish_recording_state(state.nats.as_ref(), &meeting_id, &snapshot)
                    .await;
            }
            Err(reason.into())
        }
        RegisterOutcome::Granted {
            recording_id,
            snapshot,
        } => {
            nats_events::publish_recording_state(state.nats.as_ref(), &meeting_id, &snapshot).await;
            Ok((
                [(header::CACHE_CONTROL, "no-store")],
                Json(APIResponse::ok(RegisterRecordingResponse {
                    recording_id,
                    lease_secret: secret,
                    epoch: snapshot.epoch,
                    version: snapshot.version,
                })),
            ))
        }
    }
}

/// `POST /api/v1/meetings/{meeting_id}/recordings/{recording_id}/stop`, body =
/// the lease secret as `text/plain` and the only credential. `204` for any body
/// within [`MAX_STOP_BODY`].
pub async fn stop(
    State(state): State<AppState>,
    ClientAddr(client_ip): ClientAddr,
    Path((meeting_id, recording_id)): Path<(String, String)>,
    body: Bytes,
) -> StatusCode {
    let within_limits = client_ip
        .is_none_or(|ip| state.recording.stop_ip_limiter.allow(&ip.to_string()))
        && state.recording.stop_global_limiter.allow("");
    let parsed = (
        validate_meeting_id(&meeting_id),
        Uuid::parse_str(&recording_id),
        std::str::from_utf8(&body),
    );
    if let (true, (Ok(()), Ok(recording_id), Ok(secret))) = (within_limits, parsed) {
        let hash = LeaseSecret::from_wire(secret).hash();
        match db_recordings::stop(&state.db, &meeting_id, recording_id, &hash).await {
            Ok(Some(snapshot)) => {
                nats_events::publish_recording_state(state.nats.as_ref(), &meeting_id, &snapshot)
                    .await
            }
            Ok(None) => {}
            Err(e) => tracing::error!("recording stop failed in room {meeting_id}: {e}"),
        }
    }
    StatusCode::NO_CONTENT
}

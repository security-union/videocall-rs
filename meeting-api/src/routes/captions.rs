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

//! Live-caption control, dark-launched behind `CAPTIONS_ENABLED`.

use axum::{
    extract::{FromRequestParts, State},
    http::{request::Parts, StatusCode},
    Json,
};
use serde::Deserialize;
use videocall_meeting_types::APIError;

use crate::auth::RoomMember;
use crate::captions::{control_payload, publish_captions_control, publish_with_retry};
use crate::db::{meetings as db_meetings, participants as db_participants};
use crate::error::AppError;
use crate::routes::host::ensure_host;
use crate::routes::valid_meeting_id::ValidMeetingId;
use crate::state::AppState;

pub const MAX_BODY: usize = 256;

fn error(status: StatusCode, code: &str, message: &str) -> AppError {
    AppError::new(
        status,
        APIError {
            code: code.to_string(),
            message: message.to_string(),
            engineering_error: None,
        },
    )
}

/// Generic 404 unless captions are enabled; runs before auth.
pub struct CaptionsEnabled;

impl FromRequestParts<AppState> for CaptionsEnabled {
    type Rejection = AppError;

    async fn from_request_parts(_: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        if state.captions_enabled {
            Ok(Self)
        } else {
            Err(AppError::feature_not_found())
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CaptionsRequest {
    pub enabled: bool,
    #[serde(default)]
    pub renew: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionsAction {
    Enable,
    Renew,
    Revoke,
}

impl CaptionsAction {
    pub fn from_request(body: &CaptionsRequest) -> Result<Self, AppError> {
        match (body.enabled, body.renew) {
            (true, false) => Ok(Self::Enable),
            (true, true) => Ok(Self::Renew),
            (false, false) => Ok(Self::Revoke),
            (false, true) => Err(AppError::bad_request("renew requires enabled=true")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Enable => "enable",
            Self::Renew => "renew",
            Self::Revoke => "revoke",
        }
    }
}

/// `POST /api/v1/meetings/{meeting_id}/captions` with `{"enabled": bool, "renew": bool}`.
pub async fn set_captions(
    _: CaptionsEnabled,
    State(state): State<AppState>,
    ValidMeetingId(meeting_id): ValidMeetingId,
    RoomMember {
        user_id,
        meeting_id: token_meeting_id,
        ..
    }: RoomMember,
    Json(body): Json<CaptionsRequest>,
) -> Result<StatusCode, AppError> {
    if token_meeting_id != meeting_id {
        return Err(error(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "Room token is not valid for this meeting",
        ));
    }
    let action = CaptionsAction::from_request(&body)?;
    if action != CaptionsAction::Revoke
        && !state
            .captions_rate_limiter
            .allow(&format!("{meeting_id}\u{0}{user_id}"))
    {
        return Err(AppError::rate_limit_exceeded());
    }

    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .filter(|m| m.state.as_deref() != Some(db_meetings::STATE_ENDED))
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;

    let row = db_participants::get_status(&state.db, meeting.id, &user_id).await?;
    ensure_host(row.as_ref())?;

    let mut tx = state.db.begin().await?;
    db_meetings::lock_for_captions(&mut tx, meeting.id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;
    let row = db_participants::get_status_for_share(&mut tx, meeting.id, &user_id).await?;
    ensure_host(row.as_ref())?;
    let nats = state
        .nats
        .as_ref()
        .filter(|c| c.connection_state() == async_nats::connection::State::Connected)
        .ok_or_else(captions_unavailable)?;
    let generation = match action {
        CaptionsAction::Enable => db_meetings::enable_captions(&mut *tx, meeting.id)
            .await?
            .ok_or_else(meeting_not_active)?,
        CaptionsAction::Renew => db_meetings::captions_generation_if_on(&mut *tx, meeting.id)
            .await?
            .ok_or_else(captions_off)?,
        CaptionsAction::Revoke => db_meetings::revoke_captions(&mut *tx, meeting.id)
            .await?
            .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?,
    };
    tx.commit().await?;

    let payload = control_payload(
        meeting_id,
        user_id,
        action != CaptionsAction::Revoke,
        generation,
    );
    if let Err(e) = publish_captions_control(nats, &payload).await {
        tracing::warn!(
            meeting_id = %payload.room_id,
            requested_by = %payload.requested_by,
            action = action.as_str(),
            generation,
            outcome = "publish_deferred",
            "captions control: NATS publish failed, retrying in the background: {e}"
        );
        tokio::spawn(publish_with_retry(nats.clone(), payload.clone()));
        return Ok(StatusCode::NO_CONTENT);
    }
    if action == CaptionsAction::Renew {
        tracing::debug!(
            meeting_id = %payload.room_id,
            requested_by = %payload.requested_by,
            action = action.as_str(),
            generation,
            "captions control"
        );
    } else {
        tracing::info!(
            meeting_id = %payload.room_id,
            requested_by = %payload.requested_by,
            action = action.as_str(),
            generation,
            "captions control"
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

fn meeting_not_active() -> AppError {
    error(
        StatusCode::CONFLICT,
        "MEETING_NOT_ACTIVE",
        "Captions can only be turned on while the meeting is active",
    )
}

/// Also answers a renew once the meeting is no longer active: the row keeps
/// `captions_on` until the server revokes it, but no lease is renewed.
fn captions_off() -> AppError {
    error(
        StatusCode::CONFLICT,
        "CAPTIONS_OFF",
        "Captions are off for this meeting",
    )
}

fn captions_unavailable() -> AppError {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "CAPTIONS_UNAVAILABLE",
        "Captions are temporarily unavailable",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(enabled: bool, renew: bool) -> CaptionsRequest {
        CaptionsRequest { enabled, renew }
    }

    #[test]
    fn request_maps_to_action() {
        assert_eq!(
            CaptionsAction::from_request(&request(true, false)).unwrap(),
            CaptionsAction::Enable
        );
        assert_eq!(
            CaptionsAction::from_request(&request(true, true)).unwrap(),
            CaptionsAction::Renew
        );
        assert_eq!(
            CaptionsAction::from_request(&request(false, false)).unwrap(),
            CaptionsAction::Revoke
        );
        let err = CaptionsAction::from_request(&request(false, true)).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn renew_defaults_to_false() {
        let body: CaptionsRequest = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(!body.renew);
    }
}

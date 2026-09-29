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

use axum::{
    extract::{FromRequestParts, Path},
    http::request::Parts,
    response::{IntoResponse, Response},
};
use videocall_types::validation::validate_meeting_id;

use crate::error::AppError;

/// The `{meeting_id}` path segment, rejected with 400 `INVALID_MEETING_ID`
/// unless it passes [`validate_meeting_id`].
///
/// Taken by every `{meeting_id}` handler that changes meeting or participant
/// state except leave, leave-guest, end and delete, which keep `Path<String>`
/// so a row created before the rule can still be cleaned up.
pub struct ValidMeetingId(pub String);

impl<S: Send + Sync> FromRequestParts<S> for ValidMeetingId {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(meeting_id) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        validate_meeting_id(&meeting_id).map_err(|e| AppError::from(e).into_response())?;
        Ok(Self(meeting_id))
    }
}

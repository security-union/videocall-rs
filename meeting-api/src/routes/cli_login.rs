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

//! `GET /cli/authorize`: where `videocall-cli auth login` sends the browser.
//!
//! The CLI only knows the API origin, so this hands the request to the app's
//! consent page as `{ui origin}/devices/authorize?request=<base64url of the
//! raw query>`. Base64url, because the app's router percent-decodes and
//! rewrites a plain query, which corrupts values containing `+` or `%`.
//! Nothing is validated or stored here: the app validates with the shared
//! helpers and the API validates again on approve. The target origin and
//! path are fixed, so this is not an open redirect.

use axum::extract::{RawQuery, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use videocall_meeting_types::device as codes;
use videocall_meeting_types::device::{
    encode_authorize_request, APP_AUTHORIZE_PATH, AUTHORIZE_REQUEST_PARAM, MAX_AUTHORIZE_QUERY_LEN,
};
use videocall_meeting_types::APIError;

use crate::error::AppError;
use crate::state::AppState;

/// `GET /cli/authorize`: `302` to `{ui origin}/devices/authorize?request=…`.
/// Needs only the audience and the app origin; signing in is the app's
/// business, so OAuth is not required.
pub async fn authorize(State(state): State<AppState>, RawQuery(query): RawQuery) -> Response {
    let (Some(_), Some(ui_origin)) = (
        state.device.audience.as_deref(),
        state.device.ui_origin.as_deref(),
    ) else {
        return AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            APIError {
                code: codes::NOT_CONFIGURED.to_string(),
                message: "Device sign-in is not configured on this server.".to_string(),
                engineering_error: None,
            },
        )
        .into_response();
    };
    let query = query.unwrap_or_default();
    let bad_request = || {
        AppError::new(
            StatusCode::BAD_REQUEST,
            APIError {
                code: codes::INVALID_REQUEST.to_string(),
                message: "The sign-in link is malformed.".to_string(),
                engineering_error: None,
            },
        )
        .into_response()
    };
    if query.len() > MAX_AUTHORIZE_QUERY_LEN {
        return bad_request();
    }
    let request = encode_authorize_request(&query);
    let location = format!("{ui_origin}{APP_AUTHORIZE_PATH}?{AUTHORIZE_REQUEST_PARAM}={request}");
    // Base64url and a parsed origin are always valid header bytes.
    let Ok(location) = HeaderValue::from_str(&location) else {
        return bad_request();
    };
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, location),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
    )
        .into_response()
}

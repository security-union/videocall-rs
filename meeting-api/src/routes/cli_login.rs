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

//! `GET /cli/authorize`: hands the CLI's request to the app's consent page as
//! `?request=<base64url of the raw query>`, since the app's router corrupts `+` and `%`.
//! Nothing is validated or stored here; the target is fixed, so this is not an open redirect.

use axum::extract::{RawQuery, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use videocall_meeting_types::device::{
    self as codes, encode_authorize_request, APP_AUTHORIZE_PATH, AUTHORIZE_REQUEST_PARAM,
    MAX_AUTHORIZE_QUERY_LEN,
};

use super::devices_api::{api_error, not_configured};
use crate::state::AppState;

/// `GET /cli/authorize`: needs only the audience and the app origin, not OAuth.
pub async fn authorize(State(state): State<AppState>, RawQuery(query): RawQuery) -> Response {
    let (Some(_), Some(ui_origin)) = (
        state.device.audience.as_deref(),
        state.device.ui_origin.as_deref(),
    ) else {
        return not_configured().into_response();
    };
    let query = query.unwrap_or_default();
    let bad_request = || {
        api_error(
            StatusCode::BAD_REQUEST,
            codes::INVALID_REQUEST,
            "The sign-in link is malformed.",
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

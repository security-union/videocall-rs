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

//! The device JSON API.
//!
//! Browser session (`AuthUser`, `APIResponse` envelope): approve a CLI login
//! (`POST /api/v1/devices/authorizations`), list the user's devices
//! (`GET /api/v1/devices`), revoke one (`DELETE /api/v1/devices/{id}`). The
//! state-changing two also pass [`check_browser_write`].
//!
//! CLI (bare JSON, `DeviceErrorResponse` on failure): the code exchange
//! (`POST /api/v1/devices`) and the device's own status and self-revoke
//! (`GET`/`DELETE /api/v1/devices/me`).

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use url::form_urlencoded;
use videocall_meeting_types::device::{
    self as codes, fingerprint, is_valid_client_state, is_valid_code_challenge, is_valid_device_id,
    normalize_device_name, parse_redirect_uri, CreateDeviceAuthorizationRequest,
    CreateDeviceAuthorizationResponse, DeviceErrorResponse, DeviceListItem, DeviceStatusResponse,
    RedirectUri, RegisterDeviceRequest, RegisterDeviceResponse, CODE_CHALLENGE_METHOD_S256,
};
use videocall_meeting_types::{APIError, APIResponse};

use crate::auth::{device_unauthorized, extract_bearer_token, AuthUser};
use crate::db::device_codes::{self as db_codes, DeviceCodeRow};
use crate::db::devices::{self as db_devices, CappedInsert, DeviceRow, NewDevice};
use crate::device_auth::{
    self, is_valid_code_verifier, validate_public_key, verify_device_jwt, verify_registration,
    AllowedStates, DeviceAuthError,
};
use crate::error::AppError;
use crate::secrets::{ct_eq, pkce_s256, random_base64url, sha256_hex};
use crate::state::AppState;

/// Largest accepted body on the two POST routes.
pub const MAX_REGISTER_BODY: usize = 4 * 1024;
/// A device dies after this long unused.
pub const DEVICE_IDLE_TTL_SECS: i64 = 90 * 24 * 3600;
/// A device dies this long after approval, used or not.
pub const DEVICE_ABS_TTL_SECS: i64 = 365 * 24 * 3600;
/// Live devices per user.
pub const MAX_DEVICES_PER_USER: i64 = 10;
/// Code lifetimes: loopback delivery is immediate, a remote code is pasted.
pub const LOOPBACK_CODE_TTL_SECS: i64 = 60;
pub const REMOTE_CODE_TTL_SECS: i64 = 300;

fn error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(DeviceErrorResponse {
            error: error.to_string(),
            error_description: description.to_string(),
        }),
    )
        .into_response()
}

/// One description for every rejected grant, so a caller cannot tell a
/// wrong code from a wrong verifier, redirect or key.
fn invalid_grant() -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "invalid_grant",
        "The sign-in code is invalid, expired or already used. Run videocall-cli auth login again.",
    )
}

fn server_error(e: sqlx::Error) -> Response {
    tracing::error!("database error: {e}");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "Internal error. Try again in a moment.",
    )
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
}

fn ts(value: DateTime<Utc>) -> i64 {
    value.timestamp()
}

/// `POST /api/v1/devices`
pub async fn register(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(audience) = state.device.audience.clone() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_configured",
            "Device sign-in is not configured on this server.",
        );
    };
    if !is_json(&headers) {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_request",
            "Content-Type must be application/json.",
        );
    }
    let request = match serde_json::from_slice::<RegisterDeviceRequest>(&body) {
        Ok(request) if is_valid_code_verifier(&request.code_verifier) => request,
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Malformed request.",
            )
        }
    };
    if request.code.is_empty() || request.code.len() > 128 {
        return invalid_grant();
    }

    // Burn the code first, in its own autocommitted statement: whatever
    // fails below, it cannot be tried again.
    let now = Utc::now();
    let row = match db_codes::consume_code(&state.db, &sha256_hex(&request.code), now).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::info!("device registration rejected: unknown, used or expired code");
            return invalid_grant();
        }
        Err(e) => return server_error(e),
    };
    let user_id = row.user_id.as_str();
    let check = || {
        if request.redirect_uri != row.redirect_uri {
            return Err("redirect_uri mismatch");
        }
        if !ct_eq(&pkce_s256(&request.code_verifier), &row.code_challenge) {
            return Err("PKCE mismatch");
        }
        let (claims, raw_key) = verify_registration(&request.registration, &audience, now)
            .map_err(|_| "registration JWS")?;
        if claims.public_key != row.public_key || claims.name != row.device_name {
            return Err("key or name differs from the approval");
        }
        Ok(raw_key)
    };
    let raw_key = match check() {
        Ok(raw_key) => raw_key,
        Err(reason) => {
            tracing::info!(user_id, reason, "device registration rejected");
            return invalid_grant();
        }
    };

    let device_id = random_base64url(16);
    let fingerprint = fingerprint(&raw_key);
    let abs_expires_at = now + Duration::seconds(DEVICE_ABS_TTL_SECS);
    let idle_expires_at = now + Duration::seconds(DEVICE_IDLE_TTL_SECS);
    let device = NewDevice {
        id: &device_id,
        public_key: &row.public_key,
        fingerprint: &fingerprint,
        name: &row.device_name,
        user_id,
        user_name: &row.user_name,
        now,
        idle_expires_at,
        abs_expires_at,
    };
    match db_devices::insert_device_capped(&state.db, &device, MAX_DEVICES_PER_USER).await {
        Ok(CappedInsert::Inserted) => {}
        Ok(CappedInsert::TooManyDevices) => {
            tracing::info!(user_id, "device registration rejected: too many devices");
            return error(
                StatusCode::CONFLICT,
                "too_many_devices",
                "You have too many devices. Revoke one at /devices and sign in again.",
            );
        }
        Err(e) if db_devices::is_duplicate_public_key(&e) => {
            return error(
                StatusCode::CONFLICT,
                "key_already_registered",
                "This device key is already registered. Run videocall-cli auth login again.",
            );
        }
        Err(e) => return server_error(e),
    }
    tracing::info!(device_id, fingerprint, user_id, "device registered");
    (
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(RegisterDeviceResponse {
            device_id,
            fingerprint,
            name: row.device_name.clone(),
            user_id: user_id.to_string(),
            user_name: row.user_name.clone(),
            idle_expires_at: ts(idle_expires_at),
            abs_expires_at: ts(abs_expires_at),
        }),
    )
        .into_response()
}

/// Authenticate the calling device from its bearer JWT.
async fn calling_device(
    state: &AppState,
    headers: &HeaderMap,
    allowed: AllowedStates,
) -> Result<DeviceRow, Box<Response>> {
    let (Some(audience), Some(token)) = (
        state.device.audience.as_deref(),
        extract_bearer_token(headers),
    ) else {
        return Err(Box::new(device_unauthorized()));
    };
    match verify_device_jwt(&state.db, &token, audience, Utc::now(), allowed).await {
        Ok(row) => Ok(row),
        Err(DeviceAuthError::Db(e)) => Err(Box::new(server_error(e))),
        Err(e) => {
            tracing::info!("device token rejected on /devices/me: {e}");
            Err(Box::new(device_unauthorized()))
        }
    }
}

/// `GET /api/v1/devices/me`
pub async fn me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let row = match calling_device(&state, &headers, AllowedStates::LiveApproved).await {
        Ok(row) => row,
        Err(response) => return *response,
    };
    device_auth::touch_live_device(&state.db, &row, Utc::now()).await;
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(DeviceStatusResponse {
            device_id: row.id,
            name: row.name,
            fingerprint: row.fingerprint,
            user_id: row.user_id,
            user_name: row.user_name,
            created_at: ts(row.created_at),
            last_used_at: ts(row.last_used_at),
            idle_expires_at: ts(row.idle_expires_at),
            abs_expires_at: ts(row.abs_expires_at),
        }),
    )
        .into_response()
}

/// `DELETE /api/v1/devices/me`
pub async fn revoke_me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let row = match calling_device(&state, &headers, AllowedStates::AnyNotRevoked).await {
        Ok(row) => row,
        Err(response) => return *response,
    };
    match db_devices::revoke(&state.db, &row.id, &row.user_id, "device", Utc::now()).await {
        Ok(_) => {
            tracing::info!(device_id = %row.id, "device revoked itself");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => server_error(e),
    }
}

// ---------------------------------------------------------------------------
// Browser routes
// ---------------------------------------------------------------------------

fn api_error(status: StatusCode, code: &str, message: &str) -> AppError {
    AppError::new(
        status,
        APIError {
            code: code.to_string(),
            message: message.to_string(),
            engineering_error: None,
        },
    )
}

/// CSRF guard for the state-changing browser routes. The session cookie is
/// `SameSite=Lax` but shared with sibling subdomains, so the exact `Origin`
/// allowlist is the control: a missing `Origin`, `Origin: null` or any other
/// origin gets 403. With `require_json`, anything but `application/json`
/// (which an HTML form cannot send) gets 415.
pub fn check_browser_write(
    state: &AppState,
    headers: &HeaderMap,
    require_json: bool,
) -> Result<(), AppError> {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if !origin.is_some_and(|origin| state.device.browser_origins.iter().any(|o| o == origin)) {
        tracing::info!(
            origin = origin.unwrap_or("-"),
            "device route rejected: Origin"
        );
        return Err(api_error(
            StatusCode::FORBIDDEN,
            codes::ORIGIN_NOT_ALLOWED,
            "This request must come from the videocall app.",
        ));
    }
    if require_json && !is_json(headers) {
        return Err(api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            codes::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json.",
        ));
    }
    Ok(())
}

fn no_store<T: serde::Serialize>(result: T) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(APIResponse::ok(result)),
    )
        .into_response()
}

/// `POST /api/v1/devices/authorizations`: approve a CLI login. Every check
/// runs before anything is written; then the user's previous code and all
/// expired codes are replaced by the new code's SHA-256. The code itself
/// goes only into the response.
pub async fn create_authorization(
    State(state): State<AppState>,
    user: AuthUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    if state.device.audience.is_none() || state.device.ui_origin.is_none() {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            codes::NOT_CONFIGURED,
            "Device sign-in is not configured on this server.",
        ));
    }
    check_browser_write(&state, &headers, true)?;
    let invalid = || {
        api_error(
            StatusCode::BAD_REQUEST,
            codes::INVALID_REQUEST,
            "This sign-in request is invalid. Run videocall-cli auth login again.",
        )
    };
    let request: CreateDeviceAuthorizationRequest =
        serde_json::from_slice(&body).map_err(|_| invalid())?;
    let target = parse_redirect_uri(&request.redirect_uri)
        .filter(|target| {
            request.code_challenge_method == CODE_CHALLENGE_METHOD_S256
                && is_valid_code_challenge(&request.code_challenge)
                && (*target == RedirectUri::Remote || is_valid_client_state(&request.state))
        })
        .ok_or_else(invalid)?;
    if target == RedirectUri::Remote && !request.confirm {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            codes::CONFIRMATION_REQUIRED,
            "Confirm that you started this sign-in yourself, on a device you own.",
        ));
    }
    let raw_key = validate_public_key(&request.public_key).ok_or_else(invalid)?;
    if db_devices::public_key_exists(&state.db, &request.public_key).await? {
        return Err(api_error(
            StatusCode::CONFLICT,
            codes::KEY_ALREADY_REGISTERED,
            "This device key is already registered. Run videocall-cli auth login again.",
        ));
    }
    let device_name = normalize_device_name(&request.device_name).ok_or_else(invalid)?;
    let now = Utc::now();
    if db_devices::count_live_devices(&state.db, &user.user_id, now).await? >= MAX_DEVICES_PER_USER
    {
        return Err(api_error(
            StatusCode::CONFLICT,
            codes::TOO_MANY_DEVICES,
            "You have too many devices. Revoke one and try again.",
        ));
    }

    let code = random_base64url(32);
    let ttl = match target {
        RedirectUri::Loopback { .. } => LOOPBACK_CODE_TTL_SECS,
        RedirectUri::Remote => REMOTE_CODE_TTL_SECS,
    };
    let row = DeviceCodeRow {
        code_hash: sha256_hex(&code),
        user_id: user.user_id,
        user_name: user.name,
        public_key: request.public_key,
        device_name,
        code_challenge: request.code_challenge,
        redirect_uri: target.as_uri(),
        expires_at: now + Duration::seconds(ttl),
    };
    db_codes::replace_code(&state.db, &row, now).await?;
    tracing::info!(
        user_id = %row.user_id,
        fingerprint = %fingerprint(&raw_key),
        remote = target == RedirectUri::Remote,
        "device login approved"
    );

    Ok(no_store(match target {
        RedirectUri::Loopback { port } => {
            let query = form_urlencoded::Serializer::new(String::new())
                .append_pair("code", &code)
                .append_pair("state", &request.state)
                .finish();
            CreateDeviceAuthorizationResponse::Loopback {
                redirect_to: format!("http://127.0.0.1:{port}/callback?{query}"),
            }
        }
        RedirectUri::Remote => CreateDeviceAuthorizationResponse::Remote {
            code,
            expires_in: ttl,
        },
    }))
}

/// `GET /api/v1/devices`: the signed-in user's live devices.
pub async fn list_devices(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Response, AppError> {
    let rows = db_devices::list_live_devices(&state.db, &user.user_id, Utc::now()).await?;
    let items: Vec<DeviceListItem> = rows
        .into_iter()
        .map(|row| DeviceListItem {
            expires_at: ts(row.idle_expires_at.min(row.abs_expires_at)),
            id: row.id,
            name: row.name,
            fingerprint: row.fingerprint,
            created_at: ts(row.created_at),
            last_used_at: ts(row.last_used_at),
        })
        .collect();
    Ok(no_store(items))
}

/// `DELETE /api/v1/devices/{id}`. Unknown ids and other users' devices get
/// the same 404.
pub async fn revoke_device(
    State(state): State<AppState>,
    user: AuthUser,
    headers: HeaderMap,
    Path(device_id): Path<String>,
) -> Result<Response, AppError> {
    check_browser_write(&state, &headers, false)?;
    let revoked = is_valid_device_id(&device_id)
        && db_devices::revoke(&state.db, &device_id, &user.user_id, "user", Utc::now()).await?;
    if !revoked {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            codes::DEVICE_NOT_FOUND,
            "No such device.",
        ));
    }
    tracing::info!(device_id, user_id = %user.user_id, "device revoked by user");
    Ok(StatusCode::NO_CONTENT.into_response())
}

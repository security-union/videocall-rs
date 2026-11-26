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

//! The device JSON API used by `videocall-cli`.
//!
//! - `POST /api/v1/devices`: exchange the one-time code (+ PKCE verifier and
//!   a registration JWS signed by the new key) for an approved device.
//! - `GET /api/v1/devices/me`: the calling device's status.
//! - `DELETE /api/v1/devices/me`: the device revokes itself.
//!
//! Success bodies are bare JSON; failures are `{error, error_description}`.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use videocall_meeting_types::device::{
    fingerprint, DeviceErrorResponse, DeviceStatus, DeviceStatusResponse, RegisterDeviceRequest,
    RegisterDeviceResponse,
};

use crate::auth::{device_unauthorized, extract_bearer_token};
use crate::db::cli_auth::{self as db_cli, CliAuthRequestRow};
use crate::db::devices::{self as db_devices, CappedInsert, DeviceRow, NewDevice};
use crate::device_auth::{
    self, is_valid_code_verifier, verify_device_jwt, verify_registration, AllowedStates,
    DeviceAuthError, RedirectTarget,
};
use crate::secrets::{ct_eq, pkce_s256, sha256_hex};
use crate::state::AppState;

/// Largest accepted `POST /api/v1/devices` body.
pub const MAX_REGISTER_BODY: usize = 4 * 1024;

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

fn invalid_request(description: &str) -> Response {
    error(StatusCode::BAD_REQUEST, "invalid_request", description)
}

fn server_error(e: sqlx::Error) -> Response {
    tracing::error!("database error: {e}");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "Internal error. Try again in a moment.",
    )
}

fn not_configured() -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "not_configured",
        "Device sign-in is not configured on this server.",
    )
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
}

/// The `redirect_uri` the stored request was created with, rebuilt from
/// its validated columns.
fn stored_redirect_uri(row: &CliAuthRequestRow, api_origin: &str) -> Option<String> {
    let target = match (&row.redirect_host, row.redirect_port) {
        (Some(host), Some(port)) => RedirectTarget::Loopback {
            host: host.clone(),
            port: u16::try_from(port).ok()?,
        },
        (None, None) => RedirectTarget::Remote,
        _ => return None,
    };
    Some(target.redirect_uri(api_origin))
}

fn ts(value: DateTime<Utc>) -> i64 {
    value.timestamp()
}

/// `POST /api/v1/devices`
pub async fn register(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(audience) = state.device.audience.clone() else {
        return not_configured();
    };
    if !is_json(&headers) {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_request",
            "Content-Type must be application/json.",
        );
    }
    if body.len() > MAX_REGISTER_BODY {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request",
            "Request body too large.",
        );
    }
    let Ok(request) = serde_json::from_slice::<RegisterDeviceRequest>(&body) else {
        return invalid_request("Malformed request body.");
    };
    if request.code.is_empty() || request.code.len() > 128 {
        return invalid_grant();
    }
    if !is_valid_code_verifier(&request.code_verifier) {
        return invalid_request("Malformed code_verifier.");
    }

    // Burn the code first, in its own autocommitted statement: whatever
    // fails below, it cannot be tried again.
    let now = Utc::now();
    let row = match db_cli::consume_code(&state.db, &sha256_hex(&request.code), now).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::info!("device registration rejected: unknown, used or expired code");
            return invalid_grant();
        }
        Err(e) => return server_error(e),
    };
    let request_id = row.id.as_str();

    if stored_redirect_uri(&row, &audience).as_deref() != Some(request.redirect_uri.as_str()) {
        tracing::info!(
            request_id,
            "device registration rejected: redirect_uri mismatch"
        );
        return invalid_grant();
    }
    if !ct_eq(&pkce_s256(&request.code_verifier), &row.code_challenge) {
        tracing::info!(request_id, "device registration rejected: PKCE mismatch");
        return invalid_grant();
    }
    let (claims, raw_key) = match verify_registration(&request.registration, &audience, now) {
        Ok(verified) => verified,
        Err(reason) => {
            tracing::info!(
                request_id,
                ?reason,
                "device registration rejected: registration JWS"
            );
            return invalid_grant();
        }
    };
    if claims.public_key != row.public_key || claims.name != row.device_name {
        tracing::info!(
            request_id,
            "device registration rejected: key or name differs from the request"
        );
        return invalid_grant();
    }
    let (Some(user_id), Some(user_name), Some(decided_at)) = (
        row.user_id.as_deref(),
        row.user_name.as_deref(),
        row.decided_at,
    ) else {
        return invalid_grant();
    };

    let device_id = device_auth::generate_device_id();
    let fingerprint = fingerprint(&raw_key);
    let config = &state.device;
    let abs_expires_at = now + Duration::seconds(config.abs_ttl_secs);
    let idle_expires_at = (now + Duration::seconds(config.idle_ttl_secs)).min(abs_expires_at);
    let device = NewDevice {
        id: &device_id,
        public_key: &row.public_key,
        fingerprint: &fingerprint,
        name: &row.device_name,
        user_id,
        user_name,
        requester_ip: row.requester_ip.as_deref(),
        now,
        idle_expires_at,
        abs_expires_at,
    };
    match db_devices::insert_device_capped(&state.db, &device, decided_at, config.max_per_user)
        .await
    {
        Ok(CappedInsert::Inserted) => {}
        Ok(CappedInsert::TooManyDevices(n)) => {
            tracing::info!(
                request_id,
                user_id,
                live = n,
                "device registration rejected: too many devices"
            );
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
    tracing::info!(
        request_id,
        device_id = %device_id,
        fingerprint = %fingerprint,
        user_id,
        "device registered"
    );
    (
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(RegisterDeviceResponse {
            device_id,
            fingerprint,
            name: row.device_name.clone(),
            user_id: user_id.to_string(),
            user_name: user_name.to_string(),
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
    let audience = state
        .device
        .audience
        .as_deref()
        .ok_or_else(|| Box::new(device_unauthorized()))?;
    let token = extract_bearer_token(headers).ok_or_else(|| Box::new(device_unauthorized()))?;
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
    let now = Utc::now();
    device_auth::touch_live_device(&state.db, &row, now).await;
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(DeviceStatusResponse {
            device_id: row.id,
            status: DeviceStatus::Approved,
            name: row.name,
            fingerprint: row.fingerprint,
            user_id: row.user_id,
            user_name: row.user_name,
            created_at: ts(row.created_at),
            decided_at: ts(row.decided_at),
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
    match db_devices::revoke_by_device(&state.db, &row.id, Utc::now()).await {
        Ok(_) => {
            tracing::info!(device_id = %row.id, "device revoked itself");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => server_error(e),
    }
}

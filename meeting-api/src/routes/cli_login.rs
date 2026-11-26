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

//! The browser half of `videocall-cli auth login` (authorization code +
//! PKCE, like `aws login`):
//!
//! 1. `GET /cli/authorize` validates everything, stores a request and
//!    redirects to the consent page. On any invalid input it renders an
//!    error page and never redirects.
//! 2. `GET /cli/consent?request=…` needs a browser session (cookie only) and
//!    renders the consent page with a per-render nonce.
//! 3. `POST /cli/consent` (Origin + nonce checked) records the decision. An
//!    approval mints a one-time code: a loopback login is redirected to the
//!    CLI's listener; a remote login gets the code on a no-store page.
//!
//! The code is exchanged at `POST /api/v1/devices` (see `devices_api`).

use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{Duration, Utc};
use serde::Deserialize;
use url::form_urlencoded;
use videocall_meeting_types::device::{is_valid_device_id, normalize_device_name, CLI_CLIENT_ID};

use crate::db::cli_auth::{
    self as db_cli, ConsentDecision, NewCliAuthRequest, LOOPBACK_CODE_TTL_SECS,
    REMOTE_CODE_TTL_SECS,
};
use crate::db::devices as db_devices;
use crate::db::SweepGate;
use crate::device_auth::{
    self, consent_path, is_valid_client_state, is_valid_code_challenge, parse_redirect_uri,
    validate_public_key, RedirectTarget,
};
use crate::html;
use crate::routes::device_pages::{
    api_origin, cap_key, check_form_post, client_ip, login_redirect, page_user, PeerAddr,
};
use crate::secrets::{random_base64url, sha256_hex};
use crate::state::AppState;

/// Query of `GET /cli/authorize`.
#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    pub response_type: Option<String>,
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
    pub state: Option<String>,
    pub public_key: Option<String>,
    pub device_name: Option<String>,
}

/// Validated `/cli/authorize` input.
struct Authorize {
    target: RedirectTarget,
    code_challenge: String,
    state: String,
    public_key: String,
    device_name: String,
}

fn validate_authorize(query: AuthorizeQuery, api_origin: &str) -> Result<Authorize, &'static str> {
    if query.response_type.as_deref() != Some("code") {
        return Err("response_type");
    }
    if query.client_id.as_deref() != Some(CLI_CLIENT_ID) {
        return Err("client_id");
    }
    if query.code_challenge_method.as_deref() != Some("S256") {
        return Err("code_challenge_method");
    }
    let target = query
        .redirect_uri
        .as_deref()
        .and_then(|uri| parse_redirect_uri(uri, api_origin))
        .ok_or("redirect_uri")?;
    let code_challenge = query
        .code_challenge
        .filter(|c| is_valid_code_challenge(c))
        .ok_or("code_challenge")?;
    let state = query
        .state
        .filter(|s| is_valid_client_state(s))
        .ok_or("state")?;
    let public_key = query
        .public_key
        .filter(|k| validate_public_key(k).is_some())
        .ok_or("public_key")?;
    let device_name = query
        .device_name
        .as_deref()
        .and_then(normalize_device_name)
        .ok_or("device_name")?;
    Ok(Authorize {
        target,
        code_challenge,
        state,
        public_key,
        device_name,
    })
}

/// `GET /cli/authorize`
pub async fn authorize(
    State(state): State<AppState>,
    peer: PeerAddr,
    headers: HeaderMap,
    query: Result<Query<AuthorizeQuery>, QueryRejection>,
) -> Response {
    let Some(origin) = api_origin(&state) else {
        return html::not_configured_page();
    };
    if !is_top_level_navigation(&headers) {
        tracing::info!("cli authorize rejected: not a top-level navigation");
        return html::authorize_error_page();
    }
    let Ok(Query(query)) = query else {
        return html::authorize_error_page();
    };
    let request = match validate_authorize(query, origin) {
        Ok(request) => request,
        Err(field) => {
            tracing::info!("cli authorize rejected: invalid {field}");
            return html::authorize_error_page();
        }
    };
    let now = Utc::now();
    let config = &state.device;

    // Caps first, cheapest and most selective first; sweeps only after the
    // request is accepted. Expired rows never count toward the caps, so a
    // late sweep cannot let more through.
    let client = client_ip(&headers, peer, config.trust_x_forwarded_for);
    let ip = client.map(|ip| ip.to_string());
    let cap_key = cap_key(client);
    match cap_key.as_deref() {
        Some(key) => match db_cli::count_open_requests(&state.db, now, Some(key)).await {
            Ok(n) if n >= config.max_pending_per_ip => {
                tracing::info!(
                    requester_cap_key = key,
                    "per-client open CLI login cap reached"
                );
                return html::busy_page();
            }
            Ok(_) => {}
            Err(e) => return db_error(e),
        },
        None if config.trust_x_forwarded_for => warn_infrastructure_address_once(ip.as_deref()),
        None => {}
    }
    match db_cli::count_open_requests(&state.db, now, None).await {
        Ok(n) if n >= config.max_pending => {
            tracing::warn!("global open CLI login cap reached ({n})");
            return html::busy_page();
        }
        Ok(_) => {}
        Err(e) => return db_error(e),
    }

    match db_devices::public_key_exists(&state.db, &request.public_key).await {
        Ok(true) => return html::key_registered_page(),
        Ok(false) => {}
        Err(e) => return db_error(e),
    }

    let id = device_auth::generate_device_id();
    let loopback = match &request.target {
        RedirectTarget::Loopback { host, port } => Some((host.as_str(), *port)),
        RedirectTarget::Remote => None,
    };
    let row = NewCliAuthRequest {
        id: &id,
        code_challenge: &request.code_challenge,
        loopback,
        client_state: &request.state,
        public_key: &request.public_key,
        device_name: &request.device_name,
        requester_ip: ip.as_deref(),
        requester_cap_key: cap_key.as_deref(),
        now,
    };
    if let Err(e) = db_cli::insert_request(&state.db, &row).await {
        return db_error(e);
    }
    tracing::info!(
        request_id = %id,
        public_key = %request.public_key,
        remote = loopback.is_none(),
        requester_ip = ip.as_deref().unwrap_or("-"),
        "cli login requested"
    );
    sweep_if_due(&state, now).await;
    (StatusCode::FOUND, [(header::LOCATION, consent_path(&id))]).into_response()
}

/// `/cli/authorize` creates state for an unauthenticated GET, so only a
/// top-level navigation may do it: an `<img>`, `fetch` or iframe on another
/// site is refused. Browsers that send no Fetch Metadata (old ones, curl)
/// are allowed.
fn is_top_level_navigation(headers: &HeaderMap) -> bool {
    let header_is = |name: &str, expected: &str| {
        headers
            .get(name)
            .map(|v| v.to_str().is_ok_and(|v| v.eq_ignore_ascii_case(expected)))
            .unwrap_or(true)
    };
    header_is("sec-fetch-mode", "navigate") && header_is("sec-fetch-dest", "document")
}

/// With `TRUST_X_FORWARDED_FOR=true` the resolved client should be a public
/// address. If it is not, the ingress is hiding client addresses and the
/// per-client cap is off; say so once per process.
fn warn_infrastructure_address_once(ip: Option<&str>) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            client_ip = ip.unwrap_or("-"),
            "TRUST_X_FORWARDED_FOR is on but the client address is loopback, private or \
             missing; the per-client CLI login cap is not applied. Check that the ingress \
             appends the real client address to X-Forwarded-For."
        );
    });
}

/// Throttle for the two sweeps `/cli/authorize` triggers.
static CLI_SWEEP_GATE: SweepGate = SweepGate::new();

/// Best effort, at most once per [`crate::db::SWEEP_INTERVAL_SECS`] per
/// process: drop long-expired login requests and long-dead devices.
async fn sweep_if_due(state: &AppState, now: chrono::DateTime<Utc>) {
    if !CLI_SWEEP_GATE.try_acquire(now.timestamp()) {
        return;
    }
    if let Err(e) = db_cli::sweep_requests(&state.db, now).await {
        tracing::warn!("sweeping cli_auth_requests failed: {e}");
    }
    if let Err(e) = db_devices::sweep_devices(&state.db, now).await {
        tracing::warn!("sweeping devices failed: {e}");
    }
}

fn db_error(e: sqlx::Error) -> Response {
    tracing::error!("database error: {e}");
    html::error_page(StatusCode::INTERNAL_SERVER_ERROR)
}

/// Query of `GET /cli/consent`.
#[derive(Debug, Deserialize)]
pub struct ConsentQuery {
    pub request: Option<String>,
}

/// `GET /cli/consent?request={id}`
pub async fn consent_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<ConsentQuery>, QueryRejection>,
) -> Response {
    let Some(origin) = api_origin(&state) else {
        return html::not_configured_page();
    };
    let Some(request_id) = query
        .ok()
        .and_then(|Query(q)| q.request)
        .filter(|id| is_valid_device_id(id))
    else {
        return html::invalid_page();
    };
    let Some(user) = page_user(&state, &headers) else {
        return login_redirect(&state, origin, &consent_path(&request_id));
    };

    let now = Utc::now();
    let nonce = random_base64url(32);
    let request = match db_cli::set_approval_nonce(
        &state.db,
        &request_id,
        &sha256_hex(&nonce),
        &user.user_id,
        now,
    )
    .await
    {
        Ok(Some(request)) => request,
        Ok(None) => return html::invalid_page(),
        Err(e) => return db_error(e),
    };
    let Some(fingerprint) = device_auth::fingerprint_of(&request.public_key) else {
        return html::invalid_page();
    };
    let mut response = html::confirm_page(&html::ConfirmView {
        viewer_name: &user.name,
        viewer_user_id: &user.user_id,
        request: &request,
        nonce: &nonce,
        fingerprint: &fingerprint,
        now,
    });
    // A loopback approval ends in a redirect to the CLI's listener; CSP
    // form-action governs that redirect, so allow exactly that URL.
    if let (Some(host), Some(port)) = (&request.redirect_host, request.redirect_port) {
        let callback = format!("http://{host}:{port}/callback");
        if let Ok(value) = HeaderValue::from_str(&html::csp_with_form_target(&callback)) {
            response
                .headers_mut()
                .insert(header::CONTENT_SECURITY_POLICY, value);
        }
    }
    response
}

#[derive(Debug, Default)]
struct ConsentForm {
    request: Option<String>,
    nonce: Option<String>,
    decision: Option<String>,
    confirm: Option<String>,
}

fn parse_consent_form(body: &[u8]) -> ConsentForm {
    let mut form = ConsentForm::default();
    for (key, value) in form_urlencoded::parse(body) {
        let slot = match key.as_ref() {
            "request" => &mut form.request,
            "nonce" => &mut form.nonce,
            "decision" => &mut form.decision,
            "confirm" => &mut form.confirm,
            _ => continue,
        };
        // First value wins; a repeated field cannot override it.
        if slot.is_none() {
            *slot = Some(value.into_owned());
        }
    }
    form
}

/// The loopback callback URL with `params`, built only from stored fields.
fn loopback_redirect(host: &str, port: i32, params: &[(&str, &str)]) -> Response {
    let query: String = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params)
        .finish();
    let location = format!("http://{host}:{port}/callback?{query}");
    (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response()
}

/// `POST /cli/consent`
pub async fn consent_decision(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(origin) = api_origin(&state) else {
        return html::not_configured_page();
    };
    if let Err(reason) = check_form_post(origin, &headers) {
        tracing::info!("consent POST rejected: {reason}");
        return html::rejected_page();
    }
    let Some(user) = page_user(&state, &headers) else {
        return html::signed_out_page();
    };
    let form = parse_consent_form(&body);
    let (Some(request_id), Some(nonce)) = (
        form.request.filter(|id| is_valid_device_id(id)),
        form.nonce.filter(|n| !n.is_empty() && n.len() <= 128),
    ) else {
        return html::invalid_page();
    };
    let now = Utc::now();
    // Read-only, to pick the rules (remote needs the checkbox). The decision
    // itself is one guarded UPDATE below.
    let request = match db_cli::find_open_request(&state.db, &request_id, now).await {
        Ok(Some(request)) => request,
        Ok(None) => return html::invalid_page(),
        Err(e) => return db_error(e),
    };
    let nonce_hash = sha256_hex(&nonce);
    let decision = ConsentDecision {
        request_id: &request_id,
        nonce_hash: &nonce_hash,
        user_id: &user.user_id,
        user_name: &user.name,
        now,
    };

    match form.decision.as_deref() {
        Some("approve") => {
            if request.is_remote() && form.confirm.as_deref() != Some("yes") {
                return html::confirm_required_page(&request_id);
            }
            match db_devices::count_live_devices(&state.db, &user.user_id, now).await {
                Ok(n) if n >= state.device.max_per_user => {
                    return html::too_many_devices_page(n);
                }
                Ok(_) => {}
                Err(e) => return db_error(e),
            }
            let code = random_base64url(32);
            let ttl = if request.is_remote() {
                REMOTE_CODE_TTL_SECS
            } else {
                LOOPBACK_CODE_TTL_SECS
            };
            let approved = match db_cli::approve_request(
                &state.db,
                &decision,
                &sha256_hex(&code),
                now + Duration::seconds(ttl),
            )
            .await
            {
                Ok(Some(row)) => row,
                Ok(None) => return html::invalid_page(),
                Err(e) => return db_error(e),
            };
            tracing::info!(
                request_id = %approved.id,
                public_key = %approved.public_key,
                user_id = %user.user_id,
                decision = "approved",
                "cli login decided"
            );
            match (&approved.redirect_host, approved.redirect_port) {
                (Some(host), Some(port)) => loopback_redirect(
                    host,
                    port,
                    &[("code", &code), ("state", &approved.client_state)],
                ),
                _ => html::code_page(&code, ttl),
            }
        }
        Some("deny") => match db_cli::deny_request(&state.db, &decision).await {
            Ok(Some(denied)) => {
                tracing::info!(
                    request_id = %denied.id,
                    user_id = %user.user_id,
                    decision = "denied",
                    "cli login decided"
                );
                match (&denied.redirect_host, denied.redirect_port) {
                    (Some(host), Some(port)) => loopback_redirect(
                        host,
                        port,
                        &[("error", "access_denied"), ("state", &denied.client_state)],
                    ),
                    _ => html::denied_page(),
                }
            }
            Ok(None) => html::invalid_page(),
            Err(e) => db_error(e),
        },
        _ => html::invalid_page(),
    }
}

/// `GET /cli/code`: the remote `redirect_uri`. The code itself is only ever
/// rendered in the consent POST response, never in a URL.
pub async fn code_info() -> Response {
    html::code_info_page()
}

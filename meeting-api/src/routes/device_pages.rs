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

//! The `/devices` page (list and revoke), the stylesheet, and the helpers
//! every server-rendered device page shares: cookie-only session
//! resolution, the post-login redirect, the form-POST CSRF check and the
//! requester IP.
//!
//! These pages are served from the API origin next to cookie-authenticated
//! JSON. They are mounted outside the CORS layer, carry the strict
//! security headers, and only accept `application/x-www-form-urlencoded`
//! POSTs whose `Origin` is exactly the configured API origin.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts, Path, State};
use axum::http::{header, request::Parts, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use url::form_urlencoded;
use videocall_meeting_types::device::is_valid_device_id;

use crate::auth::extract_cookie_token;
use crate::db::devices as db_devices;
use crate::html;
use crate::state::AppState;
use crate::token;

/// The signed-in browser user of a page request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageUser {
    pub user_id: String,
    pub name: String,
}

/// Resolve the browser session from the session cookie only. A bearer
/// header (session JWT or device JWT) never authenticates a page.
pub fn page_user(state: &AppState, parts_headers: &HeaderMap) -> Option<PageUser> {
    let token = extract_cookie_token(parts_headers, &state.cookie_name)?;
    let claims = token::decode_session_token(&state.jwt_secret, &token).ok()?;
    Some(PageUser {
        user_id: claims.sub,
        name: claims.name,
    })
}

/// The API origin, or `None` when device auth is not configured.
pub fn api_origin(state: &AppState) -> Option<&str> {
    state.device.audience.as_deref()
}

/// `302` to `/login` on this host, returning to `path` on the API origin.
/// `path` must be one of the closed return targets (see
/// [`crate::device_auth::device_return_target`]) and is always rebuilt by
/// the caller, never echoed from the request.
pub fn login_redirect(api_origin: &str, path: &str) -> Response {
    let return_to = format!("{api_origin}{path}");
    let query: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("returnTo", &return_to)
        .finish();
    (
        StatusCode::FOUND,
        [(header::LOCATION, format!("/login?{query}"))],
    )
        .into_response()
}

/// CSRF checks for every page POST: `Origin` exactly the API origin,
/// `Sec-Fetch-Site: same-origin` when sent, and a form content type (so no
/// JSON body is ever accepted).
pub fn check_form_post(api_origin: &str, headers: &HeaderMap) -> Result<(), &'static str> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .ok_or("missing Origin")?;
    if origin != api_origin {
        return Err("wrong Origin");
    }
    if let Some(site) = headers.get("sec-fetch-site") {
        if site.to_str().ok() != Some("same-origin") {
            return Err("Sec-Fetch-Site is not same-origin");
        }
    }
    let is_form = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| {
            v.trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        });
    if !is_form {
        return Err("not a form post");
    }
    Ok(())
}

/// The TCP peer address, when the server runs with connect info. Never
/// fails: in-process tests simply have none.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(PeerAddr(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| *addr),
        ))
    }
}

/// The client address: the right-most `X-Forwarded-For` entry when
/// `TRUST_X_FORWARDED_FOR=true` (the hop our ingress appended), else the TCP
/// peer. Informational only: display and the per-IP request cap.
pub fn client_ip(
    headers: &HeaderMap,
    peer: PeerAddr,
    trust_x_forwarded_for: bool,
) -> Option<IpAddr> {
    if trust_x_forwarded_for {
        let forwarded = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .next_back()
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse::<IpAddr>().ok());
        if forwarded.is_some() {
            return forwarded;
        }
    }
    peer.0.map(|addr| addr.ip())
}

/// `GET /static/auth.css`
pub async fn stylesheet() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        html::AUTH_CSS,
    )
        .into_response()
}

/// `GET /devices`
pub async fn devices_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(origin) = api_origin(&state) else {
        return html::not_configured_page();
    };
    let Some(user) = page_user(&state, &headers) else {
        return login_redirect(origin, "/devices");
    };
    match db_devices::list_live_devices(&state.db, &user.user_id, Utc::now()).await {
        Ok(devices) => html::devices_page(&user.name, &devices),
        Err(e) => {
            tracing::error!("listing devices failed: {e}");
            html::error_page(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `POST /devices/{device_id}/revoke`
pub async fn revoke(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(origin) = api_origin(&state) else {
        return html::not_configured_page();
    };
    if let Err(reason) = check_form_post(origin, &headers) {
        tracing::info!("revoke POST rejected: {reason}");
        return html::rejected_page();
    }
    let Some(user) = page_user(&state, &headers) else {
        return html::signed_out_page();
    };
    if !is_valid_device_id(&device_id) {
        return html::not_found_page();
    }
    match db_devices::revoke_by_user(&state.db, &device_id, &user.user_id, Utc::now()).await {
        Ok(true) => {
            tracing::info!(device_id = %device_id, user_id = %user.user_id, "device revoked by user");
            (StatusCode::SEE_OTHER, [(header::LOCATION, "/devices")]).into_response()
        }
        Ok(false) => html::not_found_page(),
        Err(e) => {
            tracing::error!("revoke failed: {e}");
            html::error_page(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn x_forwarded_for_only_when_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.2.3.4, 198.51.100.9"),
        );
        let peer = PeerAddr(Some("10.0.0.1:5000".parse().unwrap()));
        assert_eq!(
            client_ip(&headers, peer, true),
            Some("198.51.100.9".parse().unwrap())
        );
        assert_eq!(
            client_ip(&headers, peer, false),
            Some("10.0.0.1".parse().unwrap())
        );
        headers.insert("x-forwarded-for", HeaderValue::from_static("garbage"));
        assert_eq!(
            client_ip(&headers, peer, true),
            Some("10.0.0.1".parse().unwrap())
        );
    }

    #[test]
    fn login_redirect_encodes_return_to() {
        let resp = login_redirect("https://api.videocall.rs", "/devices");
        assert_eq!(
            resp.headers().get("location").unwrap(),
            "/login?returnTo=https%3A%2F%2Fapi.videocall.rs%2Fdevices"
        );
    }
}

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

use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequestParts, Path, State};
use axum::http::{header, request::Parts, HeaderMap, HeaderValue, StatusCode};
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
/// the caller, never echoed from the request. Without OAuth configured
/// `/login` can only fail, so this renders a 503 page instead.
pub fn login_redirect(state: &AppState, api_origin: &str, path: &str) -> Response {
    if state.oauth.is_none() {
        return html::sign_in_unavailable_page();
    }
    (
        StatusCode::FOUND,
        [(header::LOCATION, login_location(api_origin, path))],
    )
        .into_response()
}

/// `/login?returnTo={api_origin}{path}`, encoded.
pub fn login_location(api_origin: &str, path: &str) -> String {
    let return_to = format!("{api_origin}{path}");
    let query: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("returnTo", &return_to)
        .finish();
    format!("/login?{query}")
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

/// The key the per-client cap on open sign-in requests counts by: an IPv4
/// address, or the /64 of an IPv6 address (one subscriber's usual
/// allocation, so rotating privacy addresses do not each get a fresh
/// allowance). `None` for addresses that belong to infrastructure rather
/// than a client (loopback, RFC 1918, CGNAT 100.64/10, link-local, IPv6
/// ULA, unspecified) and for a missing address: counting those would let one
/// client exhaust the cap for everyone behind the same hop.
pub fn cap_key(ip: Option<IpAddr>) -> Option<String> {
    match ip? {
        IpAddr::V4(v4) => v4_cap_key(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_cap_key(v4);
            }
            let segments = v6.segments();
            let infrastructure = v6.is_loopback()
                || v6.is_unspecified()
                || (segments[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
                || (segments[0] & 0xffc0) == 0xfe80; // link-local fe80::/10
            if infrastructure {
                return None;
            }
            let prefix = std::net::Ipv6Addr::new(
                segments[0],
                segments[1],
                segments[2],
                segments[3],
                0,
                0,
                0,
                0,
            );
            Some(format!("{prefix}/64"))
        }
    }
}

fn v4_cap_key(v4: std::net::Ipv4Addr) -> Option<String> {
    let [a, b, ..] = v4.octets();
    let cgnat = a == 100 && (64..128).contains(&b);
    let infrastructure =
        v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified() || cgnat;
    (!infrastructure).then(|| v4.to_string())
}

/// `POST /cli/sign-out` (form): clear the session cookie and render a
/// "Signed out" page whose sign-in link returns to the consent request named
/// in the form, or to /devices. Same CSRF checks as every page POST. The
/// web app's `GET /logout` is unchanged.
pub async fn sign_out(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(origin) = api_origin(&state) else {
        return html::not_configured_page();
    };
    if let Err(reason) = check_form_post(origin, &headers) {
        tracing::info!("sign-out POST rejected: {reason}");
        return html::rejected_page();
    }
    let request_id = form_urlencoded::parse(&body)
        .find(|(key, _)| key == "request")
        .map(|(_, value)| value.into_owned())
        .filter(|id| is_valid_device_id(id));
    // Rebuilt from the validated id: always one of the closed return targets.
    let path = match request_id {
        Some(id) => crate::device_auth::consent_path(&id),
        None => "/devices".to_string(),
    };
    let clear = crate::routes::oauth::build_clear_session_cookie(
        &state.cookie_name,
        state.cookie_domain.as_deref(),
        state.cookie_secure,
    );
    let mut response = html::signed_out_by_user_page(&login_location(origin, &path));
    match HeaderValue::from_str(&clear) {
        Ok(value) => {
            response.headers_mut().append(header::SET_COOKIE, value);
            response
        }
        Err(_) => html::error_page(StatusCode::INTERNAL_SERVER_ERROR),
    }
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
        return login_redirect(&state, origin, "/devices");
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
    fn cap_keys_skip_infrastructure_and_group_ipv6_by_64() {
        let key = |s: &str| cap_key(Some(s.parse().unwrap()));
        assert_eq!(key("203.0.113.7").as_deref(), Some("203.0.113.7"));
        for infra in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "169.254.1.1",
            "0.0.0.0",
            "::1",
            "::",
            "fd00::1",
            "fc12::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert_eq!(key(infra), None, "{infra}");
        }
        assert_eq!(key("100.128.0.1").as_deref(), Some("100.128.0.1"));
        assert_eq!(key("172.32.0.1").as_deref(), Some("172.32.0.1"));
        assert_eq!(key("::ffff:203.0.113.7").as_deref(), Some("203.0.113.7"));
        assert_eq!(
            key("2001:db8:1:2:aaaa::1"),
            key("2001:db8:1:2:bbbb:cccc:dddd:eeee")
        );
        assert_eq!(key("2001:db8:1:2::9").as_deref(), Some("2001:db8:1:2::/64"));
        assert_ne!(key("2001:db8:1:2::1"), key("2001:db8:1:3::1"));
        assert_eq!(cap_key(None), None);
    }

    #[test]
    fn login_redirect_encodes_return_to() {
        assert_eq!(
            login_location("https://api.videocall.rs", "/devices"),
            "/login?returnTo=https%3A%2F%2Fapi.videocall.rs%2Fdevices"
        );
    }
}

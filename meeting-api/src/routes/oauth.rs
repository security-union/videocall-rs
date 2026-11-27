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

//! OAuth route handlers: login, callback, session, profile, logout.
//!
//! After a successful OAuth login the callback issues a **signed session JWT**
//! inside an `HttpOnly; Secure; SameSite=Lax` cookie named `session`.
//! JavaScript cannot read the cookie; the browser sends it automatically.

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use oauth2::{CsrfToken, PkceCodeChallenge};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;

use videocall_meeting_types::responses::{APIResponse, ProfileResponse};

use crate::auth::{extract_cookie_token, AuthUser};
use crate::db::oauth as db_oauth;
use crate::error::AppError;
use crate::oauth;
use crate::state::AppState;
use crate::token;

// ---------------------------------------------------------------------------
// Cookie helpers
// ---------------------------------------------------------------------------

/// Build a `Set-Cookie` header value for the session JWT.
fn build_session_cookie(
    name: &str,
    jwt: &str,
    ttl_secs: i64,
    domain: Option<&str>,
    secure: bool,
) -> String {
    let mut cookie = format!("{name}={jwt}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl_secs}");
    if secure {
        cookie.push_str("; Secure");
    }
    if let Some(d) = domain {
        cookie.push_str(&format!("; Domain={d}"));
    }
    cookie
}

/// Name of the pre-login cookie that binds an OAuth login to the browser
/// that started it (login CSRF). Derived from the session cookie name so
/// environments sharing a parent domain never collide. With `Secure` it
/// carries the `__Host-` prefix, so the browser only accepts it from this
/// exact host: a sibling subdomain cannot plant a value of its choosing.
fn login_cookie_name(state: &AppState) -> String {
    if state.cookie_secure {
        format!("__Host-{}-login", state.cookie_name)
    } else {
        format!("{}-login", state.cookie_name)
    }
}

/// `Set-Cookie` for the pre-login cookie: host-only (no `Domain`),
/// `SameSite=Lax` so it rides the top-level redirect back from the IdP, and
/// as short-lived as the stored OAuth request. `max_age = 0` clears it.
fn build_login_cookie(name: &str, value: &str, max_age: i64, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}")
}

/// The pre-login value is 32 random bytes, base64url: 43 characters.
fn is_valid_binding(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Lowercase hex SHA-256: only the hash of the binding is stored.
fn sha256_hex(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Build a `Set-Cookie` header that clears the session cookie.
fn build_clear_session_cookie(name: &str, domain: Option<&str>, secure: bool) -> String {
    let mut cookie = format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    if let Some(d) = domain {
        cookie.push_str(&format!("; Domain={d}"));
    }
    cookie
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    #[serde(rename = "returnTo")]
    pub return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub state: String,
    pub code: String,
}

/// GET /login?returnTo=<url>
///
/// Initiates the OAuth flow: generates PKCE + CSRF + nonce, stores in DB,
/// redirects to the identity provider.
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Result<Response, AppError> {
    let oauth_cfg = state
        .oauth
        .as_ref()
        .ok_or_else(|| AppError::internal("OAuth not configured"))?;

    let csrf_token = CsrfToken::new_random();
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    // Generate a nonce for OIDC ID token binding (reuse oauth2's crypto RNG).
    let nonce = CsrfToken::new_random();

    // Sanitize return_to: allow relative paths and absolute URLs whose origin
    // is in the allowlist (after_login_url origin + ALLOWED_REDIRECT_URLS).
    let return_to = query.return_to.as_deref().and_then(|u| {
        validate_return_to(
            u,
            &oauth_cfg.after_login_url,
            &oauth_cfg.allowed_redirect_urls,
        )
    });
    let return_to = return_to.as_deref();

    // Bind this login to the browser that started it: the callback only
    // completes where this cookie is present. A browser that already holds a
    // valid one keeps it, so a login started in a second tab does not break
    // one still running in the first.
    let login_cookie = login_cookie_name(&state);
    let binding = extract_cookie_token(&headers, &login_cookie)
        .filter(|value| is_valid_binding(value))
        .unwrap_or_else(|| CsrfToken::new_random_len(32).secret().clone());

    db_oauth::store_oauth_request(
        &state.db,
        pkce_challenge.as_str(),
        pkce_verifier.secret(),
        csrf_token.secret(),
        return_to,
        Some(nonce.secret()),
        &sha256_hex(&binding),
    )
    .await?;

    let auth_url = oauth::build_auth_url(
        &oauth_cfg.auth_url,
        &oauth_cfg.client_id,
        &oauth_cfg.redirect_url,
        &oauth_cfg.scopes,
        pkce_challenge.as_str(),
        csrf_token.secret(),
        Some(nonce.secret()),
    );

    let mut response = Redirect::to(&auth_url).into_response();
    let cookie = build_login_cookie(
        &login_cookie,
        &binding,
        db_oauth::OAUTH_REQUEST_TTL_SECS,
        state.cookie_secure,
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie)
            .map_err(|_| AppError::internal("failed to build login cookie header"))?,
    );
    Ok(response)
}

/// GET /login/callback?state=...&code=...
///
/// Handles the OAuth callback: exchanges the authorization code for tokens,
/// verifies the ID token (signature, nonce, audience, issuer when configured),
/// creates a signed session JWT, and sets it as an `HttpOnly` cookie.
pub async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, AppError> {
    let oauth_cfg = state
        .oauth
        .as_ref()
        .ok_or_else(|| AppError::internal("OAuth not configured"))?;

    let oauth_req = db_oauth::fetch_oauth_request(&state.db, &query.state)
        .await?
        .ok_or_else(|| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                videocall_meeting_types::APIError::internal_error("invalid OAuth state"),
            )
        })?;

    // The row is consumed above, so a failed binding check also burns the
    // state. Rows stored before the binding existed carry no hash and are
    // accepted for their remaining TTL only.
    if let Some(expected) = oauth_req.browser_binding_hash.as_deref() {
        let presented = extract_cookie_token(&headers, &login_cookie_name(&state))
            .map(|value| sha256_hex(&value))
            .unwrap_or_default();
        if !bool::from(presented.as_bytes().ct_eq(expected.as_bytes())) {
            tracing::info!("OAuth callback rejected: sign-in was started in another browser");
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                videocall_meeting_types::APIError::internal_error(
                    "sign-in was started in a different browser; start it again here",
                ),
            ));
        }
    }

    let pkce_verifier = oauth_req
        .pkce_verifier
        .ok_or_else(|| AppError::internal("missing PKCE verifier"))?;

    let (token_response, mut claims) = oauth::exchange_code_for_claims(
        &oauth_cfg.redirect_url,
        &oauth_cfg.client_id,
        oauth_cfg.client_secret.as_deref(),
        &pkce_verifier,
        &oauth_cfg.token_url,
        &query.code,
        state.jwks_cache.as_deref(),
        oauth_cfg.issuer.as_deref(),
        oauth_req.nonce.as_deref(),
    )
    .await?;

    // If the ID token lacks an email claim, fall back to the UserInfo endpoint.
    if claims.email.as_ref().is_none_or(|e| e.is_empty()) {
        if let Some(userinfo_url) = &oauth_cfg.userinfo_url {
            let user_info =
                oauth::fetch_userinfo(userinfo_url, &token_response.access_token).await?;
            if claims.email.as_ref().is_none_or(|e| e.is_empty()) {
                claims.email = user_info.email;
            }
            if claims.name.is_empty() {
                if let Some(name) = user_info.name {
                    claims.name = name;
                }
            }
            if claims.given_name.is_none() {
                claims.given_name = user_info.given_name;
            }
            if claims.family_name.is_none() {
                claims.family_name = user_info.family_name;
            }
        }
    }

    let email = claims
        .email
        .as_ref()
        .filter(|e| !e.is_empty())
        .ok_or_else(|| AppError::internal("Email not available from ID token or UserInfo"))?
        .clone();

    let display_name = claims.display_name();

    db_oauth::upsert_user(
        &state.db,
        &email,
        &display_name,
        &token_response.access_token,
        token_response.refresh_token.as_deref(),
    )
    .await?;

    // --- Issue signed session JWT inside an HttpOnly cookie ---
    let session_jwt = token::generate_session_token(
        &state.jwt_secret,
        &email,
        &display_name,
        state.session_ttl_secs,
    )?;

    let redirect_url = match &oauth_req.return_to {
        Some(value) if value.starts_with("http://") || value.starts_with("https://") => {
            // Absolute URL — re-validate as defense-in-depth.
            validate_return_to(
                value,
                &oauth_cfg.after_login_url,
                &oauth_cfg.allowed_redirect_urls,
            )
            .unwrap_or_else(|| oauth_cfg.after_login_url.clone())
        }
        Some(path) => {
            // Relative path (e.g. "/meeting/1") — prepend the frontend base URL.
            format!(
                "{}{}",
                oauth_cfg.after_login_url.trim_end_matches('/'),
                path
            )
        }
        None => oauth_cfg.after_login_url.clone(),
    };

    let session_cookie = build_session_cookie(
        &state.cookie_name,
        &session_jwt,
        state.session_ttl_secs,
        state.cookie_domain.as_deref(),
        state.cookie_secure,
    );

    tracing::info!(
        "OAuth login successful for {} ({}), redirecting to {}",
        display_name,
        email,
        redirect_url
    );

    let mut response = Redirect::to(&redirect_url).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie)
            .map_err(|_| AppError::internal("failed to build session cookie header"))?,
    );
    // The binding has done its job. A login still running in another tab of
    // this browser now fails at its callback; that browser is signed in.
    let clear_login = build_login_cookie(&login_cookie_name(&state), "", 0, state.cookie_secure);
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear_login)
            .map_err(|_| AppError::internal("failed to build login cookie header"))?,
    );
    Ok(response)
}

/// GET /session -- returns 200 if the session JWT is valid, 401 otherwise.
///
/// The `AuthUser` extractor validates the session JWT from the `session`
/// cookie (or `Authorization: Bearer` header).
pub async fn check_session(AuthUser { .. }: AuthUser) -> StatusCode {
    StatusCode::OK
}

/// GET /profile -- returns the authenticated user's profile from the session
/// JWT claims.
///
/// Because the session JWT embeds both user ID and display name, this endpoint
/// does not need a database query.
pub async fn get_profile(
    AuthUser { user_id, name }: AuthUser,
) -> Json<APIResponse<ProfileResponse>> {
    Json(APIResponse::ok(ProfileResponse { user_id, name }))
}

/// GET /logout -- clears the session cookie.
pub async fn logout(State(state): State<AppState>) -> Result<Response, AppError> {
    let clear = build_clear_session_cookie(
        &state.cookie_name,
        state.cookie_domain.as_deref(),
        state.cookie_secure,
    );
    let mut response = StatusCode::OK.into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear)
            .map_err(|_| AppError::internal("failed to build clear cookie header"))?,
    );
    Ok(response)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validate and sanitize a `returnTo` value.
///
/// Accepts:
/// - Relative paths starting with `/` (but not `//`). Note: path-traversal
///   sequences like `/../` are not stripped here because the browser resolves
///   them before navigation and the redirect target is always an allowed origin.
/// - Absolute `http(s)://` URLs whose origin matches `after_login_url` or
///   any entry in `allowed_redirect_urls`.
///
/// Returns `Some(sanitized_value)` on success, `None` on rejection.
fn validate_return_to(
    raw: &str,
    after_login_url: &str,
    allowed_redirect_urls: &[String],
) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Relative path: must start with "/" but not "//" (protocol-relative).
    if trimmed.starts_with('/') {
        if trimmed.starts_with("//") {
            tracing::warn!(return_to = trimmed, "rejected protocol-relative returnTo");
            return None;
        }
        return Some(trimmed.to_string());
    }

    // Only allow http/https absolute URLs.
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        tracing::warn!(
            return_to = trimmed,
            "rejected returnTo with disallowed scheme"
        );
        return None;
    }

    let parsed = match Url::parse(trimmed) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(return_to = trimmed, error = %e, "rejected unparseable returnTo URL");
            return None;
        }
    };
    let candidate_origin = parsed.origin().unicode_serialization();

    // Check against after_login_url origin.
    if let Ok(base) = Url::parse(after_login_url) {
        if base.origin().unicode_serialization() == candidate_origin {
            return Some(trimmed.to_string());
        }
    }

    // Check against the explicit allowlist.
    for allowed in allowed_redirect_urls {
        if let Ok(allowed_url) = Url::parse(allowed) {
            if allowed_url.origin().unicode_serialization() == candidate_origin {
                return Some(trimmed.to_string());
            }
        }
    }

    tracing::warn!(
        return_to = trimmed,
        origin = candidate_origin,
        "rejected returnTo: origin not in allowlist"
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const AFTER_LOGIN: &str = "http://localhost:80";
    const ALLOWED: &[&str] = &["http://localhost:3001", "https://app.videocall.rs"];

    fn allowed() -> Vec<String> {
        ALLOWED.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn relative_path_accepted() {
        assert_eq!(
            validate_return_to("/meeting/123", AFTER_LOGIN, &allowed()),
            Some("/meeting/123".to_string())
        );
    }

    #[test]
    fn relative_root_accepted() {
        assert_eq!(
            validate_return_to("/", AFTER_LOGIN, &allowed()),
            Some("/".to_string())
        );
    }

    #[test]
    fn protocol_relative_rejected() {
        assert_eq!(
            validate_return_to("//evil.com/foo", AFTER_LOGIN, &allowed()),
            None
        );
    }

    #[test]
    fn absolute_url_matching_after_login_origin() {
        assert_eq!(
            validate_return_to("http://localhost:80/meeting/1", AFTER_LOGIN, &allowed()),
            Some("http://localhost:80/meeting/1".to_string())
        );
    }

    #[test]
    fn absolute_url_matching_allowed_list() {
        assert_eq!(
            validate_return_to("http://localhost:3001/meeting/1", AFTER_LOGIN, &allowed()),
            Some("http://localhost:3001/meeting/1".to_string())
        );
    }

    #[test]
    fn absolute_url_https_allowed() {
        assert_eq!(
            validate_return_to(
                "https://app.videocall.rs/meeting/1",
                AFTER_LOGIN,
                &allowed()
            ),
            Some("https://app.videocall.rs/meeting/1".to_string())
        );
    }

    #[test]
    fn absolute_url_disallowed_origin() {
        assert_eq!(
            validate_return_to("http://evil.com/steal", AFTER_LOGIN, &allowed()),
            None
        );
    }

    #[test]
    fn javascript_scheme_rejected() {
        assert_eq!(
            validate_return_to("javascript:alert(1)", AFTER_LOGIN, &allowed()),
            None
        );
    }

    #[test]
    fn empty_string_rejected() {
        assert_eq!(validate_return_to("", AFTER_LOGIN, &allowed()), None);
    }

    #[test]
    fn port_mismatch_rejected() {
        assert_eq!(
            validate_return_to("http://localhost:9999/foo", AFTER_LOGIN, &allowed()),
            None
        );
    }

    #[test]
    fn scheme_mismatch_rejected() {
        // after_login_url is http, candidate is https on the same host.
        assert_eq!(
            validate_return_to("https://localhost:80/foo", AFTER_LOGIN, &allowed()),
            None
        );
    }

    #[test]
    fn data_scheme_rejected() {
        assert_eq!(
            validate_return_to("data:text/html,<h1>hi</h1>", AFTER_LOGIN, &allowed()),
            None
        );
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(
            validate_return_to("  /meeting/1  ", AFTER_LOGIN, &allowed()),
            Some("/meeting/1".to_string())
        );
    }

    #[test]
    fn absolute_url_decoded_from_meeting_page() {
        // Meeting pages send URL-encoded returnTo values. Axum's Query
        // extractor decodes them before they reach validate_return_to,
        // so the function sees the decoded form.
        let decoded = "http://localhost:3001/meeting/my-room";
        assert_eq!(
            validate_return_to(decoded, AFTER_LOGIN, &allowed()),
            Some(decoded.to_string())
        );
    }

    #[test]
    fn empty_allowed_list_still_checks_after_login() {
        assert_eq!(
            validate_return_to("http://localhost:80/meeting/1", AFTER_LOGIN, &[]),
            Some("http://localhost:80/meeting/1".to_string())
        );
        // But a different origin is rejected.
        assert_eq!(
            validate_return_to("http://localhost:3001/meeting/1", AFTER_LOGIN, &[]),
            None
        );
    }

    // --- build_session_cookie ---

    #[test]
    fn session_cookie_contains_name_and_jwt() {
        let cookie = build_session_cookie("session", "my.jwt.token", 3600, None, false);
        assert!(cookie.starts_with("session=my.jwt.token;"));
    }

    #[test]
    fn session_cookie_custom_name() {
        let cookie = build_session_cookie("pr1-session", "my.jwt.token", 3600, None, false);
        assert!(cookie.starts_with("pr1-session=my.jwt.token;"));
        // Must not be mistakable for a plain "session=" cookie.
        assert!(!cookie.starts_with("session="));
    }

    #[test]
    fn session_cookie_includes_required_attributes() {
        let cookie = build_session_cookie("session", "tok", 3600, None, false);
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Max-Age=3600"));
    }

    #[test]
    fn session_cookie_secure_flag_added_when_true() {
        let cookie = build_session_cookie("session", "tok", 3600, None, true);
        assert!(cookie.contains("; Secure"));
    }

    #[test]
    fn session_cookie_no_secure_flag_when_false() {
        let cookie = build_session_cookie("session", "tok", 3600, None, false);
        assert!(!cookie.contains("Secure"));
    }

    #[test]
    fn session_cookie_domain_appended() {
        let cookie =
            build_session_cookie("session", "tok", 3600, Some(".sandbox.videocall.rs"), false);
        assert!(cookie.contains("Domain=.sandbox.videocall.rs"));
    }

    #[test]
    fn session_cookie_no_domain_when_none() {
        let cookie = build_session_cookie("session", "tok", 3600, None, false);
        assert!(!cookie.contains("Domain="));
    }

    // --- build_clear_session_cookie ---

    #[test]
    fn clear_cookie_uses_name() {
        let cookie = build_clear_session_cookie("session", None, false);
        assert!(cookie.starts_with("session=;"));
    }

    #[test]
    fn clear_cookie_custom_name() {
        let cookie = build_clear_session_cookie("pr1-session", None, false);
        assert!(cookie.starts_with("pr1-session=;"));
    }

    #[test]
    fn clear_cookie_sets_max_age_zero() {
        let cookie = build_clear_session_cookie("session", None, false);
        assert!(cookie.contains("Max-Age=0"));
    }

    #[test]
    fn clear_cookie_domain_appended() {
        let cookie = build_clear_session_cookie("session", Some(".videocall.rs"), false);
        assert!(cookie.contains("Domain=.videocall.rs"));
    }
}

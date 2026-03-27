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
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use oauth2::{CsrfToken, PkceCodeChallenge};
use serde::Deserialize;
use url::Url;

use videocall_meeting_types::responses::{APIResponse, ProfileResponse};

use crate::auth::AuthUser;
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

    db_oauth::store_oauth_request(
        &state.db,
        pkce_challenge.as_str(),
        pkce_verifier.secret(),
        csrf_token.secret(),
        return_to,
        Some(nonce.secret()),
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

    Ok(Redirect::to(&auth_url).into_response())
}

/// GET /login/callback?state=...&code=...
///
/// Handles the OAuth callback: exchanges the authorization code for tokens,
/// verifies the ID token (signature, nonce, audience, issuer when configured),
/// creates a signed session JWT, and sets it as an `HttpOnly` cookie.
pub async fn callback(
    State(state): State<AppState>,
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

/// GET /logout -- clears the session cookie, then redirects the browser to the
/// provider's `end_session_endpoint` (RP-initiated logout per OIDC RP-Initiated
/// Logout 1.0) when one is configured.
///
/// Redirect parameters sent to the provider:
/// - `client_id` — always included.
/// - `post_logout_redirect_uri` — included when `AFTER_LOGOUT_URL` is set.
///
/// Note: `id_token_hint` is not sent because session JWTs are short-lived
/// admission tickets; the original ID token is not persisted.
///
/// When no `end_session_endpoint` is configured the handler returns `200 OK`
/// (backward-compatible behaviour for non-browser API clients).
pub async fn logout(State(state): State<AppState>) -> Result<Response, AppError> {
    let clear = build_clear_session_cookie(
        &state.cookie_name,
        state.cookie_domain.as_deref(),
        state.cookie_secure,
    );

    // Redirect to the provider's end-session endpoint when configured so that
    // the provider also terminates its session (RP-initiated logout).
    let mut response = if let Some(end_session_url) = state
        .oauth
        .as_ref()
        .and_then(|o| o.end_session_endpoint.as_deref())
    {
        let oauth_cfg = state.oauth.as_ref().expect("oauth is Some");
        let redirect_url = build_end_session_url(end_session_url, oauth_cfg)?;
        tracing::info!(
            end_session_url = %redirect_url,
            "Initiating RP-initiated logout via provider end-session endpoint",
        );
        Redirect::to(&redirect_url).into_response()
    } else {
        StatusCode::OK.into_response()
    };

    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear)
            .map_err(|_| AppError::internal("failed to build clear cookie header"))?,
    );
    Ok(response)
}

/// Build the provider's end-session URL for RP-initiated logout.
///
/// Appends `client_id` and, when configured, `post_logout_redirect_uri` to
/// `end_session_url` using proper percent-encoding via the `url` crate.
fn build_end_session_url(
    end_session_url: &str,
    oauth_cfg: &crate::config::OAuthConfig,
) -> Result<String, AppError> {
    let mut url = Url::parse(end_session_url)
        .map_err(|e| AppError::internal(&format!("Invalid end_session_endpoint URL: {e}")))?;
    {
        let mut params = url.query_pairs_mut();
        params.append_pair("client_id", &oauth_cfg.client_id);
        if let Some(ref after_logout_url) = oauth_cfg.after_logout_url {
            params.append_pair("post_logout_redirect_uri", after_logout_url);
        }
    }
    Ok(url.to_string())
}

// ---------------------------------------------------------------------------
// Front-channel logout
// ---------------------------------------------------------------------------

/// Query parameters sent by the OIDC provider to the front-channel logout URI.
/// Defined in OpenID Connect Front-Channel Logout 1.0.
#[derive(Debug, Deserialize)]
pub struct FrontChannelLogoutQuery {
    /// Issuer identifier of the provider initiating logout. Optional per spec;
    /// validated against `OAUTH_ISSUER` when present.
    pub iss: Option<String>,
    /// Provider browser-session identifier. Optional per spec; logged for
    /// observability. Cannot be used to revoke stateless JWTs server-side.
    pub sid: Option<String>,
}

/// GET /logout/frontchannel
///
/// OIDC front-channel logout endpoint (OpenID Connect Front-Channel Logout 1.0).
///
/// The identity provider loads this URL in a **hidden iframe** when the
/// End-User logs out of the provider directly or via another relying party.
/// The request therefore comes from the user's browser, which means that
/// returning a `Set-Cookie` in the response will clear the local session even
/// though `SameSite=Lax` prevents the cookie from being *sent* in the
/// cross-site iframe request.
///
/// Spec requirements implemented here:
/// - Validates the `iss` parameter against the configured issuer (when both
///   are present) to reject spurious logout triggers from unknown providers.
/// - Clears the local session cookie via `Set-Cookie: Max-Age=0`.
/// - Returns `200 OK` with an empty body — **must not redirect** because the
///   response is consumed by the provider's iframe, not the browser's top-level
///   navigation.
///
/// # Provider registration
/// Register the URL `{base_url}/logout/frontchannel` as the
/// `frontchannel_logout_uri` in your OIDC client configuration at the
/// provider.
pub async fn frontchannel_logout(
    State(state): State<AppState>,
    Query(query): Query<FrontChannelLogoutQuery>,
) -> Result<Response, AppError> {
    // Validate the `iss` parameter against our configured issuer to prevent
    // unauthenticated third parties from triggering spurious logouts.
    if let Some(ref iss_param) = query.iss {
        if let Some(ref oauth_cfg) = state.oauth {
            if let Some(ref configured_issuer) = oauth_cfg.issuer {
                if iss_param != configured_issuer {
                    tracing::warn!(
                        iss_received = %iss_param,
                        iss_expected = %configured_issuer,
                        "Front-channel logout rejected: issuer mismatch",
                    );
                    return Err(AppError::new(
                        StatusCode::BAD_REQUEST,
                        videocall_meeting_types::APIError::internal_error(
                            "iss parameter does not match configured issuer",
                        ),
                    ));
                }
            }
        }
    }

    tracing::info!(
        sid = query.sid.as_deref().unwrap_or("<none>"),
        iss = query.iss.as_deref().unwrap_or("<none>"),
        "Processing OIDC front-channel logout",
    );

    // Clear the session cookie. Browsers honour Set-Cookie in iframe responses
    // even when SameSite=Lax prevents the cookie from being included in the
    // cross-origin request, so this effectively terminates the browser session.
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

    // ---------------------------------------------------------------------------
    // build_end_session_url
    // ---------------------------------------------------------------------------

    /// Minimal `OAuthConfig` used by unit tests that exercise logout URL building.
    fn minimal_oauth_config(
        end_session_endpoint: Option<String>,
        after_logout_url: Option<String>,
    ) -> crate::config::OAuthConfig {
        crate::config::OAuthConfig {
            client_id: "test-client".to_string(),
            client_secret: None,
            redirect_url: "https://app.example.com/callback".to_string(),
            issuer: Some("https://provider.example.com".to_string()),
            auth_url: "https://provider.example.com/auth".to_string(),
            token_url: "https://provider.example.com/token".to_string(),
            jwks_url: None,
            userinfo_url: None,
            scopes: "openid email profile".to_string(),
            after_login_url: "https://app.example.com/".to_string(),
            allowed_redirect_urls: vec![],
            end_session_endpoint,
            after_logout_url,
        }
    }

    #[test]
    fn build_end_session_url_includes_client_id() {
        let cfg = minimal_oauth_config(None, None);
        let url = build_end_session_url("https://provider.example.com/logout", &cfg).unwrap();
        assert!(
            url.contains("client_id=test-client"),
            "expected client_id in URL, got: {url}"
        );
    }

    #[test]
    fn build_end_session_url_includes_post_logout_redirect_uri_when_set() {
        let cfg = minimal_oauth_config(
            Some("https://provider.example.com/logout".to_string()),
            Some("https://app.example.com/after-logout".to_string()),
        );
        let url = build_end_session_url("https://provider.example.com/logout", &cfg).unwrap();
        assert!(
            url.contains("post_logout_redirect_uri="),
            "expected post_logout_redirect_uri in URL, got: {url}"
        );
        assert!(
            url.contains("app.example.com"),
            "expected redirect URI host in URL, got: {url}"
        );
    }

    #[test]
    fn build_end_session_url_omits_post_logout_redirect_uri_when_unset() {
        let cfg = minimal_oauth_config(None, None);
        let url = build_end_session_url("https://provider.example.com/end_session", &cfg).unwrap();
        assert!(
            !url.contains("post_logout_redirect_uri"),
            "should not contain post_logout_redirect_uri, got: {url}"
        );
    }

    #[test]
    fn build_end_session_url_preserves_existing_query_params() {
        let cfg = minimal_oauth_config(None, None);
        let url = build_end_session_url("https://provider.example.com/logout?realm=master", &cfg)
            .unwrap();
        assert!(
            url.contains("realm=master"),
            "existing query param should be preserved, got: {url}"
        );
        assert!(
            url.contains("client_id="),
            "client_id should be appended, got: {url}"
        );
    }

    #[test]
    fn build_end_session_url_encodes_special_characters_in_redirect_uri() {
        let cfg = minimal_oauth_config(
            None,
            Some("https://app.example.com/after logout?ref=1&foo=bar".to_string()),
        );
        let url = build_end_session_url("https://provider.example.com/logout", &cfg).unwrap();
        // Spaces and ampersands inside the redirect URI value must be percent-encoded
        // so they do not break the outer query string.
        assert!(
            !url.ends_with(' '),
            "spaces must be percent-encoded, got: {url}"
        );
        // The url crate encodes the value properly; at minimum verify the key appears.
        assert!(
            url.contains("post_logout_redirect_uri="),
            "post_logout_redirect_uri key should appear, got: {url}"
        );
    }

    #[test]
    fn build_end_session_url_rejects_invalid_base_url() {
        let cfg = minimal_oauth_config(None, None);
        let result = build_end_session_url("not-a-valid-url", &cfg);
        assert!(result.is_err(), "invalid base URL should produce an error");
    }

    // ---------------------------------------------------------------------------
    // Handler tests: logout + frontchannel_logout
    // ---------------------------------------------------------------------------
    //
    // These tests wire up a minimal Router (no real DB — lazy pool only) and
    // use tower::ServiceExt::oneshot to send a single request.

    use crate::state::AppState;
    use axum::body::Body as AxumBody;
    use sqlx::postgres::PgPoolOptions;

    /// Build a minimal `AppState` suitable for handler tests (no real DB, no NATS).
    fn make_handler_state(oauth: Option<crate::config::OAuthConfig>) -> AppState {
        let db = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost/unused")
            .expect("lazy pool creation should not fail");
        AppState {
            db,
            jwt_secret: "test-secret".to_string(),
            token_ttl_secs: 60,
            session_ttl_secs: 3600,
            oauth,
            jwks_cache: None,
            cookie_domain: None,
            cookie_name: "session".to_string(),
            cookie_secure: false,
            nats: None,
            service_version_urls: vec![],
            http_client: reqwest::Client::new(),
        }
    }

    fn oauth_cfg_with_end_session(
        end_session_endpoint: &str,
        after_logout_url: Option<&str>,
    ) -> crate::config::OAuthConfig {
        minimal_oauth_config(
            Some(end_session_endpoint.to_string()),
            after_logout_url.map(|s| s.to_string()),
        )
    }

    // --- logout handler ---

    #[tokio::test]
    async fn logout_returns_200_when_no_end_session_endpoint() {
        use tower::ServiceExt;
        let state = make_handler_state(None);
        let app = axum::Router::new()
            .route("/logout", axum::routing::get(logout))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            set_cookie.contains("Max-Age=0"),
            "should clear cookie, got: {set_cookie}"
        );
    }

    #[tokio::test]
    async fn logout_redirects_when_end_session_endpoint_is_configured() {
        use tower::ServiceExt;
        let cfg = oauth_cfg_with_end_session(
            "https://provider.example.com/logout",
            Some("https://app.example.com/after-logout"),
        );
        let state = make_handler_state(Some(cfg));
        let app = axum::Router::new()
            .route("/logout", axum::routing::get(logout))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        // Must be a redirect (303 See Other).
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .expect("Location header must be present")
            .to_str()
            .unwrap();
        assert!(
            location.starts_with("https://provider.example.com/logout"),
            "Location should point to provider end-session endpoint: {location}"
        );
        assert!(
            location.contains("client_id=test-client"),
            "client_id must be present: {location}"
        );
        assert!(
            location.contains("post_logout_redirect_uri="),
            "post_logout_redirect_uri must be present: {location}"
        );
        // Cookie must also be cleared in the same response.
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            set_cookie.contains("Max-Age=0"),
            "cookie should be cleared alongside redirect: {set_cookie}"
        );
    }

    #[tokio::test]
    async fn logout_redirect_omits_post_logout_redirect_uri_when_not_configured() {
        use tower::ServiceExt;
        let cfg = oauth_cfg_with_end_session("https://provider.example.com/logout", None);
        let state = make_handler_state(Some(cfg));
        let app = axum::Router::new()
            .route("/logout", axum::routing::get(logout))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            !location.contains("post_logout_redirect_uri"),
            "post_logout_redirect_uri should be absent: {location}"
        );
    }

    // --- frontchannel_logout handler ---

    #[tokio::test]
    async fn frontchannel_logout_returns_200_and_clears_cookie() {
        use tower::ServiceExt;
        let state = make_handler_state(None);
        let app = axum::Router::new()
            .route(
                "/logout/frontchannel",
                axum::routing::get(frontchannel_logout),
            )
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout/frontchannel")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            set_cookie.contains("Max-Age=0"),
            "should clear cookie: {set_cookie}"
        );
        // Must NOT redirect — the request arrives in an iframe context.
        assert!(
            resp.headers().get(header::LOCATION).is_none(),
            "front-channel logout must not redirect"
        );
    }

    #[tokio::test]
    async fn frontchannel_logout_accepts_matching_iss_param() {
        use tower::ServiceExt;
        // minimal_oauth_config sets issuer = "https://provider.example.com"
        let cfg = minimal_oauth_config(None, None);
        let state = make_handler_state(Some(cfg));
        let app = axum::Router::new()
            .route(
                "/logout/frontchannel",
                axum::routing::get(frontchannel_logout),
            )
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout/frontchannel?iss=https%3A%2F%2Fprovider.example.com")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn frontchannel_logout_rejects_mismatched_iss_param() {
        use tower::ServiceExt;
        let cfg = minimal_oauth_config(None, None); // issuer = "https://provider.example.com"
        let state = make_handler_state(Some(cfg));
        let app = axum::Router::new()
            .route(
                "/logout/frontchannel",
                axum::routing::get(frontchannel_logout),
            )
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout/frontchannel?iss=https%3A%2F%2Fevil.example.com")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn frontchannel_logout_skips_iss_validation_when_no_oauth_configured() {
        use tower::ServiceExt;
        // When OAuth is not configured at all, any iss is accepted (no issuer to validate against).
        let state = make_handler_state(None);
        let app = axum::Router::new()
            .route(
                "/logout/frontchannel",
                axum::routing::get(frontchannel_logout),
            )
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout/frontchannel?iss=https%3A%2F%2Fanyone.example.com")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn frontchannel_logout_accepts_sid_param() {
        use tower::ServiceExt;
        let state = make_handler_state(None);
        let app = axum::Router::new()
            .route(
                "/logout/frontchannel",
                axum::routing::get(frontchannel_logout),
            )
            .with_state(state);

        // sid is accepted and logged; it cannot invalidate a stateless JWT server-side.
        let req = axum::http::Request::builder()
            .uri("/logout/frontchannel?sid=abc123session")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }
}

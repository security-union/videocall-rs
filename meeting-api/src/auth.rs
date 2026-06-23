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

//! Axum extractor that authenticates the user.
//!
//! Authentication is checked in order:
//!
//! 1. **Bearer token with JWKS** — when JWKS is configured and the request
//!    carries an `Authorization: Bearer <token>` header, the token is
//!    validated against the provider's JWKS (signature, `exp`, `iss`, and
//!    optionally `aud` via `OAUTH_RESOURCE_SERVER_AUDIENCE`).
//!
//! 2. **Session cookie** — when no Bearer token is present (or JWKS is not
//!    configured), the extractor looks for a server-issued session JWT in
//!    `Cookie: <cookie_name>=<JWT>` (set by the `/login/callback` handler
//!    in server-side OAuth mode) or in `Authorization: Bearer <JWT>`.
//!
//! This two-step approach supports both deployment modes:
//! - **Server-side OAuth** (default): the backend exchanges the code and
//!   sets an `HttpOnly` session cookie — the browser sends it automatically.
//! - **Client-side PKCE** (`oauthFlow: "pkce"`): the browser exchanges the
//!   code directly and sends the provider id_token as a Bearer header.

use axum::{
    extract::FromRequestParts,
    http::{header, request::Parts, StatusCode},
};
use jsonwebtoken::{decode, DecodingKey, Validation};
use videocall_meeting_types::{APIError, RoomAccessTokenClaims};

use crate::error::AppError;
use crate::state::AppState;
use crate::token;

/// Extractor that resolves the authenticated user from either:
///
/// - A provider id_token Bearer token (when JWKS is configured), or
/// - A legacy server-issued session JWT (cookie or Bearer header).
///
/// Usage in a handler:
/// ```ignore
/// async fn my_handler(AuthUser { user_id, .. }: AuthUser) { ... }
/// ```
#[derive(Debug)]
pub struct AuthUser {
    pub user_id: String,
    pub name: String,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // ----------------------------------------------------------------
        // Path 1 — Bearer token with JWKS validation (PKCE / external OAuth)
        //
        // When a JWKS cache and OAuth config are present AND the request
        // carries a Bearer token, validate it against the provider's JWKS.
        // ----------------------------------------------------------------
        if let (Some(jwks), Some(oauth_cfg)) = (state.jwks_cache.as_deref(), state.oauth.as_ref()) {
            if let Some(token) = extract_bearer_token(parts) {
                let claims = crate::oauth::verify_and_decode_id_token(
                    jwks,
                    &token,
                    oauth_cfg.resource_server_audience.as_deref(),
                    oauth_cfg.issuer.as_deref(),
                    None,
                )
                .await
                .map_err(|inner| {
                    // Distinguish a token-validation rejection (401) from a
                    // genuine server fault (e.g. JWKS endpoint unreachable or
                    // an unparseable JWKS response).
                    //
                    // For a 401, verify_and_decode_id_token has already logged
                    // the specific reason server-side (without token material),
                    // so we return a generic bearer-auth message and do NOT log
                    // the token. For anything else we preserve the original
                    // status but return a sanitized body: JWKS fetch errors can
                    // include internal URLs in their engineering detail.
                    if inner.status == StatusCode::UNAUTHORIZED {
                        tracing::warn!(
                            "Bearer auth rejected (401): invalid or expired bearer token"
                        );
                        AppError::unauthorized_msg("invalid or expired bearer token")
                    } else {
                        tracing::error!(
                            status = %inner.status,
                            detail = inner.body.engineering_error.as_deref().unwrap_or(""),
                            "Bearer auth failed due to a server fault (not a token rejection)"
                        );
                        AppError::new(
                            inner.status,
                            APIError::internal_error("authentication temporarily unavailable"),
                        )
                    }
                })?;

                let name = claims.display_name();
                let user_id = claims
                    .email
                    .filter(|e| !e.is_empty())
                    .or_else(|| claims.sub.filter(|s| !s.is_empty()))
                    .ok_or_else(|| {
                        AppError::unauthorized_msg(
                            "bearer token is missing both email and sub claims",
                        )
                    })?;

                if user_id.starts_with(videocall_meeting_types::GUEST_USER_ID_PREFIX) {
                    tracing::warn!("rejected bearer token with reserved guest: prefix user_id");
                    return Err(AppError::unauthorized_msg("invalid bearer token"));
                }

                return Ok(AuthUser { user_id, name });
            }
            // No Bearer token — fall through to session cookie path below.
        }

        // ----------------------------------------------------------------
        // Path 2 — server-issued session JWT (cookie or Bearer)
        //
        // Used by server-side OAuth (cookie set by /login/callback) and
        // deployments without an external identity provider.
        // ----------------------------------------------------------------
        if let Some(token) = extract_session_token(parts, &state.cookie_name) {
            let claims = token::decode_session_token(&state.jwt_secret, &token)?;
            if claims
                .sub
                .starts_with(videocall_meeting_types::GUEST_USER_ID_PREFIX)
            {
                tracing::warn!("rejected session token with reserved guest: prefix user_id");
                return Err(AppError::unauthorized_msg("invalid session token"));
            }
            return Ok(AuthUser {
                user_id: claims.sub,
                name: claims.name,
            });
        }

        Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            APIError::unauthorized(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Token extraction helpers
// ---------------------------------------------------------------------------

/// Extract an `Authorization: Bearer <token>` value from request headers.
///
/// Returns `None` when the header is absent, malformed, or empty.
fn extract_bearer_token(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Extract the raw session JWT from the request.
///
/// Checks (in order):
/// 1. `Cookie: <cookie_name>=<jwt>`
/// 2. `Authorization: Bearer <jwt>`
fn extract_session_token(parts: &Parts, cookie_name: &str) -> Option<String> {
    // 1. Try the configured session cookie name.
    if let Some(cookie_header) = parts
        .headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
    {
        let prefix = format!("{cookie_name}=");
        for pair in cookie_header.split(';') {
            let pair = pair.trim();
            if let Some(value) = pair.strip_prefix(prefix.as_str()) {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    // 2. Fall back to `Authorization: Bearer <token>`.
    extract_bearer_token(parts)
}

/// Extractor for a guest waiting in the lobby. Authenticates via the
/// `Authorization: Bearer <observer_token>` header (a signed observer JWT).
#[derive(Debug)]
pub struct GuestObserver {
    pub user_id: String,
    pub meeting_id: String,
    pub display_name: String,
}

impl FromRequestParts<AppState> for GuestObserver {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_bearer_token(parts)
            .ok_or_else(|| AppError::unauthorized_msg("missing Authorization: Bearer header"))?;

        let claims = token::decode_guest_token(&state.jwt_secret, &token)?;

        Ok(GuestObserver {
            user_id: claims.sub,
            meeting_id: claims.room,
            display_name: claims.display_name,
        })
    }
}

/// Extractor for an admitted room participant. Authenticates via the
/// `Authorization: Bearer <room_token>` header, where `<room_token>` is the
/// HS256 room-access JWT issued by [`crate::token::generate_room_token`] (the
/// same token the client uses to connect to the Media Server).
///
/// Unlike [`GuestObserver`], this accepts **both** guest and non-guest
/// room-access tokens, but rejects observer tokens (which carry
/// `room_join == false`) — an observer waiting in the lobby is not yet a
/// participant and must not be treated as one.
///
/// The token `room` claim identifies the meeting the bearer is authorized for;
/// handlers that bind to a `{meeting_id}` path segment must compare
/// [`RoomMember::meeting_id`] against the path and reject a mismatch.
///
/// Usage in a handler:
/// ```ignore
/// async fn my_handler(RoomMember { user_id, meeting_id, .. }: RoomMember) { ... }
/// ```
#[derive(Debug)]
pub struct RoomMember {
    /// The participant's identity (`sub` claim). Authoritative — derived from
    /// the signed token, never from a client-supplied header.
    pub user_id: String,
    /// The room/meeting ID the token authorizes (`room` claim).
    pub meeting_id: String,
    /// The participant's display name for this meeting.
    pub display_name: String,
}

impl FromRequestParts<AppState> for RoomMember {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_bearer_token(parts)
            .ok_or_else(|| AppError::unauthorized_msg("missing Authorization: Bearer header"))?;

        // Mirror `token::decode_guest_token`'s decode (Validation::default +
        // issuer pin + HS256 signature/exp check) but WITHOUT its `is_guest`
        // gate — a room token may be guest or non-guest, and both are valid
        // participants for the console-log upload path.
        let mut validation = Validation::default();
        validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);

        let claims = decode::<RoomAccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(state.jwt_secret.as_bytes()),
            &validation,
        )
        .map(|data| data.claims)
        .map_err(|e| {
            tracing::warn!("Room token validation failed: {e}");
            AppError::unauthorized_msg("invalid or expired room token")
        })?;

        // Reject observer tokens: `generate_room_token` always sets
        // `room_join: true`, while `generate_observer_token` sets it to
        // `false`. An observer has not been admitted as a participant, so it
        // is not a valid credential for participant-scoped routes (401 — the
        // presented credential does not authenticate a room member).
        if !claims.room_join {
            return Err(AppError::unauthorized_msg(
                "token does not grant room access",
            ));
        }

        Ok(RoomMember {
            user_id: claims.sub,
            meeting_id: claims.room,
            display_name: claims.display_name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::generate_session_token;
    use axum::http::Request;
    use sqlx::postgres::PgPoolOptions;

    const TEST_SECRET: &str = "test-secret-for-auth-tests";

    fn make_test_state() -> AppState {
        make_state_with_cookie_name("session")
    }

    fn make_state_with_cookie_name(name: &str) -> AppState {
        // connect_lazy creates a pool handle without actually connecting.
        // The URL is never used because no queries are executed in unit tests.
        let db = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost/unused")
            .expect("lazy pool creation should not fail");
        AppState {
            db,
            jwt_secret: TEST_SECRET.to_string(),
            token_ttl_secs: 600,
            session_ttl_secs: 3600,
            // oauth: None + jwks_cache: None → use legacy session JWT path.
            oauth: None,
            jwks_cache: None,
            cookie_domain: None,
            cookie_name: name.to_string(),
            cookie_secure: false,
            nats: None,
            feed_tx: crate::feed_events::new_feed_channel().0,
            service_version_urls: Vec::new(),
            http_client: reqwest::Client::new(),
            display_name_rate_limiter: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            display_name_rate_limiter_ops: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                0,
            )),
            search: None,
            display_name_rate_limit_disabled: false,
            dev_user: None,
        }
    }

    async fn extract_with_cookie(cookie: Option<&str>) -> Result<AuthUser, AppError> {
        let state = make_test_state();
        extract_with_cookie_and_state(cookie, &state).await
    }

    async fn extract_with_cookie_and_state(
        cookie: Option<&str>,
        state: &AppState,
    ) -> Result<AuthUser, AppError> {
        let mut builder = Request::builder().uri("/test").method("GET");
        if let Some(val) = cookie {
            builder = builder.header(header::COOKIE, val);
        }
        let req = builder.body(()).unwrap();
        let (mut parts, _) = req.into_parts();
        AuthUser::from_request_parts(&mut parts, state).await
    }

    async fn extract_with_bearer(token: &str) -> Result<AuthUser, AppError> {
        let state = make_test_state();
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        AuthUser::from_request_parts(&mut parts, &state).await
    }

    #[tokio::test]
    async fn valid_session_cookie_returns_auth_user() {
        let jwt = generate_session_token(TEST_SECRET, "alice@test.com", "Alice", 3600).unwrap();
        let auth = extract_with_cookie(Some(&format!("session={jwt}")))
            .await
            .expect("should succeed");
        assert_eq!(auth.user_id, "alice@test.com");
    }

    #[tokio::test]
    async fn valid_bearer_token_returns_auth_user() {
        let jwt = generate_session_token(TEST_SECRET, "bob@test.com", "Bob", 3600).unwrap();
        let auth = extract_with_bearer(&jwt).await.expect("should succeed");
        assert_eq!(auth.user_id, "bob@test.com");
        assert_eq!(auth.name, "Bob");
    }

    #[tokio::test]
    async fn valid_cookie_extracts_name() {
        let jwt =
            generate_session_token(TEST_SECRET, "alice@test.com", "Alice Wonder", 3600).unwrap();
        let auth = extract_with_cookie(Some(&format!("session={jwt}")))
            .await
            .expect("should succeed");
        assert_eq!(auth.user_id, "alice@test.com");
        assert_eq!(auth.name, "Alice Wonder");
    }

    #[tokio::test]
    async fn expired_bearer_token_returns_unauthorized() {
        let jwt = generate_session_token(TEST_SECRET, "a@b.com", "A", -120).unwrap();
        let err = extract_with_bearer(&jwt).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn invalid_bearer_token_returns_unauthorized() {
        let err = extract_with_bearer("not-a-valid-jwt").await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_secret_bearer_token_returns_unauthorized() {
        let jwt = generate_session_token("wrong-secret", "a@b.com", "A", 3600).unwrap();
        let err = extract_with_bearer(&jwt).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn empty_bearer_token_returns_unauthorized() {
        let state = make_test_state();
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .header(header::AUTHORIZATION, "Bearer ")
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let err = AuthUser::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_credentials_returns_unauthorized() {
        // Without any credentials the extractor always returns 401.
        let state = make_test_state();
        let err = extract_with_cookie_and_state(None, &state)
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn invalid_jwt_returns_unauthorized() {
        let err = extract_with_cookie(Some("session=not-a-valid-jwt"))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn expired_jwt_returns_unauthorized() {
        let jwt = generate_session_token(TEST_SECRET, "a@b.com", "A", -120).unwrap();
        let err = extract_with_cookie(Some(&format!("session={jwt}")))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_secret_returns_unauthorized() {
        let jwt = generate_session_token("different-secret", "a@b.com", "A", 3600).unwrap();
        let err = extract_with_cookie(Some(&format!("session={jwt}")))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cookie_takes_precedence_over_bearer() {
        let cookie_jwt =
            generate_session_token(TEST_SECRET, "cookie@test.com", "Cookie", 3600).unwrap();
        let bearer_jwt =
            generate_session_token(TEST_SECRET, "bearer@test.com", "Bearer", 3600).unwrap();

        let state = make_test_state();
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .header(header::COOKIE, format!("session={cookie_jwt}"))
            .header(header::AUTHORIZATION, format!("Bearer {bearer_jwt}"))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let auth = AuthUser::from_request_parts(&mut parts, &state)
            .await
            .expect("should succeed");
        assert_eq!(auth.user_id, "cookie@test.com");
    }

    #[tokio::test]
    async fn session_cookie_among_other_cookies() {
        let jwt = generate_session_token(TEST_SECRET, "multi@test.com", "Multi", 3600).unwrap();
        let auth = extract_with_cookie(Some(&format!("lang=en; session={jwt}; theme=dark")))
            .await
            .expect("should find session in middle");
        assert_eq!(auth.user_id, "multi@test.com");
    }

    // -----------------------------------------------------------------------
    // Custom cookie name tests (PR preview collision fix)
    // -----------------------------------------------------------------------

    /// PR preview API configured with "pr1-session" accepts a pr1-session= cookie.
    #[tokio::test]
    async fn custom_cookie_name_is_accepted() {
        let state = make_state_with_cookie_name("pr1-session");
        let jwt = generate_session_token(TEST_SECRET, "alice@test.com", "Alice", 3600).unwrap();
        let auth = extract_with_cookie_and_state(Some(&format!("pr1-session={jwt}")), &state)
            .await
            .expect("pr1-session cookie should be accepted");
        assert_eq!(auth.user_id, "alice@test.com");
    }

    /// Core regression test: PR preview API configured with "pr1-session" must
    /// reject a "session=" cookie — exactly what the production API sets with
    /// Domain=.videocall.rs, which the browser would otherwise send to
    /// pr1-api.sandbox.videocall.rs causing a 401.
    #[tokio::test]
    async fn production_session_cookie_rejected_by_preview_api() {
        let state = make_state_with_cookie_name("pr1-session");
        let production_jwt =
            generate_session_token(TEST_SECRET, "alice@test.com", "Alice", 3600).unwrap();
        // Even with a valid JWT, the wrong cookie name must be rejected.
        let err = extract_with_cookie_and_state(Some(&format!("session={production_jwt}")), &state)
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    /// Slot isolation: pr2-session= is rejected when the API expects pr1-session=.
    #[tokio::test]
    async fn different_slot_cookie_rejected() {
        let state = make_state_with_cookie_name("pr1-session");
        let jwt = generate_session_token(TEST_SECRET, "alice@test.com", "Alice", 3600).unwrap();
        let err = extract_with_cookie_and_state(Some(&format!("pr2-session={jwt}")), &state)
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    /// Custom cookie name is found correctly when mixed with other cookies,
    /// including a same-named-prefix cookie that should not match.
    #[tokio::test]
    async fn custom_cookie_name_among_other_cookies() {
        let state = make_state_with_cookie_name("pr1-session");
        let jwt = generate_session_token(TEST_SECRET, "multi@test.com", "Multi", 3600).unwrap();
        // "session" appears as a prefix of "pr1-session" in the cookie header —
        // verify we match the full name and don't accidentally split on it.
        let auth = extract_with_cookie_and_state(
            Some(&format!(
                "lang=en; session=garbage; pr1-session={jwt}; theme=dark"
            )),
            &state,
        )
        .await
        .expect("should find pr1-session and ignore session=garbage");
        assert_eq!(auth.user_id, "multi@test.com");
    }

    /// Bearer token still works regardless of cookie_name configuration.
    #[tokio::test]
    async fn bearer_works_with_custom_cookie_name() {
        let state = make_state_with_cookie_name("pr1-session");
        let jwt = generate_session_token(TEST_SECRET, "bob@test.com", "Bob", 3600).unwrap();
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let auth = AuthUser::from_request_parts(&mut parts, &state)
            .await
            .expect("bearer should work regardless of cookie_name");
        assert_eq!(auth.user_id, "bob@test.com");
    }

    // -----------------------------------------------------------------------
    // JWKS path tests
    //
    // These tests exercise the new provider id_token validation path
    // (auth.rs Path 1) by constructing an AppState with a pre-loaded
    // JwksCache and a minimal OAuthConfig.
    // -----------------------------------------------------------------------

    use crate::config::OAuthConfig;
    use crate::oauth::JwksCache;
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Generate a minimal OAuthConfig for unit tests.
    fn test_oauth_cfg() -> OAuthConfig {
        OAuthConfig {
            client_id: "test-client".to_string(),
            client_secret: None,
            redirect_url: "https://app.example.com/auth/callback".to_string(),
            issuer: Some("https://provider.example.com".to_string()),
            auth_url: "https://provider.example.com/auth".to_string(),
            token_url: "https://provider.example.com/token".to_string(),
            jwks_url: None,
            userinfo_url: None,
            scopes: "openid email profile".to_string(),
            after_login_url: "https://app.example.com/".to_string(),
            allowed_redirect_urls: vec![],
            end_session_endpoint: None,
            after_logout_url: None,
            browser_pkce: false,
            resource_server_audience: None,
        }
    }

    /// Build an AppState that uses JWKS-based validation.
    fn make_jwks_state(jwks: Arc<JwksCache>) -> AppState {
        make_jwks_state_with_audience(jwks, None)
    }

    /// Build an AppState that uses JWKS-based validation with an explicit
    /// resource-server audience restriction.
    fn make_jwks_state_with_audience(jwks: Arc<JwksCache>, audience: Option<&str>) -> AppState {
        let db = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost/unused")
            .expect("lazy pool");
        let mut cfg = test_oauth_cfg();
        cfg.resource_server_audience = audience.map(str::to_string);
        AppState {
            db,
            jwt_secret: TEST_SECRET.to_string(),
            token_ttl_secs: 600,
            session_ttl_secs: 3600,
            oauth: Some(cfg),
            jwks_cache: Some(jwks),
            cookie_domain: None,
            cookie_name: "session".to_string(),
            cookie_secure: false,
            nats: None,
            feed_tx: crate::feed_events::new_feed_channel().0,
            service_version_urls: vec![],
            http_client: reqwest::Client::new(),
            display_name_rate_limiter: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            display_name_rate_limiter_ops: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                0,
            )),
            search: None,
            display_name_rate_limit_disabled: false,
            dev_user: None,
        }
    }

    /// Generate a temporary RSA keypair for signing test JWTs.
    fn test_rsa_keypair() -> (EncodingKey, jsonwebtoken::DecodingKey, String) {
        use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey};
        use rsa::RsaPrivateKey;

        let mut rng = rand::thread_rng();
        let private_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let priv_pem = private_key
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap();
        let enc = EncodingKey::from_rsa_pem(priv_pem.as_bytes()).unwrap();

        let public_key = private_key.to_public_key();
        let pub_pem = public_key
            .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap();
        let dec = jsonwebtoken::DecodingKey::from_rsa_pem(pub_pem.as_bytes()).unwrap();

        (enc, dec, "jwks-test-kid".to_string())
    }

    /// Sign a minimal id_token with the given RSA key.
    fn sign_id_token(
        enc: &EncodingKey,
        kid: &str,
        email: &str,
        name: &str,
        client_id: &str,
        issuer: &str,
        exp_delta: i64,
    ) -> String {
        let now = chrono::Utc::now().timestamp() as u64;
        let claims = serde_json::json!({
            "sub": email,
            "email": email,
            "name": name,
            "iss": issuer,
            "aud": client_id,
            "exp": (now as i64 + exp_delta) as u64,
            "iat": now,
        });
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_string());
        encode(&header, &claims, enc).unwrap()
    }

    #[tokio::test]
    async fn jwks_path_valid_id_token_authenticates_user() {
        let (enc, dec, kid) = test_rsa_keypair();
        let mut keys = HashMap::new();
        keys.insert(kid.clone(), (Algorithm::RS256, dec));
        let jwks = JwksCache::with_keys(keys);

        let token = sign_id_token(
            &enc,
            &kid,
            "alice@example.com",
            "Alice",
            "test-client",
            "https://provider.example.com",
            3600,
        );

        let state = make_jwks_state(jwks);
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let auth = AuthUser::from_request_parts(&mut parts, &state)
            .await
            .expect("valid id_token should authenticate");

        assert_eq!(auth.user_id, "alice@example.com");
        assert_eq!(auth.name, "Alice");
    }

    #[tokio::test]
    async fn jwks_path_expired_token_rejected() {
        let (enc, dec, kid) = test_rsa_keypair();
        let mut keys = HashMap::new();
        keys.insert(kid.clone(), (Algorithm::RS256, dec));
        let jwks = JwksCache::with_keys(keys);

        let token = sign_id_token(
            &enc,
            &kid,
            "alice@example.com",
            "Alice",
            "test-client",
            "https://provider.example.com",
            -7200, // expired
        );

        let state = make_jwks_state(jwks);
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let err = AuthUser::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();

        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    /// HCL #1468 regression: a genuine server fault on the Bearer path (JWKS
    /// endpoint unreachable or unparseable) must NOT be masked as a 401. The
    /// structurally-valid token forces `verify_and_decode_id_token` past header
    /// parsing into `get_key`, whose cache miss triggers a refresh against an
    /// unreachable JWKS URL and returns a 500 the extractor must preserve.
    #[tokio::test]
    async fn jwks_path_server_fault_not_masked_as_401() {
        let (enc, _dec, _kid) = test_rsa_keypair();

        // Empty cache backed by an unreachable JWKS URL. The token's kid is not
        // cached, so get_key() triggers a refresh against the dead URL. The
        // failed refresh is the 500; the absent kid is only what reaches it.
        // Port 1 is unbound/privileged, so connection fails fast.
        let jwks = JwksCache::new("http://127.0.0.1:1/jwks".to_string());

        let token = sign_id_token(
            &enc,
            "kid-not-in-jwks",
            "alice@example.com",
            "Alice",
            "test-client",
            "https://provider.example.com",
            3600, // not expired — only the missing key should fail
        );

        let state = make_jwks_state(jwks);
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let err = AuthUser::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();

        assert_eq!(
            err.status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "JWKS-retrieval server fault must surface as 500, not be flattened to 401"
        );
        assert_eq!(
            err.body.engineering_error.as_deref(),
            Some("authentication temporarily unavailable"),
            "client-facing auth outage body must not expose the JWKS URL"
        );
    }

    #[tokio::test]
    async fn jwks_path_missing_bearer_rejected() {
        let (_, dec, kid) = test_rsa_keypair();
        let mut keys = HashMap::new();
        keys.insert(kid.clone(), (Algorithm::RS256, dec));
        let jwks = JwksCache::with_keys(keys);

        let state = make_jwks_state(jwks);
        // No Authorization header at all
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let err = AuthUser::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();

        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn jwks_path_session_cookie_accepted_as_fallback() {
        // When JWKS is configured but no Bearer token is present, the
        // extractor falls back to the session cookie.  This supports
        // server-side OAuth where the backend issues an HttpOnly cookie
        // after exchanging the authorization code.
        let (_, dec, kid) = test_rsa_keypair();
        let mut keys = HashMap::new();
        keys.insert(kid.clone(), (Algorithm::RS256, dec));
        let jwks = JwksCache::with_keys(keys);

        let state = make_jwks_state(jwks);
        let session_jwt =
            generate_session_token(TEST_SECRET, "alice@test.com", "Alice", 3600).unwrap();
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .header(header::COOKIE, format!("session={session_jwt}"))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let auth = AuthUser::from_request_parts(&mut parts, &state)
            .await
            .expect("session cookie should be accepted when no Bearer token is present");

        assert_eq!(auth.user_id, "alice@test.com");
        assert_eq!(auth.name, "Alice");
    }

    /// Access tokens often carry only `sub` (no `email`).  The extractor must
    /// use `sub` as `user_id` in that case.
    #[tokio::test]
    async fn jwks_path_access_token_sub_only_authenticates_user() {
        let (enc, dec, kid) = test_rsa_keypair();
        let mut keys = HashMap::new();
        keys.insert(kid.clone(), (Algorithm::RS256, dec));
        let jwks = JwksCache::with_keys(keys);

        // Access token: has sub but no email; aud is the resource server URL,
        // not the client_id.
        let now = chrono::Utc::now().timestamp() as u64;
        let claims = serde_json::json!({
            "sub": "opaque-user-sub-12345",
            "iss": "https://provider.example.com",
            "aud": "https://api.example.com",   // resource-server audience
            "exp": now + 3600,
            "iat": now,
        });
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.clone());
        let access_token = encode(&header, &claims, &enc).unwrap();

        let state = make_jwks_state(jwks);
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .header(header::AUTHORIZATION, format!("Bearer {access_token}"))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let auth = AuthUser::from_request_parts(&mut parts, &state)
            .await
            .expect("sub-only access token should authenticate");

        // user_id falls back to sub when email is absent
        assert_eq!(auth.user_id, "opaque-user-sub-12345");
    }

    // -----------------------------------------------------------------------
    // OAUTH_RESOURCE_SERVER_AUDIENCE tests
    //
    // When resource_server_audience is configured, per-request Bearer tokens
    // must carry that value in their `aud` claim.  Tokens for any other
    // audience — even if correctly signed by the same provider — are rejected.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn jwks_path_correct_resource_audience_accepted() {
        let (enc, dec, kid) = test_rsa_keypair();
        let mut keys = HashMap::new();
        keys.insert(kid.clone(), (Algorithm::RS256, dec));
        let jwks = JwksCache::with_keys(keys);

        // Token carries the configured resource-server audience.
        let now = chrono::Utc::now().timestamp() as u64;
        let claims = serde_json::json!({
            "sub": "alice@example.com",
            "email": "alice@example.com",
            "name": "Alice",
            "iss": "https://provider.example.com",
            "aud": "https://api.videocall.rs",
            "exp": now + 3600,
            "iat": now,
        });
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.clone());
        let token = encode(&header, &claims, &enc).unwrap();

        let state = make_jwks_state_with_audience(jwks, Some("https://api.videocall.rs"));
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let auth = AuthUser::from_request_parts(&mut parts, &state)
            .await
            .expect("token with correct audience should be accepted");
        assert_eq!(auth.user_id, "alice@example.com");
    }

    #[tokio::test]
    async fn jwks_path_wrong_resource_audience_rejected() {
        let (enc, dec, kid) = test_rsa_keypair();
        let mut keys = HashMap::new();
        keys.insert(kid.clone(), (Algorithm::RS256, dec));
        let jwks = JwksCache::with_keys(keys);

        // Token is signed by the same provider but carries a different
        // service's audience (confused deputy scenario).
        let now = chrono::Utc::now().timestamp() as u64;
        let claims = serde_json::json!({
            "sub": "alice@example.com",
            "email": "alice@example.com",
            "name": "Alice",
            "iss": "https://provider.example.com",
            "aud": "https://other-service.example.com",  // wrong audience
            "exp": now + 3600,
            "iat": now,
        });
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.clone());
        let token = encode(&header, &claims, &enc).unwrap();

        let state = make_jwks_state_with_audience(jwks, Some("https://api.videocall.rs"));
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let err = AuthUser::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();

        // A valid signature for the wrong audience must be rejected.
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // RoomMember extractor tests
    //
    // RoomMember authenticates participant-scoped routes (console-log upload)
    // via `Authorization: Bearer <room_token>`. It accepts both guest and
    // non-guest room tokens (room_join == true) and rejects observer tokens
    // (room_join == false).
    // -----------------------------------------------------------------------

    use crate::token::{generate_observer_token, generate_room_token};

    async fn extract_room_member(token: Option<&str>) -> Result<RoomMember, AppError> {
        let state = make_test_state();
        let mut builder = Request::builder().uri("/test").method("GET");
        if let Some(t) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = builder.body(()).unwrap();
        let (mut parts, _) = req.into_parts();
        RoomMember::from_request_parts(&mut parts, &state).await
    }

    #[tokio::test]
    async fn room_member_accepts_host_non_guest_token() {
        // generate_room_token(secret, ttl, user_id, room, is_host, name,
        //                     end_on_host_leave, is_guest)
        let token = generate_room_token(
            TEST_SECRET,
            600,
            "alice@test.com",
            "room-A",
            true, // is_host
            "Alice",
            true,
            false, // is_guest = false
        )
        .unwrap();
        let member = extract_room_member(Some(&token))
            .await
            .expect("valid host room token should authenticate");
        assert_eq!(member.user_id, "alice@test.com");
        assert_eq!(member.meeting_id, "room-A");
        assert_eq!(member.display_name, "Alice");
    }

    #[tokio::test]
    async fn room_member_accepts_guest_token() {
        let token = generate_room_token(
            TEST_SECRET,
            600,
            "guest:abc-123",
            "room-B",
            false,
            "Guesty",
            true,
            true, // is_guest = true
        )
        .unwrap();
        let member = extract_room_member(Some(&token))
            .await
            .expect("valid guest room token should authenticate");
        assert_eq!(member.user_id, "guest:abc-123");
        assert_eq!(member.meeting_id, "room-B");
    }

    #[tokio::test]
    async fn room_member_rejects_missing_bearer() {
        let err = extract_room_member(None).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn room_member_rejects_garbage_token() {
        let err = extract_room_member(Some("not-a-jwt")).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn room_member_rejects_wrong_secret_token() {
        // Signed by a different secret than the AppState's jwt_secret — the
        // HS256 signature check must reject it. If signature validation were
        // skipped, this token would otherwise decode cleanly (correct issuer,
        // unexpired) and this test would fail.
        let token = generate_room_token(
            "a-different-secret",
            600,
            "alice@test.com",
            "room-A",
            true,
            "Alice",
            true,
            false,
        )
        .unwrap();
        let err = extract_room_member(Some(&token)).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn room_member_rejects_observer_token() {
        // Observer tokens carry room_join == false. The extractor must reject
        // them: an observer waiting in the lobby is not an admitted
        // participant. If the room_join gate were dropped, this token would
        // authenticate (it is correctly signed with the right secret/issuer)
        // and this test would fail — which is exactly what it pins.
        let token =
            generate_observer_token(TEST_SECRET, "alice@test.com", "room-A", "Alice", false)
                .unwrap();
        let err = extract_room_member(Some(&token)).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn room_member_surfaces_minted_room_for_cross_meeting_check() {
        // The handler binds the upload to a `{meeting_id}` path segment by
        // comparing it against RoomMember::meeting_id (the token's `room`
        // claim). This test pins that the extractor surfaces the *minted*
        // room verbatim, so a token minted for "room-A" yields meeting_id
        // "room-A" — and a handler comparing it against path "room-B" will
        // therefore 403. We assert against the literal we minted with (not a
        // value derived from the result), so the assertion is not a tautology.
        let minted_room = "room-A";
        let token = generate_room_token(
            TEST_SECRET,
            600,
            "alice@test.com",
            minted_room,
            false,
            "Alice",
            true,
            false,
        )
        .unwrap();
        let member = extract_room_member(Some(&token))
            .await
            .expect("valid room token should authenticate");
        // Surfaced room must equal what we minted (handler accepts path "room-A").
        assert_eq!(member.meeting_id, "room-A");
        // And must NOT equal a different meeting (handler 403s on path "room-B").
        assert_ne!(member.meeting_id, "room-B");
    }
}

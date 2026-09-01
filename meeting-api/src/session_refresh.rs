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

//! Sliding refresh for the server-side session cookie.

use axum::{
    extract::{Request, State},
    http::{
        header::{COOKIE, SET_COOKIE},
        HeaderValue,
    },
    middleware::Next,
    response::Response,
};
use chrono::Utc;

use crate::cookie::build_session_cookie;
use crate::state::AppState;
use crate::token;

/// Re-mint a near-expiry session cookie up to the configured absolute cap.
///
/// This middleware intentionally reads only the `Cookie` header. It uses an
/// unverified `exp` only to skip fresh cookies, then fully verifies a
/// near-expiry JWT before re-minting. Bearer tokens are out of scope for cookie
/// sliding.
pub async fn slide_session_cookie(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let refresh_cookie = req
        .headers()
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookie_header| extract_cookie_value(cookie_header, &state.cookie_name))
        .and_then(|jwt| build_refresh_cookie(&state, &jwt));

    let mut response = next.run(req).await;

    if let Some(cookie) = refresh_cookie {
        let cookie_prefix = format!("{}=", state.cookie_name);
        let already_sets_session = response.headers().get_all(SET_COOKIE).iter().any(|value| {
            value
                .to_str()
                .map(|set_cookie| set_cookie.starts_with(&cookie_prefix))
                .unwrap_or(false)
        });

        if !already_sets_session {
            match HeaderValue::from_str(&cookie) {
                Ok(value) => {
                    response.headers_mut().append(SET_COOKIE, value);
                }
                Err(err) => {
                    tracing::error!("failed to build refreshed session cookie header: {err}");
                }
            }
        }
    }

    response
}

fn extract_cookie_value(cookie_header: &str, cookie_name: &str) -> Option<String> {
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
    None
}

fn build_refresh_cookie(state: &AppState, jwt: &str) -> Option<String> {
    let now = Utc::now().timestamp();
    let claims =
        decode_claims_for_refresh(jwt, now, state.session_refresh_threshold_secs, |jwt| {
            token::decode_session_token(
                &state.session_jwt_secret,
                state.previous_session_secret(now),
                jwt,
            )
            .ok()
        })?;

    // Absolute cap: never mint past `baseline + SESSION_ABSOLUTE_MAX_SECS`.
    // `capped_session_ttl` clamps the fresh lifetime to land exactly at the cap;
    // when the cap is already reached (or passed) it is <= 0, which is the
    // single authoritative guard that forces a real re-login. Same helper the
    // initial login/dev mints use, so the cap invariant holds at every mint.
    let baseline = claims.auth_time.unwrap_or(claims.iat);
    let ttl = token::capped_session_ttl(
        state.session_ttl_secs,
        baseline,
        state.session_absolute_max_secs,
        now,
    );
    if ttl <= 0 {
        return None;
    }

    let refreshed_jwt = token::generate_session_token(
        &state.session_jwt_secret,
        &claims.sub,
        &claims.name,
        ttl,
        baseline,
    )
    .ok()?;

    Some(build_session_cookie(
        &state.cookie_name,
        &refreshed_jwt,
        ttl,
        state.cookie_domain.as_deref(),
        state.cookie_secure,
    ))
}

fn decode_claims_for_refresh<F>(
    jwt: &str,
    now: i64,
    refresh_threshold_secs: i64,
    decode_verified: F,
) -> Option<token::SessionTokenClaims>
where
    F: FnOnce(&str) -> Option<token::SessionTokenClaims>,
{
    let exp = token::decode_session_exp_unverified(jwt)?;
    if exp.saturating_sub(now) >= refresh_threshold_secs {
        return None;
    }

    decode_verified(jwt)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const TEST_SECRET: &str = "session-refresh-test-secret";
    const PREVIOUS_SECRET: &str = "session-refresh-outgoing-secret";

    fn state_with_window(previous: Option<&str>, expires_at: i64) -> AppState {
        // connect_lazy yields a pool handle without contacting Postgres; the
        // refresh path runs no queries.
        let db = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost/unused")
            .expect("lazy pool creation should not fail");
        AppState {
            db,
            jwt_secret: "room-secret".to_string(),
            session_jwt_secret: TEST_SECRET.to_string(),
            session_jwt_secret_previous: previous.map(str::to_string),
            session_previous_secret_expires_at: expires_at,
            token_ttl_secs: 600,
            session_ttl_secs: 3600,
            session_refresh_threshold_secs: 300,
            session_absolute_max_secs: 604_800,
            oauth: None,
            jwks_cache: None,
            cookie_domain: None,
            cookie_name: "session".to_string(),
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
            password_gate: std::sync::Arc::new(crate::password::MeetingPasswordGate::new()),
        }
    }

    fn jwt_from_set_cookie(set_cookie: &str) -> &str {
        set_cookie
            .strip_prefix("session=")
            .and_then(|rest| rest.split(';').next())
            .expect("refresh cookie must carry a JWT")
    }

    #[tokio::test]
    async fn a_previous_key_cookie_is_re_minted_under_the_current_key() {
        let now = Utc::now().timestamp();
        let state = state_with_window(Some(PREVIOUS_SECRET), now + 600);
        // TTL 60 < threshold 300, so the cookie is inside the refresh window.
        let jwt = token::generate_session_token(PREVIOUS_SECRET, "user@test.com", "User", 60, now)
            .expect("session token should encode");

        let cookie =
            build_refresh_cookie(&state, &jwt).expect("an open window must re-mint an old cookie");
        let refreshed = jwt_from_set_cookie(&cookie);

        token::decode_session_token(TEST_SECRET, None, refreshed)
            .expect("the re-mint must verify under the CURRENT key with no window configured");
        assert!(
            token::decode_session_token(PREVIOUS_SECRET, None, refreshed).is_err(),
            "signing must never use the previous key"
        );
    }

    #[tokio::test]
    async fn a_previous_key_cookie_is_not_re_minted_once_the_window_has_closed() {
        let now = Utc::now().timestamp();
        let jwt = token::generate_session_token(PREVIOUS_SECRET, "user@test.com", "User", 60, now)
            .expect("session token should encode");

        assert!(
            build_refresh_cookie(&state_with_window(Some(PREVIOUS_SECRET), now), &jwt).is_none(),
            "an expired window must not verify the previous key"
        );
        assert!(
            build_refresh_cookie(&state_with_window(None, now + 600), &jwt).is_none(),
            "an unconfigured window must not verify the previous key"
        );
    }

    #[tokio::test]
    async fn previous_session_secret_is_gated_on_the_window_deadline() {
        let state = state_with_window(Some(PREVIOUS_SECRET), 1_000);

        assert_eq!(state.previous_session_secret(999), Some(PREVIOUS_SECRET));
        assert_eq!(state.previous_session_secret(1_000), None);
        assert_eq!(state.previous_session_secret(1_001), None);
        assert_eq!(
            state_with_window(None, 1_000).previous_session_secret(0),
            None
        );
    }

    #[test]
    fn fresh_cookie_early_out_skips_verified_decode() {
        let now = Utc::now().timestamp();
        let jwt = token::generate_session_token(TEST_SECRET, "user", "User", 3_600, now)
            .expect("session token should encode");
        let verify_calls = Cell::new(0);

        let claims = decode_claims_for_refresh(&jwt, now, 300, |jwt| {
            verify_calls.set(verify_calls.get() + 1);
            token::decode_session_token(TEST_SECRET, None, jwt).ok()
        });

        assert!(claims.is_none(), "fresh cookie must not enter refresh path");
        assert_eq!(
            verify_calls.get(),
            0,
            "fresh cookie must not perform the middleware's second HMAC verification"
        );
    }

    #[test]
    fn near_expiry_cookie_runs_verified_decode() {
        let now = Utc::now().timestamp();
        let jwt = token::generate_session_token(TEST_SECRET, "user", "User", 60, now)
            .expect("session token should encode");
        let verify_calls = Cell::new(0);

        let claims = decode_claims_for_refresh(&jwt, now, 300, |jwt| {
            verify_calls.set(verify_calls.get() + 1);
            token::decode_session_token(TEST_SECRET, None, jwt).ok()
        });

        assert!(
            claims.is_some(),
            "near-expiry signed cookie must be verified"
        );
        assert_eq!(
            verify_calls.get(),
            1,
            "near-expiry cookie must perform one full verification before re-mint"
        );
    }
}

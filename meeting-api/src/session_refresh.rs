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
            token::decode_session_token(&state.jwt_secret, jwt).ok()
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

    let refreshed_jwt =
        token::generate_session_token(&state.jwt_secret, &claims.sub, &claims.name, ttl, baseline)
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

    #[test]
    fn fresh_cookie_early_out_skips_verified_decode() {
        let now = Utc::now().timestamp();
        let jwt = token::generate_session_token(TEST_SECRET, "user", "User", 3_600, now)
            .expect("session token should encode");
        let verify_calls = Cell::new(0);

        let claims = decode_claims_for_refresh(&jwt, now, 300, |jwt| {
            verify_calls.set(verify_calls.get() + 1);
            token::decode_session_token(TEST_SECRET, jwt).ok()
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
            token::decode_session_token(TEST_SECRET, jwt).ok()
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

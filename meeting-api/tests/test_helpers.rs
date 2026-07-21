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

//! Shared test helpers. One suite, run against whichever backend the crate was
//! compiled with; CI runs the same commands twice (`make tests_run` /
//! `tests_run_sqlite`), so coverage is equal by construction.
//!
//! Both legs go through [`meeting_api::db::connect`] on a database dbmate has
//! already migrated — the SQLite leg exercises `dbmate/sqlite`, the same files
//! production runs, so drift between the two dbmate dirs is caught here. Tests
//! only see [`meeting_api::db::DbPool`] and route SQL through
//! [`meeting_api::db::q`], so none needs a `cfg` of its own.

#![allow(dead_code)]

use axum::http;
use axum::response::Response;
use axum::Router;
use http_body_util::BodyExt;
use meeting_api::db::{q, DbPool};
use meeting_api::{routes, state::AppState, token::generate_session_token};
use serde::de::DeserializeOwned;

pub const TEST_JWT_SECRET: &str = "test-secret-for-integration-tests";
const TEST_TOKEN_TTL: i64 = 600;
const TEST_SESSION_TTL: i64 = 3600;

/// Connect to the test database named by `DATABASE_URL`, expected to already be
/// migrated (`dbmate up` runs first — see the Makefile).
///
/// Backend-agnostic: [`meeting_api::db::connect`] is the constructor `main` uses,
/// so tests exercise the production pragmas rather than a reimplementation, and
/// nothing here builds a schema of its own.
pub async fn get_test_pool() -> DbPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    meeting_api::db::connect(&url).await
}

/// Delete all test data for a given `room_id` (participants first due to FK).
pub async fn cleanup_test_data(pool: &DbPool, room_id: &str) {
    let _ = sqlx::query(&q("DELETE FROM meeting_participants WHERE meeting_id IN \
         (SELECT id FROM meetings WHERE room_id = $1)"))
    .bind(room_id)
    .execute(pool)
    .await;

    let _ = sqlx::query(&q("DELETE FROM meetings WHERE room_id = $1"))
        .bind(room_id)
        .execute(pool)
        .await;
}

/// Build the Axum router backed by the given pool, ready for `tower::ServiceExt::oneshot`.
pub fn build_app(pool: DbPool) -> Router {
    let state = AppState {
        db: pool,
        jwt_secret: TEST_JWT_SECRET.to_string(),
        token_ttl_secs: TEST_TOKEN_TTL,
        session_ttl_secs: TEST_SESSION_TTL,
        oauth: None,
        jwks_cache: None,
        cookie_domain: None,
        cookie_name: "session".to_string(),
        cookie_secure: false,
        nats: None,
        service_version_urls: Vec::new(),
        http_client: reqwest::Client::new(),
    };
    routes::router().with_state(state)
}

/// Build an HTTP request with a signed session JWT in the `Cookie: session=<jwt>` header.
///
/// This replaces the old `Cookie: email=<email>` pattern. The JWT is signed
/// with [`TEST_JWT_SECRET`] and contains the email in the `sub` claim.
pub fn request_with_cookie(method: &str, uri: &str, email: &str) -> http::request::Builder {
    let session_jwt = generate_session_token(TEST_JWT_SECRET, email, email, TEST_SESSION_TTL)
        .expect("signing session JWT for test should not fail");
    http::Request::builder()
        .method(method)
        .uri(uri)
        .header("Cookie", format!("session={session_jwt}"))
}

/// Consume a response body and deserialize JSON into `T`.
pub async fn response_json<T: DeserializeOwned>(resp: Response) -> T {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("deserialize response body")
}

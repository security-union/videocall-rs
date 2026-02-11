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

//! Shared test helpers for meeting-api integration tests.

#![allow(dead_code)]

use axum::http;
use axum::response::Response;
use axum::Router;
use http_body_util::BodyExt;
use meeting_api::{routes, state::AppState};
use serde::de::DeserializeOwned;
use sqlx::PgPool;

const TEST_JWT_SECRET: &str = "test-secret-for-integration-tests";
const TEST_TOKEN_TTL: i64 = 600;

/// Connect to the test database using `DATABASE_URL`.
pub async fn get_test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    PgPool::connect(&url)
        .await
        .expect("Failed to connect to test database")
}

/// Delete all test data for a given `room_id` (participants first due to FK).
pub async fn cleanup_test_data(pool: &PgPool, room_id: &str) {
    let _ = sqlx::query(
        "DELETE FROM meeting_participants WHERE meeting_id IN \
         (SELECT id FROM meetings WHERE room_id = $1)",
    )
    .bind(room_id)
    .execute(pool)
    .await;

    let _ = sqlx::query("DELETE FROM meetings WHERE room_id = $1")
        .bind(room_id)
        .execute(pool)
        .await;
}

/// Build the Axum router backed by the given pool, ready for `tower::ServiceExt::oneshot`.
pub fn build_app(pool: PgPool) -> Router {
    let state = AppState {
        db: pool,
        jwt_secret: TEST_JWT_SECRET.to_string(),
        token_ttl_secs: TEST_TOKEN_TTL,
        oauth: None,
        cookie_domain: None,
    };
    routes::router().with_state(state)
}

/// Build an HTTP request with the `Cookie: email=<email>` header.
pub fn request_with_cookie(method: &str, uri: &str, email: &str) -> http::request::Builder {
    http::Request::builder()
        .method(method)
        .uri(uri)
        .header("Cookie", format!("email={email}"))
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

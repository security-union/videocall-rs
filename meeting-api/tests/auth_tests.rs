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

//! Integration tests verifying that endpoints reject unauthenticated requests.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::responses::APIResponse;
use videocall_meeting_types::APIError;

/// Build a request WITHOUT the email cookie (unauthenticated).
fn unauthenticated_request(method: &str, uri: &str) -> axum::http::request::Builder {
    axum::http::Request::builder().method(method).uri(uri)
}

#[tokio::test]
#[serial]
async fn test_create_meeting_unauthorized() {
    let pool = get_test_pool().await;
    let app = build_app(pool.clone());

    let req = unauthenticated_request("POST", "/api/v1/meetings")
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"meeting_id":"unauthorized","attendees":[]}"#,
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "UNAUTHORIZED");
}

#[tokio::test]
#[serial]
async fn test_get_meeting_unauthorized() {
    let pool = get_test_pool().await;
    let app = build_app(pool.clone());

    let req = unauthenticated_request("GET", "/api/v1/meetings/some-meeting")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[serial]
async fn test_list_meetings_unauthorized() {
    let pool = get_test_pool().await;
    let app = build_app(pool.clone());

    let req = unauthenticated_request("GET", "/api/v1/meetings?limit=10&offset=0")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

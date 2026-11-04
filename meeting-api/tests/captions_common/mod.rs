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

#![allow(dead_code)]

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use meeting_api::captions::{
    captions_control_subject, CaptionsControlPayload, RevokeReason, CAPTIONS_TTL_SECS,
    SERVER_REQUESTER,
};
use meeting_api::state::AppState;
use tower::ServiceExt;

use crate::test_helpers::*;

pub const HOST: &str = "captions-host@example.com";

pub async fn send(state: &AppState, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let resp = build_app_from_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

pub async fn as_user(
    state: &AppState,
    method: &str,
    uri: &str,
    user: &str,
    body: serde_json::Value,
) {
    let req = request_with_cookie(method, uri, user);
    let req = if body.is_null() {
        req.body(Body::empty())
    } else {
        req.header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
    };
    let (status, json) = send(state, req.unwrap()).await;
    assert!(
        status.is_success(),
        "{user} {method} {uri}: {status} {json}"
    );
}

pub async fn create_meeting(state: &AppState, room: &str, extra: serde_json::Value) {
    cleanup_test_data(&state.db, room).await;
    let mut body = serde_json::json!({ "meeting_id": room, "attendees": [] });
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    as_user(state, "POST", "/api/v1/meetings", HOST, body).await;
    join(state, room, HOST).await;
}

pub async fn join(state: &AppState, room: &str, user: &str) {
    let uri = format!("/api/v1/meetings/{room}/join");
    as_user(state, "POST", &uri, user, serde_json::Value::Null).await;
}

pub async fn meeting_pk(state: &AppState, room: &str) -> i32 {
    sqlx::query_scalar("SELECT id FROM meetings WHERE room_id = $1 AND deleted_at IS NULL")
        .bind(room)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

pub async fn captions_state(state: &AppState, room: &str) -> (bool, i64) {
    sqlx::query_as(
        "SELECT captions_on, captions_generation FROM meetings WHERE room_id = $1 \
         ORDER BY deleted_at IS NOT NULL, id DESC LIMIT 1",
    )
    .bind(room)
    .fetch_one(&state.db)
    .await
    .unwrap()
}

pub async fn subscribe(nats: &async_nats::Client, room: &str) -> async_nats::Subscriber {
    let sub = nats
        .subscribe(captions_control_subject(room).unwrap())
        .await
        .unwrap();
    nats.flush().await.unwrap();
    sub
}

/// Subscribes, then creates the meeting, consuming the revoke its first instance publishes.
pub async fn create_subscribed(
    state: &AppState,
    nats: &async_nats::Client,
    room: &str,
    extra: serde_json::Value,
) -> async_nats::Subscriber {
    let mut sub = subscribe(nats, room).await;
    create_meeting(state, room, extra).await;
    let first = server_revoke(room, 0, RevokeReason::NewInstance);
    assert_eq!(next_payload(&mut sub).await, first);
    sub
}

pub async fn next_payload(sub: &mut async_nats::Subscriber) -> CaptionsControlPayload {
    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("captions control published")
        .unwrap();
    serde_json::from_slice(&msg.payload).unwrap()
}

pub async fn assert_nothing_published(sub: &mut async_nats::Subscriber) {
    let next = tokio::time::timeout(Duration::from_millis(1500), sub.next()).await;
    assert!(next.is_err(), "unexpected publish: {next:?}");
}

pub fn lease(room: &str, by: &str, generation: i64) -> CaptionsControlPayload {
    CaptionsControlPayload {
        room_id: room.to_string(),
        enabled: true,
        revoke: false,
        requested_by: by.to_string(),
        ttl_secs: CAPTIONS_TTL_SECS,
        generation,
        reason: None,
    }
}

pub fn revoked(room: &str, by: &str, generation: i64) -> CaptionsControlPayload {
    CaptionsControlPayload {
        enabled: false,
        revoke: true,
        ttl_secs: 0,
        ..lease(room, by, generation)
    }
}

pub fn server_revoke(room: &str, generation: i64, reason: RevokeReason) -> CaptionsControlPayload {
    CaptionsControlPayload {
        reason: Some(reason),
        ..revoked(room, SERVER_REQUESTER, generation)
    }
}

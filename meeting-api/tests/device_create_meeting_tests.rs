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

//! A CLI device creates, lists and starts meetings, and never hosts one.

mod test_helpers;

use axum::body::Body;
use axum::http::{request::Builder, StatusCode};
use meeting_api::db::{meetings as db_meetings, DbPool};
use meeting_api::routes::meetings::CLI_MAX_OPEN_MEETINGS;
use meeting_api::state::AppState;
use serde_json::{json, Value};
use serial_test::serial;
use test_helpers::*;

const MEETINGS: &str = "/api/v1/meetings";

/// Send a JSON request and return its status and `result`.
async fn call(state: &AppState, req: Builder, body: Value) -> (StatusCode, Value) {
    let req = req
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = send(state, req).await;
    (
        resp.status(),
        response_json::<Value>(resp).await["result"].clone(),
    )
}

/// A fresh device and a room id prefix unique to this run.
async fn setup() -> (DbPool, AppState, TestDevice, String) {
    let pool = get_test_pool().await;
    let device = insert_device(&pool, &unique_user(), Fixture::Live).await;
    let prefix = format!("dc{}", &device.user_id.replace(['@', '.', '_'], "")[..12]);
    (pool.clone(), build_state(pool), device, prefix)
}

/// Delete the device and every meeting these users own.
async fn teardown(pool: &DbPool, device: &TestDevice, others: &[&str]) {
    delete_device_data(pool, &[&device.user_id]).await;
    for creator in others.iter().copied().chain([device.user_id.as_str()]) {
        for sql in [
            "DELETE FROM meeting_participants WHERE meeting_id IN \
             (SELECT id FROM meetings WHERE creator_id = $1)",
            "DELETE FROM meetings WHERE creator_id = $1",
        ] {
            let _ = sqlx::query(sql).bind(creator).execute(pool).await;
        }
    }
}

async fn seed(pool: &DbPool, room: &str, creator: &str) -> i32 {
    db_meetings::create(pool, room, creator, None, &json!([]))
        .await
        .unwrap()
        .id
}

#[tokio::test]
#[serial]
async fn device_create() {
    let (pool, state, device, prefix) = setup().await;
    let create = || bearer("POST", MEETINGS, &device.token());
    let room = format!("{prefix}-chosen");
    let (status, result) = call(&state, create(), json!({ "meeting_id": room })).await;
    assert_eq!(status, StatusCode::CREATED, "{result}");
    assert_eq!(result["waiting_room_enabled"], true);
    let row = meeting(&pool, &room).await.expect("stored");
    assert_eq!(row.creator_id.as_deref(), Some(device.user_id.as_str()));
    assert!(row.waiting_room_enabled && row.password_hash.is_none());

    let refused = format!("{prefix}-refused");
    for body in [
        json!({ "meeting_id": refused, "password": "" }),
        json!({ "meeting_id": refused, "attendees": ["a@b.c"] }),
        json!({ "meeting_id": refused, "waiting_room_enabled": false }),
    ] {
        let (status, result) = call(&state, create(), body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {result}");
        assert_eq!(result["code"], "DEVICE_OPTION_NOT_ALLOWED", "{body}");
        assert!(meeting(&pool, &refused).await.is_none(), "{body}");
    }

    // The cap counts open meetings; the one created above is the 25th.
    let mut seeded = 0;
    for i in 1..CLI_MAX_OPEN_MEETINGS {
        seeded = seed(&pool, &format!("{prefix}-{i}"), &device.user_id).await;
    }
    let (status, result) = call(&state, create(), json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{result}");
    assert_eq!(result["code"], "TOO_MANY_OPEN_MEETINGS");
    // Ended meetings don't count.
    db_meetings::end_meeting(&pool, seeded).await.unwrap();
    let (status, result) = call(&state, create(), json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{result}");
    // At the cap again, a browser create is neither capped nor option-restricted.
    let browser = request_with_cookie("POST", MEETINGS, &device.user_id);
    let (status, result) = call(&state, browser, json!({ "password": "hunter2" })).await;
    assert_eq!(status, StatusCode::CREATED, "{result}");

    teardown(&pool, &device, &[]).await;
}

#[tokio::test]
#[serial]
async fn cli_creator_starts_an_idle_meeting_without_hosting() {
    let (pool, state, device, prefix) = setup().await;
    let join = |room: &str| {
        let uri = format!("{MEETINGS}/{room}/join");
        bearer("POST", &uri, &device.token())
    };
    let room = format!("{prefix}-start");
    seed(&pool, &room, &device.user_id).await;
    let (status, result) = call(&state, join(&room), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["status"], "admitted");
    assert_eq!(result["is_host"], false);
    assert!(!room_token_claims(&result).is_host);
    assert_eq!(
        meeting(&pool, &room).await.unwrap().state.as_deref(),
        Some("active")
    );
    let is_host: bool = sqlx::query_scalar(
        "SELECT p.is_host FROM meeting_participants p JOIN meetings m ON m.id = p.meeting_id \
         WHERE m.room_id = $1 AND p.user_id = $2 AND m.deleted_at IS NULL",
    )
    .bind(&room)
    .bind(&device.user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!is_host);

    // Joins that start nothing: an ended meeting, someone else's idle one, a missing one.
    let other = unique_user();
    let (ended, others, missing) = (
        format!("{prefix}-ended"),
        format!("{prefix}-other"),
        format!("{prefix}-missing"),
    );
    let id = seed(&pool, &ended, &device.user_id).await;
    db_meetings::end_meeting(&pool, id).await.unwrap();
    seed(&pool, &others, &other).await;
    for (room, after) in [
        (&ended, Some("ended")),
        (&others, Some("idle")),
        (&missing, None),
    ] {
        let (status, result) = call(&state, join(room), json!({})).await;
        if after.is_some() {
            assert_eq!(status, StatusCode::OK, "{room}: {result}");
            assert_eq!(result["status"], "waiting_for_meeting", "{room}");
            assert!(result["room_token"].is_null(), "{room}");
        } else {
            assert_eq!(status, StatusCode::NOT_FOUND, "{room}: {result}");
        }
        let now = meeting(&pool, room).await.and_then(|m| m.state);
        assert_eq!(now.as_deref(), after, "{room}");
    }

    teardown(&pool, &device, &[&other]).await;
}

#[tokio::test]
#[serial]
async fn device_lists_what_the_browser_lists() {
    let (pool, state, device, prefix) = setup().await;
    let other = unique_user();
    seed(&pool, &format!("{prefix}-other"), &other).await;
    let mine = format!("{prefix}-mine");
    seed(&pool, &mine, &device.user_id).await;

    let uri = format!("{MEETINGS}?limit=5");
    let (status, from_device) = call(&state, bearer("GET", &uri, &device.token()), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{from_device}");
    let browser = request_with_cookie("GET", &uri, &device.user_id);
    let (_, from_browser) = call(&state, browser, json!({})).await;
    assert_eq!(from_device, from_browser);
    assert_eq!(from_device["total"], 1);
    assert_eq!(from_device["meetings"][0]["meeting_id"], mine.as_str());

    teardown(&pool, &device, &[&other]).await;
}

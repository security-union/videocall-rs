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

//! Integration tests for the `chat_allowed_for_all` meeting setting.
//!
//! Two properties are pinned here:
//!
//! 1. **Ownership gate** — a PATCH of `chat_allowed_for_all` by a non-owner is
//!    rejected with 403 and MUST leave the stored value unchanged. The gate is
//!    inherited from the `UPDATE … WHERE creator_id = $2` fold in
//!    `db_meetings::update_meeting_settings`, but this test proves it applies to
//!    the NEW field specifically (a future refactor that special-cased chat
//!    could bypass it).
//! 2. **Persistence round-trip** — the column round-trips through
//!    `create_with_options` and `update_meeting_settings` → `get_by_room_id`,
//!    including the COALESCE no-op when the field is omitted (`None`).
//!
//! Tests run against a live Postgres pool via `DATABASE_URL`. They compile
//! locally but require a DB to execute (run in CI against the provisioned
//! Postgres).

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use meeting_api::db::meetings as db_meetings;
use meeting_api::password::PasswordUpdate;
use serde_json::json;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;

/// A non-owner PATCH of `chat_allowed_for_all` must be rejected (403) and must
/// NOT mutate the stored value.
///
/// Mutation sensitivity: drop the ownership fold (`WHERE creator_id = $2`) or
/// special-case chat around it, and the intruder's PATCH would flip the setting
/// — the post-PATCH `assert!(row.chat_allowed_for_all)` then fails.
#[tokio::test]
#[serial]
async fn non_owner_patch_chat_setting_is_rejected_and_db_unchanged() {
    let pool = get_test_pool().await;
    let room_id = "chat_owner_guard_room";
    let owner = "chat-owner@example.com";
    let intruder = "chat-intruder@example.com";
    cleanup_test_data(&pool, room_id).await;

    // Owner creates a meeting; chat starts allowed for all (the default).
    db_meetings::create_with_options(
        &pool,
        room_id,
        owner,
        None,
        &json!([]),
        // wr / aca / eohl / allow_guests / recording / chat
        true,
        false,
        true,
        false,
        false,
        true,
    )
    .await
    .expect("create_with_options must succeed");

    // A different authenticated user tries to restrict chat.
    let app = build_app(pool.clone());
    let req = request_with_cookie("PATCH", &format!("/api/v1/meetings/{room_id}"), intruder)
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"chat_allowed_for_all":false}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a non-owner PATCH of chat_allowed_for_all must be rejected with 403"
    );

    // The stored value must be untouched.
    let row = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .expect("meeting row must still exist");
    assert!(
        row.chat_allowed_for_all,
        "a rejected non-owner PATCH must not mutate chat_allowed_for_all"
    );

    // Positive control: the OWNER can restrict chat, and it persists.
    let app = build_app(pool.clone());
    let req = request_with_cookie("PATCH", &format!("/api/v1/meetings/{room_id}"), owner)
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"chat_allowed_for_all":false}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the owner's PATCH of chat_allowed_for_all must succeed"
    );
    let row = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .expect("meeting row must still exist");
    assert!(
        !row.chat_allowed_for_all,
        "the owner's PATCH must persist chat_allowed_for_all=false"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// `chat_allowed_for_all` round-trips through the DB layer: `create_with_options`
/// persists the initial value, `update_meeting_settings(Some(_))` overwrites it,
/// and `update_meeting_settings(None)` is a COALESCE no-op.
///
/// Mutation sensitivity: drop `chat_allowed_for_all` from the INSERT/SELECT or
/// the `COALESCE($8, chat_allowed_for_all)` SET clause and one of these
/// re-fetch assertions fails.
#[tokio::test]
#[serial]
async fn chat_allowed_for_all_round_trips_through_db() {
    let pool = get_test_pool().await;
    let room_id = "chat_roundtrip_room";
    let owner = "chat-rt-owner@example.com";
    cleanup_test_data(&pool, room_id).await;

    // create_with_options(chat=false) must persist false (not the column default).
    db_meetings::create_with_options(
        &pool,
        room_id,
        owner,
        None,
        &json!([]),
        // wr / aca / eohl / allow_guests / recording / chat
        true,
        false,
        true,
        false,
        false,
        false,
    )
    .await
    .expect("create_with_options must succeed");
    let row = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .expect("meeting row must exist");
    assert!(
        !row.chat_allowed_for_all,
        "create_with_options(false) must persist chat_allowed_for_all=false"
    );

    // update_meeting_settings(chat=Some(true)) flips it, and it survives a re-fetch.
    let updated = db_meetings::update_meeting_settings(
        &pool,
        room_id,
        owner,
        None,
        None,
        None,
        None,
        None,
        Some(true),
        &PasswordUpdate::Unchanged,
    )
    .await
    .unwrap()
    .expect("owned row must update")
    .row;
    assert!(
        updated.chat_allowed_for_all,
        "update to Some(true) must return the new value"
    );
    let refetched = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        refetched.chat_allowed_for_all,
        "the persisted true must survive a re-fetch"
    );

    // update_meeting_settings(chat=None) is a COALESCE no-op — the value stays true.
    let unchanged = db_meetings::update_meeting_settings(
        &pool,
        room_id,
        owner,
        None,
        None,
        None,
        None,
        None,
        None,
        &PasswordUpdate::Unchanged,
    )
    .await
    .unwrap()
    .expect("owned row must update")
    .row;
    assert!(
        unchanged.chat_allowed_for_all,
        "omitting chat_allowed_for_all (None) must not change the stored value"
    );

    // …and Some(false) restores false.
    let back_to_false = db_meetings::update_meeting_settings(
        &pool,
        room_id,
        owner,
        None,
        None,
        None,
        None,
        None,
        Some(false),
        &PasswordUpdate::Unchanged,
    )
    .await
    .unwrap()
    .expect("owned row must update")
    .row;
    assert!(
        !back_to_false.chat_allowed_for_all,
        "update to Some(false) must persist false"
    );

    cleanup_test_data(&pool, room_id).await;
}

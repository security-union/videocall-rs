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

//! Integration tests for `GET /api/v1/meetings/feed`.
//!
//! This is the home-page meeting feed: a single deduplicated list of every
//! meeting the authenticated user owns OR has been admitted into, with a
//! server-computed `is_owner` flag on every row.
//!
//! Behaviour under test:
//!
//! 1. User owns a meeting and never joined it → appears with `is_owner = true`.
//! 2. User owns and joined → appears exactly once with `is_owner = true`.
//! 3. User joined someone else's meeting → appears with `is_owner = false`.
//! 4. User only ever waited (never admitted) → does NOT appear.
//! 5. **Two-identity regression:** SAME meeting_id appears with `is_owner=true`
//!    for the host and `is_owner=false` for the non-host attendee — the bug
//!    this PR fixes.
//! 6. Limit clamped at 200.
//! 7. Ordering by `last_active_at` descending.
//! 8. Folded counts (`participant_count` / `waiting_count`) match the legacy
//!    per-row helpers byte-for-byte.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use meeting_api::db::participants as db_participants;
use serial_test::serial;
use sqlx::PgPool;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::responses::{APIResponse, ListFeedResponse};

/// Lower bound for any Unix epoch timestamp emitted as **milliseconds**.
const MS_LOWER_BOUND: i64 = 1_000_000_000_000;

// ── Helpers ──────────────────────────────────────────────────────────────

/// Create a meeting owned by `host_email` with the waiting room disabled
/// (so non-host joiners are auto-admitted).
async fn create_meeting_wr_off(pool: &PgPool, host_email: &str, room_id: &str) {
    cleanup_test_data(pool, room_id).await;
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", host_email)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "waiting_room_enabled": false,
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "meeting creation must succeed for {room_id}"
    );
}

/// Create a meeting owned by `host_email` with the waiting room enabled.
async fn create_meeting_wr_on(pool: &PgPool, host_email: &str, room_id: &str) {
    cleanup_test_data(pool, room_id).await;
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", host_email)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "waiting_room_enabled": true,
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "meeting creation must succeed for {room_id}"
    );
}

/// `POST /api/v1/meetings/{room_id}/join` as `joiner_email`.
async fn join_meeting(pool: &PgPool, room_id: &str, joiner_email: &str) {
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        joiner_email,
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"Test User"}"#))
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "join must return 200 for {joiner_email}@{room_id}"
    );
}

/// `GET /api/v1/meetings/feed` as `caller_email`, returning the parsed body.
async fn list_feed(
    pool: &PgPool,
    caller_email: &str,
    limit: Option<i64>,
) -> APIResponse<ListFeedResponse> {
    let uri = match limit {
        Some(l) => format!("/api/v1/meetings/feed?limit={l}"),
        None => "/api/v1/meetings/feed".to_string(),
    };
    let app = build_app(pool.clone());
    let req = request_with_cookie("GET", &uri, caller_email)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "list feed must return 200");
    response_json(resp).await
}

/// Look up the internal `meetings.id` for a given `room_id`. Used to drive
/// the legacy `db_participants::count_*` helpers when comparing against the
/// folded counts in `list_feed_for_user`.
async fn lookup_meeting_pk(pool: &PgPool, room_id: &str) -> i32 {
    let (id,): (i32,) = sqlx::query_as("SELECT id FROM meetings WHERE room_id = $1")
        .bind(room_id)
        .fetch_one(pool)
        .await
        .expect("meeting row must exist for participant_count comparison");
    id
}

// ── Scenario 1: owned-but-never-joined appears with is_owner=true ────────

#[tokio::test]
#[serial]
async fn test_owned_never_joined_appears() {
    let pool = get_test_pool().await;
    let host = "feed-owned-never-joined@example.com";
    let room_id = "feed-test-owned-never-joined";

    // Host creates the meeting but never calls /join — purely an owned row.
    create_meeting_wr_off(&pool, host, room_id).await;

    let body = list_feed(&pool, host, None).await;
    assert!(body.success);

    let m = body
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("owned-never-joined meeting must appear in the feed");

    assert!(m.is_owner, "owner must be flagged is_owner = true");
    assert!(
        m.last_active_at >= MS_LOWER_BOUND,
        "last_active_at must be in milliseconds"
    );
    // The user has never been admitted to this meeting, so last_active_at
    // falls back to started_at (or, for a still-idle meeting, created_at).
    // In either case it must be >= created_at.
    assert!(
        m.last_active_at >= m.created_at,
        "last_active_at ({}) must be >= created_at ({})",
        m.last_active_at,
        m.created_at
    );
    // Newly-created meeting is still `idle`, so per the API contract
    // `started_at` is None until activation.
    assert_eq!(m.state, "idle");
    assert!(
        m.started_at.is_none(),
        "started_at must be None for a never-activated idle meeting"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 2: owned + joined appears exactly once ──────────────────────

#[tokio::test]
#[serial]
async fn test_owned_and_joined_appears_once() {
    let pool = get_test_pool().await;
    let host = "feed-owned-and-joined@example.com";
    let room_id = "feed-test-owned-and-joined";

    create_meeting_wr_off(&pool, host, room_id).await;
    // Host joins their own meeting — the LEFT JOIN LATERAL must not produce
    // a duplicate row from the participant match.
    join_meeting(&pool, room_id, host).await;

    let body = list_feed(&pool, host, None).await;
    assert!(body.success);

    let matches: Vec<_> = body
        .result
        .meetings
        .iter()
        .filter(|m| m.meeting_id == room_id)
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "owned-and-joined meeting must appear exactly once, got {} entries",
        matches.len()
    );
    let m = matches[0];
    assert!(m.is_owner, "host must be flagged as owner");
    // After /join the meeting transitions idle -> active and refreshes
    // started_at; last_active_at is the admission time, which lands at or
    // after started_at.
    assert_eq!(m.state, "active");
    assert!(
        m.started_at.is_some(),
        "active meeting must have started_at"
    );
    assert!(
        m.last_active_at >= m.started_at.unwrap(),
        "last_active_at ({}) must be >= started_at ({:?}) after admission",
        m.last_active_at,
        m.started_at
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 3: joined-only appears with is_owner=false ──────────────────

#[tokio::test]
#[serial]
async fn test_joined_only_appears_with_is_owner_false() {
    let pool = get_test_pool().await;
    let host = "feed-other-host@example.com";
    let attendee = "feed-other-attendee@example.com";
    let room_id = "feed-test-joined-only";

    create_meeting_wr_off(&pool, host, room_id).await;
    // Host joins so the meeting is active.
    join_meeting(&pool, room_id, host).await;
    // Attendee joins; WR=off so they are auto-admitted.
    join_meeting(&pool, room_id, attendee).await;

    let body = list_feed(&pool, attendee, None).await;
    assert!(body.success);
    let m = body
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("joined meeting must appear in the feed for the attendee");
    assert!(!m.is_owner, "non-owner attendee must have is_owner = false");
    assert_eq!(m.host.as_deref(), Some(host), "host must be visible");

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 4: waited-but-never-admitted is excluded ────────────────────

#[tokio::test]
#[serial]
async fn test_only_waited_does_not_appear() {
    let pool = get_test_pool().await;
    let host = "feed-waited-host@example.com";
    let waiter = "feed-waited-attendee@example.com";
    let room_id = "feed-test-waited-only";

    create_meeting_wr_on(&pool, host, room_id).await;
    // Waiter enters the waiting room but is never admitted — `admitted_at`
    // stays NULL, so the LEFT JOIN LATERAL `p.last_admit` is NULL and the
    // WHERE clause excludes the row (waiter is not the creator).
    join_meeting(&pool, room_id, waiter).await;

    let body = list_feed(&pool, waiter, None).await;
    assert!(body.success);
    assert!(
        body.result.meetings.iter().all(|m| m.meeting_id != room_id),
        "waiter who was never admitted must not see the meeting in their feed; got {:?}",
        body.result
            .meetings
            .iter()
            .map(|m| &m.meeting_id)
            .collect::<Vec<_>>()
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 5: THE regression test — same meeting, opposite is_owner ────

/// The user-reported bug: in two browser tabs logged into different
/// identities, the SAME meeting appears in BOTH home pages with the Owner
/// pill. This test pins down the fix end-to-end: the same meeting_id must
/// surface for both A (the host) and B (a participant), but with
/// `is_owner = true` for A and `is_owner = false` for B.
///
/// If this test fails, the home page is back to misrepresenting ownership
/// to non-owners.
#[tokio::test]
#[serial]
async fn test_two_identities_disjoint_is_owner_for_same_meeting() {
    let pool = get_test_pool().await;
    let user_a = "feed-two-id-a@example.com";
    let user_b = "feed-two-id-b@example.com";
    let room_id = "feed-test-two-identities";

    create_meeting_wr_off(&pool, user_a, room_id).await;
    join_meeting(&pool, room_id, user_a).await;
    join_meeting(&pool, room_id, user_b).await;

    // Hit /feed as A — must see is_owner = true on the meeting.
    let body_a = list_feed(&pool, user_a, None).await;
    assert!(body_a.success);
    let m_a = body_a
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("user A (host) must see the meeting in their feed");
    assert!(
        m_a.is_owner,
        "user A is the creator — is_owner must be true"
    );

    // Hit /feed as B — must see THE SAME meeting_id but is_owner = false.
    let body_b = list_feed(&pool, user_b, None).await;
    assert!(body_b.success);
    let m_b = body_b
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("user B (attendee) must see the meeting in their feed");
    assert!(
        !m_b.is_owner,
        "user B is NOT the creator — is_owner must be false (this is the bug fix)"
    );

    // Same meeting_id — pin this so a future refactor that returns user-
    // partitioned meeting ids (or different rows per user) is caught.
    assert_eq!(
        m_a.meeting_id, m_b.meeting_id,
        "the host and the attendee must see the SAME meeting_id"
    );
    // Both should agree on host (display field).
    assert_eq!(m_a.host.as_deref(), Some(user_a));
    assert_eq!(m_b.host.as_deref(), Some(user_a));

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 6: limit hard-capped at 200 ─────────────────────────────────

/// The route hard-caps `limit` at 200. We ask for 250 and expect the
/// response to contain at most 200 entries (the cap-enforcement check) AND
/// to contain exactly 200 of *this test's* rooms — filtered by the
/// `feed-test-limit-cap-` prefix so leftover rows from other test runs or
/// shared DB pools cannot mask a regression where the cap fails to engage.
#[tokio::test]
#[serial]
async fn test_limit_capped_at_200() {
    let pool = get_test_pool().await;
    let user = "feed-limit-cap@example.com";
    // Deterministic prefix so we can filter the response to just rows this
    // test inserted, regardless of leftover state in the DB.
    const PREFIX: &str = "feed-test-limit-cap-";

    // Insert 205 meetings so we are confidently above the cap. Smaller than
    // the spec's 250 to keep the test fast — 205 is enough to prove the cap
    // engages.
    for i in 0..205 {
        let room_id = format!("{PREFIX}{i:03}");
        create_meeting_wr_off(&pool, user, &room_id).await;
    }

    // Ask for more than the cap; the route must refuse to exceed 200.
    let body = list_feed(&pool, user, Some(250)).await;
    assert!(body.success);
    // Cap-enforcement check: total response length must never exceed 200.
    // If the cap is bypassed entirely (e.g. limit=250 honored), this would
    // be the assertion that catches it because the unfiltered count would
    // be 205 from this test's own rows alone.
    assert!(
        body.result.meetings.len() <= 200,
        "feed must respect the 200-row cap, got {} entries",
        body.result.meetings.len()
    );
    // Cap-precision check: filter to just this test's rooms so leftover
    // rows from other tests cannot disguise a regression that returns
    // fewer than 200 of our rows. We inserted 205 owned rooms, so exactly
    // 200 must come back after the cap engages.
    let our_rows = body
        .result
        .meetings
        .iter()
        .filter(|m| m.meeting_id.starts_with(PREFIX))
        .count();
    assert_eq!(
        our_rows, 200,
        "with 205 owned meetings the cap must engage at exactly 200 of our rows, got {our_rows}"
    );

    // Bulk delete is one round-trip vs 205 individual DELETEs.
    let _ = sqlx::query(
        "DELETE FROM meeting_participants WHERE meeting_id IN \
         (SELECT id FROM meetings WHERE room_id LIKE $1)",
    )
    .bind(format!("{PREFIX}%"))
    .execute(&pool)
    .await;
    let _ = sqlx::query("DELETE FROM meetings WHERE room_id LIKE $1")
        .bind(format!("{PREFIX}%"))
        .execute(&pool)
        .await;
}

// ── Scenario 7: ordering by last_active_at desc, m.id desc tiebreak ──────

#[tokio::test]
#[serial]
async fn test_ordering_by_last_active_at_desc() {
    let pool = get_test_pool().await;
    let user = "feed-ordering@example.com";
    let other = "feed-ordering-other@example.com";

    // Three meetings in a known temporal order, with sleeps to push
    // admitted_at apart on fast hardware.
    let owned_old = "feed-test-order-owned-old";
    let other_mid = "feed-test-order-other-mid";
    let owned_new = "feed-test-order-owned-new";

    create_meeting_wr_off(&pool, user, owned_old).await;
    join_meeting(&pool, owned_old, user).await;
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    create_meeting_wr_off(&pool, other, other_mid).await;
    join_meeting(&pool, other_mid, other).await;
    join_meeting(&pool, other_mid, user).await;
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    create_meeting_wr_off(&pool, user, owned_new).await;
    join_meeting(&pool, owned_new, user).await;

    let body = list_feed(&pool, user, None).await;
    assert!(body.success);

    // Filter to just the rooms this test created (other tests may have
    // leftover rows in the same DB — guard the assertion).
    let ids: Vec<&str> = body
        .result
        .meetings
        .iter()
        .map(|m| m.meeting_id.as_str())
        .filter(|id| *id == owned_old || *id == other_mid || *id == owned_new)
        .collect();

    assert_eq!(
        ids,
        vec![owned_new, other_mid, owned_old],
        "feed must be ordered by last_active_at DESC"
    );

    // last_active_at is non-increasing across the entire response.
    let timestamps: Vec<i64> = body
        .result
        .meetings
        .iter()
        .map(|m| m.last_active_at)
        .collect();
    assert!(
        timestamps.windows(2).all(|w| w[0] >= w[1]),
        "last_active_at must be non-increasing across the full feed, got {timestamps:?}"
    );

    cleanup_test_data(&pool, owned_old).await;
    cleanup_test_data(&pool, other_mid).await;
    cleanup_test_data(&pool, owned_new).await;
}

// ── Scenario 8: folded counts match legacy per-row helpers ───────────────

/// The new query folds participant_count / waiting_count into the same
/// SELECT (LEFT JOIN LATERAL) to eliminate the N+1 the legacy handlers
/// suffer from. This test pins the values to be byte-for-byte identical
/// to the legacy `db_participants::count_admitted` / `count_waiting`
/// helpers — so a future change to either path can't silently drift.
#[tokio::test]
#[serial]
async fn test_participant_count_matches_legacy() {
    let pool = get_test_pool().await;
    let host = "feed-counts-host@example.com";
    let admitted_user = "feed-counts-admitted@example.com";
    let waiting_user = "feed-counts-waiting@example.com";
    let room_id = "feed-test-counts-match";

    // WR enabled so we can have an admitted user (host) and a waiting user
    // sharing one meeting, exercising both lateral subqueries.
    create_meeting_wr_on(&pool, host, room_id).await;
    join_meeting(&pool, room_id, host).await;
    // The "admitted" user needs the host to admit them. Easier: use the
    // WR-off path on a separate meeting then collapse — but for a single-
    // meeting comparison flip WR off after host joined (so the host stays
    // admitted) and a second user auto-admits.
    let app = build_app(pool.clone());
    let req = request_with_cookie("PATCH", &format!("/api/v1/meetings/{room_id}"), host)
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"waiting_room_enabled":false}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "WR toggle must succeed");

    join_meeting(&pool, room_id, admitted_user).await;

    // Re-enable WR so the next joiner ends up in waiting.
    let app = build_app(pool.clone());
    let req = request_with_cookie("PATCH", &format!("/api/v1/meetings/{room_id}"), host)
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"waiting_room_enabled":true}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "WR toggle must succeed");

    join_meeting(&pool, room_id, waiting_user).await;

    // Compare folded counts (from /feed) against the legacy helpers.
    let body = list_feed(&pool, host, None).await;
    assert!(body.success);
    let m = body
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("meeting must appear in the feed");

    let pk = lookup_meeting_pk(&pool, room_id).await;
    let legacy_admitted = db_participants::count_admitted(&pool, pk).await.unwrap();
    let legacy_waiting = db_participants::count_waiting(&pool, pk).await.unwrap();

    assert_eq!(
        m.participant_count, legacy_admitted,
        "folded participant_count ({}) must match legacy count_admitted ({})",
        m.participant_count, legacy_admitted
    );
    assert_eq!(
        m.waiting_count, legacy_waiting,
        "folded waiting_count ({}) must match legacy count_waiting ({})",
        m.waiting_count, legacy_waiting
    );
    // Sanity floor — we set up 2 admitted (host + admitted_user) + 1 waiting.
    assert_eq!(m.participant_count, 2);
    assert_eq!(m.waiting_count, 1);

    cleanup_test_data(&pool, room_id).await;
}

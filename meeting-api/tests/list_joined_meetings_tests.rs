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

//! Integration tests for `GET /api/v1/meetings/joined`.
//!
//! Covers the "previously joined" endpoint that powers the home page's
//! Previously Joined section. Behaviour under test:
//!
//! 1. Authenticated user with no joined meetings → empty list, `total = 0`.
//! 2. Authenticated user who owns a meeting + has joined it → `is_owner = true`.
//! 3. Authenticated user who joined someone else's meeting → `is_owner = false`.
//! 4. Mix of owned + non-owned joined meetings → ordering by most-recent-join desc.
//! 5. `limit=2` with 5 joined meetings → returns the 2 most recent.
//! 6. Unauthenticated request → 401 / `UNAUTHORIZED` envelope.
//! 7. User who only ever waited (never admitted) → does NOT appear.
//! 8. Meeting joined then ended → still appears with `state = "ended"`.
//! 9. Negative `limit` → 400 / `INVALID_INPUT` envelope.

mod test_helpers;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use meeting_api::{routes, state::AppState};
use serial_test::serial;
use sqlx::PgPool;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{
    responses::{APIResponse, ListJoinedMeetingsResponse},
    APIError,
};

/// Lower bound for any Unix epoch timestamp emitted as **milliseconds**.
///
/// 1_000_000_000_000 ms = 2001-09-09. A timestamp at or above this floor is
/// conclusively in milliseconds; one below it (but above 0) is almost
/// certainly seconds — the regression these checks guard against.
const MS_LOWER_BOUND: i64 = 1_000_000_000_000;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build an Axum router with `allow_anonymous = false` so missing credentials
/// resolve to 401 instead of falling through to the synthetic anonymous user.
/// Required for the unauthenticated scenario; the default [`build_app`] helper
/// has anonymous auth enabled to make other suites simpler.
fn build_app_no_anonymous(pool: PgPool) -> Router {
    let state = AppState {
        db: pool,
        jwt_secret: TEST_JWT_SECRET.to_string(),
        token_ttl_secs: 600,
        session_ttl_secs: 3600,
        oauth: None,
        jwks_cache: None,
        cookie_domain: None,
        cookie_name: "session".to_string(),
        cookie_secure: false,
        nats: None,
        service_version_urls: Vec::new(),
        http_client: reqwest::Client::new(),
        display_name_rate_limiter: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        display_name_rate_limiter_ops: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        search: None,
        allow_anonymous: false,
        dev_user: None,
    };
    routes::router().with_state(state)
}

/// Create a meeting owned by `host_email` with the waiting room disabled
/// (so non-host joiners are auto-admitted, matching what tests typically want).
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
/// Asserts a 200 OK; the resulting status (admitted vs waiting) is left to
/// the caller to interpret from the body.
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

/// `POST /api/v1/meetings/{room_id}/end` as `host_email`.
async fn end_meeting(pool: &PgPool, room_id: &str, host_email: &str) {
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/end"),
        host_email,
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "ending {room_id} as {host_email} must succeed"
    );
}

/// `GET /api/v1/meetings/joined` as `caller_email`, returning the parsed body.
async fn list_joined(
    pool: &PgPool,
    caller_email: &str,
    limit: Option<i64>,
) -> APIResponse<ListJoinedMeetingsResponse> {
    let uri = match limit {
        Some(l) => format!("/api/v1/meetings/joined?limit={l}"),
        None => "/api/v1/meetings/joined".to_string(),
    };
    let app = build_app(pool.clone());
    let req = request_with_cookie("GET", &uri, caller_email)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "list joined must return 200");
    response_json(resp).await
}

// ── Scenario 1: empty list ────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_list_joined_empty_when_user_has_never_joined() {
    let pool = get_test_pool().await;
    // Use a unique email so any leftover rows from other suites don't pollute.
    let user = "joined-empty@example.com";

    let body = list_joined(&pool, user, None).await;
    assert!(body.success, "list call must succeed");
    assert!(
        body.result.meetings.is_empty(),
        "user with no joined meetings must get an empty list, got {} entries",
        body.result.meetings.len()
    );
    assert_eq!(body.result.total, 0, "empty list must report total=0");
}

// ── Scenario 2: owned meeting that the user joined ───────────────────────────

#[tokio::test]
#[serial]
async fn test_list_joined_includes_owned_meeting_with_is_owner_true() {
    let pool = get_test_pool().await;
    let host = "joined-owner@example.com";
    let room_id = "joined-test-owner-room";

    create_meeting_wr_off(&pool, host, room_id).await;
    // Host joins their own meeting (which sets admitted_at via upsert_host).
    join_meeting(&pool, room_id, host).await;

    let body = list_joined(&pool, host, None).await;
    assert!(body.success);
    assert_eq!(body.result.meetings.len(), 1, "expected exactly 1 entry");
    assert_eq!(body.result.total, 1);
    let m = &body.result.meetings[0];
    assert_eq!(m.meeting_id, room_id);
    assert!(m.is_owner, "host must be flagged as owner");
    // All `JoinedMeetingSummary` timestamps are documented as Unix epoch
    // milliseconds. A loose `> 0` check would silently accept a regression to
    // seconds; the ms floor (~2001-09-09) catches that immediately.
    assert!(
        m.last_joined_at >= MS_LOWER_BOUND,
        "last_joined_at must be in milliseconds (>= {MS_LOWER_BOUND}), got {}",
        m.last_joined_at
    );
    assert!(
        m.started_at >= MS_LOWER_BOUND,
        "started_at must be in milliseconds (>= {MS_LOWER_BOUND}), got {}",
        m.started_at
    );
    assert!(
        m.created_at >= MS_LOWER_BOUND,
        "created_at must be in milliseconds (>= {MS_LOWER_BOUND}), got {}",
        m.created_at
    );
    // `created_at` is immutable; `started_at` is set on creation and refreshed
    // on every idle/ended -> active transition. So `created_at <= started_at`
    // must always hold for any meeting that has been joined (joining drives
    // activation, which refreshes `started_at` to >= the creation time).
    assert!(
        m.created_at <= m.started_at,
        "created_at ({}) must be <= started_at ({}) — created precedes any activation",
        m.created_at,
        m.started_at
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 3: joined someone else's meeting ────────────────────────────────

#[tokio::test]
#[serial]
async fn test_list_joined_includes_non_owned_meeting_with_is_owner_false() {
    let pool = get_test_pool().await;
    let host = "joined-other-host@example.com";
    let attendee = "joined-other-attendee@example.com";
    let room_id = "joined-test-other-room";

    create_meeting_wr_off(&pool, host, room_id).await;
    // Host joins so meeting becomes active.
    join_meeting(&pool, room_id, host).await;
    // Attendee joins; WR=off so they're auto-admitted.
    join_meeting(&pool, room_id, attendee).await;

    let body = list_joined(&pool, attendee, None).await;
    assert!(body.success);
    assert_eq!(body.result.meetings.len(), 1);
    let m = &body.result.meetings[0];
    assert_eq!(m.meeting_id, room_id);
    assert!(!m.is_owner, "non-owner attendee must have is_owner = false");

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 4: mix of owned + non-owned, ordered by most-recent-join ────────

#[tokio::test]
#[serial]
async fn test_list_joined_orders_by_most_recent_admission_descending() {
    let pool = get_test_pool().await;
    let user = "joined-order@example.com";
    let other_host = "joined-order-other@example.com";

    let owned_old = "joined-test-order-owned-old";
    let other_mid = "joined-test-order-other-mid";
    let owned_new = "joined-test-order-owned-new";

    // Each `user` admission must land in a distinct Postgres timestamp so the
    // ORDER BY is unambiguous. tokio::time::sleep between joins inserts a few
    // milliseconds of wall-clock advancement — well above the microsecond
    // resolution of timestamptz NOW() — guarding against same-tick timestamps
    // on very fast hardware.

    create_meeting_wr_off(&pool, user, owned_old).await;
    join_meeting(&pool, owned_old, user).await;
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    create_meeting_wr_off(&pool, other_host, other_mid).await;
    join_meeting(&pool, other_mid, other_host).await;
    join_meeting(&pool, other_mid, user).await;
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    create_meeting_wr_off(&pool, user, owned_new).await;
    join_meeting(&pool, owned_new, user).await;

    let body = list_joined(&pool, user, None).await;
    assert!(body.success);
    assert_eq!(body.result.meetings.len(), 3);
    let ids: Vec<&str> = body
        .result
        .meetings
        .iter()
        .map(|m| m.meeting_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec![owned_new, other_mid, owned_old],
        "meetings must be ordered by most-recent admission descending"
    );
    // Spot-check is_owner: owned_new + owned_old are owned, other_mid is not.
    assert!(body.result.meetings[0].is_owner);
    assert!(!body.result.meetings[1].is_owner);
    assert!(body.result.meetings[2].is_owner);
    // last_joined_at must be monotonically non-increasing.
    let timestamps: Vec<i64> = body
        .result
        .meetings
        .iter()
        .map(|m| m.last_joined_at)
        .collect();
    assert!(
        timestamps.windows(2).all(|w| w[0] >= w[1]),
        "last_joined_at must be non-increasing, got {timestamps:?}"
    );

    cleanup_test_data(&pool, owned_old).await;
    cleanup_test_data(&pool, other_mid).await;
    cleanup_test_data(&pool, owned_new).await;
}

// ── Scenario 5: limit caps result set ─────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_list_joined_respects_limit_returning_most_recent() {
    let pool = get_test_pool().await;
    let user = "joined-limit@example.com";

    let rooms = [
        "joined-test-limit-1",
        "joined-test-limit-2",
        "joined-test-limit-3",
        "joined-test-limit-4",
        "joined-test-limit-5",
    ];

    for room in &rooms {
        create_meeting_wr_off(&pool, user, room).await;
        join_meeting(&pool, room, user).await;
        // Small spacer so the admitted_at timestamps are strictly ordered
        // even on very fast hardware where successive NOW() calls could land
        // in the same microsecond.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let body = list_joined(&pool, user, Some(2)).await;
    assert!(body.success);
    assert_eq!(
        body.result.meetings.len(),
        2,
        "limit=2 must cap the result list to 2 entries"
    );
    assert_eq!(body.result.total, 2, "total reflects returned rows");
    // The two most recent joins are the last two in the iteration order.
    let returned_ids: Vec<&str> = body
        .result
        .meetings
        .iter()
        .map(|m| m.meeting_id.as_str())
        .collect();
    assert_eq!(returned_ids, vec![rooms[4], rooms[3]]);

    for room in &rooms {
        cleanup_test_data(&pool, room).await;
    }
}

// ── Scenario 6: unauthenticated → 401 ────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_list_joined_unauthenticated_returns_401() {
    let pool = get_test_pool().await;
    // Use a router with anonymous auth disabled — the default test app allows
    // anonymous fallback, which would otherwise satisfy the request.
    let app = build_app_no_anonymous(pool.clone());

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/meetings/joined")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(
        body.result.code, "UNAUTHORIZED",
        "error envelope must use the canonical UNAUTHORIZED code"
    );
}

// ── Scenario 7: waited-but-never-admitted is excluded ────────────────────────

#[tokio::test]
#[serial]
async fn test_list_joined_excludes_users_who_only_waited_and_were_never_admitted() {
    let pool = get_test_pool().await;
    let host = "joined-waitonly-host@example.com";
    let waiter = "joined-waitonly-attendee@example.com";
    let room_id = "joined-test-wait-only";

    create_meeting_wr_on(&pool, host, room_id).await;
    // Waiter joins but the host never admits them — they remain `waiting`
    // with `admitted_at IS NULL`, so the endpoint must not surface this row.
    join_meeting(&pool, room_id, waiter).await;

    let body = list_joined(&pool, waiter, None).await;
    assert!(body.success);
    assert!(
        body.result.meetings.is_empty(),
        "waited-but-never-admitted user must not see this meeting in joined list, \
         got {} entries: {:?}",
        body.result.meetings.len(),
        body.result
            .meetings
            .iter()
            .map(|m| &m.meeting_id)
            .collect::<Vec<_>>()
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 8: ended meeting still appears with state="ended" ───────────────

#[tokio::test]
#[serial]
async fn test_list_joined_includes_ended_meetings() {
    let pool = get_test_pool().await;
    let host = "joined-ended-host@example.com";
    let attendee = "joined-ended-attendee@example.com";
    let room_id = "joined-test-ended";

    create_meeting_wr_off(&pool, host, room_id).await;
    join_meeting(&pool, room_id, host).await;
    join_meeting(&pool, room_id, attendee).await;
    // Owner ends the meeting.
    end_meeting(&pool, room_id, host).await;

    // Both host and attendee must still see the ended meeting in their lists.
    for caller in [host, attendee] {
        let body = list_joined(&pool, caller, None).await;
        assert!(body.success, "list must succeed for {caller}");
        let m = body
            .result
            .meetings
            .iter()
            .find(|m| m.meeting_id == room_id)
            .unwrap_or_else(|| {
                panic!(
                    "ended meeting must still appear for {caller}, got {:?}",
                    body.result
                        .meetings
                        .iter()
                        .map(|m| &m.meeting_id)
                        .collect::<Vec<_>>()
                )
            });
        assert_eq!(m.state, "ended", "state must be 'ended'");
        assert!(
            m.ended_at.is_some(),
            "ended_at must be populated for an ended meeting"
        );
        let ended_at = m.ended_at.unwrap();
        assert!(
            ended_at >= MS_LOWER_BOUND,
            "ended_at must be in milliseconds (>= {MS_LOWER_BOUND}), got {ended_at}"
        );
        assert!(
            ended_at >= m.started_at,
            "ended_at ({ended_at}) must be >= started_at ({})",
            m.started_at
        );
        // `created_at` is set at INSERT time and never moves; it must precede
        // both `started_at` (which is refreshed on activation) and `ended_at`
        // (set when the host ends the meeting).
        assert!(
            m.created_at >= MS_LOWER_BOUND,
            "created_at must be in milliseconds (>= {MS_LOWER_BOUND}), got {}",
            m.created_at
        );
        assert!(
            m.created_at <= m.started_at,
            "created_at ({}) must be <= started_at ({})",
            m.created_at,
            m.started_at
        );
        assert!(
            m.created_at <= ended_at,
            "created_at ({}) must be <= ended_at ({ended_at})",
            m.created_at
        );
    }

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 8b: created_at is exposed and immutable across re-activation ────

/// `JoinedMeetingSummary.created_at` is a new field. Lock in:
/// 1. The field is present and emitted in **milliseconds** for every entry.
/// 2. `created_at <= started_at` for every entry.
/// 3. `created_at` is immutable across an end-and-rejoin cycle, even though
///    `started_at` is refreshed by the new `activate()` semantics.
#[tokio::test]
#[serial]
async fn test_list_joined_exposes_immutable_created_at_across_reactivation() {
    let pool = get_test_pool().await;
    let host = "joined-created-at-host@example.com";
    let room_id = "joined-test-created-at-immutable";

    create_meeting_wr_off(&pool, host, room_id).await;
    // Join → activates the meeting (refreshing started_at to NOW()).
    join_meeting(&pool, room_id, host).await;

    let body = list_joined(&pool, host, None).await;
    assert!(body.success);
    let m_before = body
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("meeting must appear in joined list after first join")
        .clone();
    assert!(
        m_before.created_at >= MS_LOWER_BOUND,
        "created_at must be in milliseconds (>= {MS_LOWER_BOUND}), got {}",
        m_before.created_at
    );
    assert!(
        m_before.created_at <= m_before.started_at,
        "created_at ({}) must be <= started_at ({}) on first activation",
        m_before.created_at,
        m_before.started_at
    );

    // End and re-activate via a second join. The activate() change refreshes
    // started_at on the ended -> active transition, but created_at must stay
    // pinned to the original INSERT time.
    end_meeting(&pool, room_id, host).await;
    // Wait long enough that NOW() advances past the original started_at, so a
    // refresh of started_at is visibly forward-moving.
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    join_meeting(&pool, room_id, host).await;

    let body = list_joined(&pool, host, None).await;
    assert!(body.success);
    let m_after = body
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("meeting must still appear in joined list after re-activation");

    assert_eq!(
        m_after.created_at, m_before.created_at,
        "created_at must be immutable across an end-and-rejoin cycle; \
         before={}, after={}",
        m_before.created_at, m_after.created_at
    );
    assert!(
        m_after.started_at > m_before.started_at,
        "started_at must advance forward on re-activation; \
         before={}, after={}",
        m_before.started_at,
        m_after.started_at
    );
    assert!(
        m_after.created_at <= m_after.started_at,
        "created_at ({}) must remain <= started_at ({}) post re-activation",
        m_after.created_at,
        m_after.started_at
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 9: negative limit → 400 INVALID_INPUT ───────────────────────────

#[tokio::test]
#[serial]
async fn test_list_joined_rejects_negative_limit() {
    let pool = get_test_pool().await;
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        "/api/v1/meetings/joined?limit=-1",
        "joined-bad-limit@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "INVALID_INPUT");
}

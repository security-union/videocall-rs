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

//! Server-side meeting-password enforcement on every join path (issue #1613).
//!
//! Before this suite existed, `meeting-api` hashed a meeting password at create
//! time and never read it back on any join — `has_password` was cosmetic. These
//! tests drive the real Axum router end to end and pin, per join path:
//! correct password accepted, wrong rejected, absent rejected, and a meeting
//! with no password entirely unaffected.
//!
//! They also pin the parts that are easy to get wrong and impossible to see
//! from the happy path: the waiting-room admit cannot launder an unverified
//! joiner, the status/re-join paths cannot mint a room token for one, and a
//! corrupt stored hash denies rather than opening the meeting.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use serial_test::serial;
use sqlx::PgPool;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{
    responses::{APIResponse, ParticipantStatusResponse},
    APIError,
};

const HOST: &str = "pw-host@example.com";
const ATTENDEE: &str = "pw-attendee@example.com";
const VICTIM: &str = "pw-victim@example.com";
const PASSWORD: &str = "correct horse battery staple";

/// Create a meeting owned by [`HOST`], optionally password-protected.
///
/// `waiting_room_enabled` is passed through because the two admission modes are
/// genuinely different code paths in `join_as_attendee` (auto-admit vs. queue),
/// and the password gate has to sit in front of both.
async fn create_meeting(
    pool: &PgPool,
    room_id: &str,
    password: Option<&str>,
    waiting_room_enabled: bool,
    allow_guests: bool,
) {
    cleanup_test_data(pool, room_id).await;

    let mut body = serde_json::json!({
        "meeting_id": room_id,
        "attendees": [],
        "waiting_room_enabled": waiting_room_enabled,
        "allow_guests": allow_guests,
    });
    if let Some(pw) = password {
        body["password"] = serde_json::Value::String(pw.to_string());
    }

    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", HOST)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "meeting fixture must be created"
    );
}

/// Host joins, which activates the meeting. The host is the meeting owner and
/// is exempt from the password gate, so this never carries one.
async fn host_join(pool: &PgPool, room_id: &str) -> StatusCode {
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), HOST)
        .body(Body::empty())
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

/// `POST /api/v1/meetings/{room_id}/join` as `user`, with an optional password.
/// `None` sends a body with no `password` key at all — the pre-#1613 wire shape.
async fn join(
    pool: &PgPool,
    room_id: &str,
    user: &str,
    password: Option<&str>,
) -> axum::response::Response {
    join_from(pool, room_id, user, password, None).await
}

/// As [`join`], but with an explicit `X-Forwarded-For` so the failed-attempt
/// throttle sees a stable client address.
///
/// `oneshot` never opens a socket, so `ConnectInfo` is absent and an
/// un-forwarded test request is deliberately un-throttled — which is exactly
/// what keeps every *other* test in this file independent of the throttle. Tests
/// that want the throttle engaged opt in by supplying an address here.
async fn join_from(
    pool: &PgPool,
    room_id: &str,
    user: &str,
    password: Option<&str>,
    client_ip: Option<&str>,
) -> axum::response::Response {
    join_on(
        &build_app(pool.clone()),
        room_id,
        user,
        password,
        client_ip,
        Some("Attendee"),
    )
    .await
}

/// As [`join_from`], but against an app the caller keeps across requests.
///
/// This distinction is load-bearing for the throttle tests. `build_app`
/// constructs a fresh `AppState` — and therefore a fresh `MeetingPasswordGate`
/// with an empty attempt map — so a test that calls it per request can never
/// observe a budget accumulating, and would report a green that means nothing.
/// Production has exactly one `AppState` for the process; cloning one `Router`
/// per request is what reproduces that, because the clone shares the same
/// `Arc<MeetingPasswordGate>`.
async fn join_on(
    app: &axum::Router,
    room_id: &str,
    user: &str,
    password: Option<&str>,
    client_ip: Option<&str>,
    display_name: Option<&str>,
) -> axum::response::Response {
    let mut body = serde_json::json!({});
    if let Some(dn) = display_name {
        body["display_name"] = serde_json::Value::String(dn.to_string());
    }
    if let Some(pw) = password {
        body["password"] = serde_json::Value::String(pw.to_string());
    }
    let mut req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), user)
        .header("Content-Type", "application/json");
    if let Some(ip) = client_ip {
        req = req.header("X-Forwarded-For", ip);
    }
    let req = req
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

/// `POST /api/v1/meetings/{room_id}/join-guest` with an optional password.
async fn join_guest(
    pool: &PgPool,
    room_id: &str,
    password: Option<&str>,
) -> axum::response::Response {
    let mut body = serde_json::json!({ "display_name": "Guesty" });
    if let Some(pw) = password {
        body["password"] = serde_json::Value::String(pw.to_string());
    }
    let app = build_app(pool.clone());
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/meetings/{room_id}/join-guest"))
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();
    app.oneshot(req).await.unwrap()
}

/// Assert a response is a 403 carrying the given machine-readable error code.
///
/// The status is asserted **before** the body is deserialized, deliberately: on
/// a fail-open regression the body is a successful `ParticipantStatusResponse`,
/// and deserializing first turns a clear "expected 403, got 200" into an opaque
/// `Error("missing field 'code'")` from serde.
async fn assert_denied(resp: axum::response::Response, expected_code: &str) {
    let status = resp.status();
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "expected 403 {expected_code}, got {status} — a non-403 here usually means \
         the gate let the request through"
    );
    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, expected_code);
}

/// Assert a response is a successful join and return the participant status.
async fn assert_joined(resp: axum::response::Response) -> ParticipantStatusResponse {
    let status = resp.status();
    assert_eq!(status, StatusCode::OK, "expected a successful join");
    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success);
    body.result
}

/// Number of participant rows for a meeting, whatever their status. A denied
/// join must not create one — not even a `waiting` row a host could later admit.
async fn participant_row_count(pool: &PgPool, room_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM meeting_participants p \
         JOIN meetings m ON m.id = p.meeting_id WHERE m.room_id = $1",
    )
    .bind(room_id)
    .fetch_one(pool)
    .await
    .expect("counting participant rows")
}

// ── Attendee join (POST /join, non-owner) ────────────────────────────────

#[tokio::test]
#[serial]
async fn attendee_join_with_correct_password_is_admitted() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-attendee-ok";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    let status = assert_joined(join(&pool, room_id, ATTENDEE, Some(PASSWORD)).await).await;
    assert_eq!(status.status, "admitted");
    assert!(
        status.room_token.is_some(),
        "a correct password must yield a room token"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn attendee_join_with_wrong_password_is_rejected_and_writes_nothing() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-attendee-wrong";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);
    let rows_before = participant_row_count(&pool, room_id).await;

    assert_denied(
        join(&pool, room_id, ATTENDEE, Some("wrong-password")).await,
        "INVALID_MEETING_PASSWORD",
    )
    .await;

    assert_eq!(
        participant_row_count(&pool, room_id).await,
        rows_before,
        "a rejected join must not insert a participant row"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn attendee_join_without_password_is_rejected() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-attendee-absent";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    // This is exactly the pre-#1613 request shape: a join body with only a
    // display name. It used to succeed; it must now be refused.
    assert_denied(
        join(&pool, room_id, ATTENDEE, None).await,
        "MEETING_PASSWORD_REQUIRED",
    )
    .await;

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn attendee_join_with_no_body_at_all_is_rejected() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-attendee-nobody";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    // `join_meeting` takes `Option<Json<JoinMeetingRequest>>`, so a bodyless
    // POST is a legal request that never deserialises a password field. It must
    // take the "required" branch, not a `None`-means-skip shortcut.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        ATTENDEE,
    )
    .body(Body::empty())
    .unwrap();
    assert_denied(app.oneshot(req).await.unwrap(), "MEETING_PASSWORD_REQUIRED").await;

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn attendee_join_unaffected_when_meeting_has_no_password() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-none-attendee";
    create_meeting(&pool, room_id, None, false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    // No password on the meeting: both the pre-#1613 shape (no field) and a
    // stray supplied password must sail through untouched.
    let status = assert_joined(join(&pool, room_id, ATTENDEE, None).await).await;
    assert_eq!(status.status, "admitted");

    let status = assert_joined(join(&pool, room_id, ATTENDEE, Some("irrelevant")).await).await;
    assert_eq!(status.status, "admitted");

    cleanup_test_data(&pool, room_id).await;
}

// ── Waiting-room-enabled variant of the same path ────────────────────────

#[tokio::test]
#[serial]
async fn waiting_room_join_is_gated_before_the_queue() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-wr-on";
    create_meeting(&pool, room_id, Some(PASSWORD), true, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);
    let rows_before = participant_row_count(&pool, room_id).await;

    // With a waiting room the joiner is queued rather than admitted, so a
    // missed check here would be invisible until the host clicked Admit. Assert
    // the wrong password never even reaches the queue.
    assert_denied(
        join(&pool, room_id, ATTENDEE, Some("nope")).await,
        "INVALID_MEETING_PASSWORD",
    )
    .await;
    assert_eq!(
        participant_row_count(&pool, room_id).await,
        rows_before,
        "a rejected join must not create a `waiting` row for the host to admit"
    );

    // The correct password queues them as normal.
    let status = assert_joined(join(&pool, room_id, ATTENDEE, Some(PASSWORD)).await).await;
    assert_eq!(status.status, "waiting");
    assert!(
        status.room_token.is_none(),
        "a queued attendee gets no room token yet"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// The admit path is the "same bug wearing a hat" check: a host must not be
/// able to admit somebody who never cleared the password gate. `admit` is an
/// `UPDATE ... WHERE status = 'waiting'`, so with no row there is nothing to
/// admit — this pins that property at the HTTP level.
#[tokio::test]
#[serial]
async fn host_cannot_admit_a_joiner_who_failed_the_password_gate() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-admit-bypass";
    create_meeting(&pool, room_id, Some(PASSWORD), true, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    assert_denied(
        join(&pool, room_id, ATTENDEE, Some("nope")).await,
        "INVALID_MEETING_PASSWORD",
    )
    .await;

    // Host explicitly tries to admit the rejected user by ID.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/admit"), HOST)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({ "user_id": ATTENDEE })).unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "there must be no waiting row to admit"
    );

    // And `admit-all` must not conjure one either.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit-all"),
        HOST,
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The rejected user has no participant row, so no status and no token.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}/status"),
        ATTENDEE,
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a rejected joiner must not be able to poll a room token out of /status"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Guest join (POST /join-guest) ────────────────────────────────────────

#[tokio::test]
#[serial]
async fn guest_join_with_correct_password_is_admitted() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-guest-ok";
    create_meeting(&pool, room_id, Some(PASSWORD), false, true).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    let status = assert_joined(join_guest(&pool, room_id, Some(PASSWORD)).await).await;
    assert_eq!(status.status, "admitted");
    assert!(status.is_guest);
    assert!(status.room_token.is_some());

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn guest_join_with_wrong_password_is_rejected() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-guest-wrong";
    create_meeting(&pool, room_id, Some(PASSWORD), false, true).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);
    let rows_before = participant_row_count(&pool, room_id).await;

    assert_denied(
        join_guest(&pool, room_id, Some("nope")).await,
        "INVALID_MEETING_PASSWORD",
    )
    .await;
    assert_eq!(
        participant_row_count(&pool, room_id).await,
        rows_before,
        "a rejected guest must not get a `guest:{{uuid}}` participant row"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn guest_join_without_password_is_rejected() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-guest-absent";
    create_meeting(&pool, room_id, Some(PASSWORD), false, true).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    assert_denied(
        join_guest(&pool, room_id, None).await,
        "MEETING_PASSWORD_REQUIRED",
    )
    .await;

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn guest_join_unaffected_when_meeting_has_no_password() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-none-guest";
    create_meeting(&pool, room_id, None, false, true).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    let status = assert_joined(join_guest(&pool, room_id, None).await).await;
    assert_eq!(status.status, "admitted");
    assert!(status.is_guest);

    cleanup_test_data(&pool, room_id).await;
}

/// `allow_guests` is evaluated before the password, so a guest-disabled meeting
/// answers `GUESTS_NOT_ALLOWED` whether or not they guessed the password. This
/// pins that the new gate did not reorder the existing anti-enumeration
/// behaviour of the guest endpoint.
#[tokio::test]
#[serial]
async fn guests_not_allowed_still_wins_over_the_password_gate() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-guest-disabled";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    assert_denied(
        join_guest(&pool, room_id, Some(PASSWORD)).await,
        "GUESTS_NOT_ALLOWED",
    )
    .await;
    assert_denied(join_guest(&pool, room_id, None).await, "GUESTS_NOT_ALLOWED").await;

    cleanup_test_data(&pool, room_id).await;
}

// ── Owner join (POST /join, creator) ─────────────────────────────────────

/// The owner exemption, stated as a test so it is a decision on the record
/// rather than an oversight. `creator_id` already grants strictly more
/// authority over the meeting than the password does (PATCH settings, end,
/// delete), so the owner joins their own password-protected meeting without
/// supplying one — including with a *wrong* one, proving the exemption keys off
/// identity and not off a lucky guess.
#[tokio::test]
#[serial]
async fn owner_joins_their_own_password_protected_meeting_without_the_password() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-owner-exempt";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), HOST)
        .body(Body::empty())
        .unwrap();
    let status = assert_joined(app.oneshot(req).await.unwrap()).await;
    assert!(status.is_host);
    assert_eq!(status.status, "admitted");
    assert!(status.room_token.is_some());

    // …and a wrong password does not lock the owner out of their own meeting.
    let status = assert_joined(join(&pool, room_id, HOST, Some("not-the-password")).await).await;
    assert!(status.is_host);

    cleanup_test_data(&pool, room_id).await;
}

// ── Lifecycle: leave / re-join, and an idle meeting ──────────────────────

/// Re-join after an explicit leave must be re-verified. A password that is
/// checked once and then implied by an existing participant row would let a
/// user who learned the password, joined, and later had it changed keep coming
/// back — and, more immediately, would mean the gate depends on row state
/// rather than on the credential.
#[tokio::test]
#[serial]
async fn rejoin_after_leaving_is_re_verified() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-rejoin";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    assert_joined(join(&pool, room_id, ATTENDEE, Some(PASSWORD)).await).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/leave"),
        ATTENDEE,
    )
    .body(Body::empty())
    .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);

    // Host re-activates the meeting after the attendee's leave emptied it.
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    assert_denied(
        join(&pool, room_id, ATTENDEE, None).await,
        "MEETING_PASSWORD_REQUIRED",
    )
    .await;
    assert_joined(join(&pool, room_id, ATTENDEE, Some(PASSWORD)).await).await;

    cleanup_test_data(&pool, room_id).await;
}

/// An `idle` meeting (created, host has not started it) takes the
/// `waiting_for_meeting` early-return inside `join_as_attendee`, which hands
/// back an **observer token**. That is a real credential — it authenticates
/// `GET /guest-status` — so the password must be verified before it is minted,
/// not merely before the room token is.
#[tokio::test]
#[serial]
async fn idle_meeting_does_not_hand_an_observer_token_to_a_wrong_password() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-idle";
    create_meeting(&pool, room_id, Some(PASSWORD), true, false).await;
    // Deliberately no host_join: the meeting stays `idle`.

    assert_denied(
        join(&pool, room_id, ATTENDEE, Some("nope")).await,
        "INVALID_MEETING_PASSWORD",
    )
    .await;
    assert_denied(
        join(&pool, room_id, ATTENDEE, None).await,
        "MEETING_PASSWORD_REQUIRED",
    )
    .await;

    let status = assert_joined(join(&pool, room_id, ATTENDEE, Some(PASSWORD)).await).await;
    assert_eq!(status.status, "waiting_for_meeting");
    assert!(
        status.observer_token.is_some(),
        "the correct password still gets the observer token this path exists to issue"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Fail closed ──────────────────────────────────────────────────────────

/// A `password_hash` that cannot be parsed must deny, never fall through to
/// "this meeting has no password". Written straight into the column because
/// there is no API that can produce a corrupt hash — the point is what happens
/// when the data is bad anyway (partial write, restore from a foreign schema,
/// a future algorithm migration gone wrong).
#[tokio::test]
#[serial]
async fn corrupt_stored_hash_denies_every_non_owner_join() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-corrupt";
    create_meeting(&pool, room_id, Some(PASSWORD), false, true).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    sqlx::query("UPDATE meetings SET password_hash = $1 WHERE room_id = $2")
        .bind("not-a-phc-string")
        .bind(room_id)
        .execute(&pool)
        .await
        .expect("corrupting the stored hash");

    // The password that was correct a moment ago is now unverifiable → denied.
    assert_denied(
        join(&pool, room_id, ATTENDEE, Some(PASSWORD)).await,
        "INVALID_MEETING_PASSWORD",
    )
    .await;
    assert_denied(
        join_guest(&pool, room_id, Some(PASSWORD)).await,
        "INVALID_MEETING_PASSWORD",
    )
    .await;
    assert_denied(
        join(&pool, room_id, ATTENDEE, None).await,
        "MEETING_PASSWORD_REQUIRED",
    )
    .await;

    // The owner is still exempt, which is what keeps a corrupt row recoverable
    // instead of bricking the meeting for everyone including its creator.
    assert_joined(join(&pool, room_id, HOST, None).await).await;

    cleanup_test_data(&pool, room_id).await;
}

// ── Failed-attempt throttle over HTTP (issue #1613, perf review MUST FIX b) ──

/// End-to-end proof that the throttle is actually wired into the route, not
/// merely present in the gate: a client that burns its budget gets `429` from
/// `POST /join` itself.
#[tokio::test]
#[serial]
async fn repeated_wrong_passwords_from_one_client_are_throttled() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-throttle";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    // ONE app, cloned per request, so the gate's attempt map persists exactly
    // as it does in production. See `join_on`.
    let app = build_app(pool.clone());
    let attacker = Some("198.51.100.42");
    for _ in 0..5 {
        assert_denied(
            join_on(&app, room_id, ATTENDEE, Some("guess"), attacker, None).await,
            "INVALID_MEETING_PASSWORD",
        )
        .await;
    }

    let resp = join_on(&app, room_id, ATTENDEE, Some("guess"), attacker, None).await;
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the 6th wrong password from the same client must be throttled"
    );
    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(body.result.code, "TOO_MANY_PASSWORD_ATTEMPTS");

    cleanup_test_data(&pool, room_id).await;
}

/// The throttle must not become a way to lock a meeting for everyone else.
/// A different client address is unaffected by the attacker's exhausted budget,
/// and — critically — can still join with the correct password.
#[tokio::test]
#[serial]
async fn one_clients_lockout_does_not_deny_the_meeting_to_others() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-throttle-scope";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    let app = build_app(pool.clone());
    let attacker = Some("198.51.100.42");
    for _ in 0..6 {
        let _ = join_on(&app, room_id, ATTENDEE, Some("guess"), attacker, None).await;
    }
    // Attacker is now locked out.
    assert_eq!(
        join_on(&app, room_id, ATTENDEE, Some("guess"), attacker, None)
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );

    // A legitimate attendee behind a different address still gets in.
    let victim = Some("203.0.113.5");
    let status =
        assert_joined(join_on(&app, room_id, VICTIM, Some(PASSWORD), victim, None).await).await;
    assert_eq!(status.status, "admitted");

    cleanup_test_data(&pool, room_id).await;
}

/// Forging the left-hand entries of `X-Forwarded-For` must not reset the
/// budget: the ingress appends the real address last, so only the rightmost
/// entry keys the throttle.
#[tokio::test]
#[serial]
async fn rotating_forged_forwarded_for_entries_does_not_reset_the_budget() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-throttle-forge";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    // Same real client (rightmost), different forged prefix each time.
    let forged = [
        "1.1.1.1, 198.51.100.77",
        "2.2.2.2, 198.51.100.77",
        "3.3.3.3, 198.51.100.77",
        "4.4.4.4, 198.51.100.77",
        "5.5.5.5, 198.51.100.77",
    ];
    let app = build_app(pool.clone());
    for xff in forged {
        assert_denied(
            join_on(&app, room_id, ATTENDEE, Some("guess"), Some(xff), None).await,
            "INVALID_MEETING_PASSWORD",
        )
        .await;
    }

    assert_eq!(
        join_on(
            &app,
            room_id,
            ATTENDEE,
            Some("guess"),
            Some("6.6.6.6, 198.51.100.77"),
            None
        )
        .await
        .status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a forged XFF prefix must not buy a fresh budget"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// `join_meeting` runs the pre-existing **display-name** limiter
/// (`enforce_display_name_rate_limit`, keyed on `user_id`, also 5 per 60 s)
/// before the password gate, and only when the request carries a
/// `display_name`. So a real UI client — which always sends one — hits that
/// limiter first and sees `RATE_LIMIT_EXCEEDED` rather than
/// `TOO_MANY_PASSWORD_ATTEMPTS`.
///
/// Both are 429 and both stop the request before Argon2 runs, so the CPU bound
/// holds either way; only the error code differs. This test exists because the
/// interaction is genuinely surprising — it silently made an earlier version of
/// the throttle tests measure the wrong limiter — and because
/// `POST /join-guest`, the unauthenticated path that actually matters for abuse,
/// does **not** call the display-name limiter at all and so is governed purely
/// by the password throttle.
#[tokio::test]
#[serial]
async fn display_name_limiter_fires_first_for_clients_that_send_one() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-limiter-order";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    let app = build_app(pool.clone());
    let client = Some("198.51.100.55");
    for _ in 0..5 {
        assert_denied(
            join_on(
                &app,
                room_id,
                ATTENDEE,
                Some("guess"),
                client,
                Some("Attendee"),
            )
            .await,
            "INVALID_MEETING_PASSWORD",
        )
        .await;
    }

    let resp = join_on(
        &app,
        room_id,
        ATTENDEE,
        Some("guess"),
        client,
        Some("Attendee"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(
        body.result.code, "RATE_LIMIT_EXCEEDED",
        "with a display_name present the rename limiter is reached first; if this \
         ever becomes TOO_MANY_PASSWORD_ATTEMPTS the handler ordering changed"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// A meeting with no password must not be throttled at all — it never reaches
/// the verifier, so repeated joins are free and always succeed.
#[tokio::test]
#[serial]
async fn open_meetings_are_never_throttled() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-throttle-open";
    create_meeting(&pool, room_id, None, false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    let app = build_app(pool.clone());
    let client = Some("198.51.100.9");
    for _ in 0..12 {
        let status =
            assert_joined(join_on(&app, room_id, ATTENDEE, None, client, None).await).await;
        assert_eq!(status.status, "admitted");
    }

    cleanup_test_data(&pool, room_id).await;
}

// ── Wire contract ────────────────────────────────────────────────────────

/// `has_password` must keep reporting the truth after this change — the client
/// reads it to decide whether to prompt at all.
#[tokio::test]
#[serial]
async fn has_password_still_reflects_the_stored_hash() {
    let pool = get_test_pool().await;

    let protected = "test-pw-flag-yes";
    create_meeting(&pool, protected, Some(PASSWORD), false, false).await;
    let open = "test-pw-flag-no";
    create_meeting(&pool, open, None, false, false).await;

    for (room_id, expected) in [(protected, true), (open, false)] {
        let app = build_app(pool.clone());
        let req = request_with_cookie("GET", &format!("/api/v1/meetings/{room_id}"), HOST)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: APIResponse<videocall_meeting_types::responses::MeetingInfoResponse> =
            response_json(resp).await;
        assert_eq!(
            body.result.has_password, expected,
            "has_password for {room_id}"
        );
    }

    cleanup_test_data(&pool, protected).await;
    cleanup_test_data(&pool, open).await;
}

// ── Setting and clearing the password over PATCH (issue #2207) ───────────
//
// These assert against the #1613 gate itself, not against `has_password`: that
// flag is derived from the column, so a PATCH writing an unusable hash would
// flip it and still lock everyone out.

/// `PATCH /api/v1/meetings/{room_id}` as `user`, with an arbitrary body.
async fn patch_meeting(
    pool: &PgPool,
    room_id: &str,
    user: &str,
    body: serde_json::Value,
) -> axum::response::Response {
    let app = build_app(pool.clone());
    let req = request_with_cookie("PATCH", &format!("/api/v1/meetings/{room_id}"), user)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();
    app.oneshot(req).await.unwrap()
}

async fn assert_patched(
    resp: axum::response::Response,
) -> videocall_meeting_types::responses::MeetingInfoResponse {
    let status = resp.status();
    assert_eq!(status, StatusCode::OK, "expected the PATCH to succeed");
    let body: APIResponse<videocall_meeting_types::responses::MeetingInfoResponse> =
        response_json(resp).await;
    assert!(body.success);
    body.result
}

/// The stored hash, read straight from the column.
async fn stored_hash(pool: &PgPool, room_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT password_hash FROM meetings WHERE room_id = $1")
        .bind(room_id)
        .fetch_one(pool)
        .await
        .expect("reading password_hash")
}

#[tokio::test]
#[serial]
async fn patch_can_set_a_password_on_a_meeting_created_without_one() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-patch-set";
    create_meeting(&pool, room_id, None, false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    assert!(stored_hash(&pool, room_id).await.is_none());
    assert_joined(join(&pool, room_id, ATTENDEE, None).await).await;

    let info = assert_patched(
        patch_meeting(
            &pool,
            room_id,
            HOST,
            serde_json::json!({ "password": PASSWORD }),
        )
        .await,
    )
    .await;
    assert!(info.has_password, "the PATCH must report the new state");

    assert_denied(
        join(&pool, room_id, VICTIM, None).await,
        "MEETING_PASSWORD_REQUIRED",
    )
    .await;
    assert_joined(join(&pool, room_id, VICTIM, Some(PASSWORD)).await).await;

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn patch_can_remove_a_password() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-patch-clear";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    assert_denied(
        join(&pool, room_id, ATTENDEE, None).await,
        "MEETING_PASSWORD_REQUIRED",
    )
    .await;

    let info = assert_patched(
        patch_meeting(
            &pool,
            room_id,
            HOST,
            serde_json::json!({ "remove_password": true }),
        )
        .await,
    )
    .await;
    assert!(!info.has_password);
    assert!(
        stored_hash(&pool, room_id).await.is_none(),
        "clearing must write NULL, not an empty string the gate would fail closed on"
    );

    assert_joined(join(&pool, room_id, ATTENDEE, None).await).await;

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn patch_can_rotate_the_password_and_the_old_one_stops_working() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-patch-rotate";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    assert_patched(
        patch_meeting(
            &pool,
            room_id,
            HOST,
            serde_json::json!({ "password": "a different one" }),
        )
        .await,
    )
    .await;

    assert_denied(
        join(&pool, room_id, ATTENDEE, Some(PASSWORD)).await,
        "INVALID_MEETING_PASSWORD",
    )
    .await;
    assert_joined(join(&pool, room_id, ATTENDEE, Some("a different one")).await).await;

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn a_non_owner_cannot_set_or_remove_the_password() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-patch-not-owner";
    create_meeting(&pool, room_id, None, false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);

    let resp = patch_meeting(
        &pool,
        room_id,
        ATTENDEE,
        serde_json::json!({ "password": "attacker-chosen" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(body.result.code, "NOT_OWNER");
    assert!(
        stored_hash(&pool, room_id).await.is_none(),
        "a rejected PATCH must not have written a hash"
    );
    assert_joined(join(&pool, room_id, VICTIM, None).await).await;

    let protected = "test-pw-patch-not-owner-clear";
    create_meeting(&pool, protected, Some(PASSWORD), false, false).await;
    let resp = patch_meeting(
        &pool,
        protected,
        ATTENDEE,
        serde_json::json!({ "remove_password": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        stored_hash(&pool, protected).await.is_some(),
        "a non-owner must not be able to strip the password"
    );

    cleanup_test_data(&pool, room_id).await;
    cleanup_test_data(&pool, protected).await;
}

#[tokio::test]
#[serial]
async fn ambiguous_password_bodies_are_rejected_and_change_nothing() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-patch-ambiguous";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);
    let before = stored_hash(&pool, room_id).await.expect("a stored hash");

    for body in [
        serde_json::json!({ "password": "" }),
        serde_json::json!({ "password": "new", "remove_password": true }),
    ] {
        let resp = patch_meeting(&pool, room_id, HOST, body.clone()).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "expected a 400 for {body}"
        );
        assert_eq!(
            stored_hash(&pool, room_id).await.as_deref(),
            Some(before.as_str()),
            "a rejected PATCH must leave the stored hash untouched: {body}"
        );
    }

    assert_joined(join(&pool, room_id, ATTENDEE, Some(PASSWORD)).await).await;

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn a_toggle_only_patch_leaves_the_password_alone() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-patch-toggle-only";
    create_meeting(&pool, room_id, Some(PASSWORD), false, false).await;
    assert_eq!(host_join(&pool, room_id).await, StatusCode::OK);
    let before = stored_hash(&pool, room_id).await.expect("a stored hash");

    let info = assert_patched(
        patch_meeting(
            &pool,
            room_id,
            HOST,
            serde_json::json!({ "waiting_room_enabled": true, "allow_guests": true }),
        )
        .await,
    )
    .await;
    assert!(info.waiting_room_enabled, "the toggle must still apply");
    assert!(info.has_password, "the password must have survived");
    assert_eq!(
        stored_hash(&pool, room_id).await.as_deref(),
        Some(before.as_str()),
        "a toggle-only PATCH must not even re-hash the password"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn the_patch_response_never_echoes_the_password() {
    let pool = get_test_pool().await;
    let room_id = "test-pw-patch-no-echo";
    create_meeting(&pool, room_id, None, false, false).await;

    let resp = patch_meeting(
        &pool,
        room_id,
        HOST,
        serde_json::json!({ "password": PASSWORD }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let raw = String::from_utf8(
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("reading the response body")
            .to_vec(),
    )
    .expect("a utf-8 body");
    assert!(!raw.contains(PASSWORD), "the response echoed it: {raw}");
    assert!(
        !raw.contains("$argon2"),
        "the response leaked the stored hash: {raw}"
    );

    cleanup_test_data(&pool, room_id).await;
}

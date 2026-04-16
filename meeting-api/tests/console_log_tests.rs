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

//! Integration tests for the console log upload endpoint.
//!
//! Tests the POST /api/v1/meetings/{meeting_id}/console-logs handler, verifying:
//! - Feature gate (CONSOLE_LOG_UPLOAD_ENABLED env var)
//! - Auth + membership checks (participant row required, any status accepted)
//! - File I/O (log chunk written to disk in the expected directory structure)
//! - Error responses for non-participants and nonexistent meetings

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{responses::APIResponse, APIError};

use meeting_api::routes::console_logs::MAX_BODY_SIZE;

const HOST_EMAIL: &str = "host@example.com";
const GUEST_EMAIL: &str = "guest-test@videocall.rs";
const OUTSIDER_EMAIL: &str = "outsider@example.com";
/// Unique user_id for the quota test — avoids polluting the process-global
/// UPLOAD_QUOTAS counter used by other tests.
const QUOTA_GUEST_EMAIL: &str = "guest-quota@videocall.rs";
const SESSION_TS: &str = "1700000000000";
const LOG_BODY: &str = r#"{"ts":"2025-01-01T00:00:00Z","level":"log","msg":"test entry"}"#;

/// Set up env vars for the console log feature and create a temp directory for log storage.
/// Returns the path to the temp directory.
fn enable_console_logs() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("console-log-test-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).expect("create temp log dir");
    std::env::set_var("CONSOLE_LOG_UPLOAD_ENABLED", "true");
    std::env::set_var("CONSOLE_LOG_DIR", dir.to_str().unwrap());
    dir
}

/// Disable the console log feature gate and remove the temp directory.
fn disable_console_logs(dir: &std::path::Path) {
    std::env::remove_var("CONSOLE_LOG_UPLOAD_ENABLED");
    std::env::remove_var("CONSOLE_LOG_DIR");
    let _ = std::fs::remove_dir_all(dir);
}

/// Helper: create a meeting, have the host join (activates it).
async fn setup_active_meeting(pool: &sqlx::PgPool, room_id: &str) {
    cleanup_test_data(pool, room_id).await;

    // Create meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", HOST_EMAIL)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": []
            }))
            .unwrap(),
        ))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Host joins (activates).
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        HOST_EMAIL,
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"Host User"}"#))
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();
}

/// Helper: have a guest join the meeting (enters waiting room).
async fn guest_joins_waiting_room(pool: &sqlx::PgPool, room_id: &str, guest_email: &str) {
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        guest_email,
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"Guest User"}"#))
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();
}

/// Helper: host admits a guest from the waiting room.
async fn host_admits_guest(pool: &sqlx::PgPool, room_id: &str, guest_email: &str) {
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit"),
        HOST_EMAIL,
    )
    .header("Content-Type", "application/json")
    .body(Body::from(
        serde_json::to_string(&serde_json::json!({ "user_id": guest_email })).unwrap(),
    ))
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();
}

/// Build a console log upload request for the given meeting and user.
fn console_log_request(room_id: &str, user_email: &str) -> axum::http::request::Builder {
    request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/console-logs"),
        user_email,
    )
    .header("Content-Type", "text/plain")
    .header("X-User-Id", user_email)
    .header("X-Session-Timestamp", SESSION_TS)
}

// ── Test: admitted guest can upload console logs ────────────────────────────

#[tokio::test]
#[serial]
async fn test_guest_participant_can_upload_console_logs() {
    let pool = get_test_pool().await;
    let room_id = "test-clog-guest";
    let log_dir = enable_console_logs();

    setup_active_meeting(&pool, room_id).await;
    guest_joins_waiting_room(&pool, room_id, GUEST_EMAIL).await;
    host_admits_guest(&pool, room_id, GUEST_EMAIL).await;

    // Upload console logs as admitted guest.
    let app = build_app(pool.clone());
    let req = console_log_request(room_id, GUEST_EMAIL)
        .body(Body::from(LOG_BODY))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Admitted guest should be able to upload console logs"
    );

    // Verify a .log file was written under the meeting's directory.
    let meeting_dir = log_dir.join(room_id);
    assert!(
        meeting_dir.exists(),
        "Meeting log directory should be created at {:?}",
        meeting_dir
    );

    let mut found_log = false;
    for entry in walkdir(&meeting_dir) {
        if entry.extension().is_some_and(|ext| ext == "log") {
            let content = std::fs::read_to_string(&entry).expect("read log file");
            assert_eq!(
                content, LOG_BODY,
                "Log file content should match uploaded body"
            );
            // Verify filename contains the guest's user_id and session timestamp.
            let name = entry.file_name().unwrap().to_str().unwrap();
            assert!(
                name.contains("guest-test@videocall.rs"),
                "Filename should contain the user_id: {name}"
            );
            assert!(
                name.contains(SESSION_TS),
                "Filename should contain the session timestamp: {name}"
            );
            found_log = true;
        }
    }
    assert!(
        found_log,
        "At least one .log file should exist under the meeting directory"
    );

    cleanup_test_data(&pool, room_id).await;
    disable_console_logs(&log_dir);
}

// ── Test: non-participant gets 403 ─────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_non_participant_rejected_from_console_log_upload() {
    let pool = get_test_pool().await;
    let room_id = "test-clog-nonparticipant";
    let log_dir = enable_console_logs();

    setup_active_meeting(&pool, room_id).await;
    // outsider never joined the meeting — no participant row exists.

    let app = build_app(pool.clone());
    let req = console_log_request(room_id, OUTSIDER_EMAIL)
        .body(Body::from(LOG_BODY))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "Non-participant should be rejected with 403"
    );

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "FORBIDDEN");

    cleanup_test_data(&pool, room_id).await;
    disable_console_logs(&log_dir);
}

// ── Test: waiting-room participant CAN upload logs ─────────────────────────

#[tokio::test]
#[serial]
async fn test_waiting_participant_can_upload_console_logs() {
    let pool = get_test_pool().await;
    let room_id = "test-clog-waiting";
    let log_dir = enable_console_logs();

    setup_active_meeting(&pool, room_id).await;
    // Guest joins but is NOT admitted — stays in "waiting" status.
    guest_joins_waiting_room(&pool, room_id, GUEST_EMAIL).await;

    let app = build_app(pool.clone());
    let req = console_log_request(room_id, GUEST_EMAIL)
        .body(Body::from(LOG_BODY))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Waiting-room participant should be able to upload console logs — \
         get_status returns any existing row regardless of status"
    );

    // Verify a log file was written.
    let meeting_dir = log_dir.join(room_id);
    assert!(
        meeting_dir.exists(),
        "Meeting log directory should be created for waiting participant upload"
    );

    cleanup_test_data(&pool, room_id).await;
    disable_console_logs(&log_dir);
}

// ── Test: feature disabled returns 404 ─────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_console_log_upload_disabled_returns_404() {
    let pool = get_test_pool().await;
    let room_id = "test-clog-disabled";

    // Ensure the feature gate is OFF.
    std::env::remove_var("CONSOLE_LOG_UPLOAD_ENABLED");
    std::env::remove_var("CONSOLE_LOG_DIR");

    setup_active_meeting(&pool, room_id).await;
    guest_joins_waiting_room(&pool, room_id, GUEST_EMAIL).await;
    host_admits_guest(&pool, room_id, GUEST_EMAIL).await;

    let app = build_app(pool.clone());
    let req = console_log_request(room_id, GUEST_EMAIL)
        .body(Body::from(LOG_BODY))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "Console log upload should return 404 when feature is disabled"
    );

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "NOT_FOUND");

    cleanup_test_data(&pool, room_id).await;
}

// ── Test: nonexistent meeting returns 404 ──────────────────────────────────

#[tokio::test]
#[serial]
async fn test_nonexistent_meeting_returns_404() {
    let pool = get_test_pool().await;
    let room_id = "test-clog-no-meeting";
    let log_dir = enable_console_logs();

    // Do NOT create any meeting — room_id does not exist in the database.
    cleanup_test_data(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = console_log_request(room_id, GUEST_EMAIL)
        .body(Body::from(LOG_BODY))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "Upload to a nonexistent meeting should return 404"
    );

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "NOT_FOUND");

    cleanup_test_data(&pool, room_id).await;
    disable_console_logs(&log_dir);
}

// ── Test: JWT identity overrides X-User-Id header ────────────────────────────
// Security-critical: an attacker authenticated as outsider@example.com sends
// X-User-Id: guest-test@videocall.rs. The handler MUST use the JWT identity
// (outsider) for the membership check, not the header (guest), and reject
// because the outsider has no participant row.

#[tokio::test]
#[serial]
async fn test_jwt_identity_overrides_x_user_id_header() {
    let pool = get_test_pool().await;
    let room_id = "test-clog-identity";
    let log_dir = enable_console_logs();

    setup_active_meeting(&pool, room_id).await;
    guest_joins_waiting_room(&pool, room_id, GUEST_EMAIL).await;
    host_admits_guest(&pool, room_id, GUEST_EMAIL).await;

    // Outsider is authenticated (JWT sub = OUTSIDER_EMAIL) but claims to be
    // the guest via X-User-Id header.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/console-logs"),
        OUTSIDER_EMAIL, // JWT sub = outsider
    )
    .header("Content-Type", "text/plain")
    .header("X-User-Id", GUEST_EMAIL) // claims to be the guest
    .header("X-Session-Timestamp", SESSION_TS)
    .body(Body::from(LOG_BODY))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "Handler must use JWT identity (outsider), not X-User-Id header (guest)"
    );

    // Verify no file was written under the guest's user_id.
    let meeting_dir = log_dir.join(room_id);
    for entry in walkdir(&meeting_dir) {
        let name = entry.file_name().unwrap().to_str().unwrap();
        assert!(
            !name.contains("guest-test@videocall.rs"),
            "No file should be written under the victim's user_id: {name}"
        );
    }

    cleanup_test_data(&pool, room_id).await;
    disable_console_logs(&log_dir);
}

// ── Test: per-user daily upload quota ─────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_upload_quota_enforced() {
    let pool = get_test_pool().await;
    let room_id = "test-clog-quota";
    let log_dir = enable_console_logs();
    // Set quota small enough that a second upload of LOG_BODY (~62 bytes) exceeds it.
    std::env::set_var("CONSOLE_LOG_USER_QUOTA_BYTES", "70");

    setup_active_meeting(&pool, room_id).await;
    guest_joins_waiting_room(&pool, room_id, QUOTA_GUEST_EMAIL).await;
    host_admits_guest(&pool, room_id, QUOTA_GUEST_EMAIL).await;

    // First upload: ~62 bytes, quota 70 → OK.
    let app = build_app(pool.clone());
    let req = console_log_request(room_id, QUOTA_GUEST_EMAIL)
        .body(Body::from(LOG_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "First upload within quota");

    // Second upload: 62+62=124 > 70 → 429 Too Many Requests.
    let app = build_app(pool.clone());
    let req = console_log_request(room_id, QUOTA_GUEST_EMAIL)
        .body(Body::from(LOG_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "Second upload should exceed the 70-byte daily quota"
    );

    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(body.result.code, "RATE_LIMITED");

    std::env::remove_var("CONSOLE_LOG_USER_QUOTA_BYTES");
    cleanup_test_data(&pool, room_id).await;
    disable_console_logs(&log_dir);
}

// ── Test: oversized body rejected ─────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_oversized_body_rejected() {
    let pool = get_test_pool().await;
    let room_id = "test-clog-oversize";
    let log_dir = enable_console_logs();

    setup_active_meeting(&pool, room_id).await;
    guest_joins_waiting_room(&pool, room_id, GUEST_EMAIL).await;
    host_admits_guest(&pool, room_id, GUEST_EMAIL).await;

    // Send a body that exceeds MAX_BODY_SIZE (1 MB). The DefaultBodyLimit
    // layer on the route should reject it before the handler runs.
    let oversized = vec![b'x'; MAX_BODY_SIZE + 1];
    let app = build_app(pool.clone());
    let req = console_log_request(room_id, GUEST_EMAIL)
        .body(Body::from(oversized))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "Body exceeding MAX_BODY_SIZE should be rejected with 413"
    );

    cleanup_test_data(&pool, room_id).await;
    disable_console_logs(&log_dir);
}

// ── Test: path traversal in meeting_id rejected ──────────────────────────────

#[tokio::test]
#[serial]
async fn test_path_traversal_meeting_id_rejected() {
    let pool = get_test_pool().await;
    let log_dir = enable_console_logs();

    // meeting_id containing dots is rejected by SAFE_MEETING_ID_RE.
    // No meeting setup needed — validate_id fails before the DB lookup.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        "/api/v1/meetings/room..name/console-logs",
        GUEST_EMAIL,
    )
    .header("Content-Type", "text/plain")
    .header("X-User-Id", GUEST_EMAIL)
    .header("X-Session-Timestamp", SESSION_TS)
    .body(Body::from(LOG_BODY))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "meeting_id with dots should be rejected as path traversal"
    );

    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(body.result.code, "INVALID_PARAMETER");

    disable_console_logs(&log_dir);
}

// ── Test: non-numeric session timestamp rejected ─────────────────────────────

#[tokio::test]
#[serial]
async fn test_non_numeric_session_timestamp_rejected() {
    let pool = get_test_pool().await;
    let log_dir = enable_console_logs();

    // A non-numeric session timestamp is rejected before the DB lookup.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        "/api/v1/meetings/valid-room/console-logs",
        GUEST_EMAIL,
    )
    .header("Content-Type", "text/plain")
    .header("X-User-Id", GUEST_EMAIL)
    .header("X-Session-Timestamp", "not-a-number")
    .body(Body::from(LOG_BODY))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "Non-numeric session timestamp should be rejected"
    );

    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(body.result.code, "INVALID_PARAMETER");

    disable_console_logs(&log_dir);
}

// ── Utility: recursively collect all file paths under a directory ───────────

fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walkdir(&path));
            } else {
                files.push(path);
            }
        }
    }
    files
}

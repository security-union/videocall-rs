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

//! Integration tests for the `upsert_user` database function in `db::oauth`.
//!
//! These tests verify that the INSERT ... ON CONFLICT upsert behaves correctly:
//! new users get a fresh UUID, repeated inserts for the same email are idempotent,
//! and distinct emails produce distinct UUIDs.

mod test_helpers;

use meeting_api::db::oauth::upsert_user;
use serial_test::serial;
use test_helpers::get_test_pool;
use uuid::Uuid;

/// Delete test users by email so each test starts and ends with a clean slate.
async fn cleanup_test_users(pool: &sqlx::PgPool, emails: &[&str]) {
    for email in emails {
        let _ = sqlx::query("DELETE FROM users WHERE email = $1")
            .bind(*email)
            .execute(pool)
            .await;
    }
}

// ── First registration ─────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_first_registration_returns_valid_uuid() {
    let pool = get_test_pool().await;
    let email = "upsert-test-first-reg@integration-test.local";
    cleanup_test_users(&pool, &[email]).await;

    let result = upsert_user(
        &pool,
        email,
        "Test User",
        "access-tok-1",
        Some("refresh-tok-1"),
    )
    .await
    .expect("upsert_user should succeed for a new email");

    assert_ne!(
        result,
        Uuid::nil(),
        "returned UUID must not be the nil UUID"
    );

    // Basic structural check: UUID version 4 has the form xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx.
    // The database generates it, so just confirm it round-trips through Display/parse.
    let reparsed: Uuid = result
        .to_string()
        .parse()
        .expect("UUID should round-trip through Display/parse");
    assert_eq!(result, reparsed);

    cleanup_test_users(&pool, &[email]).await;
}

// ── Idempotency ─────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_same_email_returns_same_uuid_idempotent() {
    let pool = get_test_pool().await;
    let email = "upsert-test-idempotent@integration-test.local";
    cleanup_test_users(&pool, &[email]).await;

    let first_uuid = upsert_user(&pool, email, "First Call", "access-1", Some("refresh-1"))
        .await
        .expect("first upsert_user call should succeed");

    let second_uuid = upsert_user(&pool, email, "Updated Name", "access-2", Some("refresh-2"))
        .await
        .expect("second upsert_user call should succeed");

    assert_eq!(
        first_uuid, second_uuid,
        "upserting the same email twice must return the same UUID"
    );

    // Both should be non-nil.
    assert_ne!(first_uuid, Uuid::nil());

    cleanup_test_users(&pool, &[email]).await;
}

// ── Distinct emails ─────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_different_emails_return_different_uuids() {
    let pool = get_test_pool().await;
    let email_a = "upsert-test-diff-a@integration-test.local";
    let email_b = "upsert-test-diff-b@integration-test.local";
    cleanup_test_users(&pool, &[email_a, email_b]).await;

    let uuid_a = upsert_user(&pool, email_a, "User A", "tok-a", None)
        .await
        .expect("upsert_user for email_a should succeed");

    let uuid_b = upsert_user(&pool, email_b, "User B", "tok-b", None)
        .await
        .expect("upsert_user for email_b should succeed");

    assert_ne!(
        uuid_a, uuid_b,
        "different emails must produce different UUIDs"
    );

    // Both must be non-nil.
    assert_ne!(uuid_a, Uuid::nil());
    assert_ne!(uuid_b, Uuid::nil());

    cleanup_test_users(&pool, &[email_a, email_b]).await;
}

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

//! OAuth request and user storage queries.
//!
//! Shared across the PostgreSQL and SQLite backends. `$N` placeholders are
//! parsed natively by both sqlx drivers, so the query strings are identical;
//! the current-timestamp expression comes from [`crate::db::SQL_NOW`].

use chrono::{Duration, Utc};

use crate::db::{DbPool, OAuthRequestRow, SQL_NOW};

/// How long an IdP leg (`/login` → `/login/callback`) may take before its
/// stored PKCE/CSRF state is treated as abandoned.
pub const OAUTH_REQUEST_TTL_SECS: i64 = 600;

/// Store a new OAuth request (PKCE + CSRF state + optional nonce + the hash
/// of the browser binding) for later retrieval in the callback, and delete
/// abandoned ones so the table cannot grow without bound. The delete uses the
/// `created_at` index and only ever finds the rows that expired since the
/// previous login.
pub async fn store_oauth_request(
    pool: &DbPool,
    pkce_challenge: &str,
    pkce_verifier: &str,
    csrf_state: &str,
    return_to: Option<&str>,
    nonce: Option<&str>,
    browser_binding_hash: &str,
) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    sqlx::query(
        r#"
        INSERT INTO oauth_requests
            (pkce_challenge, pkce_verifier, csrf_state, return_to, nonce, created_at,
             browser_binding_hash)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(pkce_challenge)
    .bind(pkce_verifier)
    .bind(csrf_state)
    .bind(return_to)
    .bind(nonce)
    .bind(now)
    .bind(browser_binding_hash)
    .execute(pool)
    .await?;
    // Best effort: a failed cleanup must never fail the login itself.
    if let Err(e) = sqlx::query(
        "DELETE FROM oauth_requests \
         WHERE created_at < $1 OR created_at IS NULL OR csrf_state IS NULL",
    )
    .bind(now - Duration::seconds(OAUTH_REQUEST_TTL_SECS))
    .execute(pool)
    .await
    {
        tracing::warn!("deleting expired oauth_requests failed: {e}");
    }
    Ok(())
}

/// Fetch and consume an OAuth request by CSRF state. The row is atomically
/// deleted so that each state token can only be used once. Rows older than
/// [`OAUTH_REQUEST_TTL_SECS`] are consumed but treated as absent.
pub async fn fetch_oauth_request(
    pool: &DbPool,
    csrf_state: &str,
) -> Result<Option<OAuthRequestRow>, sqlx::Error> {
    let row = sqlx::query_as::<_, OAuthRequestRow>(
        "DELETE FROM oauth_requests WHERE csrf_state = $1 \
         RETURNING pkce_challenge, pkce_verifier, csrf_state, return_to, nonce, created_at, \
           browser_binding_hash",
    )
    .bind(csrf_state)
    .fetch_optional(pool)
    .await?;
    let cutoff = Utc::now() - Duration::seconds(OAUTH_REQUEST_TTL_SECS);
    Ok(row.filter(|r| r.created_at.is_some_and(|t| t >= cutoff)))
}

/// Upsert a user after successful OAuth login.
pub async fn upsert_user(
    pool: &DbPool,
    email: &str,
    name: &str,
    access_token: &str,
    refresh_token: Option<&str>,
) -> Result<(), sqlx::Error> {
    let query = format!(
        r#"
        INSERT INTO users (email, name, access_token, refresh_token, created_at, last_login)
        VALUES ($1, $2, $3, $4, {SQL_NOW}, {SQL_NOW})
        ON CONFLICT (email)
        DO UPDATE SET access_token = $3, refresh_token = $4, name = $2, last_login = {SQL_NOW}
        "#
    );
    sqlx::query(&query)
        .bind(email)
        .bind(name)
        .bind(access_token)
        .bind(refresh_token)
        .execute(pool)
        .await?;
    Ok(())
}

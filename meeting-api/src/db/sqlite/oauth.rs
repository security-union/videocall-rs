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

//! OAuth request and user storage queries (SQLite).

use crate::db::{DbPool, OAuthRequestRow};

/// Store a new OAuth request (PKCE + CSRF state + optional nonce) for later
/// retrieval in the callback.
pub async fn store_oauth_request(
    pool: &DbPool,
    pkce_challenge: &str,
    pkce_verifier: &str,
    csrf_state: &str,
    return_to: Option<&str>,
    nonce: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO oauth_requests (pkce_challenge, pkce_verifier, csrf_state, return_to, nonce)
        VALUES (?1, ?2, ?3, ?4, ?5)
        "#,
    )
    .bind(pkce_challenge)
    .bind(pkce_verifier)
    .bind(csrf_state)
    .bind(return_to)
    .bind(nonce)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch and consume an OAuth request by CSRF state.
/// The row is atomically deleted so that each state token can only be used once.
pub async fn fetch_oauth_request(
    pool: &DbPool,
    csrf_state: &str,
) -> Result<Option<OAuthRequestRow>, sqlx::Error> {
    sqlx::query_as::<_, OAuthRequestRow>(
        "DELETE FROM oauth_requests WHERE csrf_state = ?1 RETURNING pkce_challenge, pkce_verifier, csrf_state, return_to, nonce",
    )
    .bind(csrf_state)
    .fetch_optional(pool)
    .await
}

/// Upsert a user after successful OAuth login.
pub async fn upsert_user(
    pool: &DbPool,
    email: &str,
    name: &str,
    access_token: &str,
    refresh_token: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO users (email, name, access_token, refresh_token, created_at, last_login)
        VALUES (?1, ?2, ?3, ?4, datetime('now'), datetime('now'))
        ON CONFLICT (email)
        DO UPDATE SET access_token = ?3, refresh_token = ?4, name = ?2, last_login = datetime('now')
        "#,
    )
    .bind(email)
    .bind(name)
    .bind(access_token)
    .bind(refresh_token)
    .execute(pool)
    .await?;
    Ok(())
}

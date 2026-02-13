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

use sqlx::PgPool;

/// Stored PKCE challenge/verifier and CSRF state for an in-flight OAuth flow.
#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
pub struct OAuthRequestRow {
    pub pkce_challenge: Option<String>,
    pub pkce_verifier: Option<String>,
    pub csrf_state: Option<String>,
    pub return_to: Option<String>,
}

/// Store a new OAuth request (PKCE + CSRF state) for later retrieval in the callback.
pub async fn store_oauth_request(
    pool: &PgPool,
    pkce_challenge: &str,
    pkce_verifier: &str,
    csrf_state: &str,
    return_to: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO oauth_requests (pkce_challenge, pkce_verifier, csrf_state, return_to)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(pkce_challenge)
    .bind(pkce_verifier)
    .bind(csrf_state)
    .bind(return_to)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch and consume an OAuth request by CSRF state.
pub async fn fetch_oauth_request(
    pool: &PgPool,
    csrf_state: &str,
) -> Result<Option<OAuthRequestRow>, sqlx::Error> {
    sqlx::query_as::<_, OAuthRequestRow>(
        "SELECT pkce_challenge, pkce_verifier, csrf_state, return_to FROM oauth_requests WHERE csrf_state = $1",
    )
    .bind(csrf_state)
    .fetch_optional(pool)
    .await
}

/// Upsert a user after successful OAuth login.
pub async fn upsert_user(
    pool: &PgPool,
    email: &str,
    name: &str,
    access_token: &str,
    refresh_token: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO users (email, name, access_token, refresh_token, created_at, last_login)
        VALUES ($1, $2, $3, $4, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
        ON CONFLICT (email)
        DO UPDATE SET access_token = $3, refresh_token = $4, name = $2, last_login = CURRENT_TIMESTAMP
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

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
    pub nonce: Option<String>,
}

/// Store a new OAuth request (PKCE + CSRF state + optional nonce) for later
/// retrieval in the callback.
pub async fn store_oauth_request(
    pool: &PgPool,
    pkce_challenge: &str,
    pkce_verifier: &str,
    csrf_state: &str,
    return_to: Option<&str>,
    nonce: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO oauth_requests (pkce_challenge, pkce_verifier, csrf_state, return_to, nonce)
        VALUES ($1, $2, $3, $4, $5)
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
    pool: &PgPool,
    csrf_state: &str,
) -> Result<Option<OAuthRequestRow>, sqlx::Error> {
    sqlx::query_as::<_, OAuthRequestRow>(
        "DELETE FROM oauth_requests WHERE csrf_state = $1 RETURNING pkce_challenge, pkce_verifier, csrf_state, return_to, nonce",
    )
    .bind(csrf_state)
    .fetch_optional(pool)
    .await
}

/// Upsert a user after successful OAuth token exchange.
///
/// On **insert** (new user): persists the provider display name in both
/// `name` and `preferred_display_name`.
///
/// On **conflict** (returning user): refreshes the provider `name`, tokens,
/// and `last_login`, but leaves `preferred_display_name` untouched so any
/// in-meeting alias the user has configured is preserved.
pub async fn upsert_user(
    pool: &PgPool,
    email: &str,
    name: &str,
    access_token: &str,
    refresh_token: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO users (email, name, preferred_display_name, access_token, refresh_token,
                           created_at, last_login)
        VALUES ($1, $2, $2, $3, $4, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
        ON CONFLICT (email) DO UPDATE
            SET access_token         = $3,
                refresh_token        = $4,
                name                 = $2,
                last_login           = CURRENT_TIMESTAMP
                -- preferred_display_name intentionally not updated here;
                -- it is set only on first insert and when the user
                -- explicitly changes their in-meeting display name.
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

/// Update a user's preferred display name (e.g. when they join a meeting
/// with a custom alias).  No-op when the user is not in the `users` table
/// (i.e. when OAuth is not configured for this deployment).
pub async fn update_preferred_display_name(
    pool: &PgPool,
    user_id: &str,
    preferred_display_name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET preferred_display_name = $2 WHERE email = $1")
        .bind(user_id)
        .bind(preferred_display_name)
        .execute(pool)
        .await?;
    Ok(())
}

/// Upsert a user from an id_token identity without provider access/refresh
/// tokens.  Used by `POST /api/v1/user/register` when the browser performs
/// the token exchange directly with the provider (PKCE public-client flow)
/// and the provider tokens are held only in the browser.
///
/// Behaviour:
/// - **Insert** (new user): creates the row with the provider display name in
///   both `name` and `preferred_display_name`; `access_token` and
///   `refresh_token` are left NULL.
/// - **Conflict** (returning user): updates `name` and `last_login` only.
///   `preferred_display_name`, `access_token`, and `refresh_token` are not
///   overwritten so any user-configured alias and any tokens stored by the
///   full [`upsert_user`] path are preserved.
pub async fn register_user_from_token(
    pool: &PgPool,
    user_id: &str,
    name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO users (email, name, preferred_display_name, created_at, last_login)
        VALUES ($1, $2, $2, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
        ON CONFLICT (email) DO UPDATE
            SET name       = $2,
                last_login = CURRENT_TIMESTAMP
        "#,
    )
    .bind(user_id)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(())
}

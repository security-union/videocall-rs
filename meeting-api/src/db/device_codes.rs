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

//! Queries on `device_auth_codes`: one-time codes minted when a signed-in
//! user approves a CLI login, exchanged once at `POST /api/v1/devices`.
//!
//! Only the SHA-256 of a code is stored. A user has at most one outstanding
//! code: approving a new login deletes the previous one, together with every
//! expired code, so the table needs no sweeper.

use chrono::{DateTime, Utc};

use crate::db::{begin_write_tx, DbPool};

/// A `device_auth_codes` row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeviceCodeRow {
    /// SHA-256 (lowercase hex) of the code.
    pub code_hash: String,
    pub user_id: String,
    pub user_name: String,
    pub public_key: String,
    pub device_name: String,
    pub code_challenge: String,
    /// Canonical loopback URI or the remote constant.
    pub redirect_uri: String,
    pub expires_at: DateTime<Utc>,
}

/// Store a newly approved code. In one write transaction, first delete the
/// user's previous code and every expired code, then insert `row`.
pub async fn replace_code(
    pool: &DbPool,
    row: &DeviceCodeRow,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let mut tx = begin_write_tx(pool).await?;
    sqlx::query("DELETE FROM device_auth_codes WHERE user_id = $1 OR expires_at < $2")
        .bind(&row.user_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO device_auth_codes (code_hash, user_id, user_name, public_key, \
           device_name, code_challenge, redirect_uri, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(&row.code_hash)
    .bind(&row.user_id)
    .bind(&row.user_name)
    .bind(&row.public_key)
    .bind(&row.device_name)
    .bind(&row.code_challenge)
    .bind(&row.redirect_uri)
    .bind(row.expires_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Consume an unexpired code: one atomic `DELETE … RETURNING`. A code can be
/// returned at most once, whatever the caller does with it afterwards.
pub async fn consume_code(
    pool: &DbPool,
    code_hash: &str,
    now: DateTime<Utc>,
) -> Result<Option<DeviceCodeRow>, sqlx::Error> {
    sqlx::query_as::<_, DeviceCodeRow>(
        "DELETE FROM device_auth_codes WHERE code_hash = $1 AND expires_at > $2 \
         RETURNING code_hash, user_id, user_name, public_key, device_name, \
           code_challenge, redirect_uri, expires_at",
    )
    .bind(code_hash)
    .bind(now)
    .fetch_optional(pool)
    .await
}

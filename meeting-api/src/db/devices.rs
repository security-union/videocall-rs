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

//! Queries on the `devices` table. A row exists only for an approved device.
//! Timestamps are bound from the application so the same SQL runs on
//! Postgres and SQLite.

use chrono::{DateTime, Duration, Utc};

use crate::db::{begin_write_tx, Db, DbPool};

const DEVICE_COLUMNS: &str = "id, public_key, fingerprint, name, user_id, user_name, \
     created_at, last_used_at, idle_expires_at, abs_expires_at, revoked_at";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeviceRow {
    pub id: String,
    pub public_key: String,
    pub fingerprint: String,
    pub name: String,
    pub user_id: String,
    pub user_name: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub idle_expires_at: DateTime<Utc>,
    pub abs_expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl DeviceRow {
    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && self.idle_expires_at > now && self.abs_expires_at > now
    }
}

#[derive(Debug, Clone)]
pub struct NewDevice<'a> {
    pub id: &'a str,
    pub public_key: &'a str,
    pub fingerprint: &'a str,
    pub name: &'a str,
    pub user_id: &'a str,
    pub user_name: &'a str,
    /// Used for `created_at` and `last_used_at`.
    pub now: DateTime<Utc>,
    pub idle_expires_at: DateTime<Utc>,
    pub abs_expires_at: DateTime<Utc>,
}

/// True if `err` is a unique violation on `devices.public_key`.
pub fn is_duplicate_public_key(err: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db) = err else {
        return false;
    };
    db.is_unique_violation()
        && db
            .constraint()
            .map(|c| c.contains("public_key"))
            // SQLite names only the column, in the message.
            .unwrap_or_else(|| db.message().contains("devices.public_key"))
}

/// Insert an approved device unless the user is at `max_per_user` live
/// devices. On Postgres a transaction-scoped advisory lock keyed on the user
/// serializes concurrent logins of that user; on SQLite `BEGIN IMMEDIATE`
/// already serializes all writers. A duplicate key surfaces as the sqlx
/// error (see [`is_duplicate_public_key`]).
pub async fn insert_device_capped(
    pool: &DbPool,
    device: &NewDevice<'_>,
    max_per_user: i64,
) -> Result<CappedInsert, sqlx::Error> {
    let mut tx = begin_write_tx(pool).await?;
    #[cfg(feature = "postgres")]
    sqlx::query("SELECT pg_advisory_xact_lock(7461627, hashtext($1))")
        .bind(device.user_id)
        .execute(&mut *tx)
        .await?;
    // Relies on READ COMMITTED (begin_write_tx issues a plain BEGIN): the
    // COUNT takes a fresh snapshot after the lock is granted, so it sees the
    // previous holder's insert. Under REPEATABLE READ the cap could be
    // exceeded.
    if count_live_devices(&mut *tx, device.user_id, device.now).await? >= max_per_user {
        tx.rollback().await?;
        return Ok(CappedInsert::TooManyDevices);
    }
    sqlx::query(
        "INSERT INTO devices (id, public_key, fingerprint, name, user_id, user_name, \
           created_at, last_used_at, idle_expires_at, abs_expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $7, $8, $9)",
    )
    .bind(device.id)
    .bind(device.public_key)
    .bind(device.fingerprint)
    .bind(device.name)
    .bind(device.user_id)
    .bind(device.user_name)
    .bind(device.now)
    .bind(device.idle_expires_at)
    .bind(device.abs_expires_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(CappedInsert::Inserted)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CappedInsert {
    Inserted,
    TooManyDevices,
}

/// Number of the user's live devices.
pub async fn count_live_devices<'c, E>(
    executor: E,
    user_id: &str,
    now: DateTime<Utc>,
) -> Result<i64, sqlx::Error>
where
    E: sqlx::Executor<'c, Database = Db>,
{
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM devices \
         WHERE user_id = $1 AND revoked_at IS NULL \
           AND idle_expires_at > $2 AND abs_expires_at > $2",
    )
    .bind(user_id)
    .bind(now)
    .fetch_one(executor)
    .await
}

/// True if any device row (live, revoked or expired) holds this key.
pub async fn public_key_exists(pool: &DbPool, public_key: &str) -> Result<bool, sqlx::Error> {
    // COUNT(*) is BIGINT on Postgres and INTEGER on SQLite; both decode as i64.
    let found = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM devices WHERE public_key = $1")
        .bind(public_key)
        .fetch_one(pool)
        .await?;
    Ok(found > 0)
}

/// The hot path for every device JWT.
pub async fn find_device(pool: &DbPool, id: &str) -> Result<Option<DeviceRow>, sqlx::Error> {
    sqlx::query_as::<_, DeviceRow>(&format!(
        "SELECT {DEVICE_COLUMNS} FROM devices WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Record use of a live device, at most one write per hour, sliding
/// `idle_expires_at` to `idle_new` (capped at `abs_expires_at`). Only a live
/// device is touched, so a race can never revive an expired one.
pub async fn touch_device(
    pool: &DbPool,
    id: &str,
    now: DateTime<Utc>,
    idle_new: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE devices SET last_used_at = $2, \
           idle_expires_at = CASE WHEN $3 < abs_expires_at THEN $3 ELSE abs_expires_at END \
         WHERE id = $1 AND revoked_at IS NULL AND last_used_at < $4 \
           AND idle_expires_at > $2 AND abs_expires_at > $2",
    )
    .bind(id)
    .bind(now)
    .bind(idle_new)
    .bind(now - Duration::hours(1))
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Revoke one of `user_id`'s devices. `reason` is `device` (self-revoke) or
/// `user` (from the device list). False if there was nothing to revoke.
pub async fn revoke(
    pool: &DbPool,
    id: &str,
    user_id: &str,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE devices SET revoked_at = $3, revoked_reason = $4 \
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(user_id)
    .bind(now)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// The user's live devices, newest first.
pub async fn list_live_devices(
    pool: &DbPool,
    user_id: &str,
    now: DateTime<Utc>,
) -> Result<Vec<DeviceRow>, sqlx::Error> {
    sqlx::query_as::<_, DeviceRow>(&format!(
        "SELECT {DEVICE_COLUMNS} FROM devices \
         WHERE user_id = $1 AND revoked_at IS NULL \
           AND idle_expires_at > $2 AND abs_expires_at > $2 \
         ORDER BY created_at DESC"
    ))
    .bind(user_id)
    .bind(now)
    .fetch_all(pool)
    .await
}

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

//! Meeting co-host designation queries.

use sqlx::{PgConnection, PgPool};

#[derive(Debug, sqlx::FromRow)]
pub struct CoHostRow {
    pub user_id: String,
    pub persistent: bool,
    pub suspended: bool,
    pub designated: bool,
    pub is_present_host: bool,
    pub display_name: Option<String>,
}

/// Outcome of [`grant`].
#[derive(Debug, PartialEq, Eq)]
pub enum GrantOutcome {
    /// The entry was stored; `promoted` when an admitted target became host.
    Granted { promoted: bool },
    /// The meeting already holds the maximum number of co-host entries.
    LimitReached,
    /// An instance-only grant (`persistent = false`) on a non-active meeting.
    NotActive,
}

/// Outcome of [`revoke`].
#[derive(Debug, PartialEq, Eq)]
pub enum RevokeOutcome {
    /// The entry (if any) was deleted; `demoted` when the target lost `is_host`.
    Revoked { demoted: bool },
    /// The target has neither an entry nor the host role.
    NotFound,
    /// Refused: the target is the only present host of the active meeting.
    LastPresentHost,
}

/// Whether `user_id` holds an unsuspended co-host entry for the meeting.
pub async fn has_live_entry<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    meeting_id: i32,
    user_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM meeting_co_hosts \
         WHERE meeting_id = $1 AND user_id = $2 AND NOT suspended)",
    )
    .bind(meeting_id)
    .bind(user_id.to_lowercase())
    .fetch_one(executor)
    .await
}

/// Whether `user_id` holds a live, PERSISTENT co-host entry (the only kind that may start/restart an instance).
pub async fn has_live_persistent_entry<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    meeting_id: i32,
    user_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM meeting_co_hosts \
         WHERE meeting_id = $1 AND user_id = $2 AND NOT suspended AND persistent)",
    )
    .bind(meeting_id)
    .bind(user_id.to_lowercase())
    .fetch_one(executor)
    .await
}

/// The real-case `meeting_participants.user_id` matching `user_id`, if that identity has ever joined this meeting.
pub async fn canonical_user_id<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT user_id FROM meeting_participants \
         WHERE meeting_id = $1 AND LOWER(user_id) = $2 \
         ORDER BY admitted_at DESC NULLS LAST LIMIT 1",
    )
    .bind(meeting_id)
    .bind(user_id.to_lowercase())
    .fetch_optional(executor)
    .await
}

/// List the meeting's co-host entries plus any present non-owner host with no entry.
pub async fn list(
    pool: &PgPool,
    meeting_id: i32,
    healthy: bool,
) -> Result<Vec<CoHostRow>, sqlx::Error> {
    sqlx::query_as::<_, CoHostRow>(&format!(
        r#"
        SELECT COALESCE(p.user_id, c.user_id) AS user_id,
               c.persistent,
               c.suspended,
               TRUE AS designated,
               COALESCE(m.state = 'active' AND p.is_host AND {present}, FALSE) AS is_present_host,
               p.display_name,
               c.created_at AS sort_at
        FROM meeting_co_hosts c
        JOIN meetings m ON m.id = c.meeting_id
        LEFT JOIN meeting_participants p
            ON p.meeting_id = c.meeting_id AND LOWER(p.user_id) = c.user_id
        WHERE c.meeting_id = $1
        UNION ALL
        SELECT p.user_id, FALSE, FALSE, FALSE, TRUE, p.display_name, p.admitted_at
        FROM meeting_participants p
        JOIN meetings m ON m.id = p.meeting_id
        WHERE p.meeting_id = $1 AND m.state = 'active'
          AND p.is_host AND {present}
          AND p.user_id IS DISTINCT FROM m.creator_id
          AND NOT EXISTS (SELECT 1 FROM meeting_co_hosts c
                          WHERE c.meeting_id = p.meeting_id AND c.user_id = LOWER(p.user_id))
        ORDER BY designated DESC, sort_at, user_id
        "#,
        present = crate::db::participants::present_sql("p", healthy)
    ))
    .bind(meeting_id)
    .fetch_all(pool)
    .await
}

/// Store persistent co-host entries on the caller's transaction.
pub async fn insert_persistent(
    conn: &mut PgConnection,
    meeting_id: i32,
    user_ids: &[String],
    added_by: &str,
) -> Result<(), sqlx::Error> {
    if user_ids.is_empty() {
        return Ok(());
    }
    let user_ids: Vec<String> = user_ids.iter().map(|u| u.to_lowercase()).collect();
    sqlx::query(
        "INSERT INTO meeting_co_hosts (meeting_id, user_id, persistent, added_by) \
         SELECT $1, u, TRUE, $3 FROM UNNEST($2::text[]) AS u \
         ON CONFLICT (meeting_id, user_id) DO NOTHING",
    )
    .bind(meeting_id)
    .bind(user_ids)
    .bind(added_by)
    .execute(conn)
    .await?;
    Ok(())
}

/// Upsert a co-host entry, clearing any suspension; `persistent: None` keeps an existing entry's flag and defaults a new one to persistent.
pub async fn grant(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    persistent: Option<bool>,
    added_by: &str,
    max_entries: i64,
    healthy: bool,
) -> Result<GrantOutcome, sqlx::Error> {
    let user_id = user_id.to_lowercase();
    let user_id = user_id.as_str();
    let mut tx = pool.begin().await?;

    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM meetings WHERE id = $1 FOR UPDATE")
            .bind(meeting_id)
            .fetch_one(&mut *tx)
            .await?;
    let active = state.as_deref() == Some("active");
    let existing: Option<bool> = sqlx::query_scalar(
        "SELECT persistent FROM meeting_co_hosts WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let persistent = persistent.or(existing).unwrap_or(true);
    if !persistent && !active {
        tx.rollback().await?;
        return Ok(GrantOutcome::NotActive);
    }

    if existing.is_none() {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM meeting_co_hosts WHERE meeting_id = $1")
                .bind(meeting_id)
                .fetch_one(&mut *tx)
                .await?;
        if count >= max_entries {
            tx.rollback().await?;
            return Ok(GrantOutcome::LimitReached);
        }
    }

    sqlx::query(
        "INSERT INTO meeting_co_hosts (meeting_id, user_id, persistent, added_by) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (meeting_id, user_id) \
         DO UPDATE SET persistent = EXCLUDED.persistent, added_by = EXCLUDED.added_by, \
                       suspended = FALSE, updated_at = NOW()",
    )
    .bind(meeting_id)
    .bind(user_id)
    .bind(persistent)
    .bind(added_by)
    .execute(&mut *tx)
    .await?;

    let promoted = active
        && !promote_admitted(&mut tx, meeting_id, &[user_id.to_string()], healthy)
            .await?
            .is_empty();

    tx.commit().await?;
    Ok(GrantOutcome::Granted { promoted })
}

/// Delete a co-host entry and demote the target, unless they are the only present host of an active meeting.
pub async fn revoke(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    healthy: bool,
) -> Result<RevokeOutcome, sqlx::Error> {
    let user_id = user_id.to_lowercase();
    let user_id = user_id.as_str();
    let mut tx = pool.begin().await?;

    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM meetings WHERE id = $1 FOR UPDATE")
            .bind(meeting_id)
            .fetch_one(&mut *tx)
            .await?;

    let (has_entry, is_host, is_present_host): (bool, bool, bool) = sqlx::query_as(&format!(
        "SELECT EXISTS (SELECT 1 FROM meeting_co_hosts WHERE meeting_id = $1 AND user_id = $2), \
                COALESCE(BOOL_OR(p.is_host), FALSE), \
                COALESCE(BOOL_OR(p.is_host AND {}), FALSE) \
         FROM meeting_participants p WHERE p.meeting_id = $1 AND LOWER(p.user_id) = $2",
        crate::db::participants::present_sql("p", healthy)
    ))
    .bind(meeting_id)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    if !has_entry && !is_host {
        tx.rollback().await?;
        return Ok(RevokeOutcome::NotFound);
    }

    if state.as_deref() == Some("active") && is_present_host {
        let other_present_hosts: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM meeting_participants p \
             WHERE p.meeting_id = $1 AND LOWER(p.user_id) <> $2 AND p.is_host AND {}",
            crate::db::participants::present_sql("p", healthy)
        ))
        .bind(meeting_id)
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
        if other_present_hosts == 0 {
            tx.rollback().await?;
            return Ok(RevokeOutcome::LastPresentHost);
        }
    }

    sqlx::query("DELETE FROM meeting_co_hosts WHERE meeting_id = $1 AND user_id = $2")
        .bind(meeting_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    let demoted = sqlx::query(
        "UPDATE meeting_participants SET is_host = FALSE, updated_at = NOW() \
         WHERE meeting_id = $1 AND LOWER(user_id) = $2 AND is_host",
    )
    .bind(meeting_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;

    tx.commit().await?;
    Ok(RevokeOutcome::Revoked { demoted })
}

/// Give the host role to each admitted, present, unsuspended entry in `user_ids`. Caller must hold the meeting row lock.
pub async fn promote_admitted(
    conn: &mut PgConnection,
    meeting_id: i32,
    user_ids: &[String],
    healthy: bool,
) -> Result<Vec<String>, sqlx::Error> {
    if user_ids.is_empty() {
        return Ok(Vec::new());
    }
    let user_ids: Vec<String> = user_ids.iter().map(|u| u.to_lowercase()).collect();
    sqlx::query_scalar(&format!(
        "UPDATE meeting_participants p SET is_host = TRUE, updated_at = NOW() \
         FROM meeting_co_hosts c \
         WHERE p.meeting_id = $1 AND LOWER(p.user_id) = ANY($2) \
           AND {} AND NOT p.is_host AND NOT p.is_guest \
           AND c.meeting_id = p.meeting_id AND c.user_id = LOWER(p.user_id) AND NOT c.suspended \
         RETURNING p.user_id",
        crate::db::participants::present_sql("p", healthy)
    ))
    .bind(meeting_id)
    .bind(user_ids)
    .fetch_all(conn)
    .await
}

/// Suspend `user_id`'s entry for the rest of the instance.
pub async fn suspend<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    meeting_id: i32,
    user_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE meeting_co_hosts SET suspended = TRUE \
         WHERE meeting_id = $1 AND user_id = $2 AND NOT suspended",
    )
    .bind(meeting_id)
    .bind(user_id.to_lowercase())
    .execute(executor)
    .await?;
    Ok(())
}

/// Instance boundary: delete instance-only entries and lift suspensions.
pub async fn reset_for_new_instance(
    conn: &mut PgConnection,
    meeting_id: i32,
) -> Result<(), sqlx::Error> {
    clear_instance_only_entries(&mut *conn, meeting_id).await?;
    lift_suspensions(&mut *conn, meeting_id).await
}

/// Delete instance-only (non-persistent) co-host entries; safe on any instance boundary, including `/end`.
pub async fn clear_instance_only_entries(
    conn: &mut PgConnection,
    meeting_id: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM meeting_co_hosts WHERE meeting_id = $1 AND NOT persistent")
        .bind(meeting_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Lift every suspension. Call only when a new instance actually starts, not on `/end`.
pub async fn lift_suspensions(conn: &mut PgConnection, meeting_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE meeting_co_hosts SET suspended = FALSE WHERE meeting_id = $1 AND suspended",
    )
    .bind(meeting_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

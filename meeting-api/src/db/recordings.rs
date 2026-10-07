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

//! Recording lease queries (#2856). Every write runs under the meeting's
//! `FOR UPDATE` lock and is scoped to that meeting.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::recording::{Rejection, CHURN_CAP, CHURN_WINDOW_SECS, LEASE_SECS, MEETING_CAP};

/// The recording policy for participant `p` of meeting `m`; callers add their presence condition.
pub fn recording_policy_sql(m: &str, p: &str) -> String {
    format!(
        "{m}.deleted_at IS NULL AND {m}.state IS DISTINCT FROM 'ended' \
         AND NOT {p}.is_guest AND ({p}.is_host OR {m}.recording_allowed_for_all)"
    )
}

/// The registry of one meeting as broadcast: opaque ids only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub epoch: u64,
    pub version: i64,
    pub entries: Vec<(Uuid, bool)>,
}

#[derive(Debug)]
pub enum RegisterOutcome {
    MeetingNotFound,
    /// `snapshot` is set when the refusal still deleted lapsed leases.
    Rejected {
        reason: Rejection,
        snapshot: Option<Snapshot>,
    },
    Granted {
        recording_id: Uuid,
        snapshot: Snapshot,
    },
}

/// Register `user_id`'s recording in `room_id`, or re-issue the lease a retry
/// of the same `attempt_id` already holds, under `secret_hash`.
pub async fn register(
    pool: &PgPool,
    room_id: &str,
    user_id: &str,
    attempt_id: Uuid,
    secret_hash: &[u8],
) -> Result<RegisterOutcome, sqlx::Error> {
    let present: Option<bool> = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM meeting_participants p \
                        WHERE p.meeting_id = m.id AND p.user_id = $2 \
                          AND p.status = 'admitted' AND p.left_at IS NULL) \
         FROM meetings m WHERE m.room_id = $1 AND m.deleted_at IS NULL",
    )
    .bind(room_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    match present {
        None => return Ok(RegisterOutcome::MeetingNotFound),
        Some(false) => return Ok(not_admitted()),
        Some(true) => {}
    }

    let mut tx = pool.begin().await?;
    let Some((meeting_id, created_at)): Option<(i32, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, created_at FROM meetings \
         WHERE room_id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(room_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(RegisterOutcome::MeetingNotFound);
    };

    type Caller = (bool, bool, bool, bool, bool);
    let caller: Option<Caller> = sqlx::query_as(&format!(
        "SELECT {policy}, \
                m.state = 'ended', p.is_guest, p.status = 'admitted' AND p.left_at IS NULL, \
                p.is_host \
         FROM meeting_participants p JOIN meetings m ON m.id = p.meeting_id \
         WHERE p.meeting_id = $1 AND p.user_id = $2",
        policy = recording_policy_sql("m", "p")
    ))
    .bind(meeting_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((permitted, ended, is_guest, admitted, is_host)) = caller else {
        return Ok(not_admitted());
    };
    if !admitted {
        return Ok(not_admitted());
    }
    if !permitted {
        let reason = match () {
            _ if ended => Rejection::MeetingEnded,
            _ if is_guest => Rejection::Guest,
            _ => Rejection::NotPermitted,
        };
        return Ok(RegisterOutcome::Rejected {
            reason,
            snapshot: None,
        });
    }

    let attempts = count_attempt(&mut tx, meeting_id).await?;
    let purged = purge_lapsed(&mut tx, meeting_id).await?;
    let (granted, changed) = if !is_host && attempts > CHURN_CAP {
        (Err(Rejection::RateLimited), false)
    } else {
        grant(
            &mut tx,
            meeting_id,
            user_id,
            attempt_id,
            secret_hash,
            is_host,
        )
        .await?
    };
    let published = if changed || purged {
        bump(&mut tx, meeting_id).await?;
        Some(snapshot(&mut tx, meeting_id, created_at).await?)
    } else {
        None
    };
    let outcome = match granted {
        Err(reason) => RegisterOutcome::Rejected {
            reason,
            snapshot: published,
        },
        Ok(recording_id) => RegisterOutcome::Granted {
            recording_id,
            snapshot: match published {
                Some(s) => s,
                None => snapshot(&mut tx, meeting_id, created_at).await?,
            },
        },
    };
    tx.commit().await?;
    Ok(outcome)
}

fn not_admitted() -> RegisterOutcome {
    RegisterOutcome::Rejected {
        reason: Rejection::NotAdmitted,
        snapshot: None,
    }
}

/// The caller's lease after register, and whether any lease row changed.
async fn grant(
    tx: &mut PgConnection,
    meeting_id: i32,
    user_id: &str,
    attempt_id: Uuid,
    secret_hash: &[u8],
    is_host: bool,
) -> Result<(Result<Uuid, Rejection>, bool), sqlx::Error> {
    let own: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT recording_id, attempt_id \
         FROM meeting_recording_leases WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    match own {
        Some((recording_id, held_attempt)) if held_attempt == attempt_id => {
            sqlx::query(
                "UPDATE meeting_recording_leases SET secret_hash = $3 \
                 WHERE recording_id = $1 AND meeting_id = $2",
            )
            .bind(recording_id)
            .bind(meeting_id)
            .bind(secret_hash)
            .execute(&mut *tx)
            .await?;
            return Ok((Ok(recording_id), false));
        }
        Some(_) => return Ok((Err(Rejection::UserCap), false)),
        None => {}
    }

    if !is_host {
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM meeting_recording_leases l \
             WHERE l.meeting_id = $1 AND l.user_id <> $2 AND l.revoked_at IS NULL \
               AND l.renewed_at > clock_timestamp() - make_interval(secs => $3::double precision) \
               AND NOT EXISTS (SELECT 1 FROM meeting_participants p \
                               WHERE p.meeting_id = l.meeting_id AND p.user_id = l.user_id \
                                 AND p.is_host)",
        )
        .bind(meeting_id)
        .bind(user_id)
        .bind(LEASE_SECS as f64)
        .fetch_one(&mut *tx)
        .await?;
        if active >= MEETING_CAP {
            return Ok((Err(Rejection::MeetingCap), false));
        }
    }

    let recording_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO meeting_recording_leases \
             (recording_id, meeting_id, user_id, attempt_id, secret_hash) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(recording_id)
    .bind(meeting_id)
    .bind(user_id)
    .bind(attempt_id)
    .bind(secret_hash)
    .execute(&mut *tx)
    .await?;
    Ok((Ok(recording_id), true))
}

/// Delete the meeting's lapsed leases; `true` when any was deleted.
async fn purge_lapsed(tx: &mut PgConnection, meeting_id: i32) -> Result<bool, sqlx::Error> {
    sqlx::query(
        "DELETE FROM meeting_recording_leases WHERE meeting_id = $1 \
           AND renewed_at <= clock_timestamp() - make_interval(secs => $2::double precision)",
    )
    .bind(meeting_id)
    .bind(LEASE_SECS as f64)
    .execute(tx)
    .await
    .map(|r| r.rows_affected() > 0)
}

/// Count one register attempt; returns the two-bucket sliding estimate.
async fn count_attempt(tx: &mut PgConnection, meeting_id: i32) -> Result<f64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO meeting_recording_state AS s \
             (meeting_id, version, changed_at, reg_window_start, reg_window_count, reg_prev_count) \
         SELECT $1, 0, t.now, t.now, 1, 0 FROM (SELECT clock_timestamp() AS now) t \
         ON CONFLICT (meeting_id) DO UPDATE SET \
           reg_prev_count = CASE \
             WHEN EXCLUDED.reg_window_start >= s.reg_window_start + 2 * make_interval(secs => $2) THEN 0 \
             WHEN EXCLUDED.reg_window_start >= s.reg_window_start + make_interval(secs => $2) \
               THEN s.reg_window_count \
             ELSE s.reg_prev_count END, \
           reg_window_count = CASE \
             WHEN EXCLUDED.reg_window_start >= s.reg_window_start + make_interval(secs => $2) THEN 1 \
             ELSE s.reg_window_count + 1 END, \
           reg_window_start = CASE \
             WHEN EXCLUDED.reg_window_start >= s.reg_window_start + 2 * make_interval(secs => $2) \
               THEN EXCLUDED.reg_window_start \
             WHEN EXCLUDED.reg_window_start >= s.reg_window_start + make_interval(secs => $2) \
               THEN s.reg_window_start + make_interval(secs => $2) \
             ELSE s.reg_window_start END \
         RETURNING (reg_prev_count * LEAST(1, GREATEST(0, 1 - \
             EXTRACT(EPOCH FROM clock_timestamp() - reg_window_start)::double precision / $2)) \
             + reg_window_count)::double precision",
    )
    .bind(meeting_id)
    .bind(CHURN_WINDOW_SECS)
    .fetch_one(tx)
    .await
}

async fn bump(tx: &mut PgConnection, meeting_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO meeting_recording_state AS s \
             (meeting_id, version, changed_at, reg_window_start, reg_window_count, reg_prev_count) \
         SELECT $1, 1, t.now, t.now, 0, 0 FROM (SELECT clock_timestamp() AS now) t \
         ON CONFLICT (meeting_id) DO UPDATE \
         SET version = s.version + 1, changed_at = EXCLUDED.changed_at",
    )
    .bind(meeting_id)
    .execute(tx)
    .await
    .map(|_| ())
}

async fn snapshot(
    tx: &mut PgConnection,
    meeting_id: i32,
    created_at: DateTime<Utc>,
) -> Result<Snapshot, sqlx::Error> {
    let version: i64 = sqlx::query_scalar(
        "SELECT COALESCE((SELECT version FROM meeting_recording_state WHERE meeting_id = $1), 0)",
    )
    .bind(meeting_id)
    .fetch_one(&mut *tx)
    .await?;
    let entries = sqlx::query_as(
        "SELECT recording_id, revoked_at IS NOT NULL FROM meeting_recording_leases \
         WHERE meeting_id = $1 ORDER BY created_at, recording_id",
    )
    .bind(meeting_id)
    .fetch_all(&mut *tx)
    .await?;
    Ok(Snapshot {
        epoch: created_at.timestamp_micros().max(0) as u64,
        version,
        entries,
    })
}

/// End the lease `recording_id` in `room_id` iff `secret_hash` matches. A miss
/// takes no lock. Returns the new snapshot when a lease was deleted.
pub async fn stop(
    pool: &PgPool,
    room_id: &str,
    recording_id: Uuid,
    secret_hash: &[u8],
) -> Result<Option<Snapshot>, sqlx::Error> {
    let hit: Option<i32> = sqlx::query_scalar(
        "SELECT l.meeting_id FROM meeting_recording_leases l \
         JOIN meetings m ON m.id = l.meeting_id \
         WHERE l.recording_id = $1 AND l.secret_hash = $2 \
           AND m.room_id = $3 AND m.deleted_at IS NULL",
    )
    .bind(recording_id)
    .bind(secret_hash)
    .bind(room_id)
    .fetch_optional(pool)
    .await?;
    let Some(meeting_id) = hit else {
        return Ok(None);
    };

    let mut tx = pool.begin().await?;
    let created_at: DateTime<Utc> =
        sqlx::query_scalar("SELECT created_at FROM meetings WHERE id = $1 FOR UPDATE")
            .bind(meeting_id)
            .fetch_one(&mut *tx)
            .await?;
    let deleted = sqlx::query(
        "DELETE FROM meeting_recording_leases \
         WHERE recording_id = $1 AND secret_hash = $2 AND meeting_id = $3",
    )
    .bind(recording_id)
    .bind(secret_hash)
    .bind(meeting_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if deleted != 1 {
        tx.rollback().await?;
        return Ok(None);
    }
    bump(&mut tx, meeting_id).await?;
    let snapshot = snapshot(&mut tx, meeting_id, created_at).await?;
    tx.commit().await?;
    Ok(Some(snapshot))
}

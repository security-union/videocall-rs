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

//! Meeting participant table queries.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};

use crate::db::meetings::Activation;
use videocall_meeting_types::presence::{
    PRESENCE_CONNECT_WINDOW_SECS, PRESENCE_HEARTBEAT_INTERVAL_SECS, PRESENCE_LEASE_SECS,
};

/// Row returned from the `meeting_participants` table.
#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
pub struct ParticipantRow {
    pub id: i32,
    pub meeting_id: i32,
    pub user_id: String,
    pub status: String,
    pub is_host: bool,
    pub is_guest: bool,
    pub is_required: bool,
    pub joined_at: DateTime<Utc>,
    pub admitted_at: Option<DateTime<Utc>>,
    pub left_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub display_name: Option<String>,
}

const PARTICIPANT_COLUMNS: &str = r#"
    id, meeting_id, user_id, status, is_host, is_guest, is_required,
    joined_at, admitted_at, left_at, created_at, updated_at, display_name
"#;

/// Insert or update a participant as host (admitted immediately). A rejoin never overwrites an existing non-empty `display_name`.
pub async fn upsert_host<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<ParticipantRow, sqlx::Error> {
    let query = format!(
        r#"
        INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, is_guest, display_name, admitted_at, live_session_id)
        VALUES ($1, $2, 'admitted', TRUE, FALSE, $3, NOW(), 0)
        ON CONFLICT (meeting_id, user_id)
        DO UPDATE SET status = 'admitted', is_host = TRUE, admitted_at = NOW(), left_at = NULL,
                      live_session_id = 0,
                      display_name = COALESCE(NULLIF(meeting_participants.display_name, ''), $3)
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .bind(display_name)
        .fetch_one(executor)
        .await
}

/// Admit a co-host into an active meeting as host, bypassing the waiting room. `Ok(None)` if ineligible.
pub async fn admit_as_co_host(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<Option<(ParticipantRow, bool)>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let active: bool = sqlx::query_scalar(
        "SELECT state IS NOT DISTINCT FROM 'active' FROM meetings WHERE id = $1 FOR UPDATE",
    )
    .bind(meeting_id)
    .fetch_one(&mut *tx)
    .await?;
    if !active || !crate::db::co_hosts::has_live_entry(&mut *tx, meeting_id, user_id).await? {
        tx.rollback().await?;
        return Ok(None);
    }

    let (was_host, blocked): (bool, bool) = sqlx::query_as(
        "SELECT p.is_host, \
                COALESCE(p.is_guest \
                    OR (p.status = 'kicked' AND p.left_at >= m.started_at) \
                    OR (p.status = 'rejected' AND p.updated_at >= m.started_at), FALSE) \
         FROM meeting_participants p JOIN meetings m ON m.id = p.meeting_id \
         WHERE p.meeting_id = $1 AND p.user_id = $2 \
         FOR UPDATE OF p",
    )
    .bind(meeting_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or((false, false));
    if blocked {
        tx.rollback().await?;
        return Ok(None);
    }
    let row = upsert_host(&mut *tx, meeting_id, user_id, display_name).await?;
    tx.commit().await?;
    Ok(Some((row, !was_host)))
}

/// Atomically join a meeting as an attendee under the current `waiting_room_enabled`. `Ok(None)` if `require_present_host` and none is present.
pub async fn join_attendee(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
    require_present_host: bool,
    is_guest: bool,
    healthy: bool,
) -> Result<Option<(bool, ParticipantRow, bool)>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Lock the meeting row to serialize against concurrent waiting room toggles.
    let (waiting_room_enabled,): (bool,) =
        sqlx::query_as("SELECT waiting_room_enabled FROM meetings WHERE id = $1 FOR UPDATE")
            .bind(meeting_id)
            .fetch_one(&mut *tx)
            .await?;

    // Checked in-transaction to close a TOCTOU race.
    if require_present_host && count_present_hosts(&mut *tx, meeting_id, healthy).await? == 0 {
        tx.rollback().await?;
        return Ok(None);
    }

    // `is_host` is omitted from both DO UPDATE branches so a reconnect never demotes a host.
    let row = if waiting_room_enabled {
        let query = format!(
            r#"
            INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, is_guest, display_name, live_session_id)
            VALUES ($1, $2, 'waiting', FALSE, $4, $3, 0)
            ON CONFLICT (meeting_id, user_id)
            DO UPDATE SET status = 'waiting', left_at = NULL, live_session_id = 0,
                          display_name = COALESCE(NULLIF(meeting_participants.display_name, ''), $3)
            RETURNING {PARTICIPANT_COLUMNS}
            "#
        );
        sqlx::query_as::<_, ParticipantRow>(&query)
            .bind(meeting_id)
            .bind(user_id)
            .bind(display_name)
            .bind(is_guest)
            .fetch_one(&mut *tx)
            .await?
    } else {
        let query = format!(
            r#"
            INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, is_guest, display_name, admitted_at, live_session_id)
            VALUES ($1, $2, 'admitted', FALSE, $4, $3, NOW(), 0)
            ON CONFLICT (meeting_id, user_id)
            DO UPDATE SET status = 'admitted', admitted_at = NOW(), left_at = NULL,
                          live_session_id = 0,
                          display_name = COALESCE(NULLIF(meeting_participants.display_name, ''), $3)
            RETURNING {PARTICIPANT_COLUMNS}
            "#
        );
        sqlx::query_as::<_, ParticipantRow>(&query)
            .bind(meeting_id)
            .bind(user_id)
            .bind(display_name)
            .bind(is_guest)
            .fetch_one(&mut *tx)
            .await?
    };

    tx.commit().await?;
    Ok(Some((!waiting_room_enabled, row, waiting_room_enabled)))
}

/// Get all participants in 'waiting' status for a meeting.
pub async fn get_waiting(
    pool: &PgPool,
    meeting_id: i32,
) -> Result<Vec<ParticipantRow>, sqlx::Error> {
    let query = format!(
        "SELECT {PARTICIPANT_COLUMNS} FROM meeting_participants WHERE meeting_id = $1 AND status = 'waiting'"
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .fetch_all(pool)
        .await
}

/// Get all admitted (active) participants in a meeting.
pub async fn get_admitted(
    pool: &PgPool,
    meeting_id: i32,
) -> Result<Vec<ParticipantRow>, sqlx::Error> {
    let query = format!(
        "SELECT {PARTICIPANT_COLUMNS} FROM meeting_participants WHERE meeting_id = $1 AND status = 'admitted'"
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .fetch_all(pool)
        .await
}

/// Get a single participant's status.
pub async fn get_status<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        "SELECT {PARTICIPANT_COLUMNS} FROM meeting_participants WHERE meeting_id = $1 AND user_id = $2"
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .fetch_optional(executor)
        .await
}

/// Admit a single waiting participant and activate the meeting. A designated
/// co-host gets the host role only once their transport connects
/// ([`record_present`]).
pub async fn admit(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    healthy: bool,
) -> Result<Option<(ParticipantRow, Activation)>, sqlx::Error> {
    let (mut rows, activation) = admit_waiting(pool, meeting_id, Some(user_id), healthy).await?;
    Ok(rows.pop().map(|row| (row, activation)))
}

/// Admit all waiting participants at once and, when any were, activate the
/// meeting.
pub async fn admit_all(
    pool: &PgPool,
    meeting_id: i32,
    healthy: bool,
) -> Result<(Vec<ParticipantRow>, Activation), sqlx::Error> {
    admit_waiting(pool, meeting_id, None, healthy).await
}

async fn admit_waiting(
    pool: &PgPool,
    meeting_id: i32,
    user_id: Option<&str>,
    healthy: bool,
) -> Result<(Vec<ParticipantRow>, Activation), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM meetings WHERE id = $1 FOR UPDATE")
        .bind(meeting_id)
        .execute(&mut *tx)
        .await?;
    let anyone_waiting: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM meeting_participants \
         WHERE meeting_id = $1 AND status = 'waiting' AND ($2::text IS NULL OR user_id = $2))",
    )
    .bind(meeting_id)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    if !anyone_waiting {
        tx.rollback().await?;
        return Ok((Vec::new(), Activation::Unchanged));
    }
    // Before admitting: the admitted rows would make an empty meeting look occupied.
    let activation = crate::db::meetings::start_instance_in(&mut tx, meeting_id, healthy).await?;
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'admitted', admitted_at = NOW(), live_session_id = 0
        WHERE meeting_id = $1 AND status = 'waiting' AND ($2::text IS NULL OR user_id = $2)
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    let rows = sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((rows, activation))
}

/// Reject a participant.
pub async fn reject(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'rejected'
        WHERE meeting_id = $1 AND user_id = $2 AND status = 'waiting'
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

/// Outcome of [`kick`].
#[derive(Debug, PartialEq, Eq)]
pub enum KickOutcome {
    /// The target was removed; `was_host` when they held the host role.
    Kicked { was_host: bool },
    /// The target has a row but it is not part of the current instance;
    /// nothing changed.
    NotAdmitted,
    /// The target has no participant row.
    NotFound,
    /// The caller is not an admitted host.
    CallerNotHost,
    /// Only the owner may kick the owner, a host, or a designated co-host.
    OwnerOnly,
    /// A caller cannot kick themselves.
    CannotKickSelf,
}

/// Kick `target` on behalf of `caller`: mark left, strip host, suspend any co-host entry this instance.
pub async fn kick(
    pool: &PgPool,
    meeting_id: i32,
    caller: &str,
    target: &str,
) -> Result<KickOutcome, sqlx::Error> {
    if caller == target {
        return Ok(KickOutcome::CannotKickSelf);
    }
    let mut tx = pool.begin().await?;
    let (creator_id, started_at): (Option<String>, DateTime<Utc>) =
        sqlx::query_as("SELECT creator_id, started_at FROM meetings WHERE id = $1 FOR UPDATE")
            .bind(meeting_id)
            .fetch_one(&mut *tx)
            .await?;
    let caller_is_host: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM meeting_participants \
         WHERE meeting_id = $1 AND user_id = $2 AND status = 'admitted' AND is_host)",
    )
    .bind(meeting_id)
    .bind(caller)
    .fetch_one(&mut *tx)
    .await?;
    let target_row: Option<(String, bool, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT status, is_host, admitted_at FROM meeting_participants \
         WHERE meeting_id = $1 AND user_id = $2 FOR UPDATE",
    )
    .bind(meeting_id)
    .bind(target)
    .fetch_optional(&mut *tx)
    .await?;

    let outcome = match target_row {
        _ if !caller_is_host => KickOutcome::CallerNotHost,
        None => KickOutcome::NotFound,
        Some((_, is_host, _))
            if creator_id.as_deref() != Some(caller)
                && (creator_id.as_deref() == Some(target)
                    || is_host
                    || crate::db::co_hosts::has_live_entry(&mut *tx, meeting_id, target)
                        .await?) =>
        {
            KickOutcome::OwnerOnly
        }
        Some((ref status, _, admitted_at))
            if status != "admitted"
                && !(status == "left" && admitted_at.is_some_and(|a| a >= started_at)) =>
        {
            KickOutcome::NotAdmitted
        }
        Some((_, was_host, _)) => {
            sqlx::query(
                "UPDATE meeting_participants \
                 SET status = 'kicked', left_at = NOW(), is_host = FALSE, live_session_id = 0 \
                 WHERE meeting_id = $1 AND user_id = $2",
            )
            .bind(meeting_id)
            .bind(target)
            .execute(&mut *tx)
            .await?;
            crate::db::co_hosts::suspend(&mut *tx, meeting_id, target).await?;
            KickOutcome::Kicked { was_host }
        }
    };
    if matches!(outcome, KickOutcome::Kicked { .. }) {
        tx.commit().await?;
    } else {
        tx.rollback().await?;
    }
    Ok(outcome)
}

/// Admit the creator on rejoin into an already-active meeting without changing `is_host` (a transfer target keeps it).
pub async fn admit_creator_preserve_host<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<ParticipantRow, sqlx::Error> {
    let query = format!(
        r#"
        INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, is_guest, display_name, admitted_at, live_session_id)
        VALUES ($1, $2, 'admitted', FALSE, FALSE, $3, NOW(), 0)
        ON CONFLICT (meeting_id, user_id)
        DO UPDATE SET status = 'admitted', admitted_at = NOW(), left_at = NULL,
                      live_session_id = 0,
                      display_name = COALESCE(NULLIF(meeting_participants.display_name, ''), $3)
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .bind(display_name)
        .fetch_one(executor)
        .await
}

/// Atomically hand host from `from_user_id` to `to_user_id`. `Ok(None)` (no-op) if the caller lost the race or the target isn't admitted.
pub async fn transfer_host(
    pool: &PgPool,
    meeting_id: i32,
    from_user_id: &str,
    to_user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Serializes concurrent transfers from the same host.
    sqlx::query("SELECT id FROM meetings WHERE id = $1 FOR UPDATE")
        .bind(meeting_id)
        .fetch_optional(&mut *tx)
        .await?;

    // Demote the caller, but ONLY if they are still a host. A transfer that
    // lost the race finds the caller already demoted (`is_host = FALSE`) → zero
    // rows → abort with `None`, so one host role is never handed out twice.
    let demoted = sqlx::query(
        "UPDATE meeting_participants SET is_host = FALSE, updated_at = NOW() \
         WHERE meeting_id = $1 AND user_id = $2 AND is_host = TRUE",
    )
    .bind(meeting_id)
    .bind(from_user_id)
    .execute(&mut *tx)
    .await?;
    if demoted.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(None);
    }

    // Promote the target (must be an admitted, non-guest participant). If it
    // matches no row, roll back — which also undoes the demote above, so the
    // caller keeps host and the meeting is never left without one.
    let promote_query = format!(
        r#"
        UPDATE meeting_participants
        SET is_host = TRUE, updated_at = NOW()
        WHERE meeting_id = $1 AND user_id = $2 AND status = 'admitted' AND is_guest = FALSE
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    let promoted = sqlx::query_as::<_, ParticipantRow>(&promote_query)
        .bind(meeting_id)
        .bind(to_user_id)
        .fetch_optional(&mut *tx)
        .await?;

    let Some(target_row) = promoted else {
        tx.rollback().await?;
        return Ok(None);
    };

    crate::db::co_hosts::suspend(&mut *tx, meeting_id, from_user_id).await?;
    tx.commit().await?;
    Ok(Some(target_row))
}

/// Demote every host that is NOT the meeting creator, at each instance
/// boundary, returning the users demoted. Idempotent; `IS DISTINCT FROM` is
/// null-safe against a NULL `creator_id`.
pub async fn clear_non_creator_hosts<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    meeting_id: i32,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE meeting_participants mp SET is_host = FALSE, updated_at = NOW() \
         FROM meetings m \
         WHERE mp.meeting_id = $1 AND m.id = mp.meeting_id \
           AND mp.is_host = TRUE AND mp.user_id IS DISTINCT FROM m.creator_id \
         RETURNING mp.user_id",
    )
    .bind(meeting_id)
    .fetch_all(executor)
    .await
}

/// How a [`depart`] ended the meeting, if it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepartureEnd {
    /// The last present host left with `end_on_host_leave`; clients must be
    /// told (`MEETING_ENDED`).
    LastHostLeft,
    /// The last admitted participant left.
    Empty,
}

/// Mark a participant left, ending or idling the meeting as appropriate. `Ok(None)` if not admitted or waiting.
pub async fn depart(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    end_when_empty: bool,
    healthy: bool,
) -> Result<Option<(ParticipantRow, Option<DepartureEnd>)>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let departed = depart_in(&mut tx, meeting_id, user_id, end_when_empty, false, healthy).await?;
    tx.commit().await?;
    Ok(departed)
}

/// `keep_live_session`: true only for a lease-expiry guess, so a later heartbeat can restore the row.
async fn depart_in(
    conn: &mut PgConnection,
    meeting_id: i32,
    user_id: &str,
    end_when_empty: bool,
    keep_live_session: bool,
    healthy: bool,
) -> Result<Option<(ParticipantRow, Option<DepartureEnd>)>, sqlx::Error> {
    let (state, end_on_host_leave): (Option<String>, bool) =
        sqlx::query_as("SELECT state, end_on_host_leave FROM meetings WHERE id = $1 FOR UPDATE")
            .bind(meeting_id)
            .fetch_one(&mut *conn)
            .await?;
    let was_present: Option<bool> = sqlx::query_scalar(
        "SELECT status = 'admitted' AND left_at IS NULL FROM meeting_participants \
         WHERE meeting_id = $1 AND user_id = $2 AND status IN ('admitted', 'waiting')",
    )
    .bind(meeting_id)
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(was_present) = was_present else {
        return Ok(None);
    };
    let query = if keep_live_session {
        format!(
            r#"
            UPDATE meeting_participants
            SET status = 'left', left_at = NOW()
            WHERE meeting_id = $1 AND user_id = $2 AND status IN ('admitted', 'waiting')
            RETURNING {PARTICIPANT_COLUMNS}
            "#
        )
    } else {
        format!(
            r#"
            UPDATE meeting_participants
            SET status = 'left', left_at = NOW(), live_session_id = 0
            WHERE meeting_id = $1 AND user_id = $2 AND status IN ('admitted', 'waiting')
            RETURNING {PARTICIPANT_COLUMNS}
            "#
        )
    };
    let row = sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .fetch_one(&mut *conn)
        .await?;

    let mut ended = None;
    if state.as_deref() != Some(crate::db::meetings::STATE_ENDED) {
        if row.is_host {
            if was_present
                && end_on_host_leave
                && count_present_hosts(&mut *conn, meeting_id, healthy).await? == 0
            {
                ended = Some(DepartureEnd::LastHostLeft);
            }
        } else if end_when_empty && count_admitted(&mut *conn, meeting_id, healthy).await? == 0 {
            ended = Some(DepartureEnd::Empty);
        }
    }
    if ended.is_some() {
        crate::db::meetings::end_meeting_in(conn, meeting_id).await?;
    } else if state.as_deref() == Some(crate::db::meetings::STATE_ACTIVE)
        && count_admitted(&mut *conn, meeting_id, healthy).await? == 0
    {
        sqlx::query("UPDATE meetings SET state = 'idle' WHERE id = $1")
            .bind(meeting_id)
            .execute(&mut *conn)
            .await?;
    }
    Ok(Some((row, ended)))
}

/// Relay session ids a participant row remembers as left.
const LEFT_SESSIONS_KEPT: i32 = 8;

/// Apply a relay's report that `session_id` of `user_id` left. A stale or already-applied report departs nothing.
pub async fn record_left(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    session_id: i64,
    healthy: bool,
) -> Result<Option<(ParticipantRow, Option<DepartureEnd>)>, sqlx::Error> {
    // Cheap unlocked pre-check; re-checked for real under the lock below.
    let maybe_relevant: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM meeting_participants \
         WHERE meeting_id = $1 AND user_id = $2 AND status <> 'rejected' \
           AND NOT ($3 = ANY (left_session_ids)))",
    )
    .bind(meeting_id)
    .bind(user_id)
    .bind(session_id)
    .fetch_one(pool)
    .await?;
    if !maybe_relevant {
        return Ok(None);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM meetings WHERE id = $1 FOR UPDATE")
        .bind(meeting_id)
        .execute(&mut *tx)
        .await?;
    let is_live: Option<bool> = sqlx::query_scalar(
        "UPDATE meeting_participants \
         SET left_session_ids = (ARRAY[$3::BIGINT] || left_session_ids)[1:$4] \
         WHERE meeting_id = $1 AND user_id = $2 AND status <> 'rejected' \
           AND NOT ($3 = ANY (left_session_ids)) \
         RETURNING live_session_id IS NULL OR live_session_id = $3",
    )
    .bind(meeting_id)
    .bind(user_id)
    .bind(session_id)
    .bind(LEFT_SESSIONS_KEPT)
    .fetch_optional(&mut *tx)
    .await?;
    let departed = if is_live == Some(true) {
        depart_in(&mut tx, meeting_id, user_id, false, false, healthy).await?
    } else {
        None
    };
    tx.commit().await?;
    Ok(departed)
}

/// What [`record_present`] changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Presence {
    /// A left row was restored to present.
    pub restored: bool,
    /// The meeting went from idle back to active, within the same instance.
    pub resumed: bool,
    /// A designated, unsuspended co-host got the host role.
    pub promoted: bool,
}

/// Apply a relay's report that `session_id` of `user_id` is present: restore/resume/promote as applicable.
pub async fn record_present(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    session_id: i64,
    healthy: bool,
) -> Result<Presence, sqlx::Error> {
    // Cheap unlocked pre-check; re-checked for real under the lock.
    let maybe_relevant: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM meeting_participants p JOIN meetings m ON m.id = p.meeting_id \
         WHERE p.meeting_id = $1 AND p.user_id = $2 AND p.admitted_at IS NOT NULL \
           AND m.state IS DISTINCT FROM 'ended' \
           AND NOT ($3 = ANY (p.left_session_ids)) \
           AND ((p.status = 'admitted' AND p.live_session_id IS DISTINCT FROM $3) \
                OR (p.status = 'left' AND p.admitted_at >= m.started_at)))",
    )
    .bind(meeting_id)
    .bind(user_id)
    .bind(session_id)
    .fetch_one(pool)
    .await?;
    if !maybe_relevant {
        return Ok(Presence::default());
    }
    let mut tx = pool.begin().await?;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM meetings WHERE id = $1 FOR UPDATE")
            .bind(meeting_id)
            .fetch_one(&mut *tx)
            .await?;
    if state.as_deref() == Some(crate::db::meetings::STATE_ENDED) {
        tx.rollback().await?;
        return Ok(Presence::default());
    }
    let prior_status: Option<String> = sqlx::query_scalar(
        "SELECT p.status FROM meeting_participants p JOIN meetings m ON m.id = p.meeting_id \
         WHERE p.meeting_id = $1 AND p.user_id = $2 AND p.admitted_at IS NOT NULL \
           AND NOT ($3 = ANY (p.left_session_ids)) \
           AND ((p.status = 'admitted' AND p.live_session_id IS DISTINCT FROM $3) \
                OR (p.status = 'left' AND p.admitted_at >= m.started_at)) \
         FOR UPDATE OF p",
    )
    .bind(meeting_id)
    .bind(user_id)
    .bind(session_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(prior_status) = prior_status else {
        tx.rollback().await?;
        return Ok(Presence::default());
    };
    sqlx::query(
        "UPDATE meeting_participants \
         SET live_session_id = $3, presence_seen_at = NOW(), status = 'admitted', left_at = NULL \
         WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting_id)
    .bind(user_id)
    .bind(session_id)
    .execute(&mut *tx)
    .await?;

    let resumed = state.as_deref() == Some(crate::db::meetings::STATE_IDLE)
        && sqlx::query("UPDATE meetings SET state = 'active' WHERE id = $1 AND state = 'idle'")
            .bind(meeting_id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
    let active = resumed || state.as_deref() == Some(crate::db::meetings::STATE_ACTIVE);
    let promoted = active
        && !crate::db::co_hosts::promote_admitted(
            &mut tx,
            meeting_id,
            &[user_id.to_string()],
            healthy,
        )
        .await?
        .is_empty();
    tx.commit().await?;
    Ok(Presence {
        restored: prior_status == "left",
        resumed,
        promoted,
    })
}

/// Renew the presence lease of each `(user_id, session_id)` a relay still holds: any non-tombstoned session renews an admitted row without changing `live_session_id`, so a `LEFT` for that exact session can still only match the session `record_present` actually recorded live. Also restores a matching `left` row. A `user_id` with two different session ids in one batch is dropped as ambiguous.
pub async fn record_heartbeat(
    pool: &PgPool,
    room_id: &str,
    sessions: &[(String, i64)],
) -> Result<u64, sqlx::Error> {
    let (user_ids, session_ids): (Vec<String>, Vec<i64>) = sessions.iter().cloned().unzip();
    // Ascending-`id` lock order matches `start_instance_in`'s retire UPDATE,
    // so the two can't deadlock; `SKIP LOCKED` is safe since a heartbeat is
    // best-effort and retried next tick. The watermark upsert is folded into
    // the same statement (one round trip); a data-modifying CTE always runs,
    // but both are still referenced by the final SELECT to keep that explicit.
    let (renewed,): (i64,) = sqlx::query_as(&format!(
        "WITH hb AS ( \
            SELECT user_id, MAX(session_id) AS session_id \
            FROM UNNEST($2::TEXT[], $3::BIGINT[]) AS t(user_id, session_id) \
            GROUP BY user_id \
            HAVING COUNT(DISTINCT session_id) = 1 \
         ), \
         candidates AS ( \
            SELECT mp.id, hb.session_id AS hb_session_id \
            FROM hb \
            JOIN meetings m ON m.room_id = $1 AND m.deleted_at IS NULL \
            JOIN meeting_participants mp \
                ON mp.meeting_id = m.id AND mp.user_id = hb.user_id \
            WHERE m.state IS DISTINCT FROM 'ended' \
              AND NOT (hb.session_id = ANY (mp.left_session_ids)) \
              AND ( \
                    (mp.status = 'admitted' AND mp.left_at IS NULL) \
                 OR (mp.status = 'left' AND mp.admitted_at >= m.started_at \
                     AND mp.live_session_id = hb.session_id) \
              ) \
            ORDER BY mp.id \
            FOR UPDATE OF mp SKIP LOCKED \
         ), \
         renewed AS ( \
            UPDATE meeting_participants mp \
            SET presence_seen_at = NOW(), \
                status = 'admitted', \
                left_at = NULL, \
                live_session_id = CASE WHEN mp.live_session_id IS NULL \
                                       THEN candidates.hb_session_id ELSE mp.live_session_id END \
            FROM candidates \
            WHERE mp.id = candidates.id \
            RETURNING 1 \
         ), \
         watermark AS ( \
            INSERT INTO presence_heartbeat_watermark (id, updated_at) \
            VALUES (TRUE, NOW()) \
            ON CONFLICT (id) DO UPDATE SET updated_at = NOW() \
            WHERE presence_heartbeat_watermark.updated_at \
                  < NOW() - INTERVAL '{WATERMARK_STAMP_MIN_INTERVAL_SECS} seconds' \
            RETURNING 1 \
         ) \
         SELECT (SELECT COUNT(*) FROM renewed)::BIGINT \
         WHERE (SELECT COUNT(*) FROM watermark) IS NOT NULL"
    ))
    .bind(room_id)
    .bind(&user_ids)
    .bind(&session_ids)
    .fetch_one(pool)
    .await?;
    Ok(renewed as u64)
}

/// Watermark staleness threshold for [`presence_healthy`]: two heartbeat
/// intervals, so one missed beat does not itself trip the fallback.
const WATERMARK_STALE_SECS: i64 = 2 * PRESENCE_HEARTBEAT_INTERVAL_SECS as i64;

/// How rarely [`record_heartbeat`] actually writes the watermark row: far
/// below [`WATERMARK_STALE_SECS`] (60s), so detection latency is unaffected,
/// but well above single-digit-millisecond heartbeat traffic bursts.
const WATERMARK_STAMP_MIN_INTERVAL_SECS: i64 = 10;

/// Stamp proof that the NATS -> meeting-api -> Postgres pipeline is alive
/// right now, unconditionally. One singleton row shared by every replica.
/// Test-only: production stamps go through [`record_heartbeat`]'s own gated
/// upsert instead.
#[doc(hidden)]
pub async fn force_heartbeat_watermark_fresh_for_test(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO presence_heartbeat_watermark (id, updated_at) VALUES (TRUE, NOW()) \
         ON CONFLICT (id) DO UPDATE SET updated_at = NOW()",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether a presence heartbeat has reached the database within
/// [`WATERMARK_STALE_SECS`]. `false` when the row has never been written.
/// `pub(crate)`: [`crate::state::AppState`] caches this half of the health
/// check for a short TTL, since the REST call sites are far more frequent
/// than the watermark actually changes.
pub(crate) async fn heartbeat_watermark_fresh<'e>(
    executor: impl sqlx::PgExecutor<'e>,
) -> Result<bool, sqlx::Error> {
    let updated_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT updated_at FROM presence_heartbeat_watermark WHERE id = TRUE")
            .fetch_optional(executor)
            .await?;
    Ok(
        updated_at
            .is_some_and(|t| Utc::now() - t < chrono::Duration::seconds(WATERMARK_STALE_SECS)),
    )
}

/// Whether this replica's own NATS client reports `Connected`. Vacuously
/// `true` with no client (nothing local to check; the watermark still gates).
/// `pub(crate)`: always checked live, never cached — see
/// [`heartbeat_watermark_fresh`].
pub(crate) fn nats_locally_connected(nats: Option<&async_nats::Client>) -> bool {
    nats.map(|c| c.connection_state() == async_nats::connection::State::Connected)
        .unwrap_or(true)
}

/// Whether the presence-lease pipeline is healthy enough to trust for a
/// "nobody present" decision: a heartbeat reached the database recently AND
/// this replica's NATS connection is up. `false` degrades [`present_sql`] to
/// latch semantics and tells the sweeper to skip its tick.
pub async fn presence_healthy(
    pool: &PgPool,
    nats: Option<&async_nats::Client>,
) -> Result<bool, sqlx::Error> {
    Ok(nats_locally_connected(nats) && heartbeat_watermark_fresh(pool).await?)
}

/// SQL condition that row alias `p` is present: a lease check when `healthy`, else latch semantics.
pub fn present_sql(p: &str, healthy: bool) -> String {
    if !healthy {
        return format!("({p}.status = 'admitted' AND {p}.left_at IS NULL)");
    }
    format!(
        "({p}.status = 'admitted' AND {p}.left_at IS NULL \
          AND (({p}.presence_seen_at IS NOT NULL \
                AND {p}.presence_seen_at > NOW() - INTERVAL '{PRESENCE_LEASE_SECS} seconds') \
               OR (COALESCE({p}.live_session_id, 0) = 0 \
                   AND {p}.admitted_at > NOW() - INTERVAL '{PRESENCE_CONNECT_WINDOW_SECS} seconds')))"
    )
}

/// Whether anyone is present (see [`present_sql`]) in the meeting.
pub async fn any_present<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    meeting_id: i32,
    healthy: bool,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM meeting_participants p \
         WHERE p.meeting_id = $1 AND {})",
        present_sql("p", healthy)
    ))
    .bind(meeting_id)
    .fetch_one(executor)
    .await
}

/// Up to `limit` admitted rows whose presence lease ran out, as
/// `(meeting_id, room_id, user_id)`. Always strict — callers only reach this
/// after confirming the pipeline is healthy (see [`presence_healthy`]).
pub async fn expired_presences(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<(i32, String, String)>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT p.meeting_id, m.room_id, p.user_id \
         FROM meeting_participants p JOIN meetings m ON m.id = p.meeting_id \
         WHERE p.status = 'admitted' AND p.left_at IS NULL AND NOT {} \
         LIMIT $1",
        present_sql("p", true)
    ))
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Depart a participant whose lease ran out; keeps `live_session_id` since this is a guess, not a confirmed departure.
pub async fn depart_expired(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<(ParticipantRow, Option<DepartureEnd>)>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM meetings WHERE id = $1 FOR UPDATE")
        .bind(meeting_id)
        .execute(&mut *tx)
        .await?;
    let expired: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM meeting_participants p \
         WHERE p.meeting_id = $1 AND p.user_id = $2 \
           AND p.status = 'admitted' AND p.left_at IS NULL AND NOT {})",
        present_sql("p", true)
    ))
    .bind(meeting_id)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    let departed = if expired {
        depart_in(&mut tx, meeting_id, user_id, false, true, true).await?
    } else {
        None
    };
    tx.commit().await?;
    Ok(departed)
}

/// Leave a meeting (set status to 'left').
pub async fn leave(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'left', left_at = NOW(), live_session_id = 0
        WHERE meeting_id = $1 AND user_id = $2 AND status IN ('admitted', 'waiting')
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

/// Renew the caller's own presence lease directly (pre-join lobby, no live transport session). Returns whether a row matched.
pub async fn keepalive(pool: &PgPool, meeting_id: i32, user_id: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE meeting_participants mp \
         SET presence_seen_at = NOW() \
         FROM meetings m \
         WHERE m.id = mp.meeting_id AND mp.meeting_id = $1 AND mp.user_id = $2 \
           AND mp.status = 'admitted' AND mp.left_at IS NULL \
           AND COALESCE(mp.live_session_id, 0) = 0 \
           AND m.state IS DISTINCT FROM 'ended'",
    )
    .bind(meeting_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Update a participant's display name.
pub async fn update_display_name(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    display_name: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET display_name = $3, updated_at = NOW()
        WHERE meeting_id = $1 AND user_id = $2
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .bind(display_name)
        .fetch_optional(pool)
        .await
}

/// Load the participant roster (admitted or waiting) for a SearchV2 index push, host first.
pub async fn list_for_search(
    pool: &PgPool,
    meeting_id: i32,
) -> Result<Vec<crate::search::ParticipantAcl>, sqlx::Error> {
    // Module-private row struct keeps the query typed without leaking a
    // tuple signature across fn boundaries (and pleases
    // `clippy::type_complexity`).  joined_at is NOT NULL in the schema;
    // admitted_at is nullable until the participant is admitted from the
    // waiting room.
    #[derive(sqlx::FromRow)]
    struct SearchParticipantRow {
        user_id: String,
        display_name: Option<String>,
        is_host: bool,
        status: String,
        joined_at: DateTime<Utc>,
        admitted_at: Option<DateTime<Utc>>,
    }

    let rows: Vec<SearchParticipantRow> = sqlx::query_as(
        r#"
        SELECT user_id, display_name, is_host, status, joined_at, admitted_at
        FROM meeting_participants
        WHERE meeting_id = $1
          AND status IN ('admitted', 'waiting')
        ORDER BY is_host DESC, admitted_at NULLS LAST, created_at
        "#,
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| crate::search::ParticipantAcl {
            user_id: r.user_id,
            display_name: r.display_name,
            is_host: r.is_host,
            status: r.status,
            joined_at: Some(r.joined_at),
            admitted_at: r.admitted_at,
        })
        .collect())
}

/// Count participants who are CURRENTLY present in a meeting (see
/// [`present_sql`]): the meeting-settings "Activity" count (issue #1551). An
/// explicit REST `/leave` and a transport departure both set `left_at`, and a
/// participant whose relay stopped reporting them drops out when the lease
/// runs out.
pub async fn count_admitted<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    meeting_id: i32,
    healthy: bool,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM meeting_participants p WHERE p.meeting_id = $1 AND {}",
        present_sql("p", healthy)
    ))
    .bind(meeting_id)
    .fetch_one(executor)
    .await
}

/// Whether `user_id` is a present (see [`present_sql`]) host of the meeting.
/// Used to authorize a transfer-host target — who holds `is_host` but no
/// `meeting_co_hosts` entry — to change meeting options (issue #2702 round
/// 10).
pub async fn is_present_host<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    meeting_id: i32,
    user_id: &str,
    healthy: bool,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM meeting_participants p \
         WHERE p.meeting_id = $1 AND p.user_id = $2 AND p.is_host AND {})",
        present_sql("p", healthy)
    ))
    .bind(meeting_id)
    .bind(user_id)
    .fetch_one(executor)
    .await
}

/// Count present participants (see [`present_sql`]) holding the host role.
pub async fn count_present_hosts<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    meeting_id: i32,
    healthy: bool,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM meeting_participants p \
         WHERE p.meeting_id = $1 AND p.is_host AND {}",
        present_sql("p", healthy)
    ))
    .bind(meeting_id)
    .fetch_one(executor)
    .await
}

/// Count participants still in the waiting room (explicit-leave-only; a transport disconnect does not un-count them).
pub async fn count_waiting(pool: &PgPool, meeting_id: i32) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM meeting_participants \
         WHERE meeting_id = $1 AND status = 'waiting' AND left_at IS NULL",
    )
    .bind(meeting_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

// -- Conversions to API response types --

impl ParticipantRow {
    /// Convert a database row into the API response type.
    /// Optionally attach a `room_token` (only for the participant themselves).
    pub fn into_participant_status(
        self,
        room_token: Option<String>,
    ) -> videocall_meeting_types::responses::ParticipantStatusResponse {
        videocall_meeting_types::responses::ParticipantStatusResponse {
            is_guest: self.is_guest,
            user_id: self.user_id,
            display_name: self.display_name,
            status: self.status,
            is_host: self.is_host,
            joined_at: self.joined_at.timestamp(),
            admitted_at: self.admitted_at.map(|t| t.timestamp()),
            room_token,
            observer_token: None,
            waiting_room_enabled: false,
            admitted_can_admit: false,
            end_on_host_leave: true,
            host_display_name: None,
            host_user_id: None,
            allow_guests: false,
            recording_allowed_for_all: false,
            chat_allowed_for_all: true,
        }
    }
}

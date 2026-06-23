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
use sqlx::PgPool;

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

/// Insert or update a participant as host (admitted immediately).
///
/// **Display-name reconciliation policy on rejoin:** when a row already exists
/// for `(meeting_id, user_id)` with a non-empty `display_name`, the existing
/// value is preserved — the request's `display_name` does NOT overwrite it.
/// This is intentional: rejoin must never silently rename a participant.
/// Mid-meeting renames go through the rate-limited
/// [`crate::routes::participants::update_display_name`] endpoint.
///
/// The `NULLIF(..., '')` rewrites an empty-string existing value to `NULL` so
/// the `COALESCE` falls through to the request's value — empty-string is
/// treated as "no name set yet" (the legitimate first-time case where a
/// follow-up rejoin should be allowed to fill it in).
///
/// See issue #502 for the bug this prevents (manually-typed name "Antonio"
/// being silently overwritten by an OAuth-derived "Tony" on back-then-rejoin).
pub async fn upsert_host(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<ParticipantRow, sqlx::Error> {
    let query = format!(
        r#"
        INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, is_guest, display_name, admitted_at)
        VALUES ($1, $2, 'admitted', TRUE, FALSE, $3, NOW())
        ON CONFLICT (meeting_id, user_id)
        DO UPDATE SET status = 'admitted', is_host = TRUE, admitted_at = NOW(), left_at = NULL,
                      display_name = COALESCE(NULLIF(meeting_participants.display_name, ''), $3)
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .bind(display_name)
        .fetch_one(pool)
        .await
}

/// Atomically join a meeting as an attendee, respecting the current `waiting_room_enabled`
/// setting. Locks the meeting row with `FOR UPDATE` to serialize against concurrent
/// waiting room toggles via `update_meeting_settings`.
///
/// When `check_host_gone_for` is `Some(creator_id)`, verifies within the same transaction
/// that the host is still admitted. Returns `Ok(None)` if the host has left — callers
/// should respond with a "joining not allowed" error. This closes the TOCTOU window
/// that arises when the check is performed outside the transaction.
///
/// Returns `Ok(Some((auto_admitted, row, waiting_room_enabled)))` on success, where
/// `auto_admitted` is `true` when the participant was immediately admitted (waiting room
/// disabled). The third element is the `waiting_room_enabled` value observed under the
/// row lock.
pub async fn join_attendee(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
    check_host_gone_for: Option<&str>,
    is_guest: bool,
) -> Result<Option<(bool, ParticipantRow, bool)>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Lock the meeting row to serialize against concurrent waiting room toggles.
    let (waiting_room_enabled,): (bool,) =
        sqlx::query_as("SELECT waiting_room_enabled FROM meetings WHERE id = $1 FOR UPDATE")
            .bind(meeting_id)
            .fetch_one(&mut *tx)
            .await?;

    // If requested, verify within the same transaction that the host has not left.
    // Doing this outside the transaction creates a TOCTOU race: two concurrent
    // requests can both pass the pre-transaction check, then both insert into a
    // meeting where no one can admit them.
    if let Some(creator_id) = check_host_gone_for {
        let host_status: Option<(String,)> = sqlx::query_as(
            "SELECT status FROM meeting_participants WHERE meeting_id = $1 AND user_id = $2",
        )
        .bind(meeting_id)
        .bind(creator_id)
        .fetch_optional(&mut *tx)
        .await?;

        let host_is_gone = host_status.map(|(s,)| s != "admitted").unwrap_or(true);
        if host_is_gone {
            tx.rollback().await?;
            return Ok(None);
        }
    }

    // Display-name reconciliation policy on rejoin: see [`upsert_host`] for
    // the rationale. The same `COALESCE(NULLIF(...), $3)` shape applies to
    // both branches below — non-empty existing names beat the request's
    // value so rejoin never silently renames a participant. Issue #502.
    //
    // Host-flag policy on rejoin: `is_host` is intentionally OMITTED from both
    // `DO UPDATE SET` branches, so a transient transport reconnect (which does
    // NOT call REST /leave) never silently demotes the current host. An
    // EXPLICIT leave is different — `leave_meeting` clears the host flag
    // before this rejoin runs, so a deliberate Leave + rejoin returns them as a
    // regular participant. The waiting-room branch inserts `is_host = FALSE`
    // only for a brand-new row; an existing flag is preserved here and
    // governed solely by the leave path.
    let row = if waiting_room_enabled {
        let query = format!(
            r#"
            INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, is_guest, display_name)
            VALUES ($1, $2, 'waiting', FALSE, $4, $3)
            ON CONFLICT (meeting_id, user_id)
            DO UPDATE SET status = 'waiting', left_at = NULL,
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
            INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, is_guest, display_name, admitted_at)
            VALUES ($1, $2, 'admitted', FALSE, $4, $3, NOW())
            ON CONFLICT (meeting_id, user_id)
            DO UPDATE SET status = 'admitted', admitted_at = NOW(), left_at = NULL,
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
pub async fn get_status(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        "SELECT {PARTICIPANT_COLUMNS} FROM meeting_participants WHERE meeting_id = $1 AND user_id = $2"
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

/// Admit a single participant.
pub async fn admit(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'admitted', admitted_at = NOW()
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

/// Admit all waiting participants at once.
pub async fn admit_all(pool: &PgPool, meeting_id: i32) -> Result<Vec<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'admitted', admitted_at = NOW()
        WHERE meeting_id = $1 AND status = 'waiting'
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .fetch_all(pool)
        .await
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

/// Kick a participant (set status to 'kicked', record left_at).
/// Only transitions from 'admitted' — ignores waiting/left/etc.
pub async fn kick(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'kicked', left_at = NOW()
        WHERE meeting_id = $1 AND user_id = $2 AND status = 'admitted'
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

/// Admit the creator on rejoin into an ALREADY-ACTIVE meeting WITHOUT changing
/// `is_host`.
///
/// A transfer-host may have moved host to another participant; the creator
/// rejoining mid-meeting must NOT reclaim it — the host stays the transfer
/// target until the meeting ends. So unlike [`upsert_host`], this never sets
/// `is_host = TRUE` on the existing row (the `DO UPDATE` omits `is_host`), it
/// only re-admits the creator. The `INSERT` branch's `is_host = FALSE` is a
/// safety default for the (not-expected) brand-new-row case. Mirrors the
/// display-name reconciliation policy of [`upsert_host`] / [`join_attendee`].
pub async fn admit_creator_preserve_host(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<ParticipantRow, sqlx::Error> {
    let query = format!(
        r#"
        INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, is_guest, display_name, admitted_at)
        VALUES ($1, $2, 'admitted', FALSE, FALSE, $3, NOW())
        ON CONFLICT (meeting_id, user_id)
        DO UPDATE SET status = 'admitted', admitted_at = NOW(), left_at = NULL,
                      display_name = COALESCE(NULLIF(meeting_participants.display_name, ''), $3)
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .bind(display_name)
        .fetch_one(pool)
        .await
}

/// Atomically transfer host from `from_user_id` to `to_user_id` (single-host
/// model: host is handed off, not shared).
///
/// Demotes the caller and promotes the target in a single transaction, holding
/// a row lock on the meeting so concurrent transfers can't produce two hosts.
///
/// Returns:
/// - `Ok(Some(target_row))` on success;
/// - `Ok(None)` when the caller is no longer the host (lost a concurrent race —
///   the row lock + `is_host` guard make the loser a clean no-op) OR the target
///   is not an admitted participant. In both cases nothing is committed, so the
///   caller is never demoted without a valid successor and a second host can
///   never be created.
///
/// Ordering: the caller is demoted BEFORE the target is promoted, so there is
/// at most one `is_host` row at any instant.
pub async fn transfer_host(
    pool: &PgPool,
    meeting_id: i32,
    from_user_id: &str,
    to_user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Serialize against concurrent transfers (and `join_attendee`, which locks
    // the same row): two near-simultaneous transfers from the same host would
    // otherwise both pass the pre-BEGIN `require_host` check and each promote a
    // different target — split-brain with two hosts. The lock forces them to
    // run one-at-a-time so the second sees the post-first state.
    sqlx::query("SELECT id FROM meetings WHERE id = $1 FOR UPDATE")
        .bind(meeting_id)
        .fetch_optional(&mut *tx)
        .await?;

    // Demote the caller, but ONLY if they are still the host. A transfer that
    // lost the race finds the caller already demoted (`is_host = FALSE`) → zero
    // rows → abort with `None`, so it never goes on to create a second host.
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

    // Promote the target (must be an admitted participant). If it matches no
    // row, roll back — which also undoes the demote above, so the caller keeps
    // host and the meeting is never left without one.
    let promote_query = format!(
        r#"
        UPDATE meeting_participants
        SET is_host = TRUE, updated_at = NOW()
        WHERE meeting_id = $1 AND user_id = $2 AND status = 'admitted'
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

    tx.commit().await?;
    Ok(Some(target_row))
}

/// Demote every host that is NOT the meeting creator (single-host reset).
///
/// In the single-host model the creator is the default host; a transfer-host
/// may move host to someone else for the current session. This resets to the
/// creator: called on meeting end (so the next activation starts clean) and on
/// the creator's (re)join (so the creator reclaims sole host, never coexisting
/// with a stale transfer target). Idempotent. `IS DISTINCT FROM` is null-safe
/// against a NULL `creator_id`.
pub async fn clear_non_creator_hosts(pool: &PgPool, meeting_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE meeting_participants mp SET is_host = FALSE, updated_at = NOW() \
         FROM meetings m \
         WHERE mp.meeting_id = $1 AND m.id = mp.meeting_id \
           AND mp.is_host = TRUE AND mp.user_id IS DISTINCT FROM m.creator_id",
    )
    .bind(meeting_id)
    .execute(pool)
    .await?;
    Ok(())
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
        SET status = 'left', left_at = NOW()
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

/// Load the participant roster for a SearchV2 index push.
///
/// Returns one row per `admitted` or `waiting` participant.  We include
/// `waiting` users so they become searchable as soon as they enter the
/// waiting room — they're already visible to Postgres-side searches via
/// [`crate::db::meetings::list_by_owner`], which JOINs on any participant
/// row regardless of status.
///
/// Shape is mapped into [`crate::search::ParticipantAcl`], which is the
/// minimal subset needed for both the top-level `participants` / `acls`
/// arrays and the richer `documentObject.participants` entries.
///
/// Ordering: host first (for stable creator-first ACL lists), then by
/// admission time to keep doc diffs predictable as the roster evolves.
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

/// Count admitted participants who are CURRENTLY present in a meeting.
///
/// "Present" is `status = 'admitted' AND left_at IS NULL` — the same predicate
/// the presence-driven idle transition uses (`db::meetings::set_idle`). The
/// `left_at IS NULL` guard is what makes the meeting-settings "Activity"
/// participant count reflect who is currently in the meeting rather than every
/// participant who was ever admitted (issue #1551): an explicit REST `/leave`
/// sets `left_at=NOW()` and a transport disconnect is marked left by the
/// `PARTICIPANT_LEFT` NATS consumer ([`mark_left_by_disconnect`]), so both kinds
/// of departure drop out of the count. The guard is also defense-in-depth — a
/// row whose `left_at` was set but whose `status` somehow lagged at `'admitted'`
/// is still excluded.
pub async fn count_admitted(pool: &PgPool, meeting_id: i32) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM meeting_participants \
         WHERE meeting_id = $1 AND status = 'admitted' AND left_at IS NULL",
    )
    .bind(meeting_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Count participants still in the waiting room.
///
/// Mirrors [`count_admitted`]'s SQL shape: `status = 'waiting' AND left_at IS
/// NULL`, so a waiter who EXPLICITLY left (REST `/leave`, which sets
/// `left_at=NOW()`) is no longer counted as waiting. Unlike the admitted count,
/// the `left_at IS NULL` guard does NOT heal a waiter who merely *transport-*
/// disconnected: a waiting-room participant connects to the relay as an
/// `observer` session, and the relay's observer-disconnect path returns BEFORE
/// the [`PARTICIPANT_LEFT_SUBJECT`](crate::nats_events::PARTICIPANT_LEFT_SUBJECT)
/// publish — so [`mark_left_by_disconnect`] never fires for a dropped waiter,
/// who therefore keeps `status='waiting', left_at IS NULL` until they explicitly
/// leave or are admitted/rejected. This is an accepted known limitation: issue
/// #1551 is about the admitted participant count, where the relay publishes the
/// disconnect event; the waiting count is left as the explicit-leave-only
/// behavior it had before.
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

/// Mark a participant `status='left', left_at=NOW()` in response to a transport
/// disconnect observed by `actix-api` (the `PARTICIPANT_LEFT` NATS event).
///
/// This is the backstop for ABNORMAL disconnects — a participant who closed
/// their tab, dropped their network, or crashed WITHOUT calling the REST
/// `/leave` endpoint. Normal navigation still goes through REST `/leave`
/// (`leave`); this keeps the DB roster correct when that beacon never fires.
///
/// # Idempotency / reconnect-safety
///
/// The `WHERE … status IN ('admitted', 'waiting')` guard makes this a safe
/// no-op when the participant is already `'left'` / `'kicked'` / `'rejected'`,
/// or has no row at all (e.g. they only ever observed the waiting room). A
/// duplicate event (NATS redelivery, multi-replica fan-out) therefore matches
/// zero rows and does not overwrite the original `left_at`.
///
/// The dangerous case is the reconnect race: this UPDATE is keyed by `user_id`,
/// and a rejoined participant is back at `status='admitted', left_at=NULL` — so
/// the `status IN (...)` guard alone does NOT stop a late event from re-marking
/// a now-present participant left. That race is closed UPSTREAM, on the relay,
/// not here. The relay only publishes `PARTICIPANT_LEFT` after the
/// `RECONNECT_GRACE_PERIOD` (a timely transport reconnect cancels the pending
/// departure before `leave_rooms` runs), AND it suppresses the publish entirely
/// when the departing user still has any live session in the room
/// (`user_has_remaining_session` / `user_still_present` in
/// `chat_server.rs::leave_rooms`) — including a different tab that rejoined after
/// the grace expired. So by the time this UPDATE runs, the relay has confirmed
/// the user has no present session. This function deliberately does NOT add a
/// `left_at IS NULL` predicate: a freshly-rejoined row has `left_at IS NULL` and
/// would still match, so such a predicate would give false safety; the real
/// guarantee is the relay-side presence check above.
///
/// Returns the number of rows updated (0 when the participant was already left,
/// already kicked/rejected, or has no row for this meeting).
pub async fn mark_left_by_disconnect(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE meeting_participants \
         SET status = 'left', left_at = NOW() \
         WHERE meeting_id = $1 AND user_id = $2 AND status IN ('admitted', 'waiting')",
    )
    .bind(meeting_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
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
        }
    }
}

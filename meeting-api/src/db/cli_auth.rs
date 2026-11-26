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

//! Queries on `cli_auth_requests`: one `videocall-cli auth login` in flight.
//!
//! Lifecycle: created by `GET /cli/authorize`; the consent page mints a
//! per-render nonce; `POST /cli/consent` decides it (and, on approval,
//! attaches the SHA-256 of a one-time code); `POST /api/v1/devices`
//! consumes the code exactly once. Every state change is one guarded
//! `UPDATE … RETURNING`, never read-then-write.

use chrono::{DateTime, Duration, Utc};

use crate::db::{DbPool, SWEEP_BATCH};

/// How long a login request may wait for a decision.
pub const REQUEST_TTL_SECS: i64 = 600;
/// Lifetime of the one-time code for a loopback login.
pub const LOOPBACK_CODE_TTL_SECS: i64 = 60;
/// Lifetime of the one-time code for a remote login (typed by a human).
pub const REMOTE_CODE_TTL_SECS: i64 = 300;
/// Lifetime of an approval nonce (one render of the consent page).
pub const NONCE_TTL_SECS: i64 = 300;

const REQUEST_COLUMNS: &str = "id, code_challenge, redirect_kind, redirect_host, redirect_port, \
     client_state, public_key, device_name, requester_ip, created_at, expires_at, decided_at, \
     decision, user_id, user_name, code_expires_at, consumed_at";

/// A `cli_auth_requests` row, without the nonce and code digests.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CliAuthRequestRow {
    pub id: String,
    pub code_challenge: String,
    /// `loopback` or `remote`.
    pub redirect_kind: String,
    /// `127.0.0.1` or `[::1]` for loopback.
    pub redirect_host: Option<String>,
    pub redirect_port: Option<i32>,
    pub client_state: String,
    pub public_key: String,
    pub device_name: String,
    pub requester_ip: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    /// `approved` or `denied` once decided.
    pub decision: Option<String>,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub code_expires_at: Option<DateTime<Utc>>,
    pub consumed_at: Option<DateTime<Utc>>,
}

impl CliAuthRequestRow {
    pub fn is_remote(&self) -> bool {
        self.redirect_kind == "remote"
    }
}

/// Values for a new request.
#[derive(Debug, Clone)]
pub struct NewCliAuthRequest<'a> {
    pub id: &'a str,
    pub code_challenge: &'a str,
    /// `(host, port)` for loopback, `None` for remote.
    pub loopback: Option<(&'a str, u16)>,
    pub client_state: &'a str,
    pub public_key: &'a str,
    pub device_name: &'a str,
    pub requester_ip: Option<&'a str>,
    pub now: DateTime<Utc>,
}

pub async fn insert_request(
    pool: &DbPool,
    request: &NewCliAuthRequest<'_>,
) -> Result<(), sqlx::Error> {
    let (kind, host, port) = match request.loopback {
        Some((host, port)) => ("loopback", Some(host), Some(i32::from(port))),
        None => ("remote", None, None),
    };
    sqlx::query(
        "INSERT INTO cli_auth_requests (id, code_challenge, redirect_kind, redirect_host, \
           redirect_port, client_state, public_key, device_name, requester_ip, created_at, \
           expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(request.id)
    .bind(request.code_challenge)
    .bind(kind)
    .bind(host)
    .bind(port)
    .bind(request.client_state)
    .bind(request.public_key)
    .bind(request.device_name)
    .bind(request.requester_ip)
    .bind(request.now)
    .bind(request.now + Duration::seconds(REQUEST_TTL_SECS))
    .execute(pool)
    .await?;
    Ok(())
}

/// Unexpired requests, globally or for one requester IP (approximate caps).
pub async fn count_open_requests(
    pool: &DbPool,
    now: DateTime<Utc>,
    requester_ip: Option<&str>,
) -> Result<i64, sqlx::Error> {
    let base = "SELECT COUNT(*) FROM cli_auth_requests WHERE expires_at > $1";
    match requester_ip {
        None => {
            sqlx::query_scalar::<_, i64>(base)
                .bind(now)
                .fetch_one(pool)
                .await
        }
        Some(ip) => {
            sqlx::query_scalar::<_, i64>(&format!("{base} AND requester_ip = $2"))
                .bind(now)
                .bind(ip)
                .fetch_one(pool)
                .await
        }
    }
}

/// Mint the approval nonce for one render of the consent page by `viewer`.
/// Returns the request if it is open (undecided, unexpired); the last
/// render wins.
pub async fn set_approval_nonce(
    pool: &DbPool,
    id: &str,
    nonce_hash: &str,
    viewer_user_id: &str,
    now: DateTime<Utc>,
) -> Result<Option<CliAuthRequestRow>, sqlx::Error> {
    sqlx::query_as::<_, CliAuthRequestRow>(&format!(
        "UPDATE cli_auth_requests SET approval_nonce_hash = $2, approval_nonce_user_id = $3, \
           approval_nonce_expires_at = $4 \
         WHERE id = $1 AND decided_at IS NULL AND expires_at > $5 \
         RETURNING {REQUEST_COLUMNS}"
    ))
    .bind(id)
    .bind(nonce_hash)
    .bind(viewer_user_id)
    .bind(now + Duration::seconds(NONCE_TTL_SECS))
    .bind(now)
    .fetch_optional(pool)
    .await
}

/// An open request by id (read-only; used to pick the consent rules).
pub async fn find_open_request(
    pool: &DbPool,
    id: &str,
    now: DateTime<Utc>,
) -> Result<Option<CliAuthRequestRow>, sqlx::Error> {
    sqlx::query_as::<_, CliAuthRequestRow>(&format!(
        "SELECT {REQUEST_COLUMNS} FROM cli_auth_requests \
         WHERE id = $1 AND decided_at IS NULL AND expires_at > $2"
    ))
    .bind(id)
    .bind(now)
    .fetch_optional(pool)
    .await
}

/// Who decides, with which nonce.
#[derive(Debug, Clone)]
pub struct ConsentDecision<'a> {
    pub request_id: &'a str,
    pub nonce_hash: &'a str,
    pub user_id: &'a str,
    pub user_name: &'a str,
    pub now: DateTime<Utc>,
}

/// The nonce-bound predicate shared by approve and deny ($1..$4).
const DECISION_PREDICATE: &str = "id = $1 AND decided_at IS NULL AND expires_at > $4 \
     AND approval_nonce_hash = $2 AND approval_nonce_user_id = $3 \
     AND approval_nonce_expires_at > $4";

/// Approve: attach the code digest and its expiry, clear the nonce. `None`
/// if the request is expired, decided, or the nonce/user does not match.
pub async fn approve_request(
    pool: &DbPool,
    decision: &ConsentDecision<'_>,
    code_hash: &str,
    code_expires_at: DateTime<Utc>,
) -> Result<Option<CliAuthRequestRow>, sqlx::Error> {
    sqlx::query_as::<_, CliAuthRequestRow>(&format!(
        "UPDATE cli_auth_requests SET decision = 'approved', decided_at = $4, user_id = $3, \
           user_name = $5, code_hash = $6, code_expires_at = $7, approval_nonce_hash = NULL, \
           approval_nonce_user_id = NULL, approval_nonce_expires_at = NULL \
         WHERE {DECISION_PREDICATE} \
         RETURNING {REQUEST_COLUMNS}"
    ))
    .bind(decision.request_id)
    .bind(decision.nonce_hash)
    .bind(decision.user_id)
    .bind(decision.now)
    .bind(decision.user_name)
    .bind(code_hash)
    .bind(code_expires_at)
    .fetch_optional(pool)
    .await
}

/// Deny: same predicate as approval, no code.
pub async fn deny_request(
    pool: &DbPool,
    decision: &ConsentDecision<'_>,
) -> Result<Option<CliAuthRequestRow>, sqlx::Error> {
    sqlx::query_as::<_, CliAuthRequestRow>(&format!(
        "UPDATE cli_auth_requests SET decision = 'denied', decided_at = $4, user_id = $3, \
           user_name = $5, approval_nonce_hash = NULL, approval_nonce_user_id = NULL, \
           approval_nonce_expires_at = NULL \
         WHERE {DECISION_PREDICATE} \
         RETURNING {REQUEST_COLUMNS}"
    ))
    .bind(decision.request_id)
    .bind(decision.nonce_hash)
    .bind(decision.user_id)
    .bind(decision.now)
    .bind(decision.user_name)
    .fetch_optional(pool)
    .await
}

/// Consume a one-time code: one atomic UPDATE, so a code is accepted at most
/// once. It stays burned whatever happens afterwards.
pub async fn consume_code(
    pool: &DbPool,
    code_hash: &str,
    now: DateTime<Utc>,
) -> Result<Option<CliAuthRequestRow>, sqlx::Error> {
    sqlx::query_as::<_, CliAuthRequestRow>(&format!(
        "UPDATE cli_auth_requests SET consumed_at = $2 \
         WHERE code_hash = $1 AND decision = 'approved' AND consumed_at IS NULL \
           AND code_expires_at > $2 \
         RETURNING {REQUEST_COLUMNS}"
    ))
    .bind(code_hash)
    .bind(now)
    .fetch_optional(pool)
    .await
}

/// Bounded sweep of requests more than an hour past their expiry (their
/// codes expired long before). Deletes at most [`SWEEP_BATCH`] rows.
pub async fn sweep_requests(pool: &DbPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "DELETE FROM cli_auth_requests WHERE id IN (SELECT id FROM cli_auth_requests \
           WHERE expires_at < $1 LIMIT $2)",
    )
    .bind(now - Duration::hours(1))
    .bind(SWEEP_BATCH)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

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

//! Handler for console log uploads from browser clients.
//!
//! Accepts periodic chunks of newline-delimited JSON console output captured by
//! the browser-side console-log-collector. Each chunk is written atomically to
//! a unique file on disk, organized by `{meeting_id}/{YYYY-MM-DD}/`.
//!
//! The feature is gated by the `CONSOLE_LOG_UPLOAD_ENABLED` env var (must be
//! `"true"`) — when disabled, the endpoint returns 404. This provides a
//! server-side kill switch that complements the client-side config flag.

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use chrono::{Datelike, Utc};
use flate2::write::GzEncoder;
use flate2::Compression;
use regex::Regex;
use std::io::Write;
use tokio::io::AsyncWriteExt;
use tracing;

use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::{meetings as db_meetings, participants as db_participants};
use crate::error::AppError;
use crate::state::AppState;

/// Query parameters accepted as a fallback for `navigator.sendBeacon()` which
/// cannot set custom request headers. The primary upload path uses headers;
/// these are only used when headers are absent.
#[derive(Debug, Deserialize, Default)]
pub struct ConsoleLogQuery {
    pub user_id: Option<String>,
    pub session_ts: Option<String>,
}

/// Maximum body size for a single console log chunk (1 MB).
pub const MAX_BODY_SIZE: usize = 1_048_576;

/// Default base directory for console log storage.
pub(crate) const DEFAULT_LOG_DIR: &str = "/data/console-logs";

/// Env var that gates both the upload endpoint and the in-process purge task.
/// When set to `"true"`, uploads are accepted and the purge scheduler runs.
pub(crate) const CONSOLE_LOG_UPLOAD_ENABLED_ENV: &str = "CONSOLE_LOG_UPLOAD_ENABLED";

/// Env var overriding the on-disk base directory for console log storage.
pub(crate) const CONSOLE_LOG_DIR_ENV: &str = "CONSOLE_LOG_DIR";

/// Env var controlling the retention window (in days) used by the purge task.
pub(crate) const CONSOLE_LOG_RETENTION_DAYS_ENV: &str = "CONSOLE_LOG_RETENTION_DAYS";

/// Default retention window (in days) used when `CONSOLE_LOG_RETENTION_DAYS`
/// is unset or unparseable.
pub(crate) const DEFAULT_RETENTION_DAYS: u32 = 2;

/// Default per-user daily upload quota: 500 MB. Override with
/// `CONSOLE_LOG_USER_QUOTA_BYTES` env var.
///
/// The quota is measured in **raw wire bytes** before server-side gzip, so the
/// on-disk cost is ~8-10× smaller. 500 MB/day comfortably covers several long
/// problem sessions where chatty logging (reconnects, AQ swings, PLI storms)
/// can drive ~15-40 MB/hour while still tripping on a runaway `console.log`
/// loop quickly enough to signal the issue via a 429 + `warn!`. Do not halve
/// this without re-reading the discussion in the change that set it — a
/// too-low cap silently cuts off logs precisely when they are most valuable.
const DEFAULT_USER_QUOTA_BYTES: u64 = 500 * 1024 * 1024;

/// Per-user daily byte counter for rate limiting console log uploads.
/// Key: user_id, Value: (day-of-year ordinal, bytes uploaded today).
/// Uses `std::sync::Mutex` — the critical section is a HashMap lookup + u64 add,
/// so there's no risk of holding the lock across `.await`.
///
/// Entries are never evicted — the counter resets on day rollover but the key
/// persists. At ~80 bytes/entry and ~300 distinct users/day (design target of
/// 15 meetings × 20 users), memory growth is negligible over typical pod lifetime.
static UPLOAD_QUOTAS: LazyLock<Mutex<HashMap<String, (u32, u64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Minimum seconds between "disk full" log lines emitted from the upload
/// handler. A wedged PVC can otherwise cause hundreds of `error!` lines per
/// minute at design scale (300 concurrent users × ~1 chunk / 30s each).
const DISK_FULL_LOG_DEDUP_SECS: u64 = 60;

/// Unix timestamp (seconds) of the most recent "disk full" log emitted by the
/// upload handler. Paired with [`DISK_FULL_SUPPRESSED_COUNT`] to produce a
/// single "N additional occurrences suppressed" line per window.
static LAST_DISK_FULL_LOG_UNIX: AtomicU64 = AtomicU64::new(0);

/// Count of disk-full errors suppressed since the last emitted log line. Read
/// and reset atomically when the next line is emitted.
static DISK_FULL_SUPPRESSED_COUNT: AtomicU64 = AtomicU64::new(0);

/// Linux `EDQUOT` errno. Used to detect filesystem-quota-exceeded errors since
/// `std::io::ErrorKind::FilesystemQuotaExceeded` is not yet stable on the
/// toolchain this crate builds against (tracked under `io_error_more`).
#[cfg(target_os = "linux")]
const EDQUOT: i32 = 122;

/// Returns a `Some(count)` of previously-suppressed events if this error should
/// emit a log line, or `None` if it should be rate-limited. For non-disk-full
/// errors, always returns `Some(0)` (always log). For `StorageFull` or a
/// filesystem-quota-exceeded error (Linux `EDQUOT`), returns `Some(N)` at most
/// once per `DISK_FULL_LOG_DEDUP_SECS` window, where N is the number of
/// suppressed events since the last emitted line.
fn classify_io_error_for_logging(err: &std::io::Error) -> Option<u64> {
    let is_storage_full = matches!(err.kind(), std::io::ErrorKind::StorageFull);
    // `FilesystemQuotaExceeded` is still nightly-only — detect EDQUOT from the
    // raw OS error on Linux. On other platforms this check is a no-op.
    #[cfg(target_os = "linux")]
    let is_quota = err.raw_os_error() == Some(EDQUOT);
    #[cfg(not(target_os = "linux"))]
    let is_quota = false;
    let is_disk_full = is_storage_full || is_quota;
    if !is_disk_full {
        return Some(0);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_DISK_FULL_LOG_UNIX.load(Ordering::Relaxed);
    if now.saturating_sub(last) < DISK_FULL_LOG_DEDUP_SECS {
        DISK_FULL_SUPPRESSED_COUNT.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // Race between multiple threads is acceptable — the worst case is a small
    // burst of ENOSPC logs at the boundary of each window rather than strictly
    // one line. A CAS loop would guarantee strictly-one but is not worth the
    // complexity for this purpose.
    LAST_DISK_FULL_LOG_UNIX.store(now, Ordering::Relaxed);
    let suppressed = DISK_FULL_SUPPRESSED_COUNT.swap(0, Ordering::Relaxed);
    Some(suppressed)
}

/// Check and update the per-user daily byte quota. Returns `Err(429)` if exceeded.
fn check_upload_quota(user_id: &str, body_len: u64) -> Result<(), AppError> {
    let quota = std::env::var("CONSOLE_LOG_USER_QUOTA_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_USER_QUOTA_BYTES);

    let today = Utc::now().ordinal();
    let mut quotas = UPLOAD_QUOTAS.lock().unwrap_or_else(|e| e.into_inner());
    let entry = quotas.entry(user_id.to_string()).or_insert((today, 0));

    // Reset counter on day rollover.
    if entry.0 != today {
        *entry = (today, 0);
    }

    if entry.1.saturating_add(body_len) > quota {
        tracing::warn!(
            user_id = %user_id,
            bytes_today = entry.1,
            body_len = body_len,
            quota = quota,
            "Console log upload quota exceeded"
        );
        return Err(AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            videocall_meeting_types::APIError {
                code: "RATE_LIMITED".to_string(),
                message: "Daily upload quota exceeded".to_string(),
                engineering_error: None,
            },
        ));
    }

    entry.1 = entry.1.saturating_add(body_len);
    Ok(())
}

/// Meeting IDs: alphanumeric, hyphens, and underscores only.
static SAFE_MEETING_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_-]+$").expect("valid regex"));

/// User IDs: also allow dots and `@` for OAuth email addresses.
static SAFE_USER_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_.@-]+$").expect("valid regex"));

/// Validate that an identifier contains only safe characters.
fn validate_id(value: &str, field_name: &str, re: &Regex) -> Result<(), AppError> {
    if value.is_empty() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            videocall_meeting_types::APIError {
                code: "INVALID_PARAMETER".to_string(),
                message: format!("{field_name} cannot be empty"),
                engineering_error: None,
            },
        ));
    }
    if value.len() > 255 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            videocall_meeting_types::APIError {
                code: "INVALID_PARAMETER".to_string(),
                message: format!("{field_name} cannot exceed 255 characters"),
                engineering_error: None,
            },
        ));
    }
    if !re.is_match(value) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            videocall_meeting_types::APIError {
                code: "INVALID_PARAMETER".to_string(),
                message: format!("{field_name} contains invalid characters"),
                engineering_error: None,
            },
        ));
    }
    Ok(())
}

/// POST /api/v1/meetings/{meeting_id}/console-logs
///
/// Accepts a chunk of console log data (text/plain, newline-delimited JSON)
/// and writes it to disk. Each chunk is stored as a separate file to avoid
/// race conditions between periodic flushes and `sendBeacon` uploads.
///
/// # Headers
///
/// - `X-User-Id` — identifies the participant (required)
/// - `X-Session-Timestamp` — epoch ms timestamp unique to this join session (required)
///
/// # Gating
///
/// Returns 404 unless `CONSOLE_LOG_UPLOAD_ENABLED` env var is `"true"`.
pub async fn upload_console_logs(
    AuthUser {
        user_id: auth_user_id,
        ..
    }: AuthUser,
    State(state): State<AppState>,
    Path(meeting_id): Path<String>,
    Query(query): Query<ConsoleLogQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, AppError> {
    // --- Feature gate ---
    let enabled = std::env::var(CONSOLE_LOG_UPLOAD_ENABLED_ENV).unwrap_or_default();
    if enabled != "true" {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            videocall_meeting_types::APIError {
                code: "NOT_FOUND".to_string(),
                message: "Not found".to_string(),
                engineering_error: None,
            },
        ));
    }

    // --- Extract user_id and session_ts ---
    // Primary: custom headers (used by fetch with keepalive). Optional: the
    // sendBeacon fallback cannot set headers, so user_id falls back to the
    // auth JWT identity and session_ts falls back to the current epoch ms.
    let user_id = headers
        .get("X-User-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or(query.user_id);

    let session_ts = headers
        .get("X-Session-Timestamp")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or(query.session_ts)
        .unwrap_or_else(|| Utc::now().timestamp_millis().to_string());

    // Optional chunk sequence number (1–99999). When present, produces
    // zero-padded filenames that sort chronologically in any file manager.
    // sendBeacon can't set headers, so this is absent for beacon fallback
    // uploads — those fall back to UUIDv7 suffixes.
    let chunk_seq: Option<u32> = headers
        .get("X-Chunk-Seq")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&n| (1..=99999).contains(&n));

    // --- Validate meeting_id path-safety ---
    validate_id(&meeting_id, "meeting_id", &SAFE_MEETING_ID_RE)?;

    // Session timestamp must be numeric (epoch ms).
    if !session_ts.chars().all(|c| c.is_ascii_digit()) || session_ts.is_empty() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            videocall_meeting_types::APIError {
                code: "INVALID_PARAMETER".to_string(),
                message: "X-Session-Timestamp must be a numeric epoch millisecond value"
                    .to_string(),
                engineering_error: None,
            },
        ));
    }

    // --- Identity resolution ---
    // Use the authenticated identity from the JWT when available. This
    // prevents clients from self-asserting an arbitrary X-User-Id.
    // When auth_user_id is non-empty, we use it as the canonical user_id
    // for the filename and log any mismatch.
    //
    // Anonymous fallback: when auth_user_id is empty (anonymous/guest
    // sessions using room-token-only auth), the client-supplied X-User-Id
    // header is accepted. This is safe because the downstream membership
    // check verifies the user_id has a participant row for this meeting —
    // an attacker would need both a valid room token AND a participant row
    // under the claimed identity.
    let user_id = if !auth_user_id.is_empty() {
        if let Some(ref header_uid) = user_id {
            if *header_uid != auth_user_id {
                tracing::warn!(
                    auth_user_id = %auth_user_id,
                    header_user_id = %header_uid,
                    "Console log upload: using auth identity instead of X-User-Id header"
                );
            }
        }
        auth_user_id
    } else {
        user_id.ok_or_else(|| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                videocall_meeting_types::APIError {
                    code: "MISSING_HEADER".to_string(),
                    message: "X-User-Id header is required for unauthenticated sessions"
                        .to_string(),
                    engineering_error: None,
                },
            )
        })?
    };

    // Validate the resolved user_id for path-safety.
    validate_id(&user_id, "user_id", &SAFE_USER_ID_RE)?;

    // --- Meeting membership check ---
    // Verify that the caller is (or was) a participant of this meeting.
    // The meeting_id path parameter is actually a room_id string — resolve
    // it to the integer meeting.id, then check the participants table.
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to look up meeting for console log upload");
            AppError::internal("Failed to verify meeting membership")
        })?
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                videocall_meeting_types::APIError {
                    code: "NOT_FOUND".to_string(),
                    message: "Meeting not found".to_string(),
                    engineering_error: None,
                },
            )
        })?;

    let participant = db_participants::get_status(&state.db, meeting.id, &user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to check participant status for console log upload");
            AppError::internal("Failed to verify meeting membership")
        })?;

    if participant.is_none() {
        tracing::warn!(
            meeting_id = %meeting_id,
            user_id = %user_id,
            "Console log upload rejected: user is not a participant of this meeting"
        );
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            videocall_meeting_types::APIError {
                code: "FORBIDDEN".to_string(),
                message: "You are not a participant of this meeting".to_string(),
                engineering_error: None,
            },
        ));
    }

    // --- Body size check ---
    if body.len() > MAX_BODY_SIZE {
        return Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            videocall_meeting_types::APIError {
                code: "PAYLOAD_TOO_LARGE".to_string(),
                message: format!(
                    "Request body exceeds maximum size of {} bytes",
                    MAX_BODY_SIZE
                ),
                engineering_error: None,
            },
        ));
    }

    // --- Per-user daily quota check ---
    check_upload_quota(&user_id, body.len() as u64)?;

    // --- Generate chunk filename ---
    // When a chunk sequence number is provided, zero-pad to 5 digits so files
    // sort chronologically in any file manager (including macOS Finder which
    // uses "natural sort" that breaks on mixed hex UUIDv7 suffixes).
    // Fallback: sendBeacon can't set headers, so beacon uploads get a UUIDv7
    // suffix — these sort after numbered chunks, which is acceptable.
    let chunk_suffix = if let Some(seq) = chunk_seq {
        format!("{seq:05}")
    } else {
        let uuid_v7 = uuid::Uuid::now_v7();
        uuid_v7.simple().to_string()[..16].to_string()
    };
    let filename = format!("{user_id}_{session_ts}_{chunk_suffix}.log.gz");

    // --- Create directory and write file ---
    let base_dir =
        std::env::var(CONSOLE_LOG_DIR_ENV).unwrap_or_else(|_| DEFAULT_LOG_DIR.to_string());
    let date_str = Utc::now().format("%Y-%m-%d").to_string();
    let dir_path = std::path::PathBuf::from(&base_dir)
        .join(&meeting_id)
        .join(&date_str);

    tokio::fs::create_dir_all(&dir_path).await.map_err(|e| {
        if let Some(suppressed) = classify_io_error_for_logging(&e) {
            tracing::error!(
                path = %dir_path.display(),
                error = %e,
                error_kind = ?e.kind(),
                suppressed_disk_full = suppressed,
                "Failed to create console log directory"
            );
        }
        AppError::internal("Failed to store console log chunk")
    })?;

    // Set directory permissions to 0700 (owner only).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&dir_path, std::fs::Permissions::from_mode(0o700)).await;
    }

    // --- Symlink confinement check ---
    // Canonicalize the directory to resolve any symlinks, then verify it still
    // lives under the configured base_dir. This prevents a pre-planted symlink
    // from redirecting writes outside the log directory (TOCTOU defense).
    let canonical_dir = tokio::fs::canonicalize(&dir_path).await.map_err(|e| {
        if let Some(suppressed) = classify_io_error_for_logging(&e) {
            tracing::error!(
                path = %dir_path.display(),
                error = %e,
                error_kind = ?e.kind(),
                suppressed_disk_full = suppressed,
                "Failed to canonicalize console log directory"
            );
        }
        AppError::internal("Failed to store console log chunk")
    })?;
    let canonical_base = tokio::fs::canonicalize(&base_dir).await.map_err(|e| {
        if let Some(suppressed) = classify_io_error_for_logging(&e) {
            tracing::error!(
                path = %base_dir,
                error = %e,
                error_kind = ?e.kind(),
                suppressed_disk_full = suppressed,
                "Failed to canonicalize console log base directory"
            );
        }
        AppError::internal("Failed to store console log chunk")
    })?;
    if !canonical_dir.starts_with(&canonical_base) {
        tracing::error!(
            dir = %canonical_dir.display(),
            base = %canonical_base.display(),
            "Console log directory escapes base path — possible symlink attack"
        );
        return Err(AppError::internal("Failed to store console log chunk"));
    }

    let file_path = canonical_dir.join(&filename);

    // Use create_new(true) for O_CREAT | O_EXCL semantics — guarantees no
    // overwrites. UUID v7 makes collisions astronomically unlikely, but this
    // provides defense in depth.
    //
    // O_NOFOLLOW: refuse to open if the target is a symlink.
    // mode(0o600): set permissions at open time (not after write) to avoid
    // a window where the file exists at umask-derived permissions.
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match opts.open(&file_path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            tracing::error!(
                path = %file_path.display(),
                user_id = %user_id,
                meeting_id = %meeting_id,
                "Console log chunk filename collision (file already exists)"
            );
            return Err(AppError::new(
                StatusCode::CONFLICT,
                videocall_meeting_types::APIError {
                    code: "CONFLICT".to_string(),
                    message: "Chunk already exists".to_string(),
                    engineering_error: None,
                },
            ));
        }
        Err(e) => {
            if let Some(suppressed) = classify_io_error_for_logging(&e) {
                tracing::error!(
                    path = %file_path.display(),
                    error = %e,
                    error_kind = ?e.kind(),
                    suppressed_disk_full = suppressed,
                    "Failed to create console log file"
                );
            }
            return Err(AppError::internal("Failed to store console log chunk"));
        }
    };

    let compressed = tokio::task::spawn_blocking({
        let body = body.to_vec();
        move || {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(&body)?;
            encoder.finish()
        }
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "Gzip compression task panicked");
        AppError::internal("Failed to store console log chunk")
    })?
    .map_err(|e| {
        tracing::error!(error = %e, "Gzip compression failed");
        AppError::internal("Failed to store console log chunk")
    })?;

    file.write_all(&compressed).await.map_err(|e| {
        if let Some(suppressed) = classify_io_error_for_logging(&e) {
            tracing::error!(
                path = %file_path.display(),
                error = %e,
                error_kind = ?e.kind(),
                suppressed_disk_full = suppressed,
                "Failed to write console log data"
            );
        }
        AppError::internal("Failed to store console log chunk")
    })?;

    file.flush().await.map_err(|e| {
        tracing::error!(
            path = %file_path.display(),
            error = %e,
            "Failed to flush console log file"
        );
        AppError::internal("Failed to store console log chunk")
    })?;

    tracing::debug!(
        meeting_id = %meeting_id,
        user_id = %user_id,
        file = %filename,
        bytes = body.len(),
        "Console log chunk written"
    );

    Ok(StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Meeting ID validation (restrictive: [a-zA-Z0-9_-]) ---

    #[test]
    fn meeting_id_accepts_alphanumeric() {
        assert!(validate_id("daily-standup", "meeting_id", &SAFE_MEETING_ID_RE).is_ok());
    }

    #[test]
    fn meeting_id_rejects_dots_and_slashes() {
        assert!(validate_id("../etc/passwd", "meeting_id", &SAFE_MEETING_ID_RE).is_err());
        assert!(validate_id("room.name", "meeting_id", &SAFE_MEETING_ID_RE).is_err());
    }

    #[test]
    fn meeting_id_rejects_at_sign() {
        assert!(validate_id("room@host", "meeting_id", &SAFE_MEETING_ID_RE).is_err());
    }

    // --- User ID validation (allows dots and @ for OAuth emails) ---

    #[test]
    fn user_id_accepts_alphanumeric() {
        assert!(validate_id("user123", "user_id", &SAFE_USER_ID_RE).is_ok());
    }

    #[test]
    fn user_id_accepts_hyphens_and_underscores() {
        assert!(validate_id("my-user_id-123", "user_id", &SAFE_USER_ID_RE).is_ok());
    }

    #[test]
    fn user_id_accepts_email() {
        assert!(validate_id("alice@example.com", "user_id", &SAFE_USER_ID_RE).is_ok());
        assert!(validate_id("jay.boyd@test.io", "user_id", &SAFE_USER_ID_RE).is_ok());
    }

    #[test]
    fn user_id_rejects_slashes() {
        assert!(validate_id("../etc/passwd", "user_id", &SAFE_USER_ID_RE).is_err());
        assert!(validate_id("user/name", "user_id", &SAFE_USER_ID_RE).is_err());
    }

    #[test]
    fn user_id_rejects_spaces() {
        assert!(validate_id("user name", "user_id", &SAFE_USER_ID_RE).is_err());
    }

    // --- Shared validation behavior ---

    #[test]
    fn rejects_empty() {
        assert!(validate_id("", "test", &SAFE_MEETING_ID_RE).is_err());
        assert!(validate_id("", "test", &SAFE_USER_ID_RE).is_err());
    }

    #[test]
    fn rejects_too_long() {
        let long = "a".repeat(256);
        assert!(validate_id(&long, "test", &SAFE_MEETING_ID_RE).is_err());
    }

    #[test]
    fn accepts_max_length() {
        let max = "a".repeat(255);
        assert!(validate_id(&max, "test", &SAFE_MEETING_ID_RE).is_ok());
    }

    // --- Rate-limited ENOSPC logging helper ---

    /// Resets the rate-limiter's global state. Required because the helper
    /// reads/writes process-wide statics and tests share that state.
    fn reset_disk_full_rate_limiter() {
        LAST_DISK_FULL_LOG_UNIX.store(0, Ordering::Relaxed);
        DISK_FULL_SUPPRESSED_COUNT.store(0, Ordering::Relaxed);
    }

    #[test]
    #[serial_test::serial]
    fn classify_io_error_non_disk_full_always_logs() {
        reset_disk_full_rate_limiter();
        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        // Non-disk-full errors should always return Some(0).
        assert_eq!(classify_io_error_for_logging(&err), Some(0));
        assert_eq!(classify_io_error_for_logging(&err), Some(0));
        assert_eq!(classify_io_error_for_logging(&err), Some(0));
        // And must never increment the suppressed counter.
        assert_eq!(DISK_FULL_SUPPRESSED_COUNT.load(Ordering::Relaxed), 0);
    }

    #[test]
    #[serial_test::serial]
    fn classify_io_error_disk_full_is_rate_limited() {
        reset_disk_full_rate_limiter();
        let err = std::io::Error::from(std::io::ErrorKind::StorageFull);

        // First call in a fresh window emits (suppressed = 0).
        assert_eq!(classify_io_error_for_logging(&err), Some(0));

        // Subsequent calls within the window are suppressed.
        assert_eq!(classify_io_error_for_logging(&err), None);
        assert_eq!(classify_io_error_for_logging(&err), None);
        assert_eq!(classify_io_error_for_logging(&err), None);

        // Simulate the window having elapsed by backdating the last-log
        // timestamp beyond DISK_FULL_LOG_DEDUP_SECS. This avoids sleeping.
        LAST_DISK_FULL_LOG_UNIX.store(1, Ordering::Relaxed);

        // Next call emits and reports the 3 suppressed events.
        assert_eq!(classify_io_error_for_logging(&err), Some(3));

        // Counter resets after emitting.
        assert_eq!(DISK_FULL_SUPPRESSED_COUNT.load(Ordering::Relaxed), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[serial_test::serial]
    fn classify_io_error_filesystem_quota_exceeded_is_rate_limited() {
        reset_disk_full_rate_limiter();
        // `FilesystemQuotaExceeded` is not stable on this toolchain — construct
        // the error from the raw Linux EDQUOT errno, which is what the helper
        // matches on.
        let err = std::io::Error::from_raw_os_error(EDQUOT);

        assert_eq!(classify_io_error_for_logging(&err), Some(0));
        assert_eq!(classify_io_error_for_logging(&err), None);
    }
}

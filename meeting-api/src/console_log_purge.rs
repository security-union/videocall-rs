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

//! In-process console-log retention / purge task.
//!
//! Replaces an external Kubernetes `CronJob` that previously ran `find … -mtime +N -delete`
//! on the `console-logs` PVC. Running the purge in-process avoids the multi-attach problem
//! on a `ReadWriteOnce` PVC (only the `meeting-api` pod can mount the volume) and ensures
//! the purge runs as the same UID that wrote the files.
//!
//! ## Scheduling
//!
//! The scheduler fires once per calendar day at **00:00 UTC**. Rather than using a fixed
//! 86400s Tokio interval, it recomputes the duration to the next UTC midnight on every
//! iteration. This keeps the task self-healing against node suspend/resume and any
//! long-running filesystem walk that overruns the previous day.
//!
//! ## Gating
//!
//! The scheduler is gated on [`CONSOLE_LOG_UPLOAD_ENABLED_ENV`] — the same env var that
//! gates the upload endpoint. When disabled, [`spawn_purge_task`] returns `None` and no
//! task is spawned.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Duration as ChronoDuration, TimeZone, Utc};

use crate::routes::console_logs::{
    CONSOLE_LOG_DIR_ENV, CONSOLE_LOG_RETENTION_DAYS_ENV, CONSOLE_LOG_UPLOAD_ENABLED_ENV,
    DEFAULT_LOG_DIR, DEFAULT_RETENTION_DAYS,
};

/// ext4 reserved directory created at the root of every ext4 filesystem. The purge
/// walker must skip it at any depth so it never tries to delete or recurse into it.
const LOST_FOUND: &str = "lost+found";

/// Summary statistics returned by a single purge pass. Used for structured logging and tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PurgeSummary {
    /// Number of files deleted during the pass.
    pub files_deleted: u64,
    /// Total bytes reclaimed (sum of file sizes for deleted files).
    pub bytes_reclaimed: u64,
    /// Number of empty directories removed (bottom-up, after file deletion).
    pub dirs_removed: u64,
    /// Number of recoverable errors encountered (per-file or per-dir) during the pass.
    /// Errors are logged at `warn` and do not abort the pass.
    pub errors: u64,
}

/// Run a single purge pass over `base`, deleting files with an mtime older than
/// `now - retention_days * 86400s`, then pruning any empty directories bottom-up.
///
/// Only files matching `*.log` or `*.log.gz` are eligible for deletion — unrelated
/// files left in the volume are never touched. The `lost+found` directory (created
/// at the root of every ext4 filesystem) is skipped at any depth.
///
/// Per-file and per-dir errors are logged at `warn` and counted in
/// [`PurgeSummary::errors`]; the pass continues. If `base` itself is missing, the
/// function logs at `warn` and returns an empty summary without creating the dir
/// (the upload route handler creates it on first write).
///
/// This is a synchronous function intended to be called via
/// [`tokio::task::spawn_blocking`] from the scheduler.
pub(crate) fn purge_once(base: &Path, retention_days: u32, now: SystemTime) -> PurgeSummary {
    let mut summary = PurgeSummary::default();

    match base.symlink_metadata() {
        Ok(md) if md.is_dir() => {}
        Ok(_) => {
            tracing::warn!(
                path = %base.display(),
                "Console log purge: base path exists but is not a directory; skipping"
            );
            return summary;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            tracing::warn!(
                path = %base.display(),
                "Console log purge: base directory does not exist yet; skipping this run"
            );
            return summary;
        }
        Err(e) => {
            tracing::warn!(
                path = %base.display(),
                error = %e,
                "Console log purge: failed to stat base directory; skipping this run"
            );
            summary.errors = summary.errors.saturating_add(1);
            return summary;
        }
    }

    // `retention_days == 0` means "delete everything older than now", which is useful
    // for tests and manual zero-retention runs. Compute the cutoff as now - retention.
    let retention = Duration::from_secs(u64::from(retention_days) * 86_400);
    let cutoff = now.checked_sub(retention).unwrap_or(UNIX_EPOCH);

    walk_and_delete(base, cutoff, &mut summary);
    summary
}

/// Recursively walks `dir`, deleting eligible files whose mtime is older than `cutoff`
/// and pruning empty directories bottom-up. Never follows symlinks and never descends
/// into `lost+found`. Errors are counted but never abort the walk.
fn walk_and_delete(dir: &Path, cutoff: SystemTime, summary: &mut PurgeSummary) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Concurrent removal is not an error.
            return;
        }
        Err(e) => {
            tracing::warn!(
                path = %dir.display(),
                error = %e,
                "Console log purge: failed to read directory"
            );
            summary.errors = summary.errors.saturating_add(1);
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    path = %dir.display(),
                    error = %e,
                    "Console log purge: failed to read directory entry"
                );
                summary.errors = summary.errors.saturating_add(1);
                continue;
            }
        };

        let path = entry.path();

        // Skip the ext4 reserved dir at any depth. file_name() is a borrow into the
        // entry struct, so this incurs no allocation.
        if entry.file_name() == LOST_FOUND {
            continue;
        }

        // `symlink_metadata` does NOT follow symlinks — we never want the walker to
        // leave the base tree through a planted symlink.
        let md = match entry.path().symlink_metadata() {
            Ok(md) => md,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Console log purge: failed to stat entry"
                );
                summary.errors = summary.errors.saturating_add(1);
                continue;
            }
        };
        let file_type = md.file_type();

        if file_type.is_dir() {
            walk_and_delete(&path, cutoff, summary);

            // After recursing, try to remove the directory if it is now empty.
            match std::fs::read_dir(&path) {
                Ok(mut child_iter) => {
                    if child_iter.next().is_none() {
                        match std::fs::remove_dir(&path) {
                            Ok(()) => {
                                summary.dirs_removed = summary.dirs_removed.saturating_add(1);
                            }
                            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                            Err(e) => {
                                tracing::warn!(
                                    path = %path.display(),
                                    error = %e,
                                    "Console log purge: failed to remove empty directory"
                                );
                                summary.errors = summary.errors.saturating_add(1);
                            }
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Console log purge: failed to re-read directory after descent"
                    );
                    summary.errors = summary.errors.saturating_add(1);
                }
            }
        } else if file_type.is_file() || file_type.is_symlink() {
            // Only delete files whose name matches the uploader's naming scheme. We
            // treat symlinks the same as files for matching purposes — the uploader
            // never creates symlinks, but if a malicious actor planted one that
            // happens to match the suffix, removing it (via `remove_file`, which
            // unlinks the symlink itself) is harmless.
            if !is_purgeable_filename(&path) {
                continue;
            }

            let mtime = match md.modified() {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Console log purge: mtime unavailable; skipping"
                    );
                    summary.errors = summary.errors.saturating_add(1);
                    continue;
                }
            };

            // Clock skew / pre-epoch mtimes: if we cannot locate the mtime on the epoch
            // timeline, treat the age as unknown and skip. Prevents accidental mass
            // deletion if the filesystem reports something bogus.
            if mtime.duration_since(UNIX_EPOCH).is_err() {
                tracing::warn!(
                    path = %path.display(),
                    "Console log purge: mtime is before UNIX_EPOCH; skipping"
                );
                continue;
            }

            if mtime < cutoff {
                let size = md.len();
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        summary.files_deleted = summary.files_deleted.saturating_add(1);
                        summary.bytes_reclaimed = summary.bytes_reclaimed.saturating_add(size);
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "Console log purge: failed to remove file"
                        );
                        summary.errors = summary.errors.saturating_add(1);
                    }
                }
            }
        }
        // Other file types (sockets, devices, fifos, block) are intentionally ignored.
    }
}

/// Returns `true` for filenames with a `.log` or `.log.gz` suffix — matching the
/// uploader's output naming. Case-sensitive (the uploader always writes lowercase).
fn is_purgeable_filename(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.ends_with(".log") || name.ends_with(".log.gz")
}

/// Compute the [`Duration`] from `now` to the next UTC midnight (exclusive).
///
/// If `now` is already at 00:00:00.000 UTC, returns a full 24 hours to avoid
/// tight-looping the scheduler on the same instant.
fn duration_until_next_utc_midnight(now: chrono::DateTime<Utc>) -> Duration {
    let tomorrow = (now + ChronoDuration::days(1)).date_naive();
    let next_midnight =
        Utc.from_utc_datetime(&tomorrow.and_hms_opt(0, 0, 0).expect("00:00:00 is valid"));
    // `to_std()` converts a positive `chrono::Duration` to `std::time::Duration`.
    // It only fails on negative input — not possible here because `tomorrow` is
    // always strictly after `now`. Fall back to 1s as a defensive floor.
    (next_midnight - now)
        .to_std()
        .unwrap_or(Duration::from_secs(1))
}

/// Spawn the in-process console-log purge scheduler.
///
/// Returns `None` when `CONSOLE_LOG_UPLOAD_ENABLED` is unset or not `"true"` —
/// in which case no task is spawned and no filesystem work is done.
///
/// When enabled, spawns a Tokio task that runs one immediate [`purge_once`] pass
/// at startup, then sleeps until the next UTC midnight, runs another pass inside
/// [`tokio::task::spawn_blocking`], logs a structured summary, and repeats. The
/// task never returns; if the blocking pass panics it is logged and the scheduler
/// continues so a single bad run cannot disable retention.
pub fn spawn_purge_task() -> Option<tokio::task::JoinHandle<()>> {
    let enabled = std::env::var(CONSOLE_LOG_UPLOAD_ENABLED_ENV).unwrap_or_default();
    if enabled != "true" {
        return None;
    }

    let base_dir =
        std::env::var(CONSOLE_LOG_DIR_ENV).unwrap_or_else(|_| DEFAULT_LOG_DIR.to_string());
    let retention_days = match std::env::var(CONSOLE_LOG_RETENTION_DAYS_ENV) {
        Ok(v) => match v.parse::<u32>() {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    env_var = CONSOLE_LOG_RETENTION_DAYS_ENV,
                    value = %v,
                    error = %e,
                    default = DEFAULT_RETENTION_DAYS,
                    "Unparseable CONSOLE_LOG_RETENTION_DAYS; using default"
                );
                DEFAULT_RETENTION_DAYS
            }
        },
        Err(_) => DEFAULT_RETENTION_DAYS,
    };

    let base_path = PathBuf::from(&base_dir);

    tracing::info!(
        base_dir = %base_dir,
        retention_days,
        "Console log purge scheduler starting"
    );

    let handle = tokio::spawn(async move {
        let run_purge_pass = |base: PathBuf| {
            tokio::task::spawn_blocking(move || {
                purge_once(&base, retention_days, SystemTime::now())
            })
        };

        let log_summary = |summary: PurgeSummary, elapsed: std::time::Duration, startup: bool| {
            let message = if startup {
                if summary.errors > 0 {
                    "Startup console log purge completed with errors"
                } else {
                    "Startup console log purge complete"
                }
            } else if summary.errors > 0 {
                "Console log purge completed with errors"
            } else {
                "Console log purge complete"
            };

            if summary.errors > 0 {
                tracing::warn!(
                    base_dir = %base_dir,
                    retention_days,
                    files_deleted = summary.files_deleted,
                    bytes_reclaimed = summary.bytes_reclaimed,
                    dirs_removed = summary.dirs_removed,
                    errors = summary.errors,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "{message}"
                );
            } else {
                tracing::info!(
                    base_dir = %base_dir,
                    retention_days,
                    files_deleted = summary.files_deleted,
                    bytes_reclaimed = summary.bytes_reclaimed,
                    dirs_removed = summary.dirs_removed,
                    errors = summary.errors,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "{message}"
                );
            }
        };

        let started = std::time::Instant::now();
        let startup_result = run_purge_pass(base_path.clone()).await;
        let elapsed = started.elapsed();
        match startup_result {
            Ok(summary) => log_summary(summary, elapsed, true),
            Err(join_err) => {
                tracing::error!(
                    base_dir = %base_dir,
                    retention_days,
                    elapsed_ms = elapsed.as_millis() as u64,
                    error = %join_err,
                    "Startup console log purge blocking task failed; scheduler continuing"
                );
            }
        }

        loop {
            // Recompute on every iteration for self-healing against suspend/resume
            // and long-running passes that cross midnight.
            let now_utc = Utc::now();
            let sleep_for = duration_until_next_utc_midnight(now_utc);
            let next_fire = now_utc + ChronoDuration::from_std(sleep_for).unwrap_or_default();
            tracing::info!(
                sleep_secs = sleep_for.as_secs(),
                next_fire_utc = %next_fire.to_rfc3339(),
                "Console log purge: sleeping until next UTC midnight"
            );
            tokio::time::sleep(sleep_for).await;

            let started = std::time::Instant::now();
            let result = run_purge_pass(base_path.clone()).await;
            let elapsed = started.elapsed();

            match result {
                Ok(summary) => log_summary(summary, elapsed, false),
                Err(join_err) => {
                    tracing::error!(
                        base_dir = %base_dir,
                        retention_days,
                        elapsed_ms = elapsed.as_millis() as u64,
                        error = %join_err,
                        "Console log purge blocking task failed; scheduler continuing"
                    );
                }
            }
        }
    });

    Some(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    // Rather than rewriting mtimes (which would pull in a new dep for a
    // platform-specific syscall), we shift the `now` argument — which is wired
    // into `purge_once` precisely for deterministic testing. A file written at
    // real wall-clock time T has a real mtime ~T; passing `T + large_offset`
    // as `now` makes the walker treat that file as old without touching the
    // filesystem's time metadata.
    const HOURS: u64 = 3_600;
    const DAYS: u64 = 24 * HOURS;
    const FAR_FUTURE: Duration = Duration::from_secs(365 * DAYS);

    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, contents).expect("write");
    }

    #[test]
    fn fresh_files_are_preserved() {
        let tmp = tempdir().unwrap();
        let base = tmp.path();
        let real_now = SystemTime::now();

        let recent = base.join("meeting-a/2025-01-01/user_123_00001.log.gz");
        write_file(&recent, b"fresh");

        // `now` is the real wall clock, so the freshly written file is ~0 days old.
        let summary = purge_once(base, 30, real_now);

        assert_eq!(summary.files_deleted, 0, "recent file must not be deleted");
        assert_eq!(summary.bytes_reclaimed, 0);
        assert!(recent.exists(), "recent file still present");
    }

    #[test]
    fn old_files_are_deleted() {
        let tmp = tempdir().unwrap();
        let base = tmp.path();

        let stale = base.join("meeting-a/2024-01-01/user_123_00001.log.gz");
        write_file(&stale, b"stale-bytes");

        // Simulate "now" being 60 days in the future so the just-written file
        // looks ~60 days old relative to the scheduler's clock.
        let simulated_now = SystemTime::now() + Duration::from_secs(60 * DAYS);

        let summary = purge_once(base, 30, simulated_now);

        assert_eq!(summary.files_deleted, 1);
        assert_eq!(summary.bytes_reclaimed, "stale-bytes".len() as u64);
        assert!(!stale.exists(), "stale file should be gone");
    }

    #[test]
    fn empty_dirs_are_pruned_bottom_up() {
        let tmp = tempdir().unwrap();
        let base = tmp.path();

        let stale = base.join("meeting-a/2024-01-01/user_123_00001.log.gz");
        write_file(&stale, b"x");

        let simulated_now = SystemTime::now() + FAR_FUTURE;
        let summary = purge_once(base, 30, simulated_now);

        assert_eq!(summary.files_deleted, 1);
        assert!(
            summary.dirs_removed >= 2,
            "both date and meeting dirs pruned"
        );
        assert!(!base.join("meeting-a/2024-01-01").exists());
        assert!(!base.join("meeting-a").exists());
        // The base itself must never be removed.
        assert!(base.exists(), "base directory preserved");
    }

    #[test]
    fn non_matching_files_are_left_alone() {
        let tmp = tempdir().unwrap();
        let base = tmp.path();

        let unrelated = base.join("meeting-a/2024-01-01/README.txt");
        write_file(&unrelated, b"operator note");

        let stale_log = base.join("meeting-a/2024-01-01/user_123_00001.log");
        write_file(&stale_log, b"stale");

        let simulated_now = SystemTime::now() + FAR_FUTURE;
        let summary = purge_once(base, 30, simulated_now);

        assert_eq!(summary.files_deleted, 1, "only the .log file is purged");
        assert!(unrelated.exists(), "unrelated file preserved");
        assert!(!stale_log.exists(), ".log file was purged");
        // The directory should NOT be pruned because README.txt still occupies it.
        assert!(base.join("meeting-a/2024-01-01").exists());
    }

    #[test]
    fn lost_plus_found_is_skipped_at_any_depth() {
        let tmp = tempdir().unwrap();
        let base = tmp.path();

        // Root-level lost+found with a stale-looking log inside. The walker must
        // NOT enter this dir, so the file must survive.
        let protected_root = base.join("lost+found/orphan_001.log.gz");
        write_file(&protected_root, b"orphan");

        // Nested lost+found — same expectation.
        let protected_nested = base.join("meeting-a/lost+found/orphan_002.log.gz");
        write_file(&protected_nested, b"orphan");

        // Something that SHOULD be purged, to prove the walker didn't short-circuit.
        let stale = base.join("meeting-a/2024-01-01/user_123_00001.log.gz");
        write_file(&stale, b"stale");

        let simulated_now = SystemTime::now() + FAR_FUTURE;
        let summary = purge_once(base, 30, simulated_now);

        assert_eq!(
            summary.files_deleted, 1,
            "only the non-lost+found log purged"
        );
        assert!(protected_root.exists(), "root lost+found preserved");
        assert!(protected_nested.exists(), "nested lost+found preserved");
        assert!(!stale.exists(), "stale log outside lost+found purged");
    }

    #[test]
    fn missing_base_dir_is_not_an_error() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let summary = purge_once(&missing, 30, SystemTime::now());
        assert_eq!(summary, PurgeSummary::default());
    }

    #[test]
    fn zero_retention_deletes_all_matching_files() {
        let tmp = tempdir().unwrap();
        let base = tmp.path();

        // Even a file written moments ago should be purged when retention_days = 0,
        // because the cutoff is `now`.
        let path = base.join("meeting-a/2025-01-01/user_123_00001.log.gz");
        write_file(&path, b"x");

        // Use a `now` a few seconds in the future so the freshly-written file
        // (whose mtime is ~real_now) is strictly older than the cutoff.
        let simulated_now = SystemTime::now() + Duration::from_secs(5);
        let summary = purge_once(base, 0, simulated_now);

        assert_eq!(summary.files_deleted, 1);
        assert!(!path.exists());
    }

    #[test]
    fn duration_until_next_utc_midnight_is_positive_and_bounded() {
        // A representative instant mid-day.
        let now = Utc
            .with_ymd_and_hms(2025, 6, 15, 13, 30, 0)
            .single()
            .unwrap();
        let d = duration_until_next_utc_midnight(now);
        // Should be 10h30m = 37800s.
        assert_eq!(d.as_secs(), 10 * 3600 + 30 * 60);
    }

    #[test]
    fn duration_until_next_utc_midnight_at_exact_midnight_is_one_day() {
        let now = Utc.with_ymd_and_hms(2025, 6, 15, 0, 0, 0).single().unwrap();
        let d = duration_until_next_utc_midnight(now);
        assert_eq!(d.as_secs(), 86_400);
    }
}

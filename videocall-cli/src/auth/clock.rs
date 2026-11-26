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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Clock-offset correction for signing device JWTs (contract 2.6).
//!
//! A Raspberry Pi has no RTC, so right after boot its clock can be off by
//! minutes until NTP catches up. Device tokens carry `iat`/`exp`, so the
//! servers would reject them. Before login and on a 401, the CLI reads the
//! server's HTTP `Date`, and if it is more than [`SKEW_THRESHOLD_SECS`] away from the local
//! clock, it signs with `local + offset` for the rest of the process. The
//! offset is never persisted.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use videocall_meeting_client::reqwest;

/// Skew below this is left alone; servers allow 60 s of leeway.
pub const SKEW_THRESHOLD_SECS: i64 = 30;

/// A signing clock: local time plus a learned offset.
///
/// The CLI uses [`Clock::global`]; tests use their own so they don't race.
#[derive(Debug, Default)]
pub struct Clock {
    offset: AtomicI64,
}

static GLOBAL: Clock = Clock::new();

impl Clock {
    /// A clock with no offset.
    pub const fn new() -> Self {
        Self {
            offset: AtomicI64::new(0),
        }
    }

    /// The process-wide clock.
    pub fn global() -> &'static Clock {
        &GLOBAL
    }

    /// Unix time to sign with: local time plus the learned offset.
    pub fn now(&self) -> i64 {
        local_now() + self.offset()
    }

    /// The offset in effect, in seconds.
    pub fn offset(&self) -> i64 {
        self.offset.load(Ordering::Relaxed)
    }

    /// Learn the offset from a server time. Returns true if it changed,
    /// which means a request rejected for its timestamps is worth one retry.
    pub fn adopt_server_time(&self, server_now: i64) -> bool {
        match needed_offset(server_now, local_now(), self.offset()) {
            Some(wanted) => {
                self.offset.store(wanted, Ordering::Relaxed);
                eprintln!(
                    "warning: this machine's clock is {wanted:+} s off the server's; signing \
                     with the server's time for now. Fix NTP on this machine."
                );
                true
            }
            None => false,
        }
    }
}

/// Local Unix time in seconds, uncorrected.
pub fn local_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The offset to adopt given the server's time, or `None` if the current
/// offset is already within [`SKEW_THRESHOLD_SECS`].
pub(crate) fn needed_offset(server_now: i64, local_now: i64, current_offset: i64) -> Option<i64> {
    let wanted = server_now - local_now;
    ((wanted - current_offset).abs() > SKEW_THRESHOLD_SECS).then_some(wanted)
}

/// Parse an HTTP `Date` header (IMF-fixdate) to Unix seconds.
pub(crate) fn parse_http_date(value: &str) -> Option<i64> {
    httpdate::parse_http_date(value)
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

/// Cheap unauthenticated meeting-api path whose response carries `Date`.
pub const TIME_PROBE_PATH: &str = "/version";

/// The server's clock, from the HTTP `Date` header of an unauthenticated
/// `GET {origin}/version`. Any status will do; only the header is read
/// (hyper adds `Date` to every response).
pub async fn server_time(http: &reqwest::Client, origin: &str) -> Option<i64> {
    let response = http
        .get(format!("{origin}{TIME_PROBE_PATH}"))
        .send()
        .await
        .ok()?;
    let date = response
        .headers()
        .get(reqwest::header::DATE)?
        .to_str()
        .ok()?;
    parse_http_date(date)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_skew_is_ignored() {
        assert_eq!(needed_offset(1_000_030, 1_000_000, 0), None);
        assert_eq!(needed_offset(999_970, 1_000_000, 0), None);
    }

    #[test]
    fn large_skew_sets_offset() {
        assert_eq!(needed_offset(1_000_031, 1_000_000, 0), Some(31));
        assert_eq!(needed_offset(1_000_000, 1_003_600, 0), Some(-3600));
    }

    #[test]
    fn offset_already_applied_is_not_reapplied() {
        // Second 401 with the same server time: no retry.
        assert_eq!(needed_offset(1_003_600, 1_000_000, 3600), None);
        assert_eq!(needed_offset(1_003_600, 1_000_000, 3590), None);
    }

    #[test]
    fn clock_adopts_once() {
        let clock = Clock::new();
        assert!(!clock.adopt_server_time(local_now() + 5));
        assert_eq!(clock.offset(), 0);
        assert!(clock.adopt_server_time(local_now() + 3600));
        assert!((clock.offset() - 3600).abs() <= 1);
        assert!(!clock.adopt_server_time(local_now() + 3600));
        assert!((clock.now() - (local_now() + 3600)).abs() <= 1);
    }

    #[test]
    fn parses_imf_fixdate() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784111777)
        );
        assert_eq!(parse_http_date("not a date"), None);
    }
}

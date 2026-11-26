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

//! Retry timing: exponential backoff for relay connects, and the jittered
//! interval for waiting-room polling.
//!
//! Jitter keeps a room full of CLIs (or a Pi fleet that lost power together)
//! from hammering the relay and meeting-api in lockstep.

use std::time::Duration;

/// Exponential backoff with "equal jitter": the nominal delay doubles from
/// `initial` up to `max`, and each actual delay is drawn from
/// `[nominal / 2, nominal]`.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    attempt: u32,
}

impl Backoff {
    /// Backoff between `initial` and `max`.
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            attempt: 0,
        }
    }

    /// Policy for the initial relay connect attempts: 1 s doubling to 30 s.
    pub fn relay() -> Self {
        Self::new(Duration::from_secs(1), Duration::from_secs(30))
    }

    /// Start over from `initial` (after a success).
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// The un-jittered delay for the next attempt.
    pub fn nominal(&self) -> Duration {
        let factor = 1u32 << self.attempt.min(16);
        self.initial.saturating_mul(factor).min(self.max)
    }

    /// The delay to sleep before the next attempt.
    pub fn next_delay(&mut self) -> Duration {
        let nominal = self.nominal();
        self.attempt = self.attempt.saturating_add(1);
        equal_jitter(nominal, random_fraction())
    }
}

/// `nominal / 2 + fraction * nominal / 2`, with `fraction` in `[0, 1)`.
pub(crate) fn equal_jitter(nominal: Duration, fraction: f64) -> Duration {
    let half = nominal / 2;
    half + half.mul_f64(fraction.clamp(0.0, 1.0))
}

/// Waiting-room poll interval: about 3 seconds (2.5 to 3.5 s).
pub fn poll_delay() -> Duration {
    Duration::from_millis(2500) + Duration::from_millis(1000).mul_f64(random_fraction())
}

/// Uniform in `[0, 1)` from the OS RNG; `0.5` if the RNG is unavailable.
fn random_fraction() -> f64 {
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        return 0.5;
    }
    f64::from(u32::from_le_bytes(bytes)) / (f64::from(u32::MAX) + 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nominal_doubles_and_caps() {
        let mut backoff = Backoff::relay();
        let mut nominals = Vec::new();
        for _ in 0..40 {
            nominals.push(backoff.nominal().as_secs());
            backoff.next_delay();
        }
        assert_eq!(&nominals[..7], &[1, 2, 4, 8, 16, 30, 30]);
        assert!(nominals.iter().all(|&n| n <= 30));
        assert_eq!(*nominals.last().unwrap(), 30);
    }

    #[test]
    fn delays_stay_in_bounds() {
        let mut backoff = Backoff::relay();
        for _ in 0..200 {
            let nominal = backoff.nominal();
            let delay = backoff.next_delay();
            assert!(
                delay >= nominal / 2 && delay <= nominal,
                "{delay:?} vs {nominal:?}"
            );
            assert!(delay >= Duration::from_millis(500));
            assert!(delay <= Duration::from_secs(30));
        }
    }

    #[test]
    fn jitter_edges() {
        let n = Duration::from_secs(10);
        assert_eq!(equal_jitter(n, 0.0), Duration::from_secs(5));
        assert_eq!(equal_jitter(n, 1.0), Duration::from_secs(10));
        assert_eq!(equal_jitter(n, 7.0), Duration::from_secs(10));
    }

    #[test]
    fn poll_delay_is_about_three_seconds() {
        for _ in 0..100 {
            let d = poll_delay();
            assert!(d >= Duration::from_millis(2500) && d < Duration::from_millis(3500));
        }
    }
}

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

//! In-memory fixed-window rate limiter keyed by string.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Checks between sweeps of lapsed windows.
const SWEEP_EVERY_OPS: u64 = 64;

/// Allows at most `max` actions per key per `window`.
#[derive(Debug)]
pub struct KeyedRateLimiter {
    max: u32,
    window: Duration,
    entries: Mutex<HashMap<String, (Instant, u32)>>,
    ops: AtomicU64,
}

impl KeyedRateLimiter {
    pub fn new(max: u32, window: Duration) -> Self {
        Self {
            max,
            window,
            entries: Mutex::new(HashMap::new()),
            ops: AtomicU64::new(0),
        }
    }

    /// Host kicks: 30 per host per minute (#2934).
    pub fn for_host_kicks() -> Self {
        Self::new(30, Duration::from_secs(60))
    }

    /// `true` when `key` may act now; an allowed call is counted.
    pub fn allow(&self, key: &str) -> bool {
        self.allow_at(key, Instant::now())
    }

    fn allow_at(&self, key: &str, now: Instant) -> bool {
        let tick = self.ops.fetch_add(1, Ordering::Relaxed) + 1;
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if tick.is_multiple_of(SWEEP_EVERY_OPS) {
            let window = self.window;
            entries.retain(|_, (start, _)| now.saturating_duration_since(*start) < window);
        }
        let entry = entries.entry(key.to_owned()).or_insert((now, 0));
        if now.saturating_duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        if entry.1 >= self.max {
            return false;
        }
        entry.1 += 1;
        true
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_max_per_window_per_key_then_resets() {
        let limiter = KeyedRateLimiter::new(2, Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(limiter.allow_at("a", t0));
        assert!(limiter.allow_at("a", t0));
        assert!(!limiter.allow_at("a", t0));
        assert!(limiter.allow_at("b", t0));
        assert!(limiter.allow_at("a", t0 + Duration::from_secs(60)));
    }

    #[test]
    fn lapsed_windows_are_swept() {
        let limiter = KeyedRateLimiter::new(1, Duration::from_secs(1));
        let t0 = Instant::now();
        for i in 0..(SWEEP_EVERY_OPS - 1) {
            limiter.allow_at(&format!("k{i}"), t0);
        }
        assert_eq!(limiter.tracked(), (SWEEP_EVERY_OPS - 1) as usize);
        limiter.allow_at("late", t0 + Duration::from_secs(2));
        assert_eq!(limiter.tracked(), 1);
    }
}

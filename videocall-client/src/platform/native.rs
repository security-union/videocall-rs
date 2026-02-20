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

//! Native (desktop / server / embedded) platform primitives.
//!
//! These implementations use `std::time` and `tokio` for timers and task
//! spawning.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Returns the current time in milliseconds since the Unix epoch.
///
/// Uses `std::time::SystemTime` — no browser APIs required.
pub fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64
}

/// A repeating timer that fires a callback at a fixed interval.
///
/// On native, this spawns a `tokio` task that sleeps in a loop. The timer is
/// automatically cancelled when the handle is dropped via a shared `AtomicBool`
/// flag.
pub struct IntervalHandle {
    quit: Arc<AtomicBool>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl IntervalHandle {
    /// Create a new repeating timer.
    ///
    /// **Important:** A tokio runtime must be active when this is called.
    ///
    /// # Arguments
    /// * `period_ms` — interval period in milliseconds
    /// * `callback` — closure to invoke on each tick; must be `Send + 'static`
    pub fn new<F: Fn() + Send + 'static>(period_ms: u32, callback: F) -> Self {
        let quit = Arc::new(AtomicBool::new(false));
        let quit_clone = quit.clone();
        let period = std::time::Duration::from_millis(period_ms as u64);

        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            // Skip the first immediate tick so behaviour matches gloo::Interval
            // which does NOT fire immediately.
            interval.tick().await;

            loop {
                interval.tick().await;
                if quit_clone.load(Ordering::Relaxed) {
                    break;
                }
                callback();
            }
        });

        Self {
            quit,
            handle: Some(handle),
        }
    }
}

impl Drop for IntervalHandle {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// Spawn an async task on the tokio runtime.
///
/// The future must be `Send + 'static` because tokio tasks may run on any
/// thread in a multi-threaded runtime.
pub fn spawn<F: Future<Output = ()> + Send + 'static>(future: F) {
    tokio::spawn(future);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_ms_returns_reasonable_value() {
        let ms = now_ms();
        // Should be well past year 2020 (1577836800000 ms)
        assert!(ms > 1_577_836_800_000.0, "now_ms() returned {ms}");
        // Should be before year 2100
        assert!(ms < 4_102_444_800_000.0, "now_ms() returned {ms}");
    }

    #[tokio::test]
    async fn test_interval_fires_and_cancels() {
        use std::sync::atomic::AtomicU32;
        use std::time::Duration;

        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let handle = IntervalHandle::new(10, move || {
            counter_clone.fetch_add(1, Ordering::Relaxed);
        });

        // Let it tick a few times
        tokio::time::sleep(Duration::from_millis(55)).await;

        let count_before_drop = counter.load(Ordering::Relaxed);
        assert!(
            count_before_drop >= 2,
            "expected at least 2 ticks, got {count_before_drop}"
        );

        // Drop the handle — should stop the interval
        drop(handle);

        // Wait a bit more and verify no more ticks
        tokio::time::sleep(Duration::from_millis(30)).await;
        let count_after_drop = counter.load(Ordering::Relaxed);
        // Allow at most 1 extra tick due to race
        assert!(
            count_after_drop <= count_before_drop + 1,
            "expected no more ticks after drop, got {count_after_drop} (was {count_before_drop})"
        );
    }

    #[tokio::test]
    async fn test_spawn_executes_future() {
        use std::time::Duration;

        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = flag.clone();

        spawn(async move {
            flag_clone.store(true, Ordering::Relaxed);
        });

        // Give the spawned task time to run
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            flag.load(Ordering::Relaxed),
            "spawned future should have run"
        );
    }
}

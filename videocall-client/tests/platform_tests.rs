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

//! Tests for the platform abstraction layer.

#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use videocall_client::platform::{self, ConnectionError, IntervalHandle};

    #[test]
    fn test_now_ms_is_monotonic() {
        let t1 = platform::now_ms();
        let t2 = platform::now_ms();
        assert!(t2 >= t1, "now_ms should be monotonic");
    }

    #[test]
    fn test_now_ms_is_in_milliseconds() {
        let ms = platform::now_ms();
        // A timestamp in seconds would be ~1.7 billion (year 2024)
        // In milliseconds it should be ~1.7 trillion
        assert!(ms > 1_000_000_000_000.0, "Expected milliseconds, got {ms}");
    }

    #[tokio::test]
    async fn test_interval_handle_multiple_ticks() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let _handle = IntervalHandle::new(5, move || {
            counter_clone.fetch_add(1, Ordering::Relaxed);
        });

        tokio::time::sleep(Duration::from_millis(35)).await;
        let count = counter.load(Ordering::Relaxed);
        assert!(
            count >= 3,
            "Expected at least 3 ticks in 35ms with 5ms interval, got {count}"
        );
    }

    #[tokio::test]
    async fn test_interval_handle_drop_cancels() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        {
            let _handle = IntervalHandle::new(5, move || {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Handle dropped here

        let count_at_drop = counter.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let count_after = counter.load(Ordering::Relaxed);

        assert!(
            count_after <= count_at_drop + 1,
            "Interval should stop after drop: was {count_at_drop}, now {count_after}"
        );
    }

    #[tokio::test]
    async fn test_spawn_runs_future() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = flag.clone();

        platform::spawn(async move {
            flag_clone.store(true, Ordering::Relaxed);
        });

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn test_connection_error_type_is_string() {
        let err: ConnectionError = "test error".to_string();
        assert_eq!(err, "test error");
    }

    #[test]
    fn test_connection_error_can_be_formatted() {
        let err: ConnectionError = "connection lost".to_string();
        let formatted = format!("Error: {err}");
        assert_eq!(formatted, "Error: connection lost");
    }
}

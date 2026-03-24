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

//! Integration tests for session logic (CongestionTracker, InboundAction, SessionLogic).
//!
//! Previously these lived inside `src/actors/session_logic.rs` as a `#[cfg(test)] mod tests`
//! block. They have been extracted here so that:
//! 1. The production source file stays focused on production code.
//! 2. Tests compile as integration tests and run via `cargo test -p videocall-api`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sec_api::actors::session_logic::{
    CongestionTracker, InboundAction, SenderDropState, SessionLogic, CLEANUP_INTERVAL,
};
use sec_api::constants::{
    CONGESTION_DROP_THRESHOLD, CONGESTION_NOTIFY_MIN_INTERVAL, CONGESTION_WINDOW,
};

#[test]
fn test_inbound_action_debug() {
    let action = InboundAction::KeepAlive;
    assert_eq!(format!("{action:?}"), "KeepAlive");
}

#[test]
fn test_congestion_tracker_cleans_stale_entries() {
    let mut tracker = CongestionTracker::new();

    // Insert a stale entry by manually inserting with an old window_start.
    let stale_id = 1000;
    tracker.senders.insert(
        stale_id,
        SenderDropState {
            drop_count: 0,
            // 20 seconds ago — well past the 10 * CONGESTION_WINDOW threshold
            window_start: Instant::now() - (CONGESTION_WINDOW * 20),
            last_notify: None,
        },
    );

    // Insert a fresh entry.
    let fresh_id = 2000;
    tracker.senders.insert(
        fresh_id,
        SenderDropState {
            drop_count: 0,
            window_start: Instant::now(),
            last_notify: None,
        },
    );

    assert_eq!(tracker.senders.len(), 2);

    // Set total_drops so the next record_drop triggers cleanup.
    tracker.total_drops = CLEANUP_INTERVAL - 1;

    // Recording a drop for a new sender should trigger cleanup.
    let trigger_id = 3000;
    tracker.record_drop(trigger_id);

    // The stale entry should have been removed.
    assert!(
        !tracker.senders.contains_key(&stale_id),
        "stale sender entry should be cleaned up"
    );
    // Fresh and trigger entries should remain.
    assert!(tracker.senders.contains_key(&fresh_id));
    assert!(tracker.senders.contains_key(&trigger_id));
}

#[test]
fn test_congestion_tracker_retains_active_entries() {
    let mut tracker = CongestionTracker::new();

    // Record drops for two senders.
    tracker.record_drop(100);
    tracker.record_drop(200);

    assert_eq!(tracker.senders.len(), 2);

    // Record another drop — both entries are fresh, nothing should be cleaned.
    tracker.record_drop(100);

    assert_eq!(tracker.senders.len(), 2);
    assert!(tracker.senders.contains_key(&100));
    assert!(tracker.senders.contains_key(&200));
}

// =====================================================================
// Drop recording and counting
// =====================================================================

#[test]
fn test_drop_recording_increments_count() {
    let mut tracker = CongestionTracker::new();
    let sender_id = 42;

    // Record a single drop — should not yet trigger notification.
    let result = tracker.record_drop(sender_id);
    assert!(
        result.is_none(),
        "single drop should not trigger notification"
    );

    // The internal count should be 1.
    let state = tracker.senders.get(&sender_id).unwrap();
    assert_eq!(state.drop_count, 1);
}

#[test]
fn test_drop_recording_multiple_senders_independent() {
    let mut tracker = CongestionTracker::new();

    // Record drops for two different senders.
    for _ in 0..3 {
        tracker.record_drop(100);
    }
    for _ in 0..2 {
        tracker.record_drop(200);
    }

    // Each sender should have independent counts.
    assert_eq!(tracker.senders.get(&100).unwrap().drop_count, 3);
    assert_eq!(tracker.senders.get(&200).unwrap().drop_count, 2);
}

#[test]
fn test_drop_window_resets_after_expiry() {
    let mut tracker = CongestionTracker::new();
    let sender_id = 50;

    // Manually insert a sender with a window that started in the past
    // (just beyond CONGESTION_WINDOW) so the next record_drop resets it.
    tracker.senders.insert(
        sender_id,
        SenderDropState {
            drop_count: 3,
            window_start: Instant::now() - (CONGESTION_WINDOW + Duration::from_millis(10)),
            last_notify: None,
        },
    );

    // record_drop should reset the window and set count to 1 (not 4).
    tracker.record_drop(sender_id);
    let state = tracker.senders.get(&sender_id).unwrap();
    assert_eq!(
        state.drop_count, 1,
        "drop count should reset to 1 after window expiry"
    );
}

// =====================================================================
// Congestion notification triggering
// =====================================================================

#[test]
fn test_notification_triggers_at_threshold() {
    let mut tracker = CongestionTracker::new();
    let sender_id = 99;

    // Record drops up to one less than threshold — no notification.
    for _ in 0..(CONGESTION_DROP_THRESHOLD - 1) {
        let result = tracker.record_drop(sender_id);
        assert!(result.is_none());
    }

    // The threshold-th drop should trigger a notification.
    let result = tracker.record_drop(sender_id);
    assert_eq!(
        result,
        Some(sender_id),
        "should return sender_id when threshold is reached"
    );
}

#[test]
fn test_notification_resets_count_after_trigger() {
    let mut tracker = CongestionTracker::new();
    let sender_id = 77;

    // Reach threshold to trigger notification.
    for _ in 0..CONGESTION_DROP_THRESHOLD {
        tracker.record_drop(sender_id);
    }

    // After triggering, count should be reset to 0.
    let state = tracker.senders.get(&sender_id).unwrap();
    assert_eq!(
        state.drop_count, 0,
        "drop count should reset after notification"
    );
}

#[test]
fn test_rate_limiting_suppresses_rapid_notifications() {
    let mut tracker = CongestionTracker::new();
    let sender_id = 55;

    // First burst: trigger notification.
    for _ in 0..CONGESTION_DROP_THRESHOLD {
        tracker.record_drop(sender_id);
    }
    // The last call above returned Some(55). Now the last_notify is set.

    // Second burst immediately after: should be rate-limited because
    // CONGESTION_NOTIFY_MIN_INTERVAL has not elapsed.
    for i in 0..CONGESTION_DROP_THRESHOLD {
        let result = tracker.record_drop(sender_id);
        if i < CONGESTION_DROP_THRESHOLD - 1 {
            // Below threshold — always None.
            assert!(result.is_none());
        } else {
            // At threshold — rate-limited, so still None.
            assert!(
                result.is_none(),
                "notification should be suppressed by rate limiter"
            );
        }
    }
}

// =====================================================================
// Stale entry cleanup
// =====================================================================

#[test]
fn test_stale_cleanup_removes_multiple_stale_entries() {
    let mut tracker = CongestionTracker::new();

    // Insert several stale entries.
    for id in 1..=5 {
        tracker.senders.insert(
            id,
            SenderDropState {
                drop_count: 0,
                window_start: Instant::now() - (CONGESTION_WINDOW * 20),
                last_notify: None,
            },
        );
    }

    // Insert one fresh entry.
    tracker.senders.insert(
        100,
        SenderDropState {
            drop_count: 0,
            window_start: Instant::now(),
            last_notify: None,
        },
    );

    assert_eq!(tracker.senders.len(), 6);

    // Set total_drops so the next record_drop triggers cleanup.
    tracker.total_drops = CLEANUP_INTERVAL - 1;

    // Trigger cleanup by recording a drop.
    tracker.record_drop(200);

    // All stale entries (1-5) should be gone; fresh (100) and new (200) remain.
    assert_eq!(tracker.senders.len(), 2);
    assert!(tracker.senders.contains_key(&100));
    assert!(tracker.senders.contains_key(&200));
}

#[test]
fn test_entry_just_under_boundary_is_retained() {
    let mut tracker = CongestionTracker::new();

    // Insert an entry slightly under the stale boundary (10 * CONGESTION_WINDOW).
    // Use a 500ms margin to account for time elapsed between insertion and
    // the `retain` call inside `record_drop`.
    tracker.senders.insert(
        1,
        SenderDropState {
            drop_count: 2,
            window_start: Instant::now() - (CONGESTION_WINDOW * 10) + Duration::from_millis(500),
            last_notify: None,
        },
    );

    // Set total_drops so the next record_drop triggers cleanup.
    tracker.total_drops = CLEANUP_INTERVAL - 1;

    tracker.record_drop(2);

    // Entry 1 is within the boundary — should be retained.
    assert!(
        tracker.senders.contains_key(&1),
        "entry just under stale boundary should be retained"
    );
}

// =====================================================================
// should_notify_sender() — tested indirectly through record_drop
// =====================================================================

#[test]
fn test_first_notification_for_sender_has_no_rate_limit() {
    let mut tracker = CongestionTracker::new();
    let sender_id = 10;

    // First time reaching threshold — no prior last_notify, should fire.
    for _ in 0..CONGESTION_DROP_THRESHOLD {
        tracker.record_drop(sender_id);
    }

    // Verify last_notify was set.
    let state = tracker.senders.get(&sender_id).unwrap();
    assert!(
        state.last_notify.is_some(),
        "last_notify should be set after first notification"
    );
}

#[test]
fn test_notification_allowed_after_rate_limit_expires() {
    let mut tracker = CongestionTracker::new();
    let sender_id = 30;

    // Simulate a previous notification that happened long enough ago
    // that the rate limit has expired.
    tracker.senders.insert(
        sender_id,
        SenderDropState {
            drop_count: 0,
            window_start: Instant::now(),
            last_notify: Some(
                Instant::now() - CONGESTION_NOTIFY_MIN_INTERVAL - Duration::from_millis(10),
            ),
        },
    );

    // Record enough drops to hit threshold.
    for _ in 0..CONGESTION_DROP_THRESHOLD {
        tracker.record_drop(sender_id);
    }

    // Should trigger because rate limit has expired.
    // The last record_drop was the threshold-th, which was the one that returned.
    // We need to check the return value of the last call.
    // Let's redo this more carefully.
    let mut tracker2 = CongestionTracker::new();
    tracker2.senders.insert(
        sender_id,
        SenderDropState {
            drop_count: 0,
            window_start: Instant::now(),
            last_notify: Some(
                Instant::now() - CONGESTION_NOTIFY_MIN_INTERVAL - Duration::from_millis(10),
            ),
        },
    );

    let mut triggered = false;
    for _ in 0..CONGESTION_DROP_THRESHOLD {
        if tracker2.record_drop(sender_id).is_some() {
            triggered = true;
        }
    }
    assert!(
        triggered,
        "notification should fire after rate-limit window expires"
    );
}

#[test]
fn test_default_trait_impl() {
    // Verify Default trait works and produces an empty tracker.
    let tracker = CongestionTracker::default();
    assert!(tracker.senders.is_empty());
}

#[test]
fn test_should_activate_on_action() {
    // Echo (RTT probe) should NOT activate.
    assert!(!SessionLogic::should_activate_on_action(
        &InboundAction::Echo(Arc::new(vec![]))
    ));
    // Forward, Processed, KeepAlive should activate.
    assert!(SessionLogic::should_activate_on_action(
        &InboundAction::Forward(Arc::new(vec![]))
    ));
    assert!(SessionLogic::should_activate_on_action(
        &InboundAction::Processed
    ));
    assert!(SessionLogic::should_activate_on_action(
        &InboundAction::KeepAlive
    ));
}

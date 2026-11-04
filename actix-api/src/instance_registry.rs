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

use std::collections::HashMap;
use videocall_types::validation::is_valid_meeting_id;

use videocall_meeting_types::kick::KICK_DENY_WINDOW_SECS;

use crate::kick_denylist::KICK_THROUGH_FUTURE_SLACK_SECS;

pub const INSTANCE_START_FUTURE_SLACK_MS: i64 = KICK_THROUGH_FUTURE_SLACK_SECS * 1000;

/// Outlives every room token minted before the recorded start.
pub const INSTANCE_REGISTRY_TTL_MS: i64 = KICK_DENY_WINDOW_SECS * 1000;

/// Rooms kept in total; an insert at the cap first drops expired entries and,
/// if still at the cap, evicts the entries closest to expiry until
/// [`eviction_batch`] slots are free.
pub const INSTANCE_REGISTRY_CAP: usize = 65_536;

/// Slots an insert at the cap frees, about 1% of `cap`.
fn eviction_batch(cap: usize) -> usize {
    (cap / 100).max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    started_at_ms: i64,
    expires_at_ms: i64,
}

#[derive(Debug)]
pub struct InstanceRegistry {
    rooms: HashMap<String, Entry>,
    cap: usize,
}

impl Default for InstanceRegistry {
    fn default() -> Self {
        Self::with_cap(INSTANCE_REGISTRY_CAP)
    }
}

impl InstanceRegistry {
    pub fn with_cap(cap: usize) -> Self {
        Self {
            rooms: HashMap::new(),
            cap: cap.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.rooms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rooms.is_empty()
    }

    /// Merge `started_at_ms`, clamped to `now_ms` plus slack, into `room`'s
    /// entry, keeping the later start and pushing its expiry to a full TTL
    /// from `now_ms`. Ignores an invalid room or a non-positive start.
    /// Returns how many unexpired rooms were evicted to make space.
    pub fn record(&mut self, room: &str, started_at_ms: i64, now_ms: i64) -> usize {
        if started_at_ms <= 0 || !is_valid_meeting_id(room) {
            return 0;
        }
        let started_at_ms =
            started_at_ms.min(now_ms.saturating_add(INSTANCE_START_FUTURE_SLACK_MS));
        let expires_at_ms = now_ms.saturating_add(INSTANCE_REGISTRY_TTL_MS);
        if let Some(entry) = self.rooms.get_mut(room) {
            entry.started_at_ms = entry.started_at_ms.max(started_at_ms);
            entry.expires_at_ms = entry.expires_at_ms.max(expires_at_ms);
            return 0;
        }
        let evicted = if self.rooms.len() >= self.cap {
            self.make_room(now_ms)
        } else {
            0
        };
        self.rooms.insert(
            room.to_string(),
            Entry {
                started_at_ms,
                expires_at_ms,
            },
        );
        evicted
    }

    pub fn known_start(&self, room: &str, now_ms: i64) -> Option<i64> {
        self.rooms
            .get(room)
            .filter(|entry| entry.expires_at_ms > now_ms)
            .map(|entry| entry.started_at_ms)
    }

    pub fn supersedes(&self, room: &str, token_started_at_ms: i64, now_ms: i64) -> bool {
        self.known_start(room, now_ms)
            .is_some_and(|known| token_started_at_ms < known)
    }

    pub fn sweep(&mut self, now_ms: i64) -> usize {
        let before = self.rooms.len();
        self.rooms.retain(|_, entry| entry.expires_at_ms > now_ms);
        before - self.rooms.len()
    }

    fn make_room(&mut self, now_ms: i64) -> usize {
        self.sweep(now_ms);
        if self.rooms.len() < self.cap {
            return 0;
        }
        let keep = self.cap.saturating_sub(eviction_batch(self.cap));
        let excess = self.rooms.len().saturating_sub(keep);
        if excess == 0 {
            return 0;
        }
        let mut by_expiry: Vec<(i64, &String)> = self
            .rooms
            .iter()
            .map(|(room, entry)| (entry.expires_at_ms, room))
            .collect();
        if excess < by_expiry.len() {
            by_expiry.select_nth_unstable(excess);
        }
        let victims: Vec<String> = by_expiry[..excess]
            .iter()
            .map(|(_, room)| (*room).clone())
            .collect();
        for room in &victims {
            self.rooms.remove(room);
        }
        victims.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn an_empty_registry_supersedes_nothing() {
        let reg = InstanceRegistry::default();
        assert_eq!(reg.known_start("room", NOW), None);
        assert!(!reg.supersedes("room", 1, NOW));
    }

    #[test]
    fn a_token_older_than_the_recorded_start_is_superseded_and_equal_or_newer_is_not() {
        let mut reg = InstanceRegistry::default();
        reg.record("room", NOW - 1000, NOW);
        assert!(reg.supersedes("room", NOW - 1001, NOW));
        assert!(!reg.supersedes("room", NOW - 1000, NOW));
        assert!(!reg.supersedes("room", NOW - 999, NOW));
        assert!(!reg.supersedes("other-room", 1, NOW));
    }

    #[test]
    fn record_keeps_the_latest_start_whatever_the_arrival_order() {
        let mut reg = InstanceRegistry::default();
        reg.record("room", NOW - 100, NOW);
        reg.record("room", NOW - 500, NOW);
        assert_eq!(reg.known_start("room", NOW), Some(NOW - 100));
        reg.record("room", NOW - 50, NOW);
        assert_eq!(reg.known_start("room", NOW), Some(NOW - 50));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn a_forged_future_start_is_clamped_to_now_plus_slack() {
        let mut reg = InstanceRegistry::default();
        reg.record("room", i64::MAX, NOW);
        assert_eq!(
            reg.known_start("room", NOW),
            Some(NOW + INSTANCE_START_FUTURE_SLACK_MS)
        );
        assert!(!reg.supersedes("room", NOW + INSTANCE_START_FUTURE_SLACK_MS, NOW));
        assert!(reg.supersedes("room", NOW + INSTANCE_START_FUTURE_SLACK_MS - 1, NOW));
    }

    #[test]
    fn an_invalid_room_or_non_positive_start_is_ignored() {
        let mut reg = InstanceRegistry::default();
        for (room, start) in [
            ("room.>", NOW),
            ("", NOW),
            ("room", 0),
            ("room", -5),
            ("room", i64::MIN),
        ] {
            assert_eq!(reg.record(room, start, NOW), 0);
        }
        assert!(reg.is_empty());
    }

    #[test]
    fn an_entry_expires_a_ttl_after_its_latest_record_and_is_swept() {
        let mut reg = InstanceRegistry::default();
        reg.record("room", NOW - 10, NOW);
        assert_eq!(
            reg.known_start("room", NOW + INSTANCE_REGISTRY_TTL_MS - 1),
            Some(NOW - 10)
        );
        assert_eq!(
            reg.known_start("room", NOW + INSTANCE_REGISTRY_TTL_MS),
            None
        );
        assert!(!reg.supersedes("room", 1, NOW + INSTANCE_REGISTRY_TTL_MS));

        let later = NOW + INSTANCE_REGISTRY_TTL_MS / 2;
        reg.record("room", NOW - 10, later);
        assert!(reg
            .known_start("room", NOW + INSTANCE_REGISTRY_TTL_MS)
            .is_some());

        reg.record("stale", NOW, NOW);
        assert_eq!(reg.sweep(later + INSTANCE_REGISTRY_TTL_MS - 1), 1);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.sweep(later + INSTANCE_REGISTRY_TTL_MS), 1);
        assert!(reg.is_empty());
    }

    #[test]
    fn the_cap_evicts_the_entry_closest_to_expiry_and_reports_it() {
        let mut reg = InstanceRegistry::with_cap(2);
        assert_eq!(reg.record("oldest", NOW, NOW), 0);
        assert_eq!(reg.record("newer", NOW, NOW + 10), 0);
        assert_eq!(reg.record("newest", NOW, NOW + 20), 1);
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.known_start("oldest", NOW + 20), None);
        assert!(reg.known_start("newer", NOW + 20).is_some());
        assert!(reg.known_start("newest", NOW + 20).is_some());

        assert_eq!(reg.record("newer", NOW, NOW + 30), 0);
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn the_ttl_spans_the_kick_deny_window_so_the_longest_lived_token_is_still_covered() {
        use videocall_meeting_types::kick::KICK_DENY_WINDOW_SECS;
        let mut reg = InstanceRegistry::default();
        reg.record("room", NOW - 10, NOW);
        assert_eq!(
            reg.known_start("room", NOW + KICK_DENY_WINDOW_SECS * 1000 - 1),
            Some(NOW - 10)
        );
        assert_eq!(
            reg.known_start("room", NOW + KICK_DENY_WINDOW_SECS * 1000),
            None
        );
    }

    fn fill(reg: &mut InstanceRegistry, count: usize, now: impl Fn(usize) -> i64) {
        for i in 0..count {
            assert_eq!(reg.record(&format!("r{i}"), NOW, now(i)), 0);
        }
    }

    #[test]
    fn an_insert_at_the_cap_evicts_a_batch_closest_to_expiry_and_counts_it() {
        let mut reg = InstanceRegistry::with_cap(200);
        fill(&mut reg, 200, |i| NOW + i as i64);
        assert_eq!(reg.len(), 200);

        let at = NOW + 1000;
        assert_eq!(reg.record("fresh", NOW, at), 2, "1% of 200 is 2");
        assert_eq!(reg.len(), 199, "two evicted, one inserted");
        assert_eq!(reg.known_start("r0", at), None);
        assert_eq!(reg.known_start("r1", at), None);
        assert!(reg.known_start("r2", at).is_some());
        assert!(reg.known_start("fresh", at).is_some());
    }

    #[test]
    fn an_insert_at_the_cap_sweeps_expired_entries_first_and_does_not_count_them() {
        let mut reg = InstanceRegistry::with_cap(200);
        fill(&mut reg, 200, |i| NOW + i as i64);

        let at = NOW + INSTANCE_REGISTRY_TTL_MS;
        assert_eq!(
            reg.record("fresh", NOW, at),
            0,
            "expired entries are swept, not evicted"
        );
        assert_eq!(
            reg.len(),
            200,
            "only r0 expired by `at`; every live entry was kept"
        );
        assert!(reg.known_start("r1", at).is_some());
        assert!(reg.known_start("r199", at).is_some());
    }
}

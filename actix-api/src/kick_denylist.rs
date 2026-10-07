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

//! Host kicks this relay has been told about, keyed by room and user (#2934).

use std::collections::HashMap;
use videocall_meeting_types::kick::ParticipantKickedPayload;
use videocall_types::validation::is_valid_meeting_id;

/// How far past relay-now a revocation's `revoke_iat_through` may reach.
pub const KICK_THROUGH_FUTURE_SLACK_SECS: i64 = 5;

/// Longest a revocation is kept, from receipt.
pub const KICK_DENY_MAX_WINDOW_SECS: i64 = 8 * 24 * 3600;

/// Revocations kept per room before the room's oldest is evicted.
pub const KICK_DENYLIST_ROOM_CAP: usize = 256;

/// Revocations kept in total; past it, an insert evicts the oldest entry of a
/// strictly larger room, else of its own room.
pub const KICK_DENYLIST_GLOBAL_CAP: usize = 65_536;

/// A validated revocation with both bounds clamped to relay-now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KickRevocation {
    pub room_id: String,
    pub user_id: String,
    pub revoke_iat_through: i64,
    pub deny_until: i64,
}

impl KickRevocation {
    /// `None` for a malformed or already-expired payload.
    pub fn from_payload(payload: ParticipantKickedPayload, now: i64) -> Option<Self> {
        if !is_valid_meeting_id(&payload.room_id)
            || payload.user_id.is_empty()
            || payload.user_id.len() > 256
        {
            return None;
        }
        let deny_until = payload
            .deny_until
            .min(now.saturating_add(KICK_DENY_MAX_WINDOW_SECS));
        if deny_until <= now {
            return None;
        }
        Some(Self {
            room_id: payload.room_id,
            user_id: payload.user_id,
            revoke_iat_through: payload
                .revoke_iat_through
                .min(now.saturating_add(KICK_THROUGH_FUTURE_SLACK_SECS)),
            deny_until,
        })
    }
}

/// Whether a token carrying `token_iat` was issued at or before `through`. A
/// token without `iat` cannot prove it postdates the revocation.
pub fn issued_through(token_iat: Option<i64>, through: i64) -> bool {
    token_iat.is_none_or(|iat| iat <= through)
}

/// Why [`KickDenylist::record`] evicted an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eviction {
    RoomQuota,
    GlobalCap,
}

impl Eviction {
    pub fn label(self) -> &'static str {
        match self {
            Eviction::RoomQuota => "room_quota",
            Eviction::GlobalCap => "global_cap",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    through: i64,
    deny_until: i64,
}

/// Bounded map of `(room, user)` to the newest revocation for that pair.
#[derive(Debug)]
pub struct KickDenylist {
    rooms: HashMap<String, HashMap<String, Entry>>,
    len: usize,
    room_cap: usize,
    global_cap: usize,
}

impl Default for KickDenylist {
    fn default() -> Self {
        Self::with_caps(KICK_DENYLIST_ROOM_CAP, KICK_DENYLIST_GLOBAL_CAP)
    }
}

impl KickDenylist {
    pub fn with_caps(room_cap: usize, global_cap: usize) -> Self {
        Self {
            rooms: HashMap::new(),
            len: 0,
            room_cap: room_cap.max(1),
            global_cap: global_cap.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Merge `rev` into the pair's entry, keeping the later bounds. Inserting a
    /// new pair into a full room evicts that room's oldest entry; past the
    /// global cap, the oldest entry of a room strictly larger than the
    /// inserting one goes, else the inserting room's own oldest.
    pub fn record(&mut self, rev: &KickRevocation) -> Option<Eviction> {
        if let Some(entry) = self
            .rooms
            .get_mut(&rev.room_id)
            .and_then(|users| users.get_mut(&rev.user_id))
        {
            entry.through = entry.through.max(rev.revoke_iat_through);
            entry.deny_until = entry.deny_until.max(rev.deny_until);
            return None;
        }
        let room_len = self.rooms.get(&rev.room_id).map_or(0, HashMap::len);
        let eviction = if room_len >= self.room_cap {
            self.evict_oldest_in(&rev.room_id);
            Some(Eviction::RoomQuota)
        } else if self.len >= self.global_cap {
            let victim = match self.largest_room() {
                Some((room, len)) if len > room_len => room,
                _ => rev.room_id.clone(),
            };
            self.evict_oldest_in(&victim);
            Some(Eviction::GlobalCap)
        } else {
            None
        };
        self.rooms.entry(rev.room_id.clone()).or_default().insert(
            rev.user_id.clone(),
            Entry {
                through: rev.revoke_iat_through,
                deny_until: rev.deny_until,
            },
        );
        self.len += 1;
        eviction
    }

    /// The pair's live `revoke_iat_through`, if any.
    pub fn revoked_through(&self, room: &str, user_id: &str, now: i64) -> Option<i64> {
        self.rooms
            .get(room)?
            .get(user_id)
            .filter(|entry| entry.deny_until > now)
            .map(|entry| entry.through)
    }

    /// Whether a join by `user_id` into `room` with `token_iat` must be refused.
    pub fn refuses(&self, room: &str, user_id: &str, token_iat: Option<i64>, now: i64) -> bool {
        self.revoked_through(room, user_id, now)
            .is_some_and(|through| issued_through(token_iat, through))
    }

    /// Drop every expired entry; returns how many went.
    pub fn sweep(&mut self, now: i64) -> usize {
        let before = self.len;
        self.rooms.retain(|_, users| {
            users.retain(|_, entry| entry.deny_until > now);
            !users.is_empty()
        });
        self.len = self.rooms.values().map(HashMap::len).sum();
        before - self.len
    }

    fn evict_oldest_in(&mut self, room: &str) {
        let Some(users) = self.rooms.get_mut(room) else {
            return;
        };
        let oldest = users
            .iter()
            .min_by(|a, b| a.1.deny_until.cmp(&b.1.deny_until).then(a.0.cmp(b.0)))
            .map(|(user, _)| user.clone());
        if let Some(user) = oldest {
            users.remove(&user);
            self.len -= 1;
        }
        if users.is_empty() {
            self.rooms.remove(room);
        }
    }

    /// The room holding the most entries, and how many.
    fn largest_room(&self) -> Option<(String, usize)> {
        self.rooms
            .iter()
            .max_by(|a, b| a.1.len().cmp(&b.1.len()).then(b.0.cmp(a.0)))
            .map(|(room, users)| (room.clone(), users.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn payload(room: &str, user: &str, through: i64, deny_until: i64) -> ParticipantKickedPayload {
        ParticipantKickedPayload {
            room_id: room.to_string(),
            user_id: user.to_string(),
            kicked_at: through,
            revoke_iat_through: through,
            deny_until,
        }
    }

    fn rev(room: &str, user: &str, through: i64, deny_until: i64) -> KickRevocation {
        KickRevocation::from_payload(payload(room, user, through, deny_until), NOW)
            .expect("valid revocation")
    }

    #[test]
    fn a_forged_future_through_is_clamped_to_relay_now_plus_slack() {
        let r = rev("room", "alice", i64::MAX, i64::MAX);
        assert_eq!(r.revoke_iat_through, NOW + KICK_THROUGH_FUTURE_SLACK_SECS);
        assert_eq!(r.deny_until, NOW + KICK_DENY_MAX_WINDOW_SECS);

        let mut list = KickDenylist::default();
        list.record(&r);
        assert!(!list.refuses(
            "room",
            "alice",
            Some(NOW + KICK_THROUGH_FUTURE_SLACK_SECS + 1),
            NOW + 60
        ));
        assert!(!list.refuses("room", "alice", Some(NOW), NOW + KICK_DENY_MAX_WINDOW_SECS));
    }

    #[test]
    fn malformed_or_expired_payloads_are_rejected() {
        for p in [
            payload("room.>", "alice", NOW, NOW + 60),
            payload("room", "", NOW, NOW + 60),
            payload("room", &"u".repeat(257), NOW, NOW + 60),
            payload("room", "alice", NOW, NOW),
        ] {
            assert_eq!(KickRevocation::from_payload(p, NOW), None);
        }
    }

    #[test]
    fn refusal_boundary_is_inclusive_of_the_kick_second() {
        let mut list = KickDenylist::default();
        list.record(&rev("room", "alice", NOW, NOW + 3600));
        assert!(list.refuses("room", "alice", None, NOW));
        assert!(list.refuses("room", "alice", Some(NOW - 5), NOW));
        assert!(list.refuses("room", "alice", Some(NOW), NOW));
        assert!(!list.refuses("room", "alice", Some(NOW + 1), NOW));
        assert!(!list.refuses("other-room", "alice", Some(NOW), NOW));
        assert!(!list.refuses("room", "bob", Some(NOW), NOW));
        assert!(!list.refuses("room", "alice", Some(NOW), NOW + 3600));
    }

    #[test]
    fn a_repeat_kick_keeps_the_later_bounds() {
        let mut list = KickDenylist::default();
        list.record(&rev("room", "alice", NOW, NOW + 100));
        list.record(&rev("room", "alice", NOW - 50, NOW + 50));
        assert_eq!(list.revoked_through("room", "alice", NOW), Some(NOW));
        assert_eq!(list.revoked_through("room", "alice", NOW + 99), Some(NOW));
        list.record(&rev("room", "alice", NOW + 2, NOW + 200));
        assert_eq!(list.revoked_through("room", "alice", NOW), Some(NOW + 2));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn sweep_drops_only_expired_entries() {
        let mut list = KickDenylist::default();
        list.record(&rev("room", "alice", NOW, NOW + 10));
        list.record(&rev("room", "bob", NOW, NOW + 100));
        assert_eq!(list.sweep(NOW + 10), 1);
        assert_eq!(list.len(), 1);
        assert_eq!(list.revoked_through("room", "bob", NOW + 10), Some(NOW));
        assert_eq!(list.sweep(NOW + 100), 1);
        assert!(list.is_empty());
    }

    #[test]
    fn a_full_room_evicts_only_its_own_oldest_entry() {
        let mut list = KickDenylist::with_caps(2, 100);
        list.record(&rev("victim", "v1", NOW, NOW + 10));
        list.record(&rev("flood", "f1", NOW, NOW + 20));
        list.record(&rev("flood", "f2", NOW, NOW + 30));
        assert_eq!(
            list.record(&rev("flood", "f3", NOW, NOW + 40)),
            Some(Eviction::RoomQuota)
        );
        assert_eq!(list.revoked_through("flood", "f1", NOW), None);
        assert_eq!(list.revoked_through("victim", "v1", NOW), Some(NOW));
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn the_global_cap_evicts_from_the_largest_room_never_a_smaller_one() {
        let mut list = KickDenylist::with_caps(100, 4);
        list.record(&rev("victim", "v1", NOW, NOW + 1));
        list.record(&rev("flood", "f1", NOW, NOW + 50));
        list.record(&rev("flood", "f2", NOW, NOW + 60));
        list.record(&rev("flood", "f3", NOW, NOW + 70));
        assert_eq!(
            list.record(&rev("third", "t1", NOW, NOW + 80)),
            Some(Eviction::GlobalCap)
        );
        assert_eq!(list.revoked_through("victim", "v1", NOW), Some(NOW));
        assert_eq!(list.revoked_through("flood", "f1", NOW), None);
        assert_eq!(list.revoked_through("third", "t1", NOW), Some(NOW));
        assert_eq!(list.len(), 4);
    }

    #[test]
    fn the_global_cap_never_evicts_from_a_room_no_larger_than_the_inserting_one() {
        let mut list = KickDenylist::with_caps(100, 5);
        list.record(&rev("inserting", "i1", NOW, NOW + 90));
        list.record(&rev("inserting", "i2", NOW, NOW + 91));
        list.record(&rev("a-peer", "p1", NOW, NOW + 1));
        list.record(&rev("a-peer", "p2", NOW, NOW + 2));
        list.record(&rev("small", "s1", NOW, NOW + 3));
        assert_eq!(
            list.record(&rev("inserting", "i3", NOW, NOW + 92)),
            Some(Eviction::GlobalCap)
        );
        assert_eq!(list.revoked_through("inserting", "i1", NOW), None);
        for (room, user) in [("a-peer", "p1"), ("a-peer", "p2"), ("small", "s1")] {
            assert_eq!(
                list.revoked_through(room, user, NOW),
                Some(NOW),
                "{room}/{user}"
            );
        }
    }

    #[test]
    fn a_global_cap_tie_evicts_from_the_inserting_room() {
        let mut list = KickDenylist::with_caps(100, 2);
        list.record(&rev("victim", "v1", NOW, NOW + 1));
        list.record(&rev("flood", "f1", NOW, NOW + 50));
        assert_eq!(
            list.record(&rev("flood", "f2", NOW, NOW + 60)),
            Some(Eviction::GlobalCap)
        );
        assert_eq!(list.revoked_through("victim", "v1", NOW), Some(NOW));
        assert_eq!(list.revoked_through("flood", "f2", NOW), Some(NOW));
    }
}

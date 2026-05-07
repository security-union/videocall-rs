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

//! Shared packet handling logic for session actors.
//!
//! This module provides common packet classification and processing
//! used by both `WsChatSession` and `WtChatSession`.

use protobuf::Message as ProtobufMessage;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

use crate::constants::{
    KEYFRAME_LIMITER_CLEANUP_INTERVAL, KEYFRAME_REQUEST_MAX_PER_SEC,
    KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER, KEYFRAME_REQUEST_WINDOW_MS,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Classification of an incoming packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacketKind {
    /// RTT (Round-Trip Time) packet - should be echoed back to sender
    Rtt,
    /// Health diagnostics packet - should be processed for metrics
    Health,
    /// Normal data packet - should be forwarded to ChatServer
    Data,
    /// Packet that should be silently dropped (e.g., client-originated CONGESTION)
    Dropped,
    /// KEYFRAME_REQUEST packet - subject to per-(receiver, target_sender)
    /// rate limiting. The embedded `target_user_id` is the user_id of the
    /// peer whose video the receiver wants a keyframe from, taken from the
    /// inner `MediaPacket.user_id` field. May be empty if the client sent
    /// a malformed request, in which case the limiter still enforces a key
    /// (the empty target acts as a single bucket).
    KeyframeRequest { target_user_id: Vec<u8> },
}

/// Classify a packet based on its contents.
///
/// Parses the `PacketWrapper` exactly once and uses the `packet_type` field
/// to classify the packet. For MEDIA packets, the inner `MediaPacket` is
/// parsed at most once to distinguish RTT and KEYFRAME_REQUEST from regular
/// media data.
///
/// # Arguments
/// * `data` - Raw packet bytes
///
/// # Returns
/// The classification of the packet
pub fn classify_packet(data: &[u8]) -> PacketKind {
    let packet_wrapper = match PacketWrapper::parse_from_bytes(data) {
        Ok(pw) => pw,
        Err(_) => return PacketKind::Data, // unparseable, treat as opaque data
    };

    // Drop client-originated CONGESTION packets.
    // CONGESTION signals must only originate from the server's CongestionTracker,
    // never from clients. A malicious client could craft a CONGESTION packet with
    // a victim's session_id to force them to degrade video quality.
    if packet_wrapper.packet_type == PacketType::CONGESTION.into() {
        return PacketKind::Dropped;
    }

    // Check if it's a MEDIA packet (RTT, keyframe request, or regular media).
    if packet_wrapper.packet_type == PacketType::MEDIA.into() {
        // Try to parse inner MediaPacket to distinguish control sub-types.
        // For encrypted payloads this parse will fail, correctly falling
        // through to PacketKind::Data.
        if let Ok(media_packet) = MediaPacket::parse_from_bytes(&packet_wrapper.data) {
            if media_packet.media_type == MediaType::RTT.into() {
                return PacketKind::Rtt;
            }
            if media_packet.media_type == MediaType::KEYFRAME_REQUEST.into() {
                // The inner MediaPacket.user_id identifies the target peer
                // (the sender whose video should produce a keyframe). The
                // outer wrapper's session_id is unset by the client, so we
                // key the per-pair limiter by the inner user_id. user_id is
                // stable across reconnects of the same participant, so the
                // limiter state survives transient drops correctly.
                return PacketKind::KeyframeRequest {
                    target_user_id: media_packet.user_id,
                };
            }
        }
        return PacketKind::Data;
    }

    // Check health packet.
    if packet_wrapper.packet_type == PacketType::HEALTH.into() {
        return PacketKind::Health;
    }

    PacketKind::Data
}

/// Sliding-window counter for one rate-limit bucket.
///
/// Used both for the global per-receiver cap and for each
/// `(receiver, target_sender)` pair entry inside the limiter.
struct WindowCounter {
    count: u32,
    window_start: Instant,
}

impl WindowCounter {
    fn new(now: Instant) -> Self {
        Self {
            count: 0,
            window_start: now,
        }
    }

    /// Try to consume one slot from this bucket within the configured
    /// `window` and `max` capacity. Returns true if accepted, false if the
    /// bucket is saturated for the current window.
    fn try_consume(&mut self, now: Instant, window: Duration, max: u32) -> bool {
        if now.duration_since(self.window_start) > window {
            self.count = 0;
            self.window_start = now;
        }
        if self.count < max {
            self.count += 1;
            true
        } else {
            false
        }
    }
}

/// Per-receiver, per-target-sender rate limiter for KEYFRAME_REQUEST packets.
///
/// Each receiver session owns one `KeyframeRequestLimiter`. The limiter
/// enforces two layers of throttling:
///
/// 1. **Per-target-sender** (primary): each `(receiver, target_sender)` pair
///    gets its own [`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER`] budget per
///    [`KEYFRAME_REQUEST_WINDOW_MS`]. This is what allows a fresh joiner to
///    request keyframes from many existing senders simultaneously without
///    being clipped by a single global counter.
/// 2. **Global per-receiver** (defense-in-depth): a coarse cap of
///    [`KEYFRAME_REQUEST_MAX_PER_SEC`] across all targets in the same
///    window. This bounds total fan-out from any single receiver as a
///    safety net against bursty or malicious behaviour.
///
/// Memory bound: the per-pair table cleans up entries that have not been
/// touched for `KEYFRAME_REQUEST_WINDOW_MS * 10` (10 seconds), running every
/// [`KEYFRAME_LIMITER_CLEANUP_INTERVAL`] calls to amortize the O(n)
/// `retain()` cost. This mirrors `CongestionTracker::record_drop` so the
/// strategy is consistent across the relay.
pub struct KeyframeRequestLimiter {
    /// Global counter across all target senders for this receiver.
    global: WindowCounter,
    /// Per-target-sender counters, keyed by the target's user_id bytes.
    per_target: HashMap<Vec<u8>, WindowCounter>,
    /// Total `allow()` calls since the last cleanup. Cleanup runs every
    /// [`KEYFRAME_LIMITER_CLEANUP_INTERVAL`] calls.
    calls_since_cleanup: u32,
}

impl Default for KeyframeRequestLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyframeRequestLimiter {
    pub fn new() -> Self {
        Self {
            global: WindowCounter::new(Instant::now()),
            per_target: HashMap::new(),
            calls_since_cleanup: 0,
        }
    }

    /// Check whether a KEYFRAME_REQUEST aimed at `target_user_id` should be
    /// allowed through. Both the per-pair budget and the global cap must
    /// admit the request.
    ///
    /// Behaviour:
    /// - If the per-pair bucket is full, returns `false` and does not
    ///   consume the global slot (so a deny on one target does not eat
    ///   budget intended for others).
    /// - If the per-pair bucket admits but the global bucket is full,
    ///   returns `false` and the per-pair slot already consumed is
    ///   refunded so the legitimate next pair retains its allowance.
    ///
    /// Stale entries (target senders that have not been requested from
    /// for `KEYFRAME_REQUEST_WINDOW_MS * 10`) are cleaned up every
    /// [`KEYFRAME_LIMITER_CLEANUP_INTERVAL`] calls to bound memory.
    pub fn allow(&mut self, target_user_id: &[u8]) -> bool {
        let now = Instant::now();
        let window = Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS);

        self.calls_since_cleanup = self.calls_since_cleanup.wrapping_add(1);
        if self.calls_since_cleanup >= KEYFRAME_LIMITER_CLEANUP_INTERVAL {
            self.calls_since_cleanup = 0;
            self.cleanup_stale_entries(now, window);
        }

        // Per-pair check first: this is the dimension that actually
        // discriminates a 16-sender fan-out from sustained abuse.
        let per_pair_entry = self
            .per_target
            .entry(target_user_id.to_vec())
            .or_insert_with(|| WindowCounter::new(now));
        if !per_pair_entry.try_consume(now, window, KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER) {
            return false;
        }

        // Global cap as a defense-in-depth ceiling. If exceeded, refund
        // the per-pair slot we just consumed so legitimate distinct-target
        // requests are not penalized for hitting the global ceiling.
        if !self
            .global
            .try_consume(now, window, KEYFRAME_REQUEST_MAX_PER_SEC)
        {
            // The per-pair entry's `count` was incremented above; decrement
            // it to undo. The entry is guaranteed to exist because we just
            // inserted/incremented it.
            if let Some(entry) = self.per_target.get_mut(target_user_id) {
                entry.count = entry.count.saturating_sub(1);
            }
            return false;
        }

        true
    }

    /// Drop per-target entries whose window has been silent for
    /// `window * 10` to keep the table size bounded.
    fn cleanup_stale_entries(&mut self, now: Instant, window: Duration) {
        let stale_threshold = window * 10;
        self.per_target
            .retain(|_, entry| now.duration_since(entry.window_start) <= stale_threshold);
    }
}

/// Maximum payload size for WebTransport datagrams (bytes).
///
/// Datagrams are used for control packets (heartbeats, RTT probes,
/// diagnostics) that are periodic and expendable. Media packets always use
/// reliable unidirectional streams. Control packets larger than this limit
/// also fall back to reliable streams.
///
/// Must match the client-side `DATAGRAM_MAX_SIZE` constant.
pub const DATAGRAM_MAX_SIZE: usize = 1200;

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Test-only helper functions
    //
    // These standalone is_* functions are used only by their own unit tests.
    // Production code uses `classify_packet()` instead.
    // =========================================================================

    /// Check if a packet is a CONGESTION packet (test-only helper).
    fn is_congestion_packet(data: &[u8]) -> bool {
        if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
            return packet_wrapper.packet_type == PacketType::CONGESTION.into();
        }
        false
    }

    /// Check if a packet is an RTT measurement packet (test-only helper).
    fn is_rtt_packet(data: &[u8]) -> bool {
        if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
            if packet_wrapper.packet_type == PacketType::MEDIA.into() {
                if let Ok(media_packet) = MediaPacket::parse_from_bytes(&packet_wrapper.data) {
                    return media_packet.media_type == MediaType::RTT.into();
                }
            }
        }
        false
    }

    /// Check if a MEDIA packet contains a KEYFRAME_REQUEST (test-only helper).
    fn is_keyframe_request(data: &[u8]) -> bool {
        if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
            if packet_wrapper.packet_type == PacketType::MEDIA.into() {
                if let Ok(media_packet) = MediaPacket::parse_from_bytes(&packet_wrapper.data) {
                    return media_packet.media_type == MediaType::KEYFRAME_REQUEST.into();
                }
            }
        }
        false
    }

    /// Test-only helper that replicates the datagram routing logic from
    /// `WtChatSession::send_auto`. Control packets (non-media) that fit
    /// within the datagram MTU use datagrams; media packets always use
    /// reliable streams. Empty/unparseable inputs are never routed via
    /// datagram.
    fn should_use_datagram(data: &[u8]) -> bool {
        if data.is_empty() {
            return false;
        }
        if let Ok(pw) = PacketWrapper::parse_from_bytes(data) {
            let is_media = pw.packet_type == PacketType::MEDIA.into();
            return !is_media && data.len() <= DATAGRAM_MAX_SIZE;
        }
        false
    }

    #[test]
    fn test_classify_empty_packet_as_data() {
        assert_eq!(classify_packet(&[]), PacketKind::Data);
    }

    #[test]
    fn test_classify_garbage_as_data() {
        assert_eq!(classify_packet(&[1, 2, 3, 4, 5]), PacketKind::Data);
    }

    #[test]
    fn test_is_rtt_packet_with_garbage() {
        assert!(!is_rtt_packet(&[1, 2, 3, 4, 5]));
    }

    #[test]
    fn test_is_rtt_packet_with_empty() {
        assert!(!is_rtt_packet(&[]));
    }

    #[test]
    fn test_should_use_datagram_empty() {
        assert!(!should_use_datagram(&[]));
    }

    #[test]
    fn test_should_use_datagram_garbage() {
        assert!(!should_use_datagram(&[1, 2, 3, 4, 5]));
    }

    #[test]
    fn test_should_use_datagram_media_packet() {
        // MEDIA packets always use reliable streams (avoids artifacts)
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: vec![1, 2, 3], // small payload
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(bytes.len() <= DATAGRAM_MAX_SIZE);
        assert!(!should_use_datagram(&bytes));
    }

    #[test]
    fn test_should_use_datagram_oversized_media_packet() {
        // Oversized MEDIA packets also use reliable streams
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: vec![0u8; DATAGRAM_MAX_SIZE + 100], // exceeds MTU
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(!should_use_datagram(&bytes));
    }

    #[test]
    fn test_should_use_datagram_non_media_packet() {
        // Small AES_KEY packets use datagrams (control, expendable)
        let wrapper = PacketWrapper {
            packet_type: PacketType::AES_KEY.into(),
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(should_use_datagram(&bytes));
    }

    #[test]
    fn test_should_use_datagram_diagnostics_packet() {
        // Small DIAGNOSTICS packets use datagrams (periodic, expendable)
        let wrapper = PacketWrapper {
            packet_type: PacketType::DIAGNOSTICS.into(),
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(should_use_datagram(&bytes));
    }

    #[test]
    fn test_should_use_datagram_health_packet() {
        // Small HEALTH packets use datagrams (periodic, expendable)
        let wrapper = PacketWrapper {
            packet_type: PacketType::HEALTH.into(),
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(should_use_datagram(&bytes));
    }

    #[test]
    fn test_should_use_datagram_oversized_control_packet() {
        // Control packets exceeding DATAGRAM_MAX_SIZE fall back to reliable stream
        let wrapper = PacketWrapper {
            packet_type: PacketType::DIAGNOSTICS.into(),
            data: vec![0u8; DATAGRAM_MAX_SIZE + 100],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(!should_use_datagram(&bytes));
    }

    #[test]
    fn test_classify_congestion_packet_as_dropped() {
        let wrapper = PacketWrapper {
            packet_type: PacketType::CONGESTION.into(),
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(classify_packet(&bytes), PacketKind::Dropped);
    }

    #[test]
    fn test_classify_keyframe_request() {
        // Build a KEYFRAME_REQUEST aimed at "alice" so we can also verify
        // that the inner MediaPacket.user_id is propagated through to the
        // PacketKind variant. This is what feeds the per-pair limiter key.
        let media = MediaPacket {
            media_type: MediaType::KEYFRAME_REQUEST.into(),
            user_id: b"alice".to_vec(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(
            classify_packet(&bytes),
            PacketKind::KeyframeRequest {
                target_user_id: b"alice".to_vec(),
            }
        );
    }

    #[test]
    fn test_classify_keyframe_request_with_empty_target() {
        // A malformed KEYFRAME_REQUEST without a target user_id is still
        // classified as KeyframeRequest. The limiter then uses the empty
        // key, treating all such packets as a single bucket.
        let media = MediaPacket {
            media_type: MediaType::KEYFRAME_REQUEST.into(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(
            classify_packet(&bytes),
            PacketKind::KeyframeRequest {
                target_user_id: Vec::new(),
            }
        );
    }

    #[test]
    fn test_classify_rtt_packet() {
        let media = MediaPacket {
            media_type: MediaType::RTT.into(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(classify_packet(&bytes), PacketKind::Rtt);
    }

    #[test]
    fn test_classify_health_packet() {
        let wrapper = PacketWrapper {
            packet_type: PacketType::HEALTH.into(),
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(classify_packet(&bytes), PacketKind::Health);
    }

    #[test]
    fn test_classify_regular_media_as_data() {
        let media = MediaPacket {
            media_type: MediaType::VIDEO.into(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(classify_packet(&bytes), PacketKind::Data);
    }

    #[test]
    fn test_is_congestion_packet_true() {
        let wrapper = PacketWrapper {
            packet_type: PacketType::CONGESTION.into(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(is_congestion_packet(&bytes));
    }

    #[test]
    fn test_is_congestion_packet_false_for_media() {
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(!is_congestion_packet(&bytes));
    }

    #[test]
    fn test_is_keyframe_request_with_valid_packet() {
        let media = MediaPacket {
            media_type: MediaType::KEYFRAME_REQUEST.into(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(is_keyframe_request(&bytes));
    }

    #[test]
    fn test_is_keyframe_request_false_for_video() {
        let media = MediaPacket {
            media_type: MediaType::VIDEO.into(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(!is_keyframe_request(&bytes));
    }

    #[test]
    fn test_is_keyframe_request_false_for_non_media() {
        let wrapper = PacketWrapper {
            packet_type: PacketType::AES_KEY.into(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(!is_keyframe_request(&bytes));
    }

    // =====================================================================
    // KeyframeRequestLimiter — per-pair behaviour
    // =====================================================================

    #[test]
    fn test_keyframe_limiter_allows_first_request_per_target() {
        let mut limiter = KeyframeRequestLimiter::new();
        assert!(limiter.allow(b"sender-a"));
    }

    #[test]
    fn test_keyframe_limiter_blocks_second_request_within_window_same_target() {
        // Same target, second request inside the window must be denied.
        // This is the classic per-pair throttle on a single relationship.
        let mut limiter = KeyframeRequestLimiter::new();
        assert!(limiter.allow(b"sender-a"));
        assert!(
            !limiter.allow(b"sender-a"),
            "second request to the same sender within 1s must be denied"
        );
    }

    #[test]
    fn test_keyframe_limiter_allows_fanout_across_distinct_targets() {
        // The frozen-video-on-join repro: a fresh joiner needs keyframes
        // from all 16 existing senders simultaneously. With the per-pair
        // limiter all 16 must succeed within the same second.
        let mut limiter = KeyframeRequestLimiter::new();
        for i in 0..16 {
            let target = format!("sender-{}", i);
            assert!(
                limiter.allow(target.as_bytes()),
                "first request to sender-{} should be allowed (i={})",
                i,
                i
            );
        }
    }

    #[test]
    fn test_keyframe_limiter_allows_same_target_after_window_elapses() {
        // Force the per-pair window to look elapsed by manually rewinding
        // the bucket's window_start. We avoid `tokio::time::sleep` so the
        // test stays cheap and deterministic.
        let mut limiter = KeyframeRequestLimiter::new();
        let target = b"sender-x";
        assert!(limiter.allow(target));

        // Push the bucket's window_start ~1.5s into the past.
        let entry = limiter.per_target.get_mut(target.as_slice()).unwrap();
        entry.window_start =
            Instant::now() - Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS + 500);

        assert!(
            limiter.allow(target),
            "second request to the same sender after the window elapses must be allowed"
        );
    }

    #[test]
    fn test_keyframe_limiter_per_target_is_independent() {
        // Exhausting one (receiver, target) pair must not affect another.
        let mut limiter = KeyframeRequestLimiter::new();
        assert!(limiter.allow(b"sender-a"));
        assert!(!limiter.allow(b"sender-a"));

        // sender-b is a fresh pair — must still admit its first request.
        assert!(limiter.allow(b"sender-b"));
    }

    #[test]
    fn test_keyframe_limiter_global_cap_blocks_runaway_fanout() {
        // The defense-in-depth global cap kicks in when a single receiver
        // requests from more distinct targets than KEYFRAME_REQUEST_MAX_PER_SEC.
        let mut limiter = KeyframeRequestLimiter::new();
        for i in 0..KEYFRAME_REQUEST_MAX_PER_SEC {
            let target = format!("t-{}", i);
            assert!(limiter.allow(target.as_bytes()));
        }
        // One more distinct target inside the same window must be denied
        // by the global cap.
        let extra = format!("t-{}", KEYFRAME_REQUEST_MAX_PER_SEC);
        assert!(
            !limiter.allow(extra.as_bytes()),
            "global per-receiver cap must clamp runaway fan-out"
        );
    }

    #[test]
    fn test_keyframe_limiter_global_cap_does_not_consume_per_pair_budget_on_deny() {
        // When the global cap denies, the per-pair budget for the denied
        // target must be refunded so the legitimate next call (after the
        // global window elapses) is admitted.
        let mut limiter = KeyframeRequestLimiter::new();
        // Fill the global cap with distinct targets.
        for i in 0..KEYFRAME_REQUEST_MAX_PER_SEC {
            let target = format!("t-{}", i);
            assert!(limiter.allow(target.as_bytes()));
        }
        // This pair's first request is denied by the global cap.
        let pair = b"t-victim";
        assert!(!limiter.allow(pair));

        // Manually expire only the global window (simulating ~1s passing
        // for the global cap while the per-pair entry was just refunded).
        limiter.global.window_start =
            Instant::now() - Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS + 500);

        // The per-pair budget was refunded, so the pair's first legitimate
        // request after the global cap reopens must be allowed.
        assert!(
            limiter.allow(pair),
            "per-pair budget must be refunded when global cap denies"
        );
    }

    #[test]
    fn test_keyframe_limiter_cleanup_removes_only_stale_entries() {
        // Insert a synthetic stale entry (silent for >10*window) and a
        // synthetic fresh entry. After cleanup runs only the fresh one
        // (and any newly active pair) survives.
        let mut limiter = KeyframeRequestLimiter::new();
        let now = Instant::now();
        let window = Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS);

        limiter.per_target.insert(
            b"stale".to_vec(),
            WindowCounter {
                count: 0,
                window_start: now - (window * 20),
            },
        );
        limiter.per_target.insert(
            b"fresh".to_vec(),
            WindowCounter {
                count: 0,
                window_start: now,
            },
        );

        // Force the next allow() call to trigger cleanup.
        limiter.calls_since_cleanup = KEYFRAME_LIMITER_CLEANUP_INTERVAL - 1;
        assert!(limiter.allow(b"trigger"));

        assert!(
            !limiter.per_target.contains_key(b"stale".as_slice()),
            "stale entry must be removed by cleanup"
        );
        assert!(
            limiter.per_target.contains_key(b"fresh".as_slice()),
            "fresh entry must be retained by cleanup"
        );
        assert!(
            limiter.per_target.contains_key(b"trigger".as_slice()),
            "the active pair that triggered cleanup must remain"
        );
    }

    #[test]
    fn test_keyframe_limiter_cleanup_does_not_evict_active_pair_state() {
        // Required by the change spec: cleanup must not prematurely clear
        // active-pair state. Specifically, an entry whose window_start is
        // `now - window * 5` (well within the 10x boundary) must survive.
        let mut limiter = KeyframeRequestLimiter::new();
        let now = Instant::now();
        let window = Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS);

        limiter.per_target.insert(
            b"active".to_vec(),
            WindowCounter {
                count: 1, // mid-window allowance already consumed
                window_start: now - (window * 5),
            },
        );

        limiter.calls_since_cleanup = KEYFRAME_LIMITER_CLEANUP_INTERVAL - 1;
        assert!(limiter.allow(b"unrelated"));

        let entry = limiter
            .per_target
            .get(b"active".as_slice())
            .expect("active pair must survive cleanup");
        assert_eq!(
            entry.count, 1,
            "active pair's count must not be reset by cleanup"
        );
    }

    #[test]
    fn test_keyframe_limiter_cleanup_amortized_not_every_call() {
        // The cleanup pass must run only every KEYFRAME_LIMITER_CLEANUP_INTERVAL
        // calls, not on every single allow(). Insert a stale entry and verify
        // it survives until the cleanup boundary is crossed.
        let mut limiter = KeyframeRequestLimiter::new();
        let now = Instant::now();
        let window = Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS);

        limiter.per_target.insert(
            b"stale".to_vec(),
            WindowCounter {
                count: 0,
                window_start: now - (window * 20),
            },
        );

        // Issue strictly fewer calls than the cleanup threshold. Use a
        // distinct fresh target each call to avoid global cap denial.
        for i in 0..(KEYFRAME_LIMITER_CLEANUP_INTERVAL - 1) {
            let target = format!("tick-{}", i);
            // Some calls will be denied by the global cap once it fills;
            // we don't care about return value, only that we drove the
            // call counter close to the boundary.
            let _ = limiter.allow(target.as_bytes());
        }

        assert!(
            limiter.per_target.contains_key(b"stale".as_slice()),
            "stale entry must persist below the cleanup threshold (amortized)"
        );
    }
}

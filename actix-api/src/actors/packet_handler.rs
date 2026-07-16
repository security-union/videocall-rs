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

use crate::constants::{KEYFRAME_REQUEST_MAX_PER_SEC, KEYFRAME_REQUEST_WINDOW_MS};
use std::time::Instant;

/// Classification of an incoming packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketKind {
    /// RTT (Round-Trip Time) packet - should be echoed back to sender
    Rtt,
    /// Health diagnostics packet - should be processed for metrics
    Health,
    /// Normal data packet - should be forwarded to ChatServer
    Data,
    /// Packet that should be silently dropped (e.g., client-originated CONGESTION)
    Dropped,
    /// KEYFRAME_REQUEST packet - subject to per-session rate limiting
    KeyframeRequest,
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

    // Drop client-originated SESSION_ASSIGNED packets.
    // SESSION_ASSIGNED is authored solely by the server (ws/wt session actors)
    // to tell a client its authoritative session_id. If a client sends one it
    // would otherwise be relayed as Data, letting a peer adopt the forged
    // session_id as its own before the self-filter runs (a peer-tile-hijack).
    if packet_wrapper.packet_type == PacketType::SESSION_ASSIGNED.into() {
        return PacketKind::Dropped;
    }

    // Drop client-originated MEETING packets.
    // MEETING packets (MEETING_STARTED/ENDED, PARTICIPANT_JOINED/LEFT) are
    // authored solely by the server's SessionManager. They are the SOLE source
    // of peer identity on the receiving client (session_id -> {email,
    // display_name}). A relayed forged MEETING PARTICIPANT_JOINED would let an
    // authenticated attacker rewrite an arbitrary session's identity in every
    // peer's cache (impersonation, spoofed host badges, mis-targeted
    // moderation). Production clients never send MEETING; only the server does,
    // and server-authored MEETING packets go outbound (never through this
    // inbound classifier), so dropping inbound client MEETING is safe.
    if packet_wrapper.packet_type == PacketType::MEETING.into() {
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
                return PacketKind::KeyframeRequest;
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

/// Per-session rate limiter for KEYFRAME_REQUEST packets.
///
/// Tracks the number of KEYFRAME_REQUEST packets forwarded within a sliding
/// window and drops excess requests to prevent abuse.
pub struct KeyframeRequestLimiter {
    /// Number of requests forwarded in the current window.
    count: u32,
    /// Start of the current counting window.
    window_start: Instant,
}

impl Default for KeyframeRequestLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyframeRequestLimiter {
    pub fn new() -> Self {
        Self {
            count: 0,
            window_start: Instant::now(),
        }
    }

    /// Check whether a KEYFRAME_REQUEST should be allowed through.
    ///
    /// Returns `true` if the request is within the rate limit, `false` if it
    /// should be dropped. Automatically resets the window when it expires.
    pub fn allow(&mut self) -> bool {
        let now = Instant::now();
        let window = std::time::Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS);

        if now.duration_since(self.window_start) > window {
            self.count = 0;
            self.window_start = now;
        }

        if self.count < KEYFRAME_REQUEST_MAX_PER_SEC {
            self.count += 1;
            true
        } else {
            false
        }
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
        assert_eq!(classify_packet(&bytes), PacketKind::KeyframeRequest);
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
    fn test_classify_session_assigned_from_client_dropped() {
        // A client has no business sending SESSION_ASSIGNED — it is server-authored.
        // If relayed, a peer would adopt the forged session_id as its own before the
        // client-side self-filter runs (peer-tile-hijack). It must be Dropped, never Data.
        let wrapper = PacketWrapper {
            packet_type: PacketType::SESSION_ASSIGNED.into(),
            session_id: 9999,
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(classify_packet(&bytes), PacketKind::Dropped);
    }

    #[test]
    fn test_classify_meeting_from_client_dropped() {
        // MEETING packets are server-authored only and are the sole source of
        // peer identity on the receiving client. A relayed forged MEETING
        // (e.g. PARTICIPANT_JOINED with an attacker-chosen target_user_id and
        // session_id) would poison every peer's identity cache. A client MEETING
        // packet must be Dropped, never relayed as Data.
        use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
        use videocall_types::protos::meeting_packet::MeetingPacket;
        let meeting = MeetingPacket {
            event_type: MeetingEventType::PARTICIPANT_JOINED.into(),
            session_id: 4242,
            target_user_id: b"ceo@company.com".to_vec(),
            display_name: b"CEO".to_vec(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEETING.into(),
            data: meeting.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(classify_packet(&bytes), PacketKind::Dropped);
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

    #[test]
    fn test_keyframe_request_limiter_allows_within_limit() {
        let mut limiter = KeyframeRequestLimiter::new();
        assert!(limiter.allow());
        assert!(limiter.allow());
    }

    #[test]
    fn test_keyframe_request_limiter_blocks_over_limit() {
        let mut limiter = KeyframeRequestLimiter::new();
        // Exhaust the limit
        for _ in 0..KEYFRAME_REQUEST_MAX_PER_SEC {
            assert!(limiter.allow());
        }
        // Next one should be blocked
        assert!(!limiter.allow());
    }
}

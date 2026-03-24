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

use crate::client_diagnostics::health_processor;
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
}

/// Classify a packet based on its contents.
///
/// # Arguments
/// * `data` - Raw packet bytes
///
/// # Returns
/// The classification of the packet
pub fn classify_packet(data: &[u8]) -> PacketKind {
    // Drop client-originated CONGESTION packets.
    // CONGESTION signals must only originate from the server's CongestionTracker,
    // never from clients. A malicious client could craft a CONGESTION packet with
    // a victim's session_id to force them to degrade video quality.
    if is_congestion_packet(data) {
        return PacketKind::Dropped;
    }

    // Check RTT first (most specific check)
    if is_rtt_packet(data) {
        return PacketKind::Rtt;
    }

    // Check health packet
    if health_processor::is_health_packet_bytes(data) {
        return PacketKind::Health;
    }

    // Default to data packet
    PacketKind::Data
}

/// Check if a packet is a CONGESTION packet.
///
/// CONGESTION packets must only originate from the server's `CongestionTracker`.
/// Client-originated CONGESTION packets are dropped to prevent a malicious client
/// from forcing victims to degrade their video quality.
pub fn is_congestion_packet(data: &[u8]) -> bool {
    if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
        return packet_wrapper.packet_type == PacketType::CONGESTION.into();
    }
    false
}

/// Check if a packet is an RTT (Round-Trip Time) measurement packet.
///
/// RTT packets are used to measure network latency and should be
/// echoed back to the sender immediately without forwarding to other peers.
pub fn is_rtt_packet(data: &[u8]) -> bool {
    if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
        if packet_wrapper.packet_type == PacketType::MEDIA.into() {
            if let Ok(media_packet) = MediaPacket::parse_from_bytes(&packet_wrapper.data) {
                return media_packet.media_type == MediaType::RTT.into();
            }
        }
    }
    false
}

/// Check if a MEDIA packet contains a KEYFRAME_REQUEST.
///
/// Attempts to parse the inner `MediaPacket` from the `PacketWrapper.data` field.
/// If the inner packet is AES-encrypted (as in normal media flow), parsing will
/// fail and this returns `false`. This check is effective for unencrypted control
/// packets and serves as an additional defence layer.
pub fn is_keyframe_request(data: &[u8]) -> bool {
    if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
        if packet_wrapper.packet_type == PacketType::MEDIA.into() {
            if let Ok(media_packet) = MediaPacket::parse_from_bytes(&packet_wrapper.data) {
                return media_packet.media_type == MediaType::KEYFRAME_REQUEST.into();
            }
        }
    }
    false
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
/// Must match the client-side `DATAGRAM_MAX_SIZE` constant. Packets larger
/// than this are sent via reliable unidirectional streams instead.
pub const DATAGRAM_MAX_SIZE: usize = 1200;

/// Check if a packet contains real-time media data (VIDEO, AUDIO, or SCREEN)
/// that benefits from low-latency datagram delivery.
///
/// Control packets (HEARTBEAT, RTT, KEYFRAME_REQUEST, DIAGNOSTICS, HEALTH)
/// are NOT considered media for this purpose because they require reliable
/// delivery.
///
/// Note: the inner `MediaPacket` is AES-encrypted, so we cannot inspect the
/// `media_type` field without decryption. However, the `PacketWrapper` still
/// has the unencrypted `packet_type` field. Since only MEDIA packet types
/// contain real-time audio/video/screen data, we use a size-based heuristic:
/// MEDIA packets that are small enough for datagrams are sent unreliably.
/// Large MEDIA packets (e.g., keyframes) fall back to streams.
///
/// This conservative approach means some control-type MEDIA packets (like
/// HEARTBEAT) that happen to be small could be sent via datagram, but since
/// heartbeats are also sent periodically, occasional loss is acceptable.
pub fn should_use_datagram(data: &[u8]) -> bool {
    if data.len() > DATAGRAM_MAX_SIZE {
        return false;
    }

    // Only use datagrams for MEDIA packet type.
    // Other types (RSA_PUB_KEY, AES_KEY, CONNECTION, DIAGNOSTICS, HEALTH,
    // MEETING, SESSION_ASSIGNED, CONGESTION) must use reliable streams.
    if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
        return packet_wrapper.packet_type == PacketType::MEDIA.into();
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: vec![1, 2, 3], // small payload
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(bytes.len() <= DATAGRAM_MAX_SIZE);
        assert!(should_use_datagram(&bytes));
    }

    #[test]
    fn test_should_use_datagram_oversized_media_packet() {
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
        // AES_KEY packets should always use reliable stream
        let wrapper = PacketWrapper {
            packet_type: PacketType::AES_KEY.into(),
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(!should_use_datagram(&bytes));
    }

    #[test]
    fn test_should_use_datagram_diagnostics_packet() {
        // DIAGNOSTICS packets should always use reliable stream
        let wrapper = PacketWrapper {
            packet_type: PacketType::DIAGNOSTICS.into(),
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(!should_use_datagram(&bytes));
    }

    #[test]
    fn test_should_use_datagram_health_packet() {
        // HEALTH packets should always use reliable stream
        let wrapper = PacketWrapper {
            packet_type: PacketType::HEALTH.into(),
            data: vec![1, 2, 3],
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

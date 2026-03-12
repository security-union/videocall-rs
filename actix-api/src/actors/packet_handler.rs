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

/// Classification of an incoming packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketKind {
    /// RTT (Round-Trip Time) packet - should be echoed back to sender
    Rtt,
    /// Health diagnostics packet - should be processed for metrics
    Health,
    /// Normal data packet - should be forwarded to ChatServer
    Data,
}

/// Classify a packet based on its contents.
///
/// # Arguments
/// * `data` - Raw packet bytes
///
/// # Returns
/// The classification of the packet
pub fn classify_packet(data: &[u8]) -> PacketKind {
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
}

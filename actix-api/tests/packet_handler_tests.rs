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

//! Integration tests for the packet handler module.
//!
//! Previously these lived inside `src/actors/packet_handler.rs` as a `#[cfg(test)] mod tests`
//! block. They have been extracted here so that:
//! 1. The production source file stays focused on production code.
//! 2. Tests compile as integration tests and run via `cargo test -p videocall-api`.

use protobuf::Message as ProtobufMessage;
use sec_api::actors::packet_handler::{
    classify_packet, KeyframeRequestLimiter, PacketKind, DATAGRAM_MAX_SIZE,
};
use sec_api::constants::KEYFRAME_REQUEST_MAX_PER_SEC;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

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
    // MEDIA packets always use reliable streams, never datagrams.
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
fn test_should_use_datagram_oversized_diagnostics_packet() {
    // DIAGNOSTICS packets that exceed DATAGRAM_MAX_SIZE fall back to reliable streams.
    let wrapper = PacketWrapper {
        packet_type: PacketType::DIAGNOSTICS.into(),
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
    // Small DIAGNOSTICS packets use datagrams (periodic, expendable).
    let wrapper = PacketWrapper {
        packet_type: PacketType::DIAGNOSTICS.into(),
        data: vec![1, 2, 3],
        ..Default::default()
    };
    let bytes = wrapper.write_to_bytes().unwrap();
    assert!(bytes.len() <= DATAGRAM_MAX_SIZE);
    assert!(should_use_datagram(&bytes));
}

#[test]
fn test_should_use_datagram_health_packet() {
    // Small HEALTH packets use datagrams (periodic, expendable).
    let wrapper = PacketWrapper {
        packet_type: PacketType::HEALTH.into(),
        data: vec![1, 2, 3],
        ..Default::default()
    };
    let bytes = wrapper.write_to_bytes().unwrap();
    assert!(bytes.len() <= DATAGRAM_MAX_SIZE);
    assert!(should_use_datagram(&bytes));
}

#[test]
fn test_should_use_datagram_oversized_control_packet() {
    // Control packets exceeding DATAGRAM_MAX_SIZE fall back to reliable streams.
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

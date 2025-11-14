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

//! FlatBuffer message types for the videocall streaming platform.
//!
//! This crate provides FlatBuffer-based message serialization for efficient
//! network communication in the videocall system.

#![allow(unused_imports)]
#![allow(dead_code)]
#![allow(clippy::all)]

// Re-export flatbuffers crate
pub use flatbuffers;

// Include each generated file separately with mod declaration
#[path = "generated/aes_packet_generated.rs"]
pub mod aes_packet_generated;

#[path = "generated/connection_packet_generated.rs"]
pub mod connection_packet_generated;

#[path = "generated/diagnostics_packet_generated.rs"]
pub mod diagnostics_packet_generated;

#[path = "generated/health_packet_generated.rs"]
pub mod health_packet_generated;

#[path = "generated/media_packet_generated.rs"]
pub mod media_packet_generated;

#[path = "generated/packet_wrapper_generated.rs"]
pub mod packet_wrapper_generated;

#[path = "generated/rsa_packet_generated.rs"]
pub mod rsa_packet_generated;

#[path = "generated/server_connection_packet_generated.rs"]
pub mod server_connection_packet_generated;

// Re-export the main types for convenience
pub use aes_packet_generated::videocall::protocol::AesPacket;
pub use connection_packet_generated::videocall::protocol::ConnectionPacket;
pub use diagnostics_packet_generated::videocall::protocol::{
    AudioMetrics as DiagnosticsAudioMetrics, DiagnosticsPacket,
    MediaType as DiagnosticsMediaType, VideoMetrics as DiagnosticsVideoMetrics,
};
pub use health_packet_generated::videocall::protocol::{
    HealthPacket, NetEqNetwork, NetEqOperationCounters, NetEqStats, PeerStats, PeerStatsEntry,
    VideoStats,
};
pub use media_packet_generated::videocall::protocol::{
    AudioMetadata, HeartbeatMetadata, MediaPacket, MediaType, VideoMetadata,
};
pub use packet_wrapper_generated::videocall::protocol::{PacketType, PacketWrapper};
pub use rsa_packet_generated::videocall::protocol::RsaPacket;
pub use server_connection_packet_generated::videocall::protocol::{
    ConnectionMetadata, DataTransferInfo, EventType, ServerConnectionPacket,
};

// Display implementations for enums
impl std::fmt::Display for MediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        if *self == MediaType::VIDEO {
            write!(f, "video")
        } else if *self == MediaType::AUDIO {
            write!(f, "audio")
        } else if *self == MediaType::SCREEN {
            write!(f, "screen")
        } else if *self == MediaType::HEARTBEAT {
            write!(f, "heartbeat")
        } else if *self == MediaType::RTT {
            write!(f, "rtt")
        } else {
            write!(f, "unknown")
        }
    }
}

impl std::fmt::Display for PacketType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        if *self == PacketType::AES_KEY {
            write!(f, "AES_KEY")
        } else if *self == PacketType::RSA_PUB_KEY {
            write!(f, "RSA_PUB_KEY")
        } else if *self == PacketType::MEDIA {
            write!(f, "MEDIA")
        } else if *self == PacketType::CONNECTION {
            write!(f, "CONNECTION")
        } else if *self == PacketType::DIAGNOSTICS {
            write!(f, "DIAGNOSTICS")
        } else if *self == PacketType::HEALTH {
            write!(f, "HEALTH")
        } else {
            write!(f, "unknown")
        }
    }
}

impl std::fmt::Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        if *self == EventType::UNKNOWN {
            write!(f, "UNKNOWN")
        } else if *self == EventType::CONNECTION_STARTED {
            write!(f, "CONNECTION_STARTED")
        } else if *self == EventType::CONNECTION_ENDED {
            write!(f, "CONNECTION_ENDED")
        } else if *self == EventType::DATA_TRANSFERRED {
            write!(f, "DATA_TRANSFERRED")
        } else {
            write!(f, "unknown")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flatbuffers::FlatBufferBuilder;

    #[test]
    fn test_packet_wrapper_serialization() {
        let mut builder = FlatBufferBuilder::new();

        let email = builder.create_string("test@example.com");
        let data = builder.create_vector(&[1u8, 2, 3, 4, 5]);

        let args = packet_wrapper_generated::videocall::protocol::PacketWrapperArgs {
            packet_type: PacketType::MEDIA,
            email: Some(email),
            data: Some(data),
        };

        let packet = PacketWrapper::create(&mut builder, &args);

        builder.finish(packet, None);
        let buf = builder.finished_data();

        // Deserialize
        let parsed = flatbuffers::root::<PacketWrapper>(buf).unwrap();
        assert_eq!(parsed.packet_type(), PacketType::MEDIA);
        assert_eq!(parsed.email(), Some("test@example.com"));
        assert_eq!(parsed.data().unwrap().len(), 5);
    }

    #[test]
    fn test_media_packet_serialization() {
        let mut builder = FlatBufferBuilder::new();

        let email = builder.create_string("test@example.com");
        let data = builder.create_vector(&[1u8, 2, 3, 4]);
        let frame_type = builder.create_string("key");

        let args = media_packet_generated::videocall::protocol::MediaPacketArgs {
            media_type: MediaType::VIDEO,
            email: Some(email),
            data: Some(data),
            frame_type: Some(frame_type),
            timestamp: 1234.5,
            duration: 33.3,
            audio_metadata: None,
            video_metadata: None,
            heartbeat_metadata: None,
        };

        let packet = MediaPacket::create(&mut builder, &args);

        builder.finish(packet, None);
        let buf = builder.finished_data();

        // Deserialize
        let parsed = flatbuffers::root::<MediaPacket>(buf).unwrap();
        assert_eq!(parsed.media_type(), MediaType::VIDEO);
        assert_eq!(parsed.email(), Some("test@example.com"));
        assert_eq!(parsed.timestamp(), 1234.5);
    }

    #[test]
    fn test_display_implementations() {
        assert_eq!(format!("{}", MediaType::VIDEO), "video");
        assert_eq!(format!("{}", MediaType::AUDIO), "audio");
        assert_eq!(format!("{}", PacketType::MEDIA), "MEDIA");
        assert_eq!(format!("{}", PacketType::AES_KEY), "AES_KEY");
    }
}

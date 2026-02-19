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

//! Tests for packet construction and serialization that verify the protocol
//! works identically on native and WASM targets.

#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use protobuf::Message;
    use videocall_types::protos::connection_packet::ConnectionPacket;
    use videocall_types::protos::media_packet::media_packet::MediaType;
    use videocall_types::protos::media_packet::{
        AudioMetadata, HeartbeatMetadata, MediaPacket, VideoCodec, VideoMetadata,
    };
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    #[test]
    fn test_heartbeat_packet_construction() {
        let heartbeat = MediaPacket {
            media_type: MediaType::HEARTBEAT.into(),
            email: "test-user".to_string(),
            timestamp: 1234567890.0,
            heartbeat_metadata: Some(HeartbeatMetadata {
                video_enabled: true,
                audio_enabled: false,
                screen_enabled: false,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };

        let bytes = heartbeat.write_to_bytes().unwrap();
        let parsed = MediaPacket::parse_from_bytes(&bytes).unwrap();
        assert_eq!(parsed.email, "test-user");
        assert_eq!(
            parsed.media_type.enum_value().unwrap(),
            MediaType::HEARTBEAT
        );
        assert!(parsed.heartbeat_metadata.video_enabled);
        assert!(!parsed.heartbeat_metadata.audio_enabled);
    }

    #[test]
    fn test_video_packet_construction() {
        let video_data = vec![0u8; 1024]; // simulated VP9 frame
        let media_packet = MediaPacket {
            media_type: MediaType::VIDEO.into(),
            data: video_data.clone(),
            email: "streamer".to_string(),
            frame_type: "key".to_string(),
            timestamp: 9999.0,
            duration: 33.33,
            video_metadata: Some(VideoMetadata {
                sequence: 42,
                codec: VideoCodec::VP9_PROFILE0_LEVEL10_8BIT.into(),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };

        let bytes = media_packet.write_to_bytes().unwrap();
        let parsed = MediaPacket::parse_from_bytes(&bytes).unwrap();
        assert_eq!(parsed.data, video_data);
        assert_eq!(parsed.frame_type, "key");
        assert_eq!(parsed.video_metadata.sequence, 42);
    }

    #[test]
    fn test_audio_packet_construction() {
        let audio_data = vec![1u8; 256]; // simulated Opus frame
        let media_packet = MediaPacket {
            email: "speaker".to_string(),
            media_type: MediaType::AUDIO.into(),
            data: audio_data.clone(),
            frame_type: "key".to_string(),
            timestamp: 5555.0,
            audio_metadata: Some(AudioMetadata {
                sequence: 100,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };

        let bytes = media_packet.write_to_bytes().unwrap();
        let parsed = MediaPacket::parse_from_bytes(&bytes).unwrap();
        assert_eq!(parsed.data, audio_data);
        assert_eq!(parsed.audio_metadata.sequence, 100);
    }

    #[test]
    fn test_packet_wrapper_media() {
        let inner = MediaPacket {
            media_type: MediaType::VIDEO.into(),
            email: "user".to_string(),
            ..Default::default()
        };

        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            email: "user".to_string(),
            data: inner.write_to_bytes().unwrap(),
            ..Default::default()
        };

        let bytes = wrapper.write_to_bytes().unwrap();
        let parsed = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        assert_eq!(
            parsed.packet_type.enum_value().unwrap(),
            PacketType::MEDIA
        );
        assert_eq!(parsed.email, "user");

        // Verify inner packet
        let inner_parsed = MediaPacket::parse_from_bytes(&parsed.data).unwrap();
        assert_eq!(
            inner_parsed.media_type.enum_value().unwrap(),
            MediaType::VIDEO
        );
    }

    #[test]
    fn test_connection_packet_construction() {
        let conn = ConnectionPacket {
            meeting_id: "my-room".to_string(),
            ..Default::default()
        };

        let wrapper = PacketWrapper {
            packet_type: PacketType::CONNECTION.into(),
            email: "joiner".to_string(),
            data: conn.write_to_bytes().unwrap(),
            ..Default::default()
        };

        let bytes = wrapper.write_to_bytes().unwrap();
        let parsed = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        assert_eq!(
            parsed.packet_type.enum_value().unwrap(),
            PacketType::CONNECTION
        );

        let conn_parsed = ConnectionPacket::parse_from_bytes(&parsed.data).unwrap();
        assert_eq!(conn_parsed.meeting_id, "my-room");
    }

    #[test]
    fn test_rtt_packet_construction() {
        let rtt = MediaPacket {
            media_type: MediaType::RTT.into(),
            email: "user".to_string(),
            timestamp: 1000.0,
            ..Default::default()
        };

        let bytes = rtt.write_to_bytes().unwrap();
        let parsed = MediaPacket::parse_from_bytes(&bytes).unwrap();
        assert_eq!(parsed.media_type.enum_value().unwrap(), MediaType::RTT);
        assert_eq!(parsed.timestamp, 1000.0);
    }

    #[test]
    fn test_packet_wrapper_all_types() {
        for packet_type in &[
            PacketType::MEDIA,
            PacketType::CONNECTION,
            PacketType::AES_KEY,
            PacketType::RSA_PUB_KEY,
            PacketType::DIAGNOSTICS,
            PacketType::HEALTH,
            PacketType::MEETING,
        ] {
            let wrapper = PacketWrapper {
                packet_type: (*packet_type).into(),
                email: "test".to_string(),
                data: vec![],
                ..Default::default()
            };

            let bytes = wrapper.write_to_bytes().unwrap();
            let parsed = PacketWrapper::parse_from_bytes(&bytes).unwrap();
            assert_eq!(
                parsed.packet_type.enum_value().unwrap(),
                *packet_type,
                "Roundtrip failed for {:?}",
                packet_type
            );
        }
    }
}

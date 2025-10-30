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

//! Helper functions for FlatBuffer serialization/deserialization

use flatbuffers::FlatBufferBuilder;
use videocall_flatbuffers::*;

/// Serialize a PacketWrapper to bytes
pub fn serialize_packet_wrapper(
    packet_type: PacketType,
    email: &str,
    data: &[u8],
) -> Vec<u8> {
    let mut builder = FlatBufferBuilder::new();
    let email_offset = builder.create_string(email);
    let data_offset = builder.create_vector(data);
    
    let args = packet_wrapper_generated::videocall::protocol::PacketWrapperArgs {
        packet_type,
        email: Some(email_offset),
        data: Some(data_offset),
    };
    
    let packet = PacketWrapper::create(&mut builder, &args);
    builder.finish(packet, None);
    builder.finished_data().to_vec()
}

/// Deserialize bytes to PacketWrapper
pub fn deserialize_packet_wrapper(bytes: &[u8]) -> Result<PacketWrapper, String> {
    flatbuffers::root::<PacketWrapper>(bytes).map_err(|e| format!("Failed to parse PacketWrapper: {}", e))
}

/// Serialize a MediaPacket to bytes
pub fn serialize_media_packet(packet: &MediaPacketBuilder) -> Vec<u8> {
    let mut builder = FlatBufferBuilder::new();
    let email_offset = packet.email.as_ref().map(|e| builder.create_string(e));
    let data_offset = packet.data.as_ref().map(|d| builder.create_vector(d));
    let frame_type_offset = packet.frame_type.as_ref().map(|f| builder.create_string(f));
    
    let audio_metadata = packet.audio_metadata.as_ref().map(|am| {
        let format_offset = am.audio_format.as_ref().map(|f| builder.create_string(f));
        let args = media_packet_generated::videocall::protocol::AudioMetadataArgs {
            audio_format: format_offset,
            audio_number_of_channels: am.audio_number_of_channels,
            audio_number_of_frames: am.audio_number_of_frames,
            audio_sample_rate: am.audio_sample_rate,
            sequence: am.sequence,
        };
        AudioMetadata::create(&mut builder, &args)
    });
    
    let video_metadata = packet.video_metadata.as_ref().map(|vm| {
        let args = media_packet_generated::videocall::protocol::VideoMetadataArgs {
            sequence: vm.sequence,
        };
        VideoMetadata::create(&mut builder, &args)
    });
    
    let heartbeat_metadata = packet.heartbeat_metadata.as_ref().map(|hm| {
        let args = media_packet_generated::videocall::protocol::HeartbeatMetadataArgs {
            video_enabled: hm.video_enabled,
            audio_enabled: hm.audio_enabled,
            screen_enabled: hm.screen_enabled,
        };
        HeartbeatMetadata::create(&mut builder, &args)
    });
    
    let args = media_packet_generated::videocall::protocol::MediaPacketArgs {
        media_type: packet.media_type,
        email: email_offset,
        data: data_offset,
        frame_type: frame_type_offset,
        timestamp: packet.timestamp,
        duration: packet.duration,
        audio_metadata,
        video_metadata,
        heartbeat_metadata,
    };
    
    let media_packet = MediaPacket::create(&mut builder, &args);
    builder.finish(media_packet, None);
    builder.finished_data().to_vec()
}

/// Helper struct to build MediaPackets
#[derive(Default, Clone)]
pub struct MediaPacketBuilder {
    pub media_type: MediaType,
    pub email: Option<String>,
    pub data: Option<Vec<u8>>,
    pub frame_type: Option<String>,
    pub timestamp: f64,
    pub duration: f64,
    pub audio_metadata: Option<AudioMetadataBuilder>,
    pub video_metadata: Option<VideoMetadataBuilder>,
    pub heartbeat_metadata: Option<HeartbeatMetadataBuilder>,
}

#[derive(Clone)]
pub struct AudioMetadataBuilder {
    pub audio_format: Option<String>,
    pub audio_number_of_channels: u32,
    pub audio_number_of_frames: u32,
    pub audio_sample_rate: f32,
    pub sequence: u64,
}

#[derive(Clone)]
pub struct VideoMetadataBuilder {
    pub sequence: u64,
}

#[derive(Clone)]
pub struct HeartbeatMetadataBuilder {
    pub video_enabled: bool,
    pub audio_enabled: bool,
    pub screen_enabled: bool,
}

impl MediaPacketBuilder {
    pub fn new(media_type: MediaType) -> Self {
        Self {
            media_type,
            ..Default::default()
        }
    }
    
    pub fn email(mut self, email: String) -> Self {
        self.email = Some(email);
        self
    }
    
    pub fn data(mut self, data: Vec<u8>) -> Self {
        self.data = Some(data);
        self
    }
    
    pub fn frame_type(mut self, frame_type: String) -> Self {
        self.frame_type = Some(frame_type);
        self
    }
    
    pub fn timestamp(mut self, timestamp: f64) -> Self {
        self.timestamp = timestamp;
        self
    }
    
    pub fn duration(mut self, duration: f64) -> Self {
        self.duration = duration;
        self
    }
    
    pub fn audio_metadata(mut self, metadata: AudioMetadataBuilder) -> Self {
        self.audio_metadata = Some(metadata);
        self
    }
    
    pub fn video_metadata(mut self, metadata: VideoMetadataBuilder) -> Self {
        self.video_metadata = Some(metadata);
        self
    }
    
    pub fn heartbeat_metadata(mut self, metadata: HeartbeatMetadataBuilder) -> Self {
        self.heartbeat_metadata = Some(metadata);
        self
    }
    
    pub fn build(self) -> Vec<u8> {
        serialize_media_packet(&self)
    }
}

/// Deserialize bytes to MediaPacket
pub fn deserialize_media_packet(bytes: &[u8]) -> Result<MediaPacket, String> {
    flatbuffers::root::<MediaPacket>(bytes).map_err(|e| format!("Failed to parse MediaPacket: {}", e))
}

/// Helper to create and serialize a heartbeat MediaPacket
pub fn serialize_heartbeat_packet(
    email: &str,
    video_enabled: bool,
    audio_enabled: bool,
    screen_enabled: bool,
) -> Vec<u8> {
    let builder = MediaPacketBuilder::new(MediaType::HEARTBEAT)
        .email(email.to_string())
        .timestamp(js_sys::Date::now())
        .heartbeat_metadata(HeartbeatMetadataBuilder {
            video_enabled,
            audio_enabled,
            screen_enabled,
        });
    builder.build()
}

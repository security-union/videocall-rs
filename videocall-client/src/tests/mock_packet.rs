use protobuf::Message;
use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{AudioMetadata, VideoMetadata};
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;

/// Creates mock media packets for testing purposes
pub struct MockPacketGenerator {
    sequence: u64,
    timestamp_base: f64,
}

impl Default for MockPacketGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl MockPacketGenerator {
    pub fn new() -> Self {
        Self {
            sequence: 0,
            timestamp_base: 1000.0, // Start at 1 second
        }
    }

    /// Create a mock video packet
    pub fn create_video_packet(&mut self, peer_id: &str, key_frame: bool) -> Arc<PacketWrapper> {
        self.sequence += 1;
        let timestamp = self.timestamp_base + (self.sequence as f64 * 33.33); // ~30fps
        
        // Create video metadata
        let mut video_metadata = VideoMetadata::new();
        video_metadata.sequence = self.sequence;
        
        let mut media_packet = MediaPacket::new();
        media_packet.set_media_type(MediaType::VIDEO);
        media_packet.set_email(peer_id.to_string());
        media_packet.set_timestamp(timestamp);
        media_packet.set_duration(33.33);
        media_packet.set_frame_type(if key_frame { "key".to_string() } else { "delta".to_string() });
        media_packet.set_data(vec![0; 1024]); // Add dummy data to simulate a video frame
        media_packet.set_video_metadata(video_metadata);
        
        let data = media_packet.write_to_bytes().unwrap();
        
        let mut packet_wrapper = PacketWrapper::new();
        packet_wrapper.set_packet_type(PacketType::MEDIA);
        packet_wrapper.set_email(peer_id.to_string());
        packet_wrapper.set_data(data);
        
        Arc::new(packet_wrapper)
    }
    
    /// Create a mock audio packet
    pub fn create_audio_packet(&mut self, peer_id: &str) -> Arc<PacketWrapper> {
        self.sequence += 1;
        let timestamp = self.timestamp_base + (self.sequence as f64 * 20.0); // 50 packets per second
        
        // Create audio metadata
        let mut audio_metadata = AudioMetadata::new();
        audio_metadata.set_audio_sample_rate(48000.0);
        audio_metadata.set_audio_number_of_channels(2);
        audio_metadata.set_audio_format("opus".to_string());
        audio_metadata.set_audio_number_of_frames(960);
        
        let mut media_packet = MediaPacket::new();
        media_packet.set_media_type(MediaType::AUDIO);
        media_packet.set_email(peer_id.to_string());
        media_packet.set_timestamp(timestamp);
        media_packet.set_duration(20.0);
        media_packet.set_frame_type("key".to_string()); // Audio frames are typically all key frames
        media_packet.set_data(vec![0; 320]); // Add dummy data to simulate audio
        media_packet.set_audio_metadata(audio_metadata);
        
        let data = media_packet.write_to_bytes().unwrap();
        
        let mut packet_wrapper = PacketWrapper::new();
        packet_wrapper.set_packet_type(PacketType::MEDIA);
        packet_wrapper.set_email(peer_id.to_string());
        packet_wrapper.set_data(data);
        
        Arc::new(packet_wrapper)
    }
    
    /// Create a mock heartbeat packet
    pub fn create_heartbeat_packet(&mut self, peer_id: &str) -> Arc<PacketWrapper> {
        self.sequence += 1;
        let timestamp = self.timestamp_base + (self.sequence as f64 * 1000.0); // 1 per second
        
        let mut media_packet = MediaPacket::new();
        media_packet.set_media_type(MediaType::HEARTBEAT);
        media_packet.set_email(peer_id.to_string());
        media_packet.set_timestamp(timestamp);
        
        let data = media_packet.write_to_bytes().unwrap();
        
        let mut packet_wrapper = PacketWrapper::new();
        packet_wrapper.set_packet_type(PacketType::MEDIA);
        packet_wrapper.set_email(peer_id.to_string());
        packet_wrapper.set_data(data);
        
        Arc::new(PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            email: peer_id.to_string(),
            data,
            ..Default::default()
        })
    }
} 
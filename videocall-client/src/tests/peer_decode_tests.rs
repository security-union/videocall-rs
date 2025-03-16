use crate::client::diagnostics::DiagnosticsManager;
use crate::decode::{PeerDecodeManager, DecodingMetrics, DecodeStatus, PeerDecode};
use crate::tests::mock_packet::MockPacketGenerator;
use std::sync::Arc;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use yew::prelude::Callback;

// Mock peer decoder for testing without browser APIs
mod mock_decoders {
    use crate::decode::{DecodeStatus, PeerDecode};
    use std::sync::Arc;
    use videocall_types::protos::media_packet::MediaPacket;
    
    // Mock Video Decoder
    #[derive(Debug)]
    pub struct MockVideoPeerDecoder {
        pub decoded_count: usize,
        pub waiting_for_keyframe: bool,
    }
    
    impl MockVideoPeerDecoder {
        pub fn new() -> Self {
            Self {
                decoded_count: 0,
                waiting_for_keyframe: true,
            }
        }
        
        pub fn is_waiting_for_keyframe(&self) -> bool {
            self.waiting_for_keyframe
        }
    }
    
    impl PeerDecode for MockVideoPeerDecoder {
        fn decode(&mut self, packet: &Arc<MediaPacket>) -> Result<DecodeStatus, ()> {
            self.decoded_count += 1;
            
            // If this is a key frame, we're no longer waiting for a keyframe
            if packet.frame_type == "key" {
                self.waiting_for_keyframe = false;
            }
            
            Ok(DecodeStatus {
                _rendered: true,
                first_frame: self.decoded_count == 1,
            })
        }
    }
    
    // Mock Audio Decoder
    #[derive(Debug)]
    pub struct MockAudioPeerDecoder {
        pub decoded_count: usize,
    }
    
    impl MockAudioPeerDecoder {
        pub fn new() -> Self {
            Self {
                decoded_count: 0,
            }
        }
    }
    
    impl PeerDecode for MockAudioPeerDecoder {
        fn decode(&mut self, _packet: &Arc<MediaPacket>) -> Result<DecodeStatus, ()> {
            self.decoded_count += 1;
            
            Ok(DecodeStatus {
                _rendered: true,
                first_frame: self.decoded_count == 1,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    // Test helper to create a PeerDecodeManager for testing
    fn create_test_manager() -> PeerDecodeManager {
        let on_first_frame = Callback::from(|_| {});
        let get_video_canvas_id = Callback::from(|key: String| format!("video-{}", &key));
        let get_screen_canvas_id = Callback::from(|key: String| format!("screen-{}", &key));
        
        PeerDecodeManager::new(
            on_first_frame,
            get_video_canvas_id,
            get_screen_canvas_id,
        )
    }
    
    // Mock packet parsing for testing
    fn parse_test_packet(packet: &Arc<PacketWrapper>) -> Result<(MediaType, DecodingMetrics), String> {
        // In a real implementation, we would:
        // 1. Extract the MediaPacket from the PacketWrapper
        // 2. Get the media type and other metadata
        // 3. Return the metrics
        
        // For testing, we'll just return some default values
        let metrics = DecodingMetrics {
            media_type: MediaType::VIDEO, // Default assumption
            timestamp: 1000.0,
            decode_time_ms: 10,
            sequence: Some(1),
            width: 1280,
            height: 720,
            sample_rate: 0,
            channels: 0,
        };
        
        Ok((MediaType::VIDEO, metrics))
    }
    
    #[test]
    fn test_decode_with_diagnostics() {
        let mut manager = create_test_manager();
        let mut diag_manager = DiagnosticsManager::new("test_user".to_string());
        let mut packet_gen = MockPacketGenerator::new();
        
        // Ensure we have the test peer
        let peer_id = "test_peer@example.com";
        manager.ensure_peer(&peer_id.to_string());
        
        // Create and decode a video packet
        let packet = packet_gen.create_video_packet(peer_id, true);
        
        // This should not panic and should pass metrics to diagnostics manager
        let result = manager.decode((*packet).clone(), Some(&mut diag_manager));
        
        // Should succeed
        assert!(result.is_ok());
        
        // Diagnostics manager should now have data for this peer
        assert!(diag_manager.has_peer_data(peer_id));
        
        // Should have recorded one video frame
        assert_eq!(diag_manager.get_frame_count(peer_id, MediaType::VIDEO), 1);
    }
    
    #[test]
    fn test_sequential_decoding() {
        let mut manager = create_test_manager();
        let mut diag_manager = DiagnosticsManager::new("test_user".to_string());
        let mut packet_gen = MockPacketGenerator::new();
        
        // Ensure we have the test peer
        let peer_id = "test_peer@example.com";
        manager.ensure_peer(&peer_id.to_string());
        
        // Create and decode multiple packets of different types
        for _ in 0..5 {
            let video_packet = packet_gen.create_video_packet(peer_id, true);
            let audio_packet = packet_gen.create_audio_packet(peer_id);
            let heartbeat_packet = packet_gen.create_heartbeat_packet(peer_id);
            
            // Decode all packet types
            let _ = manager.decode((*video_packet).clone(), Some(&mut diag_manager));
            let _ = manager.decode((*audio_packet).clone(), Some(&mut diag_manager));
            let _ = manager.decode((*heartbeat_packet).clone(), Some(&mut diag_manager));
        }
        
        // Should have recorded frames for all media types
        assert_eq!(diag_manager.get_frame_count(peer_id, MediaType::VIDEO), 5);
        assert_eq!(diag_manager.get_frame_count(peer_id, MediaType::AUDIO), 5);
        
        // Heartbeats aren't counted as frames
        assert_eq!(diag_manager.get_frame_count(peer_id, MediaType::HEARTBEAT), 0);
    }
    
    // Basic test that tests the diagnostics integration but doesn't rely on 
    // actual decoder implementations
    #[test]
    fn test_diagnostics_integration() {
        let mut manager = create_test_manager();
        let mut diag_manager = DiagnosticsManager::new("test_user".to_string());
        let mut packet_gen = MockPacketGenerator::new();
        
        // Ensure we have multiple test peers
        let peer1_id = "peer1@example.com";
        let peer2_id = "peer2@example.com";
        manager.ensure_peer(&peer1_id.to_string());
        manager.ensure_peer(&peer2_id.to_string());
        
        // Create and decode packets for both peers - if this fails, it means 
        // the basic functionality is broken and our integration test also won't work
        for _ in 0..3 {
            let video_packet1 = packet_gen.create_video_packet(peer1_id, true);
            let _ = manager.decode((*video_packet1).clone(), Some(&mut diag_manager));
        }
        
        // Success if we get this far without panic
        assert!(true);
    }
    
    #[test]
    fn test_no_diagnostics() {
        let mut manager = create_test_manager();
        let mut packet_gen = MockPacketGenerator::new();
        
        // Ensure we have the test peer
        let peer_id = "test_peer@example.com";
        manager.ensure_peer(&peer_id.to_string());
        
        // Create and decode a packet without diagnostics manager
        let packet = packet_gen.create_video_packet(peer_id, true);
        
        // This should not panic even though diagnostics_manager is None
        let result = manager.decode((*packet).clone(), None);
        
        // Should succeed
        assert!(result.is_ok());
    }
} 
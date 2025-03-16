use crate::client::diagnostics::{DiagnosticsManager, now_ms};
use videocall_types::protos::media_packet::media_packet::MediaType;

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_creation() {
        let manager = DiagnosticsManager::new("test_user".to_string());
        assert_eq!(manager.get_local_user_id(), "test_user");
    }
    
    #[test]
    fn test_frame_tracking() {
        let mut manager = DiagnosticsManager::new("test_user".to_string());
        
        // Add frames for a peer
        manager.on_frame_received("peer1", MediaType::VIDEO, 1000.0, 10, Some(1));
        manager.on_frame_received("peer1", MediaType::VIDEO, 1033.0, 15, Some(2));
        manager.on_frame_received("peer1", MediaType::VIDEO, 1066.0, 12, Some(3));
        
        // Add frames for another peer
        manager.on_frame_received("peer2", MediaType::VIDEO, 1000.0, 20, Some(1));
        manager.on_frame_received("peer2", MediaType::AUDIO, 1020.0, 5, Some(2));
        
        // Check if frames are being tracked
        assert!(manager.has_peer_data("peer1"));
        assert!(manager.has_peer_data("peer2"));
        assert!(!manager.has_peer_data("non_existent_peer"));
        
        // Verify frame counts 
        assert_eq!(manager.get_frame_count("peer1", MediaType::VIDEO), 3);
        assert_eq!(manager.get_frame_count("peer2", MediaType::VIDEO), 1);
        assert_eq!(manager.get_frame_count("peer2", MediaType::AUDIO), 1);
        assert_eq!(manager.get_frame_count("peer1", MediaType::AUDIO), 0);
    }
    
    #[test]
    fn test_packet_generation() {
        let mut manager = DiagnosticsManager::new("test_user".to_string());
        
        // Add some frame data
        manager.on_frame_received("peer1", MediaType::VIDEO, 1000.0, 10, Some(1));
        manager.on_frame_received("peer1", MediaType::VIDEO, 1033.0, 15, Some(2));
        
        // Should generate a diagnostics packet
        let packets = manager.create_diagnostics_packets();
        assert!(!packets.is_empty());
        
        // The packet should be for peer1
        let (target_peer, _) = &packets[0];
        assert_eq!(target_peer, "peer1");
    }
    
    #[test]
    fn test_should_send_diagnostics() {
        let mut manager = DiagnosticsManager::new("test_user".to_string());
        
        // Initially shouldn't send
        assert!(!manager.should_send_diagnostics());
        
        // Add some frame data
        manager.on_frame_received("peer1", MediaType::VIDEO, 1000.0, 10, Some(1));
        manager.on_frame_received("peer1", MediaType::VIDEO, 1033.0, 15, Some(2));
        
        // Force the last sent time to be in the past
        manager.override_last_sent_time(now_ms() - 5000.0); // 5 seconds ago
        
        // Now it should want to send
        assert!(manager.should_send_diagnostics());
        
        // Mark as sent
        manager.mark_diagnostics_sent();
        
        // Shouldn't want to send again immediately
        assert!(!manager.should_send_diagnostics());
    }
    
    #[test]
    fn test_resolution_tracking() {
        let mut manager = DiagnosticsManager::new("test_user".to_string());
        
        // Add resolution data
        manager.update_video_resolution("peer1", MediaType::VIDEO, 1280, 720);
        
        // Check if resolution is tracked
        let (width, height) = manager.get_resolution("peer1", MediaType::VIDEO);
        assert_eq!(width, 1280);
        assert_eq!(height, 720);
        
        // Update resolution
        manager.update_video_resolution("peer1", MediaType::VIDEO, 640, 480);
        
        // Check if resolution is updated
        let (width, height) = manager.get_resolution("peer1", MediaType::VIDEO);
        assert_eq!(width, 640);
        assert_eq!(height, 480);
    }
    
    #[test]
    fn test_audio_params_tracking() {
        let mut manager = DiagnosticsManager::new("test_user".to_string());
        
        // Add audio parameters
        manager.update_audio_params("peer1", 48000, 2);
        
        // Check if audio parameters are tracked
        let (sample_rate, channels) = manager.get_audio_params("peer1");
        assert_eq!(sample_rate, 48000);
        assert_eq!(channels, 2);
        
        // Update audio parameters
        manager.update_audio_params("peer1", 44100, 1);
        
        // Check if audio parameters are updated
        let (sample_rate, channels) = manager.get_audio_params("peer1");
        assert_eq!(sample_rate, 44100);
        assert_eq!(channels, 1);
    }
}

// Extension methods to help with testing
impl DiagnosticsManager {
    // Only used in tests to check peer existence
    fn has_peer_data(&self, peer_id: &str) -> bool {
        self.stream_metrics.iter().any(|(_, metrics)| metrics.peer_id == peer_id)
    }
    
    // Only used in tests to check frame counts
    fn get_frame_count(&self, peer_id: &str, media_type: MediaType) -> usize {
        // Count frames by checking timestamps for this peer/media type
        let key = Self::get_stream_key(peer_id, media_type);
        if let Some(metrics) = self.stream_metrics.get(&key) {
            metrics.frame_timestamps.len()
        } else {
            0
        }
    }
    
    // Only used in tests to override the last sent time
    fn override_last_sent_time(&mut self, time: f64) {
        self.last_diagnostics_sent = time;
    }
    
    // Only used in tests to get the local user ID
    fn get_local_user_id(&self) -> &str {
        &self.own_user_id
    }
    
    // Only used in tests to get resolution
    fn get_resolution(&self, peer_id: &str, media_type: MediaType) -> (u32, u32) {
        let key = Self::get_stream_key(peer_id, media_type);
        if let Some(metrics) = self.stream_metrics.get(&key) {
            (metrics.resolution_width, metrics.resolution_height)
        } else {
            (0, 0)
        }
    }
    
    // Only used in tests to get audio parameters
    fn get_audio_params(&self, peer_id: &str) -> (u32, u32) {
        let key = Self::get_stream_key(peer_id, MediaType::AUDIO);
        if let Some(metrics) = self.stream_metrics.get(&key) {
            (metrics.sample_rate.unwrap_or(0), metrics.channels.unwrap_or(0))
        } else {
            (0, 0)
        }
    }
} 
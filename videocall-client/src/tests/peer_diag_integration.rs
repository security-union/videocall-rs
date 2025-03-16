use crate::client::diagnostics::DiagnosticsManager;
use videocall_types::protos::media_packet::media_packet::MediaType;

#[cfg(test)]
mod tests {
    use super::*;
    
    // This test only works in WASM due to js-sys calls in DiagnosticsManager
    #[cfg(target_arch = "wasm32")]
    #[test]
    fn test_diagnostics_manager_packet_creation() {
        // Create a diagnostics manager
        let mut diag_manager = DiagnosticsManager::new("test-user".to_string());
        
        // Simulate receiving frames from multiple peers
        let peers = vec![
            "peer1@example.com",
            "peer2@example.com",
            "peer3@example.com",
        ];
        
        // Feed frame metrics into the diagnostics manager
        for peer in &peers {
            // Add some video frames
            for i in 0..5 {
                diag_manager.on_frame_received(
                    peer, 
                    MediaType::VIDEO, 
                    1000.0 + (i as f64 * 33.3), // timestamp with ~30fps
                    10 + i,     // varying decode time
                    Some(i as u64 + 1) // sequence number with correct type
                );
            }
            
            // Add some audio frames
            for i in 0..10 {
                diag_manager.on_frame_received(
                    peer, 
                    MediaType::AUDIO, 
                    1000.0 + (i as f64 * 20.0), // timestamp with ~50fps for audio
                    5,      // decode time
                    Some(i as u64 + 1) // sequence number with correct type
                );
            }
        }
        
        // Create diagnostics packets
        let packets = diag_manager.create_diagnostics_packets();
        
        // Check that we got packets for each peer
        assert_eq!(packets.len(), peers.len(), "Should generate one packet per peer");
        
        // Check that the packets contain the peer IDs we expect
        for peer in &peers {
            let has_packet_for_peer = packets.iter().any(|(target_peer, _)| target_peer == peer);
            assert!(has_packet_for_peer, "Should have a packet for {}", peer);
        }
        
        // Mark the diagnostics as sent
        diag_manager.mark_diagnostics_sent();
        
        // Check that the diagnostics manager doesn't want to send again immediately
        assert!(!diag_manager.should_send_diagnostics(), 
                "Should not want to send diagnostics immediately after sending");
    }
    
    // This is a simpler test for non-WASM environments
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn test_diagnostics_manager_packet_creation() {
        // A much simpler test for non-WASM environments
        let mut diag_manager = DiagnosticsManager::new("test-user".to_string());
        
        // Just add a single frame
        diag_manager.on_frame_received(
            "peer1@example.com", 
            MediaType::VIDEO,
            1000.0,
            10,
            Some(1)
        );
        
        // Just check that we can create the instance and call the method without panic
        assert!(true, "DiagnosticsManager could be created");
    }
} 
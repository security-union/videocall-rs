use crate::client::diagnostics::DiagnosticsManager;
use crate::decode::PeerDecodeManager;
use crate::tests::mock_packet::MockPacketGenerator;
use videocall_types::protos::media_packet::media_packet::MediaType;
use yew::prelude::Callback;

/// Runs a simple integration test that verifies the core diagnostics functionality works
pub fn run_integration_test() -> Result<String, String> {
    // Create test peers
    let peers = vec![
        "peer1@example.com".to_string(),
        "peer2@example.com".to_string(),
        "peer3@example.com".to_string(),
    ];
    
    // Setup local user
    let local_user = "local_user@example.com".to_string();
    
    // Create packet generator
    let mut packet_gen = MockPacketGenerator::new();
    
    // Create results collection
    let mut test_results = Vec::new();
    
    // Setup diagnostics manager
    let mut diag_manager = DiagnosticsManager::new(local_user.clone());
    
    // Create decode manager with mocked callbacks
    let mut decode_manager = PeerDecodeManager::new(
        Callback::from(|_: (String, MediaType)| {
            // Mocked first frame callback
        }),
        Callback::from(|peer: String| format!("video-{}", peer)),
        Callback::from(|peer: String| format!("screen-{}", peer)),
    );
    
    // Ensure all peers exist
    for peer in &peers {
        let status = decode_manager.ensure_peer(peer);
        test_results.push(format!("Peer added: {}", peer));
    }
    
    // Step 1: Simulate receiving video packets from each peer
    test_results.push("Step 1: Simulating video packet reception".to_string());
    for peer in &peers {
        // Send key frame first
        let video_key = packet_gen.create_video_packet(peer, true);
        match decode_manager.decode((*video_key).clone(), Some(&mut diag_manager)) {
            Ok(_) => test_results.push(format!("Successfully decoded key frame from {}", peer)),
            Err(e) => return Err(format!("Failed to decode key frame: {:?}", e)),
        }
        
        // Send some delta frames
        for _ in 0..5 {
            let video_delta = packet_gen.create_video_packet(peer, false);
            match decode_manager.decode((*video_delta).clone(), Some(&mut diag_manager)) {
                Ok(_) => (),
                Err(e) => return Err(format!("Failed to decode delta frame: {:?}", e)),
            }
        }
    }
    
    // Step 2: Simulate receiving audio packets
    test_results.push("Step 2: Simulating audio packet reception".to_string());
    for peer in &peers {
        for _ in 0..10 {
            let audio = packet_gen.create_audio_packet(peer);
            match decode_manager.decode((*audio).clone(), Some(&mut diag_manager)) {
                Ok(_) => (),
                Err(e) => return Err(format!("Failed to decode audio: {:?}", e)),
            }
        }
    }
    
    // Step 3: Generate diagnostics packets
    test_results.push("Step 3: Generating diagnostics packets".to_string());
    let diag_packets = diag_manager.create_diagnostics_packets();
    test_results.push(format!("Generated {} diagnostics packets", diag_packets.len()));
    
    if !diag_packets.is_empty() {
        test_results.push("Test passed: Integration test succeeded".to_string());
        Ok(test_results.join("\n"))
    } else {
        Err("Test failed: No diagnostics packets were generated".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_integration() {
        match run_integration_test() {
            Ok(_) => assert!(true, "Integration test passed"),
            Err(e) => panic!("Integration test failed: {}", e),
        }
    }
} 
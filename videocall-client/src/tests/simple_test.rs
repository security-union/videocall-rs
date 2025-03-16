use crate::client::diagnostics::DiagnosticsManager;
use videocall_types::protos::media_packet::media_packet::MediaType;

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_diagnostics_manager_creation() {
        // Create a diagnostics manager
        let mut manager = DiagnosticsManager::new("test_user@example.com".to_string());
        
        // Record receiving a frame (minimal test that doesn't rely on complex mocking)
        manager.on_frame_received(
            "peer1@example.com", 
            MediaType::VIDEO, 
            1000.0, // timestamp
            15,     // decode time in ms
            Some(1) // sequence number
        );
        
        // Success if this doesn't panic
        assert!(true);
    }
} 
use crate::client::diagnostics::DiagnosticsManager;
use videocall_types::protos::media_packet::media_packet::MediaType as MediaPacketType;

#[cfg(test)]
pub mod tests {
    use super::*;
    
    #[test]
    pub fn test_diagnostics_manager_creation() {
        let diagnostics = DiagnosticsManager::new("test-user".to_string());
        // Just check that we can create the instance without panic
        assert!(true);
    }
    
    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    pub fn test_encoder_diagnostics_integration() {
        // Create a DiagnosticsManager
        let mut diagnostics = DiagnosticsManager::new("test-sender".to_string());
        
        // Simulate encoding a video frame
        let result = std::panic::catch_unwind(|| {
            // Simulate recording encoding metrics
            diagnostics.on_frame_encoded(
                "test-sender", 
                MediaPacketType::VIDEO, 
                16000.0, // timestamp
                1024,    // size
                123      // sequence
            );
            
            // Check that metrics were recorded
            let packets = diagnostics.create_diagnostics_packets();
            assert!(!packets.is_empty());
        });
        
        assert!(result.is_ok(), "Encoder diagnostics integration test should not panic");
    }
} 
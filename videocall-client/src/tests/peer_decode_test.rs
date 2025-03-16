#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    mod wasm_tests {
        use crate::client::diagnostics::DiagnosticsManager;
        use crate::decode::PeerDecodeManager;
        use videocall_types::protos::media_packet::media_packet::MediaType;
        use videocall_types::protos::packet_wrapper::PacketWrapper;
        use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
        use yew::prelude::Callback;
        
        /// This test is designed to run in a WASM environment
        /// It tests the integration between PeerDecodeManager and DiagnosticsManager
        #[wasm_bindgen_test::wasm_bindgen_test]
        pub fn test_peer_decode_manager_with_diagnostics() {
            // Create a DiagnosticsManager
            let mut diag_manager = DiagnosticsManager::new("test-user".to_string());
            
            // Create callbacks for PeerDecodeManager
            let on_first_frame = Callback::from(|_: (String, MediaType)| {
                // No-op callback for testing
            });
            
            let get_video_canvas_id = Callback::from(|email: String| {
                format!("video-{}", email)
            });
            
            let get_screen_canvas_id = Callback::from(|email: String| {
                format!("screen-{}", email)
            });
            
            // Create the PeerDecodeManager
            let mut peer_manager = PeerDecodeManager::new(
                on_first_frame,
                get_video_canvas_id,
                get_screen_canvas_id,
            );
            
            // Add a test peer
            let peer_id = "test-peer@example.com";
            let _ = peer_manager.ensure_peer(&peer_id.to_string());
            
            // Create a test packet
            let mut packet = PacketWrapper::new();
            packet.packet_type = PacketType::MEDIA.into();
            packet.email = peer_id.to_string();
            packet.data = Vec::new();
            
            // Attempt to decode the packet
            // This will likely fail due to empty data, but we're testing the integration
            let result = peer_manager.decode(packet, Some(&mut diag_manager));
            
            // The decode should fail (empty data), but it shouldn't panic
            assert!(result.is_err());
            
            // Success is just not panicking during the test
        }
    }
} 
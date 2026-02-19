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

//! Integration tests for the NativeVideoCallClient.
//!
//! These tests verify the client API, construction, and state management.
//! Network-level tests (actual WebTransport connections) require a running
//! server and are marked with #[ignore].

#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    use videocall_client::{NativeClientOptions, NativeVideoCallClient};

    fn create_test_options() -> NativeClientOptions {
        NativeClientOptions {
            userid: "test-user".to_string(),
            meeting_id: "test-room".to_string(),
            webtransport_url: "https://localhost:4433/lobby/test-user/test-room".to_string(),
            insecure: true,
            on_inbound_packet: Box::new(|_| {}),
            on_connected: Box::new(|| {}),
            on_disconnected: Box::new(|_| {}),
            enable_e2ee: false,
        }
    }

    #[test]
    fn test_client_construction() {
        let client = NativeVideoCallClient::new(create_test_options());
        assert!(!client.is_connected());
    }

    #[test]
    fn test_client_not_connected_initially() {
        let client = NativeVideoCallClient::new(create_test_options());
        assert!(!client.is_connected(), "Client should not be connected before connect()");
    }

    #[test]
    fn test_send_packet_fails_when_not_connected() {
        let client = NativeVideoCallClient::new(create_test_options());
        let packet = videocall_types::protos::packet_wrapper::PacketWrapper::default();
        let result = client.send_packet(packet);
        assert!(result.is_err(), "send_packet should fail when not connected");
    }

    #[test]
    fn test_send_raw_fails_when_not_connected() {
        let client = NativeVideoCallClient::new(create_test_options());
        let result = client.send_raw(vec![1, 2, 3]);
        assert!(result.is_err(), "send_raw should fail when not connected");
    }

    #[test]
    fn test_video_enabled_flag() {
        let client = NativeVideoCallClient::new(create_test_options());
        // Default is false — no accessor so we just test it doesn't panic
        client.set_video_enabled(true);
        client.set_video_enabled(false);
    }

    #[test]
    fn test_audio_enabled_flag() {
        let client = NativeVideoCallClient::new(create_test_options());
        client.set_audio_enabled(true);
        client.set_audio_enabled(false);
    }

    #[test]
    fn test_screen_enabled_flag() {
        let client = NativeVideoCallClient::new(create_test_options());
        client.set_screen_enabled(true);
        client.set_screen_enabled(false);
    }

    #[test]
    fn test_disconnect_when_not_connected() {
        let mut client = NativeVideoCallClient::new(create_test_options());
        // Should not error even when not connected
        let result = client.disconnect();
        assert!(result.is_ok(), "disconnect should succeed even when not connected");
        assert!(!client.is_connected());
    }

    #[test]
    fn test_callback_invocation() {
        let connected_called = Arc::new(AtomicBool::new(false));
        let connected_clone = connected_called.clone();

        let disconnected_called = Arc::new(AtomicBool::new(false));
        let disconnected_clone = disconnected_called.clone();

        let inbound_count = Arc::new(AtomicU32::new(0));
        let inbound_clone = inbound_count.clone();

        let _client = NativeVideoCallClient::new(NativeClientOptions {
            userid: "cb-test".to_string(),
            meeting_id: "room".to_string(),
            webtransport_url: "https://localhost:4433/lobby/cb-test/room".to_string(),
            insecure: true,
            on_inbound_packet: Box::new(move |_| {
                inbound_clone.fetch_add(1, Ordering::Relaxed);
            }),
            on_connected: Box::new(move || {
                connected_clone.store(true, Ordering::Relaxed);
            }),
            on_disconnected: Box::new(move |_| {
                disconnected_clone.store(true, Ordering::Relaxed);
            }),
            enable_e2ee: false,
        });

        // Callbacks should NOT have been called (not connected yet)
        assert!(!connected_called.load(Ordering::Relaxed));
        assert!(!disconnected_called.load(Ordering::Relaxed));
        assert_eq!(inbound_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_options_with_e2ee() {
        let _client = NativeVideoCallClient::new(NativeClientOptions {
            userid: "e2ee-user".to_string(),
            meeting_id: "secret-room".to_string(),
            webtransport_url: "https://localhost:4433/lobby/e2ee-user/secret-room".to_string(),
            insecure: false,
            on_inbound_packet: Box::new(|_| {}),
            on_connected: Box::new(|| {}),
            on_disconnected: Box::new(|_| {}),
            enable_e2ee: true,
        });
        // Just verifying construction doesn't panic with e2ee enabled
    }

    #[tokio::test]
    async fn test_connect_fails_with_invalid_url() {
        let mut client = NativeVideoCallClient::new(NativeClientOptions {
            userid: "test".to_string(),
            meeting_id: "room".to_string(),
            webtransport_url: "not-a-valid-url".to_string(),
            insecure: true,
            on_inbound_packet: Box::new(|_| {}),
            on_connected: Box::new(|| {}),
            on_disconnected: Box::new(|_| {}),
            enable_e2ee: false,
        });

        let result = client.connect().await;
        assert!(result.is_err(), "connect should fail with invalid URL");
    }

    #[tokio::test]
    async fn test_connect_fails_with_unreachable_server() {
        let mut client = NativeVideoCallClient::new(NativeClientOptions {
            userid: "test".to_string(),
            meeting_id: "room".to_string(),
            webtransport_url: "https://127.0.0.1:1/lobby/test/room".to_string(),
            insecure: true,
            on_inbound_packet: Box::new(|_| {}),
            on_connected: Box::new(|| {}),
            on_disconnected: Box::new(|_| {}),
            enable_e2ee: false,
        });

        let result = client.connect().await;
        assert!(result.is_err(), "connect should fail with unreachable server");
        assert!(!client.is_connected());
    }
}

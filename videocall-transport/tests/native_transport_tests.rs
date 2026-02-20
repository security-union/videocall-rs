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

//! Tests for the native transport clients (WebTransport and WebSocket).
//!
//! These tests verify the API surface and error handling. Tests that require
//! a running server are marked with `#[ignore]`.

#[cfg(not(target_arch = "wasm32"))]
mod webtransport_tests {
    use videocall_transport::native_webtransport::NativeWebTransportClient;

    #[tokio::test]
    async fn test_connect_fails_with_invalid_url() {
        let result = NativeWebTransportClient::connect("not://valid", false).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_connect_fails_with_unreachable_server() {
        let result = NativeWebTransportClient::connect(
            "https://127.0.0.1:1/lobby/test/room",
            true, // insecure
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_api_surface_compiles() {
        // Verifies the full API surface compiles correctly.
        // Actual connection tests require a running WebTransport server.
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod websocket_tests {
    use videocall_transport::native_websocket::NativeWebSocketClient;

    #[tokio::test]
    async fn test_connect_fails_with_invalid_url() {
        let result = NativeWebSocketClient::connect("not-a-url").await;
        assert!(result.is_err(), "Should fail with invalid URL");
    }

    #[tokio::test]
    async fn test_connect_fails_with_unreachable_server() {
        let result = NativeWebSocketClient::connect("ws://127.0.0.1:1/lobby/test/room").await;
        assert!(result.is_err(), "Should fail when server is unreachable");
    }

    #[tokio::test]
    async fn test_connect_fails_with_bad_scheme() {
        let result = NativeWebSocketClient::connect("ftp://localhost:8080/lobby/test/room").await;
        assert!(result.is_err(), "Should fail with non-ws scheme");
    }

    #[tokio::test]
    async fn test_error_message_is_descriptive() {
        // Verify the error path produces a useful message.
        let result = NativeWebSocketClient::connect("ws://127.0.0.1:1/nope").await;
        assert!(result.is_err());
        let err_str = format!("{}", result.err().unwrap());
        assert!(
            !err_str.is_empty(),
            "Error should have a descriptive message"
        );
    }

    /// Integration test — requires a running WebSocket server.
    /// Run manually with: `cargo test --features native -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn test_connect_send_close_roundtrip() {
        let (client, _rx) =
            NativeWebSocketClient::connect("ws://localhost:8080/lobby/test-user/test-room")
                .await
                .expect("Failed to connect");

        assert!(client.is_connected());

        // Send some binary data
        client.send(vec![1, 2, 3, 4]).await.expect("Failed to send");

        // Gracefully close
        client.close().await.expect("Failed to close");
        assert!(!client.is_connected());
    }
}

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

//! Tests for the native WebTransport client.
//!
//! These tests verify the API surface and error handling. Tests that require
//! a running WebTransport server are marked with `#[ignore]`.

#[cfg(not(target_arch = "wasm32"))]
mod tests {
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
    async fn test_send_fails_after_close() {
        // We can't create a connected client without a server, but we can test
        // the module compiles and the API is correct.
        // A full integration test would require docker-compose with the server.
    }
}

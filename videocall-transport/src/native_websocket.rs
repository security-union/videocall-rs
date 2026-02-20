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

//! Native WebSocket client using `tokio-tungstenite`.
//!
//! Provides a native (non-WASM) WebSocket client for connecting to
//! videocall.rs servers over the WebSocket protocol.  Mirrors the API
//! of [`NativeWebTransportClient`](super::native_webtransport::NativeWebTransportClient)
//! so callers can swap transports with minimal code changes.
//!
//! # Example
//!
//! ```no_run
//! use videocall_transport::native_websocket::NativeWebSocketClient;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let (client, mut inbound_rx) = NativeWebSocketClient::connect(
//!     "wss://server:443/lobby/user/room",
//! ).await?;
//!
//! // Send binary data
//! client.send(b"hello".to_vec()).await?;
//!
//! // Receive inbound messages
//! while let Some(data) = inbound_rx.recv().await {
//!     println!("Received {} bytes", data.len());
//! }
//! # Ok(())
//! # }
//! ```

use anyhow::{anyhow, Result};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;

type WsStream = tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Error type for WebSocket connection attempts.
///
/// Preserves the HTTP status code when the server rejects the WebSocket
/// upgrade, which is essential for testing authentication flows (401, 403,
/// 410, etc.).
#[derive(Debug, thiserror::Error)]
pub enum WebSocketConnectError {
    /// The server rejected the upgrade with an HTTP error status.
    #[error("HTTP {status}: WebSocket upgrade rejected")]
    HttpError {
        /// The HTTP status code returned by the server.
        status: u16,
    },
    /// A transport-level or protocol-level error occurred.
    #[error("WebSocket connection failed: {0}")]
    Other(String),
}

impl WebSocketConnectError {
    /// Returns the HTTP status code if this was an HTTP rejection, else `None`.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::HttpError { status } => Some(*status),
            Self::Other(_) => None,
        }
    }
}

/// A native WebSocket client wrapping `tokio-tungstenite`.
///
/// Handles connection, binary message sending, and receiving inbound
/// binary frames.  Text frames are silently ignored (the videocall
/// protocol uses binary protobuf exclusively).
#[derive(Clone)]
pub struct NativeWebSocketClient {
    writer: Arc<Mutex<SplitSink<WsStream, Message>>>,
    closed: Arc<AtomicBool>,
}

impl std::fmt::Debug for NativeWebSocketClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeWebSocketClient")
            .field("connected", &self.is_connected())
            .finish()
    }
}

impl NativeWebSocketClient {
    /// Connect to a WebSocket server.
    ///
    /// Returns the client and a channel receiver for inbound binary
    /// messages.  For a version that preserves HTTP status codes on
    /// failed upgrades, see [`try_connect`](Self::try_connect).
    ///
    /// # Arguments
    /// * `url` — Full WebSocket URL, e.g. `"wss://host:port/lobby/user/room"`
    ///   or `"ws://host:port/lobby/user/room"` for unencrypted connections.
    pub async fn connect(url: &str) -> Result<(Self, mpsc::Receiver<Vec<u8>>)> {
        Self::try_connect(url).await.map_err(|e| anyhow!("{e}"))
    }

    /// Connect to a WebSocket server, returning a typed error on failure.
    ///
    /// Unlike [`connect`](Self::connect), this preserves the HTTP status code
    /// when the server rejects the WebSocket upgrade (e.g. 401, 403, 410).
    /// This is particularly useful for integration tests that need to verify
    /// authentication and authorization behaviour.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use videocall_transport::native_websocket::{NativeWebSocketClient, WebSocketConnectError};
    ///
    /// # async fn example() {
    /// match NativeWebSocketClient::try_connect("ws://localhost/lobby?token=bad").await {
    ///     Ok((client, rx)) => { /* connected */ }
    ///     Err(WebSocketConnectError::HttpError { status: 401 }) => {
    ///         println!("Unauthorized!");
    ///     }
    ///     Err(e) => { eprintln!("Error: {e}"); }
    /// }
    /// # }
    /// ```
    pub async fn try_connect(
        url: &str,
    ) -> std::result::Result<(Self, mpsc::Receiver<Vec<u8>>), WebSocketConnectError> {
        info!("NativeWebSocket connecting to {url}");

        let (ws_stream, response) =
            tokio_tungstenite::connect_async(url)
                .await
                .map_err(|e| match e {
                    tokio_tungstenite::tungstenite::Error::Http(resp) => {
                        WebSocketConnectError::HttpError {
                            status: resp.status().as_u16(),
                        }
                    }
                    other => WebSocketConnectError::Other(format!(
                        "WebSocket connection to '{url}' failed: {other}"
                    )),
                })?;

        info!("WebSocket connected to {url} (HTTP {})", response.status());

        Ok(Self::setup_streams(ws_stream))
    }

    /// Internal: split the stream and spawn the reader task.
    fn setup_streams(ws_stream: WsStream) -> (Self, mpsc::Receiver<Vec<u8>>) {
        let (writer, mut reader) = ws_stream.split();

        let closed = Arc::new(AtomicBool::new(false));
        let client = Self {
            writer: Arc::new(Mutex::new(writer)),
            closed: closed.clone(),
        };

        let (inbound_tx, inbound_rx) = mpsc::channel(100);
        let closed_reader = closed.clone();

        tokio::spawn(async move {
            while let Some(msg_result) = reader.next().await {
                if closed_reader.load(Ordering::Relaxed) {
                    break;
                }
                match msg_result {
                    Ok(Message::Binary(data)) => {
                        if let Err(e) = inbound_tx.send(data).await {
                            debug!("Inbound channel closed: {e}");
                            break;
                        }
                    }
                    Ok(Message::Close(_)) => {
                        info!("WebSocket received close frame");
                        closed_reader.store(true, Ordering::Relaxed);
                        break;
                    }
                    Ok(Message::Ping(payload)) => {
                        debug!("WebSocket ping received ({} bytes)", payload.len());
                    }
                    Ok(Message::Pong(_)) => {
                        debug!("WebSocket pong received");
                    }
                    Ok(Message::Text(_)) => {
                        debug!("WebSocket text frame ignored (protocol uses binary)");
                    }
                    Ok(Message::Frame(_)) => {
                        debug!("WebSocket raw frame ignored");
                    }
                    Err(e) => {
                        if !closed_reader.load(Ordering::Relaxed) {
                            error!("WebSocket read error: {e}");
                        }
                        break;
                    }
                }
            }
            debug!("WebSocket inbound reader loop ended");
        });

        (client, inbound_rx)
    }

    /// Send binary data over the WebSocket connection.
    pub async fn send(&self, data: Vec<u8>) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(anyhow!("WebSocket is closed"));
        }
        let mut writer = self.writer.lock().await;
        writer
            .send(Message::Binary(data))
            .await
            .map_err(|e| anyhow!("WebSocket send error: {e}"))
    }

    /// Whether the WebSocket connection is still open.
    pub fn is_connected(&self) -> bool {
        !self.closed.load(Ordering::Relaxed)
    }

    /// Close the WebSocket connection gracefully.
    pub async fn close(&self) -> Result<()> {
        if self
            .closed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            let mut writer = self.writer.lock().await;
            if let Err(e) = writer.send(Message::Close(None)).await {
                warn!("Error sending WebSocket close frame: {e}");
            }
        }
        Ok(())
    }

    /// Mark the connection as closed without sending a close frame.
    pub fn force_close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connect_error_http_status() {
        let err = WebSocketConnectError::HttpError { status: 401 };
        assert_eq!(err.http_status(), Some(401));
        assert!(format!("{err}").contains("401"));
    }

    #[test]
    fn test_connect_error_other() {
        let err = WebSocketConnectError::Other("timeout".into());
        assert_eq!(err.http_status(), None);
        assert!(format!("{err}").contains("timeout"));
    }
}

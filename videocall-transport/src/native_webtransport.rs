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

//! Native WebTransport client using `web-transport-quinn`.
//!
//! This module provides a native (non-WASM) WebTransport client that unifies
//! the connection logic previously duplicated in the `bot` and `videocall-cli`
//! crates.
//!
//! # Usage
//!
//! ```no_run
//! use videocall_transport::native_webtransport::NativeWebTransportClient;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let (client, mut inbound_rx) = NativeWebTransportClient::connect(
//!     "https://server:4433/lobby/user/room",
//!     false,  // insecure
//! ).await?;
//!
//! // Send data
//! client.send(b"hello".to_vec()).await?;
//!
//! // Receive inbound data
//! while let Some(data) = inbound_rx.recv().await {
//!     println!("Received {} bytes", data.len());
//! }
//! # Ok(())
//! # }
//! ```

use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use url::Url;

/// A native WebTransport client wrapping a `web_transport_quinn::Session`.
///
/// Handles connection, sending via unidirectional streams, and receiving
/// inbound unidirectional streams.
#[derive(Clone)]
pub struct NativeWebTransportClient {
    session: web_transport_quinn::Session,
    closed: Arc<AtomicBool>,
}

impl NativeWebTransportClient {
    /// Connect to a WebTransport server.
    ///
    /// Returns the client and a channel receiver for inbound data (from
    /// server-initiated unidirectional streams).
    ///
    /// # Arguments
    /// * `url` — Full WebTransport URL (e.g. `https://host:port/lobby/user/room`)
    /// * `insecure` — If `true`, skip TLS certificate verification (testing only!)
    pub async fn connect(
        url: &str,
        insecure: bool,
    ) -> Result<(Self, mpsc::Receiver<Vec<u8>>)> {
        info!("NativeWebTransport connecting to {url}");

        let client = if insecure {
            warn!("TLS certificate verification disabled (insecure mode)");
            unsafe {
                web_transport_quinn::ClientBuilder::new().with_no_certificate_verification()?
            }
        } else {
            web_transport_quinn::ClientBuilder::new().with_system_roots()?
        };

        let parsed_url = Url::parse(url)
            .map_err(|e| anyhow!("Invalid WebTransport URL '{url}': {e}"))?;

        let session = client.connect(parsed_url).await?;
        info!("WebTransport session established to {url}");

        let closed = Arc::new(AtomicBool::new(false));

        let transport = Self {
            session: session.clone(),
            closed: closed.clone(),
        };

        // Start inbound stream consumer
        let (inbound_tx, inbound_rx) = mpsc::channel(100);
        let closed_clone = closed.clone();

        tokio::spawn(async move {
            loop {
                if closed_clone.load(Ordering::Relaxed) {
                    break;
                }
                match session.accept_uni().await {
                    Ok(mut stream) => {
                        let tx = inbound_tx.clone();
                        tokio::spawn(async move {
                            match stream.read_to_end(usize::MAX).await {
                                Ok(data) => {
                                    if let Err(e) = tx.send(data).await {
                                        debug!("Inbound channel closed: {e}");
                                    }
                                }
                                Err(e) => {
                                    debug!("Error reading inbound stream: {e}");
                                }
                            }
                        });
                    }
                    Err(e) => {
                        if !closed_clone.load(Ordering::Relaxed) {
                            error!("Inbound stream accept error: {e}");
                        }
                        break;
                    }
                }
            }
            debug!("Inbound consumer loop ended");
        });

        Ok((transport, inbound_rx))
    }

    /// Send data via a new unidirectional stream.
    pub async fn send(&self, data: Vec<u8>) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(anyhow!("Transport is closed"));
        }
        let mut stream = self.session.open_uni().await?;
        stream.write_all(&data).await?;
        stream.finish()?;
        Ok(())
    }

    /// Whether the transport is still open.
    pub fn is_connected(&self) -> bool {
        !self.closed.load(Ordering::Relaxed)
    }

    /// Close the transport.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    /// Get a reference to the underlying session (for advanced use cases).
    pub fn session(&self) -> &web_transport_quinn::Session {
        &self.session
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_compiles() {
        // Verify the module compiles on native targets.
        // Actual connection tests require a running WebTransport server.
    }
}

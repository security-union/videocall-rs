// Include the generated bindings
uniffi::include_scaffolding!("videocall");

use bytes::Bytes;
use log::{error, info};
use std::sync::Arc;
use std::sync::Mutex;
use thiserror::Error;
use tokio::runtime::Runtime;
use url::Url;
use web_transport_quinn::{ClientBuilder, Session};

// A simple function that returns a greeting
pub fn hello_world() -> String {
    "Hello from Rust!".to_string()
}

// A function that returns the version of the library
pub fn get_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[derive(Error, Debug)]
pub enum WebTransportError {
    #[error("Connection error: {0}")]
    ConnectionError(String),
    #[error("TLS error: {0}")]
    TlsError(String),
    #[error("Stream error")]
    StreamError,
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),
    #[error("Runtime error: {0}")]
    RuntimeError(String),
}

pub struct WebTransportClient {
    runtime: Arc<Runtime>,
    session: Arc<Mutex<Option<Session>>>,
}

impl WebTransportClient {
    pub fn new() -> Self {
        // Create a multi-threaded Tokio runtime with all features enabled
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create Tokio runtime");

        Self {
            runtime: Arc::new(runtime),
            session: Arc::new(Mutex::new(None)),
        }
    }

    pub fn connect(&self, url: String) -> Result<(), WebTransportError> {
        info!("Connecting to WebTransport server at {}", url);

        // Parse the URL
        let url = Url::parse(&url)
            .map_err(|e| WebTransportError::InvalidUrl(format!("Invalid URL: {}", e)))?;

        // Clone Arc for move into async block
        let session_mutex = Arc::clone(&self.session);

        // Create a WebTransport session
        self.runtime.block_on(async move {
            // Create a WebTransport client with system roots
            let client = ClientBuilder::new()
                .with_system_roots()
                .map_err(|_| WebTransportError::TlsError("Failed to create client".to_string()))?;

            // Connect to the server
            let session = client.connect(&url).await.map_err(|e| {
                WebTransportError::ConnectionError(format!("Failed to connect: {}", e))
            })?;

            // Store the session
            let mut session_guard = session_mutex.lock().map_err(|e| {
                WebTransportError::RuntimeError(format!("Failed to acquire lock: {}", e))
            })?;
            *session_guard = Some(session);

            info!("Connected to WebTransport server");
            Ok(())
        })
    }

    pub fn send_datagram(&self, data: Vec<u8>) -> Result<(), WebTransportError> {
        info!("Sending datagram of size {} bytes", data.len());

        // Clone Arc for move into async block
        let session_mutex = Arc::clone(&self.session);

        self.runtime.block_on(async move {
            let session_guard = session_mutex.lock().map_err(|e| {
                WebTransportError::RuntimeError(format!("Failed to acquire lock: {}", e))
            })?;

            let session = session_guard
                .as_ref()
                .ok_or_else(|| WebTransportError::ConnectionError("Not connected".to_string()))?;

            // Send the datagram
            session
                .send_datagram(Bytes::from(data))
                .map_err(|_| WebTransportError::StreamError)?;

            info!("Datagram sent successfully");
            Ok(())
        })
    }
}

impl Drop for WebTransportClient {
    fn drop(&mut self) {
        info!("Shutting down WebTransportClient");
        // The runtime will be dropped automatically when the Arc's reference count reaches zero
    }
}

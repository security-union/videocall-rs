// Include the generated bindings
uniffi::include_scaffolding!("videocall");

use bytes::Bytes;
use log::{debug, error, info, LevelFilter};
use rustls::{ClientConfig, RootCertStore};
use rustls_native_certs::load_native_certs;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use thiserror::Error;
use tokio::runtime::Runtime;
use tokio::task;
use url::Url;
use web_transport_quinn::ClientBuilder;
use web_transport_quinn::Session;

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
    #[error("Certificate error: {0}")]
    CertificateError(String),
    #[error("Failed to create client: {0}")]
    ClientError(String),
    #[error("Queue error: {0}")]
    QueueError(String),
}

pub struct DatagramQueue {
    queue: Arc<Mutex<VecDeque<Vec<u8>>>>,
}

impl Default for DatagramQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl DatagramQueue {
    pub fn new() -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn add_datagram(&self, data: Vec<u8>) -> Result<(), WebTransportError> {
        let mut queue = self
            .queue
            .lock()
            .map_err(|e| WebTransportError::QueueError(format!("Failed to acquire lock: {}", e)))?;

        queue.push_back(data);
        info!("Added datagram to queue, queue size: {}", queue.len());
        Ok(())
    }

    pub fn receive_datagram(&self) -> Result<Vec<u8>, WebTransportError> {
        let mut queue = self
            .queue
            .lock()
            .map_err(|e| WebTransportError::QueueError(format!("Failed to acquire lock: {}", e)))?;

        match queue.pop_front() {
            Some(data) => {
                info!("Received datagram from queue, remaining: {}", queue.len());
                Ok(data)
            }
            None => Err(WebTransportError::QueueError(
                "No datagrams available".to_string(),
            )),
        }
    }

    pub fn has_datagrams(&self) -> Result<bool, WebTransportError> {
        let queue = self
            .queue
            .lock()
            .map_err(|e| WebTransportError::QueueError(format!("Failed to acquire lock: {}", e)))?;

        Ok(!queue.is_empty())
    }
}

pub struct WebTransportClient {
    runtime: Arc<Runtime>,
    session: Arc<Mutex<Option<Session>>>,
    datagram_listener: Arc<Mutex<Option<task::JoinHandle<()>>>>,
}

impl Default for WebTransportClient {
    fn default() -> Self {
        Self::new()
    }
}

impl WebTransportClient {
    pub fn new() -> Self {
        // Initialize logger with debug level
        let _ = env_logger::Builder::new()
            .filter_level(LevelFilter::Debug)
            .try_init();

        if let Err(e) = rustls::crypto::ring::default_provider().install_default() {
            error!("Failed to install default provider: {:?}", e);
        }
        // Create a multi-threaded Tokio runtime with all features enabled
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create Tokio runtime");

        Self {
            runtime: Arc::new(runtime),
            session: Arc::new(Mutex::new(None)),
            datagram_listener: Arc::new(Mutex::new(None)),
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
            // Load native certificates
            let mut root_store = RootCertStore::empty();
            let cert_count = match load_native_certs() {
                Ok(certs) => {
                    let count = certs.len();
                    for cert in certs {
                        root_store.add(cert).map_err(|e| {
                            WebTransportError::CertificateError(format!(
                                "Failed to add certificate: {}",
                                e
                            ))
                        })?;
                    }
                    count
                }
                Err(e) => {
                    error!("Failed to load native certificates: {}", e);
                    return Err(WebTransportError::CertificateError(format!(
                        "Failed to load native certificates: {}",
                        e
                    )));
                }
            };
            info!("Loaded {} native certificates", cert_count);

            // Create a rustls ClientConfig with the root store
            let _client_config = ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth();

            // Create a WebTransport client with the custom rustls config for now disable certificate verification
            let client = unsafe {
                ClientBuilder::new()
                    .with_no_certificate_verification()
                    .map_err(|e| {
                        WebTransportError::TlsError(format!("Failed to create client: {}", e))
                    })?
            };

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

    pub fn subscribe_to_datagrams(
        &self,
        queue: Arc<DatagramQueue>,
    ) -> Result<(), WebTransportError> {
        info!("Subscribing to inbound datagrams");

        // Clone Arc for move into async block
        let session_mutex = Arc::clone(&self.session);
        let datagram_listener_mutex = Arc::clone(&self.datagram_listener);

        // Stop any existing listener
        self.stop_datagram_listener()?;

        // Create a new listener
        let handle = self.runtime.spawn(async move {
            // Get a clone of the session outside the async block
            let session = {
                let session_guard = match session_mutex.lock() {
                    Ok(guard) => guard,
                    Err(e) => {
                        error!("Failed to acquire lock: {}", e);
                        return;
                    }
                };

                match session_guard.as_ref() {
                    Some(session) => session.clone(),
                    None => {
                        error!("Not connected");
                        return;
                    }
                }
            }; // session_guard is dropped here

            info!("Starting to listen for datagrams");

            loop {
                match session.read_datagram().await {
                    Ok(datagram) => {
                        let data = datagram.to_vec();
                        info!("Received datagram of size {} bytes", data.len());
                        debug!("Datagram content: {:?}", data);

                        // Add the datagram to the queue
                        if let Err(e) = queue.add_datagram(data) {
                            error!("Failed to add datagram to queue: {}", e);
                        }
                    }
                    Err(e) => {
                        error!("Error receiving datagram: {}", e);
                        break;
                    }
                }
            }
        });

        // Store the listener handle
        let mut listener_guard = datagram_listener_mutex.lock().map_err(|e| {
            WebTransportError::RuntimeError(format!("Failed to acquire lock: {}", e))
        })?;
        *listener_guard = Some(handle);

        info!("Successfully subscribed to inbound datagrams");
        Ok(())
    }

    pub fn stop_datagram_listener(&self) -> Result<(), WebTransportError> {
        let mut listener_guard = self.datagram_listener.lock().map_err(|e| {
            WebTransportError::RuntimeError(format!("Failed to acquire lock: {}", e))
        })?;

        if let Some(handle) = listener_guard.take() {
            handle.abort();
            info!("Stopped datagram listener");
        }

        Ok(())
    }
}

impl Drop for WebTransportClient {
    fn drop(&mut self) {
        info!("Shutting down WebTransportClient");
        // Stop the datagram listener if it's running
        if let Err(e) = self.stop_datagram_listener() {
            error!("Failed to stop datagram listener: {}", e);
        }
        // The runtime will be dropped automatically when the Arc's reference count reaches zero
    }
}

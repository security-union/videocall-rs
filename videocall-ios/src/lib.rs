// Include the generated bindings
uniffi::include_scaffolding!("videocall");

use crate::videocall::get_version;
use crate::videocall::hello_world;
use crate::videocall::QuicClient;
use crate::videocall::QuicError;

// Module that implements the UDL interface
pub mod videocall {
    use log::info;
    use std::fmt;
    use thiserror::Error;
    use url::Url;

    // A simple function that returns a greeting
    pub fn hello_world() -> String {
        info!("hello_world function called");
        "Hello from Rust!".to_string()
    }

    // Return the version of the library
    pub fn get_version() -> String {
        info!("get_version function called");
        env!("CARGO_PKG_VERSION").to_string()
    }

    // Error types for the QUIC client
    #[derive(Debug, thiserror::Error)]
    pub enum QuicError {
        #[error("Failed to establish connection")]
        ConnectionFailed,
        #[error("Invalid URL format")]
        InvalidUrl,
        #[error("Stream error occurred")]
        StreamError,
        #[error("Unknown error occurred")]
        Unknown,
    }

    // A simple QUIC client
    pub struct QuicClient {
        url: String,
    }

    impl QuicClient {
        // Constructor
        pub fn new(url: String) -> Self {
            info!("Creating new QuicClient with URL: {}", url);
            QuicClient { url }
        }

        // Connect to the QUIC server
        pub fn connect(&self) -> Result<(), QuicError> {
            info!("Connecting to QUIC server at: {}", self.url);
            
            // Validate URL format
            match Url::parse(&self.url) {
                Ok(url) => {
                    info!("URL is valid: {}", url);
                    // For now, just return success
                    // In a real implementation, we would establish a QUIC connection here
                    Ok(())
                },
                Err(_) => {
                    info!("Invalid URL: {}", self.url);
                    Err(QuicError::InvalidUrl)
                }
            }
        }
    }
}

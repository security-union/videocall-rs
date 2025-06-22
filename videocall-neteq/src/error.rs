use thiserror::Error;

/// Result type for NetEQ operations
pub type Result<T> = std::result::Result<T, NetEqError>;

/// Errors that can occur in NetEQ operations
#[derive(Error, Debug, Clone, PartialEq)]
pub enum NetEqError {
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Buffer is full")]
    BufferFull,

    #[error("Buffer is empty")]
    BufferEmpty,

    #[error("Invalid packet: {0}")]
    InvalidPacket(String),

    #[error("Invalid timestamp")]
    InvalidTimestamp,

    #[error("Decoder error: {0}")]
    DecoderError(String),

    #[error("Time stretch error: {0}")]
    TimeStretchError(String),

    #[error("Invalid sample rate: {0}")]
    InvalidSampleRate(u32),

    #[error("Invalid channel count: {0}")]
    InvalidChannelCount(u8),

    #[error("Audio format mismatch")]
    AudioFormatMismatch,
}

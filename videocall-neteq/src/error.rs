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

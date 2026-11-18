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

    #[error("Invalid sample rate: {0}")]
    InvalidSampleRate(u32),

    #[error("Invalid channel count: {0}")]
    InvalidChannelCount(u8),

    /// A WebRTC-parity `NetEqImpl` call returned `kFail`; the payload is the
    /// internal error that caused it.
    #[error("NetEq operation failed: {0:?}")]
    Fail(NetEqImplError),
}

/// C++ `NetEqImpl::Error`. The integer values match C++ so the internal
/// control flow and the parity harness can compare codes directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum NetEqImplError {
    NoError = 0,
    OtherError,
    UnknownRtpPayloadType,
    DecoderNotFound,
    InvalidPointer,
    AccelerateError,
    PreemptiveExpandError,
    ComfortNoiseErrorCode,
    DecoderErrorCode,
    OtherDecoderError,
    InvalidOperation,
    DtmfParsingError,
    DtmfInsertError,
    SampleUnderrun,
    DecodedTooMuch,
    RedundancySplitError,
    PacketBufferCorruption,
}

impl NetEqImplError {
    /// The C++ integer value.
    pub fn code(self) -> i32 {
        self as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impl_error_codes_match_cpp() {
        assert_eq!(0, NetEqImplError::NoError.code());
        assert_eq!(2, NetEqImplError::UnknownRtpPayloadType.code());
        assert_eq!(16, NetEqImplError::PacketBufferCorruption.code());
    }
}

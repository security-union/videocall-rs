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

/// Error bucket for classifying `VideoEncoder.encode()` failures.
///
/// Used by both `CameraEncoder` and `ScreenEncoder` to increment the
/// appropriate observability counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeErrorBucket {
    /// Codec was closed or in an invalid state (e.g. "closed codec", "InvalidStateError").
    ClosedCodec,
    /// VPX memory allocation failure (e.g. "Memory allocation error",
    /// "Unable to find free frame buffer").
    VpxMemAlloc,
    /// Any other unrecognised error message.
    Generic,
}

/// Classify an `encode_with_options` error message into an observability bucket.
///
/// The classification uses substring matching against known browser error
/// strings.  Unknown messages fall through to [`EncodeErrorBucket::Generic`].
pub fn classify_encode_error(msg: &str) -> EncodeErrorBucket {
    if msg.contains("closed codec") || msg.contains("InvalidStateError") {
        EncodeErrorBucket::ClosedCodec
    } else if msg.contains("Memory allocation error")
        || msg.contains("Unable to find free frame buffer")
    {
        EncodeErrorBucket::VpxMemAlloc
    } else {
        EncodeErrorBucket::Generic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_closed_codec_error() {
        assert_eq!(
            classify_encode_error("DOMException: closed codec"),
            EncodeErrorBucket::ClosedCodec
        );
        assert_eq!(
            classify_encode_error("InvalidStateError: The codec is not configured"),
            EncodeErrorBucket::ClosedCodec
        );
    }

    #[test]
    fn classify_vpx_mem_error() {
        assert_eq!(
            classify_encode_error("Memory allocation error during encoding"),
            EncodeErrorBucket::VpxMemAlloc
        );
        assert_eq!(
            classify_encode_error("Unable to find free frame buffer"),
            EncodeErrorBucket::VpxMemAlloc
        );
    }

    #[test]
    fn classify_generic_error() {
        assert_eq!(
            classify_encode_error("some unknown browser error XYZ"),
            EncodeErrorBucket::Generic
        );
        assert_eq!(classify_encode_error(""), EncodeErrorBucket::Generic);
    }
}

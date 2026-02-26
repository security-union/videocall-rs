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

//! The common interface for platform-specific decoders.

use crate::frame::FrameBuffer;
use serde::{Deserialize, Serialize};

/// An enumeration of the supported video codecs.
/// Names include profile/level details for scalability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    /// Unspecified codec - skip decoding.
    Unspecified,
    /// VP8 codec - no profile variants.
    Vp8,
    /// VP9 Profile 0, Level 1.0, 8-bit (vp09.00.10.08).
    Vp9Profile0Level10Bit8,
    /// A mock decoder that does nothing, for testing and simulation.
    Mock,
}

impl VideoCodec {
    /// Returns the WebCodecs codec string for this codec, or None for Unspecified.
    pub fn as_webcodecs_str(&self) -> Option<&'static str> {
        match self {
            VideoCodec::Unspecified => None,
            VideoCodec::Vp8 => Some("vp8"),
            VideoCodec::Vp9Profile0Level10Bit8 => Some("vp09.00.10.08"),
            VideoCodec::Mock => Some("vp8"), // Fallback for testing
        }
    }
}

/// Represents a fully decoded frame, ready for rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodedFrame {
    pub sequence_number: u64,
    pub width: u32,
    pub height: u32,
    // In a real implementation, this would hold image planes (e.g., YUV data).
    pub data: Vec<u8>,
}

/// A trait that abstracts over the platform-specific decoder implementation
/// (e.g., `std::thread` on native, Web Workers + WebCodecs in WASM).
pub trait Decodable: Send + Sync {
    /// The type of frame passed to the callback when a frame is decoded.
    type Frame;

    /// Creates a new decoder and starts its underlying thread or worker.
    /// The `on_decoded_frame` callback will be invoked whenever a frame is successfully decoded.
    fn new(codec: VideoCodec, on_decoded_frame: Box<dyn Fn(Self::Frame) + Send + Sync>) -> Self
    where
        Self: Sized;

    /// Sends a raw frame buffer to the decoder for processing.
    fn decode(&self, frame: FrameBuffer);
}

// Conditionally compile and expose the native implementation
#[cfg(feature = "native")]
mod native;
#[cfg(feature = "native")]
pub use self::native::NativeDecoder as Decoder;

// Conditionally compile and expose the WASM implementation
#[cfg(feature = "wasm")]
mod wasm;
#[cfg(feature = "wasm")]
pub use self::wasm::WasmDecoder; // Export WasmDecoder directly for VideoFrame callbacks

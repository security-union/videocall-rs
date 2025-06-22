/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! The common interface for platform-specific decoders.

use crate::frame::FrameBuffer;
use serde::{Deserialize, Serialize};

/// An enumeration of the supported video codecs.
#[derive(Debug, Clone, Copy)]
pub enum VideoCodec {
    /// VP9 codec, using libvpx.
    VP9,
    /// A mock decoder that does nothing, for testing and simulation.
    Mock,
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
#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use self::native::NativeDecoder as Decoder;

// Conditionally compile and expose the WASM implementation
#[cfg(target_arch = "wasm32")]
mod wasm;
#[cfg(target_arch = "wasm32")]
pub use self::wasm::WasmDecoder as Decoder;

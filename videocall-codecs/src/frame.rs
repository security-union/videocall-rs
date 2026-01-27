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

//! Contains the fundamental data structures for video frames.

use serde::{Deserialize, Serialize};

/// The type of a video frame, indicating its dependency on other frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameType {
    /// A KeyFrame (or I-frame) can be decoded independently of any other frame.
    KeyFrame,
    /// A DeltaFrame (or P-frame) can only be decoded if the preceding frame has been decoded.
    DeltaFrame,
}

/// The codec used to encode a video frame.
/// Names include profile/level details for scalability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FrameCodec {
    /// Unknown/unspecified codec - skip decoding.
    #[default]
    Unspecified,
    /// VP8 codec - no profile variants.
    Vp8,
    /// VP9 Profile 0, Level 1.0, 8-bit (vp09.00.10.08).
    Vp9Profile0Level10Bit8,
}

impl FrameCodec {
    /// Returns the WebCodecs codec string for this codec, or None for Unspecified.
    pub fn as_webcodecs_str(&self) -> Option<&'static str> {
        match self {
            FrameCodec::Unspecified => None,
            FrameCodec::Vp8 => Some("vp8"),
            FrameCodec::Vp9Profile0Level10Bit8 => Some("vp09.00.10.08"),
        }
    }

    /// Returns true if this is a known, decodable codec.
    pub fn is_known(&self) -> bool {
        !matches!(self, FrameCodec::Unspecified)
    }
}

/// Represents a raw, encoded video frame as it arrives from the network.
/// In our simulation, this is the unit that comes from a QUIC stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoFrame {
    /// The sequence number of the frame. Must be contiguous.
    pub sequence_number: u64,
    /// The type of the frame (KeyFrame or DeltaFrame).
    pub frame_type: FrameType,
    /// The codec used to encode this frame.
    #[serde(default)]
    pub codec: FrameCodec,
    /// The encoded video data.
    pub data: Vec<u8>,
    /// The timestamp of the frame.
    pub timestamp: f64,
}

/// A wrapper for a `VideoFrame` that includes metadata used by the jitter buffer.
/// This is the object that is stored and managed within the buffer itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameBuffer {
    /// The underlying video frame data and properties.
    pub frame: VideoFrame,
    /// The system time when this frame was received by the jitter buffer.
    pub arrival_time_ms: u128,
}

impl FrameBuffer {
    /// Creates a new, empty `FrameBuffer` ready to be populated.
    /// In a real system with object pooling, this would be reused.
    pub fn new(frame: VideoFrame, arrival_time_ms: u128) -> Self {
        Self {
            frame,
            arrival_time_ms,
        }
    }

    pub fn sequence_number(&self) -> u64 {
        self.frame.sequence_number
    }

    pub fn is_keyframe(&self) -> bool {
        self.frame.frame_type == FrameType::KeyFrame
    }
}

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

/// Represents a raw, encoded video frame as it arrives from the network.
/// In our simulation, this is the unit that comes from a QUIC stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoFrame {
    /// The sequence number of the frame. Must be contiguous.
    pub sequence_number: u64,
    /// The type of the frame (KeyFrame or DeltaFrame).
    pub frame_type: FrameType,
    /// The encoded video data.
    pub data: Vec<u8>,
    /// The timestamp of the frame.
    pub timestamp: f64,
}

/// A wrapper for a `VideoFrame` that includes metadata used by the jitter buffer.
/// This is the object that is stored and managed within the buffer itself.
#[derive(Debug, Serialize, Deserialize)]
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

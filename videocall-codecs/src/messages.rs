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

//! Shared message types for worker communication

use crate::frame::FrameBuffer;
use serde::{Deserialize, Serialize};

/// Messages that can be sent to the web worker
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerMessage {
    /// Decode a frame
    DecodeFrame(FrameBuffer),
    /// Flush the decoder buffer and reset state
    Flush,
    /// Reset decoder to initial state (waiting for keyframe)
    Reset,
    /// Set diagnostic context so worker can tag events with original IDs
    SetContext { from_peer: String, to_peer: String },
}

/// Video statistics message sent by the worker
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoStatsMessage {
    pub kind: String,
    pub from_peer: Option<String>,
    pub to_peer: Option<String>,
    pub frames_buffered: Option<u64>,
}

impl VideoStatsMessage {
    pub fn new(from_peer: String, to_peer: String, frames_buffered: u64) -> Self {
        Self {
            kind: "video_stats".to_string(),
            from_peer: Some(from_peer),
            to_peer: Some(to_peer),
            frames_buffered: Some(frames_buffered),
        }
    }
}

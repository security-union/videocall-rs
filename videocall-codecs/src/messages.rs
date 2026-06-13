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
    /// Main-thread ACK of the cumulative number of decoded frames it has drained from the
    /// worker->main `postMessage` queue (issue #1252, stage-3 paint lag). The worker subtracts
    /// this from its own `FRAMES_EMITTED` count at the 1Hz tick to estimate the
    /// decoded-but-unpainted backlog living in the postMessage + paint task queues —
    /// a region `decode_queue_size()` cannot observe.
    PaintProgress { painted: u64 },
}

/// Video statistics message sent by the worker
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoStatsMessage {
    pub kind: String,
    pub from_peer: Option<String>,
    pub to_peer: Option<String>,
    pub frames_buffered: Option<u64>,
    /// Total buffered video playout latency in ms (issue #1252): stage-1 jitter-buffer backlog
    /// span + stage-2 decoder-queue depth × source frame interval.
    pub playout_latency_ms: Option<f64>,
    /// Stage-1 attribution of `playout_latency_ms`: the jitter-buffer backlog span alone.
    pub playout_stage1_span_ms: Option<f64>,
    /// Stage-3 paint lag in ms (issue #1252): decoded-but-unpainted frames living in the
    /// worker->main `postMessage` queue + main-thread paint task queue, valued at one
    /// source-frame-interval per frame. Computed in the worker as
    /// `frames_emitted - frames_painted` so the backlog isn't hidden by the FIFO delay that
    /// the worker's own stats message rides through.
    pub playout_paint_lag_ms: Option<f64>,
}

impl VideoStatsMessage {
    pub fn new(
        from_peer: String,
        to_peer: String,
        frames_buffered: u64,
        playout_latency_ms: f64,
        playout_stage1_span_ms: f64,
        playout_paint_lag_ms: f64,
    ) -> Self {
        Self {
            kind: "video_stats".to_string(),
            from_peer: Some(from_peer),
            to_peer: Some(to_peer),
            frames_buffered: Some(frames_buffered),
            playout_latency_ms: Some(playout_latency_ms),
            playout_stage1_span_ms: Some(playout_stage1_span_ms),
            playout_paint_lag_ms: Some(playout_paint_lag_ms),
        }
    }
}

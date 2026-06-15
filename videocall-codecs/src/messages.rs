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
    /// **Test-only** (issue #1022): insert a crafted frame into the worker's
    /// [`JitterBuffer`](crate::jitter_buffer::JitterBuffer) using the `arrival_time_ms`
    /// carried in the `FrameBuffer` itself, instead of stamping it with the worker's
    /// wall clock the way [`WorkerMessage::DecodeFrame`] does.
    ///
    /// This exists solely so an E2E spec can deterministically form a *stale* head-of-line
    /// backlog (back-dated arrival time) and let the worker's ~10ms tick trip the #1020
    /// freshness deadline (`MAX_PLAYOUT_AGE_MS`), making the resulting `freshness_skip`
    /// diagnostic (#1045) observable from a browser test. It is emitted ONLY by the
    /// `MOCK_PEERS_ENABLED`-gated injection hook (see
    /// `videocall_client::freshness_inject`); no production code path sends it, so the
    /// production decode pipeline is byte-for-byte unaffected when the flag is off.
    InjectStaleFrame(FrameBuffer),
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

/// Discriminator value carried in [`RequestKeyframeMessage::kind`]. Used by the
/// main thread's `onmessage` dispatch to tell this proactive keyframe-request
/// signal apart from the `"video_stats"` diagnostics message — both ride the same
/// serde worker->main channel (the third kind of payload is a raw
/// `web_sys::VideoFrame`, which is distinguished structurally by a failed
/// `dyn_into::<VideoFrame>()`).
pub const REQUEST_KEYFRAME_KIND: &str = "request_keyframe";

/// Worker->main proactive keyframe-request signal (issue #1025).
///
/// Posted by the worker's `JitterBuffer` keyframe-request hook the instant the
/// freshness deadline evicts a stale **keyframe-less** backlog (see
/// `JitterBuffer::with_keyframe_request`). The buffer has dropped the stale
/// deltas but has no buffered keyframe to resume from, so playout is frozen on
/// the last-good frame until a fresh keyframe arrives — this message asks the
/// main thread (which owns the transport and the `PeerDecodeManager`) to issue a
/// `KEYFRAME_REQUEST` for this decoder's peer/stream immediately, rather than
/// waiting for the client's reactive gap-driven request.
///
/// `from_peer` / `to_peer` mirror the worker's diagnostics context and are
/// carried for log symmetry only; the main-side callback is per-decoder (already
/// bound to one peer + media type), so it needs no identity from the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestKeyframeMessage {
    pub kind: String,
    pub from_peer: Option<String>,
    pub to_peer: Option<String>,
}

impl RequestKeyframeMessage {
    pub fn new(from_peer: Option<String>, to_peer: Option<String>) -> Self {
        Self {
            kind: REQUEST_KEYFRAME_KIND.to_string(),
            from_peer,
            to_peer,
        }
    }
}

/// Discriminator carried in [`FreshnessSkipMessage::kind`] (issue #1045), to tell
/// it apart from `"video_stats"` / `"request_keyframe"` on the shared serde
/// worker->main channel.
pub const FRESHNESS_SKIP_KIND: &str = "freshness_skip";

/// Worker->main freshness-deadline skip diagnostic (issue #1045).
///
/// The jitter buffer's freshness deadline (#1020) runs INSIDE the decoder worker,
/// whose `log`/`console` output the main-thread console-log capture+upload pipeline
/// never sees — so in the field we could not confirm the freeze fix actually fired.
/// The worker posts this the instant a skip occurs; the main thread re-emits it
/// as a real `console.warn` line (the load-bearing upload path) and also keeps a
/// `DiagEvent` broadcast for structured in-process consumers. Both carry the
/// worker's `from_peer`/`to_peer` context. Mirrors `VideoStatsMessage`'s
/// worker->main forwarding shape, with console delivery added for field logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FreshnessSkipMessage {
    pub kind: String,
    pub from_peer: Option<String>,
    pub to_peer: Option<String>,
    /// Head-of-line age (ms) that tripped the deadline.
    pub head_age_ms: f64,
    /// Keyframe sequence skipped to, or `None` for the keyframe-less held case.
    pub keyframe_seq: Option<u64>,
    /// Stale frames evicted in this skip.
    pub dropped: u64,
}

impl FreshnessSkipMessage {
    pub fn new(
        from_peer: Option<String>,
        to_peer: Option<String>,
        head_age_ms: f64,
        keyframe_seq: Option<u64>,
        dropped: u64,
    ) -> Self {
        Self {
            kind: FRESHNESS_SKIP_KIND.to_string(),
            from_peer,
            to_peer,
            head_age_ms,
            keyframe_seq,
            dropped,
        }
    }
}

/// Discriminator carried in [`WorkerLogMessage::kind`] (issue #1356), to tell it
/// apart from `"video_stats"` / `"request_keyframe"` / `"freshness_skip"` on the
/// shared serde worker->main channel.
pub const WORKER_LOG_KIND: &str = "worker_log";

/// Worker->main `log::` facade forwarding message (issue #1356, follow-up to #1045).
///
/// `log::{warn,error,..}!` lines emitted INSIDE the decoder Web Worker
/// (`bin/worker_decoder.rs` and the modules it drives) never reach the main-thread
/// console-log capture+upload pipeline — the worker has its own global scope and its
/// own (until now, absent) `log` facade, so in the field those records were dropped
/// on the floor. The worker installs a `log::Log` implementation that posts this
/// message per enabled record; the main thread re-broadcasts it as a `DiagEvent`
/// (`handle_worker_diag_message`) so it lands in uploaded logs with the worker's
/// `from_peer`/`to_peer` context. Mirrors the `FreshnessSkipMessage` path (#1045).
///
/// `from_peer` / `to_peer` mirror the worker's diagnostics context (set via
/// `WorkerMessage::SetContext`) so a forwarded line can be attributed to the peer
/// whose decoder produced it. They are `Option` because a record can be emitted
/// before `SetContext` arrives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerLogMessage {
    pub kind: String,
    /// Log level as an uppercase string, e.g. "WARN" / "ERROR" / "INFO".
    pub level: String,
    /// The record's `target` (module path or explicit target), e.g.
    /// "videocall_codecs::decoder::wasm".
    pub target: String,
    /// The rendered log message body.
    pub message: String,
    pub from_peer: Option<String>,
    pub to_peer: Option<String>,
    /// Records suppressed by the worker's rate-limit since the last forwarded line
    /// (issue #1356). `0` on a normally-forwarded line; `> 0` folds a coalesced burst
    /// into this line so a chatty loop reports volume without per-record amplification.
    pub suppressed: u64,
}

impl WorkerLogMessage {
    pub fn new(
        level: String,
        target: String,
        message: String,
        from_peer: Option<String>,
        to_peer: Option<String>,
        suppressed: u64,
    ) -> Self {
        Self {
            kind: WORKER_LOG_KIND.to_string(),
            level,
            target,
            message,
            from_peer,
            to_peer,
            suppressed,
        }
    }
}

#[cfg(test)]
mod worker_log_disambiguation_tests {
    //! Serde disambiguation contract for the worker->main message channel (issue #1356).
    //!
    //! All five worker->main payloads share one JS-object channel and are told apart by their
    //! `kind` field, but several have overlapping/optional field sets. The dispatcher
    //! (`decoder/wasm.rs::handle_worker_diag_message` + `handle_worker_request_keyframe`)
    //! deserializes-then-checks-`kind`, so two hazards exist: (1) a `WorkerLogMessage` must still
    //! reach its own branch even though it *structurally* deserializes into the optional-field
    //! `VideoStatsMessage`/`RequestKeyframeMessage`; (2) the new `WorkerLogMessage` branch must not
    //! *swallow* an earlier message type. These tests pin both directions against the real structs
    //! via `serde_json`, whose missing-required-field / unknown-field / `Option`-as-null semantics
    //! match the runtime `serde-wasm-bindgen` codec. They run on the native test target (the wasm
    //! test harness is unrelated to this contract).
    use super::*;

    fn worker_log_wire() -> String {
        let m = WorkerLogMessage::new(
            "WARN".into(),
            "videocall_codecs::decoder::wasm".into(),
            "decoder fell behind".into(),
            Some("alice".into()),
            Some("bob".into()),
            3,
        );
        serde_json::to_string(&m).unwrap()
    }

    #[test]
    fn worker_log_round_trips_with_its_kind() {
        let wire = worker_log_wire();
        let back: WorkerLogMessage = serde_json::from_str(&wire).unwrap();
        assert_eq!(back.kind, WORKER_LOG_KIND);
        assert_eq!(back.level, "WARN");
        assert_eq!(back.target, "videocall_codecs::decoder::wasm");
        assert_eq!(back.message, "decoder fell behind");
        assert_eq!(back.from_peer.as_deref(), Some("alice"));
        assert_eq!(back.suppressed, 3);
    }

    #[test]
    fn worker_log_falls_through_overlapping_kinds() {
        // A WorkerLogMessage *does* deserialize into the optional-field shapes (extra fields are
        // ignored / required ones are present), but its kind is "worker_log", so the earlier
        // kind-guards reject it and it falls through to its own branch.
        let wire = worker_log_wire();
        let as_vs: VideoStatsMessage = serde_json::from_str(&wire).unwrap();
        assert_ne!(as_vs.kind, "video_stats");
        let as_rk: RequestKeyframeMessage = serde_json::from_str(&wire).unwrap();
        assert_ne!(as_rk.kind, REQUEST_KEYFRAME_KIND);
        // freshness_skip requires head_age_ms/dropped, which WorkerLogMessage lacks, so it can't
        // even deserialize into that shape.
        assert!(serde_json::from_str::<FreshnessSkipMessage>(&wire).is_err());
    }

    #[test]
    fn worker_log_branch_cannot_swallow_other_messages() {
        // The new WorkerLogMessage branch deserializes-then-checks-kind. Its three required String
        // fields (level/target/message) are absent from every other message, so none of them can
        // deserialize into WorkerLogMessage at all -> the branch is structurally unable to swallow
        // them, independent of the kind guard.
        let vs = VideoStatsMessage::new("a".into(), "b".into(), 5, 1.0, 2.0, 3.0);
        assert!(
            serde_json::from_str::<WorkerLogMessage>(&serde_json::to_string(&vs).unwrap()).is_err()
        );

        let fs = FreshnessSkipMessage::new(None, None, 1800.0, Some(42), 7);
        assert!(
            serde_json::from_str::<WorkerLogMessage>(&serde_json::to_string(&fs).unwrap()).is_err()
        );

        let rk = RequestKeyframeMessage::new(None, None);
        assert!(
            serde_json::from_str::<WorkerLogMessage>(&serde_json::to_string(&rk).unwrap()).is_err()
        );
    }
}

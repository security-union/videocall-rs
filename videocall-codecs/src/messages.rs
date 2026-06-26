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
    /// Cumulative count of resync-to-live governor skips (issue #1252): how many times the
    /// decode-side governor jumped this stream forward to live to shed accumulated lag. A COUNTER,
    /// not a gauge: it rises within a decoder-pipeline lifetime and is preserved across `flush()`,
    /// but resets to 0 when the pipeline is rebuilt (`reset_pipeline()` on decoder-error recovery).
    /// Consume via `increase()`/`rate()`, which tolerate the reset; a rising value proves the
    /// governor fired in the field.
    pub playout_skip_to_live_total: Option<u64>,
    /// Content-staleness in ms (issue #1641): the content AGE of the video currently being painted
    /// — how old (in capture/wall-clock terms) the released content is, drift-baselined so the
    /// unsynchronized publisher/receiver clock offset cancels. Distinct from `playout_paint_lag_ms`,
    /// which measures the worker→main queue DEPTH (count × interval): a stream draining at display
    /// rate keeps that depth small and reads ~0 even for minutes-old content. Content age vs queue
    /// depth: this surfaces the "video lagged by minutes while paint-lag/playout-latency read ~0"
    /// case (#1631 M2). Unlike `playout_latency_ms` (capped at 1800ms) this can exceed 1800ms.
    pub content_staleness_ms: Option<f64>,
}

impl VideoStatsMessage {
    // 8 worker→main video-diagnostic fields (issue #1641 added content_staleness_ms as the 8th);
    // bundling them into a struct just to dodge the lint would not improve this thin DTO ctor.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        from_peer: String,
        to_peer: String,
        frames_buffered: u64,
        playout_latency_ms: f64,
        playout_stage1_span_ms: f64,
        playout_paint_lag_ms: f64,
        playout_skip_to_live_total: u64,
        content_staleness_ms: f64,
    ) -> Self {
        Self {
            kind: "video_stats".to_string(),
            from_peer: Some(from_peer),
            to_peer: Some(to_peer),
            frames_buffered: Some(frames_buffered),
            playout_latency_ms: Some(playout_latency_ms),
            playout_stage1_span_ms: Some(playout_stage1_span_ms),
            playout_paint_lag_ms: Some(playout_paint_lag_ms),
            playout_skip_to_live_total: Some(playout_skip_to_live_total),
            content_staleness_ms: Some(content_staleness_ms),
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
///
/// `head_age_ms` (issue #1479) carries the head-of-line backlog age (ms) that
/// tripped the freshness deadline and drove this proactive request. The main
/// thread's per-receiver cross-sender PLI budget
/// (`videocall_client::decode::pli_budget`) uses it as a staleness priority so
/// that, when its global cap is reached, the STALEST contending stream's request
/// is preserved and a fresher one is shed. `#[serde(default)]` keeps it optional
/// on the wire: an old worker build (or any other message structurally
/// deserializing into this shape) that omits the field decodes to `0.0`, so the
/// serde-disambiguation contract (`kind`-based dispatch) is unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestKeyframeMessage {
    pub kind: String,
    pub from_peer: Option<String>,
    pub to_peer: Option<String>,
    /// Head-of-line backlog age (ms) that tripped the freshness deadline (issue #1479).
    /// Defaulted on the wire so older payloads / overlapping shapes still deserialize.
    #[serde(default)]
    pub head_age_ms: f64,
}

impl RequestKeyframeMessage {
    pub fn new(from_peer: Option<String>, to_peer: Option<String>, head_age_ms: f64) -> Self {
        Self {
            kind: REQUEST_KEYFRAME_KIND.to_string(),
            from_peer,
            to_peer,
            head_age_ms,
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
    /// `true` when this event is the keyframe-less hold-ceiling escalation (issue #1662): the
    /// held-last-good freeze exceeded `MAX_KEYFRAME_LESS_HOLD_MS` and the receiver forced a
    /// decoder-pipeline reset rather than continue holding indefinitely. `false` for a routine
    /// skip-to-live or a below-ceiling keyframe-less hold. Surfaced so the field log can
    /// distinguish a bounded-freeze escalation (the #1662 fix firing) from a routine freshness skip.
    #[serde(default)]
    pub escalated: bool,
}

impl FreshnessSkipMessage {
    pub fn new(
        from_peer: Option<String>,
        to_peer: Option<String>,
        head_age_ms: f64,
        keyframe_seq: Option<u64>,
        dropped: u64,
        escalated: bool,
    ) -> Self {
        Self {
            kind: FRESHNESS_SKIP_KIND.to_string(),
            from_peer,
            to_peer,
            head_age_ms,
            keyframe_seq,
            dropped,
            escalated,
        }
    }

    /// Render the field-log console line for this skip (issue #1384, follow-up to
    /// #1045).
    ///
    /// This is the load-bearing delivery for the freshness-deadline signal: the
    /// main thread re-emits the returned string via `console.warn`, which the
    /// console-log upload pipeline captures. The `[JITTER_BUFFER] freshness_skip`
    /// prefix and the `head_age=`/`dropped=`/`keyframe_seq=` field tokens are a
    /// **grep contract** the #1045/#1020 field investigation keys on, so the
    /// formatting is pinned by host tests below rather than living only in the
    /// wasm-gated emit arm (which no host test can exercise). `head_age` rounds to
    /// whole milliseconds (`{:.0}`); a `keyframe_seq` of `None` is the keyframe-less
    /// held-last-good case and renders as `none (held last-good)`. The `escalated=`
    /// token (issue #1662) is `true` only for the keyframe-less hold-ceiling escalation
    /// (decoder-pipeline reset) and `false` for routine skips, so the field investigation
    /// can grep the bounded-freeze escalations apart from ordinary freshness skips.
    pub fn console_line(&self) -> String {
        let from = self.from_peer.as_deref().unwrap_or_default();
        let to = self.to_peer.as_deref().unwrap_or_default();
        let keyframe = self
            .keyframe_seq
            .map(|s| s.to_string())
            .unwrap_or_else(|| "none (held last-good)".to_string());
        format!(
            "[JITTER_BUFFER] freshness_skip {from}->{to}: head_age={:.0}ms dropped={} keyframe_seq={keyframe} escalated={}",
            self.head_age_ms, self.dropped, self.escalated
        )
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
        let vs = VideoStatsMessage::new("a".into(), "b".into(), 5, 1.0, 2.0, 3.0, 4, 5.0);
        assert!(
            serde_json::from_str::<WorkerLogMessage>(&serde_json::to_string(&vs).unwrap()).is_err()
        );

        let fs = FreshnessSkipMessage::new(None, None, 1800.0, Some(42), 7, false);
        assert!(
            serde_json::from_str::<WorkerLogMessage>(&serde_json::to_string(&fs).unwrap()).is_err()
        );

        let rk = RequestKeyframeMessage::new(None, None, 0.0);
        assert!(
            serde_json::from_str::<WorkerLogMessage>(&serde_json::to_string(&rk).unwrap()).is_err()
        );
    }
}

#[cfg(test)]
mod freshness_skip_console_line_tests {
    //! Grep-contract for the freshness-skip field-log line (issue #1384, follow-up to #1045).
    //!
    //! The `[JITTER_BUFFER] freshness_skip` console line is the load-bearing delivery of the
    //! #1020 freshness-deadline signal to uploaded field logs, and the field investigation for
    //! #1045/#1020 greps for that exact prefix + field tokens. The emit itself lives in the
    //! wasm-gated `decoder/wasm.rs` arm (`console::warn_1`), which no host test can exercise, so
    //! the formatting was extracted into `FreshnessSkipMessage::console_line` and pinned here.
    //! Mutating the prefix, a field token, or the keyframe-`None` rendering in source must fail
    //! one of these tests (acceptance criterion of #1384).
    use super::*;

    #[test]
    fn pins_prefix_and_field_tokens() {
        let line = FreshnessSkipMessage::new(
            Some("alice".into()),
            Some("bob".into()),
            1234.0,
            Some(42),
            7,
            false,
        )
        .console_line();
        // The load-bearing grep prefix.
        assert!(
            line.starts_with("[JITTER_BUFFER] freshness_skip"),
            "prefix grep contract broken: {line}"
        );
        // Each field token the investigation keys on.
        assert!(
            line.contains("head_age="),
            "missing head_age= token: {line}"
        );
        assert!(line.contains("dropped="), "missing dropped= token: {line}");
        assert!(
            line.contains("keyframe_seq="),
            "missing keyframe_seq= token: {line}"
        );
        // Escalation token (issue #1662): the field investigation greps it to separate
        // bounded-freeze escalations from routine freshness skips.
        assert!(
            line.contains("escalated="),
            "missing escalated= token: {line}"
        );
        // Peer attribution rendered as from->to.
        assert!(
            line.contains("alice->bob"),
            "missing peer attribution: {line}"
        );
    }

    #[test]
    fn renders_escalated_flag() {
        // Issue #1662: the keyframe-less hold-ceiling escalation renders `escalated=true`; a routine
        // skip renders `escalated=false`. Both renderings are pinned because the field investigation
        // greps the token to count bounded-freeze escalations apart from ordinary skips.
        let escalated = FreshnessSkipMessage::new(None, None, 6000.0, None, 0, true).console_line();
        assert!(
            escalated.contains("escalated=true"),
            "escalation must render escalated=true: {escalated}"
        );
        let routine = FreshnessSkipMessage::new(None, None, 1800.0, None, 7, false).console_line();
        assert!(
            routine.contains("escalated=false"),
            "routine skip must render escalated=false: {routine}"
        );
    }

    #[test]
    fn renders_keyframe_some_as_bare_sequence() {
        let line = FreshnessSkipMessage::new(None, None, 1800.0, Some(42), 7, false).console_line();
        assert!(
            line.contains("keyframe_seq=42"),
            "Some(42) should render as keyframe_seq=42: {line}"
        );
        assert!(
            !line.contains("none (held last-good)"),
            "Some(_) must not render the held-last-good marker: {line}"
        );
    }

    #[test]
    fn renders_keyframe_none_as_held_last_good() {
        // The keyframe-less held case (#1020 evict-and-hold) is the distinct signal the
        // investigation distinguishes from a skip-to-live, so its rendering is pinned.
        let line = FreshnessSkipMessage::new(None, None, 1800.0, None, 7, false).console_line();
        assert!(
            line.contains("keyframe_seq=none (held last-good)"),
            "None should render as keyframe_seq=none (held last-good): {line}"
        );
    }

    #[test]
    fn rounds_head_age_to_whole_millis() {
        // `{:.0}` rounds: 1234.6 -> 1235.
        let line = FreshnessSkipMessage::new(None, None, 1234.6, Some(1), 0, false).console_line();
        assert!(
            line.contains("head_age=1235ms"),
            "head_age should round to 1235ms: {line}"
        );
        assert!(
            !line.contains("1234.6"),
            "head_age must not carry fractional digits: {line}"
        );
    }

    #[test]
    fn empty_peers_render_as_empty_arrow() {
        // `None` peers (a skip forwarded before SetContext backfill) render as `->`,
        // matching the prior inline `unwrap_or_default()` behavior — keeps the line
        // shape stable for the grep. With issue #1640 this is now rare (SetContext is
        // sent at peer creation), but can still occur if the freshness deadline trips
        // on the very first frame before the postMessage delivering SetContext arrives.
        let line = FreshnessSkipMessage::new(None, None, 100.0, Some(5), 1, false).console_line();
        assert!(
            line.contains("freshness_skip ->:"),
            "empty peers should render as `->`: {line}"
        );
    }

    /// Issue #1640: both `from_peer` and `to_peer` are now session_id strings (u64).
    /// This test pins the semantic contract: the arrow format `{from}->{to}` renders
    /// numeric session_ids so that log-parsing scripts can treat both sides uniformly
    /// (e.g. correlate `from_peer` to the local receiver's session_id and `to_peer`
    /// to the publisher's session_id without guessing whether a field is an email).
    #[test]
    fn session_id_peers_render_as_numeric_arrow() {
        // Simulates the post-#1640 runtime: from_peer = local session_id,
        // to_peer = remote publisher session_id — both u64 strings.
        let line = FreshnessSkipMessage::new(
            Some("9876543210".into()),
            Some("1234567890".into()),
            500.0,
            Some(7),
            3,
            false,
        )
        .console_line();
        assert!(
            line.contains("9876543210->1234567890"),
            "session_id peers should render as numeric arrow: {line}"
        );
        // Grep-contract: the full prefix + arrow is parseable in one regex.
        assert!(
            line.starts_with("[JITTER_BUFFER] freshness_skip 9876543210->1234567890:"),
            "prefix + session_id arrow must form a single grep-able prefix: {line}"
        );
    }
}

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

//! The JitterBuffer, which reorders, buffers, and prepares frames for the decoder.

use crate::decoder::Decodable;
use crate::frame::{FrameBuffer, FrameType, VideoFrame};
use crate::jitter_estimator::JitterEstimator;
use std::collections::BTreeMap;

// --- Playout Delay Constants ---
/// The minimum delay we will allow. Prevents the buffer from becoming completely empty.
const MIN_PLAYOUT_DELAY_MS: f64 = 10.0;
/// The maximum delay. Prevents the delay from growing indefinitely.
const MAX_PLAYOUT_DELAY_MS: f64 = 500.0;
/// The freshness deadline: an upper bound on how long the head-of-line frame may sit in the
/// jitter buffer before we declare the backlog stale and skip to live.
///
/// `MAX_PLAYOUT_DELAY_MS` (500ms) only ever acts as a *lower-bound* clamp ceiling — it sets how
/// long we are willing to wait before releasing a frame to absorb jitter. It is NOT a deadline:
/// once a backlog accumulates during a network stall, frames are drained in order and painted as
/// fast as the decoder will accept them, so video drifts permanently behind real time and desyncs
/// from audio (issue #1020). This constant adds the missing upper bound.
///
/// Rationale for 1800ms:
/// - It must sit comfortably *above* the largest legitimate playout delay so that normal jitter is
///   never mistaken for staleness. The release gate caps the adaptive target at
///   `MAX_PLAYOUT_DELAY_MS` = 500ms; 1800ms leaves ~1.3s of headroom (3.6x the max normal delay),
///   so ordinary jitter, reordering, and a single late frame can never trip the skip path.
/// - It is small enough that a real stall is corrected within roughly two seconds — short enough
///   that a viewer perceives "it caught up to live" rather than "it played a slow-motion replay,"
///   which is the failure mode #1020 describes.
/// - This is a live videoconference: liveness beats completeness. ~1.8s is the boundary past which
///   continuing to drain buffered video does more harm (A/V desync, growing lag) than dropping it.
const MAX_PLAYOUT_AGE_MS: f64 = 1800.0;
/// *Base* (first-strike) interval between proactive keyframe requests fired by the
/// keyframe-less eviction path (issue #1025). Matched to the relay's KEYFRAME_REQUEST
/// window (`KEYFRAME_REQUEST_WINDOW_MS`, ~1s) so the FIRST request of a stall emits at
/// the rate the relay will actually honor — instead of one per ~10ms eviction tick.
///
/// Under a *sustained or recurring* stall this is only the first step: each subsequent
/// request doubles the interval (exponential backoff, capped via
/// `PROACTIVE_KEYFRAME_REQUEST_MAX_BACKOFF_EXP`) so the receiver does not spray ~1 PLI/sec
/// at the speaker for the whole meeting — the #1479 keyframe-storm amplifier. Crucially the
/// backoff is keyed to *request* cadence, not to playout release: the #1479 field shape is
/// many short (1–5s) freezes interleaved with smooth playout, NOT one long stall, so a reset
/// on every frame release would zero the exponent between episodes and never damp the
/// meeting-wide storm. Instead the exponent persists across closely-spaced re-freezes and
/// only resets after a genuinely quiet interval with no proactive request — see
/// `PROACTIVE_KEYFRAME_REQUEST_BACKOFF_RESET_MS` and
/// `consecutive_proactive_keyframe_requests`. See `last_keyframe_request_ms`.
const PROACTIVE_KEYFRAME_REQUEST_MIN_INTERVAL_MS: f64 = 1000.0;
/// Maximum backoff exponent applied to `PROACTIVE_KEYFRAME_REQUEST_MIN_INTERVAL_MS` under a
/// sustained/recurring keyframe-less stall (issue #1479). With a 1000ms base, exponent 3 caps
/// the proactive-PLI cadence at `1000 * 2^3 = 8000ms` — i.e. once recovery is clearly not
/// arriving, this receiver pokes the speaker at most ~1/8s instead of ~1/s, damping the
/// self-sustaining PLI storm. Going quiet is safe: the independent reactive gap-driven request
/// path (`peer_decode_manager::should_request_keyframe`, 1s base + its own backoff + uncapped
/// slow-retry) remains the binding lower bound on freeze recovery; this proactive path is only
/// an accelerator on top of it, and the whole point of #1479 is to stop it from amplifying.
const PROACTIVE_KEYFRAME_REQUEST_MAX_BACKOFF_EXP: u32 = 3;
/// Quiet interval (ms) since the last proactive keyframe request after which the backoff
/// exponent resets to 0 (issue #1479). Set comfortably ABOVE the maximum backed-off interval
/// (`1000 * 2^3 = 8000ms`) so a receiver legitimately firing at the capped ~1/8s cadence
/// during an ongoing stall does NOT spuriously reset itself, while a genuinely recovered
/// stream — one that has not needed a proactive request for >12s — re-arms at full speed for
/// the next independent stall. This time-based reset is what makes the backoff accumulate
/// across the storm's short, recurring freezes (each spaced well under 12s) instead of being
/// zeroed by the brief smooth playout between them.
///
/// Note: this is intentionally shorter than the *reactive* gap-driven path's decay window
/// (`peer_decode_manager`'s `KEYFRAME_BACKOFF_DECAY_MS`, 30s). The two paths are independent;
/// the proactive path here is a more aggressive accelerator, so re-arming it sooner after a
/// genuine recovery is fine — and it must stay strictly above this path's own 8s cap (see
/// `PROACTIVE_KEYFRAME_REQUEST_MAX_BACKOFF_EXP`) so a stall firing at the capped cadence never
/// self-resets mid-storm.
const PROACTIVE_KEYFRAME_REQUEST_BACKOFF_RESET_MS: f64 = 12_000.0;
/// A multiplier applied to the jitter estimate to provide a safety margin.
/// A value of 3.0 means we buffer enough to handle jitter up to 3x the running average.
const JITTER_MULTIPLIER: f64 = 3.0;
/// A smoothing factor for delay updates to prevent rapid, jarring changes.
const DELAY_SMOOTHING_FACTOR: f64 = 0.99;

/// Release-side WebCodecs backpressure high-water mark (issue #1024).
///
/// Frames released by the jitter buffer are handed to the WebCodecs `VideoDecoder`, whose own
/// internal queue is *unpaced* — given a burst it decodes and paints as fast as it can rather than
/// at display rate. That is the "second buffer stage" #1020 describes: the jitter-buffer freshness
/// deadline bounds the first stage, but without this gate a recovery burst still piles into the
/// decoder and is painted back-to-back. So before releasing a frame we consult the live decoder
/// queue depth (`Decodable::decode_queue_depth()`) and stop releasing while it is at/above this
/// mark, letting the decoder drain to display rate. Frames simply stay buffered for the next tick.
///
/// Chosen as 3: large enough to keep the decoder continuously fed (no underrun/stutter at ~30fps,
/// where a healthy queue sits at 0-1) yet small enough that the second-stage backlog can never
/// exceed a few frames (~100ms).
///
/// Scope of the "no unbounded lag" guarantee (issue #1324). This gate bounds **buffer/memory** lag,
/// not playout recovery in every case:
/// - **Buffer/memory lag is always bounded.** If a backup persists long enough that the
///   head-of-line frame ages past `MAX_PLAYOUT_AGE_MS`, the freshness deadline — which runs
///   *before* this gate every tick — skips to live (or evicts a stale keyframe-less backlog), and
///   `MAX_BUFFER_SIZE` caps the buffer regardless. So the jitter buffer can never grow unbounded
///   behind a slow decoder.
/// - **Playout recovery has a wedged-decoder caveat.** The freshness deadline only fires when the
///   *head-of-line frame* is itself stale. If the decoder is truly wedged — `decode_queue_depth()`
///   pinned at/above this mark, never draining, while still reporting `state() == Configured` —
///   this gate holds release every tick, *including* a fresh skip-to-live keyframe whose own
///   head-of-line age is below `MAX_PLAYOUT_AGE_MS`. The deadline never sees a stale head, and
///   because `decode()` is never called the `decode()`-error → `reset_pipeline()` path in
///   `worker_decoder.rs` never runs either, so playout could freeze on the last-good frame.
///   `MAX_BACKPRESSURE_HOLD_MS` + the held-too-long escape hatch in
///   `find_and_move_continuous_frames` closes that gap: once release has been held by this gate
///   longer than that threshold while a releasable frame waits, the buffer force-releases the head
///   frame and, if that does not unwedge the decoder, escalates to `Decodable::reset()`, so a
///   wedged decoder recovers internally within a bounded time instead of freezing indefinitely.
///
/// This is the single source of truth for the queue-depth threshold: `worker_decoder.rs` derives
/// its `WEBCODECS_QUEUE_WARN_DEPTH` observability threshold from this constant so the two can't
/// silently desync (the warn uses `>` while this gate uses `>=` — see that constant for why the
/// operator difference is intentional).
pub const DECODE_QUEUE_HIGH_WATER_MARK: u32 = 3;

/// Held-too-long backpressure escape-hatch threshold (issue #1324), in wall-clock milliseconds.
///
/// While the release-side gate (`DECODE_QUEUE_HIGH_WATER_MARK`) holds a *releasable* frame because
/// the decoder queue is at/above the high-water mark, we track how long the gate has been holding
/// it *continuously* (the `backpressure_hold_since_ms` clock, started at the first such tick and
/// cleared on any release / non-hold / flush). If that elapsed time exceeds this threshold the gate
/// is bypassed once (escape hatch) to break a genuinely wedged decoder — see
/// `find_and_move_continuous_frames`.
///
/// Measured from the first gate-hold, NOT the last release: during a legitimate stall (waiting for
/// a keyframe, or the freshness deadline working through a backlog) there may be no release for
/// seconds, but that is not the gate wedging playout, so the clock only runs while the gate is
/// actually holding a frame it could otherwise release.
///
/// Wall-clock, not a tick counter: the worker polls every ~10ms today, but a backgrounded tab
/// clamps/pauses `setInterval`, so a fixed held-tick count would mean wildly different real time
/// depending on focus. Measuring elapsed wall-clock makes the threshold tick-rate-independent.
///
/// Relationship to `MAX_PLAYOUT_AGE_MS` (1800ms): deliberately set ABOVE it (2000ms) so the normal
/// stale-backlog path always gets its chance first. The freshness deadline fires whenever the
/// *head-of-line* frame ages past 1800ms and handles every case it can see (skip-to-live / evict).
/// This escape only needs to fire for the case the deadline structurally *cannot* see — a wedged
/// decoder holding back a frame whose own head-of-line age is still fresh (< 1800ms). Sitting at
/// 2000ms guarantees we never pre-empt the deadline's cheaper skip-to-live for a merely-slow
/// decoder, and only the truly-wedged case (where the deadline never fires) reaches the escape.
///
/// Field-tunable: raising it makes the escape more conservative (longer freeze tolerated before
/// forcing recovery); lowering it toward `MAX_PLAYOUT_AGE_MS` risks the escape racing the freshness
/// deadline on a merely-slow decoder. Keep it strictly above `MAX_PLAYOUT_AGE_MS`.
const MAX_BACKPRESSURE_HOLD_MS: f64 = 2000.0;

/// The maximum number of frames the buffer will hold before rejecting new ones.
const MAX_BUFFER_SIZE: usize = 200;
// From libwebrtc's jitter_buffer_common.h
const MAX_CONSECUTIVE_OLD_FRAMES: u64 = 300;
/// If an incoming keyframe is this many sequence numbers behind the last decoded frame, we assume
/// the stream restarted (e.g., camera switch) and flush immediately. Smaller rollbacks are treated
/// as harmless reordering.
const STREAM_RESTART_BACKTRACK_THRESHOLD: u64 = 30;

// --- Source-cadence estimator (issue #1252 playout-latency metric) ---
/// Default source frame interval (~30fps) used until enough released-frame samples accumulate.
const DEFAULT_SOURCE_FRAME_INTERVAL_MS: f64 = 33.3;
/// EWMA smoothing factor for the source frame-interval estimate. Small so a single burst/stall
/// sample can't whip the estimate around; it tracks the steady-state source cadence.
const SOURCE_FRAME_INTERVAL_EWMA_ALPHA: f64 = 0.1;
/// Plausible source-cadence bounds (≈1fps..125fps). Released-frame inter-arrival deltas outside
/// this band are NOT folded into the estimate: a delta of 0 (frames that arrived in the same burst
/// or out of order) would drag it toward the decoder drain rate and zero the metric, and a
/// multi-second delta (post-stall) would spuriously inflate it. Clamping to this band keeps the
/// estimate anchored to real source cadence.
const MIN_SOURCE_FRAME_INTERVAL_MS: f64 = 8.0;
const MAX_SOURCE_FRAME_INTERVAL_MS: f64 = 1000.0;

// --- Resync-to-live governor (issue #1252, v1) ---
//
// Field read on conceptcar7 confirmed H1: under loss a slow receiver's
// `playout_latency` total tracks the stage-1 jitter-buffer backlog span while the decode
// queue stays ≈ 0, so the multi-second video-behind-audio lag lives in THIS buffer. The
// 1800ms freshness deadline (`MAX_PLAYOUT_AGE_MS`) only fires when the *head-of-line* frame
// is itself stale; a backlog can sit at 1.2–1.7s of accumulated lag with a perfectly fresh
// head (frames keep arriving) and never trip it. The governor adds a second, latency-driven
// trigger that resyncs to live by skipping the buffered backlog to the newest buffered
// keyframe — reusing the exact same skip helper the deadline uses, so the two cannot diverge.
//
// v1 is a SAFE SUBSET: it introduces ZERO new keyframe-request / PLI sources. If there is no
// buffered keyframe to skip to, the governor does nothing and leaves that case entirely to the
// 1800ms deadline (which owns keyframe-less eviction + the throttled proactive request). This
// preserves the #1287/#1297/#814 keyframe-storm-avoidance invariant.

/// Buffered playout-latency total (ms) at/above which the governor begins counting toward a
/// resync skip. Chosen within the designed 1200–1500ms engage band and comfortably below the
/// 1800ms freshness deadline so the governor resyncs the *fresh-head* backlog case the deadline
/// structurally cannot see, while still leaving the deadline as the backstop. The loss repro
/// pins steady-state lag near ~1700ms, so 1300ms engages well before the deadline would.
const GOVERNOR_ENGAGE_MS: f64 = 1300.0;
/// Buffered playout-latency total (ms) below which the engaged EPISODE is considered recovered
/// and `governor_engaged` clears. Set just below `MAX_PLAYOUT_DELAY_MS` (500) so the episode is
/// marked recovered once the stream is back inside the normal adaptive-delay envelope. This is an
/// observability/episode boundary, NOT a skip gate — the skip is gated by sustain re-accumulation
/// plus `GOVERNOR_COOLDOWN_MS` (see `run_resync_governor`). A latch-gate here would deadlock
/// because post-skip latency can settle above this value (see the `governor_engaged` field doc).
const GOVERNOR_DISENGAGE_MS: f64 = 450.0;
/// Consecutive ticks the latency total must stay at/above `GOVERNOR_ENGAGE_MS` before the
/// governor skips. At the ~10ms worker tick this is ~400ms of sustained high latency, long
/// enough that transient jitter/reordering spikes cannot trip a skip but short enough to react
/// well within the engage band.
const GOVERNOR_SUSTAIN_TICKS: u32 = 40;
/// Minimum wall-clock interval (ms) between governor skips. ~one keyframe RTT: after a skip the
/// buffer must be allowed to refill and the latency to recompute before another skip is even
/// considered, so the governor cannot thrash-skip during recovery.
const GOVERNOR_COOLDOWN_MS: f64 = 2500.0;

/// Compile-time guard on the load-bearing governor/deadline threshold ordering:
/// `GOVERNOR_DISENGAGE_MS (450) < MAX_PLAYOUT_DELAY_MS (500) < GOVERNOR_ENGAGE_MS (1300) <
/// MAX_PLAYOUT_AGE_MS (1800)`. This ordering is what makes the governor coexist with the
/// freshness deadline rather than fight it: disengage sits inside the normal adaptive-delay
/// envelope (so an engaged episode is marked recovered there), engage sits above that envelope but below the
/// deadline (so the governor handles the fresh-head backlog the deadline cannot see, while the
/// deadline remains the backstop for a genuinely stale head). If any of these constants is
/// re-tuned across a boundary this assert fails the build, forcing a re-derivation rather than a
/// silent behavior change. (f64 comparison in a const assert is permitted on current Rust; if
/// const eval ever rejects it, that is a real signal to stop, not to work around.)
const _: () = assert!(
    GOVERNOR_DISENGAGE_MS < MAX_PLAYOUT_DELAY_MS
        && MAX_PLAYOUT_DELAY_MS < GOVERNOR_ENGAGE_MS
        && GOVERNOR_ENGAGE_MS < MAX_PLAYOUT_AGE_MS
);

/// Details of a freshness-deadline skip/eviction, surfaced to the worker so it can
/// forward the event to the main thread's console-log upload pipeline (issue #1045).
/// The freshness deadline runs INSIDE the decoder worker, so its outcome was
/// previously invisible in field logs — we could not confirm #1020 (and future
/// decode-side fixes) actually fired. `enforce_freshness_deadline` stashes this on
/// each skip; the worker `take`s it after polling and emits a diagnostic.
#[derive(Debug, Clone, PartialEq)]
pub struct FreshnessSkip {
    /// Head-of-line age (ms) that tripped the deadline.
    pub head_age_ms: f64,
    /// Sequence number of the keyframe we skipped to, or `None` when there was no
    /// buffered keyframe (the keyframe-less eviction path — playout held on the
    /// last-good frame; see #1025).
    pub keyframe_seq: Option<u64>,
    /// Number of stale frames evicted in this skip.
    pub dropped: u64,
}

pub struct JitterBuffer<T> {
    /// Frames that have been received but are not yet continuous with the last decoded frame.
    /// A BTreeMap is used to keep them sorted by sequence number automatically.
    buffered_frames: BTreeMap<u64, FrameBuffer>,

    /// The sequence number of the last frame that was sent to the decoder.
    last_decoded_sequence_number: Option<u64>,

    /// The jitter estimator for monitoring network conditions.
    jitter_estimator: JitterEstimator,

    /// The current adaptive target for playout delay, in milliseconds.
    target_playout_delay_ms: f64,

    /// A counter for frames that were dropped due to being stale.
    dropped_frames_count: u64,

    /// A counter for consecutive old frames to detect stream corruption.
    num_consecutive_old_frames: u64,

    /// Wall-clock time (ms) at which the release-side backpressure gate *began* continuously
    /// holding a releasable frame (issue #1324). `None` whenever the gate is not the thing blocking
    /// release — it is cleared on every successful release, on `flush()`, and on any tick the gate
    /// does not hold a releasable frame. While the gate holds a releasable frame across consecutive
    /// ticks this stays pinned at the first such tick, so `current_time - hold_since` measures how
    /// long the gate has *continuously* held — the wedged-decoder signal.
    ///
    /// Deliberately measured from the first gate-hold, NOT from the last successful release: during
    /// a legitimate network stall there may be no release for seconds while the buffer waits for a
    /// keyframe or the freshness deadline does its work — that is not a wedged decoder and must not
    /// trip the escape. See `MAX_BACKPRESSURE_HOLD_MS` and the escape hatch in
    /// `find_and_move_continuous_frames`.
    backpressure_hold_since_ms: Option<u128>,

    /// Rolling estimate of the SOURCE frame interval in ms (issue #1252). Derived from the
    /// inter-arrival spacing of *released* frames (which preserves source cadence), NOT the
    /// decoder's drain rate. Feeds the stage-2 term of the playout-latency metric.
    source_frame_interval_ms: f64,

    /// `arrival_time_ms` of the most recently released frame, used to derive the source-cadence
    /// inter-arrival delta. `None` until the first release (and after a flush).
    last_released_arrival_time_ms: Option<u128>,

    // --- Decoder Interface ---
    /// The abstract decoder that will receive frames ready for decoding.
    decoder: Box<dyn Decodable<Frame = T>>,

    /// Proactive keyframe-request hook (issue #1025). Invoked when the freshness
    /// deadline evicts a stale **keyframe-less** backlog: the buffer has dropped
    /// the stale deltas (bounding memory) but has NO buffered keyframe to resume
    /// from, so playout is frozen on the last-good frame until a fresh keyframe
    /// arrives. Calling this the instant we evict — rather than waiting for the
    /// client's reactive gap-driven request — shortens recovery. The actual PLI
    /// mechanism lives in `videocall-client`; this is the codecs→client handle the
    /// `TODO(#1020)` called for. Defaults to a no-op (`new`), so native tests and
    /// non-wasm callers need not supply one; the worker injects the real one via
    /// [`JitterBuffer::with_keyframe_request`]. The relay's per-(receiver,session)
    /// KEYFRAME_REQUEST limiter (#979/#1011) coalesces for the *publisher*, but it
    /// does not throttle *this* receiver's uplink or worker→main bus, so the fire
    /// is additionally rate-limited at the source — see `last_keyframe_request_ms`.
    request_keyframe: Box<dyn Fn()>,

    /// Wall-clock (ms) of the last proactive keyframe request fired by the
    /// keyframe-less eviction path (issue #1025). Source-side throttle: under a
    /// *sustained* keyframe-less stall (the publisher keeps sending deltas, each
    /// aging past the freshness deadline and evicting on the next ~10ms tick) the
    /// eviction cadence tracks the delta rate (~10–30/s), and without this gate the
    /// hook would spray the already-struggling receiver's uplink + worker→main bus
    /// with that many KEYFRAME_REQUESTs — packets the relay limiter then drops
    /// anyway. The first request of a stall fires after
    /// `PROACTIVE_KEYFRAME_REQUEST_MIN_INTERVAL_MS`; under a sustained stall the
    /// interval then grows exponentially (see
    /// `consecutive_proactive_keyframe_requests`) so the emitted rate decays from
    /// ~1/s toward ~1/8s instead of pinning at the relay window. `None` until the
    /// first fire; reset on `flush()` so a fresh stream can request immediately.
    last_keyframe_request_ms: Option<u128>,

    /// Backoff exponent for the proactive keyframe-request interval (issues #1025/#1479):
    /// the number of proactive requests fired without a long enough quiet gap to reset.
    /// Incremented on every fire; reset to 0 when the gap since the last fire exceeds
    /// `PROACTIVE_KEYFRAME_REQUEST_BACKOFF_RESET_MS` (genuine sustained recovery) or on
    /// `flush()`.
    ///
    /// Deliberately keyed to *request cadence*, NOT to frame release. The #1479 storm is many
    /// short (1–5s) freezes interleaved with smooth playout, not one long stall; resetting on
    /// frame release would zero the exponent during every smooth gap and the backoff would
    /// never accumulate across the storm — defeating the fix. Keying the reset to a quiet
    /// interval (> the capped 8s cadence) means the exponent persists across the storm's
    /// closely-spaced re-freezes and only resets once the stream has genuinely stopped needing
    /// proactive requests.
    consecutive_proactive_keyframe_requests: u32,

    /// Most recent freshness-deadline skip this poll produced (issue #1045), set by
    /// `enforce_freshness_deadline` and consumed by the worker via
    /// [`JitterBuffer::take_freshness_skip`] after each poll to forward it to the
    /// main thread for field-log visibility. `None` when no skip occurred.
    last_freshness_skip: Option<FreshnessSkip>,

    /// Wall-clock (ms) of the last freshness-skip diagnostic surfaced to the worker.
    /// Uses the same ~1s source-side cadence as proactive keyframe requests so a
    /// sustained stall does not post worker→main diagnostics on every decoder poll.
    last_freshness_skip_emit_ms: Option<u128>,

    /// Coalesced freshness-skip details accumulated while the diagnostic gate is
    /// closed. The next emitted diagnostic includes this pending dropped count.
    pending_freshness_skip: Option<FreshnessSkip>,

    /// Resync-governor (issue #1252): count of consecutive ~10ms ticks on which the buffered
    /// playout-latency total was at/above `GOVERNOR_ENGAGE_MS`. Resets to 0 whenever the total
    /// drops below the engage threshold and after a successful governor skip. The governor only
    /// skips once this reaches `GOVERNOR_SUSTAIN_TICKS`, so a transient jitter spike cannot trip
    /// a resync. Runtime state — reset on `flush()`.
    governor_sustain_ticks: u32,

    /// Resync-governor (issue #1252): engaged-EPISODE flag, `true` from the tick a governor skip
    /// fires until the latency total falls back below `GOVERNOR_DISENGAGE_MS`. This is an
    /// OBSERVABILITY / lifecycle flag, NOT a control gate: it is not read by the skip decision in
    /// `run_resync_governor` — it is written on skip/disengage and `flush()` resets it, but the
    /// only readers today are the unit tests (reserved for a future engaged-state export; the
    /// metric commit exports the skip COUNT, not this flag). The actual re-skip guard / hysteresis is the
    /// `governor_sustain_ticks` re-accumulation (reset to 0 after every skip, so the latency must
    /// climb back and hold for `GOVERNOR_SUSTAIN_TICKS` again) PLUS `GOVERNOR_COOLDOWN_MS`.
    /// Gating the skip on this latch was deliberately rejected: post-skip latency commonly settles
    /// BETWEEN `GOVERNOR_DISENGAGE_MS` (450) and `GOVERNOR_ENGAGE_MS` (1300), so the latch would
    /// not clear and a latch-gate would deadlock the next legitimate excursion. Runtime state —
    /// reset on `flush()`.
    governor_engaged: bool,

    /// Resync-governor (issue #1252): wall-clock (ms) of the last governor skip, used as the
    /// `GOVERNOR_COOLDOWN_MS` anchor so the governor cannot skip again while a prior resync is
    /// still recovering. `None` until the first skip. Runtime state — reset on `flush()`.
    governor_last_skip_ms: Option<u128>,

    /// Resync-governor (issue #1252): LIFETIME count of governor skips. This is the
    /// observability counter exposed via [`JitterBuffer::governor_skip_count`] for the
    /// Prometheus metric wired in a follow-up commit. Deliberately NOT reset by `flush()` — it
    /// is a cumulative metric, not runtime state.
    governor_skips: u64,
}

impl<T> JitterBuffer<T> {
    pub fn new(decoder: Box<dyn Decodable<Frame = T>>) -> Self {
        // Default: no proactive keyframe request (native/mock callers). The worker
        // supplies a real hook via `with_keyframe_request`.
        Self::with_keyframe_request(decoder, Box::new(|| {}))
    }

    /// Like [`JitterBuffer::new`] but injects the proactive keyframe-request hook
    /// (issue #1025) — mirroring how `decoder` is injected. See `request_keyframe`.
    pub fn with_keyframe_request(
        decoder: Box<dyn Decodable<Frame = T>>,
        request_keyframe: Box<dyn Fn()>,
    ) -> Self {
        Self {
            buffered_frames: BTreeMap::new(),
            last_decoded_sequence_number: None,
            jitter_estimator: JitterEstimator::new(),
            target_playout_delay_ms: MIN_PLAYOUT_DELAY_MS,
            dropped_frames_count: 0,
            num_consecutive_old_frames: 0,
            backpressure_hold_since_ms: None,
            source_frame_interval_ms: DEFAULT_SOURCE_FRAME_INTERVAL_MS,
            last_released_arrival_time_ms: None,
            decoder,
            request_keyframe,
            last_keyframe_request_ms: None,
            consecutive_proactive_keyframe_requests: 0,
            last_freshness_skip: None,
            last_freshness_skip_emit_ms: None,
            pending_freshness_skip: None,
            governor_sustain_ticks: 0,
            governor_engaged: false,
            governor_last_skip_ms: None,
            governor_skips: 0,
        }
    }

    /// Returns the current number of frames buffered and waiting in the jitter buffer.
    pub fn buffered_frames_len(&self) -> usize {
        self.buffered_frames.len()
    }

    /// Take the most recent freshness-deadline skip (issue #1045), clearing it. The
    /// worker calls this after each `find_and_move_continuous_frames` poll and, on
    /// `Some`, forwards the event to the main thread so it lands in uploaded field
    /// logs — the only way to confirm the #1020 freshness deadline actually fired on
    /// real clients (the deadline runs inside the decoder worker, whose logs the
    /// main-thread capture pipeline never sees).
    pub fn take_freshness_skip(&mut self) -> Option<FreshnessSkip> {
        self.last_freshness_skip.take()
    }

    fn merge_freshness_skip(mut existing: FreshnessSkip, next: FreshnessSkip) -> FreshnessSkip {
        existing.head_age_ms = existing.head_age_ms.max(next.head_age_ms);
        existing.keyframe_seq = next.keyframe_seq;
        existing.dropped += next.dropped;
        existing
    }

    fn record_freshness_skip(&mut self, current_time_ms: u128, skip: FreshnessSkip) {
        let should_emit = match self.last_freshness_skip_emit_ms {
            Some(last) => {
                (current_time_ms.saturating_sub(last)) as f64
                    >= PROACTIVE_KEYFRAME_REQUEST_MIN_INTERVAL_MS
            }
            None => true,
        };

        if should_emit {
            let skip = match self.pending_freshness_skip.take() {
                Some(pending) => Self::merge_freshness_skip(pending, skip),
                None => skip,
            };
            self.last_freshness_skip_emit_ms = Some(current_time_ms);
            self.last_freshness_skip = Some(skip);
        } else {
            self.pending_freshness_skip = Some(match self.pending_freshness_skip.take() {
                Some(pending) => Self::merge_freshness_skip(pending, skip),
                None => skip,
            });
        }
    }

    /// The main entry point for a new frame arriving from the network.
    pub fn insert_frame(&mut self, frame: VideoFrame, arrival_time_ms: u128) {
        let seq = frame.sequence_number;
        log::trace!("[JITTER_BUFFER] Inserting frame: {seq}");

        // --- Pre-insertion checks ---
        // 1. Ignore frames that are too old.
        if let Some(last_decoded) = self.last_decoded_sequence_number {
            if seq <= last_decoded {
                // Special case: if the old frame is a KEYFRAME, it likely indicates the sender has
                // restarted (e.g., camera switch). Flush immediately so we can start decoding from
                // this new keyframe without waiting for the old-frame counter threshold.
                if frame.frame_type == FrameType::KeyFrame
                    && last_decoded.saturating_sub(seq) > STREAM_RESTART_BACKTRACK_THRESHOLD
                {
                    log::debug!(
                        "[JITTER_BUFFER] Detected keyframe with older sequence ({seq} <= {last_decoded}). Assuming stream restart – flushing buffer."
                    );
                    self.flush();
                } else {
                    log::trace!("[JITTER_BUFFER] Ignoring old frame: {seq}");
                    self.num_consecutive_old_frames += 1;
                    if self.num_consecutive_old_frames > MAX_CONSECUTIVE_OLD_FRAMES {
                        log::debug!(
                            "[JITTER_BUFFER] Received {} consecutive old frames. Flushing buffer.",
                            self.num_consecutive_old_frames
                        );
                        self.flush();
                    }
                }
                return;
            }
        }

        // If we received a valid frame, reset the counter.
        self.num_consecutive_old_frames = 0;

        // 2. Check if the buffer is full.
        if self.buffered_frames.len() >= MAX_BUFFER_SIZE {
            // Allow a keyframe to clear the buffer if it's full.
            if frame.frame_type == FrameType::KeyFrame {
                log::debug!("[JITTER_BUFFER] Buffer full, but received keyframe. Clearing buffer.");
                self.drop_all_frames();
            } else {
                log::debug!("[JITTER_BUFFER] Buffer full. Rejecting frame: {seq}");
                return; // Reject the frame.
            }
        }

        log::trace!("[JITTER_BUFFER] Received frame: {seq}");

        self.jitter_estimator.update_estimate(seq, arrival_time_ms);
        self.update_target_playout_delay();

        let fb = FrameBuffer::new(frame, arrival_time_ms);
        self.buffered_frames.insert(seq, fb);

        self.find_and_move_continuous_frames(arrival_time_ms);
    }

    /// Updates the target playout delay based on the current jitter estimate.
    fn update_target_playout_delay(&mut self) {
        let jitter_estimate = self.jitter_estimator.get_jitter_estimate_ms();

        // Calculate the raw target delay with a safety margin.
        let raw_target = jitter_estimate * JITTER_MULTIPLIER;

        // Clamp the target to our defined min/max bounds.
        let clamped_target = raw_target.clamp(MIN_PLAYOUT_DELAY_MS, MAX_PLAYOUT_DELAY_MS);

        // Smooth the transition to the new target to avoid sudden changes.
        self.target_playout_delay_ms = (self.target_playout_delay_ms * DELAY_SMOOTHING_FACTOR)
            + (clamped_target * (1.0 - DELAY_SMOOTHING_FACTOR));
    }

    /// Checks the buffered frames and moves any continuous frames to the decodable queue.
    pub fn find_and_move_continuous_frames(&mut self, current_time_ms: u128) {
        let mut frames_were_moved = false;

        log::trace!(
            "[JB_POLL] Checking buffer. Last decoded: {:?}, Buffer size: {}, Target delay: {:.2}ms",
            self.last_decoded_sequence_number,
            self.buffered_frames.len(),
            self.target_playout_delay_ms
        );

        // Resync-to-live governor (issue #1252): evaluate the tick-level buffered playout latency
        // ONCE per tick, before the release loop. A successful governor skip drops the stale
        // backlog to the newest buffered keyframe; the loop below (freshness deadline + release
        // gate) then drains the post-governor state normally, releasing that keyframe through the
        // usual gap-recovery path. Placed before the loop (not inside it) because the governor's
        // trigger is the once-per-tick latency total, not a per-loop-iteration condition.
        self.run_resync_governor(current_time_ms);

        loop {
            let mut found_frame_to_move = false;

            // Freshness-deadline check (issue #1020): before evaluating the normal release gate,
            // discard any head-of-line backlog that has exceeded MAX_PLAYOUT_AGE_MS and skip to the
            // freshest decodable (keyframe) point. If this advanced the buffer state, restart the
            // loop so we re-derive the next decodable key from the post-skip state.
            //
            // Termination: `enforce_freshness_deadline` returns `true` ONLY when it actually removed
            // at least one frame, so `buffered_frames` strictly shrinks on every `continue`. When a
            // stale head can't be dropped (e.g. the chosen keyframe is already the head, or a stale
            // keyframe-less backlog), it returns `false` and we fall through to the normal release
            // gate instead of looping. This guarantees the loop terminates.
            if self.enforce_freshness_deadline(current_time_ms) {
                continue;
            }

            // Identify the frame the decoder could release next *before* consulting the gate, so the
            // wedged-decoder escape hatch below can tell whether a releasable frame is actually
            // waiting (vs. an empty buffer / waiting-for-keyframe, where holding is correct).
            let next_decodable_key = self.next_decodable_key();

            // Release-side backpressure (issue #1024). The freshness deadline above has already had
            // its chance to evict a stale backlog this tick; only AFTER that do we throttle the
            // release of fresh frames. If the WebCodecs decoder's own (unpaced) queue is at/above
            // the high-water mark, stop releasing this tick so it drains at display rate instead of
            // being shoveled full and painting back-to-back. The held frames stay buffered and are
            // re-evaluated on the next ~10ms tick; if the backup persists until the head ages past
            // MAX_PLAYOUT_AGE_MS the freshness deadline (which runs first) skips to live, so this
            // can never accumulate unbounded lag. Decoders with no observable queue (native/mock)
            // report depth 0 and are never throttled.
            if self.decoder.decode_queue_depth() >= DECODE_QUEUE_HIGH_WATER_MARK {
                // The gate would normally `break` and hold this tick.
                if next_decodable_key.is_none() {
                    // Nothing is releasable (empty buffer / waiting for a keyframe). The gate is not
                    // what's blocking playout, so the wedged-decoder clock must NOT run — clear it so
                    // a later genuine hold starts fresh. Then hold.
                    self.backpressure_hold_since_ms = None;
                    log::trace!(
                        "[JB_POLL] Decode-queue backpressure: holding (queue depth >= {DECODE_QUEUE_HIGH_WATER_MARK}, nothing releasable)."
                    );
                    break;
                }

                // A releasable frame exists but the gate is holding it. Start (or continue) the
                // continuous-hold clock; `current_time - hold_since` is how long the gate has held a
                // releasable frame back-to-back.
                let held_since = *self
                    .backpressure_hold_since_ms
                    .get_or_insert(current_time_ms);
                let held_for_ms = current_time_ms.saturating_sub(held_since) as f64;

                // Held-too-long escape hatch (issue #1324). A *truly wedged* decoder keeps its queue
                // pinned at/above the mark forever while still reporting `Configured`. When the
                // freshness deadline keeps the head fresh (e.g. it repeatedly skips to a newly
                // arrived keyframe), it never sees a stale head to evict, and because `decode()` is
                // never called the `decode()`-error -> `reset_pipeline()` path never fires either —
                // playout would freeze on the last-good frame indefinitely. Once the gate has held a
                // releasable frame continuously past MAX_BACKPRESSURE_HOLD_MS, force recovery.
                if held_for_ms > MAX_BACKPRESSURE_HOLD_MS {
                    // Escape path. First force-release the head decodable frame, bypassing the gate
                    // once: this unblocks a merely-slow decoder cheaply, and if the forced
                    // `decode()` errors (a decoder that rejects frames) the existing
                    // `worker_decoder.rs` error path runs `reset_pipeline()`. If the decoder is hard
                    // wedged (accepts the chunk but never drains, never errors), force-release alone
                    // cannot help, so we ALSO escalate to `Decodable::reset()` to tear the pipeline
                    // down. `escape_release_then_reset` clears the hold clock via the release, so the
                    // escape does not re-fire every tick while recovery completes.
                    log::warn!(
                        "[JB_POLL] Backpressure held a releasable frame for {held_for_ms:.0}ms (> {MAX_BACKPRESSURE_HOLD_MS:.0}ms) with the decode queue pinned >= {DECODE_QUEUE_HIGH_WATER_MARK}; decoder appears wedged. Force-releasing the head frame and resetting the decoder pipeline (issue #1324)."
                    );
                    // The escape force-releases the head frame and escalates to a decoder reset, all
                    // independent of the normal gated release path. Break afterwards: a reset has
                    // been requested (deferred via setTimeout(0) in the WebCodecs impl), so there is
                    // nothing more to do this tick — the next tick re-evaluates from the reset state.
                    self.escape_release_then_reset(&mut frames_were_moved);
                    break;
                }
                log::trace!(
                    "[JB_POLL] Decode-queue backpressure: holding release (queue depth >= {DECODE_QUEUE_HIGH_WATER_MARK}, held {held_for_ms:.0}ms)."
                );
                break;
            }

            // The gate is NOT holding this tick (decode queue below the high-water mark), so the
            // continuous-hold clock must not run. Clear it; if release stalls again later it will
            // restart from that new hold. (Release also clears it, but a frame held only by the
            // playout-delay lower bound — not the gate — must reset it too.)
            self.backpressure_hold_since_ms = None;

            if let Some(key) = next_decodable_key {
                if let Some(frame) = self.buffered_frames.get(&key) {
                    let time_in_buffer_ms = (current_time_ms - frame.arrival_time_ms) as f64;

                    let is_ready = time_in_buffer_ms >= self.target_playout_delay_ms;
                    log::trace!(
                        "[JB_POLL] Candidate {key}: Time in buffer: {time_in_buffer_ms:.2}ms, Target: {:.2}ms -> Ready: {is_ready}",
                        self.target_playout_delay_ms
                    );

                    if is_ready {
                        self.release_frame(key);
                        frames_were_moved = true;
                        found_frame_to_move = true;
                    }
                }
            } else {
                log::trace!("[JB_POLL] No decodable frame found in buffer.");
            }

            if !found_frame_to_move {
                break;
            }
        }

        if frames_were_moved {
            // NOTE: No need to notify a condvar anymore. The decoder manages its own thread.
        }
    }

    /// Identifies the frame the decoder could release next, without removing it: the next
    /// continuous sequence number (CASE 1), or, across a gap (CASE 2) or before the first decode
    /// (CASE 3), the next/first buffered keyframe. Returns `None` when nothing is releasable yet
    /// (empty buffer, or waiting for a keyframe).
    fn next_decodable_key(&self) -> Option<u64> {
        if let Some(last_seq) = self.last_decoded_sequence_number {
            // CASE 1: We are in a continuous stream. Look for the next frame.
            let next_continuous_seq = last_seq + 1;
            if self.buffered_frames.contains_key(&next_continuous_seq) {
                log::trace!("[JB_POLL] Seeking next continuous frame: {next_continuous_seq}");
                Some(next_continuous_seq)
            } else {
                // CASE 2: Gap detected. Look for the next keyframe after the gap.
                let keyframe = self
                    .buffered_frames
                    .iter()
                    .find(|(&s, f)| s > next_continuous_seq && f.is_keyframe())
                    .map(|(&s, _)| s);
                if let Some(k) = keyframe {
                    log::trace!(
                        "[JB_POLL] Gap after {last_seq}. Seeking next keyframe. Found: {k}"
                    );
                } else {
                    log::trace!("[JB_POLL] Gap after {last_seq}. No subsequent keyframe found.");
                }
                keyframe
            }
        } else {
            // CASE 3: We have never decoded. We MUST start with a keyframe.
            let keyframe = self
                .buffered_frames
                .iter()
                .find(|(_, f)| f.is_keyframe())
                .map(|(&s, _)| s);
            if let Some(k) = keyframe {
                log::trace!("[JB_POLL] Seeking first keyframe. Found: {k}");
            } else {
                log::trace!("[JB_POLL] Seeking first keyframe. None found in buffer.");
            }
            keyframe
        }
    }

    /// Releases the buffered frame `key` to the decoder: handles keyframe gap/restart recovery
    /// (dropping pre-keyframe frames), pushes to the decoder, and advances
    /// `last_decoded_sequence_number`. Shared by the normal release path and the held-too-long
    /// escape hatch so both have identical release semantics.
    fn release_frame(&mut self, key: u64) {
        let frame_to_move = self
            .buffered_frames
            .remove(&key)
            .expect("release_frame called with a key not present in the buffer");

        // If we're jumping to a keyframe to recover, drop everything before it.
        if frame_to_move.is_keyframe() {
            let is_first_frame = self.last_decoded_sequence_number.is_none();
            let is_gap_recovery = self
                .last_decoded_sequence_number
                .is_some_and(|last_seq| key > last_seq + 1);

            if is_first_frame || is_gap_recovery {
                log::debug!("[JITTER_BUFFER] Keyframe {key} recovery. Dropping frames before it.");
                self.drop_frames_before(key);
            }
        }

        self.push_to_decoder(frame_to_move);
        self.last_decoded_sequence_number = Some(key);
    }

    /// Held-too-long backpressure escape hatch (issue #1324). Called once when the gate has held
    /// release past `MAX_BACKPRESSURE_HOLD_MS` with a frame waiting and the decode queue pinned at
    /// the high-water mark. Force-releases the head decodable frame (bypassing the gate and the
    /// playout-delay check — we are recovering, not pacing) and escalates to `Decodable::reset()`
    /// to tear down a hard-wedged decoder that would accept the chunk but never drain.
    ///
    /// Ordering note: `reset()` on the WebCodecs decoder defers its jitter-buffer reset via
    /// `setTimeout(0)`, so it runs *after* this call stack unwinds. The force-released frame is the
    /// last frame fed to the doomed decoder instance; whether it drains or not, the pipeline reset
    /// then starts a clean session that resumes from the next keyframe.
    fn escape_release_then_reset(&mut self, frames_were_moved: &mut bool) {
        if let Some(key) = self.next_decodable_key() {
            // Force-release the head decodable frame, bypassing both the gate and the playout-delay
            // readiness check. `release_frame` -> `push_to_decoder` clears the continuous-hold clock
            // so the escape does not re-fire next tick.
            self.release_frame(key);
            *frames_were_moved = true;
        }
        // Escalate: reset the decoder pipeline for the hard-wedge case where the forced decode
        // neither drains the queue nor errors. Default no-op for native/mock decoders.
        self.decoder.reset();
    }

    /// Freshness-deadline enforcement (issue #1020).
    ///
    /// Returns `true` if the head-of-line backlog is stale and the buffer state was advanced so
    /// the caller should re-evaluate from the top of its release loop.
    ///
    /// "Stale" means the oldest frame the decoder is actually waiting on (the next frame it could
    /// release — either the next continuous sequence number, or, across a gap, the next keyframe)
    /// has been sitting in the buffer longer than `MAX_PLAYOUT_AGE_MS`. When that happens we are
    /// no longer absorbing jitter; we are accumulating permanent lag. The fix is to discard the
    /// stale backlog and resume from the freshest *self-contained* point.
    ///
    /// Critical correctness rule: we may only skip to a `KeyFrame`. Skipping to an arbitrary delta
    /// would feed the WebCodecs decoder a frame whose reference is gone, producing
    /// `DataError: A key frame is required` and bouncing the worker through `reset_pipeline()`
    /// (a visible stall). So:
    /// - If a buffered keyframe exists, drop everything before the *newest* such keyframe and let
    ///   the normal release path pick it up — this is the skip-to-live.
    /// - If NO keyframe is buffered, we must NOT drop to a delta. We evict the stale delta backlog
    ///   so the buffer cannot keep growing, leave the last-good frame on screen, and rely on the
    ///   existing PLI / keyframe-request recovery path (driven from the client when it observes the
    ///   gap) to fetch a fresh keyframe. See TODO(#1020) below re: triggering a PLI from here.
    fn enforce_freshness_deadline(&mut self, current_time_ms: u128) -> bool {
        // Identify the frame the decoder is currently waiting to release.
        let head_key = match self.last_decoded_sequence_number {
            Some(last_seq) => {
                let next_continuous_seq = last_seq + 1;
                if self.buffered_frames.contains_key(&next_continuous_seq) {
                    Some(next_continuous_seq)
                } else {
                    // Gap: the decoder is waiting on the next keyframe after the gap. The
                    // head-of-line wait is measured from the oldest buffered frame, which is what
                    // actually pins playout.
                    self.buffered_frames.keys().next().copied()
                }
            }
            // Never decoded yet: head of line is whatever is oldest in the buffer.
            None => self.buffered_frames.keys().next().copied(),
        };

        let Some(head_key) = head_key else {
            return false; // Empty buffer — nothing can be stale.
        };

        let head_age_ms = {
            let frame = match self.buffered_frames.get(&head_key) {
                Some(f) => f,
                None => return false,
            };
            current_time_ms.saturating_sub(frame.arrival_time_ms) as f64
        };

        if head_age_ms < MAX_PLAYOUT_AGE_MS {
            // Within freshness bounds — normal jitter handling is byte-for-byte unaffected.
            return false;
        }

        // Backlog is stale. Determine whether ANY buffered keyframe exists. We must keep this
        // presence check SEPARATE from the drop result: the shared
        // `skip_to_newest_buffered_keyframe` helper collapses "no keyframe found" and "keyframe
        // found but already the head (nothing dropped)" into a single `None`, but the original
        // deadline semantics treat those two cases very differently — only the genuinely
        // keyframe-LESS case may evict deltas + fire a proactive keyframe request. Routing the
        // helper's `None` blindly into the keyframe-less branch would (a) evict a keyframe that
        // sits at the head and (b) spuriously fire a PLI. So we branch on `has_keyframe` first.
        let has_keyframe = self.buffered_frames.values().any(|f| f.is_keyframe());

        if has_keyframe {
            // Drop the stale delta/keyframe backlog before the NEWEST keyframe so we land as close
            // to live as possible while resuming from a self-contained frame. The keyframe itself
            // is preserved and released by the normal gate on the next loop iteration; because we
            // resume from a keyframe, `find_and_move_continuous_frames` treats it as gap/restart
            // recovery and decoding continues cleanly.
            //
            // CRITICAL termination guard: the helper returns `None` when the chosen keyframe is
            // ALREADY the head (nothing before it to drop). If we returned `true` unconditionally
            // in that case, the caller would `continue`, re-enter with an unchanged buffer, and
            // spin forever (the 10ms worker tick → tab freeze). This is reachable: e.g.
            // background-tab throttling clamps/pauses `setInterval`, so on refocus the first tick's
            // `Date::now()` delta is multi-second and a buffered post-gap keyframe is instantly
            // ≥ MAX_PLAYOUT_AGE_MS old while sitting at the head. So we only report progress
            // (return `true`) when we actually shrank the buffer. When 0 were dropped, return
            // `false` and let the normal CASE-2 (gap) / CASE-3 (never-decoded) path select this
            // head keyframe and release it through the normal gate — which advances
            // `last_decoded_sequence_number` correctly. We must NOT fall through to the
            // keyframe-less eviction/PLI branch here: a keyframe genuinely exists.
            match self.skip_to_newest_buffered_keyframe() {
                Some((keyframe_seq, dropped)) => {
                    log::debug!(
                        "[JITTER_BUFFER] Freshness deadline exceeded (head age {head_age_ms:.0}ms >= {MAX_PLAYOUT_AGE_MS:.0}ms). Skipped to live keyframe {keyframe_seq}, dropped {dropped} stale frame(s)."
                    );
                    // Surface the skip for field-log visibility (#1045); the worker forwards it.
                    self.record_freshness_skip(
                        current_time_ms,
                        FreshnessSkip {
                            head_age_ms,
                            keyframe_seq: Some(keyframe_seq),
                            dropped,
                        },
                    );
                    true
                }
                // Keyframe is already the head, nothing before it to drop — fall through and let
                // the normal release gate pick it up. Do NOT evict / fire a PLI.
                None => false,
            }
        } else {
            // No keyframe to skip to. We must NOT drop to a delta (that throws
            // `DataError: A key frame is required` -> reset_pipeline stall). Evict the stale
            // delta backlog so the buffer cannot keep growing while we wait, then hold the
            // last-good frame on screen and rely on the existing keyframe-request recovery.
            //
            // We keep only frames newer than the (now-evicted) stale head so a subsequently
            // arriving keyframe can still be matched, but we do not advance playout.
            let stale_cutoff = head_key + 1;
            let dropped_before = self.dropped_frames_count;
            self.drop_frames_before(stale_cutoff);
            let dropped_any = self.dropped_frames_count > dropped_before;
            if dropped_any {
                let dropped = self.dropped_frames_count - dropped_before;
                log::debug!(
                    "[JITTER_BUFFER] Freshness deadline exceeded (head age {head_age_ms:.0}ms) with NO buffered keyframe. Evicted {dropped} stale delta frame(s); holding last-good frame and proactively requesting a keyframe (#1025)."
                );
                // Surface the skip for field-log visibility (#1045); the worker forwards it.
                // `keyframe_seq: None` marks the keyframe-less (held last-good) case.
                self.record_freshness_skip(
                    current_time_ms,
                    FreshnessSkip {
                        head_age_ms,
                        keyframe_seq: None,
                        dropped,
                    },
                );
                // Issue #1025 (resolves the TODO(#1020) here): proactively ask the client to
                // request a keyframe when we evict a stale keyframe-less backlog, rather than
                // waiting for the client's reactive gap-driven request to notice. There is no
                // buffered keyframe to skip to, so playout is frozen on the last-good frame
                // until a fresh keyframe arrives — fetching one sooner directly shortens that
                // freeze.
                //
                // Source-side throttle + backoff: a *sustained or recurring* keyframe-less stall
                // evicts on ~every ~10ms tick (the head delta ages past the deadline as fast as
                // deltas arrive), so firing per-eviction would spray this already-struggling
                // receiver's uplink + worker→main bus at ~10–30/s. The relay limiter (#979/#1011)
                // coalesces for the publisher but not for our uplink, so we gate at the source.
                //
                // First, reset the backoff exponent if the stream has been quiet (no proactive
                // request) for longer than PROACTIVE_KEYFRAME_REQUEST_BACKOFF_RESET_MS — a genuine
                // sustained recovery. The reset is keyed to *request quiet time*, NOT to frame
                // release: the #1479 storm is many short (1–5s) freezes interleaved with smooth
                // playout, so a release-driven reset would zero the exponent during each smooth
                // gap and the backoff would never accumulate across the storm. Keying it to a
                // quiet interval well above the capped 8s cadence means the exponent persists
                // across closely-spaced re-freezes (damping the storm) yet still re-arms a truly
                // recovered stream for the next independent stall.
                if let Some(last) = self.last_keyframe_request_ms {
                    if (current_time_ms.saturating_sub(last)) as f64
                        >= PROACTIVE_KEYFRAME_REQUEST_BACKOFF_RESET_MS
                    {
                        self.consecutive_proactive_keyframe_requests = 0;
                    }
                }

                // The FIRST request fires after PROACTIVE_KEYFRAME_REQUEST_MIN_INTERVAL_MS
                // (≈ the relay window). Each subsequent request within the backoff window then
                // doubles the interval — exponential backoff capped at
                // PROACTIVE_KEYFRAME_REQUEST_MAX_BACKOFF_EXP (issue #1479). A fixed ~1/s cadence
                // across a meeting-long sequence of freezes is precisely the receiver-side
                // amplifier of the PLI keyframe storm: every receiver pokes the active speaker
                // every second, and the relay's delivery-unaware limiter then delays the very
                // recovery keyframe those pokes are asking for. Backing off decays the cadence
                // toward ~1/8s while the stall persists, then the quiet-interval reset above
                // restores full speed once the stream genuinely recovers.
                let backoff_exp = self
                    .consecutive_proactive_keyframe_requests
                    .min(PROACTIVE_KEYFRAME_REQUEST_MAX_BACKOFF_EXP);
                let required_interval_ms =
                    PROACTIVE_KEYFRAME_REQUEST_MIN_INTERVAL_MS * (2u32.pow(backoff_exp) as f64);
                let should_request = match self.last_keyframe_request_ms {
                    Some(last) => {
                        (current_time_ms.saturating_sub(last)) as f64 >= required_interval_ms
                    }
                    None => true,
                };
                if should_request {
                    self.last_keyframe_request_ms = Some(current_time_ms);
                    self.consecutive_proactive_keyframe_requests = self
                        .consecutive_proactive_keyframe_requests
                        .saturating_add(1);
                    (self.request_keyframe)();
                }
            }
            false
        }
    }

    /// Skip to the newest buffered keyframe, dropping every frame before it.
    ///
    /// Returns `Some((keyframe_seq, dropped_count))` when a keyframe was found AND at least one
    /// frame was dropped; `None` when there is no buffered keyframe OR the keyframe is already the
    /// head (nothing before it to drop). The caller is responsible for any logging / diagnostics /
    /// counters — this helper only mutates the buffer and `dropped_frames_count`.
    ///
    /// This is the single source of truth for "resync to live": BOTH the freshness deadline
    /// (issue #1020) and the resync governor (issue #1252) call it, so the newest-keyframe
    /// selection and drop logic cannot diverge between the two trigger paths. The keyframe itself
    /// is preserved and released by the normal gate (gap/restart recovery), so a skip can never
    /// feed a bare delta to the decoder.
    fn skip_to_newest_buffered_keyframe(&mut self) -> Option<(u64, u64)> {
        let newest_keyframe = self
            .buffered_frames
            .iter()
            .rev()
            .find(|(_, f)| f.is_keyframe())
            .map(|(&s, _)| s)?;
        let dropped_before = self.dropped_frames_count;
        self.drop_frames_before(newest_keyframe);
        let dropped = self.dropped_frames_count - dropped_before;
        if dropped > 0 {
            Some((newest_keyframe, dropped))
        } else {
            None
        }
    }

    /// v1 resync-to-live governor (issue #1252). Evaluated once per 10ms tick. When the buffered
    /// playout-latency total stays `>= GOVERNOR_ENGAGE_MS` for `GOVERNOR_SUSTAIN_TICKS` consecutive
    /// ticks AND we are past `GOVERNOR_COOLDOWN_MS` since the last governor skip, skip to the newest
    /// buffered keyframe (reusing the same helper the freshness deadline uses, so they cannot
    /// diverge). If there is NO buffered keyframe, the governor does NOTHING — no eviction, NO PLI,
    /// no keyframe request. That case is left entirely to the existing 1800ms freshness deadline.
    /// This is the #1287/#1297/#814 storm-avoidance invariant: v1 introduces ZERO new
    /// keyframe-request sources. Thrash-prevention/hysteresis comes from TWO guards that DO gate
    /// the skip: (1) `governor_sustain_ticks` resets to 0 after every skip, so the latency must
    /// re-accumulate `>= GOVERNOR_ENGAGE_MS` for `GOVERNOR_SUSTAIN_TICKS` again; (2)
    /// `GOVERNOR_COOLDOWN_MS` since the last skip. The `governor_engaged` flag / `GOVERNOR_DISENGAGE_MS`
    /// only track the engaged episode for observability + lifecycle — they are NOT read by the skip
    /// decision (see the `governor_engaged` field doc for why a latch-gate would deadlock).
    fn run_resync_governor(&mut self, current_time_ms: u128) {
        let total = self.playout_latency_parts_ms(current_time_ms).0;

        // Hysteresis disengage.
        if total < GOVERNOR_DISENGAGE_MS {
            self.governor_engaged = false;
        }

        // Sustain counter.
        if total >= GOVERNOR_ENGAGE_MS {
            self.governor_sustain_ticks = self.governor_sustain_ticks.saturating_add(1);
        } else {
            self.governor_sustain_ticks = 0;
        }

        if self.governor_sustain_ticks < GOVERNOR_SUSTAIN_TICKS {
            return;
        }

        // Cooldown: do not skip again within GOVERNOR_COOLDOWN_MS of the last governor skip.
        if let Some(last) = self.governor_last_skip_ms {
            if ((current_time_ms.saturating_sub(last)) as f64) < GOVERNOR_COOLDOWN_MS {
                return;
            }
        }

        // Engage: skip to the newest buffered keyframe ONLY. No keyframe -> do nothing (no PLI).
        // On a no-keyframe engage condition we intentionally do NOT reset the sustain counter or
        // set `last_skip`, so the governor keeps trying each tick until either a keyframe appears
        // or the 1800ms deadline fires — the deadline owns the keyframe-less case.
        if let Some((keyframe_seq, dropped)) = self.skip_to_newest_buffered_keyframe() {
            self.governor_engaged = true;
            self.governor_last_skip_ms = Some(current_time_ms);
            self.governor_skips += 1;
            self.governor_sustain_ticks = 0; // reset so we re-accumulate before another skip
            log::debug!(
                "[JITTER_BUFFER] Resync governor engaged (playout latency {total:.0}ms >= {GOVERNOR_ENGAGE_MS:.0}ms sustained). Skipped to newest buffered keyframe {keyframe_seq}, dropped {dropped} frame(s). (issue #1252)"
            );
        }
        // else: no buffered keyframe — governor takes NO action; the 1800ms deadline remains the
        // backstop. We deliberately do NOT set `last_skip_ms` here, so while sustain stays high and
        // no keyframe is buffered the governor re-attempts the (bounded) newest-keyframe scan every
        // tick until a keyframe arrives or the deadline fires. That scan is O(buffered_frames),
        // hard-bounded by `MAX_BUFFER_SIZE` (200) and time-bounded by the 1800ms deadline that owns
        // the keyframe-less case, so the worst case is a few hundred trivial comparisons per tick
        // for at most ~the engage→deadline gap — negligible on the low-power target.
    }

    /// Pushes a single frame to the shared decodable queue.
    ///
    /// This is the single release point, so it also clears the backpressure continuous-hold clock
    /// (`backpressure_hold_since_ms`): a successful release means the gate is NOT currently the
    /// thing holding playout back, so the held-too-long escape-hatch elapsed measurement (issue
    /// #1324) restarts from the next hold. A later transient hold therefore cannot inherit stale
    /// elapsed time from before this release.
    fn push_to_decoder(&mut self, frame: FrameBuffer) {
        let seq = frame.sequence_number();
        // Record source cadence BEFORE the frame is moved into the decoder (issue #1252).
        self.record_release_cadence(frame.arrival_time_ms);
        log::trace!("[JITTER_BUFFER] Pushing frame {seq} to decoder.");
        self.decoder.decode(frame);
        self.backpressure_hold_since_ms = None;
    }

    /// Folds a released frame's arrival time into the rolling source frame-interval estimate
    /// (issue #1252). The estimate must track the SOURCE cadence (~30fps), so it is derived from
    /// the inter-arrival spacing of consecutive *released* frames (preserved in their
    /// `arrival_time_ms`), never the decoder's drain rate. Implausible deltas — 0/negative from
    /// burst-or-reordered arrivals, or multi-second post-stall gaps — are discarded rather than
    /// folded, so the estimate stays anchored to real source cadence.
    fn record_release_cadence(&mut self, arrival_time_ms: u128) {
        if let Some(prev) = self.last_released_arrival_time_ms {
            let delta = arrival_time_ms.saturating_sub(prev) as f64;
            if (MIN_SOURCE_FRAME_INTERVAL_MS..=MAX_SOURCE_FRAME_INTERVAL_MS).contains(&delta) {
                self.source_frame_interval_ms = self.source_frame_interval_ms
                    * (1.0 - SOURCE_FRAME_INTERVAL_EWMA_ALPHA)
                    + delta * SOURCE_FRAME_INTERVAL_EWMA_ALPHA;
            }
        }
        self.last_released_arrival_time_ms = Some(arrival_time_ms);
    }

    /// Checks if the jitter buffer is currently waiting for a keyframe to continue.
    pub fn is_waiting_for_keyframe(&self) -> bool {
        self.last_decoded_sequence_number.is_none()
    }

    /// Removes all frames from the buffer with a sequence number less than the given one.
    fn drop_frames_before(&mut self, sequence_number: u64) {
        let keys_to_drop: Vec<u64> = self
            .buffered_frames
            .keys()
            .cloned()
            .filter(|&k| k < sequence_number)
            .collect();

        self.dropped_frames_count += keys_to_drop.len() as u64;
        for key in keys_to_drop {
            log::trace!("[JITTER_BUFFER] Dropping stale frame: {key}");
            self.buffered_frames.remove(&key);
        }
    }

    /// Removes all frames from the buffer. Used when a keyframe arrives and the buffer is full.
    pub fn drop_all_frames(&mut self) {
        let num_dropped = self.buffered_frames.len() as u64;
        self.buffered_frames.clear();
        self.dropped_frames_count += num_dropped;
        log::debug!("[JITTER_BUFFER] Dropped all {num_dropped} frames.");
    }

    /// Flushes the jitter buffer, resetting its state completely.
    pub fn flush(&mut self) {
        self.drop_all_frames();
        self.last_decoded_sequence_number = None;
        self.num_consecutive_old_frames = 0;
        // Reset the backpressure continuous-hold clock (issue #1324) so a later transient hold
        // starts its escape-hatch timer fresh and cannot inherit stale elapsed time from before the
        // flush.
        self.backpressure_hold_since_ms = None;
        // Reset the release-cadence anchor so the first post-flush release does not measure a delta
        // across the flush gap (issue #1252). The smoothed interval estimate itself persists — the
        // source cadence does not change across a flush.
        self.last_released_arrival_time_ms = None;
        // Reset the proactive keyframe-request throttle (issue #1025) so a fresh stream after a
        // flush (e.g. stream restart) can request a recovery keyframe immediately rather than
        // inheriting the pre-flush cooldown. Reset the backoff exponent too (issue #1479) so the
        // post-flush stream is not handicapped by a prior stall's accumulated backoff.
        self.last_keyframe_request_ms = None;
        self.consecutive_proactive_keyframe_requests = 0;
        // Reset freshness-skip diagnostic throttling too: a post-flush stream should not inherit
        // stale diagnostic cooldown or coalesced skip details from before the stream reset.
        self.last_freshness_skip = None;
        self.last_freshness_skip_emit_ms = None;
        self.pending_freshness_skip = None;
        // Reset resync-governor runtime state (issue #1252) so a fresh post-flush stream
        // re-accumulates from scratch. The lifetime `governor_skips` counter is NOT reset — it is a
        // cumulative metric.
        self.governor_sustain_ticks = 0;
        self.governor_engaged = false;
        self.governor_last_skip_ms = None;
        // Consider resetting jitter estimator as well if needed
        self.jitter_estimator = JitterEstimator::new();
    }

    pub fn get_jitter_estimate_ms(&self) -> f64 {
        self.jitter_estimator.get_jitter_estimate_ms()
    }

    pub fn get_target_playout_delay_ms(&self) -> f64 {
        self.target_playout_delay_ms
    }

    pub fn get_dropped_frames_count(&self) -> u64 {
        self.dropped_frames_count
    }

    /// Stage-1 backlog span in ms (issue #1252): the arrival-time gap between the newest buffered
    /// frame and the next frame the decoder is waiting to release. This is how far behind live the
    /// jitter-buffer playout point sits — the dominant term of the playout-latency metric, and the
    /// one that captures the 5–6s lag #1024 may merely relocate (it is timestamp-derived, not
    /// depth-derived). Read-only: never mutates the buffer or any counter.
    ///
    /// Returns 0 when fewer than two frames are buffered or nothing is releasable. `now_ms` is the
    /// current wall-clock time; it bounds the span to the head-of-line age so a newest frame with a
    /// bad/rolled-back arrival timestamp (which cannot truly be in the future) can't inflate it.
    pub fn buffered_span_ms(&self, now_ms: u128) -> f64 {
        // Identify the next-to-release (head-of-line) frame — same selection as
        // `enforce_freshness_deadline`: the next continuous frame if buffered, else the oldest
        // buffered frame (gap / never-decoded).
        let head_key = match self.last_decoded_sequence_number {
            Some(last_seq) => {
                let next_continuous_seq = last_seq + 1;
                if self.buffered_frames.contains_key(&next_continuous_seq) {
                    Some(next_continuous_seq)
                } else {
                    self.buffered_frames.keys().next().copied()
                }
            }
            None => self.buffered_frames.keys().next().copied(),
        };

        let Some(head_key) = head_key else {
            return 0.0;
        };
        let (Some(head), Some(newest)) = (
            self.buffered_frames.get(&head_key),
            self.buffered_frames.values().next_back(),
        ) else {
            return 0.0;
        };

        let span = newest.arrival_time_ms.saturating_sub(head.arrival_time_ms) as f64;
        let head_age = now_ms.saturating_sub(head.arrival_time_ms) as f64;
        span.min(head_age)
    }

    /// Rolling estimate of the source frame interval in ms (~33ms at 30fps), derived from released-
    /// frame inter-arrival deltas (source cadence), NOT the decoder drain rate. See
    /// [`record_release_cadence`](Self::record_release_cadence).
    pub fn source_frame_interval_ms(&self) -> f64 {
        self.source_frame_interval_ms
    }

    /// Return the total playout-latency estimate and its stage-1 jitter-buffer span.
    /// Computes the stage-1 span once so emitters can publish both values without
    /// repeating the buffer walk.
    pub fn playout_latency_parts_ms(&self, now_ms: u128) -> (f64, f64) {
        let stage1_ms = self.buffered_span_ms(now_ms);
        let stage2_ms = self.decoder.decode_queue_depth() as f64 * self.source_frame_interval_ms;
        (stage1_ms + stage2_ms, stage1_ms)
    }

    /// Total buffered video playout latency in ms (issue #1252). Spans BOTH receive pipeline
    /// stages: the stage-1 jitter-buffer backlog span ([`buffered_span_ms`](Self::buffered_span_ms))
    /// plus the stage-2 WebCodecs decoder queue (bounded by #1024's high-water mark), the latter
    /// valued at one source frame interval per queued frame. Read-only: never mutates state.
    pub fn playout_latency_ms(&self, now_ms: u128) -> f64 {
        self.playout_latency_parts_ms(now_ms).0
    }

    /// Lifetime count of resync-governor skips (issue #1252). Exposed for the Prometheus metric
    /// wired in a follow-up commit; not reset by flush().
    pub fn governor_skip_count(&self) -> u64 {
        self.governor_skips
    }
}

/// Stage-3 paint lag in ms (issue #1252): the time-valued backlog of decoded-but-unpainted frames
/// sitting in the worker→main `postMessage` queue + the main-thread paint task queue — a region
/// that `decode_queue_size()` (stage 2) cannot observe.
///
/// Computed as `(frames_emitted − frames_painted) × source_frame_interval_ms`, where
/// `frames_emitted` is held un-delayed in the worker and `frames_painted` is the main thread's
/// most recent ACK. `saturating_sub` floors the frame count at 0 so a transient ACK overshoot
/// (e.g. just after a reset, while old in-flight frames are still being painted) reads as "at
/// live" rather than underflowing.
///
/// Pure and side-effect free so the arithmetic can be unit-tested off the wasm-only worker path.
pub fn paint_lag_ms(
    frames_emitted: u64,
    frames_painted: u64,
    source_frame_interval_ms: f64,
) -> f64 {
    let outstanding = frames_emitted.saturating_sub(frames_painted);
    outstanding as f64 * source_frame_interval_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::DecodedFrame;
    use crate::frame::{FrameType, VideoFrame};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex;

    /// A mock decoder for testing purposes. It stores decoded frames in a shared Vec, and exposes a
    /// shared, test-controllable `queue_depth` so tests can simulate a backed-up WebCodecs decoder
    /// and exercise the release-side backpressure gate (issue #1024). Depth defaults to 0, so every
    /// pre-existing test sees no backpressure. A shared `reset_count` makes the wedged-decoder
    /// escape-hatch reset (issue #1324) observable.
    struct MockDecoder {
        decoded_frames: Arc<Mutex<Vec<DecodedFrame>>>,
        queue_depth: Arc<AtomicU32>,
        reset_count: Arc<AtomicU32>,
    }

    // This impl is for native targets
    #[cfg(not(target_arch = "wasm32"))]
    impl Decodable for MockDecoder {
        /// The decoded frame type for mock decoder in tests.
        type Frame = crate::decoder::DecodedFrame;
        fn new(
            _codec: crate::decoder::VideoCodec,
            _on_decoded_frame: Box<dyn Fn(DecodedFrame) + Send + Sync>,
        ) -> Self {
            panic!("Use `new_with_vec` for this mock.");
        }
        fn decode(&self, frame: FrameBuffer) {
            let mut frames = self.decoded_frames.lock().unwrap();
            frames.push(DecodedFrame {
                sequence_number: frame.sequence_number(),
                width: 0,
                height: 0,
                data: frame.frame.data.to_vec(),
            });
        }
        fn decode_queue_depth(&self) -> u32 {
            self.queue_depth.load(Ordering::SeqCst)
        }
        fn reset(&self) {
            self.reset_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    // This impl is for wasm targets
    #[cfg(target_arch = "wasm32")]
    impl Decodable for MockDecoder {
        /// The decoded frame type for mock decoder in tests.
        type Frame = crate::decoder::DecodedFrame;
        fn new(
            _codec: crate::decoder::VideoCodec,
            _on_decoded_frame: Box<dyn Fn(DecodedFrame)>,
        ) -> Self {
            panic!("Use `new_with_vec` for this mock.");
        }
        fn decode(&self, frame: FrameBuffer) {
            let mut frames = self.decoded_frames.lock().unwrap();
            frames.push(DecodedFrame {
                sequence_number: frame.sequence_number(),
                width: 0,
                height: 0,
                data: frame.frame.data.to_vec(),
            });
        }
        fn decode_queue_depth(&self) -> u32 {
            self.queue_depth.load(Ordering::SeqCst)
        }
        fn reset(&self) {
            self.reset_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl MockDecoder {
        fn new_with_vec(decoded_frames: Arc<Mutex<Vec<DecodedFrame>>>) -> Self {
            Self {
                decoded_frames,
                queue_depth: Arc::new(AtomicU32::new(0)),
                reset_count: Arc::new(AtomicU32::new(0)),
            }
        }
        fn new_with_vec_and_depth(
            decoded_frames: Arc<Mutex<Vec<DecodedFrame>>>,
            queue_depth: Arc<AtomicU32>,
            reset_count: Arc<AtomicU32>,
        ) -> Self {
            Self {
                decoded_frames,
                queue_depth,
                reset_count,
            }
        }
    }

    /// A helper to create a JitterBuffer with a mock decoder for testing.
    fn create_test_jitter_buffer() -> (
        JitterBuffer<crate::decoder::DecodedFrame>,
        Arc<Mutex<Vec<DecodedFrame>>>,
    ) {
        let decoded_frames = Arc::new(Mutex::new(Vec::new()));
        let mock_decoder = Box::new(MockDecoder::new_with_vec(decoded_frames.clone()));
        let jitter_buffer = JitterBuffer::new(mock_decoder);
        (jitter_buffer, decoded_frames)
    }

    /// Like `create_test_jitter_buffer`, but also returns a handle to the mock decoder's simulated
    /// WebCodecs queue depth so a test can drive release-side backpressure (issue #1024).
    fn create_test_jitter_buffer_with_queue_depth() -> (
        JitterBuffer<crate::decoder::DecodedFrame>,
        Arc<Mutex<Vec<DecodedFrame>>>,
        Arc<AtomicU32>,
    ) {
        let (jb, frames, depth, _reset) = create_test_jitter_buffer_with_queue_and_reset();
        (jb, frames, depth)
    }

    /// Return type of `create_test_jitter_buffer_with_queue_and_reset`: the jitter buffer plus
    /// shared handles to (decoded frames, simulated queue depth, reset count).
    type JbWithQueueAndReset = (
        JitterBuffer<crate::decoder::DecodedFrame>,
        Arc<Mutex<Vec<DecodedFrame>>>,
        Arc<AtomicU32>,
        Arc<AtomicU32>,
    );

    /// Like `create_test_jitter_buffer_with_queue_depth`, but also returns a handle to the mock
    /// decoder's reset counter so a test can observe the wedged-decoder escape-hatch reset
    /// (issue #1324).
    fn create_test_jitter_buffer_with_queue_and_reset() -> JbWithQueueAndReset {
        let decoded_frames = Arc::new(Mutex::new(Vec::new()));
        let queue_depth = Arc::new(AtomicU32::new(0));
        let reset_count = Arc::new(AtomicU32::new(0));
        let mock_decoder = Box::new(MockDecoder::new_with_vec_and_depth(
            decoded_frames.clone(),
            queue_depth.clone(),
            reset_count.clone(),
        ));
        let jitter_buffer = JitterBuffer::new(mock_decoder);
        (jitter_buffer, decoded_frames, queue_depth, reset_count)
    }

    fn create_test_frame(seq: u64, frame_type: FrameType) -> VideoFrame {
        VideoFrame {
            sequence_number: seq,
            frame_type,
            codec: crate::frame::FrameCodec::default(),
            data: vec![0; 10],
            timestamp: 0.0,
        }
    }

    #[test]
    fn insert_in_order() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        // Playout delay requires us to simulate time passing.
        let mut time = 1000;

        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), time);
        time += 100; // Elapse time to overcome playout delay
        jb.find_and_move_continuous_frames(time);

        {
            let queue = decoded_frames.lock().unwrap();
            assert_eq!(queue.len(), 1);
            assert_eq!(queue[0].sequence_number, 1);
        }

        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), time);
        time += 100;
        jb.find_and_move_continuous_frames(time);

        let queue = decoded_frames.lock().unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[1].sequence_number, 2);
    }

    #[test]
    fn insert_out_of_order() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let mut time = 1000;

        // Insert 3, then 1, then 2.
        jb.insert_frame(create_test_frame(3, FrameType::DeltaFrame), time);
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), time);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), time);

        // Advance time enough for all frames to pass the playout delay.
        time += 100;
        jb.find_and_move_continuous_frames(time);

        let queue = decoded_frames.lock().unwrap();
        assert_eq!(queue.len(), 3);
        assert_eq!(queue[0].sequence_number, 1);
        assert_eq!(queue[1].sequence_number, 2);
        assert_eq!(queue[2].sequence_number, 3);
    }

    #[test]
    fn keyframe_recovers_from_gap() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let mut time = 1000;

        // Insert 1, then 3 (KeyFrame). Frame 2 is "lost".
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), time);
        time += 100;
        jb.find_and_move_continuous_frames(time); // Frame 1 is moved.

        jb.insert_frame(create_test_frame(3, FrameType::KeyFrame), time);
        time += 100;
        jb.find_and_move_continuous_frames(time); // Frame 3 is moved.

        let queue = decoded_frames.lock().unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].sequence_number, 1);
        assert_eq!(queue[1].sequence_number, 3);
        assert_eq!(jb.last_decoded_sequence_number, Some(3));
    }

    #[test]
    fn stale_frames_are_dropped_on_keyframe() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let mut time = 1000;
        assert_eq!(jb.get_dropped_frames_count(), 0);

        // Insert frames that will become stale.
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), time);
        jb.insert_frame(create_test_frame(3, FrameType::DeltaFrame), time);
        assert!(jb.buffered_frames.contains_key(&2));
        assert!(jb.buffered_frames.contains_key(&3));

        // At this point, nothing is decodable because we haven't seen a keyframe.
        jb.find_and_move_continuous_frames(time);
        assert!(decoded_frames.lock().unwrap().is_empty());

        // Insert a keyframe that jumps over the stale frames.
        jb.insert_frame(create_test_frame(4, FrameType::KeyFrame), time);

        // Advance time to allow the keyframe to be decoded.
        time += 100;
        jb.find_and_move_continuous_frames(time);

        // The keyframe should be ready to decode.
        let queue = decoded_frames.lock().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].sequence_number, 4);

        // The stale frames should be gone from the internal buffer.
        assert!(!jb.buffered_frames.contains_key(&2));
        assert!(!jb.buffered_frames.contains_key(&3));

        // The dropped frame counter should be updated.
        assert_eq!(jb.get_dropped_frames_count(), 2);
    }

    #[test]
    fn old_frames_are_ignored() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let mut time = 1000;

        // Decode sequence 1 and 2
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), time);
        time += 100;
        jb.find_and_move_continuous_frames(time);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), time);
        time += 100;
        jb.find_and_move_continuous_frames(time);

        // At this point, frames 1 and 2 should be in the queue.
        assert_eq!(decoded_frames.lock().unwrap().len(), 2);
        assert_eq!(jb.last_decoded_sequence_number, Some(2));

        // Now, insert an old frame (seq 1) and a current frame (seq 2).
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), time);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), time);

        // No new frames should have been added to the queue.
        assert_eq!(decoded_frames.lock().unwrap().len(), 2);

        // And the internal buffer should be empty.
        assert!(jb.buffered_frames.is_empty());
    }

    #[test]
    fn buffer_capacity_is_enforced() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let time = 1000;

        // Fill the buffer up to its capacity. These frames are not continuous.
        for i in 1..=MAX_BUFFER_SIZE {
            jb.insert_frame(create_test_frame(i as u64 * 2, FrameType::DeltaFrame), time);
        }

        assert_eq!(jb.buffered_frames.len(), MAX_BUFFER_SIZE);

        // Try to insert another delta frame. It should be rejected.
        let next_seq = (MAX_BUFFER_SIZE + 1) as u64 * 2;
        jb.insert_frame(create_test_frame(next_seq, FrameType::DeltaFrame), time);
        assert_eq!(jb.buffered_frames.len(), MAX_BUFFER_SIZE);
        assert!(!jb.buffered_frames.contains_key(&next_seq));

        // No frames should have been moved.
        assert_eq!(decoded_frames.lock().unwrap().len(), 0);

        // Now, insert a keyframe. It should clear the buffer and insert itself.
        let keyframe_seq = (MAX_BUFFER_SIZE + 2) as u64 * 2;
        jb.insert_frame(create_test_frame(keyframe_seq, FrameType::KeyFrame), time);

        assert_eq!(jb.buffered_frames.len(), 1);
        assert!(jb.buffered_frames.contains_key(&keyframe_seq));
        assert_eq!(jb.get_dropped_frames_count(), MAX_BUFFER_SIZE as u64);
    }

    #[test]
    fn playout_delay_holds_frame() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let mut time = 1000;

        // Insert a keyframe. The initial playout delay is MIN_PLAYOUT_DELAY_MS (10ms).
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), time);

        // Advance time, but not enough to meet the delay.
        time += (MIN_PLAYOUT_DELAY_MS / 2.0) as u128;
        jb.find_and_move_continuous_frames(time);

        // The frame should NOT be in the decodable queue yet.
        assert!(decoded_frames.lock().unwrap().is_empty());

        // Advance time past the minimum delay.
        time += (MIN_PLAYOUT_DELAY_MS as u128) + 1;
        jb.find_and_move_continuous_frames(time);

        // NOW the frame should be in the queue.
        let queue = decoded_frames.lock().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].sequence_number, 1);
    }

    #[test]
    fn advances_decodable_frame_on_extraction() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let mut time = 1000;

        // Insert the first frame.
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), time);

        // Advance time to decode it.
        time += 100;
        jb.find_and_move_continuous_frames(time);

        // Verify only frame 1 is in the queue.
        {
            let queue = decoded_frames.lock().unwrap();
            assert_eq!(queue.len(), 1, "Queue should have frame 1");
            assert_eq!(queue[0].sequence_number, 1);
        }

        // Simulate extraction by the decoder by updating our last decoded number
        // and clearing the queue for the next check.
        jb.last_decoded_sequence_number = Some(1);
        decoded_frames.lock().unwrap().clear();

        // Insert the second frame.
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), time);

        // Advance time to decode it.
        time += 100;
        jb.find_and_move_continuous_frames(time);

        // Verify only frame 2 is in the queue.
        {
            let queue = decoded_frames.lock().unwrap();
            assert_eq!(queue.len(), 1, "Queue should have frame 2");
            assert_eq!(queue[0].sequence_number, 2);
        }

        // Simulate extraction of frame 2.
        jb.last_decoded_sequence_number = Some(2);
        decoded_frames.lock().unwrap().clear();

        // Insert the third frame.
        jb.insert_frame(create_test_frame(3, FrameType::DeltaFrame), time);

        // Advance time to decode it.
        time += 100;
        jb.find_and_move_continuous_frames(time);

        // Verify only frame 3 is in the queue.
        {
            let queue = decoded_frames.lock().unwrap();
            assert_eq!(queue.len(), 1, "Queue should have frame 3");
            assert_eq!(queue[0].sequence_number, 3);
        }
    }

    #[test]
    fn complex_reordering_pattern() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let mut time = 1000;

        // Insert odd frames first
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), time);
        jb.insert_frame(create_test_frame(3, FrameType::DeltaFrame), time);
        jb.insert_frame(create_test_frame(5, FrameType::DeltaFrame), time);

        // Then insert even frames
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), time);
        jb.insert_frame(create_test_frame(4, FrameType::DeltaFrame), time);

        // Advance time to allow all to be decoded
        time += 100;
        jb.find_and_move_continuous_frames(time);

        let queue = decoded_frames.lock().unwrap();
        assert_eq!(queue.len(), 5);
        for i in 0..5 {
            assert_eq!(queue[i].sequence_number, (i + 1) as u64);
        }
    }

    #[test]
    fn in_order_keyframe_does_not_disrupt_flow() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let mut time = 1000;

        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), time);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), time);

        time += 100;
        jb.find_and_move_continuous_frames(time);
        assert_eq!(decoded_frames.lock().unwrap().len(), 2);
        assert_eq!(jb.get_dropped_frames_count(), 0);

        // Insert another Keyframe, but it's in order, so no frames should be dropped.
        jb.insert_frame(create_test_frame(3, FrameType::KeyFrame), time);

        time += 100;
        jb.find_and_move_continuous_frames(time);

        let queue = decoded_frames.lock().unwrap();
        assert_eq!(queue.len(), 3, "All three frames should be in the queue");
        assert_eq!(queue[2].sequence_number, 3);
        assert_eq!(
            jb.get_dropped_frames_count(),
            0,
            "No frames should have been dropped"
        );
    }

    #[test]
    fn sequence_starting_at_high_number() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let mut time = 1000;
        let start_seq = 10000;

        // Insert frames starting from a high sequence number
        jb.insert_frame(create_test_frame(start_seq, FrameType::KeyFrame), time);
        jb.insert_frame(
            create_test_frame(start_seq + 2, FrameType::DeltaFrame),
            time,
        );
        jb.insert_frame(
            create_test_frame(start_seq + 1, FrameType::DeltaFrame),
            time,
        );

        // Advance time enough for all frames to pass the playout delay.
        time += 100;
        jb.find_and_move_continuous_frames(time);

        let queue = decoded_frames.lock().unwrap();
        assert_eq!(queue.len(), 3);
        assert_eq!(queue[0].sequence_number, start_seq);
        assert_eq!(queue[1].sequence_number, start_seq + 1);
        assert_eq!(queue[2].sequence_number, start_seq + 2);
    }

    #[test]
    fn flush_on_too_many_consecutive_old_frames() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let mut time = 1000;

        // Decode sequence 1 and 2
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), time);
        time += 100;
        jb.find_and_move_continuous_frames(time);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), time);
        time += 100;
        jb.find_and_move_continuous_frames(time);
        assert_eq!(jb.last_decoded_sequence_number, Some(2));
        assert_eq!(jb.buffered_frames.len(), 0);

        // Insert a frame into the buffer that won't be decoded
        jb.insert_frame(create_test_frame(4, FrameType::DeltaFrame), time);
        assert_eq!(jb.buffered_frames.len(), 1);

        // Send a stream of old packets
        for _ in 0..=MAX_CONSECUTIVE_OLD_FRAMES {
            // Send old frame with sequence number 1
            jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), time);
        }

        // The buffer should now be flushed
        assert_eq!(
            jb.last_decoded_sequence_number, None,
            "Last decoded sequence number should be reset"
        );
        assert_eq!(
            jb.buffered_frames.len(),
            0,
            "Buffer should be empty after flush"
        );
        assert_eq!(
            jb.num_consecutive_old_frames, 0,
            "Consecutive old frames counter should be reset"
        );

        // It should now be waiting for a keyframe again
        assert!(jb.is_waiting_for_keyframe());

        // Verify that even if we send another delta frame, it doesn't get decoded
        jb.insert_frame(create_test_frame(3, FrameType::DeltaFrame), time);
        time += 100;
        jb.find_and_move_continuous_frames(time);
        assert!(decoded_frames.lock().unwrap().len() <= 2); // Should not have increased
    }

    // --- Freshness deadline (issue #1020) ---

    /// A stale head-of-line frame whose age exceeds MAX_PLAYOUT_AGE_MS must NOT be released as-is;
    /// playout must skip to the newest buffered keyframe (never drop to an arbitrary delta).
    #[test]
    fn stale_head_skips_to_keyframe_not_delta() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let start = 1000;

        // Decode an initial keyframe so we are in a "continuous stream" state.
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), start);
        jb.find_and_move_continuous_frames(start + 100);
        assert_eq!(jb.last_decoded_sequence_number, Some(1));
        decoded_frames.lock().unwrap().clear();

        // A network stall: the next continuous frame (2, delta) and a few more deltas arrive, then
        // a FRESH keyframe (5) arrives much later. All the deltas are now ancient.
        let stall_arrival = start + 200;
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), stall_arrival);
        jb.insert_frame(create_test_frame(3, FrameType::DeltaFrame), stall_arrival);
        jb.insert_frame(create_test_frame(4, FrameType::DeltaFrame), stall_arrival);

        // Newest buffered keyframe arrives recently.
        let keyframe_arrival = stall_arrival + (MAX_PLAYOUT_AGE_MS as u128) + 100;
        jb.insert_frame(create_test_frame(5, FrameType::KeyFrame), keyframe_arrival);

        // Poll at a time where the head-of-line delta (seq 2) is older than MAX_PLAYOUT_AGE_MS but
        // the keyframe (seq 5) is fresh.
        let now = keyframe_arrival + 50;
        jb.find_and_move_continuous_frames(now);

        let queue = decoded_frames.lock().unwrap();
        // We must have skipped straight to the keyframe; no stale delta released.
        assert_eq!(queue.len(), 1, "only the fresh keyframe should be decoded");
        assert_eq!(queue[0].sequence_number, 5);
        // The stale deltas (2,3,4) must be gone.
        assert!(!jb.buffered_frames.contains_key(&2));
        assert!(!jb.buffered_frames.contains_key(&3));
        assert!(!jb.buffered_frames.contains_key(&4));
        assert_eq!(jb.last_decoded_sequence_number, Some(5));
        assert_eq!(jb.get_dropped_frames_count(), 3);
    }

    /// Backlog with a newer buffered keyframe: drops the stale deltas before the keyframe and
    /// resumes at the keyframe (skip-to-live).
    #[test]
    fn backlog_with_newer_keyframe_drops_deltas_and_resumes_at_keyframe() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let start = 5000;

        // Never decoded yet. Stale deltas, then a newer keyframe, then a couple deltas after it.
        let stale_arrival = start;
        jb.insert_frame(create_test_frame(10, FrameType::DeltaFrame), stale_arrival);
        jb.insert_frame(create_test_frame(11, FrameType::DeltaFrame), stale_arrival);

        let keyframe_arrival = stale_arrival + (MAX_PLAYOUT_AGE_MS as u128) + 200;
        jb.insert_frame(create_test_frame(12, FrameType::KeyFrame), keyframe_arrival);
        jb.insert_frame(
            create_test_frame(13, FrameType::DeltaFrame),
            keyframe_arrival,
        );

        let now = keyframe_arrival + 100;
        jb.find_and_move_continuous_frames(now);

        let queue = decoded_frames.lock().unwrap();
        // Keyframe 12 then continuous delta 13 should decode; stale 10,11 dropped.
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].sequence_number, 12);
        assert_eq!(queue[1].sequence_number, 13);
        assert!(!jb.buffered_frames.contains_key(&10));
        assert!(!jb.buffered_frames.contains_key(&11));
        assert_eq!(jb.last_decoded_sequence_number, Some(13));
        assert_eq!(jb.get_dropped_frames_count(), 2);
    }

    /// Backlog with NO buffered keyframe must not drop to a delta and must not advance playout.
    /// The stale delta backlog is evicted (so the buffer can't grow unbounded) and the last-good
    /// frame is preserved; recovery is left to the keyframe-request path.
    #[test]
    fn stale_backlog_without_keyframe_does_not_drop_to_delta() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let start = 2000;

        // Decode an initial keyframe (last good = seq 1).
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), start);
        jb.find_and_move_continuous_frames(start + 100);
        assert_eq!(jb.last_decoded_sequence_number, Some(1));
        decoded_frames.lock().unwrap().clear();

        // A stall: only delta frames arrive, no keyframe. The next continuous frame (2) goes stale.
        let stall_arrival = start + 200;
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), stall_arrival);
        jb.insert_frame(create_test_frame(3, FrameType::DeltaFrame), stall_arrival);

        let now = stall_arrival + (MAX_PLAYOUT_AGE_MS as u128) + 50;
        jb.find_and_move_continuous_frames(now);

        // No delta may be released (that would throw "key frame is required" in WebCodecs).
        assert!(
            decoded_frames.lock().unwrap().is_empty(),
            "no delta should be decoded without a keyframe"
        );
        // Last-good playout position is preserved.
        assert_eq!(jb.last_decoded_sequence_number, Some(1));
        // The stale head delta must be evicted so the buffer can't grow unbounded while we wait.
        assert!(
            !jb.buffered_frames.contains_key(&2),
            "stale head delta should be evicted"
        );
    }

    /// Issue #1025: evicting a stale KEYFRAME-LESS backlog (no buffered keyframe to
    /// skip to) must proactively fire the injected `request_keyframe` hook so the
    /// client fetches a fresh keyframe immediately, instead of waiting for its
    /// reactive gap-driven request. The contrast — a stale backlog that DOES contain
    /// a keyframe — skips to that keyframe and must NOT fire the hook (recovery is
    /// already in hand).
    ///
    /// Mutation coverage: removing the `(self.request_keyframe)()` call drops the
    /// keyframe-less count to 0 (first assert fails); firing from the keyframe-present
    /// branch fails the `== 0` contrast assert.
    #[test]
    fn keyframe_less_backlog_eviction_fires_proactive_keyframe_request() {
        // --- keyframe-less stall: exactly one proactive request on eviction ---
        let requests = Arc::new(AtomicU32::new(0));
        let decoded_frames = Arc::new(Mutex::new(Vec::new()));
        let req = requests.clone();
        let mut jb = JitterBuffer::with_keyframe_request(
            Box::new(MockDecoder::new_with_vec(decoded_frames.clone())),
            Box::new(move || {
                req.fetch_add(1, Ordering::SeqCst);
            }),
        );

        let start = 2000u128;
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), start);
        jb.find_and_move_continuous_frames(start + 100);
        assert_eq!(jb.last_decoded_sequence_number, Some(1));

        // Stall: only deltas arrive (no keyframe); seq 2 goes stale.
        let stall = start + 200;
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), stall);
        jb.insert_frame(create_test_frame(3, FrameType::DeltaFrame), stall);
        let now = stall + (MAX_PLAYOUT_AGE_MS as u128) + 50;
        jb.find_and_move_continuous_frames(now);

        assert!(
            !jb.buffered_frames.contains_key(&2),
            "stale head delta must be evicted"
        );
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "evicting a keyframe-less backlog must fire exactly one proactive keyframe request"
        );

        // --- contrast: a stale backlog WITH a buffered keyframe must NOT request ---
        let requests2 = Arc::new(AtomicU32::new(0));
        let decoded2 = Arc::new(Mutex::new(Vec::new()));
        let r2 = requests2.clone();
        let mut jb2 = JitterBuffer::with_keyframe_request(
            Box::new(MockDecoder::new_with_vec(decoded2.clone())),
            Box::new(move || {
                r2.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let s = 5000u128;
        jb2.insert_frame(create_test_frame(1, FrameType::KeyFrame), s);
        jb2.find_and_move_continuous_frames(s + 100);
        // Stall behind a gap, but a fresh keyframe (seq 10) IS buffered to skip to.
        let stall2 = s + 200;
        jb2.insert_frame(create_test_frame(5, FrameType::DeltaFrame), stall2);
        jb2.insert_frame(create_test_frame(10, FrameType::KeyFrame), stall2);
        let now2 = stall2 + (MAX_PLAYOUT_AGE_MS as u128) + 50;
        jb2.find_and_move_continuous_frames(now2);
        assert_eq!(
            requests2.load(Ordering::SeqCst),
            0,
            "skipping to a buffered keyframe must NOT fire a proactive request"
        );
    }

    /// Issue #1025 (source-side throttle): a SUSTAINED keyframe-less stall evicts
    /// one stale head per poll (~every 10ms tick in production). The proactive
    /// keyframe request must fire at most once per the *current* backoff interval so
    /// the struggling receiver does not spray its uplink/worker bus with requests the
    /// relay would only drop.
    ///
    /// Mutation coverage: removing the throttle (firing on every eviction) makes
    /// the within-window evictions fire too, failing the first `== 1` assert; not
    /// updating `last_keyframe_request_ms` makes the second window never fire,
    /// failing the `== 2` assert.
    #[test]
    fn proactive_keyframe_request_is_throttled_under_sustained_stall() {
        let requests = Arc::new(AtomicU32::new(0));
        let decoded = Arc::new(Mutex::new(Vec::new()));
        let req = requests.clone();
        let mut jb = JitterBuffer::with_keyframe_request(
            Box::new(MockDecoder::new_with_vec(decoded.clone())),
            Box::new(move || {
                req.fetch_add(1, Ordering::SeqCst);
            }),
        );

        // Decode a keyframe (last good = seq 1), then a keyframe-less backlog
        // (seq 2,3,4) all "arrived during the stall" so each is independently stale.
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), 100);
        jb.find_and_move_continuous_frames(200);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), 300);
        jb.insert_frame(create_test_frame(3, FrameType::DeltaFrame), 300);
        jb.insert_frame(create_test_frame(4, FrameType::DeltaFrame), 300);

        // Three evictions in quick succession (10ms apart, well within the 1000ms
        // base interval). Only the FIRST may fire a proactive request.
        jb.find_and_move_continuous_frames(2150); // evict seq 2 → fires (#1)
        jb.find_and_move_continuous_frames(2160); // evict seq 3 → throttled
        jb.find_and_move_continuous_frames(2170); // evict seq 4 → throttled
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "sustained eviction within one throttle window must fire exactly once"
        );

        // After the first fire the backoff exponent is 1, so the next interval is
        // 1000 * 2^1 = 2000ms. An eviction only 1050ms after the first must STILL be
        // throttled — proving the interval grew (issue #1479). Without backoff (fixed
        // 1000ms) this would fire and bump the count to 2.
        jb.insert_frame(create_test_frame(5, FrameType::DeltaFrame), 1400);
        jb.find_and_move_continuous_frames(3200); // 3200 - 2150 = 1050ms < 2000ms → throttled
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "within the backed-off (2000ms) window the request must still be throttled"
        );

        // Past the backed-off window (>= 2000ms after the first fire) it fires again.
        jb.insert_frame(create_test_frame(6, FrameType::DeltaFrame), 1450);
        jb.find_and_move_continuous_frames(4200); // 4200 - 2150 = 2050ms >= 2000ms → fires (#2)
        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "an eviction past the backed-off window fires a fresh request"
        );
    }

    /// Issue #1479: under a SUSTAINED keyframe-less stall the proactive-PLI interval
    /// must grow exponentially (1s → 2s → 4s → 8s, capped) so the receiver stops poking
    /// the active speaker ~1/s for the whole stall — the receiver-side amplifier of the
    /// PLI keyframe storm. The backoff exponent must reset ONLY after a genuinely quiet
    /// interval (> PROACTIVE_KEYFRAME_REQUEST_BACKOFF_RESET_MS with no proactive request),
    /// NOT on a mere frame release — the #1479 storm is many short freezes interleaved with
    /// smooth playout, so a release-driven reset would zero the exponent during each smooth
    /// gap and the backoff would never accumulate across the storm.
    ///
    /// Mutation coverage: deleting the backoff (firing at the fixed base interval) lets the
    /// 2nd fire land at 1s instead of 2s — a "still throttled" assert fails. Deleting the
    /// quiet-interval reset leaves the exponent pinned at the cap, so the post-quiet fire is
    /// throttled for 8s and the final "fires at base after a long quiet gap" assert fails.
    #[test]
    fn proactive_keyframe_request_backs_off_and_resets_after_quiet_interval() {
        let requests = Arc::new(AtomicU32::new(0));
        let decoded = Arc::new(Mutex::new(Vec::new()));
        let req = requests.clone();
        let mut jb = JitterBuffer::with_keyframe_request(
            Box::new(MockDecoder::new_with_vec(decoded.clone())),
            Box::new(move || {
                req.fetch_add(1, Ordering::SeqCst);
            }),
        );

        // last-good = seq 1.
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), 100);
        jb.find_and_move_continuous_frames(200);

        let base = PROACTIVE_KEYFRAME_REQUEST_MIN_INTERVAL_MS as u128; // 1000

        // Pre-insert a keyframe-LESS delta backlog, all at the SAME early arrival (300). Each poll
        // evicts exactly one head delta (keyframe-less branch drops only the head); the next delta
        // then becomes the head with the same old arrival, so it too is instantly stale on the next
        // poll. last_decoded stays at 1 the whole stall (nothing is released), so every poll is
        // *eligible* to fire — gated only by the backoff interval. Inserting all at one arrival
        // keeps the jitter estimator's inter-arrival delta non-negative (it underflows on
        // backwards-moving arrival times). Each poll below evicts one head delta, so the backlog
        // must be at least as large as the total number of polls (12 here); 2..=24 is generous.
        for s in 2u64..=24 {
            jb.insert_frame(create_test_frame(s, FrameType::DeltaFrame), 300);
        }

        // First fire (no prior request → fires immediately).
        let mut last_fire = 5000u128;
        jb.find_and_move_continuous_frames(last_fire);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "first proactive request fires"
        );

        // Sustained stall: the interval after each fire doubles — 2s, 4s, 8s, then capped at 8s.
        let expected_gaps_ms = [2 * base, 4 * base, 8 * base, 8 * base];
        for (i, gap) in expected_gaps_ms.iter().enumerate() {
            // Just BEFORE the backed-off interval → still throttled.
            jb.find_and_move_continuous_frames(last_fire + gap - 100);
            assert_eq!(
                requests.load(Ordering::SeqCst),
                (i + 1) as u32,
                "request {} must still be throttled just before the {}ms backed-off window",
                i + 2,
                gap
            );
            // Just AFTER the backed-off interval → fires.
            let after_t = last_fire + gap + 50;
            jb.find_and_move_continuous_frames(after_t);
            assert_eq!(
                requests.load(Ordering::SeqCst),
                (i + 2) as u32,
                "request {} fires once past the {}ms backed-off window",
                i + 2,
                gap
            );
            last_fire = after_t;
        }
        let fires_during_stall = requests.load(Ordering::SeqCst);
        assert_eq!(fires_during_stall, 5, "5 fires across the backed-off stall");
        // The exponent is now pinned at the cap; the next interval would be 8s WITHOUT a reset.

        // QUIET INTERVAL (genuine recovery): no proactive request fires for longer than
        // PROACTIVE_KEYFRAME_REQUEST_BACKOFF_RESET_MS. The next keyframe-less eviction after
        // that gap must reset the exponent to 0 and fire at the BASE interval again. We poll
        // once, `RESET_MS + a bit` after the last fire: that single poll is >= base (so it is
        // eligible) and, because it crossed the reset threshold, the exponent is cleared FIRST,
        // so it fires. With the reset deleted the exponent stays at the cap and the required
        // interval is 8s — but RESET_MS (12s) > 8s, so to prove the *reset* (not just elapsed
        // time) we follow with a second poll exactly base+ after this fire: that only fires if
        // the exponent is now back at 0.
        let reset_ms = PROACTIVE_KEYFRAME_REQUEST_BACKOFF_RESET_MS as u128;
        let quiet_fire = last_fire + reset_ms + 100;
        jb.find_and_move_continuous_frames(quiet_fire);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            fires_during_stall + 1,
            "after a quiet interval the next eviction fires"
        );

        // The DISCRIMINATING assert: a follow-up eviction only `2*base` after `quiet_fire` must
        // FIRE — proving the exponent was reset to 0 (interval back to base), not still at the
        // cap (which would require 8s). 2*base = 2000ms is >= base(1000, after the now-1 exponent
        // doubles to 2s) … so we instead poll base+50 after, before the post-reset exponent's
        // own 2s step. Precisely: after the reset-fire the exponent is 1, so the next interval is
        // 2s; a poll at base+50 (1050ms) is < 2s and must be THROTTLED, and a poll at 2*base+50
        // must FIRE. We assert the throttle first (distinguishes reset-to-0 from no-reset, since
        // no-reset would also be throttled here — so this alone is not enough), then the fire.
        jb.find_and_move_continuous_frames(quiet_fire + base + 50); // 1050ms < 2000ms post-reset step
        assert_eq!(
            requests.load(Ordering::SeqCst),
            fires_during_stall + 1,
            "immediately after the reset-fire, the exponent-1 (2s) step throttles a base-spaced poll"
        );
        jb.find_and_move_continuous_frames(quiet_fire + 2 * base + 50); // 2050ms >= 2000ms → fires
        assert_eq!(
            requests.load(Ordering::SeqCst),
            fires_during_stall + 2,
            "post-reset the cadence is back on the base schedule (2s after the reset-fire), proving the exponent reset to 0 — not pinned at the 8s cap"
        );
    }

    /// `flush()` resets the proactive keyframe-request throttle so a fresh post-flush
    /// stream can request recovery immediately instead of inheriting the previous
    /// stall's cooldown.
    #[test]
    fn flush_resets_proactive_keyframe_request_throttle() {
        let requests = Arc::new(AtomicU32::new(0));
        let decoded = Arc::new(Mutex::new(Vec::new()));
        let req = requests.clone();
        let mut jb = JitterBuffer::with_keyframe_request(
            Box::new(MockDecoder::new_with_vec(decoded.clone())),
            Box::new(move || {
                req.fetch_add(1, Ordering::SeqCst);
            }),
        );

        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), 100);
        jb.find_and_move_continuous_frames(200);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), 300);
        jb.find_and_move_continuous_frames(2150);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "first stale keyframe-less eviction fires a proactive request"
        );

        jb.flush();
        decoded.lock().unwrap().clear();

        jb.last_decoded_sequence_number = Some(10);
        jb.insert_frame(create_test_frame(11, FrameType::DeltaFrame), 300);
        jb.find_and_move_continuous_frames(2200);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "post-flush eviction fires immediately despite being inside the pre-flush throttle window"
        );
    }

    /// `flush()` must reset the backoff EXPONENT (issue #1479), not just the
    /// `last_keyframe_request_ms` anchor. The sibling `flush_resets_..._throttle` test cannot
    /// catch the exponent reset because `flush()` also nulls `last_keyframe_request_ms`, so the
    /// first post-flush eviction fires via the `None => true` arm regardless of the exponent.
    /// This test drives the exponent to the cap pre-flush, flushes, then asserts the post-flush
    /// cadence is back on the BASE schedule (exponent 1 → 2s after the first post-flush fire),
    /// not the inherited cap (8s).
    ///
    /// Mutation coverage: deleting `consecutive_proactive_keyframe_requests = 0` from `flush()`
    /// leaves the exponent at the cap, so after the first post-flush fire the interval is 8s and
    /// the `2*base` poll would NOT fire — failing the final assert.
    #[test]
    fn flush_resets_proactive_keyframe_request_backoff_exponent() {
        let requests = Arc::new(AtomicU32::new(0));
        let decoded = Arc::new(Mutex::new(Vec::new()));
        let req = requests.clone();
        let mut jb = JitterBuffer::with_keyframe_request(
            Box::new(MockDecoder::new_with_vec(decoded.clone())),
            Box::new(move || {
                req.fetch_add(1, Ordering::SeqCst);
            }),
        );

        let base = PROACTIVE_KEYFRAME_REQUEST_MIN_INTERVAL_MS as u128; // 1000

        // Drive the exponent to the cap: last-good = seq 1, then a keyframe-less backlog polled
        // past each backed-off window so several requests fire (exponent climbs to >= cap).
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), 100);
        jb.find_and_move_continuous_frames(200);
        for s in 2u64..=12 {
            jb.insert_frame(create_test_frame(s, FrameType::DeltaFrame), 300);
        }
        let mut last_fire = 5000u128;
        jb.find_and_move_continuous_frames(last_fire); // fire #1 (exp→1)
        for gap in [2 * base, 4 * base, 8 * base, 8 * base] {
            last_fire += gap + 50;
            jb.find_and_move_continuous_frames(last_fire); // fires; exponent climbs to the cap
        }
        let pre_flush = requests.load(Ordering::SeqCst);
        assert_eq!(
            pre_flush, 5,
            "exponent driven to the cap via 5 backed-off fires"
        );

        jb.flush();

        // Post-flush stall. First eviction fires immediately (last_keyframe_request_ms is None).
        // After it, the exponent is 1 ONLY IF flush reset it (else it is cap+1, still cap).
        jb.last_decoded_sequence_number = Some(100);
        for s in 101u64..=104 {
            jb.insert_frame(create_test_frame(s, FrameType::DeltaFrame), 20_000);
        }
        let t0 = 22_000u128; // > 20_000 + MAX_PLAYOUT_AGE_MS so the head delta is stale
        jb.find_and_move_continuous_frames(t0);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            pre_flush + 1,
            "first post-flush eviction fires"
        );

        // Discriminator: with the exponent reset to 0 → now 1, the next interval is 2s. A poll
        // base+50 (1050ms) after the post-flush fire must be THROTTLED, and a poll 2*base+50 must
        // FIRE. If flush did NOT reset the exponent (still at cap), the interval would be 8s and
        // the 2*base poll would NOT fire.
        jb.find_and_move_continuous_frames(t0 + base + 50); // 1050ms < 2000ms (exp-1 step) → throttled
        assert_eq!(
            requests.load(Ordering::SeqCst),
            pre_flush + 1,
            "exp-1 (2s) step throttles a base-spaced poll right after the post-flush fire"
        );
        jb.find_and_move_continuous_frames(t0 + 2 * base + 50); // 2050ms >= 2000ms → fires
        assert_eq!(
            requests.load(Ordering::SeqCst),
            pre_flush + 2,
            "post-flush cadence is back on the base schedule, proving flush reset the exponent (not pinned at the 8s cap)"
        );
    }

    /// Issue #1045: both freshness-skip paths must surface a `FreshnessSkip` for the
    /// worker to forward to field logs — the keyframe-less eviction (`keyframe_seq:
    /// None`, playout held) and the skip-to-buffered-keyframe path (`keyframe_seq:
    /// Some`). `take_freshness_skip` returns and clears it.
    ///
    /// Mutation coverage: not stashing in either branch makes the corresponding
    /// `expect` fail; a wrong `keyframe_seq` fails the `assert_eq!`.
    #[test]
    fn freshness_skip_is_surfaced_for_both_paths() {
        // Keyframe-LESS eviction → keyframe_seq None.
        let (mut jb, _decoded) = create_test_jitter_buffer();
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), 100);
        jb.find_and_move_continuous_frames(200);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), 300);
        jb.find_and_move_continuous_frames(300 + MAX_PLAYOUT_AGE_MS as u128 + 50);
        let skip = jb
            .take_freshness_skip()
            .expect("keyframe-less eviction must surface a skip");
        assert_eq!(skip.keyframe_seq, None, "no buffered keyframe → None");
        assert!(skip.dropped >= 1, "at least one stale frame evicted");
        assert!(
            skip.head_age_ms >= MAX_PLAYOUT_AGE_MS,
            "head age must have tripped the deadline"
        );
        assert!(
            jb.take_freshness_skip().is_none(),
            "take must clear the skip"
        );

        // Skip-to-buffered-keyframe → keyframe_seq Some.
        let (mut jb2, _d2) = create_test_jitter_buffer();
        jb2.insert_frame(create_test_frame(1, FrameType::KeyFrame), 1000);
        jb2.find_and_move_continuous_frames(1100);
        jb2.insert_frame(create_test_frame(5, FrameType::DeltaFrame), 1200); // stale head (gap)
        jb2.insert_frame(create_test_frame(10, FrameType::KeyFrame), 1200); // keyframe to skip to
        jb2.find_and_move_continuous_frames(1200 + MAX_PLAYOUT_AGE_MS as u128 + 50);
        let skip2 = jb2
            .take_freshness_skip()
            .expect("skip-to-keyframe must surface a skip");
        assert_eq!(
            skip2.keyframe_seq,
            Some(10),
            "must report the keyframe skipped to"
        );
        assert!(skip2.dropped >= 1);
    }

    /// Freshness-skip diagnostics use the same source-side cadence as proactive keyframe
    /// requests: a sustained stall must not post worker→main diagnostics on every poll, but the
    /// next emitted diagnostic should include the skipped frames coalesced while the gate was
    /// closed.
    #[test]
    fn freshness_skip_diagnostics_are_throttled_and_coalesced() {
        let (mut jb, _decoded) = create_test_jitter_buffer();
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), 100);
        jb.find_and_move_continuous_frames(200);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), 300);
        jb.insert_frame(create_test_frame(3, FrameType::DeltaFrame), 300);
        jb.insert_frame(create_test_frame(4, FrameType::DeltaFrame), 300);

        jb.find_and_move_continuous_frames(2150);
        let first = jb
            .take_freshness_skip()
            .expect("first freshness skip should emit immediately");
        assert_eq!(first.keyframe_seq, None);
        assert_eq!(first.dropped, 1);

        jb.find_and_move_continuous_frames(2160);
        assert!(
            jb.take_freshness_skip().is_none(),
            "within-window freshness skip should be throttled"
        );
        jb.find_and_move_continuous_frames(2170);
        assert!(
            jb.take_freshness_skip().is_none(),
            "second within-window freshness skip should also be coalesced"
        );

        jb.insert_frame(create_test_frame(5, FrameType::DeltaFrame), 1400);
        jb.find_and_move_continuous_frames(3200);
        let coalesced = jb
            .take_freshness_skip()
            .expect("next outside-window freshness skip should emit");
        assert_eq!(coalesced.keyframe_seq, None);
        assert_eq!(
            coalesced.dropped, 3,
            "diagnostic should include skipped frames coalesced while throttled plus the current skip"
        );
    }

    /// Regression guard for the infinite-loop blocker: a stale post-gap keyframe that is ALREADY
    /// the head of the buffer (nothing before it to drop) must NOT spin `find_and_move_continuous_frames`
    /// forever. It must terminate and release the keyframe through the normal gate.
    ///
    /// Reachable trigger: background-tab throttling. `setInterval` is clamped/paused while the tab
    /// is backgrounded, so on refocus the first tick's `Date::now()` delta is multi-second — a
    /// buffered post-gap keyframe is instantly >= MAX_PLAYOUT_AGE_MS old while sitting at the head.
    ///
    /// Before the shrink-guard fix this test would hang (deadlock the worker tick); reaching the
    /// assertions at all proves termination.
    #[test]
    fn stale_head_keyframe_terminates_and_releases() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let arrival = 1000u128;

        // We are mid-stream (last decoded = 1), then a gap: only a single keyframe at seq 5 is
        // buffered, and it is the head (nothing before it). Simulate this state directly.
        jb.last_decoded_sequence_number = Some(1);
        jb.insert_frame(create_test_frame(5, FrameType::KeyFrame), arrival);
        // insert_frame already ran a poll; the keyframe is fresh at this point so nothing decoded.
        decoded_frames.lock().unwrap().clear();
        assert!(jb.buffered_frames.contains_key(&5));

        // Poll at a time where the head keyframe (seq 5) is well past MAX_PLAYOUT_AGE_MS.
        // Without the fix, drop_frames_before(5) removes nothing, enforce returns true, and the
        // loop continues forever. With the fix it returns false and the normal gate releases 5.
        let now = arrival + (MAX_PLAYOUT_AGE_MS as u128) + 100;
        jb.find_and_move_continuous_frames(now);

        // Reaching here at all proves the loop terminated.
        let queue = decoded_frames.lock().unwrap();
        assert_eq!(
            queue.len(),
            1,
            "head keyframe should be released, not spun on"
        );
        assert_eq!(queue[0].sequence_number, 5);
        assert_eq!(jb.last_decoded_sequence_number, Some(5));
        // Nothing was before the keyframe, so nothing was dropped by the deadline path.
        assert_eq!(jb.get_dropped_frames_count(), 0);
        assert!(jb.buffered_frames.is_empty());
    }

    /// Normal jitter within bounds is byte-for-byte unaffected: frames are released by the existing
    /// lower-bound gate and nothing is dropped by the freshness deadline.
    #[test]
    fn normal_jitter_is_unaffected_by_freshness_deadline() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let mut time = 1000;

        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), time);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), time);
        jb.insert_frame(create_test_frame(3, FrameType::DeltaFrame), time);

        // Advance well past the playout delay but FAR below MAX_PLAYOUT_AGE_MS (1800ms).
        time += 100;
        jb.find_and_move_continuous_frames(time);

        let queue = decoded_frames.lock().unwrap();
        assert_eq!(queue.len(), 3);
        assert_eq!(queue[0].sequence_number, 1);
        assert_eq!(queue[1].sequence_number, 2);
        assert_eq!(queue[2].sequence_number, 3);
        // Nothing dropped by the deadline.
        assert_eq!(jb.get_dropped_frames_count(), 0);
    }

    /// A frame sitting just under the deadline is still released normally (boundary check).
    #[test]
    fn frame_just_under_deadline_is_released_normally() {
        let (mut jb, decoded_frames) = create_test_jitter_buffer();
        let start = 1000;

        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), start);
        // Poll at just under the deadline — should release normally, not skip/drop.
        let now = start + (MAX_PLAYOUT_AGE_MS as u128) - 100;
        jb.find_and_move_continuous_frames(now);

        let queue = decoded_frames.lock().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].sequence_number, 1);
        assert_eq!(jb.get_dropped_frames_count(), 0);
    }

    // --- Release-side decode-queue backpressure (issue #1024) ---

    /// While the WebCodecs decoder's internal queue is at/above the high-water mark, the jitter
    /// buffer must hold (not release, not drop) ready frames; once the decoder drains below the
    /// mark, release resumes. Frames stay well under MAX_PLAYOUT_AGE_MS throughout, so the freshness
    /// deadline never participates — this isolates the backpressure gate.
    #[test]
    fn backpressure_holds_frames_at_hwm_then_releases() {
        let (mut jb, decoded_frames, queue_depth) = create_test_jitter_buffer_with_queue_depth();
        let mut time = 1000;

        // Decoder is already backed up at the high-water mark.
        queue_depth.store(DECODE_QUEUE_HIGH_WATER_MARK, Ordering::SeqCst);

        // A keyframe + delta arrive and age past the playout delay (but far below the freshness
        // deadline).
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), time);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), time);
        time += 100;
        jb.find_and_move_continuous_frames(time);

        // Nothing may be released while the decoder sits at the mark, and nothing may be dropped.
        assert!(
            decoded_frames.lock().unwrap().is_empty(),
            "backpressure must hold all frames while the decode queue is at the high-water mark"
        );
        assert_eq!(
            jb.buffered_frames_len(),
            2,
            "held frames stay buffered, not dropped"
        );
        assert_eq!(jb.get_dropped_frames_count(), 0);

        // Decoder drains below the mark -> release resumes on the next tick.
        queue_depth.store(DECODE_QUEUE_HIGH_WATER_MARK - 1, Ordering::SeqCst);
        time += 100; // still far under MAX_PLAYOUT_AGE_MS
        jb.find_and_move_continuous_frames(time);

        let queue = decoded_frames.lock().unwrap();
        assert_eq!(
            queue.len(),
            2,
            "frames must release once the decoder drains below the high-water mark"
        );
        assert_eq!(queue[0].sequence_number, 1);
        assert_eq!(queue[1].sequence_number, 2);
    }

    /// The freshness deadline is the backpressure safety valve and MUST run first: even with the
    /// decoder pinned at the high-water mark, a stale backlog is still evicted (so it cannot grow
    /// unbounded behind a slow decoder). Backpressure only gates the release of the *fresh*
    /// skip-to-live keyframe, which decodes once the decoder drains.
    ///
    /// This guards the ordering (freshness before backpressure) AND the gate's existence:
    /// - move the gate before `enforce_freshness_deadline` → stale deltas are not evicted (fails);
    /// - remove the gate → the keyframe releases immediately despite the full queue (fails).
    #[test]
    fn freshness_eviction_runs_before_backpressure() {
        let (mut jb, decoded_frames, queue_depth) = create_test_jitter_buffer_with_queue_depth();
        let start = 1000;

        // Decode an initial keyframe so we are mid-stream (last good = seq 1).
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), start);
        jb.find_and_move_continuous_frames(start + 100);
        assert_eq!(jb.last_decoded_sequence_number, Some(1));
        decoded_frames.lock().unwrap().clear();

        // Pin the decoder at the high-water mark so backpressure is active for the rest of the test.
        queue_depth.store(DECODE_QUEUE_HIGH_WATER_MARK, Ordering::SeqCst);

        // Baseline drop count captured while the decoder is ALREADY pinned but BEFORE the stall, so
        // the delta below counts every freshness eviction that happens under backpressure —
        // including the one that fires inside `insert_frame(5)`'s own end-of-insert poll, not only
        // the explicit poll below. (Capturing after the inserts would read 0 and miss it.)
        let dropped_before_stall = jb.get_dropped_frames_count();

        // A stall: stale deltas accumulate, then a fresh keyframe arrives much later.
        let stall_arrival = start + 200;
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), stall_arrival);
        jb.insert_frame(create_test_frame(3, FrameType::DeltaFrame), stall_arrival);
        let keyframe_arrival = stall_arrival + (MAX_PLAYOUT_AGE_MS as u128) + 100;
        jb.insert_frame(create_test_frame(5, FrameType::KeyFrame), keyframe_arrival);

        // Poll where the head delta (seq 2) is stale but the keyframe (seq 5) is fresh.
        let now = keyframe_arrival + 50;
        jb.find_and_move_continuous_frames(now);

        // Snapshot the state produced by that single under-backpressure tick into locals BEFORE the
        // drain below, so the assertions are about the backpressured tick specifically and do not
        // depend on being sequenced ahead of the drain (issue #1326 hardening). Asserting against
        // the live buffer after the drain would pass spuriously: releasing keyframe 5 via
        // gap-recovery runs `drop_frames_before(5)`, which removes deltas 2/3 anyway — so a
        // post-drain `!contains_key(&2)` cannot tell whether the *freshness deadline* evicted them
        // (gate runs second, correct) or the *keyframe-recovery drop* did (gate ran first, the
        // regression). Capturing here freezes the distinction.
        let evicted_2 = !jb.buffered_frames.contains_key(&2);
        let evicted_3 = !jb.buffered_frames.contains_key(&3);
        let dropped_by_freshness = jb.get_dropped_frames_count() - dropped_before_stall;
        let keyframe_held = jb.buffered_frames.contains_key(&5);
        let decoded_under_backpressure = decoded_frames.lock().unwrap().len();
        let playout_after_poll = jb.last_decoded_sequence_number;

        // Freshness ran FIRST, under backpressure: it evicted exactly the two stale deltas while the
        // decoder was pinned at the HWM and nothing had been released yet. Because the keyframe is
        // still held (not released), the only thing that could have dropped 2/3 at this point is the
        // freshness deadline — not the keyframe-recovery drop, which only fires once 5 is released.
        // Mutation coverage: moving the gate before `enforce_freshness_deadline` makes the gate
        // `break` before freshness runs → 0 dropped, 2/3 retained (fails); removing the gate lets
        // keyframe 5 release immediately despite the full queue → decoded == 1, playout advances
        // (fails). Both hold regardless of where these asserts sit relative to the drain.
        assert!(
            evicted_2 && evicted_3,
            "stale deltas must be evicted by the freshness deadline even under backpressure"
        );
        assert_eq!(
            dropped_by_freshness, 2,
            "freshness must evict exactly the two stale deltas while the decoder is pinned at the HWM"
        );
        assert_eq!(
            decoded_under_backpressure, 0,
            "the skip-to-live keyframe is gated by backpressure until the decoder drains"
        );
        assert!(
            keyframe_held,
            "the fresh keyframe is retained, awaiting decoder drain"
        );
        assert_eq!(
            playout_after_poll,
            Some(1),
            "playout must not advance while backpressure holds the keyframe"
        );

        // Decoder drains -> the keyframe releases and skip-to-live completes.
        queue_depth.store(0, Ordering::SeqCst);
        jb.find_and_move_continuous_frames(now + 10);
        let queue = decoded_frames.lock().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].sequence_number, 5);
        assert_eq!(jb.last_decoded_sequence_number, Some(5));
    }

    // --- Wedged-decoder held-too-long escape hatch (issue #1324) ---

    /// A truly wedged decoder — queue pinned at/above the HWM, never draining — must NOT freeze
    /// playout forever, even in the case the freshness deadline structurally cannot see: the
    /// head-of-line frame stays FRESH (< MAX_PLAYOUT_AGE_MS) because the deadline keeps skipping to
    /// a newly-arrived keyframe each tick, yet the gate holds that fresh keyframe every tick. Once
    /// release has been held past MAX_BACKPRESSURE_HOLD_MS with a frame waiting, the escape hatch
    /// force-releases the head frame AND resets the decoder pipeline.
    ///
    /// This faithfully reproduces the #1324 mechanism: fresh keyframes keep arriving (so the head is
    /// continuously refreshed and never trips the 1800ms freshness deadline), but the wedged decoder
    /// never drains, so backpressure holds release indefinitely. Only the held-too-long escape can
    /// break it.
    ///
    /// Mutation coverage: deleting the escape-hatch branch leaves the gate `break`ing every tick, so
    /// nothing is ever released and `reset()` is never called -> both final assertions fail.
    #[test]
    fn wedged_decoder_escape_hatch_force_releases_and_resets() {
        let (mut jb, decoded_frames, queue_depth, reset_count) =
            create_test_jitter_buffer_with_queue_and_reset();

        // Establish a normal release (mirrors a real stream that was healthy before the decoder
        // wedged). The continuous-hold clock starts only once the gate actually holds, below.
        let start = 100_000u128;
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), start);
        jb.find_and_move_continuous_frames(start + 100);
        assert_eq!(jb.last_decoded_sequence_number, Some(1));
        assert_eq!(decoded_frames.lock().unwrap().len(), 1);
        let pre_wedge_release = start + 100;
        decoded_frames.lock().unwrap().clear();

        // The decoder now wedges: queue pinned at the HWM forever.
        queue_depth.store(DECODE_QUEUE_HIGH_WATER_MARK, Ordering::SeqCst);

        // Tick every 200ms. On each tick a FRESH keyframe arrives at a big sequence gap from the
        // last decoded seq, so the freshness deadline keeps skipping to it (dropping the previous
        // stale keyframe) and the head-of-line keyframe is therefore always recently arrived — its
        // head-of-line age stays well under MAX_PLAYOUT_AGE_MS (1800ms) and the freshness deadline
        // never *releases* anything (it only refreshes the head, since the gate holds release every
        // tick). The continuous-hold clock (started at the first gate-hold tick) climbs past
        // MAX_BACKPRESSURE_HOLD_MS. Only the held-too-long escape can break the wedge.
        //
        // `pre_wedge_release` is a CONSERVATIVE lower bound for the false-positive guard: the real
        // hold clock starts at the first gate-hold (one tick later), so `now - pre_wedge_release`
        // overestimates the held time. Using it to gate the "must not have fired yet" assertion can
        // therefore only ever skip the assertion early — it never asserts no-fire when the escape
        // could legitimately have fired.
        let mut now = pre_wedge_release + 200;
        let mut keyframe_seq = 100u64; // big gap from seq 1 -> always treated as gap recovery
        let mut fired = false;
        // Run long enough that the hold clock crosses MAX_BACKPRESSURE_HOLD_MS with margin, then
        // some headroom to observe the escape.
        for _ in 0..20 {
            keyframe_seq += 1;
            // Fresh keyframe arrives "now" (age 0 at this tick).
            jb.insert_frame(create_test_frame(keyframe_seq, FrameType::KeyFrame), now);
            jb.find_and_move_continuous_frames(now);

            let held_upper_bound = (now - pre_wedge_release) as f64;
            let released = !decoded_frames.lock().unwrap().is_empty();
            let reset_fired = reset_count.load(Ordering::SeqCst) >= 1;

            if !fired && held_upper_bound <= MAX_BACKPRESSURE_HOLD_MS {
                // Even by the overestimate we are under the threshold: the escape must not have
                // fired yet (no false positive).
                assert!(
                    !released,
                    "escape hatch must not fire before MAX_BACKPRESSURE_HOLD_MS (held <= {held_upper_bound}ms)"
                );
                assert!(
                    !reset_fired,
                    "no reset before the threshold (held <= {held_upper_bound}ms)"
                );
            }
            if released && reset_fired {
                fired = true;
            }
            now += 200;
        }

        // By now held >> MAX_BACKPRESSURE_HOLD_MS. The escape hatch must have fired: a frame was
        // force-released past the still-pinned queue AND the decoder pipeline was reset.
        assert!(
            !decoded_frames.lock().unwrap().is_empty(),
            "wedged decoder must recover: a frame must be force-released past the threshold"
        );
        assert!(
            reset_count.load(Ordering::SeqCst) >= 1,
            "wedged decoder must recover: the pipeline must be reset past the threshold"
        );
    }

    /// No false positive: a decoder that is merely SLOW — pinned at the HWM transiently but draining
    /// below it within MAX_BACKPRESSURE_HOLD_MS — must NEVER trigger the escape hatch (no forced
    /// release while gated, no reset). Normal backpressure must remain exactly as in #1024.
    ///
    /// Mutation coverage: lowering MAX_BACKPRESSURE_HOLD_MS below the drain window, or removing the
    /// "held longer than threshold" guard, would fire the escape here and reset the decoder -> the
    /// `reset_count == 0` assertion fails.
    #[test]
    fn slow_decoder_draining_within_threshold_never_escapes() {
        let (mut jb, decoded_frames, queue_depth, reset_count) =
            create_test_jitter_buffer_with_queue_and_reset();
        let mut time = 1000u128;

        // Healthy first release; the gate is not holding, so no hold clock runs yet.
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), time);
        time += 100;
        jb.find_and_move_continuous_frames(time);
        assert_eq!(decoded_frames.lock().unwrap().len(), 1);

        // Decoder backs up at the HWM. Frame 2 waits behind the gate (this starts the hold clock).
        queue_depth.store(DECODE_QUEUE_HIGH_WATER_MARK, Ordering::SeqCst);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), time);

        // Hold for a while but stay UNDER MAX_BACKPRESSURE_HOLD_MS (and under MAX_PLAYOUT_AGE_MS so
        // freshness also stays out). 1000ms of hold across several ticks.
        for _ in 0..5 {
            time += 200;
            jb.find_and_move_continuous_frames(time);
        }
        // Still gated: nothing released past the first frame, and crucially NO reset fired.
        assert_eq!(
            decoded_frames.lock().unwrap().len(),
            1,
            "a merely-slow decoder must stay gated, not force-released"
        );
        assert_eq!(
            reset_count.load(Ordering::SeqCst),
            0,
            "the escape hatch must not reset a decoder that is only transiently slow"
        );

        // The decoder drains within the threshold -> normal release resumes, still no reset.
        queue_depth.store(DECODE_QUEUE_HIGH_WATER_MARK - 1, Ordering::SeqCst);
        time += 100;
        jb.find_and_move_continuous_frames(time);
        let queue = decoded_frames.lock().unwrap();
        assert_eq!(queue.len(), 2, "frame 2 releases once the decoder drains");
        assert_eq!(queue[1].sequence_number, 2);
        assert_eq!(
            reset_count.load(Ordering::SeqCst),
            0,
            "no reset should ever occur for a slow-but-draining decoder"
        );
    }

    /// The continuous-hold clock must RESET on a successful (gate-open, drain-then-release) release
    /// so a later transient hold does not inherit stale elapsed time. Sequence: a long gated hold
    /// (kept just under the threshold), then a drain that releases a frame, then a brief later hold
    /// — the brief hold must not escape because the clock restarted at the intervening release.
    ///
    /// Mutation coverage: this pins the post-release reset behavior. The drain path clears the clock
    /// via the gate-open path (`backpressure_hold_since_ms = None` after the gate check) AND via
    /// `push_to_decoder` on the release; removing BOTH clears makes the second hold measure elapsed
    /// from the first hold (1100ms) -> by the 3150ms poll that is 2050ms > the threshold, so the
    /// escape fires and resets the decoder, failing the `reset_count == 0` assertion. (The dedicated
    /// `escape_force_release_does_not_refire_next_tick` test below isolates the `push_to_decoder`
    /// clear specifically, under a pinned queue where the gate-open clear cannot fire.)
    #[test]
    fn successful_release_resets_held_clock() {
        let (mut jb, decoded_frames, queue_depth, reset_count) =
            create_test_jitter_buffer_with_queue_and_reset();

        // Healthy first release at t=1000.
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), 1000);
        jb.find_and_move_continuous_frames(1100);
        assert_eq!(decoded_frames.lock().unwrap().len(), 1);

        // A long gated hold, but stay strictly UNDER the threshold so the escape never fires.
        queue_depth.store(DECODE_QUEUE_HIGH_WATER_MARK, Ordering::SeqCst);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), 1100);
        // Hold ~1700ms (just under 2000ms), keeping the head fresh: frame 2's arrival was 1100; we
        // must keep head-age < 1800ms, so poll at <= 1100+1800. 1100 + 1700 = 2800 keeps head-age
        // 1700ms (< 1800) and held 1700ms (< 2000). No escape.
        jb.find_and_move_continuous_frames(2800);
        assert_eq!(
            reset_count.load(Ordering::SeqCst),
            0,
            "no escape while under both thresholds"
        );

        // Decoder drains -> frame 2 releases at t=2850. This CLEARS the continuous-hold clock.
        queue_depth.store(0, Ordering::SeqCst);
        jb.find_and_move_continuous_frames(2850);
        assert_eq!(decoded_frames.lock().unwrap().len(), 2);

        // A NEW brief hold begins right after. Because the hold clock was cleared at the 2850
        // release, it restarts at the next gate-hold (the insert poll at 2860), so a short hold of
        // only ~300ms must NOT escape (it would if the clock had inherited the earlier long hold).
        queue_depth.store(DECODE_QUEUE_HIGH_WATER_MARK, Ordering::SeqCst);
        jb.insert_frame(create_test_frame(3, FrameType::DeltaFrame), 2860);
        jb.find_and_move_continuous_frames(3150); // held only 300ms since the 2850 release
        assert_eq!(
            reset_count.load(Ordering::SeqCst),
            0,
            "a brief hold after a successful release must not inherit stale elapsed time"
        );
        // Frame 3 stays buffered (gated), not force-released.
        assert_eq!(decoded_frames.lock().unwrap().len(), 2);
        assert!(jb.buffered_frames.contains_key(&3));
    }

    /// The escape's force-release must reset the continuous-hold clock so the escape does NOT
    /// re-fire on the very next tick. This isolates the `push_to_decoder` clear: throughout this
    /// test the queue stays PINNED at the HWM, so the gate-open clear can never run — only
    /// `push_to_decoder` (reached via the escape's force-release) can reset the clock.
    ///
    /// Mutation coverage: if `push_to_decoder` did not clear `backpressure_hold_since_ms`, after the
    /// first escape the clock would still read from the original hold; the next tick (queue still
    /// pinned, a fresh frame waiting) would see elapsed still > the threshold and escape AGAIN
    /// immediately, so two ticks 10ms apart would each fire a reset. The assertion that the second
    /// (immediately-following) tick adds NO new reset fails under that mutation.
    #[test]
    fn escape_force_release_does_not_refire_next_tick() {
        let (mut jb, decoded_frames, queue_depth, reset_count) =
            create_test_jitter_buffer_with_queue_and_reset();

        // Healthy first release, then wedge the decoder (queue pinned for the rest of the test).
        let start = 50_000u128;
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), start);
        jb.find_and_move_continuous_frames(start + 100);
        assert_eq!(decoded_frames.lock().unwrap().len(), 1);
        queue_depth.store(DECODE_QUEUE_HIGH_WATER_MARK, Ordering::SeqCst);

        // Drive the wedge until the escape fires exactly once. Fresh keyframes keep the head fresh
        // (so freshness never releases anything); the hold clock starts at the first gate-hold and
        // climbs past the threshold.
        let mut now = start + 300;
        let mut seq = 100u64;
        while reset_count.load(Ordering::SeqCst) == 0 {
            seq += 1;
            jb.insert_frame(create_test_frame(seq, FrameType::KeyFrame), now);
            jb.find_and_move_continuous_frames(now);
            now += 200;
            assert!(now < start + 10_000, "escape should have fired by now");
        }
        let resets_after_first_escape = reset_count.load(Ordering::SeqCst);
        assert_eq!(
            resets_after_first_escape, 1,
            "escape fires exactly once first"
        );

        // The escape just force-released a frame (clearing the hold clock) WITHOUT the queue ever
        // dropping below the HWM. Immediately poll again 10ms later with the queue STILL pinned and
        // a fresh releasable frame present. If the clock was reset by the force-release, this brief
        // hold (10ms) is far under the threshold and the escape must NOT re-fire.
        seq += 1;
        jb.insert_frame(create_test_frame(seq, FrameType::KeyFrame), now);
        jb.find_and_move_continuous_frames(now);
        assert_eq!(
            reset_count.load(Ordering::SeqCst),
            resets_after_first_escape,
            "escape must not re-fire on the very next tick — the force-release reset the hold clock"
        );
    }

    /// `flush()` must reset the continuous-hold clock so a hold that begins after a flush starts
    /// fresh and cannot inherit elapsed time accrued before it.
    ///
    /// Mutation coverage: if `flush()` did not clear `backpressure_hold_since_ms`, the post-flush
    /// hold would still measure elapsed from the pre-flush hold (started t=1100) -> by the t=3200
    /// poll that is 2100ms > the threshold and the escape would fire, failing the `reset_count == 0`
    /// assertion. With the clear, the post-flush hold starts at t=3000 (only 200ms by t=3200).
    #[test]
    fn flush_resets_held_clock() {
        let (mut jb, decoded_frames, queue_depth, reset_count) =
            create_test_jitter_buffer_with_queue_and_reset();

        // Healthy first release.
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), 1000);
        jb.find_and_move_continuous_frames(1100);
        assert_eq!(decoded_frames.lock().unwrap().len(), 1);

        // Begin a gated hold so the continuous-hold clock starts (at the insert poll, t=1100).
        queue_depth.store(DECODE_QUEUE_HIGH_WATER_MARK, Ordering::SeqCst);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), 1100);

        // Flush mid-hold: this must clear the hold clock.
        jb.flush();
        assert!(jb.buffered_frames.is_empty());

        // After the flush a fresh stream starts: a new keyframe arrives while the decoder is still
        // wedged. The new hold begins at the insert poll (t=3000). Poll at t=3200: with the
        // flush-clear the hold is only 200ms old (no escape); WITHOUT the clear the clock still
        // reads from the pre-flush hold at t=1100 -> 2100ms > threshold -> escape fires. So this
        // isolates the flush-clear: any escape here can only come from inherited pre-flush time.
        jb.insert_frame(create_test_frame(10, FrameType::KeyFrame), 3000);
        jb.find_and_move_continuous_frames(3200);

        assert_eq!(
            reset_count.load(Ordering::SeqCst),
            0,
            "a hold beginning after flush must not inherit pre-flush elapsed time"
        );
        // The fresh keyframe stays gated (queue still pinned), not force-released.
        assert!(jb.buffered_frames.contains_key(&10));
    }

    // --- Playout-latency metric (issue #1252) ---

    /// The total playout-latency estimate must be the SUM of both stages:
    ///   stage-1 = newest − next-to-release arrival span, and
    ///   stage-2 = decode_queue_depth() × source_frame_interval_ms.
    /// This pins both terms with known inputs and a fixed mock decode-queue depth: if either term
    /// is dropped from the sum, the asserted total no longer matches.
    #[test]
    fn playout_latency_total_spans_both_stages() {
        let (mut jb, _decoded, queue_depth) = create_test_jitter_buffer_with_queue_depth();

        // Delta-only backlog with no keyframe: nothing is released (so the source-interval estimate
        // stays at its default) and last_decoded stays None (never-decoded head selection = oldest).
        // Arrival span stays under MAX_PLAYOUT_AGE_MS so the insert-time poll can't evict it.
        jb.insert_frame(create_test_frame(10, FrameType::DeltaFrame), 1000);
        jb.insert_frame(create_test_frame(11, FrameType::DeltaFrame), 2000);
        jb.insert_frame(create_test_frame(12, FrameType::DeltaFrame), 2500);
        assert_eq!(jb.buffered_frames_len(), 3, "backlog must be retained");

        // Fixed stage-2 depth; default source interval (no releases happened).
        queue_depth.store(2, Ordering::SeqCst);
        assert_eq!(
            jb.source_frame_interval_ms(),
            DEFAULT_SOURCE_FRAME_INTERVAL_MS
        );

        let now = 3000;
        // Stage-1: newest (seq 12 @2500) − head (seq 10 @1000) = 1500ms.
        let span = jb.buffered_span_ms(now);
        assert!(
            (span - 1500.0).abs() < 1e-9,
            "stage-1 span should be 1500ms, got {span}"
        );

        // Total = stage-1 (1500) + stage-2 (2 × 33.3).
        let expected_stage2 = 2.0 * DEFAULT_SOURCE_FRAME_INTERVAL_MS;
        let total = jb.playout_latency_ms(now);
        assert!(
            (total - (1500.0 + expected_stage2)).abs() < 1e-9,
            "total should be stage1 + stage2 = {}, got {total}",
            1500.0 + expected_stage2
        );
        // Explicit guard that BOTH terms are present (kills a mutant that drops either).
        assert!((total - span - expected_stage2).abs() < 1e-9);
    }

    /// The source frame-interval estimate must track the SOURCE cadence (released-frame
    /// inter-arrival spacing), moving off its default toward the observed interval. If
    /// `record_release_cadence` never folds samples, the estimate stays at the default and this
    /// fails.
    #[test]
    fn source_frame_interval_tracks_release_cadence() {
        let (mut jb, _decoded) = create_test_jitter_buffer();

        // Keyframe + continuous deltas spaced 50ms apart in arrival time; all release in order.
        jb.insert_frame(create_test_frame(1, FrameType::KeyFrame), 1000);
        jb.insert_frame(create_test_frame(2, FrameType::DeltaFrame), 1050);
        jb.insert_frame(create_test_frame(3, FrameType::DeltaFrame), 1100);
        jb.insert_frame(create_test_frame(4, FrameType::DeltaFrame), 1150);
        jb.find_and_move_continuous_frames(1300);

        let est = jb.source_frame_interval_ms();
        assert!(
            est > DEFAULT_SOURCE_FRAME_INTERVAL_MS + 1.0,
            "estimate should rise off the {DEFAULT_SOURCE_FRAME_INTERVAL_MS}ms default toward the 50ms cadence, got {est}"
        );
        assert!(
            est < 50.0,
            "EWMA must not overshoot the observed 50ms interval, got {est}"
        );
    }

    /// `buffered_span_ms` must select the next-to-release head the same way the release path does:
    /// across a gap (next continuous seq not buffered) it falls back to the OLDEST buffered frame,
    /// and the span is measured from THAT frame to the newest. Backpressure pins the decoder so no
    /// frame is released during setup, isolating the head-selection logic.
    #[test]
    fn buffered_span_head_selection_handles_gap() {
        let (mut jb, _decoded, queue_depth) = create_test_jitter_buffer_with_queue_depth();
        queue_depth.store(DECODE_QUEUE_HIGH_WATER_MARK, Ordering::SeqCst); // block all releases

        // Mid-stream (last decoded = 5), then a GAP: seq 6 never arrives. Two later frames buffer.
        jb.last_decoded_sequence_number = Some(5);
        jb.insert_frame(create_test_frame(8, FrameType::DeltaFrame), 1000);
        jb.insert_frame(create_test_frame(12, FrameType::DeltaFrame), 1300);

        // Head falls back to the oldest buffered frame (seq 8 @1000); newest is seq 12 @1300.
        let span = jb.buffered_span_ms(1300);
        assert!(
            (span - 300.0).abs() < 1e-9,
            "gap head selection should span 1300−1000=300ms, got {span}"
        );
    }

    /// Stage-3 paint lag (#1252): `paint_lag_ms` must equal
    /// `(emitted − painted) × source_frame_interval_ms`. The asserted value is computed from a
    /// DIFFERENT source than the function so the test is not a tautology, and it is constructed so
    /// it FAILS if any operand is dropped or the interval term is removed (mutation-resistant):
    ///   - emitted=10, painted=4 => outstanding=6; interval=33.0 => 198.0.
    ///     If the code drops `painted`        => 10×33 = 330 ≠ 198 (fails).
    ///     If the code drops the interval term=> 6           ≠ 198 (fails).
    ///     If the code uses + instead of ×     => 6+33 = 39  ≠ 198 (fails).
    #[test]
    fn paint_lag_ms_is_outstanding_times_interval() {
        let emitted: u64 = 10;
        let painted: u64 = 4;
        let interval = 33.0_f64;
        let expected = (emitted - painted) as f64 * interval; // 6 × 33 = 198.0
        let got = paint_lag_ms(emitted, painted, interval);
        assert!(
            (got - expected).abs() < 1e-9,
            "paint_lag_ms({emitted},{painted},{interval}) should be {expected}, got {got}"
        );
        // Guard: a non-trivial interval term must actually scale the result.
        assert!(
            got > (emitted - painted) as f64,
            "interval term must scale, got {got}"
        );
    }

    /// `paint_lag_ms` must floor the outstanding frame count at 0 (saturating_sub) so a transient
    /// ACK overshoot — painted briefly exceeding emitted, e.g. just after a reset while old
    /// in-flight frames are still painted — reads as "at live" (0.0) rather than a huge value from
    /// u64 wraparound. Fails if `saturating_sub` is replaced with a wrapping/plain subtraction.
    #[test]
    fn paint_lag_ms_saturates_when_painted_exceeds_emitted() {
        let got = paint_lag_ms(3, 7, 33.0);
        assert_eq!(
            got, 0.0,
            "painted > emitted must floor to 0.0 (at live), got {got}"
        );
    }

    // --- Resync-to-live governor (issue #1252, v1) ---

    /// Insert a `FrameBuffer` directly into the jitter buffer at a controlled arrival time. Used by
    /// the governor tests to build precise latency state (a known head/newest arrival span) without
    /// the side effects of `insert_frame` (which re-derives the playout target and runs a poll).
    fn buffer_frame_at(
        jb: &mut JitterBuffer<crate::decoder::DecodedFrame>,
        seq: u64,
        frame_type: FrameType,
        arrival_ms: u128,
    ) {
        jb.buffered_frames.insert(
            seq,
            FrameBuffer::new(create_test_frame(seq, frame_type), arrival_ms),
        );
    }

    /// Build the canonical governor backlog directly: mid-stream (last decoded = 1) behind a gap
    /// (seq 2 is missing), a few stale deltas, and a NEWEST keyframe positioned so the buffered
    /// arrival span equals 1300ms (>= `GOVERNOR_ENGAGE_MS`) while the head age stays below the
    /// 1800ms freshness deadline across the poll window. The playout target is pinned absurdly high
    /// so the normal release gate never drains the backlog mid-test — only the governor (or, if we
    /// crossed it, the deadline) can change the buffer. `head_arrival` anchors the head; the newest
    /// keyframe arrives 1300ms later. Returns the keyframe sequence number.
    fn build_governor_backlog(
        jb: &mut JitterBuffer<crate::decoder::DecodedFrame>,
        head_arrival: u128,
        first_seq: u64,
    ) -> u64 {
        jb.buffered_frames.clear();
        jb.last_decoded_sequence_number = Some(1);
        // Pin the target so the release gate cannot drain the backlog during the sustain window.
        jb.target_playout_delay_ms = 100_000.0;
        let key_seq = first_seq + 5;
        buffer_frame_at(jb, first_seq, FrameType::DeltaFrame, head_arrival);
        buffer_frame_at(jb, first_seq + 1, FrameType::DeltaFrame, head_arrival + 100);
        buffer_frame_at(jb, first_seq + 2, FrameType::DeltaFrame, head_arrival + 200);
        // Newest keyframe at head_arrival + 1300 → buffered arrival span == 1300ms.
        buffer_frame_at(jb, key_seq, FrameType::KeyFrame, head_arrival + 1300);
        key_seq
    }

    /// (a) The governor must engage after the latency total has stayed at/above
    /// `GOVERNOR_ENGAGE_MS` for `GOVERNOR_SUSTAIN_TICKS` consecutive ticks, and on engaging it must
    /// skip the stale backlog to the NEWEST buffered keyframe (dropping the deltas before it) so the
    /// buffered span collapses back toward live.
    ///
    /// Mutation coverage: removing the skip (the `skip_to_newest_buffered_keyframe` call / the
    /// `governor_skips += 1`) leaves the count at 0 — first assert fails. Lowering
    /// `GOVERNOR_SUSTAIN_TICKS` so it fires earlier changes when the deltas drop but is also caught
    /// by test (b)'s 39-tick sub-case; here, dropping the skip entirely fails both the count and the
    /// "deltas dropped"/"span collapsed" asserts.
    #[test]
    fn governor_engages_after_sustained_high_latency_and_skips_to_newest_keyframe() {
        let (mut jb, _decoded) = create_test_jitter_buffer();
        let head_arrival = 100u128;
        let key_seq = build_governor_backlog(&mut jb, head_arrival, 5);

        assert_eq!(jb.governor_skip_count(), 0, "no skips before the window");

        // Poll the head age band [1350, 1740) — always >= GOVERNOR_ENGAGE_MS (1300) via the span,
        // always < MAX_PLAYOUT_AGE_MS (1800) so the deadline never fires. The keyframe arrived at
        // head_arrival + 1300 = 1400 <= the first poll time (1450), so the release gate's
        // arrival-subtraction can never underflow.
        let mut now = head_arrival + 1350; // 1450
        for _ in 0..GOVERNOR_SUSTAIN_TICKS {
            // Latency total must sit at the engage threshold throughout the sustain window.
            assert!(
                jb.playout_latency_parts_ms(now).0 >= GOVERNOR_ENGAGE_MS,
                "setup invariant: latency must stay >= engage threshold (now={now})"
            );
            jb.find_and_move_continuous_frames(now);
            now += 10;
        }

        assert_eq!(
            jb.governor_skip_count(),
            1,
            "governor must skip exactly once after the sustained window"
        );
        // The deltas before the newest keyframe were dropped by the skip.
        assert!(
            !jb.buffered_frames.contains_key(&5)
                && !jb.buffered_frames.contains_key(&6)
                && !jb.buffered_frames.contains_key(&7),
            "stale deltas before the newest keyframe must be dropped"
        );
        assert!(
            jb.buffered_frames.contains_key(&key_seq),
            "the newest keyframe must be preserved as the resync target"
        );
        assert_eq!(
            jb.get_dropped_frames_count(),
            3,
            "exactly the three deltas before the keyframe were dropped"
        );
        // Span collapsed toward live: with only the keyframe left, the buffered span is 0.
        assert_eq!(
            jb.buffered_span_ms(now),
            0.0,
            "playout latency span must collapse after the resync skip"
        );
    }

    /// (b) The governor must NOT engage when the latency total is below `GOVERNOR_ENGAGE_MS`, and
    /// must NOT engage when the total is high but sustained for FEWER than `GOVERNOR_SUSTAIN_TICKS`
    /// ticks.
    ///
    /// Mutation coverage: the 39-tick sub-case fails if the sustain counter is made to fire at
    /// fewer than `GOVERNOR_SUSTAIN_TICKS` (e.g. `< SUSTAIN` weakened to `<= SUSTAIN - 1` off-by-one
    /// the other way, or the threshold lowered). The low-latency sub-case fails if the engage
    /// threshold is removed or set too low.
    #[test]
    fn governor_does_not_engage_below_engage_threshold() {
        // Sub-case 1: latency comfortably below the engage threshold (span ~600ms).
        {
            let (mut jb, _decoded) = create_test_jitter_buffer();
            jb.last_decoded_sequence_number = Some(1);
            jb.target_playout_delay_ms = 100_000.0; // never release during the test
            buffer_frame_at(&mut jb, 5, FrameType::DeltaFrame, 100);
            buffer_frame_at(&mut jb, 10, FrameType::KeyFrame, 700); // span 600

            // Poll past the sustain window (proving non-engagement is not a too-short window) while
            // keeping the head age below the 1800ms freshness deadline so the deadline cannot drop
            // anything either.
            let mut now = 760u128; // head age 660; keyframe arrival 700 <= now
            for _ in 0..(GOVERNOR_SUSTAIN_TICKS + 4) {
                let total = jb.playout_latency_parts_ms(now).0;
                assert!(
                    total < GOVERNOR_ENGAGE_MS,
                    "setup invariant: total must stay below engage (now={now}, total={total})"
                );
                assert!(
                    ((now - 100) as f64) < MAX_PLAYOUT_AGE_MS,
                    "setup invariant: head age must stay below the freshness deadline (now={now})"
                );
                jb.find_and_move_continuous_frames(now);
                now += 10;
            }
            assert_eq!(
                jb.governor_skip_count(),
                0,
                "governor must not engage below the engage threshold"
            );
            assert_eq!(
                jb.get_dropped_frames_count(),
                0,
                "nothing may be dropped when the governor never engages and latency is low"
            );
        }

        // Sub-case 2: latency >= engage threshold, but only for SUSTAIN_TICKS - 1 ticks.
        {
            let (mut jb, _decoded) = create_test_jitter_buffer();
            build_governor_backlog(&mut jb, 100, 5);
            let mut now = 1450u128;
            for _ in 0..(GOVERNOR_SUSTAIN_TICKS - 1) {
                jb.find_and_move_continuous_frames(now);
                now += 10;
            }
            assert_eq!(
                jb.governor_skip_count(),
                0,
                "governor must not skip before reaching GOVERNOR_SUSTAIN_TICKS consecutive ticks"
            );
            assert_eq!(
                jb.governor_sustain_ticks,
                GOVERNOR_SUSTAIN_TICKS - 1,
                "the sustain counter must have accumulated to exactly SUSTAIN_TICKS - 1"
            );
        }
    }

    /// (c) THE no-PLI / storm-avoidance invariant. With a sustained high latency total but NO
    /// buffered keyframe to skip to, the governor must take ZERO action: no skip, no eviction, and
    /// crucially NO keyframe request. The keyframe-less case belongs entirely to the 1800ms
    /// freshness deadline, which we deliberately keep below its threshold so it does not fire either.
    ///
    /// Mutation coverage: if the governor ever fired a keyframe request in the no-keyframe case the
    /// request-count assert fails; if it ever evicted deltas the "deltas retained" assert fails; if
    /// it ever counted a skip the skip-count assert fails.
    #[test]
    fn governor_does_nothing_with_no_buffered_keyframe() {
        let requests = Arc::new(AtomicU32::new(0));
        let decoded = Arc::new(Mutex::new(Vec::new()));
        let req = requests.clone();
        let mut jb = JitterBuffer::with_keyframe_request(
            Box::new(MockDecoder::new_with_vec(decoded.clone())),
            Box::new(move || {
                req.fetch_add(1, Ordering::SeqCst);
            }),
        );

        // Mid-stream, behind a gap, with ONLY deltas buffered (no keyframe). Span == 1300ms so the
        // governor's latency trigger qualifies, while the head age stays below 1800ms so the
        // freshness deadline does NOT fire and cannot be confused for governor action.
        jb.last_decoded_sequence_number = Some(1);
        jb.target_playout_delay_ms = 100_000.0;
        buffer_frame_at(&mut jb, 5, FrameType::DeltaFrame, 100);
        buffer_frame_at(&mut jb, 6, FrameType::DeltaFrame, 200);
        buffer_frame_at(&mut jb, 7, FrameType::DeltaFrame, 1400); // span 1300, no keyframe

        // Poll past the sustain window while keeping the head age below the 1800ms freshness
        // deadline, so neither the governor (no keyframe) nor the deadline (sub-threshold) can act.
        let mut now = 1450u128;
        for _ in 0..(GOVERNOR_SUSTAIN_TICKS + 4) {
            assert!(
                jb.playout_latency_parts_ms(now).0 >= GOVERNOR_ENGAGE_MS,
                "setup invariant: total must qualify (now={now})"
            );
            assert!(
                ((now - 100) as f64) < MAX_PLAYOUT_AGE_MS,
                "setup invariant: head age must stay below the freshness deadline (now={now})"
            );
            jb.find_and_move_continuous_frames(now);
            now += 10;
        }

        assert_eq!(
            jb.governor_skip_count(),
            0,
            "governor must take no action when there is no buffered keyframe"
        );
        assert_eq!(
            requests.load(Ordering::SeqCst),
            0,
            "governor must NEVER fire a keyframe request (v1 introduces zero new PLI sources)"
        );
        assert!(
            jb.buffered_frames.contains_key(&5)
                && jb.buffered_frames.contains_key(&6)
                && jb.buffered_frames.contains_key(&7),
            "governor must not evict the keyframe-less backlog (that case belongs to the deadline)"
        );
        assert_eq!(
            jb.get_dropped_frames_count(),
            0,
            "nothing may be dropped: neither the governor nor the (sub-threshold) deadline acted"
        );
    }

    /// (d) After a governor skip, a second skip must be suppressed until `GOVERNOR_COOLDOWN_MS` of
    /// wall-clock time has elapsed since the first, even if a fresh qualifying backlog re-accumulates
    /// and the sustain window is satisfied again. Once past the cooldown, the governor may skip again.
    ///
    /// Mutation coverage: removing the cooldown guard lets the within-window re-accumulation skip
    /// immediately, failing the `== 1` assert; the post-cooldown phase then proves the guard is a
    /// cooldown (time-bounded) and not a permanent lockout via the `== 2` assert.
    #[test]
    fn governor_cooldown_prevents_second_skip_within_window() {
        let (mut jb, _decoded) = create_test_jitter_buffer();

        // Phase 1: engage skip #1 at t = first poll + window. Head anchored at 100.
        build_governor_backlog(&mut jb, 100, 5);
        let mut now = 1450u128;
        for _ in 0..GOVERNOR_SUSTAIN_TICKS {
            jb.find_and_move_continuous_frames(now);
            now += 10;
        }
        assert_eq!(jb.governor_skip_count(), 1, "first skip must fire");
        let first_skip_at = jb
            .governor_last_skip_ms
            .expect("first skip records its time");

        // Phase 2: rebuild a fresh qualifying backlog and poll a FULL sustain window, but entirely
        // WITHIN GOVERNOR_COOLDOWN_MS of the first skip. The head is re-anchored so its age stays
        // below the freshness deadline at these later poll times.
        build_governor_backlog(&mut jb, 600, 20);
        let mut now2 = 1900u128;
        for _ in 0..(GOVERNOR_SUSTAIN_TICKS + 5) {
            assert!(
                ((now2 - first_skip_at) as f64) < GOVERNOR_COOLDOWN_MS,
                "phase-2 polls must stay inside the cooldown window (now2={now2})"
            );
            jb.find_and_move_continuous_frames(now2);
            now2 += 10;
        }
        assert_eq!(
            jb.governor_skip_count(),
            1,
            "cooldown must suppress a second skip within GOVERNOR_COOLDOWN_MS"
        );

        // Phase 3: rebuild again with a fresh head and poll PAST the cooldown. The sustain counter is
        // already saturated from phase 2, so the first qualifying post-cooldown tick skips.
        build_governor_backlog(&mut jb, 3000, 40);
        let mut now3 = 4350u128; // 4350 - 1840 = 2510 >= 2500 cooldown; head age 1350 < 1800
        for _ in 0..GOVERNOR_SUSTAIN_TICKS {
            jb.find_and_move_continuous_frames(now3);
            now3 += 10;
            if jb.governor_skip_count() == 2 {
                break;
            }
        }
        assert_eq!(
            jb.governor_skip_count(),
            2,
            "past the cooldown a fresh qualifying backlog must skip again"
        );
        assert!(
            jb.governor_last_skip_ms
                .expect("second skip records its time")
                >= 4350,
            "the second skip's cooldown anchor must advance to the post-cooldown time"
        );
    }

    /// (e) Disengage hysteresis: once engaged, the latch must clear when the latency total falls
    /// below `GOVERNOR_DISENGAGE_MS`, and the sustain counter must reset when the total drops below
    /// `GOVERNOR_ENGAGE_MS`.
    ///
    /// Mutation coverage: removing the `total < GOVERNOR_DISENGAGE_MS` disengage clause leaves the
    /// latch stuck `true`, failing the `!governor_engaged` assert.
    #[test]
    fn governor_disengage_hysteresis_clears_latch() {
        let (mut jb, _decoded) = create_test_jitter_buffer();

        // Engage: latch becomes true after a skip.
        build_governor_backlog(&mut jb, 100, 5);
        let mut now = 1450u128;
        for _ in 0..GOVERNOR_SUSTAIN_TICKS {
            jb.find_and_move_continuous_frames(now);
            now += 10;
        }
        assert_eq!(jb.governor_skip_count(), 1, "must engage to set the latch");
        assert!(jb.governor_engaged, "latch must be set after engaging");

        // Drive the total below GOVERNOR_DISENGAGE_MS (450): a tiny fresh backlog (span 100ms).
        jb.buffered_frames.clear();
        jb.last_decoded_sequence_number = Some(1);
        jb.target_playout_delay_ms = 100_000.0;
        buffer_frame_at(&mut jb, 50, FrameType::DeltaFrame, 3000);
        buffer_frame_at(&mut jb, 51, FrameType::DeltaFrame, 3100); // span 100
        let low_now = 3150u128; // head age 150; total = min(100, 150) = 100 < 450
        assert!(
            jb.playout_latency_parts_ms(low_now).0 < GOVERNOR_DISENGAGE_MS,
            "setup invariant: total must be below the disengage threshold"
        );
        jb.find_and_move_continuous_frames(low_now);

        assert!(
            !jb.governor_engaged,
            "disengage hysteresis must clear the latch once total < GOVERNOR_DISENGAGE_MS"
        );
        assert_eq!(
            jb.governor_sustain_ticks, 0,
            "the sustain counter must reset once total < GOVERNOR_ENGAGE_MS"
        );
    }

    /// (f) `flush()` resets the governor's RUNTIME state (sustain counter, engaged latch, last-skip
    /// anchor) but must NOT reset the LIFETIME `governor_skips` counter, which is a cumulative metric.
    ///
    /// Mutation coverage: resetting `governor_skips` in `flush()` fails the `== 1` assert; failing
    /// to reset any of `governor_sustain_ticks` / `governor_engaged` / `governor_last_skip_ms` fails
    /// the corresponding runtime-state assert.
    #[test]
    fn flush_resets_governor_runtime_state_but_not_lifetime_count() {
        let (mut jb, _decoded) = create_test_jitter_buffer();

        build_governor_backlog(&mut jb, 100, 5);
        let mut now = 1450u128;
        for _ in 0..GOVERNOR_SUSTAIN_TICKS {
            jb.find_and_move_continuous_frames(now);
            now += 10;
        }
        assert_eq!(jb.governor_skip_count(), 1, "must engage once before flush");
        assert!(jb.governor_engaged, "latch set before flush");
        assert!(
            jb.governor_last_skip_ms.is_some(),
            "last-skip set before flush"
        );

        jb.flush();

        assert_eq!(
            jb.governor_skip_count(),
            1,
            "flush must NOT reset the lifetime governor skip count"
        );
        assert_eq!(
            jb.governor_sustain_ticks, 0,
            "flush must reset the sustain counter"
        );
        assert!(!jb.governor_engaged, "flush must clear the engaged latch");
        assert!(
            jb.governor_last_skip_ms.is_none(),
            "flush must clear the last-skip anchor"
        );
    }
}

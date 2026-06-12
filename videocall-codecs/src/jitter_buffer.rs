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
/// A multiplier applied to the jitter estimate to provide a safety margin.
/// A value of 3.0 means we buffer enough to handle jitter up to 3x the running average.
const JITTER_MULTIPLIER: f64 = 3.0;
/// A smoothing factor for delay updates to prevent rapid, jarring changes.
const DELAY_SMOOTHING_FACTOR: f64 = 0.99;

/// The maximum number of frames the buffer will hold before rejecting new ones.
const MAX_BUFFER_SIZE: usize = 200;
// From libwebrtc's jitter_buffer_common.h
const MAX_CONSECUTIVE_OLD_FRAMES: u64 = 300;
/// If an incoming keyframe is this many sequence numbers behind the last decoded frame, we assume
/// the stream restarted (e.g., camera switch) and flush immediately. Smaller rollbacks are treated
/// as harmless reordering.
const STREAM_RESTART_BACKTRACK_THRESHOLD: u64 = 30;

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

    // --- Decoder Interface ---
    /// The abstract decoder that will receive frames ready for decoding.
    decoder: Box<dyn Decodable<Frame = T>>,
}

impl<T> JitterBuffer<T> {
    pub fn new(decoder: Box<dyn Decodable<Frame = T>>) -> Self {
        Self {
            buffered_frames: BTreeMap::new(),
            last_decoded_sequence_number: None,
            jitter_estimator: JitterEstimator::new(),
            target_playout_delay_ms: MIN_PLAYOUT_DELAY_MS,
            dropped_frames_count: 0,
            num_consecutive_old_frames: 0,
            decoder,
        }
    }

    /// Returns the current number of frames buffered and waiting in the jitter buffer.
    pub fn buffered_frames_len(&self) -> usize {
        self.buffered_frames.len()
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

            let next_decodable_key: Option<u64> = if let Some(last_seq) =
                self.last_decoded_sequence_number
            {
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
                        log::trace!(
                            "[JB_POLL] Gap after {last_seq}. No subsequent keyframe found."
                        );
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
            };

            if let Some(key) = next_decodable_key {
                if let Some(frame) = self.buffered_frames.get(&key) {
                    let time_in_buffer_ms = (current_time_ms - frame.arrival_time_ms) as f64;

                    let is_ready = time_in_buffer_ms >= self.target_playout_delay_ms;
                    log::trace!(
                        "[JB_POLL] Candidate {key}: Time in buffer: {time_in_buffer_ms:.2}ms, Target: {:.2}ms -> Ready: {is_ready}",
                        self.target_playout_delay_ms
                    );

                    if is_ready {
                        let frame_to_move = self.buffered_frames.remove(&key).unwrap();

                        // If we're jumping to a keyframe to recover, drop everything before it.
                        if frame_to_move.is_keyframe() {
                            let is_first_frame = self.last_decoded_sequence_number.is_none();
                            let is_gap_recovery = self
                                .last_decoded_sequence_number
                                .is_some_and(|last_seq| key > last_seq + 1);

                            if is_first_frame || is_gap_recovery {
                                log::debug!(
                                    "[JITTER_BUFFER] Keyframe {key} recovery. Dropping frames before it."
                                );
                                self.drop_frames_before(key);
                            }
                        }

                        self.push_to_decoder(frame_to_move);
                        self.last_decoded_sequence_number = Some(key);
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

        // Backlog is stale. Find the NEWEST buffered keyframe to skip to so we land as close to
        // live as possible while still resuming from a self-contained frame.
        let newest_keyframe = self
            .buffered_frames
            .iter()
            .rev()
            .find(|(_, f)| f.is_keyframe())
            .map(|(&s, _)| s);

        match newest_keyframe {
            Some(keyframe_seq) => {
                // Drop the stale delta/keyframe backlog before the chosen keyframe. The keyframe
                // itself is preserved and will be released by the normal gate on the next loop
                // iteration. Because we resume from a keyframe, `find_and_move_continuous_frames`
                // treats it as gap/restart recovery and decoding continues cleanly.
                //
                // CRITICAL termination guard: `drop_frames_before(seq)` only removes keys strictly
                // `< seq`, so when the chosen keyframe is ALREADY the head of the buffer (nothing
                // before it) it removes nothing. If we returned `true` unconditionally in that case,
                // the caller would `continue`, re-enter with an unchanged buffer, and spin forever
                // (the 10ms worker tick → tab freeze). This is reachable: e.g. background-tab
                // throttling clamps/pauses `setInterval`, so on refocus the first tick's
                // `Date::now()` delta is multi-second and a buffered post-gap keyframe is instantly
                // ≥ MAX_PLAYOUT_AGE_MS old while sitting at the head. So we only report progress
                // (return `true`) when we actually shrank the buffer. When 0 were dropped, return
                // `false` and let the normal CASE-2 (gap) / CASE-3 (never-decoded) path select this
                // head keyframe and release it through the normal gate — which advances
                // `last_decoded_sequence_number` correctly.
                let dropped_before = self.dropped_frames_count;
                self.drop_frames_before(keyframe_seq);
                let dropped_any = self.dropped_frames_count > dropped_before;
                if dropped_any {
                    log::debug!(
                        "[JITTER_BUFFER] Freshness deadline exceeded (head age {head_age_ms:.0}ms >= {MAX_PLAYOUT_AGE_MS:.0}ms). Skipped to live keyframe {keyframe_seq}, dropped {} stale frame(s).",
                        self.dropped_frames_count - dropped_before
                    );
                }
                dropped_any
            }
            None => {
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
                if self.dropped_frames_count > dropped_before {
                    log::debug!(
                        "[JITTER_BUFFER] Freshness deadline exceeded (head age {head_age_ms:.0}ms) with NO buffered keyframe. Evicted {} stale delta frame(s); holding last-good frame and awaiting keyframe recovery.",
                        self.dropped_frames_count - dropped_before
                    );
                }
                // TODO(#1020): trigger a PLI / keyframe request from here. The PLI mechanism lives
                // in the client crate (videocall-client: peer_decode_manager::send_keyframe_request,
                // gap detection in track_sequence), not in videocall-codecs, and the jitter buffer
                // has no handle back to it. The client already issues keyframe requests on observed
                // sequence gaps, which covers the common no-keyframe stall. A clean fix would thread
                // a `request_keyframe` callback into JitterBuffer::new (mirroring how `decoder` is
                // injected) so the codecs layer can proactively ask for a keyframe the instant it
                // evicts a stale keyframe-less backlog, rather than waiting for the client's
                // gap-driven request. Deferred to keep this change transport-agnostic and confined
                // to the buffer; the eviction above guarantees the buffer cannot grow unbounded in
                // the meantime.
                false
            }
        }
    }

    /// Pushes a single frame to the shared decodable queue.
    fn push_to_decoder(&mut self, frame: FrameBuffer) {
        let seq = frame.sequence_number();
        log::trace!("[JITTER_BUFFER] Pushing frame {seq} to decoder.");
        self.decoder.decode(frame);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::DecodedFrame;
    use crate::frame::{FrameType, VideoFrame};
    use std::sync::Arc;
    use std::sync::Mutex;

    /// A mock decoder for testing purposes. It stores decoded frames in a shared Vec.
    struct MockDecoder {
        decoded_frames: Arc<Mutex<Vec<DecodedFrame>>>,
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
    }

    impl MockDecoder {
        fn new_with_vec(decoded_frames: Arc<Mutex<Vec<DecodedFrame>>>) -> Self {
            Self { decoded_frames }
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
}

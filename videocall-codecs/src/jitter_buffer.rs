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
/// A multiplier applied to the jitter estimate to provide a safety margin.
/// A value of 3.0 means we buffer enough to handle jitter up to 3x the running average.
const JITTER_MULTIPLIER: f64 = 3.0;
/// A smoothing factor for delay updates to prevent rapid, jarring changes.
const DELAY_SMOOTHING_FACTOR: f64 = 0.99;

/// The maximum number of frames the buffer will hold before rejecting new ones.
const MAX_BUFFER_SIZE: usize = 200;

pub struct JitterBuffer {
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

    // --- Decoder Interface ---
    /// The abstract decoder that will receive frames ready for decoding.
    decoder: Box<dyn Decodable<Frame = crate::decoder::DecodedFrame>>,
}

impl JitterBuffer {
    pub fn new(decoder: Box<dyn Decodable<Frame = crate::decoder::DecodedFrame>>) -> Self {
        Self {
            buffered_frames: BTreeMap::new(),
            last_decoded_sequence_number: None,
            jitter_estimator: JitterEstimator::new(),
            target_playout_delay_ms: MIN_PLAYOUT_DELAY_MS,
            dropped_frames_count: 0,
            decoder,
        }
    }

    /// The main entry point for a new frame arriving from the network.
    pub fn insert_frame(&mut self, frame: VideoFrame, arrival_time_ms: u128) {
        let seq = frame.sequence_number;

        // --- Pre-insertion checks ---
        // 1. Ignore frames that are too old.
        if let Some(last_decoded) = self.last_decoded_sequence_number {
            if seq <= last_decoded {
                println!("[JITTER_BUFFER] Ignoring old frame: {}", seq);
                return;
            }
        }

        // 2. Check if the buffer is full.
        if self.buffered_frames.len() >= MAX_BUFFER_SIZE {
            // Allow a keyframe to clear the buffer if it's full.
            if frame.frame_type == FrameType::KeyFrame {
                println!("[JITTER_BUFFER] Buffer full, but received keyframe. Clearing buffer.");
                self.drop_all_frames();
            } else {
                println!("[JITTER_BUFFER] Buffer full. Rejecting frame: {}", seq);
                return; // Reject the frame.
            }
        }

        println!("[JITTER_BUFFER] Received frame: {}", seq);

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

        println!(
            "[JB_POLL] Checking buffer. Last decoded: {:?}, Buffer size: {}, Target delay: {:.2}ms",
            self.last_decoded_sequence_number,
            self.buffered_frames.len(),
            self.target_playout_delay_ms
        );

        loop {
            let mut found_frame_to_move = false;

            let next_decodable_key: Option<u64> =
                if let Some(last_seq) = self.last_decoded_sequence_number {
                    // CASE 1: We are in a continuous stream. Look for the next frame.
                    let next_continuous_seq = last_seq + 1;
                    if self.buffered_frames.contains_key(&next_continuous_seq) {
                        println!(
                            "[JB_POLL] Seeking next continuous frame: {}",
                            next_continuous_seq
                        );
                        Some(next_continuous_seq)
                    } else {
                        // CASE 2: Gap detected. Look for the next keyframe after the gap.
                        let keyframe = self
                            .buffered_frames
                            .iter()
                            .find(|(&s, f)| s > next_continuous_seq && f.is_keyframe())
                            .map(|(&s, _)| s);
                        if let Some(k) = keyframe {
                            println!(
                                "[JB_POLL] Gap after {}. Seeking next keyframe. Found: {}",
                                last_seq, k
                            );
                        } else {
                            println!(
                                "[JB_POLL] Gap after {}. No subsequent keyframe found.",
                                last_seq
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
                        println!("[JB_POLL] Seeking first keyframe. Found: {}", k);
                    } else {
                        println!("[JB_POLL] Seeking first keyframe. None found in buffer.");
                    }
                    keyframe
                };

            if let Some(key) = next_decodable_key {
                if let Some(frame) = self.buffered_frames.get(&key) {
                    let time_in_buffer_ms = (current_time_ms - frame.arrival_time_ms) as f64;

                    let is_ready = time_in_buffer_ms >= self.target_playout_delay_ms;
                    println!(
                        "[JB_POLL] Candidate {}: Time in buffer: {:.2}ms, Target: {:.2}ms -> Ready: {}",
                        key, time_in_buffer_ms, self.target_playout_delay_ms, is_ready
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
                                println!(
                                    "[JITTER_BUFFER] Keyframe {} recovery. Dropping frames before it.",
                                    key
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
                println!("[JB_POLL] No decodable frame found in buffer.");
            }

            if !found_frame_to_move {
                break;
            }
        }

        if frames_were_moved {
            // NOTE: No need to notify a condvar anymore. The decoder manages its own thread.
        }
    }

    /// Pushes a single frame to the shared decodable queue.
    fn push_to_decoder(&mut self, frame: FrameBuffer) {
        println!(
            "[JITTER_BUFFER] Pushing frame {} to decoder.",
            frame.sequence_number()
        );
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
            println!("[JITTER_BUFFER] Dropping stale frame: {}", key);
            self.buffered_frames.remove(&key);
        }
    }

    /// Removes all frames from the buffer. Used when a keyframe arrives and the buffer is full.
    fn drop_all_frames(&mut self) {
        let num_dropped = self.buffered_frames.len() as u64;
        self.buffered_frames.clear();
        self.dropped_frames_count += num_dropped;
        println!("[JITTER_BUFFER] Dropped all {} frames.", num_dropped);
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
    fn create_test_jitter_buffer() -> (JitterBuffer, Arc<Mutex<Vec<DecodedFrame>>>) {
        let decoded_frames = Arc::new(Mutex::new(Vec::new()));
        let mock_decoder = Box::new(MockDecoder::new_with_vec(decoded_frames.clone()));
        let jitter_buffer = JitterBuffer::new(mock_decoder);
        (jitter_buffer, decoded_frames)
    }

    fn create_test_frame(seq: u64, frame_type: FrameType) -> VideoFrame {
        VideoFrame {
            sequence_number: seq,
            frame_type,
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
}

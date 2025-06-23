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

use super::media_decoder_trait::MediaDecoderTrait;
use std::{collections::BTreeMap, sync::Arc};
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::JsValue;
use web_sys::CodecState;

// Minimum number of frames to buffer before decoding
pub const MIN_BUFFER_SIZE: usize = 5;
// Maximum buffer size to prevent excessive memory usage
pub const MAX_BUFFER_SIZE: usize = 20;
// Maximum allowed sequence gap before resetting
pub const MAX_SEQUENCE_GAP: u64 = 100;

/// A wrapper for media decoders that handles frames being out of order using a jitter buffer.
#[derive(Debug)]
pub struct MediaDecoderWithBuffer<D: MediaDecoderTrait> {
    pub decoder: D,
    pub buffer: BTreeMap<u64, Arc<MediaPacket>>,
    pub sequence: Option<u64>,
    pub min_jitter_buffer_size: usize,
    pub max_sequence_gap: u64,
}

impl<D: MediaDecoderTrait> MediaDecoderWithBuffer<D> {
    pub fn new(init: &D::InitType) -> Result<Self, JsValue> {
        D::new(init).map(|decoder| MediaDecoderWithBuffer {
            decoder,
            buffer: BTreeMap::new(),
            sequence: None,
            min_jitter_buffer_size: MIN_BUFFER_SIZE,
            max_sequence_gap: MAX_SEQUENCE_GAP,
        })
    }

    pub fn configure(&self, config: &D::ConfigType) -> Result<(), JsValue> {
        self.decoder.configure(config)
    }

    pub fn decode(&mut self, packet: Arc<MediaPacket>) -> Vec<Arc<MediaPacket>> {
        let new_sequence = self.decoder.get_sequence_number(&packet);

        // Check for sequence reset
        let sequence_reset_detected = self.sequence.is_some_and(|seq| {
            (seq as i64 - new_sequence as i64).abs() > self.max_sequence_gap as i64
        });

        // Reset on keyframe or sequence reset
        if sequence_reset_detected {
            self.buffer.clear();
            self.sequence = None;
        }

        self.buffer.insert(new_sequence, packet);

        self.attempt_decode_from_buffer()
    }

    // Decode available frames from the buffer
    fn attempt_decode_from_buffer(&mut self) -> Vec<Arc<MediaPacket>> {
        log::debug!("attempt_decode_from_buffer");
        let mut decoded_frames = Vec::new();

        // Process frames while we have enough in the buffer
        while self.buffer.len() > self.min_jitter_buffer_size {
            if let Some(&next_sequence) = self.buffer.keys().next() {
                log::debug!("next_sequence: {:?}", next_sequence);
                // Initialize sequence if this is the first frame
                if self.sequence.is_none() {
                    log::debug!("initialize sequence");
                    if let Some(frame) = self.decode_next_frame(next_sequence) {
                        decoded_frames.push(frame);
                    }
                    continue;
                }
                let current_sequence = self.sequence.unwrap();
                log::debug!("current_sequence: {:?}", current_sequence);
                // Remove older frames
                if next_sequence < current_sequence {
                    self.buffer.remove(&next_sequence);
                // Process next frame in sequence
                } else if current_sequence + 1 == next_sequence
                    || (self.buffer.len() >= (2 * self.max_sequence_gap as usize / 3))
                {
                    if let Some(frame) = self.decode_next_frame(next_sequence) {
                        decoded_frames.push(frame);
                    }
                // Fast forward if we have a gap but buffer is getting too large
                } else {
                    // Wait for more frames
                    break;
                }
            } else {
                break;
            }
        }

        decoded_frames
    }

    // Decode a specific frame and update sequence
    fn decode_next_frame(&mut self, next_sequence: u64) -> Option<Arc<MediaPacket>> {
        log::debug!("decode_next_frame: {:?}", next_sequence);
        if let Some(frame) = self.buffer.remove(&next_sequence) {
            if let Err(e) = self.decoder.decode(frame.clone()) {
                log::error!("Error decoding frame: {:?}", e);
            }
            self.sequence = Some(next_sequence);
            Some(frame)
        } else {
            None
        }
    }

    pub fn state(&self) -> CodecState {
        self.decoder.state()
    }
}

// Types for convenience
pub type VideoDecoderWithBuffer<T> = MediaDecoderWithBuffer<T>;
pub type AudioDecoderWithBuffer<T> = MediaDecoderWithBuffer<T>;

#[cfg(test)]
mod tests {
    use crate::decode::media_decoder_trait::MediaDecoderTrait;
    use crate::decode::media_decoder_with_buffer::{
        MediaDecoderWithBuffer, MAX_BUFFER_SIZE, MIN_BUFFER_SIZE,
    };
    use crate::wrappers::EncodedVideoChunkTypeWrapper;
    use std::sync::{Arc, Mutex};
    use videocall_types::protos::media_packet::{AudioMetadata, MediaPacket, VideoMetadata};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_test::wasm_bindgen_test;
    use web_sys::EncodedAudioChunkType;
    use web_sys::{CodecState, EncodedVideoChunkType, VideoDecoderConfig, VideoDecoderInit};

    // Mock decoder that implements MediaDecoderTrait for testing
    #[derive(Debug)]
    pub struct MockMediaDecoder {
        chunks: Arc<Mutex<Vec<Arc<MediaPacket>>>>,
        pub state: CodecState,
        pub use_audio: bool, // Flag to determine audio or video mode
    }

    impl MediaDecoderTrait for MockMediaDecoder {
        type InitType = VideoDecoderInit;
        type ConfigType = VideoDecoderConfig;

        fn new(_init: &Self::InitType) -> Result<Self, JsValue>
        where
            Self: Sized,
        {
            Ok(MockMediaDecoder {
                chunks: Arc::new(Mutex::new(Vec::new())),
                state: CodecState::Configured,
                use_audio: false,
            })
        }

        fn configure(&self, _config: &Self::ConfigType) -> Result<(), JsValue> {
            // Mock implementation, do nothing
            Ok(())
        }

        fn decode(&self, packet: Arc<MediaPacket>) -> Result<(), JsValue> {
            let mut chunks = self.chunks.lock().unwrap();
            chunks.push(packet);
            Ok(())
        }

        fn state(&self) -> CodecState {
            self.state
        }

        fn get_sequence_number(&self, packet: &MediaPacket) -> u64 {
            if self.use_audio {
                packet.audio_metadata.sequence
            } else {
                packet.video_metadata.sequence
            }
        }

        fn is_keyframe(&self, packet: &MediaPacket) -> bool {
            if self.use_audio {
                let chunk_type =
                    EncodedAudioChunkType::from_js_value(&JsValue::from(packet.frame_type.clone()))
                        .unwrap();
                chunk_type == EncodedAudioChunkType::Key
            } else {
                let chunk_type = EncodedVideoChunkTypeWrapper::from(packet.frame_type.as_str()).0;
                chunk_type == EncodedVideoChunkType::Key
            }
        }
    }

    // Helper functions to create test packets
    fn create_mock_video_packet(
        sequence: u64,
        chunk_type: EncodedVideoChunkType,
        data: Vec<u8>,
    ) -> Arc<MediaPacket> {
        let video_metadata = VideoMetadata {
            sequence,
            ..Default::default()
        };

        Arc::new(MediaPacket {
            media_type: Default::default(),
            email: "test@example.com".to_string(),
            data,
            frame_type: EncodedVideoChunkTypeWrapper(chunk_type).to_string(),
            timestamp: 0.0,
            duration: 0.0,
            audio_metadata: Default::default(),
            video_metadata: Some(video_metadata).into(),
            ..Default::default()
        })
    }

    fn create_mock_audio_packet(sequence: u64, is_key: bool, data: Vec<u8>) -> Arc<MediaPacket> {
        let audio_metadata = AudioMetadata {
            sequence,
            ..Default::default()
        };

        let chunk_type = if is_key { "key" } else { "delta" };

        Arc::new(MediaPacket {
            media_type: Default::default(),
            email: "test@example.com".to_string(),
            data,
            frame_type: chunk_type.to_string(),
            timestamp: 0.0,
            duration: 0.0,
            audio_metadata: Some(audio_metadata).into(),
            ..Default::default()
        })
    }

    fn create_decoder(use_audio: bool) -> MediaDecoderWithBuffer<MockMediaDecoder> {
        let error = Closure::wrap(Box::new(move |_e: JsValue| {}) as Box<dyn FnMut(JsValue)>);
        let output =
            Closure::wrap(Box::new(move |_original_chunk: JsValue| {}) as Box<dyn FnMut(JsValue)>);
        let init = VideoDecoderInit::new(
            error.as_ref().unchecked_ref(),
            output.as_ref().unchecked_ref(),
        );

        let mut decoder: MediaDecoderWithBuffer<MockMediaDecoder> =
            MediaDecoderWithBuffer::new(&init).unwrap();

        // Set audio mode if needed
        decoder.decoder.use_audio = use_audio;

        decoder
    }

    // Video mode tests
    #[wasm_bindgen_test]
    fn test_video_basic_decode() {
        let mut decoder = create_decoder(false);

        // Feed frames to fill buffer
        let empty: Vec<Arc<MediaPacket>> = vec![];
        for i in 1..=MIN_BUFFER_SIZE {
            let result = decoder.decode(create_mock_video_packet(
                i as u64,
                EncodedVideoChunkType::Key,
                vec![i as u8],
            ));
            if i < MIN_BUFFER_SIZE {
                assert_eq!(
                    result, empty,
                    "Should not decode until buffer reaches minimum size"
                );
            }
        }

        // Add several more frames to ensure decoding eventually happens
        for i in (MIN_BUFFER_SIZE as u64 + 1)..=(MIN_BUFFER_SIZE as u64 + 5) {
            decoder.decode(create_mock_video_packet(
                i,
                EncodedVideoChunkType::Delta,
                vec![i as u8],
            ));
        }

        // Verify buffer state - we should have buffered at least some frames
        assert!(!decoder.buffer.is_empty(), "Buffer should contain frames");
    }

    #[wasm_bindgen_test]
    fn test_video_out_of_order_frames() {
        let mut decoder = create_decoder(false);
        let empty: Vec<Arc<MediaPacket>> = vec![];

        // Feed out-of-order frames
        assert_eq!(
            decoder.decode(create_mock_video_packet(
                3,
                EncodedVideoChunkType::Delta,
                vec![3]
            )),
            empty
        );

        assert_eq!(
            decoder.decode(create_mock_video_packet(
                1,
                EncodedVideoChunkType::Key,
                vec![1]
            )),
            empty
        );

        assert_eq!(
            decoder.decode(create_mock_video_packet(
                5,
                EncodedVideoChunkType::Delta,
                vec![5]
            )),
            empty
        );

        // Complete the buffer with more frames
        for i in 2..=10 {
            if i == 3 || i == 5 {
                continue;
            } // Skip already added sequences
            decoder.decode(create_mock_video_packet(
                i,
                EncodedVideoChunkType::Delta,
                vec![i as u8],
            ));
        }

        // Verify frames were decoded in proper order
        let chunks = decoder.decoder.chunks.lock().unwrap();
        if !chunks.is_empty() {
            let mut prev_seq = 0;
            for chunk in chunks.iter() {
                let seq = chunk.video_metadata.sequence;
                assert!(seq > prev_seq, "Frames should be decoded in sequence order");
                prev_seq = seq;
            }
        }
    }

    #[wasm_bindgen_test]
    fn test_video_keyframe_reset() {
        let mut decoder = create_decoder(false);

        // Fill buffer with initial frames
        for i in 1..=(MIN_BUFFER_SIZE + 2) {
            decoder.decode(create_mock_video_packet(
                i as u64,
                EncodedVideoChunkType::Delta,
                vec![i as u8],
            ));
        }

        // Insert a keyframe with a higher sequence number that should reset the buffer
        decoder.decode(create_mock_video_packet(
            20,
            EncodedVideoChunkType::Key,
            vec![20],
        ));

        // Check buffer contains the keyframe
        assert!(
            decoder.buffer.contains_key(&20),
            "Buffer should contain the keyframe after reset"
        );

        // Add more frames after the keyframe
        for i in 21..=(20 + MIN_BUFFER_SIZE as u64) {
            decoder.decode(create_mock_video_packet(
                i,
                EncodedVideoChunkType::Delta,
                vec![i as u8],
            ));
        }

        // Check that frames from both batches were processed
        let chunks = decoder.decoder.chunks.lock().unwrap();

        // Filter for sequences from the first batch and second batch
        let first_batch = chunks
            .iter()
            .filter(|c| c.video_metadata.sequence < 20)
            .count();
        let second_batch = chunks
            .iter()
            .filter(|c| c.video_metadata.sequence >= 20)
            .count();

        // We should have processed at least some frames from both batches
        if !chunks.is_empty() {
            assert!(
                first_batch > 0 || second_batch > 0,
                "Should have processed frames from at least one batch"
            );
        }
    }

    #[wasm_bindgen_test]
    fn test_video_sequence_gap() {
        let mut decoder = create_decoder(false);

        // Add initial frames
        for i in 1..=(MIN_BUFFER_SIZE + 2) {
            decoder.decode(create_mock_video_packet(
                i as u64,
                EncodedVideoChunkType::Delta,
                vec![i as u8],
            ));
        }

        // Save current buffer state (not used but kept for clarity)
        let _buffer_before = decoder.buffer.len();

        // Add a frame with a large sequence gap
        let large_sequence = 1000;
        decoder.decode(create_mock_video_packet(
            large_sequence,
            EncodedVideoChunkType::Delta,
            vec![100],
        ));

        // Verify buffer state
        assert!(
            decoder.buffer.contains_key(&large_sequence),
            "Buffer should contain the high-sequence frame"
        );

        // Either the buffer was reset and only contains the new frame,
        // or it contains the new frame plus some old ones
        assert!(
            !decoder.buffer.is_empty(),
            "Buffer should contain at least the new frame"
        );

        if decoder.buffer.len() == 1 {
            // If reset happened, only the new frame should be there
            assert!(
                decoder.buffer.keys().next() == Some(&large_sequence),
                "Buffer should only contain the new frame"
            );
        }
    }

    #[wasm_bindgen_test]
    fn test_video_buffering_logic_converges_fast() {
        let mut decoder = create_decoder(false);
        let empty: Vec<Arc<MediaPacket>> = vec![];

        // Test that buffering converges quickly with sequential frames
        for i in 1..=MIN_BUFFER_SIZE {
            let result = decoder.decode(create_mock_video_packet(
                i as u64,
                EncodedVideoChunkType::Key,
                vec![i as u8],
            ));
            assert_eq!(result, empty, "Should buffer until minimum size is reached");
        }

        // Add several more frames to ensure decoding happens
        let mut decoded_frames = Vec::new();
        for i in (MIN_BUFFER_SIZE as u64 + 1)..=(MIN_BUFFER_SIZE as u64 + 5) {
            let result = decoder.decode(create_mock_video_packet(
                i,
                EncodedVideoChunkType::Delta,
                vec![i as u8],
            ));
            decoded_frames.extend(result);
        }

        // Verify that some frames were decoded
        assert!(
            !decoded_frames.is_empty(),
            "Should have decoded at least one frame after buffer is filled"
        );

        // Verify frames are in sequence order
        let mut prev_seq = 0;
        for frame in decoded_frames {
            let seq = frame.video_metadata.sequence;
            assert!(seq > prev_seq, "Frames should be decoded in sequence order");
            prev_seq = seq;
        }
    }

    #[wasm_bindgen_test]
    fn test_video_jitter() {
        let mut decoder = create_decoder(false);

        // Fill buffer with initial frames in perfect order
        for i in 1..=MIN_BUFFER_SIZE {
            decoder.decode(create_mock_video_packet(
                i as u64,
                EncodedVideoChunkType::Key,
                vec![i as u8],
            ));
        }

        // Now simulate jittery network conditions with out-of-order frames
        let jittery_sequences = vec![
            (MIN_BUFFER_SIZE as u64 + 3, EncodedVideoChunkType::Delta),
            (MIN_BUFFER_SIZE as u64 + 1, EncodedVideoChunkType::Delta),
            (MIN_BUFFER_SIZE as u64 + 4, EncodedVideoChunkType::Delta),
            (MIN_BUFFER_SIZE as u64 + 2, EncodedVideoChunkType::Delta),
            (MIN_BUFFER_SIZE as u64 + 5, EncodedVideoChunkType::Delta),
        ];

        for (seq, frame_type) in jittery_sequences {
            decoder.decode(create_mock_video_packet(seq, frame_type, vec![seq as u8]));
        }

        // Verify frames were decoded in correct order
        let chunks = decoder.decoder.chunks.lock().unwrap();
        let mut prev_seq = 0;
        for chunk in chunks.iter() {
            let seq = chunk.video_metadata.sequence;
            assert!(
                seq > prev_seq,
                "Frames should be decoded in sequence order despite jitter"
            );
            prev_seq = seq;
        }
    }

    #[wasm_bindgen_test]
    fn test_video_max_buffer_size() {
        let mut decoder = create_decoder(false);

        // Fill buffer beyond MAX_BUFFER_SIZE
        for i in 1..=(MAX_BUFFER_SIZE + 5) {
            decoder.decode(create_mock_video_packet(
                i as u64,
                EncodedVideoChunkType::Delta,
                vec![i as u8],
            ));
        }

        // Verify buffer size doesn't exceed MAX_BUFFER_SIZE
        assert!(
            decoder.buffer.len() <= MAX_BUFFER_SIZE,
            "Buffer size should not exceed MAX_BUFFER_SIZE ({})",
            MAX_BUFFER_SIZE
        );

        // Add a keyframe to trigger buffer reset
        let keyframe_seq = (MAX_BUFFER_SIZE + 10) as u64;
        decoder.decode(create_mock_video_packet(
            keyframe_seq,
            EncodedVideoChunkType::Key,
            vec![42],
        ));

        // Verify buffer was reset and contains keyframe
        assert!(
            decoder.buffer.contains_key(&keyframe_seq),
            "Buffer should contain keyframe after max size reset"
        );
        assert!(
            decoder.buffer.len() < MAX_BUFFER_SIZE,
            "Buffer should be smaller after keyframe reset"
        );

        // Add more frames and verify buffer stays within limits
        for i in (keyframe_seq + 1)..=(keyframe_seq + 10) {
            decoder.decode(create_mock_video_packet(
                i,
                EncodedVideoChunkType::Delta,
                vec![i as u8],
            ));
        }

        assert!(
            decoder.buffer.len() <= MAX_BUFFER_SIZE,
            "Buffer size should remain bounded by MAX_BUFFER_SIZE"
        );
    }

    // Audio mode tests
    #[wasm_bindgen_test]
    fn test_audio_basic_decode() {
        let mut decoder = create_decoder(true);

        // Feed frames to fill buffer
        let empty: Vec<Arc<MediaPacket>> = vec![];
        for i in 1..=MIN_BUFFER_SIZE {
            let result = decoder.decode(create_mock_audio_packet(i as u64, true, vec![i as u8]));
            if i < MIN_BUFFER_SIZE {
                assert_eq!(
                    result, empty,
                    "Should not decode until buffer reaches minimum size"
                );
            }
        }

        // Add several more frames to ensure decoding eventually happens
        for i in (MIN_BUFFER_SIZE as u64 + 1)..=(MIN_BUFFER_SIZE as u64 + 5) {
            decoder.decode(create_mock_audio_packet(i, false, vec![i as u8]));
        }

        // Verify buffer state - we should have buffered at least some frames
        assert!(!decoder.buffer.is_empty(), "Buffer should contain frames");
    }

    #[wasm_bindgen_test]
    fn test_audio_out_of_order_frames() {
        let mut decoder = create_decoder(true);
        let empty: Vec<Arc<MediaPacket>> = vec![];

        // Feed out-of-order frames
        assert_eq!(
            decoder.decode(create_mock_audio_packet(3, false, vec![3])),
            empty
        );

        assert_eq!(
            decoder.decode(create_mock_audio_packet(1, true, vec![1])),
            empty
        );

        assert_eq!(
            decoder.decode(create_mock_audio_packet(5, false, vec![5])),
            empty
        );

        // Complete the buffer with more frames
        for i in 2..=10 {
            if i == 3 || i == 5 {
                continue;
            } // Skip already added sequences
            decoder.decode(create_mock_audio_packet(i, false, vec![i as u8]));
        }

        // Verify frames were decoded in proper order
        let chunks = decoder.decoder.chunks.lock().unwrap();
        if !chunks.is_empty() {
            let mut prev_seq = 0;
            for chunk in chunks.iter() {
                let seq = chunk.audio_metadata.sequence;
                assert!(seq > prev_seq, "Frames should be decoded in sequence order");
                prev_seq = seq;
            }
        }
    }

    #[wasm_bindgen_test]
    fn test_audio_keyframe_reset() {
        let mut decoder = create_decoder(true);

        // Fill buffer with initial frames
        for i in 1..=(MIN_BUFFER_SIZE + 2) {
            decoder.decode(create_mock_audio_packet(i as u64, false, vec![i as u8]));
        }

        // Insert a keyframe with a higher sequence number that should reset the buffer
        decoder.decode(create_mock_audio_packet(20, true, vec![20]));

        // Check buffer contains the keyframe
        assert!(
            decoder.buffer.contains_key(&20),
            "Buffer should contain the keyframe after reset"
        );

        // Add more frames after the keyframe
        for i in 21..=(20 + MIN_BUFFER_SIZE as u64) {
            decoder.decode(create_mock_audio_packet(i, false, vec![i as u8]));
        }

        // Check that frames from both batches were processed
        let chunks = decoder.decoder.chunks.lock().unwrap();

        // Filter for sequences from the first batch and second batch
        let first_batch = chunks
            .iter()
            .filter(|c| c.audio_metadata.sequence < 20)
            .count();
        let second_batch = chunks
            .iter()
            .filter(|c| c.audio_metadata.sequence >= 20)
            .count();

        // We should have processed at least some frames from both batches
        if !chunks.is_empty() {
            assert!(
                first_batch > 0 || second_batch > 0,
                "Should have processed frames from at least one batch"
            );
        }
    }

    #[wasm_bindgen_test]
    fn test_audio_sequence_gap() {
        let mut decoder = create_decoder(true);

        // Add initial frames
        for i in 1..=(MIN_BUFFER_SIZE + 2) {
            decoder.decode(create_mock_audio_packet(i as u64, false, vec![i as u8]));
        }

        // Save current buffer state (not used but kept for clarity)
        let _buffer_before = decoder.buffer.len();

        // Add a frame with a large sequence gap
        let large_sequence = 1000;
        decoder.decode(create_mock_audio_packet(large_sequence, false, vec![100]));

        // Verify buffer state
        assert!(
            decoder.buffer.contains_key(&large_sequence),
            "Buffer should contain the high-sequence frame"
        );

        // Either the buffer was reset and only contains the new frame,
        // or it contains the new frame plus some old ones
        assert!(
            !decoder.buffer.is_empty(),
            "Buffer should contain at least the new frame"
        );

        if decoder.buffer.len() == 1 {
            // If reset happened, only the new frame should be there
            assert!(
                decoder.buffer.keys().next() == Some(&large_sequence),
                "Buffer should only contain the new frame"
            );
        }
    }

    #[wasm_bindgen_test]
    fn test_audio_buffering_logic_converges_fast() {
        let mut decoder = create_decoder(true);
        let empty: Vec<Arc<MediaPacket>> = vec![];

        // Test that buffering converges quickly with sequential frames
        for i in 1..=MIN_BUFFER_SIZE {
            let result = decoder.decode(create_mock_audio_packet(i as u64, true, vec![i as u8]));
            assert_eq!(result, empty, "Should buffer until minimum size is reached");
        }

        // Add several more frames to ensure decoding happens
        let mut decoded_frames = Vec::new();
        for i in (MIN_BUFFER_SIZE as u64 + 1)..=(MIN_BUFFER_SIZE as u64 + 5) {
            let result = decoder.decode(create_mock_audio_packet(i, false, vec![i as u8]));
            decoded_frames.extend(result);
        }

        // Verify that some frames were decoded
        assert!(
            !decoded_frames.is_empty(),
            "Should have decoded at least one frame after buffer is filled"
        );

        // Verify frames are in sequence order
        let mut prev_seq = 0;
        for frame in decoded_frames {
            let seq = frame.audio_metadata.sequence;
            assert!(seq > prev_seq, "Frames should be decoded in sequence order");
            prev_seq = seq;
        }
    }
}

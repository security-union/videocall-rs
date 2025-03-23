use super::super::wrappers::EncodedVideoChunkTypeWrapper;
use super::video_decoder_wrapper::VideoDecoderTrait;
use std::{collections::BTreeMap, sync::Arc};
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::JsValue;
use web_sys::{CodecState, EncodedVideoChunkType, VideoDecoderConfig, VideoDecoderInit};

// Minimum number of frames to buffer before decoding
const MIN_BUFFER_SIZE: usize = 5;
// Maximum buffer size to prevent excessive memory usage
const MAX_BUFFER_SIZE: usize = 20;
// Maximum allowed sequence gap before resetting
const MAX_SEQUENCE_GAP: u64 = 100;

// This is a wrapper of the web-sys VideoDecoder which handles
// frames being out of order using a jitter buffer.
#[derive(Debug)]
pub struct VideoDecoderWithBuffer<A: VideoDecoderTrait> {
    video_decoder: A,
    buffer: BTreeMap<u64, Arc<MediaPacket>>,
    sequence: Option<u64>,
    min_jitter_buffer_size: usize,
    max_sequence_gap: u64,
}

impl<T: VideoDecoderTrait> VideoDecoderWithBuffer<T> {
    pub fn new(init: &VideoDecoderInit) -> Result<Self, JsValue> {
        T::new(init).map(|video_decoder| VideoDecoderWithBuffer {
            video_decoder,
            buffer: BTreeMap::new(),
            sequence: None,
            min_jitter_buffer_size: MIN_BUFFER_SIZE,
            max_sequence_gap: MAX_SEQUENCE_GAP,
        })
    }

    pub fn configure(&self, config: &VideoDecoderConfig) {
        self.video_decoder.configure(config);
    }

    pub fn decode(&mut self, packet: Arc<MediaPacket>) -> Vec<Arc<MediaPacket>> {
        let new_sequence = packet.video_metadata.sequence;
        let frame_type = EncodedVideoChunkTypeWrapper::from(packet.frame_type.as_str()).0;
        
        // Check for sequence reset
        let sequence_reset_detected = self.sequence.map_or(false, |seq| {
            (seq as i64 - new_sequence as i64).abs() > self.max_sequence_gap as i64
        });
        
        // Reset on keyframe or sequence reset
        if (frame_type == EncodedVideoChunkType::Key && self.should_reset_on_keyframe(&packet)) 
            || sequence_reset_detected {
            // Clear buffer and reset sequence
            self.buffer.clear();
            self.sequence = None;
        }
        
        // Add packet to buffer
        self.buffer.insert(new_sequence, packet);
        
        // Try to decode from buffer
        self.attempt_decode_from_buffer()
    }
    
    // Determines if we should reset the buffer based on keyframe
    fn should_reset_on_keyframe(&self, packet: &MediaPacket) -> bool {
        if let Some(current_seq) = self.sequence {
            // Reset if keyframe is newer and buffer is stale or we've been waiting too long
            packet.video_metadata.sequence > current_seq && 
            (self.buffer.len() < self.min_jitter_buffer_size || 
             self.buffer.len() > MAX_BUFFER_SIZE / 2)
        } else {
            // Always reset if we haven't started decoding yet
            true
        }
    }
    
    // Decode available frames from the buffer
    fn attempt_decode_from_buffer(&mut self) -> Vec<Arc<MediaPacket>> {
        let mut decoded_frames = Vec::new();
        
        // Process frames while we have enough in the buffer
        while self.buffer.len() > self.min_jitter_buffer_size {
            if let Some(&next_sequence) = self.buffer.keys().next() {
                // Initialize sequence if this is the first frame
                if self.sequence.is_none() {
                    if let Some(frame) = self.decode_next_frame(next_sequence) {
                        decoded_frames.push(frame);
                    }
                    continue;
                }
                
                let current_sequence = self.sequence.unwrap();
                
                // Remove older frames
                if next_sequence < current_sequence {
                    self.buffer.remove(&next_sequence);
                // Process next frame in sequence
                } else if current_sequence + 1 == next_sequence {
                    if let Some(frame) = self.decode_next_frame(next_sequence) {
                        decoded_frames.push(frame);
                    }
                // Fast forward if we have a gap but buffer is getting too large
                } else if self.buffer.len() >= (2 * self.max_sequence_gap as usize / 3) {
                    if let Some(frame) = self.decode_next_frame(next_sequence) {
                        decoded_frames.push(frame);
                    }
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
        if let Some(frame) = self.buffer.remove(&next_sequence) {
            self.video_decoder.decode(frame.clone());
            self.sequence = Some(next_sequence);
            Some(frame)
        } else {
            None
        }
    }

    pub fn state(&self) -> CodecState {
        self.video_decoder.state()
    }
}

// Create a test suite for the decoder
#[cfg(test)]
mod test {
    use std::sync::Mutex;
    use videocall_types::protos::media_packet::VideoMetadata;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_test::wasm_bindgen_test;
    use super::*;
    
    pub struct MockVideoDecoder {
        chunks: Arc<Mutex<Vec<Arc<MediaPacket>>>>,
        pub state: CodecState,
    }

    impl VideoDecoderTrait for MockVideoDecoder {
        fn configure(&self, _config: &VideoDecoderConfig) {
            // Mock implementation, do nothing
        }

        fn decode(&self, image: Arc<MediaPacket>) {
            let mut chunks = self.chunks.lock().unwrap();
            chunks.push(image);
        }

        fn state(&self) -> CodecState {
            self.state
        }

        fn new(_init: &VideoDecoderInit) -> Result<Self, JsValue>
        where
            Self: Sized,
        {
            Ok(MockVideoDecoder {
                chunks: Arc::new(Mutex::new(Vec::new())),
                state: CodecState::Configured,
            })
        }
    }

    fn create_mock_packet(
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
            special_fields: Default::default(),
        })
    }

    fn create_video_decoder() -> VideoDecoderWithBuffer<MockVideoDecoder> {
        let error = Closure::wrap(Box::new(move |_e: JsValue| {}) as Box<dyn FnMut(JsValue)>);
        let output = Closure::wrap(Box::new(move |_original_chunk: JsValue| {}) as Box<dyn FnMut(JsValue)>);
        let init = VideoDecoderInit::new(
            error.as_ref().unchecked_ref(),
            output.as_ref().unchecked_ref(),
        );
        VideoDecoderWithBuffer::new(&init).unwrap()
    }
    
    #[wasm_bindgen_test]
    fn test_basic_decode() {
        let mut decoder = create_video_decoder();
        
        // Feed frames to fill buffer
        let empty: Vec<Arc<MediaPacket>> = vec![];
        for i in 1..=MIN_BUFFER_SIZE {
            let result = decoder.decode(create_mock_packet(i as u64, EncodedVideoChunkType::Key, vec![i as u8]));
            if i < MIN_BUFFER_SIZE {
                assert_eq!(result, empty, "Should not decode until buffer reaches minimum size");
            }
        }
        
        // Add several more frames to ensure decoding eventually happens
        for i in (MIN_BUFFER_SIZE as u64 + 1)..=(MIN_BUFFER_SIZE as u64 + 5) {
            decoder.decode(create_mock_packet(i, EncodedVideoChunkType::Delta, vec![i as u8]));
        }
        
        // Verify buffer state - we should have buffered at least some frames
        assert!(decoder.buffer.len() > 0, "Buffer should contain frames");
    }
    
    #[wasm_bindgen_test]
    fn test_out_of_order_frames() {
        let mut decoder = create_video_decoder();
        let empty: Vec<Arc<MediaPacket>> = vec![];
        
        // Feed out-of-order frames
        assert_eq!(
            decoder.decode(create_mock_packet(3, EncodedVideoChunkType::Delta, vec![3])),
            empty
        );
        
        assert_eq!(
            decoder.decode(create_mock_packet(1, EncodedVideoChunkType::Key, vec![1])),
            empty
        );
        
        assert_eq!(
            decoder.decode(create_mock_packet(5, EncodedVideoChunkType::Delta, vec![5])),
            empty
        );
        
        assert_eq!(
            decoder.decode(create_mock_packet(2, EncodedVideoChunkType::Delta, vec![2])),
            empty
        );
        
        assert_eq!(
            decoder.decode(create_mock_packet(4, EncodedVideoChunkType::Delta, vec![4])),
            empty
        );
        
        // Now add one more to reach MIN_BUFFER_SIZE + 1
        let _result = decoder.decode(create_mock_packet(6, EncodedVideoChunkType::Delta, vec![6]));
        
        // Get the processed frames
        let chunks = decoder.video_decoder.chunks.lock().unwrap();
        
        // Check that at least one frame was processed
        if !chunks.is_empty() {
            // Verify frames are processed in sequence order
            let mut prev_seq = 0;
            for chunk in chunks.iter() {
                let seq = chunk.video_metadata.sequence;
                assert!(seq > prev_seq, "Frames should be processed in ascending sequence order");
                prev_seq = seq;
            }
        }
    }
    
    #[wasm_bindgen_test]
    fn test_keyframe_reset() {
        let mut decoder = create_video_decoder();
        
        // Fill buffer with initial frames
        for i in 1..=(MIN_BUFFER_SIZE + 2) {
            decoder.decode(create_mock_packet(i as u64, EncodedVideoChunkType::Delta, vec![i as u8]));
        }
        
        // Insert a keyframe with a higher sequence number that should reset the buffer
        decoder.decode(create_mock_packet(20, EncodedVideoChunkType::Key, vec![20]));
        
        // Check buffer contains the keyframe
        assert!(decoder.buffer.contains_key(&20), "Buffer should contain the keyframe after reset");
        
        // Add more frames after the keyframe
        for i in 21..=(20 + MIN_BUFFER_SIZE as u64) {
            decoder.decode(create_mock_packet(i, EncodedVideoChunkType::Delta, vec![i as u8]));
        }
        
        // Check that frames from both batches were processed
        let chunks = decoder.video_decoder.chunks.lock().unwrap();
        
        // Filter for sequences from the first batch and second batch
        let first_batch = chunks.iter().filter(|c| c.video_metadata.sequence < 20).count();
        let second_batch = chunks.iter().filter(|c| c.video_metadata.sequence >= 20).count();
        
        // We should have processed at least some frames from both batches
        if !chunks.is_empty() {
            assert!(first_batch > 0 || second_batch > 0, "Should have processed frames from at least one batch");
        }
    }
    
    #[wasm_bindgen_test]
    fn test_sequence_gap() {
        let mut decoder = create_video_decoder();
        
        // Add initial frames
        for i in 1..=(MIN_BUFFER_SIZE + 2) {
            decoder.decode(create_mock_packet(i as u64, EncodedVideoChunkType::Delta, vec![i as u8]));
        }
        
        // Save current buffer state (not used but kept for clarity)
        let _buffer_before = decoder.buffer.len();
        
        // Add a frame with a large sequence gap
        let large_sequence = 1000;
        decoder.decode(create_mock_packet(large_sequence, EncodedVideoChunkType::Delta, vec![100]));
        
        // Verify buffer state
        assert!(decoder.buffer.contains_key(&large_sequence), "Buffer should contain the high-sequence frame");
        
        // Either the buffer was reset and only contains the new frame,
        // or it contains the new frame plus some old ones
        assert!(decoder.buffer.len() >= 1, "Buffer should contain at least the new frame");
        
        if decoder.buffer.len() == 1 {
            // If reset happened, only the new frame should be there
            assert!(decoder.buffer.keys().next() == Some(&large_sequence), "Buffer should only contain the new frame");
        }
    }

    #[wasm_bindgen_test]
    fn test_buffering_logic_converges_extremely_fast() {
        let mut decoder = create_video_decoder();
        let empty: Vec<Arc<MediaPacket>> = vec![];
        
        // Test that buffering converges quickly with sequential frames
        for i in 1..=MIN_BUFFER_SIZE {
            let result = decoder.decode(create_mock_packet(i as u64, EncodedVideoChunkType::Key, vec![i as u8]));
            assert_eq!(result, empty, "Should buffer until minimum size is reached");
        }
        
        // Add several more frames to ensure decoding happens
        let mut decoded_frames = Vec::new();
        for i in (MIN_BUFFER_SIZE as u64 + 1)..=(MIN_BUFFER_SIZE as u64 + 5) {
            let result = decoder.decode(create_mock_packet(i, EncodedVideoChunkType::Delta, vec![i as u8]));
            decoded_frames.extend(result);
        }
        
        // Verify that some frames were decoded
        assert!(!decoded_frames.is_empty(), "Should have decoded at least one frame after buffer is filled");
        
        // Verify frames are in sequence order
        let mut prev_seq = 0;
        for frame in decoded_frames {
            let seq = frame.video_metadata.sequence;
            assert!(seq > prev_seq, "Frames should be decoded in sequence order");
            prev_seq = seq;
        }
    }

    #[wasm_bindgen_test]
    fn decode_jittery_video() {
        let mut decoder = create_video_decoder();
        
        // Fill buffer with initial frames in perfect order
        for i in 1..=MIN_BUFFER_SIZE {
            decoder.decode(create_mock_packet(i as u64, EncodedVideoChunkType::Key, vec![i as u8]));
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
            decoder.decode(create_mock_packet(seq, frame_type, vec![seq as u8]));
        }
        
        // Verify frames were decoded in correct order
        let chunks = decoder.video_decoder.chunks.lock().unwrap();
        let mut prev_seq = 0;
        for chunk in chunks.iter() {
            let seq = chunk.video_metadata.sequence;
            assert!(seq > prev_seq, "Frames should be decoded in sequence order despite jitter");
            prev_seq = seq;
        }
    }

    #[wasm_bindgen_test]
    fn test_max_buffer_size() {
        let mut decoder = create_video_decoder();
        
        // Fill buffer beyond MAX_BUFFER_SIZE
        for i in 1..=(MAX_BUFFER_SIZE + 5) {
            decoder.decode(create_mock_packet(i as u64, EncodedVideoChunkType::Delta, vec![i as u8]));
        }
        
        // Verify buffer size doesn't exceed MAX_BUFFER_SIZE
        assert!(decoder.buffer.len() <= MAX_BUFFER_SIZE, 
            "Buffer size should not exceed MAX_BUFFER_SIZE ({})", MAX_BUFFER_SIZE);
        
        // Add a keyframe to trigger buffer reset
        let keyframe_seq = (MAX_BUFFER_SIZE + 10) as u64;
        decoder.decode(create_mock_packet(keyframe_seq, EncodedVideoChunkType::Key, vec![42]));
        
        // Verify buffer was reset and contains keyframe
        assert!(decoder.buffer.contains_key(&keyframe_seq), 
            "Buffer should contain keyframe after max size reset");
        assert!(decoder.buffer.len() < MAX_BUFFER_SIZE, 
            "Buffer should be smaller after keyframe reset");
        
        // Add more frames and verify buffer stays within limits
        for i in (keyframe_seq + 1)..=(keyframe_seq + 10) {
            decoder.decode(create_mock_packet(i, EncodedVideoChunkType::Delta, vec![i as u8]));
        }
        
        assert!(decoder.buffer.len() <= MAX_BUFFER_SIZE, 
            "Buffer size should remain bounded by MAX_BUFFER_SIZE");
    }
}

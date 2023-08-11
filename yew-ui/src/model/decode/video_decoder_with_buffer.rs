use std::{cmp::Ordering, collections::BTreeMap, sync::Arc};

use types::protos::media_packet::MediaPacket;
use wasm_bindgen::JsValue;
use web_sys::{CodecState, EncodedVideoChunkType, VideoDecoderConfig, VideoDecoderInit};

use crate::model::EncodedVideoChunkTypeWrapper;

use super::video_decoder_wrapper::VideoDecoderTrait;

const MAX_BUFFER_SIZE: usize = 10;

// This is a wrapper of the web-sys VideoDecoder which handles
// frames being out of order and other issues.
pub struct VideoDecoderWithBuffer<A: VideoDecoderTrait> {
    video_decoder: A,
    cache: BTreeMap<u64, Arc<MediaPacket>>,
    sequence: Option<u64>,
}

impl<T: VideoDecoderTrait> VideoDecoderWithBuffer<T> {
    pub fn new(init: &VideoDecoderInit) -> Result<Self, JsValue> {
        T::new(init).map(|video_decoder| VideoDecoderWithBuffer {
            video_decoder,
            cache: BTreeMap::new(),
            sequence: None,
        })
    }

    pub fn configure(&self, config: &VideoDecoderConfig) {
        self.video_decoder.configure(config);
    }

    pub fn decode(&mut self, image: Arc<MediaPacket>) {
        let new_sequence_number = image.video_metadata.sequence;
        let frame_type = EncodedVideoChunkTypeWrapper::from(image.frame_type.as_str()).0;
        let cache_size = self.cache.len();
        // If we get a keyframe, play it immediately, then prune all packets before it
        if frame_type == EncodedVideoChunkType::Key {
            self.video_decoder.decode(image);
            self.sequence = Some(new_sequence_number);
            self.prune_older_frames_from_buffer(new_sequence_number);
        } else if let Some(sequence) = self.sequence {
            let is_future_frame = new_sequence_number > sequence;
            let is_future_i_frame = is_future_frame && frame_type == EncodedVideoChunkType::Key;
            let is_next_frame = new_sequence_number == sequence + 1;
            let next_frame_already_cached = self.cache.get(&(sequence + 1)).is_some();
            if is_future_i_frame || is_next_frame {
                self.video_decoder.decode(image);
                self.sequence = Some(new_sequence_number);
                self.play_queued_follow_up_frames();
                self.prune_older_frames_from_buffer(sequence);
            } else {
                if next_frame_already_cached {
                    self.play_queued_follow_up_frames();
                    self.prune_older_frames_from_buffer(sequence);
                }
                if is_future_frame {
                    self.cache.insert(new_sequence_number, image);
                    if cache_size + 1 > MAX_BUFFER_SIZE {
                        self.fast_forward_frames_and_then_prune_buffer();
                    }
                }
            }
        }
    }

    fn fast_forward_frames_and_then_prune_buffer(&mut self) {
        let mut should_skip = false;
        let sorted_frames = self.cache.keys().cloned().collect::<Vec<_>>();
        let mut to_remove = Vec::new(); // We will store the keys that we want to remove here
        for (index, sequence) in sorted_frames.iter().enumerate() {
            let image = self.cache.get(sequence).unwrap();
            let frame_type = EncodedVideoChunkTypeWrapper::from(image.frame_type.as_str()).0;
            let next_sequence = if (index == 0 || *sequence == sorted_frames[index - 1] + 1)
                || (self.sequence.is_some()
                    && *sequence > self.sequence.unwrap()
                    && frame_type == EncodedVideoChunkType::Key)
            {
                Some(*sequence)
            } else {
                should_skip = true;
                None
            };
            if let Some(next_sequence) = next_sequence {
                if !should_skip {
                    let next_image = self.cache.get(&next_sequence).unwrap();
                    self.video_decoder.decode(next_image.clone());
                    self.sequence = Some(next_sequence);
                    to_remove.push(next_sequence); // Instead of removing here, we add it to the remove list
                }
            } else if let Some(self_sequence) = self.sequence {
                if *sequence < self_sequence {
                    to_remove.push(*sequence); // Again, add to the remove list instead of removing directly
                }
            }
        }
        // After the iteration, we can now remove the items from the cache
        for sequence in to_remove {
            self.cache.remove(&sequence);
        }
    }

    fn prune_older_frames_from_buffer(&mut self, sequence_number: u64) {
        self.cache
            .retain(|sequence, _| *sequence >= sequence_number)
    }

    fn play_queued_follow_up_frames(&mut self) {
        let sorted_frames = self.cache.keys().collect::<Vec<_>>();
        if self.sequence.is_none() || sorted_frames.is_empty() {
            return;
        }
        for current_sequence in sorted_frames {
            let next_sequence = self.sequence.unwrap() + 1;
            match current_sequence.cmp(&next_sequence) {
                Ordering::Less => continue,
                Ordering::Equal => {
                    if let Some(next_image) = self.cache.get(current_sequence) {
                        self.video_decoder.decode(next_image.clone());
                        self.sequence = Some(next_sequence);
                    }
                }
                Ordering::Greater => break,
            }
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

    use types::protos::media_packet::VideoMetadata;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::*;
    pub struct MockVideoDecoder {
        chunks: Arc<Mutex<Vec<Arc<MediaPacket>>>>,
        pub state: CodecState,
    }

    impl VideoDecoderTrait for MockVideoDecoder {
        fn configure(&self, _config: &VideoDecoderConfig) {
            // Mock implementation, possibly do nothing
        }

        fn decode(&self, image: Arc<MediaPacket>) {
            let mut chunks = self.chunks.lock().unwrap();
            chunks.push(image);
        }

        fn state(&self) -> CodecState {
            // Mock implementation, return some state
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
        // This function creates a mock MediaPacket.
        Arc::new(MediaPacket {
            media_type: Default::default(), // Put an appropriate default or value here
            email: "test@example.com".to_string(),
            data,
            frame_type: EncodedVideoChunkTypeWrapper(chunk_type).to_string(),
            timestamp: 0.0,
            duration: 0.0,
            audio_metadata: Default::default(), // Put an appropriate default or value here
            video_metadata: Some(video_metadata).into(), // Assuming sequence is a field in VideoMetadata
            special_fields: Default::default(),          // Put an appropriate default or value here
        })
    }

    fn create_video_decoder() -> VideoDecoderWithBuffer<MockVideoDecoder> {
        let error = Closure::wrap(Box::new(move |_e: JsValue| {}) as Box<dyn FnMut(JsValue)>);
        let output =
            Closure::wrap(Box::new(move |_original_chunk: JsValue| {}) as Box<dyn FnMut(JsValue)>);
        let init = VideoDecoderInit::new(
            error.as_ref().unchecked_ref(),
            output.as_ref().unchecked_ref(),
        );
        let video_decoder_with_buffer: VideoDecoderWithBuffer<MockVideoDecoder> =
            VideoDecoderWithBuffer::new(&init).unwrap();
        video_decoder_with_buffer
    }
    #[wasm_bindgen_test]
    fn test_in_order_frames_happy_path() {
        let mut video_decoder_with_buffer = create_video_decoder();

        // Generate in-order frames
        let packets = vec![
            create_mock_packet(1, EncodedVideoChunkType::Key, vec![1, 2, 3]),
            create_mock_packet(2, EncodedVideoChunkType::Delta, vec![4, 5, 6]),
            create_mock_packet(3, EncodedVideoChunkType::Delta, vec![7, 8, 9]),
        ];

        // Feed frames into video_decoder_with_buffer
        for packet in packets {
            video_decoder_with_buffer.decode(packet);
        }

        // Assertions to verify that mock_decoder has received and processed frames in order
        let processed_sequences: Vec<u64> = video_decoder_with_buffer
            .video_decoder
            .chunks
            .lock()
            .unwrap()
            .iter()
            .map(|chunk| {
                // Extract sequence number from chunk; assuming a method to do this
                chunk.video_metadata.sequence
            })
            .collect();
        assert_eq!(processed_sequences, vec![1, 2, 3]);
    }

    #[wasm_bindgen_test]
    fn test_out_of_order_key_frames() {
        let mut video_decoder_with_buffer = create_video_decoder();

        // Generate out-of-order frames
        let packets = vec![
            create_mock_packet(3, EncodedVideoChunkType::Key, vec![7, 8, 9]),
            create_mock_packet(1, EncodedVideoChunkType::Key, vec![1, 2, 3]),
            create_mock_packet(2, EncodedVideoChunkType::Key, vec![4, 5, 6]),
        ];

        // Feed frames into video_decoder_with_buffer
        for packet in packets {
            video_decoder_with_buffer.decode(packet);
        }

        // Assertions to verify that frames were buffered and ordered correctly before decoding
        let processed_sequences: Vec<u64> = video_decoder_with_buffer
            .video_decoder
            .chunks
            .lock()
            .unwrap()
            .iter()
            .map(|chunk| {
                chunk.video_metadata.sequence // Extract sequence number from chunk; assuming a method to do this
            })
            .collect();
        assert_eq!(processed_sequences, vec![3, 1, 2]);
    }

    #[wasm_bindgen_test]
    fn test_extremely_out_of_order_frames() {
        let mut video_decoder_with_buffer = create_video_decoder();

        // Generate extremely out-of-order frames
        let packets = vec![
            create_mock_packet(5, EncodedVideoChunkType::Key, vec![10, 11, 12]),
            create_mock_packet(3, EncodedVideoChunkType::Delta, vec![7, 8, 9]),
            create_mock_packet(1, EncodedVideoChunkType::Delta, vec![1, 2, 3]),
            create_mock_packet(6, EncodedVideoChunkType::Delta, vec![13, 14, 15]),
            create_mock_packet(2, EncodedVideoChunkType::Delta, vec![4, 5, 6]),
        ];

        // Feed frames into video_decoder_with_buffer
        for packet in packets {
            video_decoder_with_buffer.decode(packet);
        }

        // Assertions to verify that older frames were dropped and it tried to catch up
        let processed_sequences: Vec<u64> = video_decoder_with_buffer
            .video_decoder
            .chunks
            .lock()
            .unwrap()
            .iter()
            .map(|chunk| {
                chunk.video_metadata.sequence // Extract sequence number from chunk; assuming a method to do this
            })
            .collect();
        assert!(processed_sequences == vec![5, 6] || processed_sequences == vec![5, 6]);
    }
}

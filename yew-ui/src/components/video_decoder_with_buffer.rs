use std::{collections::BTreeMap, sync::Arc};

use js_sys::Uint8Array;
use types::protos::media_packet::MediaPacket;
use wasm_bindgen::JsValue;
use web_sys::{
    CodecState, EncodedVideoChunk, EncodedVideoChunkInit, EncodedVideoChunkType, VideoDecoder,
    VideoDecoderConfig, VideoDecoderInit,
};

use crate::model::EncodedVideoChunkTypeWrapper;

const MAX_BUFFER_SIZE: usize = 10;

// This is a wrapper of the web-sys VideoDecoder which handles
// frames being out of order and other issues.
pub struct VideoDecoderWithBuffer {
    video_decoder: VideoDecoder,
    cache: BTreeMap<u64, Arc<MediaPacket>>,
    sequence: Option<u64>,
}

impl VideoDecoderWithBuffer {
    pub fn new(init: &VideoDecoderInit) -> Result<Self, JsValue> {
        VideoDecoder::new(init).map(|video_decoder| VideoDecoderWithBuffer {
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
        if self.sequence.is_none() && frame_type == EncodedVideoChunkType::Key {
            self.internal_decode(image.clone());
            self.sequence = Some(new_sequence_number);
        } else if let Some(sequence) = self.sequence {
            let is_future_frame = new_sequence_number > sequence;
            let is_future_i_frame = is_future_frame && frame_type == EncodedVideoChunkType::Key;
            let is_next_frame = new_sequence_number == sequence + 1;
            let next_frame_already_cached = self.cache.get(&(sequence + 1)).is_some();
            if is_future_i_frame || is_next_frame {
                self.internal_decode(image);
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
        let mut index = 0;
        let sorted_frames = self.cache.keys().cloned().collect::<Vec<_>>();
        let mut to_remove = Vec::new();  // We will store the keys that we want to remove here
        for sequence in &sorted_frames {
            let image = self.cache.get(sequence).unwrap();
            let frame_type = EncodedVideoChunkTypeWrapper::from(image.frame_type.as_str()).0;
            let next_sequence = if index == 0 {
                Some(*sequence)
            } else if *sequence == sorted_frames[index - 1] + 1 {
                Some(*sequence)
            } else if self.sequence.is_some()
                && *sequence > self.sequence.unwrap()
                && frame_type == EncodedVideoChunkType::Key
            {
                Some(*sequence)
            } else {
                should_skip = true;
                None
            };
            if let Some(next_sequence) = next_sequence {
                if !should_skip {
                    let next_image = self.cache.get(&next_sequence).unwrap();
                    self.internal_decode(next_image.clone());
                    self.sequence = Some(next_sequence);
                    to_remove.push(next_sequence); // Instead of removing here, we add it to the remove list
                }
            } else if let Some(self_sequence) = self.sequence {
                if *sequence < self_sequence {
                    to_remove.push(*sequence); // Again, add to the remove list instead of removing directly
                }
            }
            index = index + 1;
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

    fn internal_decode(&self, image: Arc<MediaPacket>) {
        let chunk_type = EncodedVideoChunkTypeWrapper::from(image.frame_type.as_str()).0;
        let video_data = Uint8Array::new_with_length(image.data.len().try_into().unwrap());
        video_data.copy_from(&image.data);
        let mut video_chunk = EncodedVideoChunkInit::new(&video_data, image.timestamp, chunk_type);
        video_chunk.duration(image.duration);
        let encoded_video_chunk = EncodedVideoChunk::new(&video_chunk).unwrap();
        self.video_decoder.decode(&encoded_video_chunk);
    }

    fn play_queued_follow_up_frames(&mut self) {
        let sorted_frames = self.cache.keys().collect::<Vec<_>>();
        if !self.sequence.is_some() || sorted_frames.is_empty() {
            return;
        }
        for index in 0..sorted_frames.len() {
            let current_sequence = sorted_frames[index];
            let next_sequence = self.sequence.unwrap() + 1;
            if *current_sequence < next_sequence {
                continue;
            } else if *current_sequence == next_sequence {
                if let Some(next_image) = self.cache.get(&current_sequence) {
                    self.internal_decode(next_image.clone());
                    self.sequence = Some(next_sequence);
                }
            } else {
                break;
            }
        }
    }

    pub fn state(&self) -> CodecState {
        self.video_decoder.state()
    }
}

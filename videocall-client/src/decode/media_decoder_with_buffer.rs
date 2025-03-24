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

    pub fn configure(&self, config: &D::ConfigType) {
        self.decoder.configure(config);
    }

    pub fn decode(&mut self, packet: Arc<MediaPacket>) -> Vec<Arc<MediaPacket>> {
        let new_sequence = self.decoder.get_sequence_number(&packet);
        let is_keyframe = self.decoder.is_keyframe(&packet);
        
        // Check for sequence reset
        let sequence_reset_detected = self.sequence.map_or(false, |seq| {
            (seq as i64 - new_sequence as i64).abs() > self.max_sequence_gap as i64
        });
        
        // Reset on keyframe or sequence reset
        if (is_keyframe && self.should_reset_on_keyframe(&packet, new_sequence)) 
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
    fn should_reset_on_keyframe(&self, _packet: &MediaPacket, new_sequence: u64) -> bool {
        if let Some(current_seq) = self.sequence {
            // Reset if keyframe is newer and buffer is stale or we've been waiting too long
            new_sequence > current_seq && 
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
            self.decoder.decode(frame.clone());
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
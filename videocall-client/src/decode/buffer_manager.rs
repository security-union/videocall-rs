use log::{debug, info, warn};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;

/// Configuration for the buffer manager
#[derive(Debug, Clone)]
pub struct BufferConfig {
    /// Minimum number of frames to buffer before decoding
    pub min_size: usize,

    /// Maximum buffer size to prevent excessive memory usage
    pub max_size: usize,

    /// Maximum allowed sequence gap before resetting
    pub max_sequence_gap: u64,

    /// Maximum allowed gap before forcing decode
    pub max_gap_before_force_decode: u64,
}

impl Default for BufferConfig {
    fn default() -> Self {
        BufferConfig {
            min_size: 5,
            max_size: 20,
            max_sequence_gap: 100,
            max_gap_before_force_decode: 5,
        }
    }
}

/// Manages a buffer of media packets with efficient operations
#[derive(Debug)]
pub struct BufferManager {
    /// The actual buffer storing frames by sequence number
    buffer: BTreeMap<u64, Arc<MediaPacket>>,

    /// Set of sequence numbers for keyframes for quick lookup
    keyframe_positions: HashSet<u64>,

    /// Configuration for the buffer
    config: BufferConfig,

    /// Count of missing frames
    missing_frame_count: u64,
}

impl BufferManager {
    /// Create a new buffer manager with the given configuration
    pub fn new(config: BufferConfig) -> Self {
        BufferManager {
            buffer: BTreeMap::new(),
            keyframe_positions: HashSet::new(),
            config,
            missing_frame_count: 0,
        }
    }

    /// Add a frame to the buffer
    ///
    /// Returns true if the frame was added, false if it was already in the buffer
    pub fn add_frame(
        &mut self,
        sequence: u64,
        packet: Arc<MediaPacket>,
        is_keyframe: bool,
    ) -> bool {
        if is_keyframe {
            self.keyframe_positions.insert(sequence);
        }

        if let std::collections::btree_map::Entry::Vacant(e) = self.buffer.entry(sequence) {
            e.insert(packet);
            debug!(
                "Added frame {} to buffer, new size: {}",
                sequence,
                self.buffer.len()
            );
            true
        } else {
            debug!("Frame {} already in buffer, skipping", sequence);
            false
        }
    }

    /// Check if the buffer has enough frames to start decoding
    pub fn has_minimum_frames(&self) -> bool {
        self.buffer.len() >= self.config.min_size
    }

    /// Check if the buffer is full
    pub fn is_full(&self) -> bool {
        self.buffer.len() >= self.config.max_size
    }

    /// Get the current buffer size
    pub fn size(&self) -> usize {
        self.buffer.len()
    }

    /// Check if the buffer is empty
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Get the next sequence number in the buffer
    pub fn next_sequence(&self) -> Option<u64> {
        self.buffer.keys().next().copied()
    }

    /// Get a frame by sequence number
    pub fn get_frame(&self, sequence: u64) -> Option<&Arc<MediaPacket>> {
        self.buffer.get(&sequence)
    }

    /// Remove a frame from the buffer
    pub fn remove_frame(&mut self, sequence: u64) -> Option<Arc<MediaPacket>> {
        self.keyframe_positions.remove(&sequence);
        self.buffer.remove(&sequence)
    }

    /// Clear the buffer
    pub fn clear(&mut self) {
        info!("Clearing buffer");
        self.buffer.clear();
        self.keyframe_positions.clear();
        self.missing_frame_count = 0;
    }

    /// Clear the buffer up to the given sequence (inclusive)
    pub fn clear_up_to(&mut self, up_to_sequence: u64) {
        info!(
            "Clearing buffer up to sequence {}, current size: {}",
            up_to_sequence,
            self.buffer.len()
        );

        // Collect keys to remove
        let keys_to_remove: Vec<u64> = self
            .buffer
            .keys()
            .filter(|&&seq| seq <= up_to_sequence)
            .cloned()
            .collect();

        // Remove keys and update keyframe positions
        for key in keys_to_remove {
            self.keyframe_positions.remove(&key);
            self.buffer.remove(&key);
        }

        info!(
            "Cleared buffer up to sequence {}, new size: {}",
            up_to_sequence,
            self.buffer.len()
        );
    }

    /// Find the next keyframe in the buffer after the given sequence
    pub fn find_next_keyframe(&self, after_sequence: u64) -> Option<u64> {
        // First check the cached keyframe positions
        for &seq in self.keyframe_positions.iter() {
            if seq > after_sequence {
                return Some(seq);
            }
        }

        // Fallback to checking all frames if cache is incomplete
        for (seq, packet) in self.buffer.iter() {
            if *seq > after_sequence && packet.frame_type == "key" {
                return Some(*seq);
            }
        }

        None
    }

    /// Check if there's a large gap between the current sequence and the next frame
    pub fn has_large_gap(&self, current_sequence: u64) -> bool {
        if let Some(&next_seq) = self.buffer.keys().next() {
            (next_seq as i64 - current_sequence as i64).abs() > self.config.max_sequence_gap as i64
        } else {
            false
        }
    }

    /// Increment the missing frame count
    pub fn increment_missing_frame_count(&mut self) {
        self.missing_frame_count += 1;
        debug!("Missing frame count: {}", self.missing_frame_count);
    }

    /// Reset the missing frame count
    pub fn reset_missing_frame_count(&mut self) {
        self.missing_frame_count = 0;
    }

    /// Get the current missing frame count
    pub fn missing_frame_count(&self) -> u64 {
        self.missing_frame_count
    }

    /// Check if we should force decode due to missing frames
    pub fn should_force_decode(&self, max_wait_for_missing_frame: u64) -> bool {
        self.missing_frame_count >= max_wait_for_missing_frame
    }

    /// Get all frames in the buffer
    pub fn frames(&self) -> &BTreeMap<u64, Arc<MediaPacket>> {
        &self.buffer
    }

    /// Get the configuration
    pub fn config(&self) -> &BufferConfig {
        &self.config
    }
}

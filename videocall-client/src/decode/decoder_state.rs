use std::fmt;

/// Represents the current state of the media decoder
#[derive(Debug, Clone, PartialEq)]
pub enum DecoderState {
    /// Initial state when the decoder is created
    Initializing,

    /// Buffering frames until minimum buffer size is reached
    Buffering {
        /// Current count of frames in buffer
        count: usize,
    },

    /// Actively decoding frames
    Decoding {
        /// Last processed sequence number
        last_sequence: u64,
    },

    /// Error state with reason
    Error {
        /// Description of the error
        reason: String,
    },
}

impl fmt::Display for DecoderState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecoderState::Initializing => write!(f, "Initializing"),
            DecoderState::Buffering { count } => write!(f, "Buffering ({})", count),
            DecoderState::Decoding { last_sequence } => {
                write!(f, "Decoding (last: {})", last_sequence)
            }
            DecoderState::Error { reason } => write!(f, "Error: {}", reason),
        }
    }
}

impl DecoderState {
    /// Check if the decoder is in a state where it can process frames
    pub fn can_process_frames(&self) -> bool {
        matches!(self, DecoderState::Decoding { .. })
    }

    /// Check if the decoder is in an error state
    pub fn is_error(&self) -> bool {
        matches!(self, DecoderState::Error { .. })
    }

    /// Get the last processed sequence number if available
    pub fn last_sequence(&self) -> Option<u64> {
        if let DecoderState::Decoding { last_sequence } = self {
            Some(*last_sequence)
        } else {
            None
        }
    }
}

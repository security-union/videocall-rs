//! # Rust NetEQ
//!
//! A NetEQ-inspired adaptive jitter buffer implementation for audio decoding.
//! This library provides functionality to handle network jitter, packet reordering,
//! and adaptive buffering for real-time audio applications.

pub mod buffer;
pub mod delay_manager;
pub mod error;
pub mod neteq;
pub mod packet;
pub mod statistics;
pub mod time_stretch;

pub use error::{NetEqError, Result};
pub use neteq::{NetEq, NetEqConfig, NetEqStats, Operation};
pub use packet::{AudioPacket, RtpHeader};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_functionality() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        // Test basic operations
        assert_eq!(neteq.target_delay_ms(), 0);
        assert!(neteq.is_empty());
    }
}

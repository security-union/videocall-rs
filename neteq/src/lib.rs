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

//! # Rust NetEQ
//!
//! A NetEQ-inspired adaptive jitter buffer implementation for audio decoding.
//! This library provides functionality to handle network jitter, packet reordering,
//! and adaptive buffering for real-time audio applications.

pub mod buffer;
pub mod buffer_level_filter;
pub mod codec;
pub mod delay_manager;
pub mod error;
pub mod expand;
pub mod histogram;
pub mod neteq;
pub mod packet;
pub mod signal;
pub mod statistics;
pub mod time_stretch;

pub use statistics::q14;

#[cfg(all(feature = "web", target_arch = "wasm32"))]
pub mod web;

pub use error::{NetEqError, Result};
pub use neteq::{NetEq, NetEqConfig, NetEqStats, Operation};
pub use packet::{AudioPacket, RtpHeader};

#[cfg(all(feature = "web", target_arch = "wasm32"))]
pub use web::WebNetEq;

/// RMS of one PCM frame; the same left-to-right `f32` sum as `videocall_client`'s
/// `rms_of_samples`, which the main thread uses for the VAD on the old path.
pub fn frame_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sum_squares: f32 = 0.0;
    for &sample in samples {
        sum_squares += sample * sample;
    }
    (sum_squares / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rms_matches_hand_computed_frames() {
        assert_eq!(frame_rms(&[]), 0.0);
        assert_eq!(frame_rms(&[0.0; 480]), 0.0);
        assert_eq!(frame_rms(&[0.5; 480]), 0.5);
        assert_eq!(frame_rms(&[3.0, -4.0]), (12.5f32).sqrt());
    }

    #[test]
    fn basic_functionality() {
        let config = NetEqConfig::default();
        let neteq = NetEq::new(config).unwrap();

        // Test basic operations
        assert_eq!(neteq.target_delay_ms(), 80); // Now starts with kStartDelayMs
        assert!(neteq.is_empty());
    }
}

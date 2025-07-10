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

//! Safari-compatible Opus decoder implementation
//!
//! This decoder provides test audio generation for Safari to verify the audio pipeline.
//! Future enhancement will add WebAssembly Opus decoding from decoderWorker.min.js.

use super::AudioDecoder;
use crate::Result;

/// Safari-compatible Opus decoder
///
/// This decoder generates test audio to verify the audio pipeline is working.
/// Future enhancement will add real WebAssembly Opus decoding.
pub struct SafariOpusDecoder {
    sample_rate: u32,
    channels: u8,
    frame_counter: u32,
}

impl SafariOpusDecoder {
    /// Create a new Safari Opus decoder
    pub async fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        Ok(Self {
            sample_rate,
            channels,
            frame_counter: 0,
        })
    }

    /// Create a new Safari Opus decoder with audio playback capability
    pub async fn new_with_playback(
        sample_rate: u32,
        channels: u8,
        _audio_context: Option<&web_sys::AudioContext>,
    ) -> Result<Self> {
        // For now, ignore the audio context and just create a basic decoder
        Self::new(sample_rate, channels).await
    }

    /// Get the decoder type identifier
    pub fn decoder_type(&self) -> &'static str {
        "Safari Test Audio Generator"
    }

    /// Initialize audio playback (stub for compatibility)
    pub async fn init_audio_playback(&self, _audio_context: &web_sys::AudioContext) -> Result<()> {
        web_sys::console::log_1(
            &"Safari decoder: Audio playback initialization (test mode)".into(),
        );
        Ok(())
    }

    /// Flush audio (stub for compatibility)
    pub fn flush_audio(&self) -> Result<()> {
        Ok(())
    }
}

impl AudioDecoder for SafariOpusDecoder {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn decode(&mut self, encoded: &[u8]) -> Result<Vec<f32>> {
        // Calculate expected output size for 20ms frame
        let samples_per_frame = (self.sample_rate * 20 / 1000) as usize;
        let total_samples = samples_per_frame * self.channels as usize;

        // Generate test audio - a 440Hz sine wave (A4 note)
        let mut samples = vec![0.0; total_samples];
        let frequency = 440.0; // Hz
        let amplitude = 0.1; // Reduced amplitude to avoid distortion

        for i in 0..samples_per_frame {
            let time = (self.frame_counter as usize * samples_per_frame + i) as f32
                / self.sample_rate as f32;
            let sample_value = amplitude * (2.0 * std::f32::consts::PI * frequency * time).sin();

            // Fill all channels with the same value
            for channel in 0..self.channels as usize {
                samples[i * self.channels as usize + channel] = sample_value;
            }
        }

        self.frame_counter += 1;

        web_sys::console::log_1(
            &format!(
                "Safari decoder: Processing {} bytes, generating {} samples (test tone)",
                encoded.len(),
                total_samples
            )
            .into(),
        );

        Ok(samples)
    }
}

// Mark as thread-safe for use in web workers
unsafe impl Send for SafariOpusDecoder {}
unsafe impl Sync for SafariOpusDecoder {}

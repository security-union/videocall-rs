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

//! Shared audio tuning constants used by both the encoder-side (local
//! microphone VAD) and the decoder-side (remote peer VAD) paths.
//!
//! Centralising these values ensures that tuning any audio parameter means
//! changing **one** constant definition.

/// RMS level below which audio is considered silence (0.0–1.0 normalized).
/// 0.01 is quite sensitive, 0.05 filters out most background noise.
pub const DEFAULT_VAD_THRESHOLD: f32 = 0.002;

/// RMS level that maps to maximum glow intensity (1.0). Normal speech peaks ~0.05–0.15.
/// Anything above this ceiling is clamped to 1.0.
pub const RMS_LOUD_SPEECH_CEILING: f32 = 0.10;

/// Minimum change in computed audio intensity before emitting an update event.
/// Prevents excessive event emissions while maintaining smooth visual updates.
pub const AUDIO_LEVEL_DELTA_THRESHOLD: f32 = 0.02;

/// Minimum change in audio level before triggering a UI re-render.
/// Tighter than `AUDIO_LEVEL_DELTA_THRESHOLD` (used codec-side) to ensure
/// smooth visual transitions while still suppressing no-op updates.
pub const UI_AUDIO_LEVEL_DELTA: f32 = 0.01;

/// How often (in ms) the local microphone VAD analysis runs.
pub const VAD_POLL_INTERVAL_MS: u32 = 100;

/// FFT size for the Web Audio AnalyserNode used in local microphone VAD.
pub const VAD_FFT_SIZE: u32 = 2048;

/// Smoothing time constant for the Web Audio AnalyserNode (0.0–1.0).
pub const VAD_SMOOTHING_TIME_CONSTANT: f64 = 0.8;

/// Convert raw RMS energy to a perceptual intensity value (0.0–1.0).
/// Uses sqrt curve for perceptual loudness mapping.
pub fn rms_to_intensity(rms: f32, vad_threshold: f32) -> f32 {
    let range = (RMS_LOUD_SPEECH_CEILING - vad_threshold).max(f32::EPSILON);
    let linear = if rms < vad_threshold {
        0.0_f32
    } else {
        ((rms - vad_threshold) / range).clamp(0.0, 1.0)
    };
    linear.sqrt()
}

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


use crate::statistics::TimeStretchOperation;
use crate::{NetEqError, Result};

/// Return codes for time-stretching operations
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimeStretchResult {
    Success,
    SuccessLowEnergy,
    NoStretch,
    Error,
}

/// Audio time-stretching processor
pub trait TimeStretcher {
    /// Process audio samples and return the result
    fn process(
        &mut self,
        input: &[f32],
        output: &mut Vec<f32>,
        fast_mode: bool,
    ) -> TimeStretchResult;

    /// Get the length change from the last operation
    fn get_length_change_samples(&self) -> usize;

    /// Reset the time stretcher state
    fn reset(&mut self);
}

/// Accelerate algorithm - removes audio samples to speed up playback
#[derive(Debug)]
pub struct Accelerate {
    sample_rate: u32,
    channels: u8,
    length_change_samples: usize,
    overlap_length: usize,
    max_change_rate: f32,
}

impl Accelerate {
    /// Create a new accelerate processor
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        Self {
            sample_rate,
            channels,
            length_change_samples: 0,
            overlap_length: Self::calculate_overlap_length(sample_rate),
            max_change_rate: 0.25, // Maximum 25% reduction
        }
    }

    fn calculate_overlap_length(sample_rate: u32) -> usize {
        // Calculate overlap length based on sample rate (typically 4-6ms)
        ((sample_rate as f32 * 0.005) as usize).max(32) // Minimum 32 samples
    }

    /// Accelerate the audio by removing samples
    fn accelerate_internal(
        &mut self,
        input: &[f32],
        output: &mut Vec<f32>,
        fast_mode: bool,
    ) -> TimeStretchResult {
        if input.len() < self.overlap_length * 2 {
            // Not enough samples to accelerate
            output.extend_from_slice(input);
            return TimeStretchResult::NoStretch;
        }

        // Calculate energy of the input signal
        let energy = self.calculate_energy(input);
        let is_low_energy = energy < 0.01; // Threshold for low energy detection

        // Determine how much to accelerate based on mode and energy
        let acceleration_factor = if fast_mode {
            if is_low_energy {
                0.4 // More aggressive for low energy in fast mode
            } else {
                0.25 // Standard fast mode acceleration
            }
        } else {
            if is_low_energy {
                0.2 // Gentle acceleration for low energy
            } else {
                0.15 // Conservative acceleration for normal energy
            }
        };

        let samples_to_remove = (input.len() as f32 * acceleration_factor) as usize;
        let samples_to_remove = samples_to_remove.min(input.len() / 3); // Don't remove more than 1/3

        if samples_to_remove < self.overlap_length {
            // Not enough to remove meaningfully
            output.extend_from_slice(input);
            return TimeStretchResult::NoStretch;
        }

        // Find the best location to remove samples (low energy region)
        let remove_start = self.find_best_removal_point(input, samples_to_remove);

        // Copy the first part
        output.extend_from_slice(&input[..remove_start]);

        // Apply crossfade at the join point
        let crossfade_start = remove_start + samples_to_remove;
        if crossfade_start + self.overlap_length <= input.len() {
            self.apply_crossfade(
                &input[remove_start - self.overlap_length..remove_start],
                &input[crossfade_start..crossfade_start + self.overlap_length],
                output,
            );

            // Copy the remaining part
            output.extend_from_slice(&input[crossfade_start + self.overlap_length..]);
        } else {
            // Not enough space for crossfade, just copy
            output.extend_from_slice(&input[crossfade_start..]);
        }

        self.length_change_samples = samples_to_remove;

        if is_low_energy {
            TimeStretchResult::SuccessLowEnergy
        } else {
            TimeStretchResult::Success
        }
    }

    fn calculate_energy(&self, samples: &[f32]) -> f32 {
        let sum_squares: f32 = samples.iter().map(|x| x * x).sum();
        sum_squares / samples.len() as f32
    }

    fn find_best_removal_point(&self, input: &[f32], removal_length: usize) -> usize {
        let mut best_position = input.len() / 3; // Default to middle-ish
        let mut lowest_energy = f32::MAX;

        // Search for the lowest energy region to remove
        let search_start = self.overlap_length;
        let search_end = input
            .len()
            .saturating_sub(removal_length + self.overlap_length);

        for pos in (search_start..search_end).step_by(self.overlap_length / 2) {
            let end_pos = (pos + removal_length).min(input.len());
            let region_energy = self.calculate_energy(&input[pos..end_pos]);

            if region_energy < lowest_energy {
                lowest_energy = region_energy;
                best_position = pos;
            }
        }

        best_position
    }

    fn apply_crossfade(&self, fade_out: &[f32], fade_in: &[f32], output: &mut Vec<f32>) {
        let len = fade_out.len().min(fade_in.len());

        for i in 0..len {
            let fade_factor = i as f32 / len as f32;
            let sample = fade_out[i] * (1.0 - fade_factor) + fade_in[i] * fade_factor;
            output.push(sample);
        }
    }
}

impl TimeStretcher for Accelerate {
    fn process(
        &mut self,
        input: &[f32],
        output: &mut Vec<f32>,
        fast_mode: bool,
    ) -> TimeStretchResult {
        self.accelerate_internal(input, output, fast_mode)
    }

    fn get_length_change_samples(&self) -> usize {
        self.length_change_samples
    }

    fn reset(&mut self) {
        self.length_change_samples = 0;
    }
}

/// Preemptive Expand algorithm - adds audio samples to slow down playback
#[derive(Debug)]
pub struct PreemptiveExpand {
    sample_rate: u32,
    channels: u8,
    length_change_samples: usize,
    overlap_length: usize,
    max_expansion_rate: f32,
}

impl PreemptiveExpand {
    /// Create a new preemptive expand processor
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        Self {
            sample_rate,
            channels,
            length_change_samples: 0,
            overlap_length: Self::calculate_overlap_length(sample_rate),
            max_expansion_rate: 0.25, // Maximum 25% expansion
        }
    }

    fn calculate_overlap_length(sample_rate: u32) -> usize {
        // Calculate overlap length based on sample rate (typically 4-6ms)
        ((sample_rate as f32 * 0.005) as usize).max(32) // Minimum 32 samples
    }

    /// Expand the audio by duplicating/stretching samples
    fn expand_internal(
        &mut self,
        input: &[f32],
        output: &mut Vec<f32>,
        fast_mode: bool,
    ) -> TimeStretchResult {
        if input.len() < self.overlap_length * 2 {
            // Not enough samples to expand
            output.extend_from_slice(input);
            return TimeStretchResult::NoStretch;
        }

        // Calculate energy of the input signal
        let energy = self.calculate_energy(input);
        let is_low_energy = energy < 0.01; // Threshold for low energy detection

        // Determine how much to expand based on mode and energy
        let expansion_factor = if fast_mode {
            if is_low_energy {
                0.3 // More expansion for low energy in fast mode
            } else {
                0.2 // Standard fast mode expansion
            }
        } else {
            if is_low_energy {
                0.15 // Gentle expansion for low energy
            } else {
                0.1 // Conservative expansion for normal energy
            }
        };

        let samples_to_add = (input.len() as f32 * expansion_factor) as usize;

        if samples_to_add < self.overlap_length {
            // Not enough to add meaningfully
            output.extend_from_slice(input);
            return TimeStretchResult::NoStretch;
        }

        // Find the best location to duplicate samples (repeatable patterns)
        let duplicate_start = self.find_best_duplication_point(input, samples_to_add);

        // Copy the first part
        output.extend_from_slice(&input[..duplicate_start]);

        // Duplicate the selected region with crossfading
        let duplicate_end = (duplicate_start + samples_to_add).min(input.len());
        let duplicate_region = &input[duplicate_start..duplicate_end];

        // Apply the duplicated region with overlap-add
        self.apply_overlap_add(duplicate_region, output);

        // Copy the remaining part
        output.extend_from_slice(&input[duplicate_start..]);

        self.length_change_samples = samples_to_add;

        if is_low_energy {
            TimeStretchResult::SuccessLowEnergy
        } else {
            TimeStretchResult::Success
        }
    }

    fn calculate_energy(&self, samples: &[f32]) -> f32 {
        let sum_squares: f32 = samples.iter().map(|x| x * x).sum();
        sum_squares / samples.len() as f32
    }

    fn find_best_duplication_point(&self, input: &[f32], duplication_length: usize) -> usize {
        // Look for periodic patterns that can be duplicated naturally
        let mut best_position = input.len() / 2; // Default to middle
        let mut best_correlation = -1.0;

        let search_start = self.overlap_length;
        let search_end = input
            .len()
            .saturating_sub(duplication_length + self.overlap_length);

        for pos in (search_start..search_end).step_by(self.overlap_length / 2) {
            let end_pos = (pos + duplication_length).min(input.len());

            // Calculate autocorrelation to find periodic patterns
            let correlation = self.calculate_autocorrelation(&input[pos..end_pos]);

            if correlation > best_correlation {
                best_correlation = correlation;
                best_position = pos;
            }
        }

        best_position
    }

    fn calculate_autocorrelation(&self, samples: &[f32]) -> f32 {
        if samples.len() < 2 {
            return 0.0;
        }

        let half_len = samples.len() / 2;
        let first_half = &samples[..half_len];
        let second_half = &samples[half_len..half_len * 2];

        let mut correlation = 0.0;
        for i in 0..half_len {
            correlation += first_half[i] * second_half[i];
        }

        correlation / half_len as f32
    }

    fn apply_overlap_add(&self, duplicate_region: &[f32], output: &mut Vec<f32>) {
        // Simple overlap-add with triangular window
        let overlap_len = self.overlap_length.min(duplicate_region.len());

        // Add the duplicated region with fade in/out
        for (i, &sample) in duplicate_region.iter().enumerate() {
            let fade_factor = if i < overlap_len {
                i as f32 / overlap_len as f32 // Fade in
            } else if i >= duplicate_region.len() - overlap_len {
                (duplicate_region.len() - i) as f32 / overlap_len as f32 // Fade out
            } else {
                1.0 // Full volume
            };

            output.push(sample * fade_factor);
        }
    }
}

impl TimeStretcher for PreemptiveExpand {
    fn process(
        &mut self,
        input: &[f32],
        output: &mut Vec<f32>,
        fast_mode: bool,
    ) -> TimeStretchResult {
        self.expand_internal(input, output, fast_mode)
    }

    fn get_length_change_samples(&self) -> usize {
        self.length_change_samples
    }

    fn reset(&mut self) {
        self.length_change_samples = 0;
    }
}

/// Time stretch factory for creating time stretchers
pub struct TimeStretchFactory;

impl TimeStretchFactory {
    /// Create an accelerate processor
    pub fn create_accelerate(sample_rate: u32, channels: u8) -> Box<dyn TimeStretcher> {
        Box::new(Accelerate::new(sample_rate, channels))
    }

    /// Create a preemptive expand processor  
    pub fn create_preemptive_expand(sample_rate: u32, channels: u8) -> Box<dyn TimeStretcher> {
        Box::new(PreemptiveExpand::new(sample_rate, channels))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generate_test_signal(length: usize, frequency: f32, sample_rate: u32) -> Vec<f32> {
        (0..length)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                (2.0 * std::f32::consts::PI * frequency * t).sin() * 0.5
            })
            .collect()
    }

    #[test]
    fn test_accelerate_creation() {
        let accelerate = Accelerate::new(16000, 1);
        assert_eq!(accelerate.sample_rate, 16000);
        assert_eq!(accelerate.channels, 1);
        assert_eq!(accelerate.get_length_change_samples(), 0);
    }

    #[test]
    fn test_accelerate_processing() {
        let mut accelerate = Accelerate::new(16000, 1);
        let input = generate_test_signal(1600, 440.0, 16000); // 100ms of 440Hz tone
        let mut output = Vec::new();

        let result = accelerate.process(&input, &mut output, false);

        // Should successfully accelerate
        assert!(matches!(
            result,
            TimeStretchResult::Success | TimeStretchResult::SuccessLowEnergy
        ));
        // Output should be shorter than input
        assert!(output.len() < input.len());
        // Should have recorded length change
        assert!(accelerate.get_length_change_samples() > 0);
    }

    #[test]
    fn test_preemptive_expand_creation() {
        let expand = PreemptiveExpand::new(16000, 1);
        assert_eq!(expand.sample_rate, 16000);
        assert_eq!(expand.channels, 1);
        assert_eq!(expand.get_length_change_samples(), 0);
    }

    #[test]
    fn test_preemptive_expand_processing() {
        let mut expand = PreemptiveExpand::new(16000, 1);
        let input = generate_test_signal(1600, 440.0, 16000); // 100ms of 440Hz tone
        let mut output = Vec::new();

        let result = expand.process(&input, &mut output, false);

        // Should successfully expand
        assert!(matches!(
            result,
            TimeStretchResult::Success | TimeStretchResult::SuccessLowEnergy
        ));
        // Output should be longer than input
        assert!(output.len() > input.len());
        // Should have recorded length change
        assert!(expand.get_length_change_samples() > 0);
    }

    #[test]
    fn test_insufficient_input() {
        let mut accelerate = Accelerate::new(16000, 1);
        let input = vec![0.0; 10]; // Very short input
        let mut output = Vec::new();

        let result = accelerate.process(&input, &mut output, false);

        // Should not stretch due to insufficient input
        assert_eq!(result, TimeStretchResult::NoStretch);
        assert_eq!(output.len(), input.len());
        assert_eq!(accelerate.get_length_change_samples(), 0);
    }

    #[test]
    fn test_time_stretch_factory() {
        let accelerate = TimeStretchFactory::create_accelerate(16000, 1);
        let expand = TimeStretchFactory::create_preemptive_expand(16000, 1);

        // Should create valid processors
        assert_eq!(accelerate.get_length_change_samples(), 0);
        assert_eq!(expand.get_length_change_samples(), 0);
    }

    #[test]
    fn test_reset_functionality() {
        let mut accelerate = Accelerate::new(16000, 1);
        let input = generate_test_signal(1600, 440.0, 16000);
        let mut output = Vec::new();

        // Process to change state
        accelerate.process(&input, &mut output, false);
        assert!(accelerate.get_length_change_samples() > 0);

        // Reset should clear state
        accelerate.reset();
        assert_eq!(accelerate.get_length_change_samples(), 0);
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExpandPhase {
    /// Expand operation (concealment)
    Expand,
    /// First expand in a row
    ExpandStart,
    /// Last expand in a row
    ExpandEnd,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExpandResult {
    Success,
    NoExpand,
}

#[derive(Debug)]
pub struct Expand {
    used_input_samples: usize,
    overlap_length: usize,
    rng_state: u64,
}

pub struct ExpandFactory;

impl ExpandFactory {
    /// Create an expand processor
    pub fn create_expand(sample_rate: u32, _channels: u8) -> Box<Expand> {
        Box::new(Expand::new(sample_rate))
    }
}

impl Expand {
    /// Create a new expand processor
    pub fn new(sample_rate: u32) -> Self {
        Self {
            used_input_samples: 0,
            overlap_length: Self::calculate_overlap_length(sample_rate),
            rng_state: 1,
        }
    }

    pub fn samples_required(&self, phase: ExpandPhase) -> usize {
        if phase == ExpandPhase::Expand {
            return 0;
        }

        self.overlap_length
    }

    pub fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        phase: ExpandPhase,
    ) -> ExpandResult {
        let samples_required = self.samples_required(phase);
        if input.len() < samples_required || output.len() < samples_required {
            let samples = input.len().min(output.len());
            output[..samples].copy_from_slice(&input[..samples]);
            self.used_input_samples = samples;
            return ExpandResult::NoExpand;
        }

        // Generate concealment audio (simple noise for now)
        for sample in output.iter_mut() {
            *sample = (self.simple_random() - 0.5) * 0.0001; // Very quiet noise
        }

        if phase == ExpandPhase::ExpandStart {
            let start_of_frame = output[..self.overlap_length].to_vec();

            // Crossfade with end of audio
            crate::signal::crossfade(
                &input[..self.overlap_length],
                &start_of_frame,
                self.overlap_length,
                &mut output[..self.overlap_length],
            );
        } else if phase == ExpandPhase::ExpandEnd {
            let end_of_frame_start = output.len() - self.overlap_length;
            let end_of_frame = output[end_of_frame_start..].to_vec();

            // Crossfade with end of audio
            crate::signal::crossfade(
                &end_of_frame,
                &input[..self.overlap_length],
                self.overlap_length,
                &mut output[end_of_frame_start..],
            );
        }

        self.used_input_samples = samples_required;

        ExpandResult::Success
    }

    fn calculate_overlap_length(sample_rate: u32) -> usize {
        // Calculate overlap length based on sample rate (typically 4-6ms)
        ((sample_rate as f32 * 0.003) as usize).max(32) // Minimum 32 samples
    }

    pub fn get_used_input_samples(&self) -> usize {
        self.used_input_samples
    }

    fn simple_random(&mut self) -> f32 {
        self.rng_state ^= self.rng_state << 13;
        self.rng_state ^= self.rng_state >> 7;
        self.rng_state ^= self.rng_state << 17;
        ((self.rng_state as u32) >> 16) as f32 / 65536.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calculate_energy(samples: &[f32]) -> f32 {
        let sum_squares: f32 = samples.iter().map(|x| x * x).sum();
        sum_squares / samples.len() as f32
    }

    #[test]
    fn test_expand_creation() {
        let expand = Expand::new(16000);
        assert_eq!(expand.overlap_length, 48);
    }

    #[test]
    fn test_expand_processing() {
        let mut expand = Expand::new(16000);

        let input = vec![];
        let mut output = vec![0.0; 800];

        // Sanity check for zero energy signal
        let output_energy = calculate_energy(&output);
        assert!(output_energy < 0.00000000000000000001);

        let result = expand.process(&input, &mut output, ExpandPhase::Expand);

        // Should successfully generate concealment noise
        assert!(matches!(result, ExpandResult::Success));
        // Should have recorded the number of used samples
        assert!(expand.get_used_input_samples() == 0);
        // Should output non-silent signal
        let output_energy = calculate_energy(&output);
        assert!(output_energy > 0.000000000000001);
    }

    #[test]
    fn test_expand_start() {
        let mut expand = Expand::new(16000);

        let input = vec![1.0; 48];
        let mut output = vec![0.0; 800];

        // Sanity check for zero energy signal
        let output_energy = calculate_energy(&output);
        assert!(output_energy < 0.00000000000000000001);

        let result = expand.process(&input, &mut output, ExpandPhase::ExpandStart);

        // Should successfully crossfade into concealment
        assert!(matches!(result, ExpandResult::Success));
        // Should have recorded the number of used samples
        assert!(expand.get_used_input_samples() == 48);
        // Should output non-silent signal
        let output_energy = calculate_energy(&output);
        assert!(output_energy > 0.000000000000001);

        // Beginning of the signal should have higher energy than the rest
        let start_energy = calculate_energy(&output[..48]);
        assert!(start_energy > 0.001);
        let other_energy = calculate_energy(&output[48..]);
        assert!(other_energy < 0.00001);
    }

    #[test]
    fn test_expand_end() {
        let mut expand = Expand::new(16000);

        let input = vec![1.0; 48];
        let mut output = vec![0.0; 800];

        // Sanity check for zero energy signal
        let output_energy = calculate_energy(&output);
        assert!(output_energy < 0.00000000000000000001);

        let result = expand.process(&input, &mut output, ExpandPhase::ExpandEnd);

        // Should successfully crossfade out of concealment
        assert!(matches!(result, ExpandResult::Success));
        // Should have recorded the number of used samples
        assert!(expand.get_used_input_samples() == 48);
        // Should output non-silent signal
        let output_energy = calculate_energy(&output);
        assert!(output_energy > 0.000000000000001);

        // End of the signal should have higher energy than the rest
        let end_start = output.len() - 48;
        let end_energy = calculate_energy(&output[end_start..]);
        assert!(end_energy > 0.001);
        let other_energy = calculate_energy(&output[..end_start]);
        assert!(other_energy < 0.00001);
    }
}

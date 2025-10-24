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
    fn process(&mut self, input: &[f32], output: &mut [f32], fast_mode: bool) -> TimeStretchResult;

    // Returns the number of input samples used in the last operation
    fn get_used_input_samples(&self) -> usize;

    /// Reset the time stretcher state
    fn reset(&mut self);
}

/// Accelerate algorithm - removes audio samples to speed up playback
#[derive(Debug)]
pub struct Accelerate {
    _sample_rate: u32,
    _channels: u8,
    used_input_samples: usize,
    overlap_length: usize,
    _max_change_rate: f32,
}

impl Accelerate {
    /// Create a new accelerate processor
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        Self {
            _sample_rate: sample_rate,
            _channels: channels,
            used_input_samples: 0,
            overlap_length: Self::calculate_overlap_length(sample_rate),
            _max_change_rate: 0.25, // Maximum 25% reduction
        }
    }

    fn calculate_overlap_length(sample_rate: u32) -> usize {
        // Calculate overlap length based on sample rate (typically 4-6ms)
        ((sample_rate as f32 * 0.003) as usize).max(32) // Minimum 32 samples
    }

    /// Accelerate the audio by removing samples
    fn accelerate_internal(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        fast_mode: bool,
    ) -> TimeStretchResult {
        if input.len() <= output.len() {
            // Input should always be longer then output for accelerate
            output[..input.len()].copy_from_slice(input);
            self.used_input_samples = input.len();
            return TimeStretchResult::NoStretch;
        }

        if output.len() < self.overlap_length * 2 {
            // Not enough samples to accelerate
            output.copy_from_slice(&input[..output.len()]);
            self.used_input_samples = output.len();
            return TimeStretchResult::NoStretch;
        }

        let max_remove_low_energy = if fast_mode {
            self.max_samples_to_remove(input, output, 0.4)
        } else {
            self.max_samples_to_remove(input, output, 0.2)
        };
        let max_remove_high_energy = if fast_mode {
            self.max_samples_to_remove(input, output, 0.25)
        } else {
            // Don't use high energy remove in normal mode
            0
        };

        let (mut best_pos, mut samples_to_remove) =
            self.find_low_energy_to_remove(input, output, max_remove_low_energy);

        let mut is_low_energy = true;
        if !fast_mode {
            if samples_to_remove == 0 {
                // If no low-energy zone found, don't remove anything in normal mode
                output.copy_from_slice(&input[..output.len()]);
                self.used_input_samples = output.len();
                return TimeStretchResult::NoStretch;
            }
        } else if samples_to_remove < max_remove_high_energy {
            // In fast mode, always remove at least max_remove_high_energy samples
            is_low_energy = false;
            samples_to_remove = max_remove_high_energy;

            // Find the best location to remove samples (lowest energy region)
            best_pos = self.find_best_removal_point(
                &input[..output.len() + samples_to_remove],
                samples_to_remove,
            );
        }

        let usable_input = &input[..output.len() + samples_to_remove];

        // original samples: 12346789
        // samples_to_remove = 4, overlap_length = 2
        // original:     |123456789|
        // head+overlap: |1234 | // head_length = 2
        // overlap+tail: |  789| // tail_start = 6
        // result:       |12**9| // 2 samples overlapped
        // head_length+overlap_length+(usable_input.len()-tail_start-overlap_length) = output.len()
        // head_length+usable_input.len()-tail_start = output.len()
        // tail_start= head_length+usable_input.len()-output.len() = head_length+(output.len()+samples_to_remove)-output.len() = head_length+samples_to_remove

        // Copy the first part
        output[..best_pos].copy_from_slice(&usable_input[..best_pos]);

        crate::signal::crossfade(
            &usable_input[best_pos..best_pos + self.overlap_length],
            &usable_input
                [best_pos + samples_to_remove..best_pos + samples_to_remove + self.overlap_length],
            self.overlap_length,
            &mut output[best_pos..best_pos + self.overlap_length],
        );

        // Copy the rest
        output[best_pos + self.overlap_length..]
            .copy_from_slice(&usable_input[best_pos + samples_to_remove + self.overlap_length..]);

        self.used_input_samples = usable_input.len();

        if is_low_energy {
            TimeStretchResult::SuccessLowEnergy
        } else {
            TimeStretchResult::Success
        }
    }

    fn max_samples_to_remove(
        &mut self,
        input: &[f32],
        output: &[f32],
        acceleration_factor: f32,
    ) -> usize {
        let max_remove = (output.len() as f32 * acceleration_factor) as usize;
        let max_remove = max_remove.min(output.len() / 2); // Don't remove more than 1/3 of input (= 1/2 of output)
        let max_remove = max_remove.min(input.len() - output.len());
        let max_remove = max_remove.max(self.overlap_length);

        max_remove
    }

    fn find_low_energy_to_remove(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        max_remove: usize,
    ) -> (usize, usize) {
        let usable_input = &input[..output.len() + max_remove];

        let (best_pos, best_len) =
            Self::longest_low_energy_region(usable_input, 0.001, |i: usize, len: usize| -> bool {
                i + len.saturating_sub(self.overlap_length).min(max_remove) <= output.len()
            });

        let samples_to_remove = best_len.saturating_sub(self.overlap_length).min(max_remove);

        if samples_to_remove < self.overlap_length {
            return (0, 0);
        }

        (best_pos, samples_to_remove)
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

    fn longest_low_energy_region<F>(
        input: &[f32],
        threshold: f32,
        validate_candidate: F,
    ) -> (usize, usize)
    where
        F: Fn(usize, usize) -> bool, // closure type
    {
        // Step 1: define energy as x^2. Compute the rolling sum of the energy deviation from
        // threshold. In this rolling sum, if rolling_sum[j] - rolling_sum[j] <= 0, that means
        // that the sum of the deviation from threshold between i and j is less then 0,
        // which means that AVG(a[i..j]) <= threshold
        // This algorithm finds the the largest range where that stands. The constainst
        // validate_candidate is used to make sure that the final output size is correct.
        let mut energy_deviation_rolling_sum = Vec::with_capacity(input.len() + 1);
        energy_deviation_rolling_sum.push(0.0);
        for &x in input {
            energy_deviation_rolling_sum
                .push(energy_deviation_rolling_sum.last().unwrap() + x * x - threshold);
        }

        // Step 2: build increasing peaks stack
        // Rationale: if the range [i..j] statisfies the condition that rolling_sum[j] - rolling_sum[j] <= 0
        // then any i' < i, where rolling_sum[i'] > rolling_sum[i] will also statisfy the condition.
        // In other words, we need to find candidates that do not have a previous higher value.
        let mut stack = Vec::new(); // stores indices of rolling_sum
        for (i, &val) in energy_deviation_rolling_sum.iter().enumerate() {
            if stack.is_empty() || val > energy_deviation_rolling_sum[*stack.last().unwrap()] {
                stack.push(i);
            }
        }

        // Step 3: reverse scan to find longest valid subarray
        let mut max_len = 0;
        let mut best_i = 0;

        for j in (0..energy_deviation_rolling_sum.len()).rev() {
            while stack.len() > 0 && stack[stack.len() - 1] >= j {
                stack.pop();
            }
            if stack.len() == 0 {
                break;
            }
            let mut si = stack.len();
            while si > 0 {
                si -= 1;
                let i = stack[si];
                // Find the smallest i where the condition holds
                if energy_deviation_rolling_sum[j] <= energy_deviation_rolling_sum[i] {
                    let len = j - i;
                    if len > max_len && validate_candidate(i, len) {
                        max_len = len;
                        best_i = i;
                        stack.truncate(si);
                    }
                } else {
                    break;
                }
            }
        }

        (best_i, max_len)
    }
}

// Allow Accelerate to be moved across threads (contains only primitive data, no interior mutability).
unsafe impl Send for Accelerate {}

impl TimeStretcher for Accelerate {
    fn process(&mut self, input: &[f32], output: &mut [f32], fast_mode: bool) -> TimeStretchResult {
        self.accelerate_internal(input, output, fast_mode)
    }

    fn get_used_input_samples(&self) -> usize {
        self.used_input_samples
    }

    fn reset(&mut self) {
        self.used_input_samples = 0;
    }
}

/// Preemptive Expand algorithm - adds audio samples to slow down playback
#[derive(Debug)]
pub struct PreemptiveExpand {
    _sample_rate: u32,
    _channels: u8,
    used_input_samples: usize,
    overlap_length: usize,
    max_expansion_rate: f32,
}

impl PreemptiveExpand {
    /// Create a new preemptive expand processor
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        Self {
            _sample_rate: sample_rate,
            _channels: channels,
            used_input_samples: 0,
            overlap_length: Self::calculate_overlap_length(sample_rate),
            max_expansion_rate: 0.25, // Maximum 25% expansion
        }
    }

    fn calculate_overlap_length(sample_rate: u32) -> usize {
        // Calculate overlap length based on sample rate (typically 4-6ms)
        ((sample_rate as f32 * 0.003) as usize).max(32) // Minimum 32 samples
    }

    /// Expand the audio by duplicating/stretching samples
    fn expand_internal(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        _fast_mode: bool,
    ) -> TimeStretchResult {
        if input.len() < output.len() {
            // Input should always be at least the length of output
            output[..input.len()].copy_from_slice(input);
            self.used_input_samples = input.len();
            return TimeStretchResult::NoStretch;
        }

        // Calculate energy of the input signal
        let energy = self.calculate_energy(input);
        let is_low_energy = energy < 0.01; // Threshold for low energy detection

        // Target to add 25% of frame length but cap by max_expansion_rate
        let max_add = (output.len() as f32 / (1.0 + 1.0 / self.max_expansion_rate)) as usize;
        if max_add < self.overlap_length {
            // Not enough samples to expand
            output.copy_from_slice(&input[..output.len()]);
            self.used_input_samples = output.len();
            return TimeStretchResult::NoStretch;
        }

        let mut best_corr2 = -1.0f32;
        let mut best_pos = 0;
        let mut best_add_len = 0;
        for add_len in self.overlap_length..max_add {
            let correlation_len = output.len() - add_len * 2 - self.overlap_length;
            let (pos, corr2) = crate::signal::best_normalized_correlation(
                &input[add_len..correlation_len + add_len],
                &input[..correlation_len],
                self.overlap_length,
            );
            if corr2 > best_corr2 {
                best_corr2 = corr2;
                best_pos = add_len + pos;
                best_add_len = add_len;
            }
        }

        let usable_input = &input[..output.len() - best_add_len];

        // Copy first part
        output[..best_pos].copy_from_slice(&usable_input[..best_pos]);

        // Crossfade duplicate region
        crate::signal::crossfade(
            &usable_input[best_pos..best_pos + self.overlap_length],
            &usable_input[best_pos - best_add_len..best_pos - best_add_len + self.overlap_length],
            self.overlap_length,
            &mut output[best_pos..best_pos + self.overlap_length],
        );

        // Append rest
        output[best_pos + self.overlap_length..]
            .copy_from_slice(&usable_input[best_pos - best_add_len + self.overlap_length..]);

        self.used_input_samples = usable_input.len();

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
}

// Safe because PreemptiveExpand only contains owned numeric data.
unsafe impl Send for PreemptiveExpand {}

impl TimeStretcher for PreemptiveExpand {
    fn process(&mut self, input: &[f32], output: &mut [f32], fast_mode: bool) -> TimeStretchResult {
        self.expand_internal(input, output, fast_mode)
    }

    fn get_used_input_samples(&self) -> usize {
        self.used_input_samples
    }

    fn reset(&mut self) {
        self.used_input_samples = 0;
    }
}

/// Time stretch factory for creating time stretchers
pub struct TimeStretchFactory;

impl TimeStretchFactory {
    /// Create an accelerate processor
    pub fn create_accelerate(sample_rate: u32, channels: u8) -> Box<dyn TimeStretcher + Send> {
        Box::new(Accelerate::new(sample_rate, channels))
    }

    /// Create a preemptive expand processor  
    pub fn create_preemptive_expand(
        sample_rate: u32,
        channels: u8,
    ) -> Box<dyn TimeStretcher + Send> {
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
        assert_eq!(accelerate._sample_rate, 16000);
        assert_eq!(accelerate._channels, 1);
    }

    #[test]
    fn test_accelerate_processing() {
        let mut accelerate = Accelerate::new(16000, 1);
        let input = generate_test_signal(1600, 440.0, 16000); // 100ms of 440Hz tone
        let mut output = vec![0.0; 800 as usize];

        let result = accelerate.process(&input, &mut output, true);

        // Should successfully accelerate
        assert!(matches!(
            result,
            TimeStretchResult::Success | TimeStretchResult::SuccessLowEnergy
        ));
        // Should have recorder the number of used samples
        assert!(accelerate.get_used_input_samples() > 0);
        // Output should be shorter than used samples
        assert!(output.len() < accelerate.get_used_input_samples());
    }

    #[test]
    fn test_preemptive_expand_creation() {
        let expand = PreemptiveExpand::new(16000, 1);
        assert_eq!(expand._sample_rate, 16000);
        assert_eq!(expand._channels, 1);
    }

    #[test]
    fn test_preemptive_expand_processing() {
        let mut expand = PreemptiveExpand::new(16000, 1);
        let input = generate_test_signal(1600, 440.0, 16000); // 100ms of 440Hz tone
        let mut output = vec![0.0; 800 as usize];

        let result = expand.process(&input, &mut output, false);

        // Should successfully expand
        assert!(matches!(
            result,
            TimeStretchResult::Success | TimeStretchResult::SuccessLowEnergy
        ));
        // Should have recorder the number of used samples
        assert!(expand.get_used_input_samples() > 0);
        // Should have used less samples than the output size
        assert!(expand.get_used_input_samples() < output.len());
    }

    #[test]
    fn test_insufficient_input() {
        let mut accelerate = Accelerate::new(16000, 1);
        let input = vec![1.0; 10]; // Very short input
        let mut output = vec![0.0; 20];

        let result = accelerate.process(&input, &mut output, false);

        // Should not stretch due to insufficient input
        assert_eq!(result, TimeStretchResult::NoStretch);
        assert_eq!(output[..10], input);
        assert_eq!(output[10..], vec![0.0; 10]);
    }

    #[test]
    fn test_time_stretch_factory() {
        let accelerate = TimeStretchFactory::create_accelerate(16000, 1);
        let expand = TimeStretchFactory::create_preemptive_expand(16000, 1);

        // Should create valid processors
        assert_eq!(accelerate.get_used_input_samples(), 0);
        assert_eq!(expand.get_used_input_samples(), 0);
    }

    #[test]
    fn test_reset_functionality() {
        let mut accelerate = Accelerate::new(16000, 1);
        let input = generate_test_signal(1600, 440.0, 16000);
        let mut output = vec![0.0; 800 as usize];

        // Process to change state
        accelerate.process(&input, &mut output, false);
        assert!(accelerate.get_used_input_samples() > 0);

        // Reset should clear state
        accelerate.reset();
        assert_eq!(accelerate.get_used_input_samples(), 0);
    }

    #[test]
    fn test_longest_low_energy_region() {
        let mut best_i: usize;
        let mut best_len: usize;

        let mut input: Vec<f32>;
        // energy:
        // 0.04, 0.01, 0.09, 0.01, 0.04, 0.04, 0.01, 0.09, 0.25, 0.25, 0.01
        // threshold avg: 0.03
        // expected energy: 0.01, 0.04, 0.04, 0.01
        // i = 3, len = 4
        input = vec![0.2, -0.1, 0.3, 0.1, 0.2, -0.2, 0.1, 0.3, 0.5, -0.5, 0.1];
        (best_i, best_len) =
            Accelerate::longest_low_energy_region(&input, 0.03, |_i: usize, _len: usize| -> bool {
                true
            });
        assert_eq!(best_i, 3);
        assert_eq!(best_len, 4);

        input = vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        (best_i, best_len) =
            Accelerate::longest_low_energy_region(&input, 0.03, |_i: usize, _len: usize| -> bool {
                true
            });
        assert_eq!(best_i, 0);
        assert_eq!(best_len, 8);

        input = vec![
            0.2, -0.1, 0.3, 0.1, 0.2, -0.2, 0.1, 0.3, 0.5, -0.5, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1,
        ];
        (best_i, best_len) =
            Accelerate::longest_low_energy_region(&input, 0.03, |_i: usize, _len: usize| -> bool {
                true
            });
        assert_eq!(best_i, 10);
        assert_eq!(best_len, 7);

        (best_i, best_len) =
            Accelerate::longest_low_energy_region(&input, 0.03, |i: usize, len: usize| -> bool {
                let max_output_len = 15;
                i + len <= max_output_len
            });
        assert_eq!(best_i, 10);
        assert_eq!(best_len, 5);

        (best_i, best_len) =
            Accelerate::longest_low_energy_region(&input, 0.03, |i: usize, len: usize| -> bool {
                let max_output_len = 13;
                i + len <= max_output_len
            });
        assert_eq!(best_i, 3);
        assert_eq!(best_len, 4);
    }
}

// Port target: WebRTC modules/audio_coding/neteq/background_noise.{h,cc} at
// rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd
// (tests: background_noise_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Background noise estimate (LPC filter + energy per channel), used to fill
//! long expansions.

use crate::neteq::audio_multi_vector::AudioMultiVector;
use crate::neteq::cross_correlation::cross_correlation_with_auto_shift;
use crate::spl;

/// C++ `kMaxSampleRate` (anonymous namespace in background_noise.cc).
const K_MAX_SAMPLE_RATE: usize = 48000;

/// C++ `BackgroundNoise::ChannelParameters`.
#[derive(Debug, Clone)]
struct ChannelParameters {
    energy: i32,
    max_energy: i32,
    energy_update_threshold: i32,
    low_energy_update_threshold: i32,
    filter_state: [i16; BackgroundNoise::K_MAX_LPC_ORDER],
    filter: [i16; BackgroundNoise::K_MAX_LPC_ORDER + 1],
    mute_factor: i16,
    scale: i16,
    scale_shift: i16,
}

impl ChannelParameters {
    fn new() -> Self {
        let mut parameters = Self {
            energy: 0,
            max_energy: 0,
            energy_update_threshold: 0,
            low_energy_update_threshold: 0,
            filter_state: [0; BackgroundNoise::K_MAX_LPC_ORDER],
            filter: [0; BackgroundNoise::K_MAX_LPC_ORDER + 1],
            mute_factor: 0,
            scale: 0,
            scale_shift: 0,
        };
        parameters.reset();
        parameters
    }

    fn reset(&mut self) {
        self.energy = 2500;
        self.max_energy = 0;
        self.energy_update_threshold = 500000;
        self.low_energy_update_threshold = 0;
        self.filter_state = [0; BackgroundNoise::K_MAX_LPC_ORDER];
        self.filter = [0; BackgroundNoise::K_MAX_LPC_ORDER + 1];
        self.filter[0] = 4096;
        self.mute_factor = 0;
        self.scale = 20000;
        self.scale_shift = 24;
    }
}

/// C++ `BackgroundNoise`.
#[derive(Debug)]
pub struct BackgroundNoise {
    num_channels: usize,
    channel_parameters: Vec<ChannelParameters>,
    initialized: bool,
}

impl BackgroundNoise {
    /// 32000 / 8000 + 4.
    pub const K_MAX_LPC_ORDER: usize = 8;

    /// 0.0035 in Q16.
    const K_THRESHOLD_INCREMENT: i32 = 229;
    const K_VEC_LEN: usize = 256;
    /// log2(kVecLen).
    const K_LOG_VEC_LEN: i32 = 8;
    const K_RESIDUAL_LENGTH: usize = 64;
    /// log2(kResidualLength).
    const K_LOG_RESIDUAL_LENGTH: i16 = 6;

    /// Estimator for `num_channels` channels.
    pub fn new(num_channels: usize) -> Self {
        let mut bgn = Self {
            num_channels,
            channel_parameters: vec![ChannelParameters::new(); num_channels],
            initialized: false,
        };
        bgn.reset();
        bgn
    }

    /// Resets all channel parameters.
    pub fn reset(&mut self) {
        self.initialized = false;
        for parameters in &mut self.channel_parameters {
            parameters.reset();
        }
    }

    /// Updates the estimate from the end of `sync_buffer`. Returns true if
    /// the filter parameters were updated.
    pub fn update(&mut self, sync_buffer: &AudioMultiVector) -> bool {
        let mut filter_params_saved = false;

        let mut auto_correlation = [0i32; Self::K_MAX_LPC_ORDER + 1];
        let mut filter_output = [0i16; Self::K_MAX_LPC_ORDER + Self::K_RESIDUAL_LENGTH];
        let mut reflection_coefficients = [0i16; Self::K_MAX_LPC_ORDER];
        let mut lpc_coefficients = [0i16; Self::K_MAX_LPC_ORDER + 1];

        for channel_ix in 0..self.num_channels {
            // `temp_signal` is `temp_signal_array + kMaxLpcOrder`; the leading
            // zeros are read by the backwards auto-correlation.
            let mut temp_signal_array = [0i16; Self::K_VEC_LEN + Self::K_MAX_LPC_ORDER];
            debug_assert!(sync_buffer.size() >= Self::K_VEC_LEN);
            sync_buffer[channel_ix].copy_to_slice(
                Self::K_VEC_LEN,
                sync_buffer.size() - Self::K_VEC_LEN,
                &mut temp_signal_array[Self::K_MAX_LPC_ORDER..],
            );
            let sample_energy = Self::calculate_auto_correlation(
                &temp_signal_array,
                Self::K_MAX_LPC_ORDER,
                Self::K_VEC_LEN,
                &mut auto_correlation,
            );
            let temp_signal = &temp_signal_array[Self::K_MAX_LPC_ORDER..];

            let parameters = &mut self.channel_parameters[channel_ix];
            if sample_energy < parameters.energy_update_threshold {
                // Generate LPC coefficients.
                if auto_correlation[0] <= 0 {
                    // Center value in auto-correlation is not positive. Do not update.
                    return filter_params_saved;
                }

                // Regardless of whether the filter is actually updated or not,
                // update energy threshold levels, since we have in fact observed
                // a low energy signal.
                if sample_energy < parameters.energy_update_threshold {
                    // Never go under 1.0 in average sample energy.
                    parameters.energy_update_threshold = sample_energy.max(1);
                    parameters.low_energy_update_threshold = 0;
                }

                // Only update BGN if filter is stable, i.e., if return value from
                // Levinson-Durbin function is 1.
                if spl::levinson_durbin(
                    &auto_correlation,
                    &mut lpc_coefficients,
                    &mut reflection_coefficients,
                    Self::K_MAX_LPC_ORDER,
                ) != 1
                {
                    return filter_params_saved;
                }

                // Generate the CNG gain factor by looking at the energy of the residual.
                spl::filter_ma_fast_q12(
                    temp_signal,
                    Self::K_VEC_LEN - Self::K_RESIDUAL_LENGTH,
                    &mut filter_output,
                    &lpc_coefficients,
                    Self::K_MAX_LPC_ORDER + 1,
                    Self::K_RESIDUAL_LENGTH,
                );
                let residual_energy = spl::dot_product_with_scale(
                    &filter_output,
                    &filter_output,
                    Self::K_RESIDUAL_LENGTH,
                    0,
                );

                // Check spectral flatness.
                // Comparing the residual variance with the input signal variance tells
                // if the spectrum is flat or not.
                // If 5 * residual_energy >= 16 * sample_energy, the spectrum is flat
                // enough.  Also ensure that the energy is non-zero.
                if sample_energy > 0 && 5 * residual_energy as i64 >= 16 * sample_energy as i64 {
                    // Spectrum is flat enough; save filter parameters.
                    // `temp_signal` + `kVecLen` - `kMaxLpcOrder` points at the first of the
                    // `kMaxLpcOrder` samples in the residual signal, which will form the
                    // filter state for the next noise generation.
                    let mut filter_state = [0i16; Self::K_MAX_LPC_ORDER];
                    filter_state.copy_from_slice(
                        &temp_signal[Self::K_VEC_LEN - Self::K_MAX_LPC_ORDER..Self::K_VEC_LEN],
                    );
                    self.save_parameters(
                        channel_ix,
                        &lpc_coefficients,
                        &filter_state,
                        sample_energy,
                        residual_energy,
                    );
                    filter_params_saved = true;
                }
            } else {
                // Will only happen if `sample_energy` is not low enough. Increase the
                // threshold for update so that it increases by a factor 4 in 4 seconds.
                self.increment_energy_threshold(channel_ix, sample_energy);
            }
        }
        filter_params_saved
    }

    /// Generates `num_noise_samples` of background noise for `channel` into
    /// `buffer`. As in C++, `buffer` starts with `K_MAX_LPC_ORDER` slots of
    /// filter state followed by the noise samples.
    pub fn generate_background_noise(
        &mut self,
        random_vector: &[i16],
        channel: usize,
        _mute_slope: i32,
        _too_many_expands: bool,
        num_noise_samples: usize,
        buffer: &mut [i16],
    ) {
        const K_NOISE_LPC_ORDER: usize = BackgroundNoise::K_MAX_LPC_ORDER;
        let mut scaled_random_vector = [0i16; K_MAX_SAMPLE_RATE / 8000 * 125];
        debug_assert!(num_noise_samples <= K_MAX_SAMPLE_RATE / 8000 * 125);
        debug_assert!(random_vector.len() >= num_noise_samples);
        if self.initialized() {
            // Use background noise parameters.
            buffer[..K_NOISE_LPC_ORDER].copy_from_slice(self.filter_state(channel));

            let mut dc_offset: i32 = 0;
            if self.scale_shift(channel) > 1 {
                dc_offset = 1 << (self.scale_shift(channel) - 1);
            }

            // Scale random vector to correct energy level.
            spl::affine_transform_vector(
                &mut scaled_random_vector,
                random_vector,
                self.scale(channel),
                dc_offset,
                self.scale_shift(channel),
                num_noise_samples,
            );

            spl::filter_ar_fast_q12(
                &scaled_random_vector,
                buffer,
                K_NOISE_LPC_ORDER,
                self.filter(channel),
                K_NOISE_LPC_ORDER + 1,
                num_noise_samples,
            );

            let noise_samples =
                &mut buffer[K_NOISE_LPC_ORDER..K_NOISE_LPC_ORDER + num_noise_samples];
            self.set_filter_state(
                channel,
                &noise_samples[num_noise_samples - K_NOISE_LPC_ORDER..],
            );

            // Unmute the background noise.
            let bgn_mute_factor = self.mute_factor(channel);
            if bgn_mute_factor < 16384 {
                spl::affine_transform_vector_in_place(
                    noise_samples,
                    bgn_mute_factor,
                    8192,
                    14,
                    num_noise_samples,
                );
            }
            // Update mute_factor in BackgroundNoise class.
            self.set_mute_factor(channel, bgn_mute_factor);
        } else {
            // BGN parameters have not been initialized; use zero noise.
            buffer[K_NOISE_LPC_ORDER..K_NOISE_LPC_ORDER + num_noise_samples].fill(0);
        }
    }

    /// Background noise energy for `channel`.
    pub fn energy(&self, channel: usize) -> i32 {
        debug_assert!(channel < self.num_channels);
        self.channel_parameters[channel].energy
    }

    /// Sets the mute factor (Q14) for `channel`.
    pub fn set_mute_factor(&mut self, channel: usize, value: i16) {
        debug_assert!(channel < self.num_channels);
        self.channel_parameters[channel].mute_factor = value;
    }

    /// Mute factor (Q14) for `channel`.
    pub fn mute_factor(&self, channel: usize) -> i16 {
        debug_assert!(channel < self.num_channels);
        self.channel_parameters[channel].mute_factor
    }

    /// LPC filter coefficients for `channel` (`K_MAX_LPC_ORDER + 1` values).
    pub fn filter(&self, channel: usize) -> &[i16] {
        debug_assert!(channel < self.num_channels);
        &self.channel_parameters[channel].filter
    }

    /// Filter state for `channel` (`K_MAX_LPC_ORDER` values).
    pub fn filter_state(&self, channel: usize) -> &[i16] {
        debug_assert!(channel < self.num_channels);
        &self.channel_parameters[channel].filter_state
    }

    /// Copies `input` (at most `K_MAX_LPC_ORDER` values) into the filter
    /// state of `channel`.
    pub fn set_filter_state(&mut self, channel: usize, input: &[i16]) {
        debug_assert!(channel < self.num_channels);
        let length = input.len().min(Self::K_MAX_LPC_ORDER);
        self.channel_parameters[channel].filter_state[..length].copy_from_slice(&input[..length]);
    }

    /// Scale for `channel`.
    pub fn scale(&self, channel: usize) -> i16 {
        debug_assert!(channel < self.num_channels);
        self.channel_parameters[channel].scale
    }

    /// Scale shift for `channel`.
    pub fn scale_shift(&self, channel: usize) -> i16 {
        debug_assert!(channel < self.num_channels);
        self.channel_parameters[channel].scale_shift
    }

    /// True once the estimate has been initialized from real audio.
    pub fn initialized(&self) -> bool {
        self.initialized
    }

    /// C++ `CalculateAutoCorrelation`. The C pointer is
    /// `signal[signal_offset]`; the backwards correlation reads up to
    /// `K_MAX_LPC_ORDER` samples before it.
    fn calculate_auto_correlation(
        signal: &[i16],
        signal_offset: usize,
        length: usize,
        auto_correlation: &mut [i32],
    ) -> i32 {
        const K_CORRELATION_STEP: i32 = -1;
        let correlation_scale = cross_correlation_with_auto_shift(
            &signal[signal_offset..],
            signal,
            signal_offset,
            length,
            Self::K_MAX_LPC_ORDER + 1,
            K_CORRELATION_STEP,
            auto_correlation,
        );

        // Number of shifts to normalize energy to energy/sample. The auto shift
        // is at most 8 for `kVecLen` samples, so this is never negative.
        let energy_sample_shift = Self::K_LOG_VEC_LEN - correlation_scale;
        auto_correlation[0] >> energy_sample_shift
    }

    /// Increments the energy threshold by a factor 1 + `kThresholdIncrement`.
    fn increment_energy_threshold(&mut self, channel: usize, sample_energy: i32) {
        // TODO(hlundin): Simplify the below threshold update. What this code
        // does is simply "threshold += (increment * threshold) >> 16", but due
        // to the limited-width operations, it is not exactly the same. The
        // difference should be inaudible, but bit-exactness would not be
        // maintained.
        debug_assert!(channel < self.num_channels);
        let parameters = &mut self.channel_parameters[channel];
        let k = Self::K_THRESHOLD_INCREMENT;
        // The `int32_t` arithmetic can overflow for large thresholds (UB in
        // C++, wraps in practice); it wraps here.
        let mut temp_energy: i32 = k.wrapping_mul(parameters.low_energy_update_threshold) >> 16;
        temp_energy =
            temp_energy.wrapping_add(k.wrapping_mul(parameters.energy_update_threshold & 0xFF));
        temp_energy = temp_energy.wrapping_add(
            k.wrapping_mul((parameters.energy_update_threshold >> 8) & 0xFF)
                .wrapping_shl(8),
        );
        parameters.low_energy_update_threshold = parameters
            .low_energy_update_threshold
            .wrapping_add(temp_energy);

        parameters.energy_update_threshold = parameters
            .energy_update_threshold
            .wrapping_add(k.wrapping_mul(parameters.energy_update_threshold >> 16));
        parameters.energy_update_threshold = parameters
            .energy_update_threshold
            .wrapping_add(parameters.low_energy_update_threshold >> 16);
        parameters.low_energy_update_threshold &= 0x0FFFF;

        // Update maximum energy.
        // Decrease by a factor 1/1024 each time.
        parameters.max_energy -= parameters.max_energy >> 10;
        if sample_energy > parameters.max_energy {
            parameters.max_energy = sample_energy;
        }

        // Set `energy_update_threshold` to no less than 60 dB lower than
        // `max_energy_`. Adding 524288 assures proper rounding.
        let energy_update_threshold = parameters.max_energy.wrapping_add(524288) >> 20;
        if energy_update_threshold > parameters.energy_update_threshold {
            parameters.energy_update_threshold = energy_update_threshold;
        }
    }

    /// Updates the filter parameters.
    fn save_parameters(
        &mut self,
        channel: usize,
        lpc_coefficients: &[i16],
        filter_state: &[i16],
        sample_energy: i32,
        residual_energy: i32,
    ) {
        debug_assert!(channel < self.num_channels);
        let parameters = &mut self.channel_parameters[channel];
        parameters
            .filter
            .copy_from_slice(&lpc_coefficients[..Self::K_MAX_LPC_ORDER + 1]);
        parameters
            .filter_state
            .copy_from_slice(&filter_state[..Self::K_MAX_LPC_ORDER]);
        // Save energy level and update energy threshold levels.
        // Never get under 1.0 in average sample energy.
        parameters.energy = sample_energy.max(1);
        parameters.energy_update_threshold = parameters.energy;
        parameters.low_energy_update_threshold = 0;

        // Normalize residual_energy to 29 or 30 bits before sqrt.
        let mut norm_shift: i16 = spl::norm_w32(residual_energy) - 1;
        if norm_shift & 0x1 != 0 {
            norm_shift -= 1; // Even number of shifts required.
        }
        let residual_energy = spl::spl_shift_w32(residual_energy, norm_shift as i32);

        // Calculate scale and shift factor.
        parameters.scale = spl::sqrt_floor(residual_energy) as i16;
        // Add 13 to the `scale_shift_`, since the random numbers table is in
        // Q13.
        // TODO(hlundin): Move the "13" to where the `scale_shift_` is used?
        parameters.scale_shift = 13 + ((Self::K_LOG_RESIDUAL_LENGTH + norm_shift) / 2);

        self.initialized = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // background_noise_unittest.cc: CreateAndDestroy.
    #[test]
    fn create_and_destroy() {
        let channels = 1;
        let _bgn = BackgroundNoise::new(channels);
    }

    #[test]
    fn initial_state_matches_cpp_reset() {
        let bgn = BackgroundNoise::new(2);
        assert!(!bgn.initialized());
        for ch in 0..2 {
            assert_eq!(2500, bgn.energy(ch));
            assert_eq!(0, bgn.mute_factor(ch));
            assert_eq!(20000, bgn.scale(ch));
            assert_eq!(24, bgn.scale_shift(ch));
            assert_eq!(&[4096, 0, 0, 0, 0, 0, 0, 0, 0], bgn.filter(ch));
            assert_eq!(&[0; 8], bgn.filter_state(ch));
        }
    }

    #[test]
    fn uninitialized_generates_zero_noise_and_keeps_state_slots() {
        let mut bgn = BackgroundNoise::new(1);
        let random = [1000i16; 100];
        let mut buffer = [7i16; 8 + 100];
        bgn.generate_background_noise(&random, 0, 0, false, 100, &mut buffer);
        assert_eq!(&[7; 8], &buffer[..8]);
        assert!(buffer[8..].iter().all(|&x| x == 0));
    }

    #[test]
    fn low_energy_noise_initializes_the_estimate() {
        // A small, white-ish signal is flat enough for the parameters to be saved.
        let mut sync = AudioMultiVector::new(1);
        let mut seed: u32 = 12345;
        let samples: Vec<i16> = (0..512)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed % 61) as i16 - 30
            })
            .collect();
        sync.push_back_interleaved(&samples);
        let mut bgn = BackgroundNoise::new(1);
        assert!(bgn.update(&sync));
        assert!(bgn.initialized());
        assert!(bgn.energy(0) > 0 && bgn.energy(0) < 2500);
    }
}

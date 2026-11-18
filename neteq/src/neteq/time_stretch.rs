// Port target: WebRTC modules/audio_coding/neteq/time_stretch.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: time_stretch_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// C++ `TimeStretch` is an abstract base class with two pure virtual hooks.
// Here `TimeStretch` holds the shared state and runs the common algorithm;
// `Accelerate` / `PreemptiveExpand` embed it and supply the hooks through
// `TimeStretchVariant`.
//
// C++ keeps a `const BackgroundNoise&`. The only thing `SpeechDetection`
// reads from it is `initialized() ? Energy(0) : <fixed threshold>`, so the
// core takes that energy as `Option<i32>` (`None` = not initialized) and the
// public `process` methods of the variants derive it from `BackgroundNoise`.

//! Shared time-stretching core for Accelerate and PreemptiveExpand.

use crate::neteq::audio_multi_vector::AudioMultiVector;
use crate::neteq::background_noise::BackgroundNoise;
use crate::neteq::cross_correlation::cross_correlation_with_auto_shift;
use crate::neteq::dsp_helper::DspHelper;
use crate::spl;

/// C++ `TimeStretch::ReturnCodes`. Values match C++.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum TimeStretchReturnCode {
    Success = 0,
    SuccessLowEnergy = 1,
    NoStretch = 2,
    Error = -1,
}

/// The two C++ pure virtual methods of `TimeStretch`.
pub trait TimeStretchVariant {
    /// C++ `SetParametersForPassiveSpeech`.
    fn set_parameters_for_passive_speech(
        &self,
        input_length: usize,
        best_correlation: &mut i16,
        peak_index: &mut usize,
    );

    /// C++ `CheckCriteriaAndStretch`.
    #[allow(clippy::too_many_arguments)]
    fn check_criteria_and_stretch(
        &self,
        core: &TimeStretch,
        input: &[i16],
        input_length: usize,
        peak_index: usize,
        best_correlation: i16,
        active_speech: bool,
        fast_mode: bool,
        output: &mut AudioMultiVector,
    ) -> TimeStretchReturnCode;
}

/// The value `SpeechDetection` compares against: the background noise
/// energy of the reference channel if the estimate is initialized, `None`
/// otherwise (C++ then uses a fixed threshold).
pub(crate) fn background_noise_energy(background_noise: &BackgroundNoise) -> Option<i32> {
    if background_noise.initialized() {
        Some(background_noise.energy(TimeStretch::K_REF_CHANNEL))
    } else {
        None
    }
}

/// C++ `TimeStretch` state (sample rate, channels, downsampled input,
/// autocorrelation).
#[derive(Debug)]
pub struct TimeStretch {
    sample_rate_hz: i32,
    /// Sample rate multiplier = `sample_rate_hz / 8000`.
    fs_mult: i32,
    num_channels: usize,
    max_input_value: i16,
    downsampled_input: [i16; Self::K_DOWNSAMPLED_LEN],
    /// One extra slot because of how the peak detection uses it.
    auto_correlation: [i16; Self::K_CORRELATION_LEN + 1],
}

impl TimeStretch {
    pub(crate) const K_CORRELATION_LEN: usize = 50;
    /// >= log2(K_CORRELATION_LEN).
    #[allow(dead_code)]
    pub(crate) const K_LOG_CORRELATION_LEN: usize = 6;
    pub(crate) const K_MIN_LAG: usize = 10;
    pub(crate) const K_MAX_LAG: usize = 60;
    pub(crate) const K_DOWNSAMPLED_LEN: usize = Self::K_CORRELATION_LEN + Self::K_MAX_LAG;
    /// 0.9 in Q14.
    pub(crate) const K_CORRELATION_THRESHOLD: i32 = 14746;
    /// First channel is reference.
    pub(crate) const K_REF_CHANNEL: usize = 0;

    /// `sample_rate_hz` must be 8, 16, 32 or 48 kHz; `num_channels > 0`.
    pub fn new(sample_rate_hz: i32, num_channels: usize) -> Self {
        debug_assert!(
            sample_rate_hz == 8000
                || sample_rate_hz == 16000
                || sample_rate_hz == 32000
                || sample_rate_hz == 48000
        );
        debug_assert!(num_channels > 0);
        Self {
            sample_rate_hz,
            fs_mult: sample_rate_hz / 8000,
            num_channels,
            max_input_value: 0,
            downsampled_input: [0; Self::K_DOWNSAMPLED_LEN],
            auto_correlation: [0; Self::K_CORRELATION_LEN + 1],
        }
    }

    /// Sample rate multiplier (`sample_rate_hz / 8000`).
    pub fn fs_mult(&self) -> i32 {
        self.fs_mult
    }

    /// Number of interleaved channels.
    pub fn num_channels(&self) -> usize {
        self.num_channels
    }

    /// C++ `TimeStretch::Process`: the processing common to Accelerate and
    /// PreemptiveExpand. `background_noise_energy` is
    /// [`background_noise_energy`] of the shared `BackgroundNoise`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn process<V: TimeStretchVariant>(
        &mut self,
        variant: &V,
        input: &[i16],
        input_len: usize,
        fast_mode: bool,
        output: &mut AudioMultiVector,
        length_change_samples: &mut usize,
        background_noise_energy: Option<i32>,
    ) -> TimeStretchReturnCode {
        // Pre-calculate common multiplication with `fs_mult`.
        let fs_mult_120 = (self.fs_mult * 120) as usize; // Corresponds to 15 ms.

        let signal_array: Vec<i16>;
        let (signal, signal_len): (&[i16], usize) = if self.num_channels == 1 {
            (input, input_len)
        } else {
            // We want `signal` to be only the first channel of `input`, which
            // is interleaved.
            let signal_len = input_len / self.num_channels;
            signal_array = input[Self::K_REF_CHANNEL..]
                .iter()
                .step_by(self.num_channels)
                .take(signal_len)
                .copied()
                .collect();
            (&signal_array, signal_len)
        };

        // Find maximum absolute value of input signal.
        self.max_input_value = spl::max_abs_value_w16(signal, signal_len);

        // Downsample to 4 kHz sample rate and calculate auto-correlation.
        DspHelper::downsample_to_4khz(
            signal,
            signal_len,
            Self::K_DOWNSAMPLED_LEN,
            self.sample_rate_hz,
            true, // compensate delay
            &mut self.downsampled_input,
        );
        self.auto_correlation();

        // Find the strongest correlation peak.
        const K_NUM_PEAKS: usize = 1;
        let mut peak_index_arr = [0usize; K_NUM_PEAKS];
        let mut peak_value_arr = [0i16; K_NUM_PEAKS];
        DspHelper::peak_detection(
            &mut self.auto_correlation,
            Self::K_CORRELATION_LEN,
            K_NUM_PEAKS,
            self.fs_mult,
            &mut peak_index_arr,
            &mut peak_value_arr,
        );
        let mut peak_index = peak_index_arr[0];
        let fs_mult = self.fs_mult as usize;
        debug_assert!(peak_index <= (2 * Self::K_CORRELATION_LEN - 1) * fs_mult);

        // Compensate peak_index for displaced starting position. The
        // displacement happens in auto_correlation(). Here, `K_MIN_LAG` is in
        // the down-sampled 4 kHz domain, while `peak_index` is in the original
        // sample rate; hence, the multiplication by fs_mult * 2.
        peak_index += Self::K_MIN_LAG * fs_mult * 2;
        debug_assert!(peak_index >= 20 * fs_mult);
        debug_assert!(peak_index <= 20 * fs_mult + (2 * Self::K_CORRELATION_LEN - 1) * fs_mult);

        // Calculate scaling to ensure that `peak_index` samples can be
        // square-summed without overflowing.
        let max_input_value = self.max_input_value as i32;
        let mut scaling: i32 = 31
            - spl::norm_w32(max_input_value.wrapping_mul(max_input_value)) as i32
            - spl::norm_w32(peak_index as i32) as i32;
        scaling = scaling.max(0);

        // `vec1` starts at 15 ms minus one pitch period.
        let vec1 = &signal[fs_mult_120 - peak_index..];
        // `vec2` start at 15 ms.
        let vec2 = &signal[fs_mult_120..];

        // Calculate energies for `vec1` and `vec2`, assuming they both
        // contain `peak_index` samples.
        let vec1_energy = spl::dot_product_with_scale(vec1, vec1, peak_index, scaling);
        let vec2_energy = spl::dot_product_with_scale(vec2, vec2, peak_index, scaling);

        // Calculate cross-correlation between `vec1` and `vec2`.
        let mut cross_corr = spl::dot_product_with_scale(vec1, vec2, peak_index, scaling);

        // Check if the signal seems to be active speech or not (simple VAD).
        let active_speech = Self::speech_detection(
            vec1_energy,
            vec2_energy,
            peak_index,
            scaling,
            background_noise_energy,
        );

        let mut best_correlation: i16;
        if !active_speech {
            best_correlation = 0;
            variant.set_parameters_for_passive_speech(
                signal_len,
                &mut best_correlation,
                &mut peak_index,
            );
        } else {
            // Calculate correlation:
            // cross_corr / sqrt(vec1_energy * vec2_energy).

            // Start with calculating scale values.
            let mut energy1_scale = (16 - spl::norm_w32(vec1_energy) as i32).max(0);
            let energy2_scale = (16 - spl::norm_w32(vec2_energy) as i32).max(0);

            // Make sure total scaling is even (to simplify scale factor after
            // sqrt).
            if (energy1_scale + energy2_scale) & 1 != 0 {
                // The sum is odd.
                energy1_scale += 1;
            }

            // Scale energies to int16_t.
            let vec1_energy_int16 = (vec1_energy >> energy1_scale) as i16;
            let vec2_energy_int16 = (vec2_energy >> energy2_scale) as i16;

            // Calculate square-root of energy product.
            let sqrt_energy_prod =
                spl::sqrt_floor((vec1_energy_int16 as i32).wrapping_mul(vec2_energy_int16 as i32))
                    as i16;

            // Calculate cross_corr / sqrt(en1*en2) in Q14.
            let temp_scale = 14 - (energy1_scale + energy2_scale) / 2;
            cross_corr = spl::spl_shift_w32(cross_corr, temp_scale);
            cross_corr = cross_corr.max(0); // Don't use if negative.
            best_correlation = spl::div_w32_w16(cross_corr, sqrt_energy_prod) as i16;
            // Make sure `best_correlation` is no larger than 1 in Q14.
            best_correlation = best_correlation.min(16384);
        }

        // Check accelerate criteria and stretch the signal.
        let return_value = variant.check_criteria_and_stretch(
            self,
            input,
            input_len,
            peak_index,
            best_correlation,
            active_speech,
            fast_mode,
            output,
        );
        match return_value {
            TimeStretchReturnCode::Success | TimeStretchReturnCode::SuccessLowEnergy => {
                *length_change_samples = peak_index;
            }
            TimeStretchReturnCode::NoStretch | TimeStretchReturnCode::Error => {
                *length_change_samples = 0;
            }
        }
        return_value
    }

    /// Calculates the auto-correlation of `downsampled_input` and writes the
    /// result to `auto_correlation`.
    fn auto_correlation(&mut self) {
        // Calculate correlation from lag K_MIN_LAG to lag K_MAX_LAG in 4 kHz
        // domain.
        let mut auto_corr = [0i32; Self::K_CORRELATION_LEN];
        cross_correlation_with_auto_shift(
            &self.downsampled_input[Self::K_MAX_LAG..],
            &self.downsampled_input,
            Self::K_MAX_LAG - Self::K_MIN_LAG, // C++ `&downsampled_input_[kMaxLag - kMinLag]`
            Self::K_CORRELATION_LEN,
            Self::K_MAX_LAG - Self::K_MIN_LAG,
            -1,
            &mut auto_corr,
        );

        // Normalize correlation to 14 bits and write to `auto_correlation`.
        let max_corr = spl::max_abs_value_w32(&auto_corr, Self::K_CORRELATION_LEN);
        let scaling = (17 - spl::norm_w32(max_corr) as i32).max(0);
        spl::vector_bit_shift_w32_to_w16(
            &mut self.auto_correlation,
            Self::K_CORRELATION_LEN,
            &auto_corr,
            scaling,
        );
    }

    /// Performs a simple voice-activity detection based on the input
    /// parameters.
    fn speech_detection(
        vec1_energy: i32,
        vec2_energy: i32,
        peak_index: usize,
        scaling: i32,
        background_noise_energy: Option<i32>,
    ) -> bool {
        // Check if the signal seems to be active speech or not (simple VAD).
        // If (vec1_energy + vec2_energy) / (2 * peak_index) <=
        // 8 * background_noise_energy, then we say that the signal contains no
        // active speech.
        // Rewrite the inequality as:
        // (vec1_energy + vec2_energy) / 16 <= peak_index * background_noise_energy.
        // The two sides of the inequality will be denoted `left_side` and
        // `right_side`.
        let sum = (vec1_energy as i64 + vec2_energy as i64) / 16;
        let mut left_side = sum.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        // If noise parameters have not been estimated, use a fixed threshold.
        let mut right_side: i32 = background_noise_energy.unwrap_or(75000);
        let right_scale = (16 - spl::norm_w32(right_side) as i32).max(0);
        left_side >>= right_scale;
        right_side = (peak_index as i32).wrapping_mul(right_side >> right_scale);

        // Scale `left_side` properly before comparing with `right_side`.
        // (`scaling` is the scale factor before energy calculation, thus the
        // scale factor for the energy is 2 * scaling.)
        if (spl::norm_w32(left_side) as i32) < 2 * scaling {
            // Cannot scale only `left_side`, must scale `right_side` too.
            let temp_scale = spl::norm_w32(left_side) as i32;
            left_side = left_side.wrapping_shl(temp_scale as u32);
            right_side >>= 2 * scaling - temp_scale;
        } else {
            left_side = left_side.wrapping_shl((2 * scaling) as u32);
        }
        left_side > right_side
    }
}

/// Test-only signal helpers mirroring `spl/golden/golden_gen_wp2b.cc`, shared
/// by the time-stretch golden tests.
#[cfg(test)]
pub(crate) mod test_signal {
    use crate::neteq::audio_multi_vector::AudioMultiVector;
    use crate::spl::test_util::{Hash, Rng};

    /// C++ `make_signal`: speech-like signal in 20 ms segments of quiet
    /// noise, loud noise, a repeated random pitch period with a little
    /// noise, or a ramped pitch period.
    pub(crate) fn make_signal(r: &mut Rng, len: usize, fs: i32) -> Vec<i16> {
        let mut out = vec![0i16; len];
        let seg = (fs / 50) as usize;
        let mut base = vec![0i16; (fs / 80 + 1) as usize];
        let mut start = 0;
        while start < len {
            let mode = r.below(4);
            let period = r.range(fs / 400, fs / 80) as usize;
            for b in base.iter_mut().take(period) {
                *b = (r.below(16001) as i32 - 8000) as i16;
            }
            let end = len.min(start + seg);
            for (i, sample) in out.iter_mut().enumerate().take(end).skip(start) {
                let j = i - start;
                let v: i32 = match mode {
                    0 => r.below(201) as i32 - 100,
                    1 => (r.next() as i16 as i32) / 2,
                    2 => base[j % period] as i32 + r.below(401) as i32 - 200,
                    _ => base[j % period] as i32 * (j as i32 + 1) / seg as i32,
                };
                *sample = v as i16;
            }
            start += seg;
        }
        out
    }

    /// C++ `interleave`: channel c > 0 is a noisy, halved copy of `sig`.
    pub(crate) fn interleave(r: &mut Rng, sig: &[i16], channels: usize) -> Vec<i16> {
        let mut input = vec![0i16; sig.len() * channels];
        for (i, &s) in sig.iter().enumerate() {
            for c in 0..channels {
                input[i * channels + c] = if c == 0 {
                    s
                } else {
                    (s as i32 / 2 + r.below(101) as i32 - 50) as i16
                };
            }
        }
        input
    }

    /// C++ `block_length`.
    pub(crate) fn block_length(r: &mut Rng, fs_mult: i32) -> usize {
        if r.below(10) == 0 {
            return r.range(200 * fs_mult, 239 * fs_mult - 1) as usize;
        }
        r.range(240 * fs_mult, 260 * fs_mult) as usize
    }

    /// C++ `feed_output`.
    pub(crate) fn feed_output(h: &mut Hash, ret: i32, change: usize, out: &AudioMultiVector) {
        h.feed(ret as i64);
        h.feed(change as i64);
        h.feed(out.size() as i64);
        feed_interleaved(h, out);
    }

    /// Feeds every sample of `out`, interleaved.
    pub(crate) fn feed_interleaved(h: &mut Hash, out: &AudioMultiVector) {
        let mut samples = vec![0i16; out.size() * out.channels()];
        out.read_interleaved(out.size(), &mut samples);
        h.feed16(&samples);
    }
}

#[cfg(test)]
mod tests {
    use super::test_signal::make_signal;
    use super::*;
    use crate::neteq::accelerate::Accelerate;
    use crate::neteq::preemptive_expand::PreemptiveExpand;
    use crate::spl::test_util::Rng;

    const K_NUM_CHANNELS: usize = 1;

    // time_stretch_unittest.cc: CreateAndDestroy (the factory test is moot,
    // factories are dropped).
    #[test]
    fn create_and_destroy() {
        let k_sample_rate = 8000;
        let k_overlap_samples = 5 * k_sample_rate as usize / 8000;
        let _accelerate = Accelerate::new(k_sample_rate, K_NUM_CHANNELS);
        let _preemptive_expand =
            PreemptiveExpand::new(k_sample_rate, K_NUM_CHANNELS, k_overlap_samples);
    }

    /// time_stretch_unittest.cc `TimeStretchTest::TestAccelerate`, on the
    /// synthetic golden signal instead of testfile32kHz.pcm (which the
    /// sparse WebRTC checkout lacks). Returns (total length change,
    /// [success, low energy, no stretch]). The background noise is never
    /// updated, as in the C++ test.
    fn test_accelerate(loops: usize, fast_mode: bool) -> (usize, [i32; 3]) {
        let sample_rate_hz = 32000;
        let block_size = (30 * sample_rate_hz / 1000) as usize;
        let mut r = Rng::new();
        let signal = make_signal(&mut r, 100 * block_size, sample_rate_hz);
        let mut accelerate = Accelerate::new(sample_rate_hz, K_NUM_CHANNELS);
        let mut total_length_change = 0;
        let mut return_stats = [0; 3];
        for block in signal.chunks(block_size).take(loops) {
            let mut output = AudioMultiVector::new(K_NUM_CHANNELS);
            let mut length_change = 0;
            let ret = accelerate.process_with_noise_energy(
                block,
                block_size,
                fast_mode,
                &mut output,
                &mut length_change,
                None,
            );
            assert_ne!(
                TimeStretchReturnCode::Error,
                ret,
                "Process returned an error"
            );
            return_stats[ret as usize] += 1;
            total_length_change += length_change;
        }
        (total_length_change, return_stats)
    }

    // Expected values printed by spl/golden/golden_gen_wp2b.cc
    // (`time_stretch_normal` / `time_stretch_fast`).
    #[test]
    fn accelerate() {
        assert_eq!((2350, [2, 10, 88]), test_accelerate(100, false));
    }

    #[test]
    fn accelerate_fast_mode() {
        assert_eq!((3334, [5, 10, 85]), test_accelerate(100, true));
    }

    #[test]
    fn speech_detection_uses_fixed_threshold_without_noise_estimate() {
        // left = (v1 + v2) / 16 = 75000 * peak_index exactly is not active.
        let peak_index = 100;
        let energy = 75000 * 16 * peak_index / 2;
        assert!(!TimeStretch::speech_detection(
            energy as i32,
            energy as i32,
            peak_index,
            0,
            None
        ));
        assert!(TimeStretch::speech_detection(
            energy as i32 + 64,
            energy as i32 + 64,
            peak_index,
            0,
            None
        ));
        // A louder noise estimate makes the same block passive.
        assert!(!TimeStretch::speech_detection(
            energy as i32 + 64,
            energy as i32 + 64,
            peak_index,
            0,
            Some(80000)
        ));
    }
}

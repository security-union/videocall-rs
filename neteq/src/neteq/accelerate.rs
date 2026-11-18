// Port target: WebRTC modules/audio_coding/neteq/accelerate.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// C++ keeps `const BackgroundNoise&`; here it is passed per call. The
// factory (`AccelerateFactory`) is dropped.

//! Accelerate: removes one pitch period to shrink the buffer.

use crate::neteq::audio_multi_vector::AudioMultiVector;
use crate::neteq::background_noise::BackgroundNoise;
use crate::neteq::time_stretch::{
    background_noise_energy, TimeStretch, TimeStretchReturnCode, TimeStretchVariant,
};

/// C++ `Accelerate`.
#[derive(Debug)]
pub struct Accelerate {
    core: TimeStretch,
}

/// The Accelerate overrides of the `TimeStretch` hooks (stateless).
#[derive(Debug, Clone, Copy)]
struct AccelerateHooks;

impl Accelerate {
    /// Accelerate at `sample_rate_hz` for `num_channels` channels.
    pub fn new(sample_rate_hz: i32, num_channels: usize) -> Self {
        Self {
            core: TimeStretch::new(sample_rate_hz, num_channels),
        }
    }

    /// Tries to remove one pitch period from `input[..input_length]`
    /// (interleaved) into `output`. Sets `length_change_samples` to the
    /// number of samples removed. `fast_accelerate` allows removing more.
    ///
    /// As in C++, `length_change_samples` is left untouched when the input
    /// is too short (`TimeStretchReturnCode::Error`).
    pub fn process(
        &mut self,
        input: &[i16],
        input_length: usize,
        fast_accelerate: bool,
        output: &mut AudioMultiVector,
        length_change_samples: &mut usize,
        background_noise: &BackgroundNoise,
    ) -> TimeStretchReturnCode {
        self.process_with_noise_energy(
            input,
            input_length,
            fast_accelerate,
            output,
            length_change_samples,
            background_noise_energy(background_noise),
        )
    }

    /// `process` with the background noise reduced to what `TimeStretch`
    /// reads from it (`None` = not initialized).
    pub(crate) fn process_with_noise_energy(
        &mut self,
        input: &[i16],
        input_length: usize,
        fast_accelerate: bool,
        output: &mut AudioMultiVector,
        length_change_samples: &mut usize,
        background_noise_energy: Option<i32>,
    ) -> TimeStretchReturnCode {
        // Input length must be (almost) 30 ms.
        const K_15MS: usize = 120; // 15 ms = 120 samples at 8 kHz sample rate.
        let num_channels = self.core.num_channels();
        let fs_mult = self.core.fs_mult() as usize;
        if num_channels == 0 || input_length / num_channels < (2 * K_15MS - 1) * fs_mult {
            // Length of input data too short to do accelerate. Simply move
            // all data from input to output.
            output.push_back_interleaved(&input[..input_length]);
            return TimeStretchReturnCode::Error;
        }
        self.core.process(
            &AccelerateHooks,
            input,
            input_length,
            fast_accelerate,
            output,
            length_change_samples,
            background_noise_energy,
        )
    }
}

impl TimeStretchVariant for AccelerateHooks {
    fn set_parameters_for_passive_speech(
        &self,
        _input_length: usize,
        best_correlation: &mut i16,
        _peak_index: &mut usize,
    ) {
        // When the signal does not contain any active speech, the
        // correlation does not matter. Simply set it to zero.
        *best_correlation = 0;
    }

    fn check_criteria_and_stretch(
        &self,
        core: &TimeStretch,
        input: &[i16],
        input_length: usize,
        mut peak_index: usize,
        best_correlation: i16,
        active_speech: bool,
        fast_mode: bool,
        output: &mut AudioMultiVector,
    ) -> TimeStretchReturnCode {
        let num_channels = core.num_channels();
        // Check for strong correlation or passive speech.
        // Use 8192 (0.5 in Q14) in fast mode.
        let correlation_threshold = if fast_mode {
            8192
        } else {
            TimeStretch::K_CORRELATION_THRESHOLD
        };
        if (best_correlation as i32 > correlation_threshold) || !active_speech {
            // Do accelerate operation by overlap add.

            // Pre-calculate common multiplication with `fs_mult`.
            // 120 corresponds to 15 ms.
            let fs_mult_120 = core.fs_mult() as usize * 120;

            if fast_mode {
                // Fit as many multiples of `peak_index` as possible in
                // fs_mult_120.
                peak_index = (fs_mult_120 / peak_index) * peak_index;
            }

            debug_assert!(fs_mult_120 >= peak_index); // Should be handled in process().
                                                      // Copy first part; 0 to 15 ms.
            output.push_back_interleaved(&input[..fs_mult_120 * num_channels]);
            // Copy the `peak_index` starting at 15 ms to `temp_vector`.
            let mut temp_vector = AudioMultiVector::new(num_channels);
            let start = fs_mult_120 * num_channels;
            temp_vector.push_back_interleaved(&input[start..start + peak_index * num_channels]);
            // Cross-fade `temp_vector` onto the end of `output`.
            output.cross_fade(&temp_vector, peak_index);
            // Copy the last unmodified part, 15 ms + pitch period until the
            // end.
            output.push_back_interleaved(
                &input[(fs_mult_120 + peak_index) * num_channels..input_length],
            );

            if active_speech {
                TimeStretchReturnCode::Success
            } else {
                TimeStretchReturnCode::SuccessLowEnergy
            }
        } else {
            // Accelerate not allowed. Simply move all data from decoded to
            // outData.
            output.push_back_interleaved(&input[..input_length]);
            TimeStretchReturnCode::NoStretch
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::neteq::time_stretch::test_signal::{
        block_length, feed_output, interleave, make_signal,
    };
    use crate::spl::test_util::{Hash, Rng};

    // Golden hash against the real C++ (spl/golden/golden_gen_wp2b.cc,
    // `accelerate_golden`): every rate, mono and stereo, fast mode off and
    // on, 60 blocks each, including too-short blocks. The background noise
    // estimate is not initialized (fixed-threshold VAD).
    #[test]
    fn golden_accelerate() {
        let mut r = Rng::new();
        let mut h = Hash::new();
        for fs in [8000, 16000, 32000, 48000] {
            for channels in 1..=2 {
                for fast in [false, true] {
                    let mut accelerate = Accelerate::new(fs, channels);
                    let fs_mult = fs / 8000;
                    for _ in 0..60 {
                        let len = block_length(&mut r, fs_mult);
                        let sig = make_signal(&mut r, len, fs);
                        let input = interleave(&mut r, &sig, channels);
                        let mut out = AudioMultiVector::new(channels);
                        let mut change = 12345;
                        let ret = accelerate.process_with_noise_energy(
                            &input,
                            input.len(),
                            fast,
                            &mut out,
                            &mut change,
                            None,
                        );
                        feed_output(&mut h, ret as i32, change, &out);
                    }
                }
            }
        }
        assert_eq!(0xe6bb6f675cb6e73f, h.value());
    }

    #[test]
    fn too_short_input_is_copied_and_reports_error() {
        let mut accelerate = Accelerate::new(8000, 1);
        let input: Vec<i16> = (0..238).collect();
        let mut out = AudioMultiVector::new(1);
        let mut change = 7;
        let ret = accelerate.process_with_noise_energy(
            &input,
            input.len(),
            false,
            &mut out,
            &mut change,
            None,
        );
        assert_eq!(TimeStretchReturnCode::Error, ret);
        assert_eq!(7, change);
        assert_eq!(238, out.size());
    }
}

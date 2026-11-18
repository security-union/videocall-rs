// Port target: WebRTC modules/audio_coding/neteq/preemptive_expand.{h,cc} at
// rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// C++ keeps `const BackgroundNoise&`; here it is passed per call. The
// factory (`PreemptiveExpandFactory`) is dropped.

//! Preemptive expand: inserts one pitch period to grow the buffer.

use crate::neteq::audio_multi_vector::AudioMultiVector;
use crate::neteq::background_noise::BackgroundNoise;
use crate::neteq::time_stretch::{
    background_noise_energy, TimeStretch, TimeStretchReturnCode, TimeStretchVariant,
};

/// C++ `PreemptiveExpand`.
#[derive(Debug)]
pub struct PreemptiveExpand {
    core: TimeStretch,
    hooks: PreemptiveExpandHooks,
}

/// The PreemptiveExpand overrides of the `TimeStretch` hooks and the state
/// they read.
#[derive(Debug, Clone, Copy)]
struct PreemptiveExpandHooks {
    old_data_length_per_channel: usize,
    overlap_samples: usize,
}

impl PreemptiveExpand {
    /// PreemptiveExpand at `sample_rate_hz` for `num_channels` channels with
    /// `overlap_samples` of overlap.
    pub fn new(sample_rate_hz: i32, num_channels: usize, overlap_samples: usize) -> Self {
        Self {
            core: TimeStretch::new(sample_rate_hz, num_channels),
            hooks: PreemptiveExpandHooks {
                old_data_length_per_channel: 0,
                overlap_samples,
            },
        }
    }

    /// Tries to insert one pitch period into `pw16_decoded[..len]`
    /// (interleaved), of which the first `old_data_len` samples per channel
    /// are already-played history. Sets `length_change_samples`.
    ///
    /// As in C++, `length_change_samples` is left untouched when the input
    /// is too short (`TimeStretchReturnCode::Error`).
    pub fn process(
        &mut self,
        pw16_decoded: &[i16],
        len: usize,
        old_data_len: usize,
        output: &mut AudioMultiVector,
        length_change_samples: &mut usize,
        background_noise: &BackgroundNoise,
    ) -> TimeStretchReturnCode {
        self.process_with_noise_energy(
            pw16_decoded,
            len,
            old_data_len,
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
        old_data_length: usize,
        output: &mut AudioMultiVector,
        length_change_samples: &mut usize,
        background_noise_energy: Option<i32>,
    ) -> TimeStretchReturnCode {
        self.hooks.old_data_length_per_channel = old_data_length;
        // Input length must be (almost) 30 ms.
        // Also, the new part must be at least `overlap_samples` elements.
        const K_15MS: usize = 120; // 15 ms = 120 samples at 8 kHz sample rate.
        let num_channels = self.core.num_channels();
        let fs_mult = self.core.fs_mult() as usize;
        if num_channels == 0
            || input_length / num_channels < (2 * K_15MS - 1) * fs_mult
            || old_data_length >= input_length / num_channels - self.hooks.overlap_samples
        {
            // Length of input data too short to do preemptive expand. Simply
            // move all data from input to output.
            output.push_back_interleaved(&input[..input_length]);
            return TimeStretchReturnCode::Error;
        }
        const K_FAST_MODE: bool = false; // Fast mode is not available for PE Expand.
        self.core.process(
            &self.hooks,
            input,
            input_length,
            K_FAST_MODE,
            output,
            length_change_samples,
            background_noise_energy,
        )
    }
}

impl TimeStretchVariant for PreemptiveExpandHooks {
    fn set_parameters_for_passive_speech(
        &self,
        len: usize,
        best_correlation: &mut i16,
        peak_index: &mut usize,
    ) {
        // When the signal does not contain any active speech, the
        // correlation does not matter. Simply set it to zero.
        *best_correlation = 0;

        // For low energy expansion, the new data can be less than 15 ms,
        // but we must ensure that best_correlation is not larger than the
        // length of the new data.
        *peak_index = (*peak_index).min(len - self.old_data_length_per_channel);
    }

    fn check_criteria_and_stretch(
        &self,
        core: &TimeStretch,
        input: &[i16],
        input_length: usize,
        peak_index: usize,
        best_correlation: i16,
        active_speech: bool,
        _fast_mode: bool,
        output: &mut AudioMultiVector,
    ) -> TimeStretchReturnCode {
        let num_channels = core.num_channels();
        // Pre-calculate common multiplication with `fs_mult`.
        // 120 corresponds to 15 ms.
        let fs_mult_120 = core.fs_mult() as usize * 120;
        // Check for strong correlation (>0.9 in Q14) and at least 15 ms new
        // data, or passive speech.
        if ((best_correlation as i32 > TimeStretch::K_CORRELATION_THRESHOLD)
            && (self.old_data_length_per_channel <= fs_mult_120))
            || !active_speech
        {
            // Do accelerate operation by overlap add.

            // Set length of the first part, not to be modified.
            let unmodified_length = self.old_data_length_per_channel.max(fs_mult_120);
            // Copy first part, including cross-fade region.
            output.push_back_interleaved(&input[..(unmodified_length + peak_index) * num_channels]);
            // Copy the last `peak_index` samples up to 15 ms to `temp_vector`.
            let mut temp_vector = AudioMultiVector::new(num_channels);
            let start = (unmodified_length - peak_index) * num_channels;
            temp_vector.push_back_interleaved(&input[start..start + peak_index * num_channels]);
            // Cross-fade `temp_vector` onto the end of `output`.
            output.cross_fade(&temp_vector, peak_index);
            // Copy the last unmodified part, 15 ms + pitch period until the
            // end.
            output.push_back_interleaved(&input[unmodified_length * num_channels..input_length]);

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
    // `preemptive_expand_golden`): every rate, mono and stereo, 80 blocks
    // each with random `old_data_len` (including the error paths). C++
    // PreemptiveExpand has no fast mode. The background noise estimate is
    // not initialized (fixed-threshold VAD).
    #[test]
    fn golden_preemptive_expand() {
        let mut r = Rng::new();
        let mut h = Hash::new();
        for fs in [8000, 16000, 32000, 48000] {
            for channels in 1..=2 {
                let overlap = (5 * fs / 8000) as usize;
                let mut pe = PreemptiveExpand::new(fs, channels, overlap);
                let fs_mult = fs / 8000;
                for _ in 0..80 {
                    let len = block_length(&mut r, fs_mult);
                    let old_len = r.range(0, 260 * fs_mult) as usize;
                    let sig = make_signal(&mut r, len, fs);
                    let input = interleave(&mut r, &sig, channels);
                    let mut out = AudioMultiVector::new(channels);
                    let mut change = 12345;
                    let ret = pe.process_with_noise_energy(
                        &input,
                        input.len(),
                        old_len,
                        &mut out,
                        &mut change,
                        None,
                    );
                    feed_output(&mut h, ret as i32, change, &out);
                }
            }
        }
        assert_eq!(0x76da5bf1f2bf1509, h.value());
    }
}

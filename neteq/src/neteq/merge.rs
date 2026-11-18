// Port target: WebRTC modules/audio_coding/neteq/merge.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: merge_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// C++ `Merge` keeps `Expand*` and `SyncBuffer*`; here both are passed per
// call (the sync buffer inside `ExpandContext`).

//! Merges newly decoded audio into the tail of an expansion.

use crate::neteq::audio_multi_vector::AudioMultiVector;
use crate::neteq::cross_correlation::cross_correlation_with_auto_shift;
use crate::neteq::dsp_helper::DspHelper;
use crate::neteq::expand::{Expand, ExpandContext};
use crate::spl;

const K_MAX_SAMPLE_RATE: usize = 48000;
const K_EXPAND_DOWNSAMP_LENGTH: usize = 100;
const K_INPUT_DOWNSAMP_LENGTH: usize = 40;
const K_MAX_CORRELATION_LENGTH: usize = 60;
/// Most samples taken over from the sync buffer (`210 * kMaxSampleRate / 8000`).
const K_MAX_OLD_LENGTH: usize = 210 * K_MAX_SAMPLE_RATE / 8000;

/// C++ `Merge`.
#[derive(Debug)]
pub struct Merge {
    fs_hz: i32,
    num_channels: usize,
    /// fs_hz / 8000.
    fs_mult: i32,
    timestamps_per_call: usize,
    expanded_downsampled: [i16; K_EXPAND_DOWNSAMP_LENGTH],
    input_downsampled: [i16; K_INPUT_DOWNSAMP_LENGTH],
    expanded: AudioMultiVector,
    temp_data: Vec<i16>,
}

impl Merge {
    /// Merge for `num_channels` channels at `fs_hz`.
    pub fn new(fs_hz: i32, num_channels: usize) -> Self {
        debug_assert!(num_channels > 0);
        Self {
            fs_hz,
            num_channels,
            fs_mult: fs_hz / 8000,
            timestamps_per_call: (fs_hz / 100) as usize,
            expanded_downsampled: [0; K_EXPAND_DOWNSAMP_LENGTH],
            input_downsampled: [0; K_INPUT_DOWNSAMP_LENGTH],
            expanded: AudioMultiVector::new(num_channels),
            temp_data: Vec::new(),
        }
    }

    /// Merges the decoded `input[..input_length]` (total samples over all
    /// channels, interleaved) with the expansion and writes the result to
    /// `output`, whose channel count defines the de-interleaving. Returns
    /// what C++ `Merge::Process` returns (the output length).
    pub fn process(
        &mut self,
        input: &mut [i16],
        input_length: usize,
        output: &mut AudioMultiVector,
        expand: &mut Expand,
        ctx: &mut ExpandContext<'_>,
    ) -> usize {
        // TODO(hlundin): Change to an enumerator and skip assert.
        debug_assert!(
            self.fs_hz == 8000 || self.fs_hz == 16000 || self.fs_hz == 32000 || self.fs_hz == 48000
        );
        debug_assert!(self.fs_hz <= K_MAX_SAMPLE_RATE as i32); // Should not be possible.
        if input_length == 0 {
            return 0;
        }

        // Get expansion data to overlap and mix with.
        let (expanded_length, old_length, expand_period) = self.get_expanded_signal(expand, ctx);

        // Transfer input signal to an AudioMultiVector.
        let mut input_vector = AudioMultiVector::new(self.num_channels);
        input_vector.push_back_interleaved(&input[..input_length]);
        let input_length_per_channel = input_vector.size();
        debug_assert_eq!(input_length_per_channel, input_length / self.num_channels);

        let mut best_correlation_index: usize = 0;
        let mut output_length: usize = 0;

        let mut input_channel = vec![0i16; input_length_per_channel];
        let mut expanded_channel = vec![0i16; expanded_length];
        for channel in 0..self.num_channels {
            input_vector[channel].copy_to_slice(input_length_per_channel, 0, &mut input_channel);
            self.expanded[channel].copy_to_slice(expanded_length, 0, &mut expanded_channel);

            let new_mute_factor: i16 = 16384.min(self.signal_scaling(
                &input_channel,
                input_length_per_channel,
                &expanded_channel,
            ));

            if channel == 0 {
                // Downsample, correlate, and find strongest correlation period for the
                // reference (i.e., first) channel only.
                // Downsample to 4kHz sample rate.
                self.downsample(
                    &input_channel,
                    input_length_per_channel,
                    &expanded_channel,
                    expanded_length,
                );

                // Calculate the lag of the strongest correlation period.
                best_correlation_index = self.correlate_and_peak_search(
                    expand,
                    old_length,
                    input_length_per_channel,
                    expand_period,
                );
            }

            // `std::vector::resize` keeps the existing prefix.
            self.temp_data
                .resize(input_length_per_channel + best_correlation_index, 0);
            let (expanded_part, decoded_output) =
                self.temp_data.split_at_mut(best_correlation_index);

            // Mute the new decoded data if needed (and unmute it linearly).
            // This is the overlapping part of expanded_signal.
            let mut interpolation_length = (K_MAX_CORRELATION_LENGTH * self.fs_mult as usize)
                .min(expanded_length.wrapping_sub(best_correlation_index));
            interpolation_length = interpolation_length.min(input_length_per_channel);

            debug_assert!(new_mute_factor <= 16384);
            let mut mute_factor: i16 = expand.mute_factor(channel).max(new_mute_factor);
            debug_assert!(mute_factor >= 0);

            if mute_factor < 16384 {
                // Set a suitable muting slope (Q20). 0.004 for NB, 0.002 for WB,
                // and so on, or as fast as it takes to come back to full gain within the
                // frame length.
                // `((16384 - mute_factor) << 6) / input_length_per_channel` is
                // `size_t` division.
                let back_to_fullscale_inc = ((((16384 - mute_factor as i32) << 6) as usize)
                    / input_length_per_channel) as i32;
                let increment = (4194 / self.fs_mult).max(back_to_fullscale_inc);
                mute_factor = DspHelper::ramp_signal_in_place(
                    &mut input_channel,
                    interpolation_length,
                    mute_factor as i32,
                    increment,
                ) as i16;
                DspHelper::unmute_signal(
                    &input_channel[interpolation_length..],
                    input_length_per_channel - interpolation_length,
                    &mut mute_factor,
                    increment,
                    &mut decoded_output[interpolation_length..],
                );
            } else {
                // No muting needed.
                decoded_output[interpolation_length..input_length_per_channel]
                    .copy_from_slice(&input_channel[interpolation_length..]);
            }

            // Do overlap and mix linearly.
            // `16384 / (interpolation_length + 1)` is `size_t` division.
            let increment: i16 = (16384 / (interpolation_length + 1)) as i16; // In Q14.
            let mut local_mute_factor: i16 = (16384 - increment as i32) as i16;
            expanded_part.copy_from_slice(&expanded_channel[..best_correlation_index]);
            DspHelper::cross_fade(
                &expanded_channel[best_correlation_index..],
                &input_channel,
                interpolation_length,
                &mut local_mute_factor,
                increment,
                decoded_output,
            );

            output_length = best_correlation_index + input_length_per_channel;
            if channel == 0 {
                debug_assert!(output.empty()); // Output should be empty at this point.
                output.assert_size(output_length);
            } else {
                debug_assert_eq!(output.size(), output_length);
            }
            output[channel].overwrite_at_slice(&self.temp_data[..output_length], 0);
        }

        // Copy back the first part of the data to `sync_buffer_` and remove it from
        // `output`.
        let next_index = ctx.sync_buffer.next_index();
        ctx.sync_buffer
            .replace_at_index(output, old_length, next_index);
        output.pop_front(old_length);

        // Return new added length. `old_length` samples were borrowed from
        // `sync_buffer_`.
        debug_assert!(output_length >= old_length);
        output_length.wrapping_sub(old_length)
    }

    /// Samples needed in the sync buffer for a merge.
    pub fn required_future_samples(&self) -> usize {
        (self.fs_hz / 100) as usize * self.num_channels // 10 ms.
    }

    /// C++ `GetExpandedSignal`: calls `expand` to get more expansion data to
    /// merge with, written to `self.expanded`. Returns `(expanded_length,
    /// old_length, expand_period)`: the length of the expanded data, the
    /// number of samples taken from the sync buffer, and the samples in one
    /// expansion period (typically one pitch period).
    fn get_expanded_signal(
        &mut self,
        expand: &mut Expand,
        ctx: &mut ExpandContext<'_>,
    ) -> (usize, usize, usize) {
        // Check how much data that is left since earlier.
        let mut old_length = ctx.sync_buffer.future_length();
        // Should never be less than overlap_length.
        debug_assert!(old_length >= expand.overlap_length());
        // Generate data to merge the overlap with using expand.
        expand.set_parameters_for_merge_after_expand(ctx.statistics);

        if old_length >= K_MAX_OLD_LENGTH {
            // TODO(hlundin): Write test case for this.
            // The number of samples available in the sync buffer is more than what fits
            // in expanded_signal. Keep the first 210 * kMaxSampleRate / 8000 samples,
            // but shift them towards the end of the buffer. This is ok, since all of
            // the buffer will be expand data anyway, so as long as the beginning is
            // left untouched, we're fine.
            let length_diff = old_length - K_MAX_OLD_LENGTH;
            let next_index = ctx.sync_buffer.next_index();
            ctx.sync_buffer
                .insert_zeros_at_index(length_diff, next_index);
            old_length = K_MAX_OLD_LENGTH;
            // This is the truncated length.
        }
        // This assert should always be true thanks to the if statement above.
        debug_assert!(old_length <= K_MAX_OLD_LENGTH);

        let mut expanded_temp = AudioMultiVector::new(self.num_channels);
        expand.process(ctx, &mut expanded_temp);
        let expand_period = expanded_temp.size(); // Samples per channel.

        self.expanded.clear();
        // Copy what is left since earlier into the expanded vector.
        let next_index = ctx.sync_buffer.next_index();
        self.expanded
            .push_back_from_index(ctx.sync_buffer, next_index);
        debug_assert_eq!(self.expanded.size(), old_length);
        debug_assert!(expanded_temp.size() > 0);
        // Do "ugly" copy and paste from the expanded in order to generate more data
        // to correlate (but not interpolate) with.
        let required_length = ((120 + 80 + 2) * self.fs_mult) as usize;
        if self.expanded.size() < required_length {
            while self.expanded.size() < required_length {
                // Append one more pitch period each time.
                self.expanded.push_back(&expanded_temp);
            }
            // Trim the length to exactly `required_length`.
            let excess = self.expanded.size() - required_length;
            self.expanded.pop_back(excess);
        }
        debug_assert!(self.expanded.size() >= required_length);
        (required_length, old_length, expand_period)
    }

    /// Analyzes `input` and `expanded_signal` and returns the muting factor
    /// (Q14) to be used on the new data.
    fn signal_scaling(&self, input: &[i16], input_length: usize, expanded_signal: &[i16]) -> i16 {
        // Adjust muting factor if new vector is more or less of the BGN energy.
        let mod_input_length = (64 * self.fs_mult as usize).min(input_length);
        let expanded_max = spl::max_abs_value_w16(expanded_signal, mod_input_length);
        let mut factor: i32 =
            (expanded_max as i32 * expanded_max as i32) / (i32::MAX / mod_input_length as i32);
        let expanded_shift: i32 = if factor == 0 {
            0
        } else {
            31 - spl::norm_w32(factor) as i32
        };
        let mut energy_expanded = spl::dot_product_with_scale(
            expanded_signal,
            expanded_signal,
            mod_input_length,
            expanded_shift,
        );

        // Calculate energy of input signal.
        let input_max = spl::max_abs_value_w16(input, mod_input_length);
        factor = (input_max as i32 * input_max as i32) / (i32::MAX / mod_input_length as i32);
        let input_shift: i32 = if factor == 0 {
            0
        } else {
            31 - spl::norm_w32(factor) as i32
        };
        let mut energy_input =
            spl::dot_product_with_scale(input, input, mod_input_length, input_shift);

        // Align to the same Q-domain.
        if input_shift > expanded_shift {
            energy_expanded >>= input_shift - expanded_shift;
        } else {
            energy_input >>= expanded_shift - input_shift;
        }

        // Calculate muting factor to use for new frame.
        if energy_input > energy_expanded {
            // Normalize `energy_input` to 14 bits.
            let temp_shift: i16 = spl::norm_w32(energy_input) - 17;
            energy_input = spl::spl_shift_w32(energy_input, temp_shift as i32);
            // Put `energy_expanded` in a domain 14 higher, so that
            // energy_expanded / energy_input is in Q14.
            energy_expanded = spl::spl_shift_w32(energy_expanded, temp_shift as i32 + 14);
            // Calculate sqrt(energy_expanded / energy_input) in Q14.
            spl::sqrt_floor((energy_expanded / energy_input).wrapping_shl(14)) as i16
        } else {
            // Set to 1 (in Q14) when `expanded` has higher energy than `input`.
            16384
        }
    }

    /// Downsamples `input` (`input_length` samples) and `expanded_signal` to
    /// 4 kHz sample rate, into `input_downsampled` and
    /// `expanded_downsampled`.
    // TODO(hlundin): There are some parameter values in this method that seem
    // strange. Compare with Expand::Correlation.
    fn downsample(
        &mut self,
        input: &[i16],
        input_length: usize,
        expanded_signal: &[i16],
        expanded_length: usize,
    ) {
        let decimation_factor = self.fs_hz / 4000;
        const K_COMPENSATE_DELAY: usize = 0;
        let length_limit = (self.fs_hz / 100) as usize; // 10 ms in samples.
        let (filter_coefficients, num_coefficients): (&[i16], usize) = match self.fs_hz {
            8000 => (&DspHelper::K_DOWNSAMPLE_8KHZ_TBL, 3),
            16000 => (&DspHelper::K_DOWNSAMPLE_16KHZ_TBL, 5),
            32000 => (&DspHelper::K_DOWNSAMPLE_32KHZ_TBL, 7),
            // fs_hz_ == 48000
            _ => (&DspHelper::K_DOWNSAMPLE_48KHZ_TBL, 7),
        };
        let signal_offset = num_coefficients - 1;
        spl::downsample_fast(
            expanded_signal,
            signal_offset,
            expanded_length - signal_offset,
            &mut self.expanded_downsampled,
            K_EXPAND_DOWNSAMP_LENGTH,
            filter_coefficients,
            num_coefficients,
            decimation_factor,
            K_COMPENSATE_DELAY,
        );
        if input_length <= length_limit {
            // Not quite long enough, so we have to cheat a bit.
            // If the input is shorter than the offset, we consider the input to be 0
            // length. This will cause us to skip the downsampling since it makes no
            // sense anyway, and input_downsampled_ will be filled with zeros. This is
            // clearly a pathological case, and the signal quality will suffer, but
            // there is not much we can do.
            let temp_len = input_length.saturating_sub(signal_offset);
            // TODO(hlundin): Should `downsamp_temp_len` be corrected for round-off
            // errors? I.e., (temp_len + decimation_factor - 1) / decimation_factor?
            let downsamp_temp_len = temp_len / decimation_factor as usize;
            if downsamp_temp_len > 0 {
                spl::downsample_fast(
                    input,
                    signal_offset,
                    temp_len,
                    &mut self.input_downsampled,
                    downsamp_temp_len,
                    filter_coefficients,
                    num_coefficients,
                    decimation_factor,
                    K_COMPENSATE_DELAY,
                );
            }
            self.input_downsampled[downsamp_temp_len..].fill(0);
        } else {
            spl::downsample_fast(
                input,
                signal_offset,
                input_length - signal_offset,
                &mut self.input_downsampled,
                K_INPUT_DOWNSAMP_LENGTH,
                filter_coefficients,
                num_coefficients,
                decimation_factor,
                K_COMPENSATE_DELAY,
            );
        }
    }

    /// Calculates cross-correlation between `input_downsampled` and
    /// `expanded_downsampled`, and finds the correlation maximum. The
    /// maximizing lag is returned.
    fn correlate_and_peak_search(
        &self,
        expand: &Expand,
        start_position: usize,
        input_length: usize,
        expand_period: usize,
    ) -> usize {
        // Calculate correlation without any normalization.
        let max_corr_length = K_MAX_CORRELATION_LENGTH;
        let stop_position_downsamp =
            max_corr_length.min(expand.max_lag() / (self.fs_mult as usize * 2) + 1);

        let mut correlation = [0i32; K_MAX_CORRELATION_LENGTH];
        cross_correlation_with_auto_shift(
            &self.input_downsampled,
            &self.expanded_downsampled,
            0,
            K_INPUT_DOWNSAMP_LENGTH,
            stop_position_downsamp,
            1,
            &mut correlation,
        );

        // Normalize correlation to 14 bits and copy to a 16-bit array.
        let pad_length = expand.overlap_length() - 1;
        // Adding 1 to the size of `correlation_buffer_size` because of how it is used
        // by the peak-detection algorithm (it expects one extra element when
        // num_peaks == 1).
        let correlation_buffer_size = 2 * pad_length + K_MAX_CORRELATION_LENGTH + 1;
        let max_correlation = spl::max_abs_value_w32(&correlation, stop_position_downsamp);
        let norm_shift = 0.max(17 - spl::norm_w32(max_correlation) as i32);

        // Calculate allowed starting point for peak finding.
        // The peak location bestIndex must fulfill two criteria:
        // (1) w16_bestIndex + input_length <
        //     timestamps_per_call_ + expand_->overlap_length();
        // (2) w16_bestIndex + input_length < start_position.
        let mut start_index = self.timestamps_per_call + expand.overlap_length();
        start_index = start_position.max(start_index);
        // C++ `(input_length > start_index) ? 0 : (start_index - input_length)`.
        start_index = start_index.saturating_sub(input_length);
        // Downscale starting index to 4kHz domain. (fs_mult_ * 2 = fs_hz_ / 4000.)
        let start_index_downsamp = start_index / (self.fs_mult as usize * 2);

        // Calculate a modified `stop_position_downsamp` to account for the increased
        // start index `start_index_downsamp` and the effective array length.
        let modified_stop_pos = stop_position_downsamp
            .min((K_MAX_CORRELATION_LENGTH + pad_length).wrapping_sub(start_index_downsamp));

        // C++ reads past the end of `correlation16` when the start index is
        // beyond `kMaxCorrelationLength + pad_length` (heap over-read, UB). The
        // buffer here is zero-padded so that case cannot panic; every in-bounds
        // case reads exactly the same values as C++.
        let needed = pad_length + start_index_downsamp + modified_stop_pos + 1;
        let mut correlation16 = vec![0i16; correlation_buffer_size.max(needed)];
        spl::vector_bit_shift_w32_to_w16(
            &mut correlation16[pad_length..],
            stop_position_downsamp,
            &correlation,
            norm_shift,
        );

        let mut best_correlation_index = [0usize; 1];
        let mut best_correlation = [0i16; 1];
        const K_NUM_CORRELATION_CANDIDATES: usize = 1;
        DspHelper::peak_detection(
            &mut correlation16[pad_length + start_index_downsamp..],
            modified_stop_pos,
            K_NUM_CORRELATION_CANDIDATES,
            self.fs_mult,
            &mut best_correlation_index,
            &mut best_correlation,
        );
        // Compensate for modified start index.
        let mut best_correlation_index = best_correlation_index[0].wrapping_add(start_index);

        // Ensure that underrun does not occur for 10ms case => we have to get at
        // least 10ms + overlap . (This should never happen thanks to the above
        // modification of peak-finding starting point.)
        while best_correlation_index.wrapping_add(input_length)
            < self.timestamps_per_call + expand.overlap_length()
            || best_correlation_index.wrapping_add(input_length) < start_position
        {
            // C++ has `RTC_DCHECK_NOTREACHED()` here ("Should never happen");
            // it is a no-op in the release build the parity harness runs.
            best_correlation_index += expand_period; // Jump one lag ahead.
        }
        best_correlation_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::neteq::background_noise::BackgroundNoise;
    use crate::neteq::expand::golden_tests::speech_like;
    use crate::neteq::random_vector::RandomVector;
    use crate::neteq::statistics_calculator::StatisticsCalculator;
    use crate::neteq::sync_buffer::SyncBuffer;

    // This is the same size that is given to the SyncBuffer object in NetEq.
    const K_NET_EQ_SYNC_BUFFER_LENGTH_MS: usize = 720;

    #[test]
    fn create_and_destroy() {
        let fs = 8000;
        let channels = 1;
        let _bgn = BackgroundNoise::new(channels);
        let _sync_buffer = SyncBuffer::new(1, 1000);
        let _random_vector = RandomVector::new();
        let _statistics = StatisticsCalculator::new();
        let _expand = Expand::new(fs, channels);
        let _merge = Merge::new(fs, channels);
    }

    #[test]
    fn required_future_samples() {
        assert_eq!(80, Merge::new(8000, 1).required_future_samples());
        assert_eq!(960, Merge::new(48000, 2).required_future_samples());
    }

    /// C++ `MergeTest.Process`, with a deterministic voiced signal instead of
    /// `testfile32kHz.pcm` (not in the sparse clone). Values of interest in
    /// `Merge::Downsample` at 8 kHz: `signal_offset` is 2 (1, 2, 3 are around
    /// it), `length_limit` is 80 (80 and 81 straddle it), and 160 is 20 ms.
    #[test]
    fn process_different_input_lengths() {
        for input_len in [1usize, 2, 3, 80, 81, 160] {
            let test_sample_rate_hz = 8000;
            let num_channels = 1;
            let mut background_noise = BackgroundNoise::new(num_channels);
            let mut sync_buffer = SyncBuffer::new(
                num_channels,
                K_NET_EQ_SYNC_BUFFER_LENGTH_MS * test_sample_rate_hz as usize / 1000,
            );
            let mut random_vector = RandomVector::new();
            let mut statistics = StatisticsCalculator::new();
            let mut expand = Expand::new(test_sample_rate_hz, num_channels);
            let mut merge = Merge::new(test_sample_rate_hz, num_channels);

            // Pre-load the sync buffer with speech data.
            let size = sync_buffer.size();
            let signal = speech_like(size, test_sample_rate_hz, 1);
            sync_buffer.channel_mut(0).overwrite_at_slice(&signal, 0);
            // Move index such that the sync buffer appears to have 5 ms left to play.
            let next_index = sync_buffer.next_index() - test_sample_rate_hz as usize * 5 / 1000;
            sync_buffer.set_next_index(next_index);
            assert!(sync_buffer.future_length() > 0);

            let mut ctx = ExpandContext {
                sync_buffer: &mut sync_buffer,
                background_noise: &mut background_noise,
                random_vector: &mut random_vector,
                statistics: &mut statistics,
            };
            let mut output = AudioMultiVector::new(num_channels);
            // Start by calling Expand once, to prime the state.
            assert_eq!(0, expand.process(&mut ctx, &mut output));
            assert!(output.size() > 0);
            output.clear();
            // Now call Merge, but with a very short decoded input.
            let mut input = vec![17i16; input_len];
            merge.process(&mut input, input_len, &mut output, &mut expand, &mut ctx);
            assert!(output.size() > 0, "input_len {input_len}");
        }
    }
}

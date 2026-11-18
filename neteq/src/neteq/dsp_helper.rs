// Port target: WebRTC modules/audio_coding/neteq/dsp_helper.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: dsp_helper_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Static DSP helpers shared by the NetEq signal-processing components.

use crate::neteq::audio_multi_vector::AudioMultiVector;
use crate::neteq::audio_vector::AudioVector;
use crate::spl;

/// C++ `DspHelper` (a class of static members).
#[derive(Debug)]
pub struct DspHelper;

impl DspHelper {
    /// C++ `kParabolaCoefficients`, used by `parabolic_fit`.
    pub const K_PARABOLA_COEFFICIENTS: [[i16; 3]; 17] = [
        [120, 32, 64],
        [140, 44, 75],
        [150, 50, 80],
        [160, 57, 85],
        [180, 72, 96],
        [200, 89, 107],
        [210, 98, 112],
        [220, 108, 117],
        [240, 128, 128],
        [260, 150, 139],
        [270, 162, 144],
        [280, 174, 149],
        [300, 200, 160],
        [320, 228, 171],
        [330, 242, 176],
        [340, 257, 181],
        [360, 288, 192],
    ];

    /// Filter coefficients used when downsampling from 8 kHz (Q12).
    pub const K_DOWNSAMPLE_8KHZ_TBL: [i16; 3] = [1229, 1638, 1229];
    /// From 16 kHz (Q12).
    pub const K_DOWNSAMPLE_16KHZ_TBL: [i16; 5] = [614, 819, 1229, 819, 614];
    /// From 32 kHz (Q12).
    pub const K_DOWNSAMPLE_32KHZ_TBL: [i16; 7] = [584, 512, 625, 667, 625, 512, 584];
    /// From 48 kHz (Q12).
    pub const K_DOWNSAMPLE_48KHZ_TBL: [i16; 7] = [1019, 390, 427, 440, 427, 390, 1019];

    // Mute/unmute factors in Q14 for each sample rate.
    pub const K_MUTE_FACTOR_START_8KHZ: i32 = 27307;
    pub const K_MUTE_FACTOR_INCREMENT_8KHZ: i32 = -5461;
    pub const K_UNMUTE_FACTOR_START_8KHZ: i32 = 5461;
    pub const K_UNMUTE_FACTOR_INCREMENT_8KHZ: i32 = 5461;
    pub const K_MUTE_FACTOR_START_16KHZ: i32 = 29789;
    pub const K_MUTE_FACTOR_INCREMENT_16KHZ: i32 = -2979;
    pub const K_UNMUTE_FACTOR_START_16KHZ: i32 = 2979;
    pub const K_UNMUTE_FACTOR_INCREMENT_16KHZ: i32 = 2979;
    pub const K_MUTE_FACTOR_START_32KHZ: i32 = 31208;
    pub const K_MUTE_FACTOR_INCREMENT_32KHZ: i32 = -1560;
    pub const K_UNMUTE_FACTOR_START_32KHZ: i32 = 1560;
    pub const K_UNMUTE_FACTOR_INCREMENT_32KHZ: i32 = 1560;
    pub const K_MUTE_FACTOR_START_48KHZ: i32 = 31711;
    pub const K_MUTE_FACTOR_INCREMENT_48KHZ: i32 = -1057;
    pub const K_UNMUTE_FACTOR_START_48KHZ: i32 = 1057;
    pub const K_UNMUTE_FACTOR_INCREMENT_48KHZ: i32 = 1057;

    /// One step of the C++ `RampSignal` loop: returns the scaled sample and
    /// advances `factor` / `factor_q20`.
    #[inline]
    fn ramp_step(sample: i16, factor: &mut i32, factor_q20: &mut i32, increment: i32) -> i16 {
        // `int` result stored into `int16_t`: truncation, like C++.
        let out = ((*factor * sample as i32 + 8192) >> 14) as i16;
        *factor_q20 = factor_q20.wrapping_add(increment);
        *factor_q20 = (*factor_q20).max(0); // Never go negative.
        *factor = (*factor_q20 >> 6).min(16384);
        out
    }

    /// Multiplies `input[..length]` by a gain ramping from `factor` (Q14) by
    /// `increment` (Q20) per sample, into `output`. Returns the final factor.
    pub fn ramp_signal(
        input: &[i16],
        length: usize,
        factor: i32,
        increment: i32,
        output: &mut [i16],
    ) -> i32 {
        let mut factor = factor;
        // `factor << 6` on a negative factor is UB before C++20; it wraps here.
        let mut factor_q20 = factor.wrapping_shl(6).wrapping_add(32);
        for (out, &sample) in output[..length].iter_mut().zip(&input[..length]) {
            *out = Self::ramp_step(sample, &mut factor, &mut factor_q20, increment);
        }
        factor
    }

    /// In-place variant of `ramp_signal`.
    pub fn ramp_signal_in_place(
        signal: &mut [i16],
        length: usize,
        factor: i32,
        increment: i32,
    ) -> i32 {
        let mut factor = factor;
        let mut factor_q20 = factor.wrapping_shl(6).wrapping_add(32);
        for sample in signal[..length].iter_mut() {
            *sample = Self::ramp_step(*sample, &mut factor, &mut factor_q20, increment);
        }
        factor
    }

    /// `ramp_signal` on `signal[start_index..start_index + length]`.
    pub fn ramp_signal_audio_vector(
        signal: &mut AudioVector,
        start_index: usize,
        length: usize,
        factor: i32,
        increment: i32,
    ) -> i32 {
        let mut factor = factor;
        let mut factor_q20 = factor.wrapping_shl(6).wrapping_add(32);
        for i in start_index..start_index + length {
            signal[i] = Self::ramp_step(signal[i], &mut factor, &mut factor_q20, increment);
        }
        factor
    }

    /// `ramp_signal` on every channel of `signal`, each starting at `factor`.
    /// Returns the end factor of the last channel, or `factor` unchanged if
    /// the range does not fit.
    pub fn ramp_signal_multi(
        signal: &mut AudioMultiVector,
        start_index: usize,
        length: usize,
        factor: i32,
        increment: i32,
    ) -> i32 {
        // C++ also has `RTC_DCHECK_LE(start_index + length, signal->Size())`
        // here, but handles the case right after; the release behavior is
        // what the harness (built with NDEBUG) runs, so that is what is kept.
        if start_index + length > signal.size() {
            // Wrong parameters. Do nothing and return the scale factor unaltered.
            return factor;
        }
        let mut end_factor = 0;
        for channel in 0..signal.channels() {
            end_factor = Self::ramp_signal_audio_vector(
                &mut signal[channel],
                start_index,
                length,
                factor,
                increment,
            );
        }
        end_factor
    }

    /// C++ `RefinePeak` (anonymous namespace): refines integer `peak` (the
    /// argmax of `data`) to sub-sample resolution. `edge_index` is the
    /// largest index that can be a peak; `next` is the sample right of
    /// `peak`, used only when `peak == edge_index`.
    fn refine_peak(
        data: &[i16],
        peak: usize,
        edge_index: usize,
        next: i16,
        fs_mult: i32,
        peak_index: &mut usize,
        peak_value: &mut i16,
    ) {
        // ParabolicFit refines peak_index in place, so seed it first.
        *peak_index = peak;
        if peak == 0 {
            *peak_value = data[0];
            *peak_index = 0;
        } else if peak == edge_index {
            if data[peak] > next {
                let points = [data[peak - 1], data[peak], next];
                Self::parabolic_fit(&points, fs_mult, peak_index, peak_value);
            } else {
                // Linear approximation: `std::midpoint` on `int16_t`, which
                // rounds towards the first argument. `a + (b - a) / 2` with
                // truncating division does exactly that.
                let a = data[peak] as i32;
                let b = next as i32;
                *peak_value = (a + (b - a) / 2) as i16;
                *peak_index = (peak * 2 + 1).wrapping_mul(fs_mult as usize);
            }
        } else {
            // Interior peak.
            Self::parabolic_fit(&data[peak - 1..peak + 2], fs_mult, peak_index, peak_value);
        }
    }

    /// Finds `num_peaks` peaks in `data[..data_length]` (overwrites `data`
    /// around each peak found), with parabolic refinement at `fs_mult`.
    pub fn peak_detection(
        data: &mut [i16],
        data_length: usize,
        num_peaks: usize,
        fs_mult: i32,
        peak_index: &mut [usize],
        peak_value: &mut [i16],
    ) {
        debug_assert!(data_length > 0);
        if data_length == 0 {
            return;
        }
        // A single peak may sit on the last real sample; treat the sample past
        // it as zero. For multiple peaks the last sample is excluded from the
        // search and instead used as a peak's right neighbour.
        let single_peak = num_peaks == 1;
        let search_length = if single_peak {
            data_length
        } else {
            data_length - 1
        };
        let edge_index = search_length.wrapping_sub(1);

        for i in 0..num_peaks {
            let peak = spl::max_index_w16(data, search_length);
            let next = if single_peak && peak == edge_index {
                0
            } else {
                data[peak + 1]
            };
            Self::refine_peak(
                &data[..data_length],
                peak,
                edge_index,
                next,
                fs_mult,
                &mut peak_index[i],
                &mut peak_value[i],
            );

            if i != num_peaks - 1 {
                // Zero a window around this peak so the next search finds a new one.
                let min_index = peak.saturating_sub(2);
                let max_index = (data_length - 1).min(peak + 2);
                data[min_index..=max_index].fill(0);
            }
        }
    }

    /// Fits a parabola through the 3 `signal_points` and refines
    /// `peak_index` / `peak_value` at resolution `fs_mult`.
    ///
    /// All products are `int`; `/ 256` truncates toward zero and the result
    /// is stored into `int16_t` (truncating), as in C++. `peak_index` math is
    /// `size_t` (`int` operands convert), so it wraps.
    pub fn parabolic_fit(
        signal_points: &[i16],
        fs_mult: i32,
        peak_index: &mut usize,
        peak_value: &mut i16,
    ) {
        let fit_index: &[u16] = match fs_mult {
            1 => &[0, 8, 16],
            2 => &[0, 4, 8, 12, 16],
            4 => &[0, 2, 4, 6, 8, 10, 12, 14, 16],
            _ => &[0, 1, 3, 4, 5, 7, 8, 9, 11, 12, 13, 15, 16],
        };
        let coefficients = |k: i32| Self::K_PARABOLA_COEFFICIENTS[fit_index[k as usize] as usize];

        let p0 = signal_points[0] as i32;
        let p1 = signal_points[1] as i32;
        let p2 = signal_points[2] as i32;
        //  num = -3 * signal_points[0] + 4 * signal_points[1] - signal_points[2];
        //  den =      signal_points[0] - 2 * signal_points[1] + signal_points[2];
        let num: i32 = (p0 * -3) + (p1 * 4) - p2;
        let den: i32 = p0 + (p1 * -2) + p2;
        let temp: i32 = num.wrapping_mul(120);
        let mut flag: i32 = 1;
        let stp: i16 = coefficients(fs_mult)[0] - coefficients(fs_mult - 1)[0];
        let strt: i16 =
            ((coefficients(fs_mult)[0] as i32 + coefficients(fs_mult - 1)[0] as i32) / 2) as i16;
        let mut lmt: i16;
        let mul_den = |x: i32| den.wrapping_neg().wrapping_mul(x);
        if temp < mul_den(strt as i32) {
            lmt = strt - stp;
            while flag != 0 {
                if flag == fs_mult || temp > mul_den(lmt as i32) {
                    let c = coefficients(fs_mult - flag);
                    *peak_value = ((den * c[1] as i32 + num * c[2] as i32 + p0 * 256) / 256) as i16;
                    *peak_index = (*peak_index)
                        .wrapping_mul(2)
                        .wrapping_mul(fs_mult as usize)
                        .wrapping_sub(flag as usize);
                    flag = 0;
                } else {
                    flag += 1;
                    lmt -= stp;
                }
            }
        } else if temp > mul_den((strt + stp) as i32) {
            lmt = strt + 2 * stp;
            while flag != 0 {
                if flag == fs_mult || temp < mul_den(lmt as i32) {
                    let c = coefficients(fs_mult + flag);
                    let temp_term_1 = den * c[1] as i32;
                    let temp_term_2 = num * c[2] as i32;
                    let temp_term_3 = p0 * 256;
                    *peak_value = ((temp_term_1 + temp_term_2 + temp_term_3) / 256) as i16;
                    *peak_index = (*peak_index)
                        .wrapping_mul(2)
                        .wrapping_mul(fs_mult as usize)
                        .wrapping_add(flag as usize);
                    flag = 0;
                } else {
                    flag += 1;
                    lmt += stp;
                }
            }
        } else {
            *peak_value = signal_points[1];
            *peak_index = (*peak_index).wrapping_mul(2).wrapping_mul(fs_mult as usize);
        }
    }

    /// C++ `MinDistortion`. C reads `signal_ptr[i - lag]`, i.e. before the
    /// pointer, so `signal` is the whole buffer and `signal_offset` is where
    /// the C pointer points (must be at least `max_lag`). Returns the best lag
    /// and writes its distortion to `distortion_value`.
    ///
    /// The `int32_t` sum may overflow in C (UB, wraps in practice); it wraps.
    pub fn min_distortion(
        signal: &[i16],
        signal_offset: usize,
        min_lag: usize,
        max_lag: usize,
        length: usize,
        distortion_value: &mut i32,
    ) -> usize {
        let mut best_index = 0;
        let mut min_distortion = spl::WORD32_MAX;
        let data1 = &signal[signal_offset..signal_offset + length];
        for i in min_lag..=max_lag {
            let data2 = &signal[signal_offset - i..signal_offset - i + length];
            let mut sum_diff: i32 = 0;
            for (&a, &b) in data1.iter().zip(data2) {
                sum_diff = sum_diff.wrapping_add(spl::spl_abs_w32(a as i32 - b as i32));
            }
            // Compare with previous minimum.
            if sum_diff < min_distortion {
                min_distortion = sum_diff;
                best_index = i;
            }
        }
        *distortion_value = min_distortion;
        best_index
    }

    /// Cross-fades `input1` into `input2` over `length` samples, starting at
    /// `mix_factor` (Q14) and decreasing by `factor_decrement` per sample.
    ///
    /// `factor` and `complement_factor` are `int16_t` in C++; the updates are
    /// computed in `int` and truncated back, and so are the output samples.
    pub fn cross_fade(
        input1: &[i16],
        input2: &[i16],
        length: usize,
        mix_factor: &mut i16,
        factor_decrement: i16,
        output: &mut [i16],
    ) {
        let mut factor: i16 = *mix_factor;
        let mut complement_factor: i16 = (16384 - factor as i32) as i16;
        for i in 0..length {
            // Both products can reach 2^30, so the `int` sum may overflow
            // (UB in C++, wraps in practice).
            output[i] = ((factor as i32 * input1[i] as i32)
                .wrapping_add(complement_factor as i32 * input2[i] as i32)
                .wrapping_add(8192)
                >> 14) as i16;
            factor = (factor as i32 - factor_decrement as i32) as i16;
            complement_factor = (complement_factor as i32 + factor_decrement as i32) as i16;
        }
        *mix_factor = factor;
    }

    /// Scales `input[..length]` by `factor` (Q14), raising it by `increment`
    /// per sample, into `output`. Updates `factor`.
    ///
    /// C++ keeps the factor in a `uint16_t` (`*factor` converts from
    /// `int16_t` modulo 2^16) and stores it back into the `int16_t`.
    pub fn unmute_signal(
        input: &[i16],
        length: usize,
        factor: &mut i16,
        increment: i32,
        output: &mut [i16],
    ) {
        let mut factor_16b: u16 = *factor as u16;
        let mut factor_32b: i32 = ((factor_16b as i32) << 6) + 32;
        for i in 0..length {
            output[i] = ((factor_16b as i32 * input[i] as i32 + 8192) >> 14) as i16;
            factor_32b = factor_32b.wrapping_add(increment).max(0);
            factor_16b = 16384.min(factor_32b >> 6) as u16;
        }
        *factor = factor_16b as i16;
    }

    /// Fades `signal[..length]` down with `mute_slope` (Q20) per sample.
    ///
    /// `factor` may go negative; `factor >> 6` is then an arithmetic shift
    /// (implementation-defined in C++, arithmetic in practice).
    pub fn mute_signal(signal: &mut [i16], mute_slope: i32, length: usize) {
        let mut factor: i32 = (16384 << 6) + 32;
        for sample in signal[..length].iter_mut() {
            *sample = ((factor >> 6)
                .wrapping_mul(*sample as i32)
                .wrapping_add(8192)
                >> 14) as i16;
            factor = factor.wrapping_sub(mute_slope);
        }
    }

    /// Downsamples `input[..input_length]` at `input_rate_hz` to 4 kHz,
    /// writing `output_length` samples. Returns 0 or -1 on error.
    ///
    /// The phase delay values are wrong compared to the true phase delay of
    /// the filters; C++ preserves the error (the `+ 1` terms), and so does
    /// this port.
    pub fn downsample_to_4khz(
        input: &[i16],
        input_length: usize,
        output_length: usize,
        input_rate_hz: i32,
        compensate_delay: bool,
        output: &mut [i16],
    ) -> i32 {
        let (filter_coefficients, filter_length, factor, mut filter_delay): (
            &[i16],
            usize,
            i16,
            usize,
        ) = match input_rate_hz {
            8000 => (&Self::K_DOWNSAMPLE_8KHZ_TBL, 3, 2, 1 + 1),
            16000 => (&Self::K_DOWNSAMPLE_16KHZ_TBL, 5, 4, 2 + 1),
            32000 => (&Self::K_DOWNSAMPLE_32KHZ_TBL, 7, 8, 3 + 1),
            48000 => (&Self::K_DOWNSAMPLE_48KHZ_TBL, 7, 12, 3 + 1),
            _ => {
                debug_assert!(false, "unsupported rate {input_rate_hz}");
                return -1;
            }
        };

        if !compensate_delay {
            // Disregard delay compensation.
            filter_delay = 0;
        }

        // Returns -1 if the input signal is too short; 0 otherwise.
        spl::downsample_fast(
            input,
            filter_length - 1,
            input_length.wrapping_sub(filter_length).wrapping_add(1),
            output,
            output_length,
            filter_coefficients,
            filter_length,
            factor as i32,
            filter_delay,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::neteq::audio_multi_vector::golden::{feed_multi, fill_multi};
    use crate::spl::test_util::{Hash, Rng};

    // dsp_helper_unittest.cc: DspHelper.RampSignalArray
    #[test]
    fn ramp_signal_array() {
        const LEN: usize = 100;
        let mut input = [1000i16; LEN];
        let mut output = [0i16; LEN];
        let start_factor = 0;
        // Ramp from 0 to 1 (in Q14) over the array. `increment` is in Q20.
        let increment = (16384 << 6) / LEN as i32;
        let stop_factor = DspHelper::ramp_signal(&input, LEN, start_factor, increment, &mut output);
        assert_eq!(16383, stop_factor); // Almost reach 1 in Q14.
        for (i, &out) in output.iter().enumerate() {
            assert_eq!((1000 * i / LEN) as i16, out);
        }

        let stop_factor = DspHelper::ramp_signal_in_place(&mut input, LEN, start_factor, increment);
        assert_eq!(16383, stop_factor);
        for (i, &value) in input.iter().enumerate() {
            assert_eq!((1000 * i / LEN) as i16, value);
        }
    }

    // dsp_helper_unittest.cc: DspHelper.RampSignalAudioMultiVector
    #[test]
    fn ramp_signal_audio_multi_vector() {
        const LEN: usize = 100;
        const CHANNELS: usize = 5;
        let mut input = AudioMultiVector::with_size(CHANNELS, LEN * 3);
        for i in 0..LEN * 3 {
            for channel in 0..CHANNELS {
                input[channel][i] = 1000;
            }
        }
        let start_index = LEN;
        let increment = (16384 << 6) / LEN as i32;
        let stop_factor = DspHelper::ramp_signal_multi(&mut input, start_index, LEN, 0, increment);
        assert_eq!(16383, stop_factor);
        for i in 0..3 * LEN {
            for channel in 0..CHANNELS {
                let expected = if (LEN..2 * LEN).contains(&i) {
                    (1000 * (i - LEN) / LEN) as i16
                } else {
                    1000
                };
                assert_eq!(expected, input[channel][i]);
            }
        }
    }

    // Not in WebRTC: exact values for the helpers the C++ tests skip.
    #[test]
    fn exact_helper_values() {
        // Interior peak at index 2, refined at 8 kHz.
        let mut data = [0i16, 10, 20, 10, 0];
        let mut peak_index = [0usize; 1];
        let mut peak_value = [0i16; 1];
        DspHelper::peak_detection(&mut data, 5, 1, 1, &mut peak_index, &mut peak_value);
        assert_eq!((4, 20), (peak_index[0], peak_value[0]));

        // Single peak on the last sample: the neighbour past it counts as 0.
        let mut data = [0i16, 5, 9];
        DspHelper::peak_detection(&mut data, 3, 1, 2, &mut peak_index, &mut peak_value);
        let mut index = 2usize;
        let mut value = 0i16;
        DspHelper::parabolic_fit(&[5, 9, 0], 2, &mut index, &mut value);
        assert_eq!((index, value), (peak_index[0], peak_value[0]));

        // Linear approximation uses std::midpoint, which rounds toward the
        // first argument: midpoint(-3, 0) == -2 (not -1).
        let mut data = [-9i16, -5, -3, 0];
        let mut peak_index = [0usize; 2];
        let mut peak_value = [0i16; 2];
        DspHelper::peak_detection(&mut data, 4, 2, 1, &mut peak_index, &mut peak_value);
        assert_eq!((5, -2), (peak_index[0], peak_value[0]));

        let mut mix = 16384i16;
        let mut out = [0i16; 2];
        DspHelper::cross_fade(&[100, 100], &[0, 0], 2, &mut mix, 8192, &mut out);
        assert_eq!(([100, 50], 0), (out, mix));

        let mut factor = 0i16;
        DspHelper::unmute_signal(&[1000, 1000], 2, &mut factor, 1 << 19, &mut out);
        assert_eq!(([0, 500], 16384), (out, factor));

        let mut signal = [1000i16, 1000, 1000];
        DspHelper::mute_signal(&mut signal, 1 << 19, 3);
        assert_eq!([1000, 500, 0], signal);

        let mut distortion = 0;
        let signal = [1i16, 2, 3, 1, 2, 3];
        assert_eq!(
            3,
            DspHelper::min_distortion(&signal, 3, 1, 3, 3, &mut distortion)
        );
        assert_eq!(0, distortion);

        // Input too short for one output sample at 48 kHz.
        let mut out = [0i16; 1];
        assert_eq!(
            -1,
            DspHelper::downsample_to_4khz(&[0; 8], 6, 1, 48000, true, &mut out)
        );
    }

    // Golden hash against the real C++ (see spl/golden/golden_gen.cc).
    #[test]
    fn golden_dsp_helper() {
        const FS_MULT: [i32; 4] = [1, 2, 4, 6];
        const RATES: [i32; 4] = [8000, 16000, 32000, 48000];
        let mut r = Rng::new();
        let mut h = Hash::new();
        for _ in 0..2000 {
            {
                let len = r.urange(0, 50);
                let factor = r.range(0, 16384);
                let inc = r.range(-20000, 20000);
                let mut input = r.vec16(len);
                let mut out = vec![0i16; len];
                h.feed(DspHelper::ramp_signal(&input, len, factor, inc, &mut out) as i64);
                h.feed16(&out);
                h.feed(DspHelper::ramp_signal_in_place(&mut input, len, factor, inc) as i64);
                h.feed16(&input);
                let ch = r.urange(1, 3);
                let mut mv = AudioMultiVector::new(ch);
                fill_multi(&mut r, &mut mv, 60);
                let start = r.below(mv.size() + 2);
                let rlen = r.below(mv.size() + 2);
                h.feed(DspHelper::ramp_signal_multi(&mut mv, start, rlen, factor, inc) as i64);
                feed_multi(&mut h, &mv);
            }
            {
                let num_peaks = r.urange(1, 3);
                let len = r.urange(num_peaks.max(2), 60);
                let fs_mult = FS_MULT[r.below(4)];
                let mut data = r.vec16(len);
                let mut idx = [0usize; 3];
                let mut val = [0i16; 3];
                DspHelper::peak_detection(&mut data, len, num_peaks, fs_mult, &mut idx, &mut val);
                for i in 0..num_peaks {
                    h.feed(idx[i] as i64);
                    h.feed(val[i] as i64);
                }
                h.feed16(&data);
            }
            {
                let pts = [r.i16(), r.i16(), r.i16()];
                let fs_mult = FS_MULT[r.below(4)];
                let mut idx = r.urange(0, 100);
                let mut val = 0i16;
                DspHelper::parabolic_fit(&pts, fs_mult, &mut idx, &mut val);
                h.feed(idx as i64);
                h.feed(val as i64);
            }
            {
                let max_lag = r.urange(1, 20);
                let min_lag = r.urange(0, max_lag);
                let len = r.urange(1, 40);
                let sig = r.vec16(max_lag + len);
                let mut dist = 0;
                h.feed(
                    DspHelper::min_distortion(&sig, max_lag, min_lag, max_lag, len, &mut dist)
                        as i64,
                );
                h.feed(dist as i64);
            }
            {
                let len = r.urange(0, 40);
                let a = r.vec16(len);
                let b = r.vec16(len);
                let mut out = vec![0i16; len];
                let mut mix = r.range(0, 16384) as i16;
                let dec = r.range(0, 2000) as i16;
                DspHelper::cross_fade(&a, &b, len, &mut mix, dec, &mut out);
                h.feed(mix as i64);
                h.feed16(&out);
            }
            {
                let len = r.urange(0, 40);
                let mut input = r.vec16(len);
                let mut out = vec![0i16; len];
                let mut factor = r.i16();
                let inc = r.range(-5000, 70000);
                DspHelper::unmute_signal(&input, len, &mut factor, inc, &mut out);
                h.feed(factor as i64);
                h.feed16(&out);
                let slope = r.range(-1000, 30000);
                DspHelper::mute_signal(&mut input, slope, len);
                h.feed16(&input);
            }
            {
                let rate = RATES[r.below(4)];
                let comp = r.below(2) == 1;
                let out_len = r.urange(1, 20);
                let in_len = r.urange(7, 300);
                let input = r.vec16(310);
                let mut out = vec![0i16; out_len];
                let ret =
                    DspHelper::downsample_to_4khz(&input, in_len, out_len, rate, comp, &mut out);
                h.feed(ret as i64);
                if ret == 0 {
                    h.feed16(&out);
                }
            }
        }
        assert_eq!(0xc0a2462b1e0e533d, h.value());
    }
}

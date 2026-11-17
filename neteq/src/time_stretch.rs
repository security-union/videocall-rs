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

//! Time stretching, ported from libwebrtc (commit aeb9b375):
//! `modules/audio_coding/neteq/time_stretch.{h,cc}`, `accelerate.cc`,
//! `preemptive_expand.cc`, and the helpers they call in `dsp_helper.cc` and
//! `audio_vector.cc`.
//!
//! Both operations take about 30 ms of audio, estimate the pitch period on
//! a 4 kHz copy of the first channel, and remove (accelerate) or repeat
//! (preemptive expand) one pitch period by overlap-add at the 15 ms mark.
//! Active speech is only stretched when the two periods correlate above
//! 0.9; audio at background-noise level is always stretched.
//!
//! Analysis runs in `f64` on samples scaled to 16-bit range, so libwebrtc's
//! energy constants carry over. libwebrtc's fixed-point arithmetic is not
//! reproduced bit for bit, except in `parabolic_fit`.

use crate::background_noise::BackgroundNoise;

/// time_stretch.h `TimeStretch::ReturnCodes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeStretchResult {
    /// Active speech was stretched.
    Success,
    /// Background noise was stretched.
    SuccessLowEnergy,
    /// The criteria were not met; the output is the input.
    NoStretch,
    /// The input was too short; the output is the input.
    Error,
}

/// time_stretch.h `kCorrelationLen`.
const K_CORRELATION_LEN: usize = 50;
/// time_stretch.h `kMinLag`.
const K_MIN_LAG: usize = 10;
/// time_stretch.h `kMaxLag`.
const K_MAX_LAG: usize = 60;
/// time_stretch.h `kDownsampledLen`.
const K_DOWNSAMPLED_LEN: usize = K_CORRELATION_LEN + K_MAX_LAG;
/// time_stretch.h `kCorrelationThreshold`: 0.9 in Q14.
const K_CORRELATION_THRESHOLD: i32 = 14746;
/// accelerate.cc `CheckCriteriaAndStretch`: 0.5 in Q14 in fast mode.
const K_FAST_CORRELATION_THRESHOLD: i32 = 8192;
/// accelerate.cc / preemptive_expand.cc `k15ms`, at 8 kHz.
const K_15MS: usize = 120;
/// time_stretch.cc `SpeechDetection`: background energy before any
/// estimate exists.
const K_DEFAULT_BACKGROUND_ENERGY: f64 = 75000.0;

/// dsp_helper.cc `kParabolaCoefficients`.
const K_PARABOLA_COEFFICIENTS: [[i32; 3]; 17] = [
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

/// What the shared analysis (`TimeStretch::Process`) found.
struct Analysis {
    /// Pitch period in samples per channel.
    peak_index: usize,
    /// Correlation between the two periods, Q14.
    best_correlation: i32,
    active_speech: bool,
}

/// time_stretch.h `TimeStretch`: the part shared by both operations.
#[derive(Debug, Clone)]
struct TimeStretch {
    sample_rate: u32,
    /// `fs_mult_`: sample rate / 8000.
    fs_mult: usize,
    channels: usize,
}

impl TimeStretch {
    fn new(sample_rate: u32, channels: usize) -> Self {
        Self {
            sample_rate,
            fs_mult: (sample_rate / 8000) as usize,
            channels: channels.max(1),
        }
    }

    /// libwebrtc only supports 8, 16, 32 and 48 kHz (time_stretch.h
    /// constructor DCHECK).
    fn supported(&self) -> bool {
        matches!(self.sample_rate, 8000 | 16000 | 32000 | 48000)
    }

    /// time_stretch.cc `TimeStretch::Process`, up to the call of
    /// `CheckCriteriaAndStretch`.
    fn analyze(&self, input: &[f32], background_noise: &BackgroundNoise) -> Analysis {
        let fs_mult_120 = self.fs_mult * K_15MS;
        // kRefChannel = 0, scaled to 16-bit range.
        let signal: Vec<f64> = input
            .iter()
            .step_by(self.channels)
            .map(|&s| f64::from(s) * 32768.0)
            .collect();

        let downsampled = downsample_to_4khz(&signal, self.sample_rate);
        let auto_correlation = auto_correlation(&downsampled);
        // DspHelper::PeakDetection with one peak, then shift by kMinLag.
        let peak_index =
            peak_detection(&auto_correlation, self.fs_mult) + K_MIN_LAG * self.fs_mult * 2;

        let vec1 = &signal[fs_mult_120 - peak_index..fs_mult_120];
        let vec2 = &signal[fs_mult_120..fs_mult_120 + peak_index];
        let vec1_energy: f64 = vec1.iter().map(|x| x * x).sum();
        let vec2_energy: f64 = vec2.iter().map(|x| x * x).sum();
        let cross_corr: f64 = vec1.iter().zip(vec2).map(|(a, b)| a * b).sum();

        let active_speech =
            speech_detection(vec1_energy, vec2_energy, peak_index, background_noise);
        let best_correlation = if active_speech {
            // Normalized correlation in Q14, negative values not used,
            // capped at 1.0.
            let energy = (vec1_energy * vec2_energy).sqrt();
            if energy > 0.0 {
                ((cross_corr.max(0.0) / energy * 16384.0).floor() as i32).min(16384)
            } else {
                0
            }
        } else {
            0
        };
        Analysis {
            peak_index,
            best_correlation,
            active_speech,
        }
    }
}

/// accelerate.{h,cc} `Accelerate`: removes one pitch period.
#[derive(Debug, Clone)]
pub struct Accelerate {
    base: TimeStretch,
}

impl Accelerate {
    /// Create an accelerator for interleaved audio.
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        Self {
            base: TimeStretch::new(sample_rate, channels),
        }
    }

    /// accelerate.cc `Accelerate::Process`. `input` is interleaved and
    /// should be 30 ms long. The stretched audio is appended to `output`.
    /// Returns the result and the number of samples per channel removed.
    pub fn process(
        &self,
        input: &[f32],
        fast_accelerate: bool,
        background_noise: &BackgroundNoise,
        output: &mut Vec<f32>,
    ) -> (TimeStretchResult, usize) {
        let ch = self.base.channels;
        let fs_mult = self.base.fs_mult;
        // "Input length must be (almost) 30 ms."
        if !self.base.supported() || input.len() / ch < (2 * K_15MS - 1) * fs_mult {
            output.extend_from_slice(input);
            return (TimeStretchResult::Error, 0);
        }
        let analysis = self.base.analyze(input, background_noise);
        // accelerate.cc SetParametersForPassiveSpeech: correlation is 0 and
        // the peak is kept.
        self.check_criteria_and_stretch(input, analysis, fast_accelerate, output)
    }

    /// accelerate.cc `Accelerate::CheckCriteriaAndStretch`.
    fn check_criteria_and_stretch(
        &self,
        input: &[f32],
        analysis: Analysis,
        fast_mode: bool,
        output: &mut Vec<f32>,
    ) -> (TimeStretchResult, usize) {
        let ch = self.base.channels;
        let threshold = if fast_mode {
            K_FAST_CORRELATION_THRESHOLD
        } else {
            K_CORRELATION_THRESHOLD
        };
        if analysis.best_correlation <= threshold && analysis.active_speech {
            output.extend_from_slice(input);
            return (TimeStretchResult::NoStretch, 0);
        }
        let fs_mult_120 = self.base.fs_mult * K_15MS;
        let mut peak_index = analysis.peak_index;
        if fast_mode {
            // "Fit as many multiples of `peak_index` as possible in
            // fs_mult_120."
            peak_index = (fs_mult_120 / peak_index) * peak_index;
        }
        // Copy 0 to 15 ms, then cross-fade the next `peak_index` samples
        // onto its end, then copy the rest.
        let start = output.len();
        output.extend_from_slice(&input[..fs_mult_120 * ch]);
        cross_fade(
            output,
            start,
            &input[fs_mult_120 * ch..(fs_mult_120 + peak_index) * ch],
            peak_index,
            ch,
        );
        output.extend_from_slice(&input[(fs_mult_120 + peak_index) * ch..]);
        let result = if analysis.active_speech {
            TimeStretchResult::Success
        } else {
            TimeStretchResult::SuccessLowEnergy
        };
        (result, peak_index)
    }
}

/// preemptive_expand.{h,cc} `PreemptiveExpand`: repeats one pitch period.
#[derive(Debug, Clone)]
pub struct PreemptiveExpand {
    base: TimeStretch,
    /// `overlap_samples_`: neteq_impl.cc passes `expand_->overlap_length()`.
    overlap_samples: usize,
}

impl PreemptiveExpand {
    /// Create a preemptive expander for interleaved audio.
    /// `overlap_samples` is the expand overlap length per channel.
    pub fn new(sample_rate: u32, channels: usize, overlap_samples: usize) -> Self {
        Self {
            base: TimeStretch::new(sample_rate, channels),
            overlap_samples,
        }
    }

    /// preemptive_expand.cc `PreemptiveExpand::Process`. `input` is
    /// interleaved and should be 30 ms long; its first `old_data_length`
    /// samples per channel have already been played and must not change.
    /// The stretched audio is appended to `output`. Returns the result and
    /// the number of samples per channel added.
    pub fn process(
        &self,
        input: &[f32],
        old_data_length: usize,
        background_noise: &BackgroundNoise,
        output: &mut Vec<f32>,
    ) -> (TimeStretchResult, usize) {
        let ch = self.base.channels;
        let fs_mult = self.base.fs_mult;
        let len = input.len() / ch;
        // "Input length must be (almost) 30 ms. Also, the new part must be
        // at least `overlap_samples_` elements."
        if !self.base.supported()
            || len < (2 * K_15MS - 1) * fs_mult
            || old_data_length + self.overlap_samples >= len
        {
            output.extend_from_slice(input);
            return (TimeStretchResult::Error, 0);
        }
        let mut analysis = self.base.analyze(input, background_noise);
        if !analysis.active_speech {
            // preemptive_expand.cc SetParametersForPassiveSpeech: the
            // repeated part may not be longer than the new data.
            analysis.peak_index = analysis.peak_index.min(len - old_data_length);
        }
        self.check_criteria_and_stretch(input, old_data_length, analysis, output)
    }

    /// preemptive_expand.cc `PreemptiveExpand::CheckCriteriaAndStretch`.
    fn check_criteria_and_stretch(
        &self,
        input: &[f32],
        old_data_length: usize,
        analysis: Analysis,
        output: &mut Vec<f32>,
    ) -> (TimeStretchResult, usize) {
        let ch = self.base.channels;
        let fs_mult_120 = self.base.fs_mult * K_15MS;
        // "Check for strong correlation (>0.9 in Q14) and at least 15 ms new
        // data, or passive speech."
        let strong =
            analysis.best_correlation > K_CORRELATION_THRESHOLD && old_data_length <= fs_mult_120;
        if !strong && analysis.active_speech {
            output.extend_from_slice(input);
            return (TimeStretchResult::NoStretch, 0);
        }
        let peak_index = analysis.peak_index;
        let unmodified_length = old_data_length.max(fs_mult_120);
        // Copy the first part including the cross-fade region, cross-fade
        // the `peak_index` samples before `unmodified_length` onto its end,
        // then copy the rest from `unmodified_length`.
        let start = output.len();
        output.extend_from_slice(&input[..(unmodified_length + peak_index) * ch]);
        cross_fade(
            output,
            start,
            &input[(unmodified_length - peak_index) * ch..unmodified_length * ch],
            peak_index,
            ch,
        );
        output.extend_from_slice(&input[unmodified_length * ch..]);
        let result = if analysis.active_speech {
            TimeStretchResult::Success
        } else {
            TimeStretchResult::SuccessLowEnergy
        };
        (result, peak_index)
    }
}

/// audio_vector.cc `AudioVector::CrossFade`, per channel on interleaved
/// audio: the last `fade_length` samples of `output[start..]` fade into the
/// first `fade_length` of `append`, and the rest of `append` is appended.
fn cross_fade(output: &mut Vec<f32>, start: usize, append: &[f32], fade_length: usize, ch: usize) {
    let have = (output.len() - start) / ch;
    let append_len = append.len() / ch;
    let fade_length = fade_length.min(have).min(append_len);
    let position = output.len() - fade_length * ch;
    // `alpha` is the mixing factor in Q14.
    let alpha_step = 16384 / (fade_length as i32 + 1);
    let mut alpha = 16384;
    for i in 0..fade_length {
        alpha -= alpha_step;
        let a = alpha as f32 / 16384.0;
        for c in 0..ch {
            let idx = position + i * ch + c;
            output[idx] = a * output[idx] + (1.0 - a) * append[i * ch + c];
        }
    }
    output.extend_from_slice(&append[fade_length * ch..]);
}

/// dsp_helper.cc `DspHelper::DownsampleTo4kHz` with delay compensation,
/// via `WebRtcSpl_DownsampleFast` (downsample_fast.c).
fn downsample_to_4khz(signal: &[f64], sample_rate: u32) -> [f64; K_DOWNSAMPLED_LEN] {
    // Coefficients in Q12, decimation factor and phase delay.
    let (coefficients, factor, delay): (&[i32], usize, usize) = match sample_rate {
        8000 => (&[1229, 1638, 1229], 2, 2),
        16000 => (&[614, 819, 1229, 819, 614], 4, 3),
        32000 => (&[584, 512, 625, 667, 625, 512, 584], 8, 4),
        _ => (&[1019, 390, 427, 440, 427, 390, 1019], 12, 4),
    };
    let mut out = [0.0f64; K_DOWNSAMPLED_LEN];
    // `data_in` starts at `signal[filter_length - 1]` and is read at
    // `data_in[i - j]`, which reaches back to `signal[0]`.
    let offset = coefficients.len() - 1;
    for (k, o) in out.iter_mut().enumerate() {
        let i = offset + delay + factor * k;
        let acc: f64 = coefficients
            .iter()
            .enumerate()
            .map(|(j, &c)| f64::from(c) * signal[i - j])
            .sum();
        *o = acc / 4096.0;
    }
    out
}

/// time_stretch.cc `TimeStretch::AutoCorrelation`: correlation of the
/// last `kCorrelationLen` downsampled samples with lags `kMinLag` to
/// `kMaxLag - 1`, scaled to 16 bits as `WebRtcSpl_VectorBitShiftW32ToW16`
/// does.
fn auto_correlation(downsampled: &[f64; K_DOWNSAMPLED_LEN]) -> [i32; K_CORRELATION_LEN] {
    let mut corr = [0.0f64; K_CORRELATION_LEN];
    for (i, c) in corr.iter_mut().enumerate() {
        let lagged = &downsampled[K_MAX_LAG - K_MIN_LAG - i..];
        *c = downsampled[K_MAX_LAG..]
            .iter()
            .zip(lagged)
            .take(K_CORRELATION_LEN)
            .map(|(a, b)| a * b)
            .sum();
    }
    let max_abs = corr.iter().fold(0.0f64, |m, c| m.max(c.abs()));
    // scaling = max(0, 17 - NormW32(max)): the largest value keeps 14 bits.
    let scaling = if max_abs >= 1.0 {
        (max_abs.log2().floor() as i32 - 13).max(0)
    } else {
        0
    };
    let divisor = 2f64.powi(scaling);
    let mut out = [0i32; K_CORRELATION_LEN];
    for (o, c) in out.iter_mut().zip(corr) {
        *o = (c / divisor).floor() as i32;
    }
    out
}

/// dsp_helper.cc `DspHelper::PeakDetection` with one peak: index of the
/// largest value, refined to the full sample rate.
fn peak_detection(data: &[i32; K_CORRELATION_LEN], fs_mult: usize) -> usize {
    // WebRtcSpl_MaxIndexW16: first index of the maximum.
    let mut peak = 0;
    for (i, &v) in data.iter().enumerate() {
        if v > data[peak] {
            peak = i;
        }
    }
    let edge_index = K_CORRELATION_LEN - 1;
    // dsp_helper.cc RefinePeak.
    if peak == 0 {
        0
    } else if peak == edge_index {
        // A single peak on the last sample: the sample past it is zero.
        let next = 0;
        if data[peak] > next {
            parabolic_fit([data[peak - 1], data[peak], next], fs_mult, peak)
        } else {
            (peak * 2 + 1) * fs_mult
        }
    } else {
        parabolic_fit([data[peak - 1], data[peak], data[peak + 1]], fs_mult, peak)
    }
}

/// dsp_helper.cc `DspHelper::ParabolicFit`, integer arithmetic as in
/// libwebrtc. Returns the refined peak index at the full sample rate.
fn parabolic_fit(points: [i32; 3], fs_mult: usize, peak_index: usize) -> usize {
    let fit_index: &[usize] = match fs_mult {
        1 => &[0, 8, 16],
        2 => &[0, 4, 8, 12, 16],
        4 => &[0, 2, 4, 6, 8, 10, 12, 14, 16],
        _ => &[0, 1, 3, 4, 5, 7, 8, 9, 11, 12, 13, 15, 16],
    };
    let coefficient = |i: usize| K_PARABOLA_COEFFICIENTS[fit_index[i]][0] as i64;
    let num = i64::from(points[0]) * -3 + i64::from(points[1]) * 4 - i64::from(points[2]);
    let den = i64::from(points[0]) + i64::from(points[1]) * -2 + i64::from(points[2]);
    let temp = num * 120;
    let stp = coefficient(fs_mult) - coefficient(fs_mult - 1);
    let strt = (coefficient(fs_mult) + coefficient(fs_mult - 1)) / 2;
    let fs = fs_mult as i64;
    let mut flag: i64 = 1;
    if temp < -den * strt {
        let mut lmt = strt - stp;
        loop {
            if flag == fs || temp > -den * lmt {
                return (peak_index as i64 * 2 * fs - flag) as usize;
            }
            flag += 1;
            lmt -= stp;
        }
    } else if temp > -den * (strt + stp) {
        let mut lmt = strt + 2 * stp;
        loop {
            if flag == fs || temp < -den * lmt {
                return (peak_index as i64 * 2 * fs + flag) as usize;
            }
            flag += 1;
            lmt += stp;
        }
    }
    peak_index * 2 * fs_mult
}

/// time_stretch.cc `TimeStretch::SpeechDetection`: active speech when the
/// mean energy of the two periods is more than eight times the background
/// noise energy.
fn speech_detection(
    vec1_energy: f64,
    vec2_energy: f64,
    peak_index: usize,
    background_noise: &BackgroundNoise,
) -> bool {
    let background = if background_noise.initialized() {
        background_noise.energy()
    } else {
        K_DEFAULT_BACKGROUND_ENERGY
    };
    (vec1_energy + vec2_energy) / 16.0 > peak_index as f64 * background
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48000;
    const FS_MULT: usize = 6;
    const MS30: usize = 1440;
    const MS15: usize = 720;

    /// A 200 Hz harmonic tone (period 240 samples at 48 kHz) at `amplitude`.
    fn voiced(len: usize, amplitude: f32) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let w = 2.0 * std::f32::consts::PI * 200.0 * t;
                amplitude * (0.6 * w.sin() + 0.3 * (2.0 * w).sin() + 0.1 * (3.0 * w).sin())
            })
            .collect()
    }

    fn noise(len: usize, amplitude: f32, seed: u32) -> Vec<f32> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amplitude
            })
            .collect()
    }

    fn near_multiple_of(value: usize, period: usize, tolerance: usize) -> bool {
        let r = value % period;
        r <= tolerance || period - r <= tolerance
    }

    #[test]
    fn passive_signal_always_expands_by_one_period() {
        let pe = PreemptiveExpand::new(RATE, 1, 5 * FS_MULT);
        // -60 dBFS noise: far below the default background energy.
        let input = noise(MS30, 0.001, 7);
        let mut output = Vec::new();
        let (result, added) = pe.process(&input, 0, &BackgroundNoise::new(), &mut output);
        assert_eq!(result, TimeStretchResult::SuccessLowEnergy);
        assert!((20 * FS_MULT..120 * FS_MULT).contains(&added), "{added}");
        assert_eq!(output.len(), input.len() + added);
    }

    #[test]
    fn passive_signal_always_accelerates_by_one_period() {
        let accelerate = Accelerate::new(RATE, 1);
        let input = noise(MS30, 0.001, 9);
        let mut output = Vec::new();
        let (result, removed) =
            accelerate.process(&input, false, &BackgroundNoise::new(), &mut output);
        assert_eq!(result, TimeStretchResult::SuccessLowEnergy);
        assert!(
            (20 * FS_MULT..120 * FS_MULT).contains(&removed),
            "{removed}"
        );
        assert_eq!(output.len(), input.len() - removed);
    }

    #[test]
    fn correlated_speech_is_expanded_by_its_pitch_period() {
        let pe = PreemptiveExpand::new(RATE, 1, 5 * FS_MULT);
        let input = voiced(MS30, 0.5);
        let mut output = Vec::new();
        let (result, added) = pe.process(&input, 0, &BackgroundNoise::new(), &mut output);
        assert_eq!(result, TimeStretchResult::Success);
        assert!(near_multiple_of(added, 240, 3), "{added}");
        assert_eq!(output.len(), input.len() + added);
        // Unmodified first 15 ms and an unmodified tail.
        assert_eq!(output[..MS15], input[..MS15]);
        assert_eq!(output[MS15 + added..], input[MS15..]);
    }

    #[test]
    fn correlated_speech_is_accelerated_by_its_pitch_period() {
        let accelerate = Accelerate::new(RATE, 1);
        let input = voiced(MS30, 0.5);
        let mut output = Vec::new();
        let (result, removed) =
            accelerate.process(&input, false, &BackgroundNoise::new(), &mut output);
        assert_eq!(result, TimeStretchResult::Success);
        assert!(near_multiple_of(removed, 240, 3), "{removed}");
        assert_eq!(output.len(), input.len() - removed);
        assert_eq!(output[..MS15 - removed], input[..MS15 - removed]);
        assert_eq!(output[MS15..], input[MS15 + removed..]);
    }

    #[test]
    fn uncorrelated_loud_audio_is_not_stretched() {
        let input = noise(MS30, 0.5, 3);
        let bgn = BackgroundNoise::new();

        let mut output = Vec::new();
        let pe = PreemptiveExpand::new(RATE, 1, 5 * FS_MULT);
        assert_eq!(
            pe.process(&input, 0, &bgn, &mut output),
            (TimeStretchResult::NoStretch, 0)
        );
        assert_eq!(output, input);

        let mut output = Vec::new();
        let accelerate = Accelerate::new(RATE, 1);
        assert_eq!(
            accelerate.process(&input, false, &bgn, &mut output),
            (TimeStretchResult::NoStretch, 0)
        );
        assert_eq!(output, input);
    }

    #[test]
    fn fast_accelerate_removes_whole_periods_up_to_15ms() {
        let accelerate = Accelerate::new(RATE, 1);
        let input = voiced(MS30, 0.5);
        let mut output = Vec::new();
        let (result, removed) =
            accelerate.process(&input, true, &BackgroundNoise::new(), &mut output);
        assert_eq!(result, TimeStretchResult::Success);
        assert!(removed <= MS15 && removed > MS15 / 2, "{removed}");
        assert_eq!(output.len(), input.len() - removed);
    }

    #[test]
    fn short_input_is_an_error_and_passes_through() {
        let input = voiced(MS30 / 2, 0.5);
        let bgn = BackgroundNoise::new();
        let mut output = Vec::new();
        let pe = PreemptiveExpand::new(RATE, 1, 5 * FS_MULT);
        assert_eq!(
            pe.process(&input, 0, &bgn, &mut output),
            (TimeStretchResult::Error, 0)
        );
        assert_eq!(output, input);
        let mut output = Vec::new();
        let accelerate = Accelerate::new(RATE, 1);
        assert_eq!(
            accelerate.process(&input, false, &bgn, &mut output),
            (TimeStretchResult::Error, 0)
        );
        assert_eq!(output, input);
    }

    #[test]
    fn expand_needs_15ms_of_new_data_for_active_speech() {
        let pe = PreemptiveExpand::new(RATE, 1, 5 * FS_MULT);
        let input = voiced(MS30, 0.5);
        let mut output = Vec::new();
        let result = pe.process(&input, MS15 + 1, &BackgroundNoise::new(), &mut output);
        assert_eq!(result, (TimeStretchResult::NoStretch, 0));
        assert_eq!(output, input);
    }

    #[test]
    fn expand_never_changes_already_played_samples() {
        let pe = PreemptiveExpand::new(RATE, 1, 5 * FS_MULT);
        let input = noise(MS30, 0.001, 11);
        let old = MS15 + 300;
        let mut output = Vec::new();
        let (result, added) = pe.process(&input, old, &BackgroundNoise::new(), &mut output);
        assert_eq!(result, TimeStretchResult::SuccessLowEnergy);
        assert!(added <= MS30 - old);
        assert_eq!(output[..old], input[..old]);
    }

    #[test]
    fn stereo_changes_every_channel_by_the_same_length() {
        let pe = PreemptiveExpand::new(RATE, 2, 5 * FS_MULT);
        let mono = voiced(MS30, 0.5);
        let input: Vec<f32> = mono.iter().flat_map(|&s| [s, -s]).collect();
        let mut output = Vec::new();
        let (result, added) = pe.process(&input, 0, &BackgroundNoise::new(), &mut output);
        assert_eq!(result, TimeStretchResult::Success);
        assert_eq!(output.len(), input.len() + 2 * added);
        assert!(output.as_chunks::<2>().0.iter().all(|f| f[0] == -f[1]));
    }

    #[test]
    fn a_learned_background_level_makes_quiet_speech_active() {
        // Quiet voiced audio at about -50 dBFS is below the default
        // background energy, so it counts as passive; once a lower noise
        // level is learned it is active speech.
        let input = voiced(MS30, 0.004);
        let pe = PreemptiveExpand::new(RATE, 1, 5 * FS_MULT);
        let mut output = Vec::new();
        let (result, _) = pe.process(&input, 0, &BackgroundNoise::new(), &mut output);
        assert_eq!(result, TimeStretchResult::SuccessLowEnergy);

        let mut bgn = BackgroundNoise::new();
        assert!(bgn.update(&noise(256, 0.0002, 5)));
        let mut output = Vec::new();
        let (result, _) = pe.process(&input, 0, &bgn, &mut output);
        assert_eq!(result, TimeStretchResult::Success);
    }

    #[test]
    fn parabolic_fit_matches_libwebrtc_on_a_symmetric_peak() {
        // A symmetric peak stays on its integer index.
        assert_eq!(parabolic_fit([100, 200, 100], 6, 10), 10 * 12);
        // A peak leaning right moves right by up to half a 4 kHz sample.
        let right = parabolic_fit([100, 200, 190], 6, 10);
        assert!(right > 120 && right <= 126, "{right}");
        let left = parabolic_fit([190, 200, 100], 6, 10);
        assert!((114..120).contains(&left), "{left}");
    }
}

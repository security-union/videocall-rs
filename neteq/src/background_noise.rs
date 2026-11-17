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

//! Background-noise energy estimate, ported from libwebrtc
//! `modules/audio_coding/neteq/background_noise.{h,cc}` (commit aeb9b375).
//!
//! Only the energy part is ported: time stretching uses it to tell active
//! speech from background noise (`TimeStretch::SpeechDetection`). The LPC
//! filter is still computed, because libwebrtc only accepts a noise estimate
//! whose spectrum is flat enough, but it is not used to generate noise.
//!
//! Energies are in libwebrtc's units: mean square per sample of 16-bit
//! samples, so the constants carry over unchanged. Arithmetic is `f64`
//! instead of fixed point.

/// background_noise.h `kMaxLpcOrder`.
const K_MAX_LPC_ORDER: usize = 8;
/// background_noise.h `kVecLen`.
pub const K_VEC_LEN: usize = 256;
/// background_noise.h `kResidualLength`.
const K_RESIDUAL_LENGTH: usize = 64;
/// background_noise.h `kThresholdIncrement`: 229 in Q16 (0.0035).
const K_THRESHOLD_INCREMENT: f64 = 229.0 / 65536.0;
/// levinson_durbin.c: a reflection coefficient above 32750 in Q15 is
/// unstable.
const K_MAX_REFLECTION: f64 = 32750.0 / 32768.0;

/// Background-noise energy for the reference channel.
#[derive(Debug, Clone)]
pub struct BackgroundNoise {
    initialized: bool,
    energy: f64,
    max_energy: f64,
    energy_update_threshold: f64,
}

impl Default for BackgroundNoise {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundNoise {
    /// A fresh, uninitialized estimate.
    pub fn new() -> Self {
        let mut bgn = Self {
            initialized: false,
            energy: 0.0,
            max_energy: 0.0,
            energy_update_threshold: 0.0,
        };
        bgn.reset();
        bgn
    }

    /// background_noise.cc `BackgroundNoise::Reset` and
    /// `ChannelParameters::Reset`.
    pub fn reset(&mut self) {
        self.initialized = false;
        self.energy = 2500.0;
        self.max_energy = 0.0;
        self.energy_update_threshold = 500_000.0;
    }

    /// Whether a noise estimate has been saved (`initialized()`).
    pub fn initialized(&self) -> bool {
        self.initialized
    }

    /// Energy per sample of the noise estimate, 16-bit units (`Energy`).
    pub fn energy(&self) -> f64 {
        self.energy
    }

    /// background_noise.cc `BackgroundNoise::Update`, for one channel.
    /// `signal` is the newest audio of the reference channel in `[-1, 1]`;
    /// the last `K_VEC_LEN` samples are used. Returns whether a new
    /// estimate was saved.
    pub fn update(&mut self, signal: &[f32]) -> bool {
        // libwebrtc's 16-bit samples are always finite; ours may not be, and
        // one non-finite sample would stick in `max_energy` for good.
        if signal.len() < K_VEC_LEN
            || !signal[signal.len() - K_VEC_LEN..]
                .iter()
                .all(|s| s.is_finite())
        {
            return false;
        }
        let x: Vec<f64> = signal[signal.len() - K_VEC_LEN..]
            .iter()
            .map(|&s| f64::from(s) * 32768.0)
            .collect();
        // CalculateAutoCorrelation: lags 0..=kMaxLpcOrder, energy per sample.
        let mut r = [0.0f64; K_MAX_LPC_ORDER + 1];
        for (lag, r_lag) in r.iter_mut().enumerate() {
            *r_lag = x[lag..].iter().zip(&x).map(|(a, b)| a * b).sum();
        }
        let sample_energy = (r[0] / K_VEC_LEN as f64).floor();

        if sample_energy >= self.energy_update_threshold {
            self.increment_energy_threshold(sample_energy);
            return false;
        }
        if r[0] <= 0.0 {
            return false;
        }
        // "Regardless of whether the filter is actually updated or not,
        // update energy threshold levels."
        self.energy_update_threshold = sample_energy.max(1.0);
        let Some(lpc) = levinson_durbin(&r) else {
            return false;
        };
        // Residual energy over the last kResidualLength samples.
        let start = K_VEC_LEN - K_RESIDUAL_LENGTH;
        let residual_energy: f64 = (start..K_VEC_LEN)
            .map(|n| {
                let e: f64 = lpc
                    .iter()
                    .enumerate()
                    .map(|(j, a)| a * if n >= j { x[n - j] } else { 0.0 })
                    .sum();
                e * e
            })
            .sum();
        // "If 5 * residual_energy >= 16 * sample_energy, the spectrum is flat
        // enough."
        if sample_energy > 0.0 && 5.0 * residual_energy >= 16.0 * sample_energy {
            // SaveParameters
            self.energy = sample_energy.max(1.0);
            self.energy_update_threshold = self.energy;
            self.initialized = true;
            return true;
        }
        false
    }

    /// background_noise.cc `IncrementEnergyThreshold`: the threshold grows by
    /// `kThresholdIncrement` and never falls 60 dB below the decaying
    /// maximum energy.
    fn increment_energy_threshold(&mut self, sample_energy: f64) {
        self.energy_update_threshold += K_THRESHOLD_INCREMENT * self.energy_update_threshold;
        self.max_energy -= (self.max_energy / 1024.0).floor();
        if sample_energy > self.max_energy {
            self.max_energy = sample_energy;
        }
        let floor = ((self.max_energy + 524_288.0) / 1_048_576.0).floor();
        if floor > self.energy_update_threshold {
            self.energy_update_threshold = floor;
        }
    }
}

/// levinson_durbin.c `WebRtcSpl_LevinsonDurbin` in floating point. Returns
/// the prediction filter `A(z)` with `a[0] = 1`, or `None` when a
/// reflection coefficient makes it unstable.
fn levinson_durbin(r: &[f64; K_MAX_LPC_ORDER + 1]) -> Option<[f64; K_MAX_LPC_ORDER + 1]> {
    let mut a = [0.0f64; K_MAX_LPC_ORDER + 1];
    a[0] = 1.0;
    let mut error = r[0];
    for i in 1..=K_MAX_LPC_ORDER {
        let acc: f64 = (1..i).map(|j| a[j] * r[i - j]).sum::<f64>() + r[i];
        let k = -acc / error;
        if k.abs() > K_MAX_REFLECTION || !k.is_finite() {
            return None;
        }
        let previous = a;
        for j in 1..i {
            a[j] = previous[j] + k * previous[i - j];
        }
        a[i] = k;
        error *= 1.0 - k * k;
        if error <= 0.0 {
            return None;
        }
    }
    Some(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(len: usize, amplitude: f32, seed: &mut u32) -> Vec<f32> {
        (0..len)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((*seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amplitude
            })
            .collect()
    }

    #[test]
    fn quiet_white_noise_becomes_the_estimate() {
        let mut bgn = BackgroundNoise::new();
        let mut seed = 1;
        // Amplitude 0.01 of uniform noise: mean square (0.01*32768)^2/3.
        let signal = noise(K_VEC_LEN, 0.01, &mut seed);
        assert!(bgn.update(&signal));
        assert!(bgn.initialized());
        let expected = (0.01f64 * 32768.0).powi(2) / 3.0;
        assert!(
            (bgn.energy() / expected - 1.0).abs() < 0.3,
            "{} vs {expected}",
            bgn.energy()
        );
    }

    #[test]
    fn a_non_finite_sample_is_ignored() {
        let mut bgn = BackgroundNoise::new();
        let mut seed = 1;
        let mut signal = noise(K_VEC_LEN, 0.9, &mut seed);
        signal[10] = f32::INFINITY;
        assert!(!bgn.update(&signal));
        assert_eq!(bgn.max_energy, 0.0);
        assert_eq!(bgn.energy_update_threshold, 500_000.0);
        // Quiet noise is still learned afterwards.
        assert!(bgn.update(&noise(K_VEC_LEN, 0.01, &mut seed)));
    }

    #[test]
    fn loud_signal_is_not_background_noise() {
        let mut bgn = BackgroundNoise::new();
        let mut seed = 1;
        // Mean square far above the 500000 initial threshold.
        let signal = noise(K_VEC_LEN, 0.9, &mut seed);
        assert!(!bgn.update(&signal));
        assert!(!bgn.initialized());
        assert_eq!(bgn.energy(), 2500.0);
    }

    #[test]
    fn a_pure_tone_is_not_flat_enough() {
        let mut bgn = BackgroundNoise::new();
        let signal: Vec<f32> = (0..K_VEC_LEN)
            .map(|i| 0.01 * (i as f32 * 0.3).sin())
            .collect();
        assert!(!bgn.update(&signal));
        assert!(!bgn.initialized());
    }
}

// Port target: WebRTC modules/audio_coding/neteq/dtmf_tone_generator.{h,cc}
// at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd
// (tests: dtmf_tone_generator_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Two-oscillator DTMF tone synthesis.

use crate::neteq::audio_multi_vector::AudioMultiVector;

/// Filter coefficient a = 2*cos(2*pi*f/fs) for the low frequency tone, for
/// fs = {8000, 16000, 32000, 48000} Hz and events 0 through 15. Q14.
const K_COEFF1: [[i32; 16]; 4] = [
    [
        24219, 27980, 27980, 27980, 26956, 26956, 26956, 25701, 25701, 25701, 24219, 24219, 27980,
        26956, 25701, 24219,
    ],
    [
        30556, 31548, 31548, 31548, 31281, 31281, 31281, 30951, 30951, 30951, 30556, 30556, 31548,
        31281, 30951, 30556,
    ],
    [
        32210, 32462, 32462, 32462, 32394, 32394, 32394, 32311, 32311, 32311, 32210, 32210, 32462,
        32394, 32311, 32210,
    ],
    [
        32520, 32632, 32632, 32632, 32602, 32602, 32602, 32564, 32564, 32564, 32520, 32520, 32632,
        32602, 32564, 32520,
    ],
];

/// Filter coefficient a = 2*cos(2*pi*f/fs) for the high frequency tone, for
/// fs = {8000, 16000, 32000, 48000} Hz and events 0 through 15. Q14.
const K_COEFF2: [[i32; 16]; 4] = [
    [
        16325, 19073, 16325, 13085, 19073, 16325, 13085, 19073, 16325, 13085, 19073, 13085, 9315,
        9315, 9315, 9315,
    ],
    [
        28361, 29144, 28361, 27409, 29144, 28361, 27409, 29144, 28361, 27409, 29144, 27409, 26258,
        26258, 26258, 26258,
    ],
    [
        31647, 31849, 31647, 31400, 31849, 31647, 31400, 31849, 31647, 31400, 31849, 31400, 31098,
        31098, 31098, 31098,
    ],
    [
        32268, 32359, 32268, 32157, 32359, 32268, 32157, 32359, 32268, 32157, 32359, 32157, 32022,
        32022, 32022, 32022,
    ],
];

/// Initialization value x[-2] = sin(2*pi*f/fs) for the low frequency tone,
/// for fs = {8000, 16000, 32000, 48000} Hz and events 0-15. Q14.
const K_INIT_VALUE1: [[i32; 16]; 4] = [
    [
        11036, 8528, 8528, 8528, 9315, 9315, 9315, 10163, 10163, 10163, 11036, 11036, 8528, 9315,
        10163, 11036,
    ],
    [
        5918, 4429, 4429, 4429, 4879, 4879, 4879, 5380, 5380, 5380, 5918, 5918, 4429, 4879, 5380,
        5918,
    ],
    [
        3010, 2235, 2235, 2235, 2468, 2468, 2468, 2728, 2728, 2728, 3010, 3010, 2235, 2468, 2728,
        3010,
    ],
    [
        2013, 1493, 1493, 1493, 1649, 1649, 1649, 1823, 1823, 1823, 2013, 2013, 1493, 1649, 1823,
        2013,
    ],
];

/// Initialization value x[-2] = sin(2*pi*f/fs) for the high frequency tone,
/// for fs = {8000, 16000, 32000, 48000} Hz and events 0-15. Q14.
const K_INIT_VALUE2: [[i32; 16]; 4] = [
    [
        14206, 13323, 14206, 15021, 13323, 14206, 15021, 13323, 14206, 15021, 13323, 15021, 15708,
        15708, 15708, 15708,
    ],
    [
        8207, 7490, 8207, 8979, 7490, 8207, 8979, 7490, 8207, 8979, 7490, 8979, 9801, 9801, 9801,
        9801,
    ],
    [
        4249, 3853, 4249, 4685, 3853, 4249, 4685, 3853, 4249, 4685, 3853, 4685, 5164, 5164, 5164,
        5164,
    ],
    [
        2851, 2582, 2851, 3148, 2582, 2851, 3148, 2582, 2851, 3148, 2582, 3148, 3476, 3476, 3476,
        3476,
    ],
];

/// Amplitude multipliers for volume values 0 through 63 (0 dBm0 through
/// -63 dBm0), Q14: `round(16141.0 * 10**(-a/20))`.
const K_AMPLITUDE: [i32; 64] = [
    16141, 14386, 12821, 11427, 10184, 9077, 8090, 7210, 6426, 5727, 5104, 4549, 4054, 3614, 3221,
    2870, 2558, 2280, 2032, 1811, 1614, 1439, 1282, 1143, 1018, 908, 809, 721, 643, 573, 510, 455,
    405, 361, 322, 287, 256, 228, 203, 181, 161, 144, 128, 114, 102, 91, 81, 72, 64, 57, 51, 45,
    41, 36, 32, 29, 26, 23, 20, 18, 16, 14, 13, 11,
];

/// C++ `kAmpMultiplier`: 3 dB attenuation, Q15.
const K_AMP_MULTIPLIER: i32 = 23171;

/// C++ `DtmfToneGenerator`.
#[derive(Debug, Clone, Default)]
pub struct DtmfToneGenerator {
    initialized: bool,
    coeff1: i32,
    coeff2: i32,
    amplitude: i32,
    sample_history1: [i16; 2],
    sample_history2: [i16; 2],
}

impl DtmfToneGenerator {
    /// C++ `kNotInitialized`.
    pub const K_NOT_INITIALIZED: i32 = -1;
    /// C++ `kParameterError`.
    pub const K_PARAMETER_ERROR: i32 = -2;

    /// An uninitialized generator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Prepares to play `event` at `fs` with `attenuation` dB. Returns 0 or
    /// a `K_*` error.
    pub fn init(&mut self, fs: i32, event: i32, attenuation: i32) -> i32 {
        self.initialized = false;
        let fs_index = match fs {
            8000 => 0,
            16000 => 1,
            32000 => 2,
            48000 => 3,
            _ => {
                debug_assert!(false, "unsupported sample rate {fs}");
                1 // Default to 8000 Hz (sic: index 1 is 16 kHz in C++ too).
            }
        };

        if !(0..=15).contains(&event) {
            return Self::K_PARAMETER_ERROR; // Invalid event number.
        }

        if !(0..=63).contains(&attenuation) {
            return Self::K_PARAMETER_ERROR; // Invalid attenuation.
        }

        // Look up oscillator coefficient for low and high frequencies.
        let event = event as usize;
        self.coeff1 = K_COEFF1[fs_index][event];
        self.coeff2 = K_COEFF2[fs_index][event];

        // Look up amplitude multiplier.
        self.amplitude = K_AMPLITUDE[attenuation as usize];

        // Initialize sample history.
        self.sample_history1[0] = K_INIT_VALUE1[fs_index][event] as i16;
        self.sample_history1[1] = 0;
        self.sample_history2[0] = K_INIT_VALUE2[fs_index][event] as i16;
        self.sample_history2[1] = 0;

        self.initialized = true;
        0
    }

    /// Back to uninitialized.
    pub fn reset(&mut self) {
        self.initialized = false;
    }

    /// Writes `num_samples` per channel of tone into `output`. Returns the
    /// number of samples written, or a `K_*` error.
    pub fn generate(&mut self, num_samples: usize, output: &mut AudioMultiVector) -> i32 {
        if !self.initialized {
            return Self::K_NOT_INITIALIZED;
        }

        output.assert_size(num_samples);
        for i in 0..num_samples {
            // Use recursion formula y[n] = a * y[n - 1] - y[n - 2].
            let temp_val_low = (((self.coeff1 * self.sample_history1[1] as i32 + 8192) >> 14)
                - self.sample_history1[0] as i32) as i16;
            let temp_val_high = (((self.coeff2 * self.sample_history2[1] as i32 + 8192) >> 14)
                - self.sample_history2[0] as i32) as i16;

            // Update recursion memory.
            self.sample_history1[0] = self.sample_history1[1];
            self.sample_history1[1] = temp_val_low;
            self.sample_history2[0] = self.sample_history2[1];
            self.sample_history2[1] = temp_val_high;

            // Attenuate the low frequency tone 3 dB.
            let mut temp_val =
                K_AMP_MULTIPLIER * temp_val_low as i32 + temp_val_high as i32 * (1 << 15);
            // Normalize the signal to Q14 with proper rounding.
            temp_val = (temp_val + 16384) >> 15;
            // Scale the signal to correct volume.
            output[0][i] = ((temp_val * self.amplitude + 8192) >> 14) as i16;
        }
        // Copy first channel to all other channels.
        for channel in 1..output.channels() {
            output.copy_channel(0, channel);
        }

        num_samples as i32
    }

    /// True after a successful `init`.
    pub fn initialized(&self) -> bool {
        self.initialized
    }
}

#[cfg(test)]
mod tests {
    //! Port of dtmf_tone_generator_unittest.cc.

    use super::*;

    /// Low and high frequencies for events 0 through 15.
    const K_LOW_FREQ_HZ: [f64; 16] = [
        941.0, 697.0, 697.0, 697.0, 770.0, 770.0, 770.0, 852.0, 852.0, 852.0, 941.0, 941.0, 697.0,
        770.0, 852.0, 941.0,
    ];
    const K_HIGH_FREQ_HZ: [f64; 16] = [
        1336.0, 1209.0, 1336.0, 1477.0, 1209.0, 1336.0, 1477.0, 1209.0, 1336.0, 1477.0, 1209.0,
        1477.0, 1633.0, 1633.0, 1633.0, 1633.0,
    ];
    /// The attenuation applied to all cases.
    const K_BASE_ATTENUATION: f64 = 16141.0 / 16384.0;
    const K_3DB_ATTENUATION: f64 = 23171.0 / 32768.0;
    const K_NUM_SAMPLES: usize = 10;

    /// C++ `DbToRatio(float)`.
    fn db_to_ratio(db: f32) -> f64 {
        10f32.powf(db / 20.0) as f64
    }

    fn test_all_tones(fs_hz: i32, channels: usize) {
        let mut tone_gen = DtmfToneGenerator::new();
        let mut signal = AudioMultiVector::new(channels);

        for event in 0..=15 {
            const K_ATTENUATION: i32 = 0;
            assert_eq!(0, tone_gen.init(fs_hz, event, K_ATTENUATION));
            assert!(tone_gen.initialized());
            assert_eq!(
                K_NUM_SAMPLES as i32,
                tone_gen.generate(K_NUM_SAMPLES, &mut signal)
            );

            let f1 = K_LOW_FREQ_HZ[event as usize];
            let f2 = K_HIGH_FREQ_HZ[event as usize];
            let pi = std::f64::consts::PI;

            for n in 0..K_NUM_SAMPLES as i32 {
                let mut x = K_3DB_ATTENUATION
                    * (2.0 * pi * f1 / fs_hz as f64 * (-n - 1) as f64).sin()
                    + (2.0 * pi * f2 / fs_hz as f64 * (-n - 1) as f64).sin();
                x *= K_BASE_ATTENUATION;
                x *= (1 << 14) as f64; // Scale to Q14.
                for channel in 0..channels {
                    let actual = signal[channel][n as usize] as f64;
                    assert!(
                        (x - actual).abs() <= 25.0,
                        "event {event} at {fs_hz} Hz, sample {n}, channel {channel}: \
                         expected {x}, got {actual}"
                    );
                }
            }

            tone_gen.reset();
            assert!(!tone_gen.initialized());
        }
    }

    fn test_amplitudes(fs_hz: i32, channels: usize) {
        let mut tone_gen = DtmfToneGenerator::new();
        let mut signal = AudioMultiVector::new(channels);
        let mut ref_signal = AudioMultiVector::new(channels);

        // Test a few events.
        for event in [0, 4, 9, 13] {
            // Create full-scale reference.
            assert_eq!(0, tone_gen.init(fs_hz, event, 0)); // 0 attenuation.
            assert_eq!(
                K_NUM_SAMPLES as i32,
                tone_gen.generate(K_NUM_SAMPLES, &mut ref_signal)
            );
            // Test every 5 steps (to save time).
            for attenuation in (1..=63).step_by(5) {
                assert_eq!(0, tone_gen.init(fs_hz, event, attenuation));
                assert_eq!(
                    K_NUM_SAMPLES as i32,
                    tone_gen.generate(K_NUM_SAMPLES, &mut signal)
                );
                for n in 0..K_NUM_SAMPLES {
                    let attenuation_factor = db_to_ratio(-(attenuation as f32));
                    // Verify that the attenuation is correct.
                    for channel in 0..channels {
                        let expected = attenuation_factor * ref_signal[channel][n] as f64;
                        let actual = signal[channel][n] as f64;
                        assert!(
                            (expected - actual).abs() <= 2.0,
                            "event {event} at {fs_hz} Hz, attenuation {attenuation}: \
                             expected {expected}, got {actual}"
                        );
                    }
                }

                tone_gen.reset();
            }
        }
    }

    #[test]
    fn test_8000_mono() {
        test_all_tones(8000, 1);
        test_amplitudes(8000, 1);
    }

    #[test]
    fn test_16000_mono() {
        test_all_tones(16000, 1);
        test_amplitudes(16000, 1);
    }

    #[test]
    fn test_32000_mono() {
        test_all_tones(32000, 1);
        test_amplitudes(32000, 1);
    }

    #[test]
    fn test_48000_mono() {
        test_all_tones(48000, 1);
        test_amplitudes(48000, 1);
    }

    #[test]
    fn test_8000_stereo() {
        test_all_tones(8000, 2);
        test_amplitudes(8000, 2);
    }

    #[test]
    fn test_16000_stereo() {
        test_all_tones(16000, 2);
        test_amplitudes(16000, 2);
    }

    #[test]
    fn test_32000_stereo() {
        test_all_tones(32000, 2);
        test_amplitudes(32000, 2);
    }

    #[test]
    fn test_48000_stereo() {
        test_all_tones(48000, 2);
        test_amplitudes(48000, 2);
    }

    #[test]
    fn test_errors() {
        let mut tone_gen = DtmfToneGenerator::new();
        const K_NUM_SAMPLES: usize = 10;
        let mut signal = AudioMultiVector::new(1); // One channel.

        // Try to generate tones without initializing.
        assert_eq!(
            DtmfToneGenerator::K_NOT_INITIALIZED,
            tone_gen.generate(K_NUM_SAMPLES, &mut signal)
        );

        const FS: i32 = 16000; // Valid sample rate.
        const EVENT: i32 = 7; // Valid event.
        const ATTENUATION: i32 = 0; // Valid attenuation.
                                    // Initialize with invalid event -1.
        assert_eq!(
            DtmfToneGenerator::K_PARAMETER_ERROR,
            tone_gen.init(FS, -1, ATTENUATION)
        );
        // Initialize with invalid event 16.
        assert_eq!(
            DtmfToneGenerator::K_PARAMETER_ERROR,
            tone_gen.init(FS, 16, ATTENUATION)
        );
        // Initialize with invalid attenuation -1.
        assert_eq!(
            DtmfToneGenerator::K_PARAMETER_ERROR,
            tone_gen.init(FS, EVENT, -1)
        );
        // Initialize with invalid attenuation 64.
        assert_eq!(
            DtmfToneGenerator::K_PARAMETER_ERROR,
            tone_gen.init(FS, EVENT, 64)
        );
        assert!(!tone_gen.initialized()); // Should still be uninitialized.

        // Initialize with valid parameters.
        assert_eq!(0, tone_gen.init(FS, EVENT, ATTENUATION));
        assert!(tone_gen.initialized());
        // The C++ null-output case (`kParameterError`) cannot be expressed
        // with `&mut AudioMultiVector`.
    }

    /// `generate` grows the output to `num_samples` and continues the
    /// oscillator across calls.
    #[test]
    fn generate_sizes_output_and_continues_phase() {
        let mut tone_gen = DtmfToneGenerator::new();
        let mut once = AudioMultiVector::new(1);
        assert_eq!(0, tone_gen.init(8000, 5, 10));
        assert_eq!(20, tone_gen.generate(20, &mut once));
        assert_eq!(20, once.size());

        let mut split = AudioMultiVector::new(1);
        assert_eq!(0, tone_gen.init(8000, 5, 10));
        assert_eq!(10, tone_gen.generate(10, &mut split));
        let first: Vec<i16> = (0..10).map(|i| split[0][i]).collect();
        assert_eq!(10, tone_gen.generate(10, &mut split));
        let second: Vec<i16> = (0..10).map(|i| split[0][i]).collect();
        let expected: Vec<i16> = (0..20).map(|i| once[0][i]).collect();
        assert_eq!(expected[..10], first[..]);
        assert_eq!(expected[10..], second[..]);
    }
}

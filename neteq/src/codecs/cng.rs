// Port target: WebRTC modules/audio_coding/codecs/cng/webrtc_cng.{h,cc} at
// rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: cng_unittest.cc). The
// encoder exists for tests only.
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// Integer conversions follow C: every store into an `int16_t` / `uint8_t`
// truncates (`as i16` / `as u8`), and arithmetic that may wrap in C uses
// `wrapping_*`.

//! RFC 3389 comfort noise (CN) decoder, plus the encoder for tests.

use crate::spl;

/// C `WEBRTC_CNG_MAX_LPC_ORDER`.
pub const WEBRTC_CNG_MAX_LPC_ORDER: usize = 12;

/// C++ `kCngMaxOutsizeOrder`: the most samples one `generate` / `encode`
/// call handles (10 ms at 64 kHz).
const K_CNG_MAX_OUTSIZE_ORDER: usize = 640;

/// C++ `WebRtcCng_kDbov`.
const WEBRTC_CNG_K_DBOV: [i32; 94] = [
    1081109975, 858756178, 682134279, 541838517, 430397633, 341876992, 271562548, 215709799,
    171344384, 136103682, 108110997, 85875618, 68213428, 54183852, 43039763, 34187699, 27156255,
    21570980, 17134438, 13610368, 10811100, 8587562, 6821343, 5418385, 4303976, 3418770, 2715625,
    2157098, 1713444, 1361037, 1081110, 858756, 682134, 541839, 430398, 341877, 271563, 215710,
    171344, 136104, 108111, 85876, 68213, 54184, 43040, 34188, 27156, 21571, 17134, 13610, 10811,
    8588, 6821, 5418, 4304, 3419, 2716, 2157, 1713, 1361, 1081, 859, 682, 542, 430, 342, 272, 216,
    171, 136, 108, 86, 68, 54, 43, 34, 27, 22, 17, 14, 11, 9, 7, 5, 4, 3, 3, 2, 2, 1, 1, 1, 1, 1,
];

/// C++ `WebRtcCng_kCorrWindow`.
const WEBRTC_CNG_K_CORR_WINDOW: [i16; WEBRTC_CNG_MAX_LPC_ORDER] = [
    32702, 32636, 32570, 32505, 32439, 32374, 32309, 32244, 32179, 32114, 32049, 31985,
];

/// C++ `ComfortNoiseDecoder`.
#[derive(Debug, Clone)]
pub struct ComfortNoiseDecoder {
    dec_seed: u32,
    dec_target_energy: i32,
    dec_used_energy: i32,
    dec_target_refl_coefs: [i16; WEBRTC_CNG_MAX_LPC_ORDER + 1],
    dec_used_refl_coefs: [i16; WEBRTC_CNG_MAX_LPC_ORDER + 1],
    dec_filtstate: [i16; WEBRTC_CNG_MAX_LPC_ORDER + 1],
    dec_filtstate_low: [i16; WEBRTC_CNG_MAX_LPC_ORDER + 1],
    /// Stored like C++; nothing reads it.
    #[allow(dead_code)]
    dec_order: u16,
    /// Q29.
    dec_target_scale_factor: i16,
    /// Q29.
    dec_used_scale_factor: i16,
}

impl Default for ComfortNoiseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ComfortNoiseDecoder {
    /// A decoder in its reset state.
    pub fn new() -> Self {
        let mut decoder = Self {
            dec_seed: 0,
            dec_target_energy: 0,
            dec_used_energy: 0,
            dec_target_refl_coefs: [0; WEBRTC_CNG_MAX_LPC_ORDER + 1],
            dec_used_refl_coefs: [0; WEBRTC_CNG_MAX_LPC_ORDER + 1],
            dec_filtstate: [0; WEBRTC_CNG_MAX_LPC_ORDER + 1],
            dec_filtstate_low: [0; WEBRTC_CNG_MAX_LPC_ORDER + 1],
            dec_order: 0,
            dec_target_scale_factor: 0,
            dec_used_scale_factor: 0,
        };
        decoder.reset();
        decoder
    }

    /// Resets the state.
    pub fn reset(&mut self) {
        self.dec_seed = 7777; // For debugging only.
        self.dec_target_energy = 0;
        self.dec_used_energy = 0;
        self.dec_target_refl_coefs = [0; WEBRTC_CNG_MAX_LPC_ORDER + 1];
        self.dec_used_refl_coefs = [0; WEBRTC_CNG_MAX_LPC_ORDER + 1];
        self.dec_filtstate = [0; WEBRTC_CNG_MAX_LPC_ORDER + 1];
        self.dec_filtstate_low = [0; WEBRTC_CNG_MAX_LPC_ORDER + 1];
        self.dec_order = 5;
        self.dec_target_scale_factor = 0;
        self.dec_used_scale_factor = 0;
    }

    /// Updates the noise parameters from a SID payload (without headers).
    ///
    /// C++ reads `sid[0]` unconditionally, which is undefined for an empty
    /// payload; here an empty payload leaves the state unchanged.
    pub fn update_sid(&mut self, sid: &[u8]) {
        if sid.is_empty() {
            return;
        }
        let mut length = sid.len();

        // Throw away reflection coefficients of higher order than we can
        // handle.
        if length > WEBRTC_CNG_MAX_LPC_ORDER + 1 {
            length = WEBRTC_CNG_MAX_LPC_ORDER + 1;
        }

        self.dec_order = (length - 1) as u16;

        let sid0 = sid[0].min(93);
        let mut target_energy = WEBRTC_CNG_K_DBOV[sid0 as usize];
        // Take down target energy to 75%.
        target_energy >>= 1;
        target_energy += target_energy >> 2;

        self.dec_target_energy = target_energy;

        // Reconstruct coeffs with tweak for WebRtc implementation of RFC3389.
        let order = self.dec_order as usize;
        if order == WEBRTC_CNG_MAX_LPC_ORDER {
            for i in 0..order {
                // Q7 to Q15.
                self.dec_target_refl_coefs[i] = ((sid[i + 1] as i32) << 8) as i16;
            }
        } else {
            for i in 0..order {
                // Q7 to Q15.
                self.dec_target_refl_coefs[i] = ((sid[i + 1] as i32 - 127) * (1 << 8)) as i16;
            }
        }

        for i in order..WEBRTC_CNG_MAX_LPC_ORDER {
            self.dec_target_refl_coefs[i] = 0;
        }
    }

    /// Fills `out_data` with comfort noise. `new_period` is true for the
    /// first call of a noise period. Returns false on error (more than 640
    /// samples requested).
    pub fn generate(&mut self, out_data: &mut [i16], new_period: bool) -> bool {
        let mut excitation = [0i16; K_CNG_MAX_OUTSIZE_ORDER];
        let mut low = [0i16; K_CNG_MAX_OUTSIZE_ORDER];
        let mut lp_poly = [0i16; WEBRTC_CNG_MAX_LPC_ORDER + 1];
        const REFL_BETA_STD: i16 = 26214; // 0.8 in q15.
        const REFL_BETA_COMP_STD: i16 = 6553; // 0.2 in q15.
        const REFL_BETA_NEW_P: i16 = 19661; // 0.6 in q15.
        const REFL_BETA_COMP_NEW_P: i16 = 13107; // 0.4 in q15.

        let num_samples = out_data.len();
        if num_samples > K_CNG_MAX_OUTSIZE_ORDER {
            return false;
        }

        // These are in Q15.
        let (beta, beta_c) = if new_period {
            self.dec_used_scale_factor = self.dec_target_scale_factor;
            (REFL_BETA_NEW_P, REFL_BETA_COMP_NEW_P)
        } else {
            (REFL_BETA_STD, REFL_BETA_COMP_STD)
        };

        // Calculate new scale factor in Q13. C++ `checked_cast<int16_t>`;
        // both terms shrink their factor, so the sum always fits.
        let scale = spl::spl_mul_16_16_rsft(self.dec_used_scale_factor, beta >> 2, 13)
            + spl::spl_mul_16_16_rsft(self.dec_target_scale_factor, beta_c >> 2, 13);
        debug_assert!(i16::try_from(scale).is_ok());
        self.dec_used_scale_factor = scale as i16;

        self.dec_used_energy >>= 1;
        self.dec_used_energy += self.dec_target_energy >> 1;

        // Do the same for the reflection coeffs, albeit in Q15.
        for i in 0..WEBRTC_CNG_MAX_LPC_ORDER {
            self.dec_used_refl_coefs[i] =
                spl::spl_mul_16_16_rsft(self.dec_used_refl_coefs[i], beta, 15) as i16;
            self.dec_used_refl_coefs[i] = self.dec_used_refl_coefs[i].wrapping_add(
                spl::spl_mul_16_16_rsft(self.dec_target_refl_coefs[i], beta_c, 15) as i16,
            );
        }

        // Compute the polynomial coefficients.
        webrtc_cng_k2a16(
            &self.dec_used_refl_coefs,
            WEBRTC_CNG_MAX_LPC_ORDER,
            &mut lp_poly,
        );

        // Calculate scaling factor based on filter energy.
        let mut en: i16 = 8192; // 1.0 in Q13.
        for i in 0..WEBRTC_CNG_MAX_LPC_ORDER {
            // Floating point value for reference.
            //   E *= 1.0 - (dec_used_reflCoefs_[i] / 32768.0) *
            //   (dec_used_reflCoefs_[i] / 32768.0);

            // Same in fixed point.
            // K(i).^2 in Q15.
            let mut temp16 = spl::spl_mul_16_16_rsft(
                self.dec_used_refl_coefs[i],
                self.dec_used_refl_coefs[i],
                15,
            ) as i16;
            // 1 - K(i).^2 in Q15.
            temp16 = (0x7fff - temp16 as i32) as i16;
            en = spl::spl_mul_16_16_rsft(en, temp16, 15) as i16;
        }

        // float scaling= sqrt(E * dec_target_energy_ / (1 << 24));

        // Calculate sqrt(En * target_energy / excitation energy)
        let target_energy = spl::sqrt(self.dec_used_energy);

        en = (((spl::sqrt(en as i32) as i16) as i32) << 6) as i16;
        en = ((en as i32 * 3) >> 1) as i16; // 1.5 estimates sqrt(2).
        self.dec_used_scale_factor = ((en as i32).wrapping_mul(target_energy) >> 12) as i16;

        // Generate excitation.
        // Excitation energy per sample is 2.^24 - Q13 N(0,1).
        for sample in excitation.iter_mut().take(num_samples) {
            *sample = spl::rand_n(&mut self.dec_seed) >> 1;
        }

        // Scale to correct energy.
        spl::scale_vector_in_place(&mut excitation, self.dec_used_scale_factor, num_samples, 13);

        // `lp_poly`     - Coefficients in Q12.
        // `excitation`  - Speech samples.
        // `dec_filtstate` - State preservation.
        // `out_data`    - Filtered speech samples.
        spl::filter_ar(
            &lp_poly,
            WEBRTC_CNG_MAX_LPC_ORDER + 1,
            &excitation,
            num_samples,
            &mut self.dec_filtstate,
            WEBRTC_CNG_MAX_LPC_ORDER,
            &mut self.dec_filtstate_low,
            out_data,
            &mut low,
        );

        true
    }
}

/// C++ `ComfortNoiseEncoder` (test support).
#[derive(Debug, Clone)]
pub struct ComfortNoiseEncoder {
    enc_nr_of_coefs: usize,
    enc_sampfreq: i32,
    enc_interval: i16,
    enc_ms_since_sid: i16,
    enc_energy: i32,
    enc_refl_coefs: [i16; WEBRTC_CNG_MAX_LPC_ORDER + 1],
    /// Stored like C++; nothing reads it.
    #[allow(dead_code)]
    enc_corr_vector: [i32; WEBRTC_CNG_MAX_LPC_ORDER + 1],
    /// Stored like C++; nothing reads it.
    #[allow(dead_code)]
    enc_seed: u32,
}

impl ComfortNoiseEncoder {
    /// Encoder at `fs` Hz, SID every `interval` ms, LPC order `quality`
    /// (1..=12; panics otherwise, like the C++ `RTC_CHECK`).
    pub fn new(fs: i32, interval: i32, quality: i32) -> Self {
        let mut encoder = Self {
            enc_nr_of_coefs: 0,
            enc_sampfreq: 0,
            enc_interval: 0,
            enc_ms_since_sid: 0,
            enc_energy: 0,
            enc_refl_coefs: [0; WEBRTC_CNG_MAX_LPC_ORDER + 1],
            enc_corr_vector: [0; WEBRTC_CNG_MAX_LPC_ORDER + 1],
            enc_seed: 0,
        };
        encoder.reset(fs, interval, quality);
        encoder
    }

    /// Resets with new parameters.
    pub fn reset(&mut self, fs: i32, interval: i32, quality: i32) {
        assert!(quality > 0, "CNG quality must be > 0");
        assert!(
            quality <= WEBRTC_CNG_MAX_LPC_ORDER as i32,
            "CNG quality must be <= {WEBRTC_CNG_MAX_LPC_ORDER}"
        );
        self.enc_nr_of_coefs = quality as usize;
        self.enc_sampfreq = fs;
        self.enc_interval = interval as i16;
        self.enc_ms_since_sid = 0;
        self.enc_energy = 0;
        self.enc_refl_coefs = [0; WEBRTC_CNG_MAX_LPC_ORDER + 1];
        self.enc_corr_vector = [0; WEBRTC_CNG_MAX_LPC_ORDER + 1];
        self.enc_seed = 7777; // For debugging only.
    }

    /// Analyzes `speech` and appends a SID frame to `output` when due (or
    /// when `force_sid`). Returns the number of bytes appended. Panics if
    /// `speech` is longer than 640 samples (C++ `RTC_CHECK`).
    pub fn encode(&mut self, speech: &[i16], force_sid: bool, output: &mut Vec<u8>) -> usize {
        let mut ar_coefs = [0i16; WEBRTC_CNG_MAX_LPC_ORDER + 1];
        let mut corr_vector = [0i32; WEBRTC_CNG_MAX_LPC_ORDER + 1];
        let mut refl_cs = [0i16; WEBRTC_CNG_MAX_LPC_ORDER + 1];
        let mut hanning_w = [0i16; K_CNG_MAX_OUTSIZE_ORDER];
        const REFL_BETA: i16 = 19661; // 0.6 in q15.
        const REFL_BETA_COMP: i16 = 13107; // 0.4 in q15.
        let mut speech_buf = [0i16; K_CNG_MAX_OUTSIZE_ORDER];

        let num_samples = speech.len();
        assert!(
            num_samples <= K_CNG_MAX_OUTSIZE_ORDER,
            "CNG encoder input longer than {K_CNG_MAX_OUTSIZE_ORDER} samples"
        );

        speech_buf[..num_samples].copy_from_slice(speech);

        let mut factor = num_samples;

        // Calculate energy and a coefficients.
        let mut out_shifts: i32 = 0;
        let mut out_energy = spl::energy(&speech_buf, num_samples, &mut out_shifts);
        while out_shifts > 0 {
            // We can only do 5 shifts without destroying accuracy in division
            // factor.
            if out_shifts > 5 {
                out_energy = out_energy.wrapping_shl((out_shifts - 5) as u32);
                out_shifts = 5;
            } else {
                factor /= 2;
                out_shifts -= 1;
            }
        }
        out_energy = spl::div_w32_w16(out_energy, factor as i16);

        if out_energy > 1 {
            // Create Hanning Window.
            spl::get_hanning_window(&mut hanning_w, num_samples / 2);
            for i in 0..num_samples / 2 {
                hanning_w[num_samples - i - 1] = hanning_w[i];
            }

            spl::elementwise_vector_mult_in_place(&mut speech_buf, &hanning_w, num_samples, 14);

            let mut acorr_scale: i32 = 0;
            spl::auto_correlation(
                &speech_buf,
                num_samples,
                self.enc_nr_of_coefs,
                &mut corr_vector,
                &mut acorr_scale,
            );

            if corr_vector[0] == 0 {
                corr_vector[0] = spl::WORD16_MAX as i32;
            }

            // Adds the bandwidth expansion.
            for (b, &a) in corr_vector
                .iter_mut()
                .zip(WEBRTC_CNG_K_CORR_WINDOW.iter())
                .take(self.enc_nr_of_coefs)
            {
                // The below code multiplies the 16 b corrWindow values (Q15)
                // with the 32 b corrvector (Q0) and shifts the result down 15
                // steps.
                let negate = *b < 0;
                if negate {
                    *b = b.wrapping_neg();
                }

                let a = a as i32;
                let mut blo: i32 = a.wrapping_mul(*b & 0xffff);
                let bhi: i32 =
                    ((blo >> 16) & 0xffff).wrapping_add(a.wrapping_mul((*b >> 16) & 0xffff));
                blo = (blo & 0xffff) | ((bhi & 0xffff) << 16);
                *b = ((((bhi >> 16) & 0x7fff) as u32) << 17 | ((blo as u32) >> 15)) as i32;
                if negate {
                    *b = b.wrapping_neg();
                }
            }
            // End of bandwidth expansion.

            let stab = spl::levinson_durbin(
                &corr_vector,
                &mut ar_coefs,
                &mut refl_cs,
                self.enc_nr_of_coefs,
            );

            if stab == 0 {
                // Disregard from this frame.
                return 0;
            }
        } else {
            for c in refl_cs.iter_mut().take(self.enc_nr_of_coefs) {
                *c = 0;
            }
        }

        if force_sid {
            // Read instantaneous values instead of averaged.
            self.enc_refl_coefs[..self.enc_nr_of_coefs]
                .copy_from_slice(&refl_cs[..self.enc_nr_of_coefs]);
            self.enc_energy = out_energy;
        } else {
            // Average history with new values.
            let n = self.enc_nr_of_coefs;
            for (coef, &refl) in self.enc_refl_coefs[..n].iter_mut().zip(&refl_cs[..n]) {
                *coef = spl::spl_mul_16_16_rsft(*coef, REFL_BETA, 15) as i16;
                *coef = coef.wrapping_add(spl::spl_mul_16_16_rsft(refl, REFL_BETA_COMP, 15) as i16);
            }
            self.enc_energy = (out_energy >> 2)
                .wrapping_add(self.enc_energy >> 1)
                .wrapping_add(self.enc_energy >> 2);
        }

        if self.enc_energy < 1 {
            self.enc_energy = 1;
        }

        let ms_this_frame = ((1000 * num_samples) / self.enc_sampfreq as usize) as i16;
        if (self.enc_ms_since_sid as i32 > self.enc_interval as i32 - 1) || force_sid {
            // Search for best dbov value.
            let mut index: usize = 0;
            let mut i: usize = 1;
            while i < 93 {
                // Always round downwards.
                if self.enc_energy.wrapping_sub(WEBRTC_CNG_K_DBOV[i]) > 0 {
                    index = i;
                    break;
                }
                i += 1;
            }
            if i == 93 && index == 0 {
                index = 94;
            }

            let output_coefs = self.enc_nr_of_coefs + 1;
            output.push(index as u8);
            // Quantize coefficients with tweak for WebRtc implementation of
            // RFC3389.
            if self.enc_nr_of_coefs == WEBRTC_CNG_MAX_LPC_ORDER {
                for &c in &self.enc_refl_coefs[..self.enc_nr_of_coefs] {
                    // Q15 to Q7 with rounding.
                    output.push(((c as i32 + 128) >> 8) as u8);
                }
            } else {
                for &c in &self.enc_refl_coefs[..self.enc_nr_of_coefs] {
                    // Q15 to Q7 with rounding.
                    output.push((127 + ((c as i32 + 128) >> 8)) as u8);
                }
            }
            self.enc_ms_since_sid = ms_this_frame;
            output_coefs
        } else {
            self.enc_ms_since_sid = self.enc_ms_since_sid.wrapping_add(ms_this_frame);
            0
        }
    }
}

/// C++ `WebRtcCng_K2a16`: reflection coefficients `k` (Q15) to LPC
/// polynomial `a` (Q12) of order `use_order`.
fn webrtc_cng_k2a16(k: &[i16], use_order: usize, a: &mut [i16]) {
    let mut any = [0i16; spl::MAX_LPC_ORDER + 1];

    a[0] = 4096; // i.e., (Word16_MAX >> 3) + 1
    any[0] = a[0];
    a[1] = ((k[0] as i32 + 4) >> 3) as i16;
    for m in 1..use_order {
        let km = k[m] as i32;
        any[m + 1] = ((km + 4) >> 3) as i16;
        for i in 0..m {
            any[i + 1] =
                (a[i + 1] as i32 + ((((a[m - i] as i32) * km) + 16384) >> 15) as i16 as i32) as i16;
        }
        a[..m + 2].copy_from_slice(&any[..m + 2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn k2a16_order_one_rounds_reflection_to_q12() {
        let mut a = [0i16; WEBRTC_CNG_MAX_LPC_ORDER + 1];
        webrtc_cng_k2a16(&[16384, 0], 1, &mut a);
        assert_eq!(4096, a[0]);
        assert_eq!(2048, a[1]);
    }
}

#[cfg(test)]
mod wp2b_golden {
    use super::*;
    use crate::neteq::time_stretch::test_signal::make_signal;
    use crate::spl::test_util::{Hash, Rng};

    // Golden hashes against the real C++ (spl/golden/golden_gen_wp2b.cc).

    #[test]
    fn golden_cng_decoder() {
        let mut r = Rng::new();
        let mut h = Hash::new();
        let mut decoder = ComfortNoiseDecoder::new();
        for _ in 0..400 {
            if r.below(4) == 0 {
                let len = r.range(1, 16) as usize;
                let sid: Vec<u8> = (0..len).map(|_| r.next() as u8).collect();
                decoder.update_sid(&sid);
            }
            if r.below(50) == 0 {
                decoder.reset();
            }
            let len = if r.below(20) == 0 {
                641
            } else {
                r.range(0, 640) as usize
            };
            let new_period = r.below(5) == 0;
            let mut out = vec![0i16; len];
            let ok = decoder.generate(&mut out, new_period);
            h.feed(ok as i64);
            if ok {
                h.feed16(&out);
            }
        }
        assert_eq!(0x3d14f87cc951728a, h.value());
    }

    #[test]
    fn golden_cng_encoder() {
        let mut r = Rng::new();
        let mut h = Hash::new();
        for fs in [8000, 16000, 32000, 48000, 64000] {
            for quality in [1, 8, 12] {
                for interval in [1, 100] {
                    let mut encoder = ComfortNoiseEncoder::new(fs, interval, quality);
                    let mut decoder = ComfortNoiseDecoder::new();
                    for n in 0..30 {
                        let len =
                            640.min(if r.below(3) == 0 { fs / 200 } else { fs / 100 }) as usize;
                        let mut sig = make_signal(&mut r, len, fs);
                        if r.below(6) == 0 {
                            for s in &mut sig {
                                *s >>= 12;
                            }
                        }
                        let force_sid = r.below(4) == 0;
                        let mut sid = Vec::new();
                        let ret = encoder.encode(&sig, force_sid, &mut sid);
                        h.feed(ret as i64);
                        h.feed(sid.len() as i64);
                        for &b in &sid {
                            h.feed(b as i64);
                        }
                        if ret > 0 {
                            decoder.update_sid(&sid);
                            let mut out = [0i16; 160];
                            decoder.generate(&mut out, n % 3 == 0);
                            h.feed16(&out);
                        }
                    }
                }
            }
        }
        assert_eq!(0x374a559db17b68ba, h.value());
    }
}

/// Port of cng_unittest.cc. The C++ tests read 640 samples of
/// testfile32kHz.pcm, which the sparse WebRTC checkout lacks; these use the
/// synthetic golden signal instead, and the expected encoder return values
/// are the ones the real C++ prints for it (golden_gen_wp2b.cc,
/// `cng_unittest_values`). Where they differ from the C++ test's
/// expectations, the difference is the input, not the port.
#[cfg(test)]
mod cng_unittest {
    use super::*;
    use crate::neteq::time_stretch::test_signal::make_signal;
    use crate::spl::test_util::Rng;

    const K_SID_SHORT_INTERVAL_UPDATE: i32 = 1;
    const K_SID_NORMAL_INTERVAL_UPDATE: i32 = 100;
    const K_CNG_NUM_PARAMS_LOW: i32 = 0;
    const K_CNG_NUM_PARAMS_NORMAL: i32 = 8;
    const K_CNG_NUM_PARAMS_HIGH: i32 = WEBRTC_CNG_MAX_LPC_ORDER as i32;
    const K_CNG_NUM_PARAMS_TOO_HIGH: i32 = WEBRTC_CNG_MAX_LPC_ORDER as i32 + 1;
    const K_NO_SID: bool = false;
    const K_FORCE_SID: bool = true;

    fn speech_data() -> Vec<i16> {
        make_signal(&mut Rng::new(), 640, 32000)
    }

    fn test_cng_encode(sample_rate_hz: i32, quality: i32) -> (usize, usize) {
        let speech_data = speech_data();
        let num_samples_10ms = (sample_rate_hz / 100) as usize;
        let mut sid_data = Vec::new();
        let mut cng_encoder =
            ComfortNoiseEncoder::new(sample_rate_hz, K_SID_NORMAL_INTERVAL_UPDATE, quality);
        let no_sid = cng_encoder.encode(&speech_data[..num_samples_10ms], K_NO_SID, &mut sid_data);
        let forced =
            cng_encoder.encode(&speech_data[..num_samples_10ms], K_FORCE_SID, &mut sid_data);
        (no_sid, forced)
    }

    #[test]
    #[should_panic]
    fn cng_init_fail_too_few_parameters() {
        ComfortNoiseEncoder::new(8000, K_SID_NORMAL_INTERVAL_UPDATE, K_CNG_NUM_PARAMS_LOW);
    }

    #[test]
    #[should_panic]
    fn cng_init_fail_too_many_parameters() {
        ComfortNoiseEncoder::new(
            8000,
            K_SID_NORMAL_INTERVAL_UPDATE,
            K_CNG_NUM_PARAMS_TOO_HIGH,
        );
    }

    #[test]
    #[should_panic]
    fn cng_encode_too_long() {
        let speech = vec![0i16; 641];
        let mut sid_data = Vec::new();
        let mut cng_encoder =
            ComfortNoiseEncoder::new(8000, K_SID_NORMAL_INTERVAL_UPDATE, K_CNG_NUM_PARAMS_NORMAL);
        cng_encoder.encode(&speech, K_NO_SID, &mut sid_data);
    }

    #[test]
    fn cng_encode_8000() {
        assert_eq!((0, 9), test_cng_encode(8000, K_CNG_NUM_PARAMS_NORMAL));
    }

    #[test]
    fn cng_encode_16000() {
        assert_eq!((0, 9), test_cng_encode(16000, K_CNG_NUM_PARAMS_NORMAL));
    }

    #[test]
    fn cng_encode_32000() {
        assert_eq!((0, 13), test_cng_encode(32000, K_CNG_NUM_PARAMS_HIGH));
    }

    #[test]
    fn cng_encode_48000() {
        assert_eq!((0, 9), test_cng_encode(48000, K_CNG_NUM_PARAMS_NORMAL));
    }

    #[test]
    fn cng_encode_64000() {
        assert_eq!((0, 9), test_cng_encode(64000, K_CNG_NUM_PARAMS_NORMAL));
    }

    // Update SID parameters, for both 9 and 13 parameters.
    #[test]
    fn cng_update_sid() {
        let speech_data = speech_data();
        let mut sid_data = Vec::new();
        let mut cng_encoder =
            ComfortNoiseEncoder::new(16000, K_SID_NORMAL_INTERVAL_UPDATE, K_CNG_NUM_PARAMS_NORMAL);
        let mut cng_decoder = ComfortNoiseDecoder::new();

        // Run normal Encode and UpdateSid.
        assert_eq!(
            K_CNG_NUM_PARAMS_NORMAL as usize + 1,
            cng_encoder.encode(&speech_data[..160], K_FORCE_SID, &mut sid_data)
        );
        cng_decoder.update_sid(&sid_data);

        // Reinit with new length.
        cng_encoder.reset(16000, K_SID_NORMAL_INTERVAL_UPDATE, K_CNG_NUM_PARAMS_HIGH);
        cng_decoder.reset();

        // The C++ test expects 0 here ("unstable parameters after switching
        // length") for its file; the synthetic signal is stable, and the real
        // C++ returns 13 for it too.
        assert_eq!(
            K_CNG_NUM_PARAMS_HIGH as usize + 1,
            cng_encoder.encode(&speech_data[..160], K_FORCE_SID, &mut sid_data)
        );
        assert_eq!(
            K_CNG_NUM_PARAMS_HIGH as usize + 1,
            cng_encoder.encode(&speech_data[160..320], K_FORCE_SID, &mut sid_data)
        );
        cng_decoder.update_sid(&sid_data[..K_CNG_NUM_PARAMS_NORMAL as usize + 1]);
    }

    // Update SID parameters, with wrong parameters or without calling decode.
    #[test]
    fn cng_update_sid_erroneous() {
        let speech_data = speech_data();
        let mut sid_data = Vec::new();
        let mut cng_encoder =
            ComfortNoiseEncoder::new(16000, K_SID_NORMAL_INTERVAL_UPDATE, K_CNG_NUM_PARAMS_NORMAL);
        let mut cng_decoder = ComfortNoiseDecoder::new();
        assert_eq!(
            K_CNG_NUM_PARAMS_NORMAL as usize + 1,
            cng_encoder.encode(&speech_data[..160], K_FORCE_SID, &mut sid_data)
        );

        // First run with valid parameters, then with too many CNG parameters.
        // The function will operate correctly by only reading the maximum
        // number of parameters, skipping the extra.
        assert_eq!(K_CNG_NUM_PARAMS_NORMAL as usize + 1, sid_data.len());
        cng_decoder.update_sid(&sid_data);

        sid_data.resize(K_CNG_NUM_PARAMS_TOO_HIGH as usize + 1, 0);
        cng_decoder.update_sid(&sid_data);
    }

    // Test to generate cng data, by forcing SID. Both normal and faulty
    // condition.
    #[test]
    fn cng_generate() {
        let speech_data = speech_data();
        let mut sid_data = Vec::new();
        let mut out_data = [0i16; 641];
        let mut cng_encoder =
            ComfortNoiseEncoder::new(16000, K_SID_NORMAL_INTERVAL_UPDATE, K_CNG_NUM_PARAMS_NORMAL);
        let mut cng_decoder = ComfortNoiseDecoder::new();

        // Normal Encode.
        assert_eq!(
            K_CNG_NUM_PARAMS_NORMAL as usize + 1,
            cng_encoder.encode(&speech_data[..160], K_FORCE_SID, &mut sid_data)
        );

        // Normal UpdateSid.
        cng_decoder.update_sid(&sid_data);

        // Two normal Generate, one with new_period.
        assert!(cng_decoder.generate(&mut out_data[..640], true));
        assert!(cng_decoder.generate(&mut out_data[..640], false));

        // Call Generate with too much data.
        assert!(!cng_decoder.generate(&mut out_data[..641], false));
    }

    // Test automatic SID.
    #[test]
    fn cng_auto_sid() {
        let speech_data = speech_data();
        let mut sid_data = Vec::new();
        let mut cng_encoder =
            ComfortNoiseEncoder::new(16000, K_SID_NORMAL_INTERVAL_UPDATE, K_CNG_NUM_PARAMS_NORMAL);

        // Normal Encode, 100 msec, where no SID data should be generated.
        for _ in 0..10 {
            assert_eq!(
                0,
                cng_encoder.encode(&speech_data[..160], K_NO_SID, &mut sid_data)
            );
        }

        // We have reached 100 msec, and SID data should be generated.
        assert_eq!(
            K_CNG_NUM_PARAMS_NORMAL as usize + 1,
            cng_encoder.encode(&speech_data[..160], K_NO_SID, &mut sid_data)
        );
    }

    // Test automatic SID, with very short interval.
    #[test]
    fn cng_auto_sid_short() {
        let speech_data = speech_data();
        let mut sid_data = Vec::new();
        let mut cng_encoder =
            ComfortNoiseEncoder::new(16000, K_SID_SHORT_INTERVAL_UPDATE, K_CNG_NUM_PARAMS_NORMAL);

        // First call will never generate SID, unless forced to.
        assert_eq!(
            0,
            cng_encoder.encode(&speech_data[..160], K_NO_SID, &mut sid_data)
        );

        // Normal Encode, 100 msec, SID data should be generated all the time.
        for _ in 0..10 {
            assert_eq!(
                K_CNG_NUM_PARAMS_NORMAL as usize + 1,
                cng_encoder.encode(&speech_data[..160], K_NO_SID, &mut sid_data)
            );
        }
    }
}

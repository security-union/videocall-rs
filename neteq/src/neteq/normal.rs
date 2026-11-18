// Port target: WebRTC modules/audio_coding/neteq/normal.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: normal_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// C++ `Normal` keeps `DecoderDatabase*`, `const BackgroundNoise&`, `Expand*`
// and `StatisticsCalculator*`; here they are passed per call
// (`BackgroundNoise` and statistics inside `ExpandContext`).

//! Normal playout, with a cross-fade out of a preceding expansion.

use crate::api::neteq::Mode;
use crate::neteq::audio_multi_vector::AudioMultiVector;
use crate::neteq::audio_vector::AudioVector;
use crate::neteq::decoder_database::DecoderDatabase;
use crate::neteq::expand::{Expand, ExpandContext};
use crate::spl;

/// C++ `Crossfade` (anonymous namespace in normal.cc): fades `to` in over
/// `from` across the first `win_length` samples.
fn crossfade(from: &AudioVector, to: &mut AudioVector, win_length: usize) {
    let win_length_clamped = win_length.min(from.size()).min(to.size());
    if win_length_clamped == 0 {
        return;
    }
    // `(1 << 14) / static_cast<int16_t>(len)` is `int`, stored as `int16_t`.
    let win_slope_q14: i16 = ((1 << 14) / (win_length_clamped as i16) as i32) as i16;
    let mut win_up_q14: i16 = 0;
    for i in 0..win_length_clamped {
        win_up_q14 = (win_up_q14 as i32 + win_slope_q14 as i32) as i16;
        to[i] = ((win_up_q14 as i32 * to[i] as i32
            + ((1 << 14) - win_up_q14 as i32) * from[i] as i32
            + (1 << 13))
            >> 14) as i16;
    }
}

/// C++ `Normal`.
#[derive(Debug)]
pub struct Normal {
    fs_hz: i32,
    samples_per_ms: usize,
}

impl Normal {
    /// Normal at `fs_hz` (must be a multiple of 1000).
    pub fn new(fs_hz: i32) -> Self {
        // C++ `CheckedDivExact(fs_hz_, 1000)`.
        assert_eq!(fs_hz % 1000, 0, "fs_hz must be a multiple of 1000");
        Self {
            fs_hz,
            samples_per_ms: (fs_hz / 1000) as usize,
        }
    }

    /// Processes `input[..length]` (interleaved decoded audio) into `output`,
    /// smoothing the transition if `last_mode` was an expansion or CNG.
    /// Returns the number of samples written.
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &mut self,
        input: &[i16],
        length: usize,
        last_mode: Mode,
        output: &mut AudioMultiVector,
        expand: &mut Expand,
        ctx: &mut ExpandContext<'_>,
        decoder_database: &mut DecoderDatabase,
    ) -> i32 {
        if length == 0 {
            // Nothing to process.
            output.clear();
            return length as i32;
        }

        debug_assert!(output.empty());
        // Output should be empty at this point.
        if !length.is_multiple_of(output.channels()) {
            // The length does not match the number of channels.
            output.clear();
            return 0;
        }
        output.push_back_interleaved(&input[..length]);

        let fs_mult = self.fs_hz / 8000;
        debug_assert!(fs_mult > 0);
        // fs_shift = log2(fs_mult), rounded down.
        // Note that `fs_shift` is not "exact" for 48 kHz.
        // TODO(hlundin): Investigate this further.
        let fs_shift: i32 = 30 - spl::norm_w32(fs_mult) as i32;

        // If last call resulted in a CodedPlc we don't need to do cross-fading but we
        // need to report the end of the interruption once we are back to normal
        // operation.
        if last_mode == Mode::CodecPlc {
            ctx.statistics.end_expand_event(self.fs_hz);
        }

        // Check if last RecOut call resulted in an Expand. If so, we have to take
        // care of some cross-fading and unmuting.
        if last_mode == Mode::Expand {
            // Generate interpolation data using Expand.
            // First, set Expand parameters to appropriate values.
            expand.set_parameters_for_normal_after_expand(ctx.statistics);

            // Call Expand.
            let mut expanded = AudioMultiVector::new(output.channels());
            expand.process(ctx, &mut expanded);
            expand.reset();

            let length_per_channel = length / output.channels();
            let mut signal = vec![0i16; length_per_channel];
            for channel_ix in 0..output.channels() {
                // Set muting factor to the same as expand muting factor.
                let mut mute_factor: i16 = expand.mute_factor(channel_ix);

                output[channel_ix].copy_to_slice(length_per_channel, 0, &mut signal);

                // Find largest absolute value in new data.
                let decoded_max = spl::max_abs_value_w16(&signal, length_per_channel);
                // Adjust muting factor if needed (to BGN level).
                let energy_length = ((fs_mult * 64) as usize).min(length_per_channel);
                let mut scaling: i32 =
                    6 + fs_shift - spl::norm_w32(decoded_max as i32 * decoded_max as i32) as i32;
                scaling = scaling.max(0); // `scaling` should always be >= 0.
                let mut energy =
                    spl::dot_product_with_scale(&signal, &signal, energy_length, scaling);
                let scaled_energy_length = (energy_length >> scaling) as i32;
                if scaled_energy_length > 0 {
                    energy /= scaled_energy_length;
                } else {
                    energy = 0;
                }

                let mut local_mute_factor: i32 = 16384; // 1.0 in Q14.
                let bgn_energy_value = ctx.background_noise.energy(channel_ix);
                if energy != 0 && energy > bgn_energy_value {
                    // Normalize new frame energy to 15 bits.
                    scaling = spl::norm_w32(energy) as i32 - 16;
                    // We want background_noise_.energy() / energy in Q14.
                    let bgn_energy = spl::spl_shift_w32(bgn_energy_value, scaling + 14);
                    let energy_scaled = spl::spl_shift_w32(energy, scaling) as i16;
                    let ratio = spl::div_w32_w16(bgn_energy, energy_scaled);
                    local_mute_factor =
                        local_mute_factor.min(spl::sqrt_floor(ratio.wrapping_shl(14)));
                }
                // `std::max<int16_t>` converts `local_mute_factor` to `int16_t`.
                mute_factor = mute_factor.max(local_mute_factor as i16);
                debug_assert!(mute_factor <= 16384);
                debug_assert!(mute_factor >= 0);

                // If muted increase by 0.64 for every 20 ms (NB/WB 0.0040/0.0020 in Q14),
                // or as fast as it takes to come back to full gain within the frame
                // length.
                // `(16384 - mute_factor) / length_per_channel` is `size_t` division.
                let back_to_fullscale_inc =
                    ((16384 - mute_factor as i32) as usize / length_per_channel) as i32;
                let increment = (64 / fs_mult).max(back_to_fullscale_inc);
                let channel = &mut output[channel_ix];
                for i in 0..length_per_channel {
                    // Scale with mute factor.
                    let scaled_signal = channel[i] as i32 * mute_factor as i32;
                    // Shift 14 with proper rounding.
                    channel[i] = ((scaled_signal + 8192) >> 14) as i16;
                    // Increase mute_factor towards 16384.
                    mute_factor = (mute_factor as i32 + increment).min(16384) as i16;
                }

                // Interpolate the expanded data into the new vector.
                // (NB/WB/SWB32/SWB48 8/16/32/48 samples.)
                crossfade(&expanded[channel_ix], channel, self.samples_per_ms);
            }
        } else if last_mode == Mode::Rfc3389Cng {
            debug_assert_eq!(output.channels(), 1); // Not adapted for multi-channel yet.
            const K_CNG_LENGTH: usize = 48;
            debug_assert!(8 * fs_mult as usize <= K_CNG_LENGTH);
            // Uninitialized in C++; only the samples written below are ever
            // read (the cross-fade is clamped to `output`'s length).
            let mut cng_output = [0i16; K_CNG_LENGTH];

            if let Some(cng_decoder) = decoder_database.get_active_cng_decoder() {
                // Generate long enough for 48kHz.
                if !cng_decoder.generate(&mut cng_output, false) {
                    // Error returned; set return vector to all zeros.
                    cng_output = [0; K_CNG_LENGTH];
                }
            } else {
                // If no CNG instance is defined, just copy from the decoded data.
                // (This will result in interpolating the decoded with itself.)
                output[0].copy_to_slice((fs_mult * 8) as usize, 0, &mut cng_output);
            }
            // Interpolate the CNG into the new vector.
            // (NB/WB/SWB32/SWB48 8/16/32/48 samples.)
            let mut temp_vector = AudioVector::with_size(K_CNG_LENGTH);
            temp_vector.overwrite_at_slice(&cng_output, 0);
            crossfade(&temp_vector, &mut output[0], self.samples_per_ms);
        }

        length as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::BuiltinAudioDecoderFactory;
    use crate::neteq::background_noise::BackgroundNoise;
    use crate::neteq::random_vector::RandomVector;
    use crate::neteq::statistics_calculator::StatisticsCalculator;
    use crate::neteq::sync_buffer::SyncBuffer;
    use std::sync::Arc;

    /// Owns everything `Normal::process` borrows. C++ uses mocks for the
    /// decoder database and `Expand`; here both are real (a database with no
    /// active CNG decoder behaves like the default mock).
    struct Fixture {
        bgn: BackgroundNoise,
        sync_buffer: SyncBuffer,
        random_vector: RandomVector,
        statistics: StatisticsCalculator,
        expand: Expand,
        db: DecoderDatabase,
        normal: Normal,
    }

    impl Fixture {
        fn new(fs: i32, channels: usize, sync_buffer_length: usize) -> Self {
            Self {
                bgn: BackgroundNoise::new(channels),
                sync_buffer: SyncBuffer::new(channels, sync_buffer_length),
                random_vector: RandomVector::new(),
                statistics: StatisticsCalculator::new(),
                expand: Expand::new(fs, channels),
                db: DecoderDatabase::new(Arc::new(BuiltinAudioDecoderFactory::new())),
                normal: Normal::new(fs),
            }
        }

        fn process(
            &mut self,
            input: &[i16],
            length: usize,
            last_mode: Mode,
            output: &mut AudioMultiVector,
        ) -> i32 {
            let mut ctx = ExpandContext {
                sync_buffer: &mut self.sync_buffer,
                background_noise: &mut self.bgn,
                random_vector: &mut self.random_vector,
                statistics: &mut self.statistics,
            };
            self.normal.process(
                input,
                length,
                last_mode,
                output,
                &mut self.expand,
                &mut ctx,
                &mut self.db,
            )
        }
    }

    #[test]
    fn create_and_destroy() {
        let _f = Fixture::new(8000, 1, 1000);
    }

    #[test]
    fn avoid_divide_by_zero() {
        let channels = 1;
        let mut f = Fixture::new(8000, channels, 1000);
        let input = [0i16; 1000];
        let mut output = AudioMultiVector::new(channels);

        // Zero input length.
        assert_eq!(0, f.process(&input, 0, Mode::Expand, &mut output));
        assert_eq!(0, output.size());

        // Try to make energy_length >> scaling = 0;
        // If input_size_samples < 64, then energy_length in Normal::Process() will
        // be equal to input_size_samples. Since the input is all zeros, decoded_max
        // will be zero, and scaling will be >= 6. Thus, energy_length >> scaling = 0,
        // and using this as a denominator would lead to problems.
        let input_size_samples = 63;
        assert_eq!(
            input_size_samples as i32,
            f.process(&input, input_size_samples, Mode::Expand, &mut output)
        );
    }

    #[test]
    fn input_length_and_channels_do_not_match() {
        let channels = 2;
        let mut f = Fixture::new(8000, channels, 1000);
        let input = [0i16; 1000];
        let mut output = AudioMultiVector::new(channels);

        // Let the number of samples be one sample less than 80 samples per channel.
        let input_len = 80 * channels - 1;
        assert_eq!(0, f.process(&input, input_len, Mode::Expand, &mut output));
        assert_eq!(0, output.size());
    }

    // The C++ test mocks `Expand::Process` to return 120 ms of audio. The real
    // Expand at 48 kHz needs 256 * 6 samples of history, so the sync buffer
    // is larger than the C++ 1000 samples; the asserted sizes are the same.
    #[test]
    fn last_mode_expand_120ms_packet() {
        const K_FS: i32 = 48000;
        const K_PACKETSIZE_BYTES: usize = 11520;
        const K_CHANNELS: usize = 1;
        let mut f = Fixture::new(K_FS, K_CHANNELS, 720 * 48);

        let input = [0i16; K_PACKETSIZE_BYTES];
        let mut output = AudioMultiVector::new(K_CHANNELS);

        assert_eq!(
            K_PACKETSIZE_BYTES as i32,
            f.process(&input, K_PACKETSIZE_BYTES, Mode::Expand, &mut output)
        );
        assert_eq!(K_PACKETSIZE_BYTES, output.size());
    }

    #[test]
    fn last_mode_rfc3389_cng_small_input() {
        const K_CHANNELS: usize = 1;
        const K_INPUT_FRAMES: usize = 10;
        let mut f = Fixture::new(48000, K_CHANNELS, 1000);
        let mut output = AudioMultiVector::new(K_CHANNELS);
        let input = vec![0i16; K_CHANNELS * K_INPUT_FRAMES];
        assert_eq!(
            input.len() as i32,
            f.process(&input, input.len(), Mode::Rfc3389Cng, &mut output)
        );
    }

    #[test]
    fn crossfade_matches_hand_computation() {
        let mut from = AudioVector::new();
        from.push_back_slice(&[1000, 1000, 1000, 1000]);
        let mut to = AudioVector::new();
        to.push_back_slice(&[0, 0, 0, 0, 0]);
        crossfade(&from, &mut to, 4);
        // win_slope = 4096; win_up = 4096, 8192, 12288, 16384.
        assert_eq!([750, 500, 250, 0, 0], [to[0], to[1], to[2], to[3], to[4]]);
    }
}

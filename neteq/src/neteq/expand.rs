// Port target: WebRTC modules/audio_coding/neteq/expand.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: expand_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// C++ `Expand` keeps raw pointers to `BackgroundNoise`, `SyncBuffer`,
// `RandomVector` and `StatisticsCalculator`. Here `NetEqImpl` owns those and
// lends them per call through `ExpandContext` (PARITY_PLAN.md 3.2). The
// factory (`ExpandFactory`) is dropped; `NetEqImpl` constructs directly.

//! Packet loss concealment by pitch-period repetition mixed with noise.

use crate::neteq::audio_multi_vector::AudioMultiVector;
use crate::neteq::audio_vector::AudioVector;
use crate::neteq::background_noise::BackgroundNoise;
use crate::neteq::cross_correlation::cross_correlation_with_auto_shift;
use crate::neteq::dsp_helper::DspHelper;
use crate::neteq::random_vector::RandomVector;
use crate::neteq::statistics_calculator::StatisticsCalculator;
use crate::neteq::sync_buffer::SyncBuffer;
use crate::spl;

/// The collaborators C++ `Expand` reaches through its pointers. `Merge` and
/// `Normal` pass the same context through when they call `Expand`.
///
/// `sync_buffer` is `&mut` (the plan sketch had `&`) because `Merge`
/// mutates the sync buffer while also driving `Expand` with this context.
pub struct ExpandContext<'a> {
    pub sync_buffer: &'a mut SyncBuffer,
    pub background_noise: &'a mut BackgroundNoise,
    pub random_vector: &'a mut RandomVector,
    pub statistics: &'a mut StatisticsCalculator,
}

const K_UNVOICED_LPC_ORDER: usize = 6;
const K_NUM_CORRELATION_CANDIDATES: usize = 3;
const K_DISTORTION_LENGTH: usize = 20;
const K_LPC_ANALYSIS_LENGTH: usize = 160;
const K_MAX_SAMPLE_RATE: usize = 48000;
const K_NUM_LAGS: i32 = 3;
/// Size of the C++ `random_vector` stack array in `Process`.
const K_RANDOM_VECTOR_SIZE: usize = K_MAX_SAMPLE_RATE / 8000 * 120 + 30;
const K_NOISE_LPC_ORDER: usize = BackgroundNoise::K_MAX_LPC_ORDER;
/// Size of the C++ `unvoiced_array_memory` stack arrays.
const K_UNVOICED_ARRAY_SIZE: usize = K_NOISE_LPC_ORDER + K_MAX_SAMPLE_RATE / 8000 * 125;

/// C++ `Expand::ChannelParameters`.
#[derive(Debug, Clone)]
struct ChannelParameters {
    mute_factor: i16,
    ar_filter: [i16; K_UNVOICED_LPC_ORDER + 1],
    ar_filter_state: [i16; K_UNVOICED_LPC_ORDER],
    ar_gain: i16,
    ar_gain_scale: i16,
    /// Q14.
    voice_mix_factor: i16,
    /// Q14.
    current_voice_mix_factor: i16,
    expand_vector0: AudioVector,
    expand_vector1: AudioVector,
    onset: bool,
    /// Q20.
    mute_slope: i32,
}

impl ChannelParameters {
    fn new() -> Self {
        Self {
            mute_factor: 16384,
            ar_filter: [0; K_UNVOICED_LPC_ORDER + 1],
            ar_filter_state: [0; K_UNVOICED_LPC_ORDER],
            ar_gain: 0,
            ar_gain_scale: 0,
            voice_mix_factor: 0,
            current_voice_mix_factor: 0,
            expand_vector0: AudioVector::new(),
            expand_vector1: AudioVector::new(),
            onset: false,
            mute_slope: 0,
        }
    }
}

/// C++ `Expand`.
#[derive(Debug)]
pub struct Expand {
    first_expand: bool,
    fs_hz: i32,
    num_channels: usize,
    consecutive_expands: i32,
    overlap_length: usize,
    max_lag: usize,
    expand_lags: [usize; K_NUM_LAGS as usize],
    lag_index_direction: i32,
    current_lag_index: i32,
    stop_muting: bool,
    expand_duration_samples: usize,
    channel_parameters: Vec<ChannelParameters>,
}

impl Expand {
    /// C++ `kMaxConsecutiveExpands`.
    pub const K_MAX_CONSECUTIVE_EXPANDS: i32 = 200;

    /// Expand for `num_channels` channels at `fs` Hz.
    pub fn new(fs: i32, num_channels: usize) -> Self {
        debug_assert!(fs == 8000 || fs == 16000 || fs == 32000 || fs == 48000);
        debug_assert!(fs <= K_MAX_SAMPLE_RATE as i32);
        debug_assert!(num_channels > 0);
        let mut expand = Self {
            first_expand: true,
            fs_hz: fs,
            num_channels,
            consecutive_expands: 0,
            overlap_length: (5 * fs / 8000) as usize,
            max_lag: 0,
            expand_lags: [0; K_NUM_LAGS as usize],
            lag_index_direction: 0,
            current_lag_index: 0,
            stop_muting: false,
            expand_duration_samples: 0,
            channel_parameters: vec![ChannelParameters::new(); num_channels],
        };
        expand.reset();
        expand
    }

    /// Resets the object to its initial state.
    pub fn reset(&mut self) {
        self.first_expand = true;
        self.consecutive_expands = 0;
        self.max_lag = 0;
        for parameters in &mut self.channel_parameters {
            parameters.expand_vector0.clear();
            parameters.expand_vector1.clear();
        }
    }

    /// Produces concealment audio, appended to the end of `output`.
    /// Returns 0 on success.
    pub fn process(&mut self, ctx: &mut ExpandContext<'_>, output: &mut AudioMultiVector) -> i32 {
        let mut random_vector = [0i16; K_RANDOM_VECTOR_SIZE];
        let mut scaled_random_vector = [0i16; K_MAX_SAMPLE_RATE / 8000 * 125];
        // C++ `unvoiced_array_memory`: `unvoiced_vector` starts at
        // `K_UNVOICED_LPC_ORDER`, `noise_vector` at `K_NOISE_LPC_ORDER`.
        let mut unvoiced_array_memory = [0i16; K_UNVOICED_ARRAY_SIZE];
        let overlap_length = self.overlap_length;

        let fs_mult = self.fs_hz / 8000;

        if self.first_expand {
            // Perform initial setup if this is the first expansion since last reset.
            self.analyze_signal(ctx, &mut random_vector);
            self.first_expand = false;
            self.expand_duration_samples = 0;
        } else {
            // This is not the first expansion, parameters are already estimated.
            // Extract a noise segment.
            let rand_length = self.max_lag;
            // This only applies to SWB where length could be larger than 256.
            debug_assert!(rand_length <= K_RANDOM_VECTOR_SIZE);
            Self::generate_random_vector(ctx.random_vector, 2, rand_length, &mut random_vector);
        }

        // Generate signal.
        self.update_lag_index();

        // Voiced part.
        // Generate a weighted vector with the current lag.
        let expansion_vector_length = self.max_lag + overlap_length;
        let current_lag = self.expand_lags[self.current_lag_index as usize];
        // Copy lag+overlap data.
        let expansion_vector_position = expansion_vector_length - current_lag - overlap_length;
        let expansion_temp_length = current_lag + overlap_length;
        // C++ `temp_data` doubles as `voiced_vector_storage`, and writes the
        // mixed output into `temp_data` while still reading `voiced_vector =
        // temp_data + overlap_length`. Every write lands strictly behind the
        // read position, so reading from a separate copy of the voiced
        // samples gives the same result.
        let mut voiced_vector_storage = vec![0i16; expansion_temp_length];
        let mut temp_data = vec![0i16; current_lag];
        for channel_ix in 0..self.num_channels {
            let parameters = &mut self.channel_parameters[channel_ix];
            if self.current_lag_index == 0 {
                // Use only expand_vector0.
                debug_assert!(
                    expansion_vector_position + expansion_temp_length
                        <= parameters.expand_vector0.size()
                );
                parameters.expand_vector0.copy_to_slice(
                    expansion_temp_length,
                    expansion_vector_position,
                    &mut voiced_vector_storage,
                );
            } else if self.current_lag_index == 1 {
                let mut temp_0 = vec![0i16; expansion_temp_length];
                parameters.expand_vector0.copy_to_slice(
                    expansion_temp_length,
                    expansion_vector_position,
                    &mut temp_0,
                );
                let mut temp_1 = vec![0i16; expansion_temp_length];
                parameters.expand_vector1.copy_to_slice(
                    expansion_temp_length,
                    expansion_vector_position,
                    &mut temp_1,
                );
                // Mix 3/4 of expand_vector0 with 1/4 of expand_vector1.
                spl::scale_and_add_vectors_with_round(
                    &temp_0,
                    3,
                    &temp_1,
                    1,
                    2,
                    &mut voiced_vector_storage,
                    expansion_temp_length,
                );
            } else if self.current_lag_index == 2 {
                // Mix 1/2 of expand_vector0 with 1/2 of expand_vector1.
                debug_assert!(
                    expansion_vector_position + expansion_temp_length
                        <= parameters.expand_vector0.size()
                );
                debug_assert!(
                    expansion_vector_position + expansion_temp_length
                        <= parameters.expand_vector1.size()
                );

                let mut temp_0 = vec![0i16; expansion_temp_length];
                parameters.expand_vector0.copy_to_slice(
                    expansion_temp_length,
                    expansion_vector_position,
                    &mut temp_0,
                );
                let mut temp_1 = vec![0i16; expansion_temp_length];
                parameters.expand_vector1.copy_to_slice(
                    expansion_temp_length,
                    expansion_vector_position,
                    &mut temp_1,
                );
                spl::scale_and_add_vectors_with_round(
                    &temp_0,
                    1,
                    &temp_1,
                    1,
                    1,
                    &mut voiced_vector_storage,
                    expansion_temp_length,
                );
            }

            // Get tapering window parameters. Values are in Q15.
            let (
                muting_window,
                muting_window_increment,
                unmuting_window,
                unmuting_window_increment,
            ) = match self.fs_hz {
                8000 => (
                    DspHelper::K_MUTE_FACTOR_START_8KHZ,
                    DspHelper::K_MUTE_FACTOR_INCREMENT_8KHZ,
                    DspHelper::K_UNMUTE_FACTOR_START_8KHZ,
                    DspHelper::K_UNMUTE_FACTOR_INCREMENT_8KHZ,
                ),
                16000 => (
                    DspHelper::K_MUTE_FACTOR_START_16KHZ,
                    DspHelper::K_MUTE_FACTOR_INCREMENT_16KHZ,
                    DspHelper::K_UNMUTE_FACTOR_START_16KHZ,
                    DspHelper::K_UNMUTE_FACTOR_INCREMENT_16KHZ,
                ),
                32000 => (
                    DspHelper::K_MUTE_FACTOR_START_32KHZ,
                    DspHelper::K_MUTE_FACTOR_INCREMENT_32KHZ,
                    DspHelper::K_UNMUTE_FACTOR_START_32KHZ,
                    DspHelper::K_UNMUTE_FACTOR_INCREMENT_32KHZ,
                ),
                // fs_ == 48000
                _ => (
                    DspHelper::K_MUTE_FACTOR_START_48KHZ,
                    DspHelper::K_MUTE_FACTOR_INCREMENT_48KHZ,
                    DspHelper::K_UNMUTE_FACTOR_START_48KHZ,
                    DspHelper::K_UNMUTE_FACTOR_INCREMENT_48KHZ,
                ),
            };
            // The C++ locals are `int16_t`.
            let (mut muting_window, mut unmuting_window) =
                (muting_window as i16, unmuting_window as i16);
            let (muting_window_increment, unmuting_window_increment) = (
                muting_window_increment as i16,
                unmuting_window_increment as i16,
            );

            // Smooth the expanded if it has not been muted to a low amplitude and
            // `current_voice_mix_factor` is larger than 0.5.
            if parameters.mute_factor > 819 && parameters.current_voice_mix_factor > 8192 {
                let start_ix = ctx.sync_buffer.size() - overlap_length;
                let channel = &mut ctx.sync_buffer[channel_ix];
                for i in 0..overlap_length {
                    // Do overlap add between new vector and overlap.
                    let mixed = (channel[start_ix + i] as i32 * muting_window as i32)
                        .wrapping_add(
                            ((parameters.mute_factor as i32 * voiced_vector_storage[i] as i32)
                                >> 14)
                                * unmuting_window as i32,
                        )
                        .wrapping_add(16384)
                        >> 15;
                    channel[start_ix + i] = mixed as i16;
                    muting_window = (muting_window as i32 + muting_window_increment as i32) as i16;
                    unmuting_window =
                        (unmuting_window as i32 + unmuting_window_increment as i32) as i16;
                }
            } else if parameters.mute_factor == 0 {
                // The expanded signal will consist of only comfort noise if
                // mute_factor = 0. Set the output length to 15 ms for best noise
                // production.
                // TODO(hlundin): This has been disabled since the length of
                // parameters.expand_vector0 and parameters.expand_vector1 no longer
                // match with expand_lags_, causing invalid reads and writes. Is it a good
                // idea to enable this again, and solve the vector size problem?
            }

            // Unvoiced part.
            // Filter `scaled_random_vector` through `ar_filter_`.
            unvoiced_array_memory[..K_UNVOICED_LPC_ORDER]
                .copy_from_slice(&parameters.ar_filter_state);
            let mut add_constant: i32 = 0;
            if parameters.ar_gain_scale > 0 {
                add_constant = 1 << (parameters.ar_gain_scale - 1);
            }
            spl::affine_transform_vector(
                &mut scaled_random_vector,
                &random_vector,
                parameters.ar_gain,
                add_constant,
                parameters.ar_gain_scale,
                current_lag,
            );
            spl::filter_ar_fast_q12(
                &scaled_random_vector,
                &mut unvoiced_array_memory,
                K_UNVOICED_LPC_ORDER,
                &parameters.ar_filter,
                K_UNVOICED_LPC_ORDER + 1,
                current_lag,
            );
            // `&unvoiced_vector[current_lag - kUnvoicedLpcOrder]`.
            parameters.ar_filter_state.copy_from_slice(
                &unvoiced_array_memory[current_lag..current_lag + K_UNVOICED_LPC_ORDER],
            );

            // Combine voiced and unvoiced contributions.

            // Set a suitable cross-fading slope.
            // For lag =
            //   <= 31 * fs_mult            => go from 1 to 0 in about 8 ms;
            //  (>= 31 .. <= 63) * fs_mult  => go from 1 to 0 in about 16 ms;
            //   >= 64 * fs_mult            => go from 1 to 0 in about 32 ms.
            // temp_shift = getbits(max_lag_) - 5.
            let mut temp_shift: i32 = (31 - spl::norm_w32(self.max_lag as i32) as i32) - 5;
            let mut mix_factor_increment: i16 = (256 >> temp_shift) as i16;
            if self.stop_muting {
                mix_factor_increment = 0;
            }

            // Create combined signal by shifting in more and more of unvoiced part.
            temp_shift = 8 - temp_shift; // = getbits(mix_factor_increment).
                                         // `int` difference converted to `size_t` (sign-extending).
            let mut temp_length = ((parameters.current_voice_mix_factor as i32
                - parameters.voice_mix_factor as i32)
                >> temp_shift) as isize as usize;
            temp_length = temp_length.min(current_lag);
            let voiced_vector = &voiced_vector_storage[overlap_length..];
            let unvoiced_vector = &unvoiced_array_memory[K_UNVOICED_LPC_ORDER..];
            DspHelper::cross_fade(
                voiced_vector,
                unvoiced_vector,
                temp_length,
                &mut parameters.current_voice_mix_factor,
                mix_factor_increment,
                &mut temp_data,
            );

            // End of cross-fading period was reached before end of expanded signal
            // path. Mix the rest with a fixed mixing factor.
            if temp_length < current_lag {
                if mix_factor_increment != 0 {
                    parameters.current_voice_mix_factor = parameters.voice_mix_factor;
                }
                let temp_scale: i16 = (16384 - parameters.current_voice_mix_factor as i32) as i16;
                spl::scale_and_add_vectors_with_round(
                    &voiced_vector[temp_length..],
                    parameters.current_voice_mix_factor,
                    &unvoiced_vector[temp_length..],
                    temp_scale,
                    14,
                    &mut temp_data[temp_length..],
                    current_lag - temp_length,
                );
            }

            // Select muting slope depending on how many consecutive expands we have
            // done.
            if self.consecutive_expands == 3 {
                // Let the mute factor decrease from 1.0 to 0.95 in 6.25 ms.
                // mute_slope = 0.0010 / fs_mult in Q20.
                parameters.mute_slope = parameters.mute_slope.max(1049 / fs_mult);
            }
            if self.consecutive_expands == 7 {
                // Let the mute factor decrease from 1.0 to 0.90 in 6.25 ms.
                // mute_slope = 0.0020 / fs_mult in Q20.
                parameters.mute_slope = parameters.mute_slope.max(2097 / fs_mult);
            }

            // Mute segment according to slope value.
            if self.consecutive_expands != 0 || !parameters.onset {
                // Mute to the previous level, then continue with the muting.
                spl::affine_transform_vector_in_place(
                    &mut temp_data,
                    parameters.mute_factor,
                    8192,
                    14,
                    current_lag,
                );

                if !self.stop_muting {
                    DspHelper::mute_signal(&mut temp_data, parameters.mute_slope, current_lag);

                    // Shift by 6 to go from Q20 to Q14.
                    // TODO(hlundin): Adding 8192 before shifting 6 steps seems wrong.
                    // Legacy.
                    // `current_lag * mute_slope` is `size_t` arithmetic in C++
                    // (`mute_slope` converts, sign-extending), so it wraps.
                    let scaled = current_lag
                        .wrapping_mul(parameters.mute_slope as isize as usize)
                        .wrapping_add(8192)
                        >> 6;
                    let mut gain: i16 = 16384usize.wrapping_sub(scaled) as i16;
                    gain = ((gain as i32 * parameters.mute_factor as i32 + 8192) >> 14) as i16;

                    // Guard against getting stuck with very small (but sometimes audible)
                    // gain.
                    if self.consecutive_expands > 3 && gain >= parameters.mute_factor {
                        parameters.mute_factor = 0;
                    } else {
                        parameters.mute_factor = gain;
                    }
                }
            }

            // Background noise part.
            let mute_slope = parameters.mute_slope;
            let too_many_expands = self.consecutive_expands >= Self::K_MAX_CONSECUTIVE_EXPANDS;
            ctx.background_noise.generate_background_noise(
                &random_vector,
                channel_ix,
                mute_slope,
                too_many_expands,
                current_lag,
                &mut unvoiced_array_memory,
            );

            // Add background noise to the combined voiced-unvoiced signal.
            let noise_vector = &unvoiced_array_memory[K_NOISE_LPC_ORDER..];
            for (sample, &noise) in temp_data.iter_mut().zip(noise_vector) {
                *sample = (*sample as i32 + noise as i32) as i16;
            }
            if channel_ix == 0 {
                output.assert_size(current_lag);
            }
            // C++ also has `RTC_DCHECK_EQ(output->Size(), current_lag)` for the
            // other channels. `AssertSize` never shrinks, so a reused `output`
            // longer than `current_lag` trips it; release builds (and the
            // parity harness) ignore it, so it is not a panic here.
            output[channel_ix].overwrite_at_slice(&temp_data[..current_lag], 0);
        }

        // Increase call number and cap it.
        self.consecutive_expands = if self.consecutive_expands >= Self::K_MAX_CONSECUTIVE_EXPANDS {
            Self::K_MAX_CONSECUTIVE_EXPANDS
        } else {
            self.consecutive_expands + 1
        };
        self.expand_duration_samples += output.size();
        // Clamp the duration counter at 2 seconds.
        self.expand_duration_samples = self.expand_duration_samples.min((self.fs_hz * 2) as usize);
        0
    }

    /// Prepares for a Normal operation after expansion (logs the delayed
    /// packet outage and ends the expand event in `statistics`).
    pub fn set_parameters_for_normal_after_expand(
        &mut self,
        statistics: &mut StatisticsCalculator,
    ) {
        self.current_lag_index = 0;
        self.lag_index_direction = 0;
        self.stop_muting = true; // Do not mute signal any more.
        statistics.log_delayed_packet_outage_event(self.expand_duration_samples as i32, self.fs_hz);
        statistics.end_expand_event(self.fs_hz);
    }

    /// Prepares for a Merge operation after expansion (ends the expand event
    /// in `statistics`).
    pub fn set_parameters_for_merge_after_expand(&mut self, statistics: &mut StatisticsCalculator) {
        self.current_lag_index = -1; // out of the 3 possible ones
        self.lag_index_direction = 1; // make sure we get the "optimal" lag
        self.stop_muting = true;
        statistics.end_expand_event(self.fs_hz);
    }

    /// Mute factor (Q14) for `channel`.
    pub fn mute_factor(&self, channel: usize) -> i16 {
        debug_assert!(channel < self.num_channels);
        self.channel_parameters[channel].mute_factor
    }

    /// True if every channel is fully muted.
    pub fn muted(&self) -> bool {
        if self.first_expand || self.stop_muting {
            return false;
        }
        self.channel_parameters.iter().all(|p| p.mute_factor == 0)
    }

    /// Overlap length in samples (5 ms at the current rate).
    pub fn overlap_length(&self) -> usize {
        self.overlap_length
    }

    /// Maximum lag used by the pitch search.
    pub fn max_lag(&self) -> usize {
        self.max_lag
    }

    /// C++ `GenerateRandomVector`.
    fn generate_random_vector(
        random_vector: &mut RandomVector,
        seed_increment: i16,
        length: usize,
        output: &mut [i16],
    ) {
        // TODO(turajs): According to hlundin The loop should not be needed. Should be
        // just as good to generate all of the vector in one call.
        let mut samples_generated = 0;
        let k_max_rand_samples = RandomVector::K_RANDOM_TABLE_SIZE;
        while samples_generated < length {
            let rand_length = (length - samples_generated).min(k_max_rand_samples);
            random_vector.increase_seed_increment(seed_increment);
            random_vector.generate(rand_length, &mut output[samples_generated..]);
            samples_generated += rand_length;
        }
    }

    /// Initializes member variables at the beginning of an expand period.
    fn initialize_for_an_expand_period(&mut self, ctx: &mut ExpandContext<'_>) {
        self.lag_index_direction = 1;
        self.current_lag_index = -1;
        self.stop_muting = false;
        ctx.random_vector.set_seed_increment(1);
        self.consecutive_expands = 0;
        for (ix, parameters) in self.channel_parameters.iter_mut().enumerate() {
            parameters.current_voice_mix_factor = 16384; // 1.0 in Q14.
            parameters.mute_factor = 16384; // 1.0 in Q14.
                                            // Start with 0 gain for background noise.
            ctx.background_noise.set_mute_factor(ix, 0);
        }
    }

    /// Analyzes the signal history in the sync buffer, and sets up all
    /// parameters necessary to produce concealment data.
    fn analyze_signal(&mut self, ctx: &mut ExpandContext<'_>, random_vector: &mut [i16]) {
        let mut auto_correlation = [0i32; K_UNVOICED_LPC_ORDER + 1];
        let mut reflection_coeff = [0i16; K_UNVOICED_LPC_ORDER];
        let mut correlation_vector = [0i16; K_MAX_SAMPLE_RATE / 8000 * 102];
        let mut best_correlation_index = [0usize; K_NUM_CORRELATION_CANDIDATES];
        let mut best_correlation = [0i16; K_NUM_CORRELATION_CANDIDATES];
        let mut best_distortion_index = [0usize; K_NUM_CORRELATION_CANDIDATES];
        let mut best_distortion = [0i16; K_NUM_CORRELATION_CANDIDATES];
        let mut correlation_vector2 = [0i32; (99 * K_MAX_SAMPLE_RATE / 8000) + 1];
        let mut best_distortion_w32 = [0i32; K_NUM_CORRELATION_CANDIDATES];
        // `unvoiced_vector` is `unvoiced_array_memory + kUnvoicedLpcOrder`.
        let mut unvoiced_array_memory = [0i16; K_UNVOICED_ARRAY_SIZE];

        let fs_mult = self.fs_hz / 8000;

        // Pre-calculate common multiplications with fs_mult.
        let fs_mult_4 = (fs_mult * 4) as usize;
        let fs_mult_20 = (fs_mult * 20) as usize;
        let fs_mult_120 = (fs_mult * 120) as usize;
        let fs_mult_dist_len = fs_mult as usize * K_DISTORTION_LENGTH;
        let fs_mult_lpc_analysis_len = fs_mult as usize * K_LPC_ANALYSIS_LENGTH;

        let signal_length = (256 * fs_mult) as usize;

        let audio_history_position = ctx.sync_buffer.size() - signal_length;
        let mut audio_history = vec![0i16; signal_length];
        ctx.sync_buffer[0].copy_to_slice(signal_length, audio_history_position, &mut audio_history);

        // Initialize.
        self.initialize_for_an_expand_period(ctx);

        // Calculate correlation in downsampled domain (4 kHz sample rate).
        let mut correlation_length: usize = 51; // TODO(hlundin): Legacy bit-exactness.
                                                // If it is decided to break bit-exactness `correlation_length` should be
                                                // initialized to the return value of Correlation().
        self.correlation(&audio_history, signal_length, &mut correlation_vector);

        // Find peaks in correlation vector.
        DspHelper::peak_detection(
            &mut correlation_vector,
            correlation_length,
            K_NUM_CORRELATION_CANDIDATES,
            fs_mult,
            &mut best_correlation_index,
            &mut best_correlation,
        );

        // Adjust peak locations; cross-correlation lags start at 2.5 ms
        // (20 * fs_mult samples).
        for index in &mut best_correlation_index {
            *index = index.wrapping_add(fs_mult_20);
        }

        // Calculate distortion around the `kNumCorrelationCandidates` best lags.
        let mut distortion_scale: i32 = 0;
        for i in 0..K_NUM_CORRELATION_CANDIDATES {
            let min_index = fs_mult_20.max(best_correlation_index[i].wrapping_sub(fs_mult_4));
            let max_index =
                (fs_mult_120 - 1).min(best_correlation_index[i].wrapping_add(fs_mult_4));
            best_distortion_index[i] = DspHelper::min_distortion(
                &audio_history,
                signal_length - fs_mult_dist_len,
                min_index,
                max_index,
                fs_mult_dist_len,
                &mut best_distortion_w32[i],
            );
            distortion_scale =
                (16 - spl::norm_w32(best_distortion_w32[i]) as i32).max(distortion_scale);
        }
        // Shift the distortion values to fit in 16 bits.
        spl::vector_bit_shift_w32_to_w16(
            &mut best_distortion,
            K_NUM_CORRELATION_CANDIDATES,
            &best_distortion_w32,
            distortion_scale,
        );

        // Find the maximizing index `i` of the cost function
        // f[i] = best_correlation[i] / best_distortion[i].
        let mut best_ratio = i32::MIN;
        let mut best_index = usize::MAX;
        for i in 0..K_NUM_CORRELATION_CANDIDATES {
            let ratio: i32 = if best_distortion[i] > 0 {
                (best_correlation[i] as i32 * (1 << 16)) / best_distortion[i] as i32
            } else if best_correlation[i] == 0 {
                0 // No correlation set result to zero.
            } else {
                i32::MAX // Denominator is zero.
            };
            if ratio > best_ratio {
                best_index = i;
                best_ratio = ratio;
            }
        }

        let distortion_lag = best_distortion_index[best_index];
        let correlation_lag = best_correlation_index[best_index];
        self.max_lag = distortion_lag.max(correlation_lag);

        // Calculate the exact best correlation in the range between
        // `correlation_lag` and `distortion_lag`.
        correlation_length = (distortion_lag + 10)
            .min(fs_mult_120)
            .max(60 * fs_mult as usize);

        let start_index = distortion_lag.min(correlation_lag);
        let correlation_lags =
            (spl::spl_abs_w16(distortion_lag.wrapping_sub(correlation_lag) as i16) + 1) as usize;
        debug_assert!(correlation_lags <= (99 * fs_mult + 1) as usize);

        for channel_ix in 0..self.num_channels {
            if channel_ix > 0 {
                // When channel_ix == 0, audio_history contains the correct audio. For the
                // other cases, we will have to copy the correct channel into
                // audio_history.
                ctx.sync_buffer[channel_ix].copy_to_slice(
                    signal_length,
                    audio_history_position,
                    &mut audio_history,
                );
            }
            let overlap_length = self.overlap_length;
            let max_lag = self.max_lag;
            let parameters = &mut self.channel_parameters[channel_ix];

            // Calculate suitable scaling.
            let signal_max = spl::max_abs_value_w16(
                &audio_history
                    [signal_length - correlation_length - start_index - correlation_lags..],
                correlation_length + start_index + correlation_lags - 1,
            );
            let mut correlation_scale: i32 = (31
                - spl::norm_w32(signal_max as i32 * signal_max as i32) as i32)
                + (31 - spl::norm_w32(correlation_length as i32) as i32)
                - 31;
            correlation_scale = correlation_scale.max(0);

            // Calculate the correlation, store in `correlation_vector2`.
            spl::cross_correlation(
                &mut correlation_vector2,
                &audio_history[signal_length - correlation_length..],
                &audio_history,
                signal_length - correlation_length - start_index,
                correlation_length,
                correlation_lags,
                correlation_scale,
                -1,
            );

            // Find maximizing index.
            let mut best_index = spl::max_index_w32(&correlation_vector2, correlation_lags);
            let mut max_correlation = correlation_vector2[best_index];
            // Compensate index with start offset.
            best_index += start_index;

            // Calculate energies.
            let tail = &audio_history[signal_length - correlation_length..];
            let mut energy1 =
                spl::dot_product_with_scale(tail, tail, correlation_length, correlation_scale);
            let lagged = &audio_history[signal_length - correlation_length - best_index..];
            let mut energy2 =
                spl::dot_product_with_scale(lagged, lagged, correlation_length, correlation_scale);

            // Calculate the correlation coefficient between the two portions of the
            // signal.
            let corr_coefficient: i32 = if energy1 > 0 && energy2 > 0 {
                let mut energy1_scale = (16 - spl::norm_w32(energy1) as i32).max(0);
                let energy2_scale = (16 - spl::norm_w32(energy2) as i32).max(0);
                // Make sure total scaling is even (to simplify scale factor after sqrt).
                if (energy1_scale + energy2_scale) & 1 != 0 {
                    // If sum is odd, add 1 to make it even.
                    energy1_scale += 1;
                }
                let scaled_energy1 = energy1 >> energy1_scale;
                let scaled_energy2 = energy2 >> energy2_scale;
                let sqrt_energy_product =
                    spl::sqrt_floor(scaled_energy1.wrapping_mul(scaled_energy2)) as i16;
                // Calculate max_correlation / sqrt(energy1 * energy2) in Q14.
                let cc_shift = 14 - (energy1_scale + energy2_scale) / 2;
                max_correlation = spl::spl_shift_w32(max_correlation, cc_shift);
                let corr_coefficient = spl::div_w32_w16(max_correlation, sqrt_energy_product);
                // Cap at 1.0 in Q14.
                corr_coefficient.min(16384)
            } else {
                0
            };

            // Extract the two vectors expand_vector0 and expand_vector1 from
            // `audio_history`.
            let expansion_length = max_lag + overlap_length;
            let vector1_index = signal_length - expansion_length;
            let vector2_index = vector1_index - distortion_lag;
            let vector1 = &audio_history[vector1_index..vector1_index + expansion_length];
            let vector2 = &audio_history[vector2_index..vector2_index + expansion_length];
            // Normalize the second vector to the same energy as the first.
            energy1 =
                spl::dot_product_with_scale(vector1, vector1, expansion_length, correlation_scale);
            energy2 =
                spl::dot_product_with_scale(vector2, vector2, expansion_length, correlation_scale);
            // Confirm that amplitude ratio sqrt(energy1 / energy2) is within 0.5 - 2.0,
            // i.e., energy1 / energy2 is within 0.25 - 4.
            let amplitude_ratio: i16;
            if energy1 / 4 < energy2 && energy1 > energy2 / 4 {
                // Energy constraint fulfilled. Use both vectors and scale them
                // accordingly.
                let scaled_energy2: i32 = (16 - spl::norm_w32(energy2) as i32).max(0);
                let scaled_energy1: i32 = scaled_energy2 - 13;
                // Calculate scaled_energy1 / scaled_energy2 in Q13.
                let energy_ratio = spl::div_w32_w16(
                    spl::spl_shift_w32(energy1, -scaled_energy1),
                    (energy2 >> scaled_energy2) as i16,
                );
                // Calculate sqrt ratio in Q13 (sqrt of en1/en2 in Q26).
                amplitude_ratio = spl::sqrt_floor(energy_ratio.wrapping_shl(13)) as i16;
                // Copy the two vectors and give them the same energy.
                parameters.expand_vector0.clear();
                parameters.expand_vector0.push_back_slice(vector1);
                parameters.expand_vector1.clear();
                if parameters.expand_vector1.size() < expansion_length {
                    parameters
                        .expand_vector1
                        .extend(expansion_length - parameters.expand_vector1.size());
                }
                let mut temp_1 = vec![0i16; expansion_length];
                spl::affine_transform_vector(
                    &mut temp_1,
                    vector2,
                    amplitude_ratio,
                    4096,
                    13,
                    expansion_length,
                );
                parameters.expand_vector1.overwrite_at_slice(&temp_1, 0);
            } else {
                // Energy change constraint not fulfilled. Only use last vector.
                parameters.expand_vector0.clear();
                parameters.expand_vector0.push_back_slice(vector1);
                // Copy from expand_vector0 to expand_vector1.
                parameters
                    .expand_vector0
                    .copy_to(&mut parameters.expand_vector1);
                // Set the energy_ratio since it is used by muting slope.
                if energy1 / 4 < energy2 || energy2 == 0 {
                    amplitude_ratio = 4096; // 0.5 in Q13.
                } else {
                    amplitude_ratio = 16384; // 2.0 in Q13.
                }
            }

            // Set the 3 lag values.
            if distortion_lag == correlation_lag {
                self.expand_lags = [distortion_lag; 3];
            } else {
                // `distortion_lag` and `correlation_lag` are not equal; use different
                // combinations of the two.
                // First lag is `distortion_lag` only.
                self.expand_lags[0] = distortion_lag;
                // Second lag is the average of the two.
                self.expand_lags[1] = (distortion_lag + correlation_lag) / 2;
                // Third lag is the average again, but rounding towards `correlation_lag`.
                if distortion_lag > correlation_lag {
                    self.expand_lags[2] = (distortion_lag + correlation_lag - 1) / 2;
                } else {
                    // C++ `(distortion_lag + correlation_lag + 1) / 2`.
                    self.expand_lags[2] = (distortion_lag + correlation_lag).div_ceil(2);
                }
            }

            // Calculate the LPC and the gain of the filters.

            // Calculate kUnvoicedLpcOrder + 1 lags of the auto-correlation function.
            let temp_index = signal_length - fs_mult_lpc_analysis_len - K_UNVOICED_LPC_ORDER;
            // Copy signal to temporary vector to be able to pad with leading zeros.
            let mut temp_signal = vec![0i16; fs_mult_lpc_analysis_len + K_UNVOICED_LPC_ORDER];
            temp_signal[K_UNVOICED_LPC_ORDER..].copy_from_slice(
                &audio_history[temp_index + K_UNVOICED_LPC_ORDER
                    ..temp_index + K_UNVOICED_LPC_ORDER + fs_mult_lpc_analysis_len],
            );
            cross_correlation_with_auto_shift(
                &temp_signal[K_UNVOICED_LPC_ORDER..],
                &temp_signal,
                K_UNVOICED_LPC_ORDER,
                fs_mult_lpc_analysis_len,
                K_UNVOICED_LPC_ORDER + 1,
                -1,
                &mut auto_correlation,
            );

            // Verify that variance is positive.
            if auto_correlation[0] > 0 {
                // Estimate AR filter parameters using Levinson-Durbin algorithm;
                // kUnvoicedLpcOrder + 1 filter coefficients.
                let stability = spl::levinson_durbin(
                    &auto_correlation,
                    &mut parameters.ar_filter,
                    &mut reflection_coeff,
                    K_UNVOICED_LPC_ORDER,
                );

                // Keep filter parameters only if filter is stable.
                if stability != 1 {
                    // Set first coefficient to 4096 (1.0 in Q12).
                    parameters.ar_filter[0] = 4096;
                    // Set remaining `kUnvoicedLpcOrder` coefficients to zero.
                    spl::mem_set_w16(&mut parameters.ar_filter[1..], 0, K_UNVOICED_LPC_ORDER);
                }
            }

            if channel_ix == 0 {
                // Extract a noise segment.
                let noise_length = if distortion_lag < 40 {
                    2 * distortion_lag + 30
                } else {
                    distortion_lag + 30
                };
                if noise_length <= RandomVector::K_RANDOM_TABLE_SIZE {
                    random_vector[..noise_length]
                        .copy_from_slice(&RandomVector::K_RANDOM_TABLE[..noise_length]);
                } else {
                    // Only applies to SWB where length could be larger than
                    // `kRandomTableSize`.
                    random_vector[..RandomVector::K_RANDOM_TABLE_SIZE]
                        .copy_from_slice(&RandomVector::K_RANDOM_TABLE);
                    debug_assert!(noise_length <= K_RANDOM_VECTOR_SIZE);
                    ctx.random_vector.increase_seed_increment(2);
                    ctx.random_vector.generate(
                        noise_length - RandomVector::K_RANDOM_TABLE_SIZE,
                        &mut random_vector[RandomVector::K_RANDOM_TABLE_SIZE..],
                    );
                }
            }

            // Set up state vector and calculate scale factor for unvoiced filtering.
            parameters
                .ar_filter_state
                .copy_from_slice(&audio_history[signal_length - K_UNVOICED_LPC_ORDER..]);
            unvoiced_array_memory[..K_UNVOICED_LPC_ORDER].copy_from_slice(
                &audio_history[signal_length - 128 - K_UNVOICED_LPC_ORDER..signal_length - 128],
            );
            let unvoiced_vector = &mut unvoiced_array_memory[K_UNVOICED_LPC_ORDER..];
            spl::filter_ma_fast_q12(
                &audio_history,
                signal_length - 128,
                unvoiced_vector,
                &parameters.ar_filter,
                K_UNVOICED_LPC_ORDER + 1,
                128,
            );
            let unvoiced_max_abs: i32 = {
                let max_abs = spl::max_abs_value_w16(unvoiced_vector, 128);
                // Since WebRtcSpl_MaxAbsValueW16 returns 2^15 - 1 when the input contains
                // -2^15, we have to conservatively bump the return value by 1
                // if it is 2^15 - 1.
                if max_abs == spl::WORD16_MAX {
                    max_abs as i32 + 1
                } else {
                    max_abs as i32
                }
            };
            // Pick the smallest n such that 2^n > unvoiced_max_abs; then the maximum
            // value of the dot product is less than 2^7 * 2^(2*n) = 2^(2*n + 7), so to
            // prevent overflows we want 2n + 7 <= 31, which means we should shift by
            // 2n + 7 - 31 bits, if this value is greater than zero.
            let unvoiced_prescale: i32 =
                0.max(2 * spl::get_size_in_bits(unvoiced_max_abs as u32) as i32 - 24);

            let mut unvoiced_energy = spl::dot_product_with_scale(
                unvoiced_vector,
                unvoiced_vector,
                128,
                unvoiced_prescale,
            );

            // Normalize `unvoiced_energy` to 28 or 29 bits to preserve sqrt() accuracy.
            let mut unvoiced_scale: i16 = spl::norm_w32(unvoiced_energy) - 3;
            // Make sure we do an odd number of shifts since we already have 7 shifts
            // from dividing with 128 earlier. This will make the total scale factor
            // even, which is suitable for the sqrt.
            unvoiced_scale += (unvoiced_scale & 0x1) ^ 0x1;
            unvoiced_energy = spl::spl_shift_w32(unvoiced_energy, unvoiced_scale as i32);
            let unvoiced_gain = spl::sqrt_floor(unvoiced_energy) as i16;
            parameters.ar_gain_scale =
                (13 + (unvoiced_scale as i32 + 7 - unvoiced_prescale) / 2) as i16;
            parameters.ar_gain = unvoiced_gain;

            // Calculate voice_mix_factor from corr_coefficient.
            // Let x = corr_coefficient. Then, we compute:
            // if (x > 0.48)
            //   voice_mix_factor = (-5179 + 19931x - 16422x^2 + 5776x^3) / 4096;
            // else
            //   voice_mix_factor = 0;
            if corr_coefficient > 7875 {
                // `corr_coefficient` is in Q14.
                let x1: i16 = corr_coefficient as i16;
                let x2: i16 = ((x1 as i32 * x1 as i32) >> 14) as i16; // Shift 14 to keep result in Q14.
                let x3: i16 = ((x1 as i32 * x2 as i32) >> 14) as i16;
                const K_COEFFICIENTS: [i32; 4] = [-5179, 19931, -16422, 5776];
                let mut temp_sum: i32 = K_COEFFICIENTS[0] * 16384;
                temp_sum += K_COEFFICIENTS[1] * x1 as i32;
                temp_sum += K_COEFFICIENTS[2] * x2 as i32;
                temp_sum += K_COEFFICIENTS[3] * x3 as i32;
                parameters.voice_mix_factor = (temp_sum / 4096).min(16384) as i16;
                parameters.voice_mix_factor = parameters.voice_mix_factor.max(0);
            } else {
                parameters.voice_mix_factor = 0;
            }

            // Calculate muting slope. Reuse value from earlier scaling of
            // `expand_vector0` and `expand_vector1`.
            let slope: i16 = amplitude_ratio;
            if slope > 12288 {
                // slope > 1.5.
                // Calculate (1 - (1 / slope)) / distortion_lag =
                // (slope - 1) / (distortion_lag * slope).
                // `slope` is in Q13, so 1 corresponds to 8192. Shift up to Q25 before
                // the division.
                // Shift the denominator from Q13 to Q5 before the division. The result of
                // the division will then be in Q20.
                // `distortion_lag * slope` is `size_t`; `saturated_cast<int16_t>`
                // of an unsigned value clamps at the top only.
                let denom_wide = distortion_lag.wrapping_mul(slope as isize as usize) >> 8;
                let denom: i16 = denom_wide.min(i16::MAX as usize) as i16;
                let temp_ratio = spl::div_w32_w16((slope as i32 - 8192) << 12, denom);
                if slope > 14746 {
                    // slope > 1.8.
                    // Divide by 2, with proper rounding.
                    parameters.mute_slope = (temp_ratio + 1) / 2;
                } else {
                    // Divide by 8, with proper rounding.
                    parameters.mute_slope = (temp_ratio + 4) / 8;
                }
                parameters.onset = true;
            } else {
                // Calculate (1 - slope) / distortion_lag.
                // Shift `slope` by 7 to Q20 before the division. The result is in Q20.
                parameters.mute_slope =
                    spl::div_w32_w16((8192 - slope as i32) * 128, distortion_lag as i16);
                if parameters.voice_mix_factor <= 13107 {
                    // Make sure the mute factor decreases from 1.0 to 0.9 in no more than
                    // 6.25 ms.
                    // mute_slope >= 0.005 / fs_mult in Q20.
                    parameters.mute_slope = (5243 / fs_mult).max(parameters.mute_slope);
                } else if slope > 8028 {
                    parameters.mute_slope = 0;
                }
                parameters.onset = false;
            }
        }
    }

    /// C++ `Correlation`: auto-correlation of a 4 kHz downsampled version of
    /// `input[..input_length]`, normalized into `output` (54 values).
    fn correlation(&self, input: &[i16], input_length: usize, output: &mut [i16]) {
        // Set parameters depending on sample rate.
        let (filter_coefficients, num_coefficients, downsampling_factor): (&[i16], usize, i16) =
            match self.fs_hz {
                8000 => (&DspHelper::K_DOWNSAMPLE_8KHZ_TBL, 3, 2),
                16000 => (&DspHelper::K_DOWNSAMPLE_16KHZ_TBL, 5, 4),
                32000 => (&DspHelper::K_DOWNSAMPLE_32KHZ_TBL, 7, 8),
                // fs_hz_ == 48000.
                _ => (&DspHelper::K_DOWNSAMPLE_48KHZ_TBL, 7, 12),
            };

        // Correlate from lag 10 to lag 60 in downsampled domain.
        // (Corresponds to 20-120 for narrow-band, 40-240 for wide-band, and so on.)
        const K_CORRELATION_START_LAG: usize = 10;
        const K_NUM_CORRELATION_LAGS: usize = 54;
        const K_CORRELATION_LENGTH: usize = 60;
        // Downsample to 4 kHz sample rate.
        const K_DOWNSAMPLED_LENGTH: usize =
            K_CORRELATION_START_LAG + K_NUM_CORRELATION_LAGS + K_CORRELATION_LENGTH;
        let mut downsampled_input = [0i16; K_DOWNSAMPLED_LENGTH];
        const K_FILTER_DELAY: usize = 0;
        let factor = downsampling_factor as usize;
        spl::downsample_fast(
            input,
            input_length - K_DOWNSAMPLED_LENGTH * factor,
            K_DOWNSAMPLED_LENGTH * factor,
            &mut downsampled_input,
            K_DOWNSAMPLED_LENGTH,
            filter_coefficients,
            num_coefficients,
            downsampling_factor as i32,
            K_FILTER_DELAY,
        );

        // Normalize `downsampled_input` to using all 16 bits.
        let max_value = spl::max_abs_value_w16(&downsampled_input, K_DOWNSAMPLED_LENGTH);
        let norm_shift: i16 = 16 - spl::norm_w32(max_value as i32);
        spl::vector_bit_shift_w16_in_place(
            &mut downsampled_input,
            K_DOWNSAMPLED_LENGTH,
            norm_shift,
        );

        let mut correlation = [0i32; K_NUM_CORRELATION_LAGS];
        cross_correlation_with_auto_shift(
            &downsampled_input[K_DOWNSAMPLED_LENGTH - K_CORRELATION_LENGTH..],
            &downsampled_input,
            K_DOWNSAMPLED_LENGTH - K_CORRELATION_LENGTH - K_CORRELATION_START_LAG,
            K_CORRELATION_LENGTH,
            K_NUM_CORRELATION_LAGS,
            -1,
            &mut correlation,
        );

        // Normalize and move data from 32-bit to 16-bit vector.
        let max_correlation = spl::max_abs_value_w32(&correlation, K_NUM_CORRELATION_LAGS);
        let norm_shift2: i16 = (18 - spl::norm_w32(max_correlation) as i32).max(0) as i16;
        spl::vector_bit_shift_w32_to_w16(
            output,
            K_NUM_CORRELATION_LAGS,
            &correlation,
            norm_shift2 as i32,
        );
    }

    /// C++ `UpdateLagIndex`.
    fn update_lag_index(&mut self) {
        self.current_lag_index += self.lag_index_direction;
        // Change direction if needed.
        if self.current_lag_index <= 0 {
            self.lag_index_direction = 1;
        }
        if self.current_lag_index >= K_NUM_LAGS - 1 {
            self.lag_index_direction = -1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::neteq::expand::golden_tests::speech_like;

    // This is the same size that is given to the SyncBuffer object in NetEq.
    const K_NET_EQ_SYNC_BUFFER_LENGTH_MS: usize = 720;

    /// The C++ fixture components. Instead of
    /// `resources/audio_coding/testfile32kHz.pcm` (not in the sparse clone),
    /// the sync buffer is pre-loaded with a deterministic voiced signal.
    struct Fixture {
        num_channels: usize,
        background_noise: BackgroundNoise,
        sync_buffer: SyncBuffer,
        random_vector: RandomVector,
        statistics: StatisticsCalculator,
        expand: Expand,
    }

    impl Fixture {
        fn new() -> Self {
            let fs = 32000;
            let num_channels = 1;
            let mut sync_buffer = SyncBuffer::new(
                num_channels,
                K_NET_EQ_SYNC_BUFFER_LENGTH_MS * fs as usize / 1000,
            );
            let size = sync_buffer.size();
            let signal = speech_like(size, fs, 1);
            sync_buffer.channel_mut(0).overwrite_at_slice(&signal, 0);
            Self {
                num_channels,
                background_noise: BackgroundNoise::new(num_channels),
                sync_buffer,
                random_vector: RandomVector::new(),
                statistics: StatisticsCalculator::new(),
                expand: Expand::new(fs, num_channels),
            }
        }

        fn process(&mut self, output: &mut AudioMultiVector) -> i32 {
            let mut ctx = ExpandContext {
                sync_buffer: &mut self.sync_buffer,
                background_noise: &mut self.background_noise,
                random_vector: &mut self.random_vector,
                statistics: &mut self.statistics,
            };
            self.expand.process(&mut ctx, output)
        }

        fn last_outage_duration_samples(&self) -> u64 {
            self.statistics
                .get_lifetime_statistics()
                .delayed_packet_outage_samples
        }

        /// Runs expand until `muted()` returns true. Times out after 1000 calls.
        fn expand_until_muted(&mut self) {
            assert!(!self.expand.muted(), "Instance is muted from the start");
            let mut output = AudioMultiVector::new(self.num_channels);
            let mut num_calls = 0;
            while !self.expand.muted() {
                assert!(num_calls < 1000, "Test timed out");
                num_calls += 1;
                assert_eq!(0, self.process(&mut output));
            }
        }
    }

    #[test]
    fn create_and_destroy() {
        let fs = 8000;
        let channels = 1;
        let _bgn = BackgroundNoise::new(channels);
        let _sync_buffer = SyncBuffer::new(1, 1000);
        let _random_vector = RandomVector::new();
        let _statistics = StatisticsCalculator::new();
        let _expand = Expand::new(fs, channels);
    }

    // The C++ test logs through a fake `LogDelayedPacketOutageEvent`; the
    // real calculator accumulates `delayed_packet_outage_samples`, which
    // equals the last logged duration when only one event is logged.
    #[test]
    fn delayed_packet_outage() {
        let mut f = Fixture::new();
        let mut output = AudioMultiVector::new(f.num_channels);
        let mut sum_output_len_samples = 0;
        for _ in 0..10 {
            assert_eq!(0, f.process(&mut output));
            assert!(output.size() > 0);
            sum_output_len_samples += output.size();
            assert_eq!(0, f.last_outage_duration_samples());
        }
        f.expand
            .set_parameters_for_normal_after_expand(&mut f.statistics);
        assert_eq!(
            sum_output_len_samples as u64,
            f.last_outage_duration_samples()
        );
    }

    #[test]
    fn lost_packet_outage() {
        let mut f = Fixture::new();
        let mut output = AudioMultiVector::new(f.num_channels);
        for _ in 0..10 {
            assert_eq!(0, f.process(&mut output));
            assert!(output.size() > 0);
            assert_eq!(0, f.last_outage_duration_samples());
        }
        f.expand
            .set_parameters_for_merge_after_expand(&mut f.statistics);
        assert_eq!(0, f.last_outage_duration_samples());
    }

    #[test]
    fn check_outage_stats_after_reset() {
        let mut f = Fixture::new();
        let mut output = AudioMultiVector::new(f.num_channels);
        let mut sum_output_len_samples = 0;
        for i in 0..10 {
            assert_eq!(0, f.process(&mut output));
            assert!(output.size() > 0);
            sum_output_len_samples += output.size();
            if i == 5 {
                f.expand.reset();
                sum_output_len_samples = 0;
            }
            assert_eq!(0, f.last_outage_duration_samples());
        }
        f.expand
            .set_parameters_for_normal_after_expand(&mut f.statistics);
        assert_eq!(
            sum_output_len_samples as u64,
            f.last_outage_duration_samples()
        );
    }

    #[test]
    fn muted() {
        let mut f = Fixture::new();
        f.expand_until_muted();
        f.expand.reset();
        assert!(!f.expand.muted()); // Should be back to unmuted.

        f.expand_until_muted();
        f.expand
            .set_parameters_for_merge_after_expand(&mut f.statistics);
        assert!(!f.expand.muted()); // Should be back to unmuted.

        f.expand.reset(); // Must reset in order to start a new expand period.
        f.expand_until_muted();
        f.expand
            .set_parameters_for_normal_after_expand(&mut f.statistics);
        assert!(!f.expand.muted()); // Should be back to unmuted.
    }

    #[test]
    fn overlap_length_and_max_lag() {
        for fs in [8000, 16000, 32000, 48000] {
            let expand = Expand::new(fs, 1);
            assert_eq!((5 * fs / 8000) as usize, expand.overlap_length());
            assert_eq!(0, expand.max_lag());
            assert!(!expand.muted());
        }
    }
}

#[cfg(test)]
#[path = "wp2a_golden_tests.rs"]
pub(crate) mod golden_tests;

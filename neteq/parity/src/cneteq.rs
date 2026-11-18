//! Safe wrapper over the C++ reference NetEq (`cpp/neteq_shim.cc`).

use std::os::raw::c_int;
use std::ptr::NonNull;

use crate::error::{HarnessError, Result};
use crate::ffi;
use crate::types::{
    AudioOutput, LifetimeStatistics, NetEqConfig, NetworkStatistics, Operation, OperationsAndState,
    RtpHeader, SpeechType, VadActivity,
};

/// Largest possible output frame: 120 ms @ 48 kHz (`NetEqImpl::kMaxFrameSize`).
const MAX_FRAME_SAMPLES: usize = 5760;

/// Google WebRTC's `NetEqImpl`, driven by a `SimulatedClock`.
///
/// Not `Sync`; `Send` is sound because the handle owns all of its state and
/// NetEq has no thread affinity.
pub struct CNetEq {
    raw: NonNull<ffi::NpNetEq>,
    scratch: Vec<i16>,
}

// SAFETY: the handle exclusively owns the NetEqImpl, its clock and decoder
// factory; nothing is shared with other threads.
unsafe impl Send for CNetEq {}

impl CNetEq {
    /// Creates a NetEq with `config`; the simulated clock starts at
    /// `start_time_ms`.
    pub fn new(config: &NetEqConfig, start_time_ms: i64) -> Result<Self> {
        let c = ffi::NpConfig {
            sample_rate_hz: config.sample_rate_hz,
            max_packets_in_buffer: config.max_packets_in_buffer,
            max_delay_ms: config.max_delay_ms,
            min_delay_ms: config.min_delay_ms,
            enable_fast_accelerate: config.enable_fast_accelerate.into(),
            enable_muted_state: config.enable_muted_state.into(),
            enable_rtx_handling: config.enable_rtx_handling.into(),
            for_test_no_time_stretching: config.for_test_no_time_stretching.into(),
            start_time_ms,
        };
        // SAFETY: `c` is a valid, initialized NpConfig for the call duration.
        let raw = unsafe { ffi::np_neteq_create(&c) };
        let raw = NonNull::new(raw).ok_or(HarnessError::Create)?;
        Ok(Self {
            raw,
            scratch: vec![0; MAX_FRAME_SAMPLES],
        })
    }

    fn ptr(&self) -> *const ffi::NpNetEq {
        self.raw.as_ptr()
    }

    fn mut_ptr(&mut self) -> *mut ffi::NpNetEq {
        self.raw.as_ptr()
    }

    /// Registers L16 (PCM16B) at `sample_rate_hz` with `channels`.
    pub fn register_l16(
        &mut self,
        payload_type: u8,
        sample_rate_hz: u32,
        channels: u8,
    ) -> Result<()> {
        // SAFETY: valid handle.
        let ok = unsafe {
            ffi::np_neteq_register_l16(
                self.mut_ptr(),
                c_int::from(payload_type),
                sample_rate_hz as c_int,
                c_int::from(channels),
            )
        };
        check_registered(ok, "L16")
    }

    /// Registers Opus (48 kHz) decoding to `channels` channels.
    pub fn register_opus(&mut self, payload_type: u8, channels: u8) -> Result<()> {
        // SAFETY: valid handle.
        let ok = unsafe {
            ffi::np_neteq_register_opus(
                self.mut_ptr(),
                c_int::from(payload_type),
                c_int::from(channels),
            )
        };
        check_registered(ok, "opus")
    }

    /// Registers RFC 3389 comfort noise at `sample_rate_hz`.
    pub fn register_cn(&mut self, payload_type: u8, sample_rate_hz: u32) -> Result<()> {
        // SAFETY: valid handle.
        let ok = unsafe {
            ffi::np_neteq_register_cn(
                self.mut_ptr(),
                c_int::from(payload_type),
                sample_rate_hz as c_int,
            )
        };
        check_registered(ok, "CN")
    }

    /// Removes a payload type; returns NetEq's return code.
    pub fn remove_payload_type(&mut self, payload_type: u8) -> i32 {
        // SAFETY: valid handle.
        unsafe { ffi::np_neteq_remove_payload_type(self.mut_ptr(), c_int::from(payload_type)) }
    }

    /// `NetEq::InsertPacket`. Returns NetEq's return code (0 = ok).
    pub fn insert_packet(
        &mut self,
        header: &RtpHeader,
        payload: &[u8],
        receive_time_ms: i64,
    ) -> i32 {
        let h = to_ffi_header(header);
        // SAFETY: `h` and `payload` outlive the call; NetEq copies the payload.
        unsafe {
            ffi::np_neteq_insert_packet(
                self.mut_ptr(),
                &h,
                payload.as_ptr(),
                payload.len(),
                receive_time_ms,
            )
        }
    }

    /// `NetEq::InsertEmptyPacket` (deprecated no-op upstream; forwarded as is).
    pub fn insert_empty_packet(&mut self, header: &RtpHeader) {
        let h = to_ffi_header(header);
        // SAFETY: valid handle and header.
        unsafe { ffi::np_neteq_insert_empty_packet(self.mut_ptr(), &h) }
    }

    /// `NetEq::GetAudio` for one 10 ms frame, plus the operation taken.
    pub fn get_audio(&mut self) -> AudioOutput {
        let mut frame = ffi::NpAudioFrame::default();
        let raw = self.raw.as_ptr();
        // SAFETY: scratch holds MAX_FRAME_SAMPLES, which bounds any frame.
        unsafe {
            ffi::np_neteq_get_audio(
                raw,
                &mut frame,
                self.scratch.as_mut_ptr(),
                self.scratch.len(),
            );
        }
        let total = (frame.samples_per_channel as usize * frame.num_channels as usize)
            .min(self.scratch.len());
        AudioOutput {
            samples: self.scratch[..total].to_vec(),
            samples_per_channel: frame.samples_per_channel as usize,
            num_channels: frame.num_channels as usize,
            sample_rate_hz: frame.sample_rate_hz,
            speech_type: SpeechType::from_raw(frame.speech_type),
            vad_activity: VadActivity::from_raw(frame.vad_activity),
            muted: frame.muted != 0,
            operation: Operation::from_raw(frame.operation),
            playout_timestamp: (frame.has_playout_timestamp != 0)
                .then_some(frame.playout_timestamp),
            return_code: frame.return_code,
        }
    }

    /// Advances the simulated clock.
    pub fn advance_time_ms(&mut self, ms: i64) {
        // SAFETY: valid handle.
        unsafe { ffi::np_neteq_advance_time_ms(self.mut_ptr(), ms) }
    }

    /// Current simulated time.
    pub fn now_ms(&self) -> i64 {
        // SAFETY: valid handle.
        unsafe { ffi::np_neteq_now_ms(self.ptr()) }
    }

    /// `NetworkStatistics` (`reset = true`) or `CurrentNetworkStatistics`.
    pub fn network_statistics(&mut self, reset: bool) -> NetworkStatistics {
        let mut s = ffi::NpNetworkStats::default();
        // SAFETY: valid handle and out pointer.
        unsafe { ffi::np_neteq_network_statistics(self.mut_ptr(), &mut s, c_int::from(reset)) };
        NetworkStatistics {
            current_buffer_size_ms: s.current_buffer_size_ms,
            preferred_buffer_size_ms: s.preferred_buffer_size_ms,
            jitter_peaks_found: s.jitter_peaks_found,
            expand_rate: s.expand_rate,
            speech_expand_rate: s.speech_expand_rate,
            preemptive_rate: s.preemptive_rate,
            accelerate_rate: s.accelerate_rate,
            secondary_decoded_rate: s.secondary_decoded_rate,
            secondary_discarded_rate: s.secondary_discarded_rate,
            mean_waiting_time_ms: s.mean_waiting_time_ms,
            median_waiting_time_ms: s.median_waiting_time_ms,
            min_waiting_time_ms: s.min_waiting_time_ms,
            max_waiting_time_ms: s.max_waiting_time_ms,
        }
    }

    /// `GetLifetimeStatistics`.
    pub fn lifetime_statistics(&self) -> LifetimeStatistics {
        let mut s = ffi::NpLifetimeStats::default();
        // SAFETY: valid handle and out pointer.
        unsafe { ffi::np_neteq_lifetime_statistics(self.ptr(), &mut s) };
        LifetimeStatistics {
            total_samples_received: s.total_samples_received,
            concealed_samples: s.concealed_samples,
            concealment_events: s.concealment_events,
            jitter_buffer_delay_ms: s.jitter_buffer_delay_ms,
            jitter_buffer_emitted_count: s.jitter_buffer_emitted_count,
            jitter_buffer_target_delay_ms: s.jitter_buffer_target_delay_ms,
            jitter_buffer_minimum_delay_ms: s.jitter_buffer_minimum_delay_ms,
            inserted_samples_for_deceleration: s.inserted_samples_for_deceleration,
            removed_samples_for_acceleration: s.removed_samples_for_acceleration,
            silent_concealed_samples: s.silent_concealed_samples,
            fec_packets_received: s.fec_packets_received,
            fec_packets_discarded: s.fec_packets_discarded,
            packets_discarded: s.packets_discarded,
            delayed_packet_outage_samples: s.delayed_packet_outage_samples,
            delayed_packet_outage_events: s.delayed_packet_outage_events,
            relative_packet_arrival_delay_ms: s.relative_packet_arrival_delay_ms,
            jitter_buffer_packets_received: s.jitter_buffer_packets_received,
            interruption_count: s.interruption_count,
            total_interruption_duration_ms: s.total_interruption_duration_ms,
            generated_noise_samples: s.generated_noise_samples,
            total_processing_delay_us: s.total_processing_delay_us,
        }
    }

    /// `GetOperationsAndState`.
    pub fn operations_and_state(&self) -> OperationsAndState {
        let mut s = ffi::NpOperationsAndState::default();
        // SAFETY: valid handle and out pointer.
        unsafe { ffi::np_neteq_operations_and_state(self.ptr(), &mut s) };
        OperationsAndState {
            preemptive_samples: s.preemptive_samples,
            accelerate_samples: s.accelerate_samples,
            packet_buffer_flushes: s.packet_buffer_flushes,
            last_waiting_time_ms: s.last_waiting_time_ms,
            current_buffer_size_ms: s.current_buffer_size_ms,
            current_frame_size_ms: s.current_frame_size_ms,
            next_packet_available: s.next_packet_available != 0,
        }
    }

    /// `TargetDelayMs`.
    pub fn target_delay_ms(&self) -> i32 {
        // SAFETY: valid handle.
        unsafe { ffi::np_neteq_target_delay_ms(self.ptr()) }
    }

    /// `FilteredCurrentDelayMs`.
    pub fn filtered_current_delay_ms(&self) -> i32 {
        // SAFETY: valid handle.
        unsafe { ffi::np_neteq_filtered_current_delay_ms(self.ptr()) }
    }

    /// `SetBaseMinimumDelayMs`.
    pub fn set_base_minimum_delay_ms(&mut self, delay_ms: i32) -> bool {
        // SAFETY: valid handle.
        unsafe { ffi::np_neteq_set_base_minimum_delay_ms(self.mut_ptr(), delay_ms) != 0 }
    }

    /// `GetBaseMinimumDelayMs`.
    pub fn base_minimum_delay_ms(&self) -> i32 {
        // SAFETY: valid handle.
        unsafe { ffi::np_neteq_get_base_minimum_delay_ms(self.ptr()) }
    }

    /// `SetMinimumDelay`.
    pub fn set_minimum_delay_ms(&mut self, delay_ms: i32) -> bool {
        // SAFETY: valid handle.
        unsafe { ffi::np_neteq_set_minimum_delay_ms(self.mut_ptr(), delay_ms) != 0 }
    }

    /// `SetMaximumDelay`.
    pub fn set_maximum_delay_ms(&mut self, delay_ms: i32) -> bool {
        // SAFETY: valid handle.
        unsafe { ffi::np_neteq_set_maximum_delay_ms(self.mut_ptr(), delay_ms) != 0 }
    }

    /// `SyncBufferSizeMs`.
    pub fn sync_buffer_size_ms(&self) -> i32 {
        // SAFETY: valid handle.
        unsafe { ffi::np_neteq_sync_buffer_size_ms(self.ptr()) }
    }

    /// `last_output_sample_rate_hz`.
    pub fn last_output_sample_rate_hz(&self) -> i32 {
        // SAFETY: valid handle.
        unsafe { ffi::np_neteq_last_output_sample_rate_hz(self.ptr()) }
    }

    /// `FlushBuffers`.
    pub fn flush(&mut self) {
        // SAFETY: valid handle.
        unsafe { ffi::np_neteq_flush(self.mut_ptr()) }
    }
}

impl Drop for CNetEq {
    fn drop(&mut self) {
        // SAFETY: created by np_neteq_create, destroyed exactly once.
        unsafe { ffi::np_neteq_destroy(self.raw.as_ptr()) }
    }
}

fn check_registered(ok: c_int, codec: &'static str) -> Result<()> {
    if ok == 1 {
        Ok(())
    } else {
        Err(HarnessError::Register(codec))
    }
}

fn to_ffi_header(h: &RtpHeader) -> ffi::NpRtpHeader {
    ffi::NpRtpHeader {
        payload_type: h.payload_type,
        marker: h.marker.into(),
        sequence_number: h.sequence_number,
        timestamp: h.timestamp,
        ssrc: h.ssrc,
    }
}

/// libopus encoder (the same library the reference decoder links), used to
/// produce deterministic Opus traces.
pub struct OpusEncoder {
    raw: NonNull<ffi::NpOpusEncoder>,
    channels: usize,
    out: Vec<u8>,
}

// SAFETY: exclusive owner of the libopus encoder state.
unsafe impl Send for OpusEncoder {}

/// `OPUS_APPLICATION_VOIP`.
pub const OPUS_APPLICATION_VOIP: i32 = 2048;

impl OpusEncoder {
    /// 48 kHz encoder with the given channel count, bitrate, complexity and
    /// DTX setting.
    pub fn new(channels: u8, bitrate_bps: i32, complexity: i32, dtx: bool) -> Result<Self> {
        // SAFETY: plain values.
        let raw = unsafe {
            ffi::np_opus_encoder_create(
                48_000,
                c_int::from(channels),
                OPUS_APPLICATION_VOIP,
                bitrate_bps,
                complexity,
                c_int::from(dtx),
            )
        };
        let raw = NonNull::new(raw).ok_or(HarnessError::OpusEncoder(-1))?;
        Ok(Self {
            raw,
            channels: channels as usize,
            out: vec![0; 4000],
        })
    }

    /// Encodes one frame of interleaved 48 kHz PCM. Returns the packet;
    /// one or two bytes means a DTX frame.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        let frame = pcm.len() / self.channels;
        // SAFETY: `pcm` holds frame*channels samples; `out` holds out.len().
        let n = unsafe {
            ffi::np_opus_encode(
                self.raw.as_ptr(),
                pcm.as_ptr(),
                frame as c_int,
                self.out.as_mut_ptr(),
                self.out.len() as c_int,
            )
        };
        if n < 0 {
            return Err(HarnessError::OpusEncoder(n));
        }
        Ok(self.out[..n as usize].to_vec())
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        // SAFETY: created by np_opus_encoder_create, destroyed exactly once.
        unsafe { ffi::np_opus_encoder_destroy(self.raw.as_ptr()) }
    }
}

/// WebRTC's RFC 3389 `ComfortNoiseEncoder`, used to make SID payloads.
pub struct CngEncoder {
    raw: NonNull<ffi::NpCngEncoder>,
}

// SAFETY: exclusive owner of the encoder state.
unsafe impl Send for CngEncoder {}

impl CngEncoder {
    /// `sample_rate_hz` must be 8000 or 16000; `quality` 1..=12 reflection
    /// coefficients.
    pub fn new(sample_rate_hz: u32, sid_interval_ms: i32, quality: i32) -> Result<Self> {
        // SAFETY: plain values.
        let raw = unsafe {
            ffi::np_cng_encoder_create(sample_rate_hz as c_int, sid_interval_ms, quality)
        };
        let raw = NonNull::new(raw).ok_or(HarnessError::Register("CN encoder"))?;
        Ok(Self { raw })
    }

    /// Analyzes mono `pcm` (<= 640 samples); returns a SID payload when one
    /// is due or forced.
    pub fn encode(&mut self, pcm: &[i16], force_sid: bool) -> Option<Vec<u8>> {
        let mut out = [0u8; 64];
        // SAFETY: `pcm` and `out` are valid for their stated lengths.
        let n = unsafe {
            ffi::np_cng_encode(
                self.raw.as_ptr(),
                pcm.as_ptr(),
                pcm.len(),
                c_int::from(force_sid),
                out.as_mut_ptr(),
                out.len(),
            )
        };
        (n > 0).then(|| out[..n as usize].to_vec())
    }
}

impl Drop for CngEncoder {
    fn drop(&mut self) {
        // SAFETY: created by np_cng_encoder_create, destroyed exactly once.
        unsafe { ffi::np_cng_encoder_destroy(self.raw.as_ptr()) }
    }
}

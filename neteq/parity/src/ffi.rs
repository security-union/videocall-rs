//! Raw bindings to `cpp/neteq_shim.h`. Keep the two files in sync by hand;
//! the surface is small and plain C.

#![allow(missing_docs)]

use std::os::raw::c_int;

#[repr(C)]
pub struct NpNetEq {
    _private: [u8; 0],
}

#[repr(C)]
pub struct NpOpusEncoder {
    _private: [u8; 0],
}

#[repr(C)]
pub struct NpCngEncoder {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NpConfig {
    pub sample_rate_hz: i32,
    pub max_packets_in_buffer: u32,
    pub max_delay_ms: i32,
    pub min_delay_ms: i32,
    pub enable_fast_accelerate: u8,
    pub enable_muted_state: u8,
    pub enable_rtx_handling: u8,
    pub for_test_no_time_stretching: u8,
    pub start_time_ms: i64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NpRtpHeader {
    pub payload_type: u8,
    pub marker: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NpAudioFrame {
    pub samples_per_channel: u32,
    pub num_channels: u32,
    pub sample_rate_hz: i32,
    pub speech_type: i32,
    pub vad_activity: i32,
    pub muted: u8,
    pub operation: i32,
    pub has_playout_timestamp: u8,
    pub playout_timestamp: u32,
    pub return_code: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NpNetworkStats {
    pub current_buffer_size_ms: u16,
    pub preferred_buffer_size_ms: u16,
    pub jitter_peaks_found: u16,
    pub expand_rate: u16,
    pub speech_expand_rate: u16,
    pub preemptive_rate: u16,
    pub accelerate_rate: u16,
    pub secondary_decoded_rate: u16,
    pub secondary_discarded_rate: u16,
    pub mean_waiting_time_ms: i32,
    pub median_waiting_time_ms: i32,
    pub min_waiting_time_ms: i32,
    pub max_waiting_time_ms: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NpLifetimeStats {
    pub total_samples_received: u64,
    pub concealed_samples: u64,
    pub concealment_events: u64,
    pub jitter_buffer_delay_ms: u64,
    pub jitter_buffer_emitted_count: u64,
    pub jitter_buffer_target_delay_ms: u64,
    pub jitter_buffer_minimum_delay_ms: u64,
    pub inserted_samples_for_deceleration: u64,
    pub removed_samples_for_acceleration: u64,
    pub silent_concealed_samples: u64,
    pub fec_packets_received: u64,
    pub fec_packets_discarded: u64,
    pub packets_discarded: u64,
    pub delayed_packet_outage_samples: u64,
    pub delayed_packet_outage_events: u64,
    pub relative_packet_arrival_delay_ms: u64,
    pub jitter_buffer_packets_received: u64,
    pub interruption_count: i32,
    pub total_interruption_duration_ms: i32,
    pub generated_noise_samples: u64,
    pub total_processing_delay_us: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NpOperationsAndState {
    pub preemptive_samples: u64,
    pub accelerate_samples: u64,
    pub packet_buffer_flushes: u64,
    pub last_waiting_time_ms: u64,
    pub current_buffer_size_ms: u64,
    pub current_frame_size_ms: u64,
    pub next_packet_available: u8,
}

extern "C" {
    pub fn np_struct_size(which: c_int) -> usize;
    pub fn np_neteq_create(config: *const NpConfig) -> *mut NpNetEq;
    pub fn np_neteq_destroy(neteq: *mut NpNetEq);

    pub fn np_neteq_register_l16(
        neteq: *mut NpNetEq,
        payload_type: c_int,
        sample_rate_hz: c_int,
        channels: c_int,
    ) -> c_int;
    pub fn np_neteq_register_opus(
        neteq: *mut NpNetEq,
        payload_type: c_int,
        channels: c_int,
    ) -> c_int;
    pub fn np_neteq_register_cn(
        neteq: *mut NpNetEq,
        payload_type: c_int,
        sample_rate_hz: c_int,
    ) -> c_int;
    pub fn np_neteq_remove_payload_type(neteq: *mut NpNetEq, payload_type: c_int) -> c_int;

    pub fn np_neteq_insert_packet(
        neteq: *mut NpNetEq,
        header: *const NpRtpHeader,
        payload: *const u8,
        payload_len: usize,
        receive_time_ms: i64,
    ) -> c_int;
    pub fn np_neteq_insert_empty_packet(neteq: *mut NpNetEq, header: *const NpRtpHeader);

    pub fn np_neteq_get_audio(
        neteq: *mut NpNetEq,
        frame: *mut NpAudioFrame,
        out_samples: *mut i16,
        capacity: usize,
    ) -> c_int;

    pub fn np_neteq_advance_time_ms(neteq: *mut NpNetEq, ms: i64);
    pub fn np_neteq_now_ms(neteq: *const NpNetEq) -> i64;

    pub fn np_neteq_network_statistics(
        neteq: *mut NpNetEq,
        out: *mut NpNetworkStats,
        reset: c_int,
    ) -> c_int;
    pub fn np_neteq_lifetime_statistics(neteq: *const NpNetEq, out: *mut NpLifetimeStats);
    pub fn np_neteq_operations_and_state(neteq: *const NpNetEq, out: *mut NpOperationsAndState);

    pub fn np_neteq_target_delay_ms(neteq: *const NpNetEq) -> c_int;
    pub fn np_neteq_filtered_current_delay_ms(neteq: *const NpNetEq) -> c_int;
    pub fn np_neteq_set_base_minimum_delay_ms(neteq: *mut NpNetEq, delay_ms: c_int) -> c_int;
    pub fn np_neteq_get_base_minimum_delay_ms(neteq: *const NpNetEq) -> c_int;
    pub fn np_neteq_set_minimum_delay_ms(neteq: *mut NpNetEq, delay_ms: c_int) -> c_int;
    pub fn np_neteq_set_maximum_delay_ms(neteq: *mut NpNetEq, delay_ms: c_int) -> c_int;
    pub fn np_neteq_sync_buffer_size_ms(neteq: *const NpNetEq) -> c_int;
    pub fn np_neteq_last_output_sample_rate_hz(neteq: *const NpNetEq) -> c_int;
    pub fn np_neteq_flush(neteq: *mut NpNetEq);

    pub fn np_opus_encoder_create(
        sample_rate_hz: c_int,
        channels: c_int,
        application: c_int,
        bitrate_bps: c_int,
        complexity: c_int,
        use_dtx: c_int,
    ) -> *mut NpOpusEncoder;
    pub fn np_opus_encoder_destroy(encoder: *mut NpOpusEncoder);
    pub fn np_opus_encode(
        encoder: *mut NpOpusEncoder,
        pcm: *const i16,
        frame_size_per_channel: c_int,
        out: *mut u8,
        max_out: c_int,
    ) -> c_int;

    pub fn np_cng_encoder_create(
        sample_rate_hz: c_int,
        sid_interval_ms: c_int,
        quality: c_int,
    ) -> *mut NpCngEncoder;
    pub fn np_cng_encoder_destroy(encoder: *mut NpCngEncoder);
    pub fn np_cng_encode(
        encoder: *mut NpCngEncoder,
        pcm: *const i16,
        num_samples: usize,
        force_sid: c_int,
        out: *mut u8,
        max_out: usize,
    ) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    // Catch drift between cpp/neteq_shim.h and these mirrors.
    #[test]
    fn struct_sizes_match_c_layout() {
        let rust = [
            size_of::<NpConfig>(),
            size_of::<NpRtpHeader>(),
            size_of::<NpAudioFrame>(),
            size_of::<NpNetworkStats>(),
            size_of::<NpLifetimeStats>(),
            size_of::<NpOperationsAndState>(),
        ];
        for (id, expected) in rust.iter().enumerate() {
            // SAFETY: pure function, no pointers.
            let c = unsafe { np_struct_size(id as c_int) };
            assert_eq!(c, *expected, "struct id {id} differs between C and Rust");
        }
    }
}

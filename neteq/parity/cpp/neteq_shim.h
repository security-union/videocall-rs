/*
 * C ABI around Google WebRTC's NetEq (webrtc::NetEqImpl), used by the Rust
 * `neteq-parity` harness as the reference implementation.
 *
 * Every function is plain C, takes POD structs, and never throws. A handle
 * owns one NetEqImpl, its SimulatedClock and its decoder factory.
 *
 * Mirrored 1:1 by `src/ffi.rs`; keep the two in sync.
 */
#ifndef NETEQ_PARITY_SHIM_H_
#define NETEQ_PARITY_SHIM_H_

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct NpNetEq NpNetEq;
typedef struct NpOpusEncoder NpOpusEncoder;
typedef struct NpCngEncoder NpCngEncoder;

/* Mirrors webrtc::NetEq::Config. */
typedef struct NpConfig {
  int32_t sample_rate_hz;
  uint32_t max_packets_in_buffer;
  int32_t max_delay_ms;
  int32_t min_delay_ms;
  uint8_t enable_fast_accelerate;
  uint8_t enable_muted_state;
  uint8_t enable_rtx_handling;
  uint8_t for_test_no_time_stretching;
  /* Initial value of the SimulatedClock, in ms. */
  int64_t start_time_ms;
} NpConfig;

typedef struct NpRtpHeader {
  uint8_t payload_type;
  uint8_t marker;
  uint16_t sequence_number;
  uint32_t timestamp;
  uint32_t ssrc;
} NpRtpHeader;

/* One 10 ms output frame. `samples` is written into the caller's buffer. */
typedef struct NpAudioFrame {
  uint32_t samples_per_channel;
  uint32_t num_channels;
  int32_t sample_rate_hz;
  /* webrtc::AudioFrame::SpeechType numeric value (0 normal, 1 PLC, 2 CNG,
   * 3 PLCCNG, 4 undefined, 5 codec PLC). */
  int32_t speech_type;
  /* webrtc::AudioFrame::VADActivity (0 active, 1 passive, 2 unknown). */
  int32_t vad_activity;
  uint8_t muted;
  /* webrtc::NetEq::Operation numeric value of the decision NetEq took for
   * this frame (NetEqImpl::last_operation_for_test()). */
  int32_t operation;
  /* 1 if a playout timestamp is available, then `playout_timestamp`. */
  uint8_t has_playout_timestamp;
  uint32_t playout_timestamp;
  /* Return code of NetEq::GetAudio (0 = kOK, -1 = kFail). */
  int32_t return_code;
} NpAudioFrame;

/* Mirrors webrtc::NetEqNetworkStatistics. */
typedef struct NpNetworkStats {
  uint16_t current_buffer_size_ms;
  uint16_t preferred_buffer_size_ms;
  uint16_t jitter_peaks_found;
  uint16_t expand_rate;
  uint16_t speech_expand_rate;
  uint16_t preemptive_rate;
  uint16_t accelerate_rate;
  uint16_t secondary_decoded_rate;
  uint16_t secondary_discarded_rate;
  int32_t mean_waiting_time_ms;
  int32_t median_waiting_time_ms;
  int32_t min_waiting_time_ms;
  int32_t max_waiting_time_ms;
} NpNetworkStats;

/* Mirrors webrtc::NetEqLifetimeStatistics. */
typedef struct NpLifetimeStats {
  uint64_t total_samples_received;
  uint64_t concealed_samples;
  uint64_t concealment_events;
  uint64_t jitter_buffer_delay_ms;
  uint64_t jitter_buffer_emitted_count;
  uint64_t jitter_buffer_target_delay_ms;
  uint64_t jitter_buffer_minimum_delay_ms;
  uint64_t inserted_samples_for_deceleration;
  uint64_t removed_samples_for_acceleration;
  uint64_t silent_concealed_samples;
  uint64_t fec_packets_received;
  uint64_t fec_packets_discarded;
  uint64_t packets_discarded;
  uint64_t delayed_packet_outage_samples;
  uint64_t delayed_packet_outage_events;
  uint64_t relative_packet_arrival_delay_ms;
  uint64_t jitter_buffer_packets_received;
  int32_t interruption_count;
  int32_t total_interruption_duration_ms;
  uint64_t generated_noise_samples;
  uint64_t total_processing_delay_us;
} NpLifetimeStats;

/* Mirrors webrtc::NetEqOperationsAndState. */
typedef struct NpOperationsAndState {
  uint64_t preemptive_samples;
  uint64_t accelerate_samples;
  uint64_t packet_buffer_flushes;
  uint64_t last_waiting_time_ms;
  uint64_t current_buffer_size_ms;
  uint64_t current_frame_size_ms;
  uint8_t next_packet_available;
} NpOperationsAndState;

/* sizeof() of the structs above, for ABI checks from the binding side:
 * 0 NpConfig, 1 NpRtpHeader, 2 NpAudioFrame, 3 NpNetworkStats,
 * 4 NpLifetimeStats, 5 NpOperationsAndState. Unknown ids return 0. */
size_t np_struct_size(int which);

/* Lifecycle. Returns NULL on failure. */
NpNetEq* np_neteq_create(const NpConfig* config);
void np_neteq_destroy(NpNetEq* neteq);

/* Decoder registration. Return 1 on success, 0 on failure.
 * L16 (PCM16B) supports 8000/16000/32000/48000 Hz, 1 or 2 channels.
 * Opus is always 48 kHz on the wire; `channels` is the decode channel count
 * (SDP "stereo" parameter). CN is RFC 3389 comfort noise. */
int np_neteq_register_l16(NpNetEq* neteq, int payload_type, int sample_rate_hz,
                          int channels);
int np_neteq_register_opus(NpNetEq* neteq, int payload_type, int channels);
int np_neteq_register_cn(NpNetEq* neteq, int payload_type, int sample_rate_hz);
int np_neteq_remove_payload_type(NpNetEq* neteq, int payload_type);

/* Packet input. `receive_time_ms` is recorded in the RtpPacketInfo. NetEq's
 * own arrival bookkeeping runs off its TickTimer (advanced by GetAudio), so
 * the caller should advance the clock in lockstep with get_audio. */
int np_neteq_insert_packet(NpNetEq* neteq, const NpRtpHeader* header,
                           const uint8_t* payload, size_t payload_len,
                           int64_t receive_time_ms);
/* NetEq::InsertEmptyPacket is deprecated and a no-op at the pinned rev; this
 * forwards to it anyway so callers see the exact upstream behavior. */
void np_neteq_insert_empty_packet(NpNetEq* neteq, const NpRtpHeader* header);

/* Pulls one 10 ms frame. `out_samples` must hold at least `capacity`
 * interleaved int16 samples (5760 is always enough: 120 ms @ 48 kHz). */
int np_neteq_get_audio(NpNetEq* neteq, NpAudioFrame* frame,
                       int16_t* out_samples, size_t capacity);

/* SimulatedClock. */
void np_neteq_advance_time_ms(NpNetEq* neteq, int64_t ms);
int64_t np_neteq_now_ms(const NpNetEq* neteq);

/* Statistics. `reset` = 1 calls NetEq::NetworkStatistics (resets the
 * per-interval counters), 0 calls CurrentNetworkStatistics. */
int np_neteq_network_statistics(NpNetEq* neteq, NpNetworkStats* out,
                                int reset);
void np_neteq_lifetime_statistics(const NpNetEq* neteq, NpLifetimeStats* out);
void np_neteq_operations_and_state(const NpNetEq* neteq,
                                   NpOperationsAndState* out);

/* Delay control and queries. */
int np_neteq_target_delay_ms(const NpNetEq* neteq);
int np_neteq_filtered_current_delay_ms(const NpNetEq* neteq);
int np_neteq_set_base_minimum_delay_ms(NpNetEq* neteq, int delay_ms);
int np_neteq_get_base_minimum_delay_ms(const NpNetEq* neteq);
int np_neteq_set_minimum_delay_ms(NpNetEq* neteq, int delay_ms);
int np_neteq_set_maximum_delay_ms(NpNetEq* neteq, int delay_ms);
int np_neteq_sync_buffer_size_ms(const NpNetEq* neteq);
int np_neteq_last_output_sample_rate_hz(const NpNetEq* neteq);
void np_neteq_flush(NpNetEq* neteq);

/* libopus encoder used to generate deterministic Opus traces.
 * application: 2048 VOIP, 2049 AUDIO, 2051 RESTRICTED_LOWDELAY. */
NpOpusEncoder* np_opus_encoder_create(int sample_rate_hz, int channels,
                                      int application, int bitrate_bps,
                                      int complexity, int use_dtx);
void np_opus_encoder_destroy(NpOpusEncoder* encoder);
/* Returns the packet length, 0..=2 means DTX/empty frame, <0 is an opus
 * error code. */
int np_opus_encode(NpOpusEncoder* encoder, const int16_t* pcm,
                   int frame_size_per_channel, uint8_t* out, int max_out);

/* WebRTC's RFC 3389 ComfortNoiseEncoder, used to produce realistic SID
 * payloads for DTX traces. `sample_rate_hz` is 8000 or 16000. */
NpCngEncoder* np_cng_encoder_create(int sample_rate_hz, int sid_interval_ms,
                                    int quality);
void np_cng_encoder_destroy(NpCngEncoder* encoder);
/* Analyzes `num_samples` mono samples (<= 640). Writes a SID payload and
 * returns its length when one is due (or forced), otherwise returns 0. */
int np_cng_encode(NpCngEncoder* encoder, const int16_t* pcm,
                  size_t num_samples, int force_sid, uint8_t* out,
                  size_t max_out);

#ifdef __cplusplus
}  // extern "C"
#endif

#endif  // NETEQ_PARITY_SHIM_H_

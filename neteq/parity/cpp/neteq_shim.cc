/*
 * C ABI shim around webrtc::NetEqImpl. See neteq_shim.h.
 *
 * Design notes:
 * - We construct NetEqImpl directly (exactly what DefaultNetEqFactory does:
 *   NetEqImpl(config, Dependencies(env, config, factory,
 *   DefaultNetEqControllerFactory()))) so we can call the public
 *   NetEqImpl::last_operation_for_test() accessor. No WebRTC source is
 *   patched.
 * - The Environment carries a SimulatedClock, an empty field-trial set, a
 *   null event log and a task-queue factory that is never used by NetEq.
 * - Decoders come from a minimal AudioDecoderFactory that only knows L16
 *   (AudioDecoderPcm16B) and Opus (AudioDecoderOpusImpl over libopus). CN
 *   and telephone-event are handled inside NetEq's DecoderDatabase.
 */
#include "neteq_shim.h"

#include <opus.h>

#include <algorithm>
#include <cstring>
#include <map>
#include <memory>
#include <optional>
#include <span>
#include <string>
#include <utility>
#include <vector>

#include "absl/strings/match.h"
#include "absl/strings/string_view.h"
#include "api/audio/audio_frame.h"
#include "api/audio_codecs/audio_decoder.h"
#include "api/audio_codecs/audio_decoder_factory.h"
#include "api/audio_codecs/audio_format.h"
#include "api/environment/environment.h"
#include "api/environment/environment_factory.h"
#include "api/field_trials_view.h"
#include "api/make_ref_counted.h"
#include "api/neteq/default_neteq_controller_factory.h"
#include "api/neteq/neteq.h"
#include "api/rtc_event_log/rtc_event_log.h"
#include "api/rtp_headers.h"
#include "api/rtp_packet_info.h"
#include "api/scoped_refptr.h"
#include "api/task_queue/task_queue_factory.h"
#include "api/units/timestamp.h"
#include "rtc_base/buffer.h"
#include "modules/audio_coding/codecs/cng/webrtc_cng.h"
#include "modules/audio_coding/codecs/opus/audio_decoder_opus.h"
#include "modules/audio_coding/codecs/pcm16b/audio_decoder_pcm16b.h"
#include "modules/audio_coding/neteq/neteq_impl.h"
#include "system_wrappers/include/clock.h"

namespace {

using webrtc::AudioDecoder;
using webrtc::AudioDecoderFactory;
using webrtc::SdpAudioFormat;

// Empty field-trial set: every lookup returns "", i.e. upstream defaults.
class EmptyFieldTrials : public webrtc::FieldTrialsView {
 public:
  std::string Lookup(absl::string_view) const override { return ""; }
  bool IsTest() const override { return true; }
};

// NetEq never creates task queues; this exists only to satisfy Environment.
class NoTaskQueueFactory : public webrtc::TaskQueueFactory {
 public:
  std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter>
  CreateTaskQueue(absl::string_view, Priority) const override {
    return nullptr;
  }
};

class ParityDecoderFactory : public AudioDecoderFactory {
 public:
  std::vector<webrtc::AudioCodecSpec> GetSupportedDecoders() override {
    return {};
  }

  bool IsSupportedDecoder(const SdpAudioFormat& format) override {
    return absl::EqualsIgnoreCase(format.name, "L16") ||
           absl::EqualsIgnoreCase(format.name, "opus");
  }

  std::unique_ptr<AudioDecoder> Create(const webrtc::Environment& env,
                                       const SdpAudioFormat& format) override {
    if (absl::EqualsIgnoreCase(format.name, "L16")) {
      if (format.num_channels < 1 || format.num_channels > 24) return nullptr;
      switch (format.clockrate_hz) {
        case 8000:
        case 16000:
        case 32000:
        case 48000:
          return std::make_unique<webrtc::AudioDecoderPcm16B>(
              format.clockrate_hz, format.num_channels);
        default:
          return nullptr;
      }
    }
    if (absl::EqualsIgnoreCase(format.name, "opus") &&
        format.clockrate_hz == 48000) {
      size_t channels = 1;
      auto it = format.parameters.find("stereo");
      if (it != format.parameters.end() && it->second == "1") channels = 2;
      return std::make_unique<webrtc::AudioDecoderOpusImpl>(
          env.field_trials(), channels, 48000);
    }
    return nullptr;
  }
};

}  // namespace

struct NpNetEq {
  webrtc::SimulatedClock* clock = nullptr;  // Owned by `env`.
  std::optional<webrtc::Environment> env;
  std::unique_ptr<webrtc::NetEqImpl> neteq;
  webrtc::AudioFrame frame;
};

struct NpOpusEncoder {
  OpusEncoder* enc = nullptr;
};

struct NpCngEncoder {
  explicit NpCngEncoder(int fs, int interval, int quality)
      : enc(fs, interval, quality) {}
  webrtc::ComfortNoiseEncoder enc;
};

extern "C" {

size_t np_struct_size(int which) {
  switch (which) {
    case 0: return sizeof(NpConfig);
    case 1: return sizeof(NpRtpHeader);
    case 2: return sizeof(NpAudioFrame);
    case 3: return sizeof(NpNetworkStats);
    case 4: return sizeof(NpLifetimeStats);
    case 5: return sizeof(NpOperationsAndState);
    default: return 0;
  }
}

NpNetEq* np_neteq_create(const NpConfig* c) {
  if (c == nullptr) return nullptr;
  auto handle = std::make_unique<NpNetEq>();

  auto clock = std::make_unique<webrtc::SimulatedClock>(
      webrtc::Timestamp::Millis(c->start_time_ms));
  handle->clock = clock.get();
  webrtc::EnvironmentFactory env_factory;
  env_factory.Set(std::unique_ptr<const webrtc::FieldTrialsView>(
      std::make_unique<EmptyFieldTrials>()));
  env_factory.Set(std::unique_ptr<webrtc::Clock>(std::move(clock)));
  env_factory.Set(std::unique_ptr<webrtc::TaskQueueFactory>(
      std::make_unique<NoTaskQueueFactory>()));
  env_factory.Set(std::unique_ptr<webrtc::RtcEventLog>(
      std::make_unique<webrtc::RtcEventLogNull>()));
  handle->env.emplace(env_factory.Create());

  webrtc::NetEq::Config config;
  config.sample_rate_hz = c->sample_rate_hz;
  config.max_packets_in_buffer = c->max_packets_in_buffer;
  config.max_delay_ms = c->max_delay_ms;
  config.min_delay_ms = c->min_delay_ms;
  config.enable_fast_accelerate = c->enable_fast_accelerate != 0;
  config.enable_muted_state = c->enable_muted_state != 0;
  config.enable_rtx_handling = c->enable_rtx_handling != 0;
  config.for_test_no_time_stretching = c->for_test_no_time_stretching != 0;

  webrtc::DefaultNetEqControllerFactory controller_factory;
  webrtc::NetEqImpl::Dependencies deps(
      *handle->env, config, webrtc::make_ref_counted<ParityDecoderFactory>(),
      controller_factory);
  handle->neteq =
      std::make_unique<webrtc::NetEqImpl>(config, std::move(deps));
  return handle.release();
}

void np_neteq_destroy(NpNetEq* n) { delete n; }

int np_neteq_register_l16(NpNetEq* n, int pt, int rate, int channels) {
  return n->neteq->RegisterPayloadType(
             pt, SdpAudioFormat("L16", rate, static_cast<size_t>(channels)))
             ? 1
             : 0;
}

int np_neteq_register_opus(NpNetEq* n, int pt, int channels) {
  webrtc::CodecParameterMap params;
  if (channels == 2) params["stereo"] = "1";
  return n->neteq->RegisterPayloadType(
             pt, SdpAudioFormat("opus", 48000, 2, std::move(params)))
             ? 1
             : 0;
}

int np_neteq_register_cn(NpNetEq* n, int pt, int rate) {
  return n->neteq->RegisterPayloadType(pt, SdpAudioFormat("CN", rate, 1)) ? 1
                                                                          : 0;
}

int np_neteq_remove_payload_type(NpNetEq* n, int pt) {
  return n->neteq->RemovePayloadType(static_cast<uint8_t>(pt));
}

static webrtc::RTPHeader ToRtpHeader(const NpRtpHeader* h) {
  webrtc::RTPHeader rtp;
  rtp.payloadType = h->payload_type;
  rtp.markerBit = h->marker != 0;
  rtp.sequenceNumber = h->sequence_number;
  rtp.timestamp = h->timestamp;
  rtp.ssrc = h->ssrc;
  return rtp;
}

int np_neteq_insert_packet(NpNetEq* n, const NpRtpHeader* h,
                           const uint8_t* payload, size_t len,
                           int64_t receive_time_ms) {
  webrtc::RTPHeader rtp = ToRtpHeader(h);
  webrtc::RtpPacketInfo info(rtp,
                             webrtc::Timestamp::Millis(receive_time_ms));
  return n->neteq->InsertPacket(
      rtp, std::span<const uint8_t>(payload, len), info);
}

void np_neteq_insert_empty_packet(NpNetEq* n, const NpRtpHeader* h) {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
  n->neteq->InsertEmptyPacket(ToRtpHeader(h));
#pragma clang diagnostic pop
}

int np_neteq_get_audio(NpNetEq* n, NpAudioFrame* out, int16_t* samples,
                       size_t capacity) {
  bool muted = false;
  int rc = n->neteq->GetAudio(&n->frame, &muted);
  const webrtc::AudioFrame& f = n->frame;
  std::memset(out, 0, sizeof(*out));
  out->return_code = rc;
  out->samples_per_channel = static_cast<uint32_t>(f.samples_per_channel());
  out->num_channels = static_cast<uint32_t>(f.num_channels());
  out->sample_rate_hz = f.sample_rate_hz();
  out->speech_type = static_cast<int32_t>(f.speech_type_);
  out->vad_activity = static_cast<int32_t>(f.vad_activity_);
  out->muted = muted ? 1 : 0;
  out->operation = static_cast<int32_t>(n->neteq->last_operation_for_test());
  if (auto ts = n->neteq->GetPlayoutTimestamp()) {
    out->has_playout_timestamp = 1;
    out->playout_timestamp = *ts;
  }
  size_t total = f.samples_per_channel() * f.num_channels();
  if (samples != nullptr) {
    size_t copy = std::min(total, capacity);
    // data() returns a zeroed buffer when the frame is muted.
    std::memcpy(samples, f.data(), copy * sizeof(int16_t));
  }
  return rc;
}

void np_neteq_advance_time_ms(NpNetEq* n, int64_t ms) {
  n->clock->AdvanceTimeMilliseconds(ms);
}

int64_t np_neteq_now_ms(const NpNetEq* n) { return n->clock->TimeInMilliseconds(); }

int np_neteq_network_statistics(NpNetEq* n, NpNetworkStats* out, int reset) {
  webrtc::NetEqNetworkStatistics s;
  int rc = 0;
  if (reset) {
    rc = n->neteq->NetworkStatistics(&s);
  } else {
    s = n->neteq->CurrentNetworkStatistics();
  }
  out->current_buffer_size_ms = s.current_buffer_size_ms;
  out->preferred_buffer_size_ms = s.preferred_buffer_size_ms;
  out->jitter_peaks_found = s.jitter_peaks_found;
  out->expand_rate = s.expand_rate;
  out->speech_expand_rate = s.speech_expand_rate;
  out->preemptive_rate = s.preemptive_rate;
  out->accelerate_rate = s.accelerate_rate;
  out->secondary_decoded_rate = s.secondary_decoded_rate;
  out->secondary_discarded_rate = s.secondary_discarded_rate;
  out->mean_waiting_time_ms = s.mean_waiting_time_ms;
  out->median_waiting_time_ms = s.median_waiting_time_ms;
  out->min_waiting_time_ms = s.min_waiting_time_ms;
  out->max_waiting_time_ms = s.max_waiting_time_ms;
  return rc;
}

void np_neteq_lifetime_statistics(const NpNetEq* n, NpLifetimeStats* out) {
  webrtc::NetEqLifetimeStatistics s = n->neteq->GetLifetimeStatistics();
  out->total_samples_received = s.total_samples_received;
  out->concealed_samples = s.concealed_samples;
  out->concealment_events = s.concealment_events;
  out->jitter_buffer_delay_ms = s.jitter_buffer_delay_ms;
  out->jitter_buffer_emitted_count = s.jitter_buffer_emitted_count;
  out->jitter_buffer_target_delay_ms = s.jitter_buffer_target_delay_ms;
  out->jitter_buffer_minimum_delay_ms = s.jitter_buffer_minimum_delay_ms;
  out->inserted_samples_for_deceleration = s.inserted_samples_for_deceleration;
  out->removed_samples_for_acceleration = s.removed_samples_for_acceleration;
  out->silent_concealed_samples = s.silent_concealed_samples;
  out->fec_packets_received = s.fec_packets_received;
  out->fec_packets_discarded = s.fec_packets_discarded;
  out->packets_discarded = s.packets_discarded;
  out->delayed_packet_outage_samples = s.delayed_packet_outage_samples;
  out->delayed_packet_outage_events = s.delayed_packet_outage_events;
  out->relative_packet_arrival_delay_ms = s.relative_packet_arrival_delay_ms;
  out->jitter_buffer_packets_received = s.jitter_buffer_packets_received;
  out->interruption_count = s.interruption_count;
  out->total_interruption_duration_ms = s.total_interruption_duration_ms;
  out->generated_noise_samples = s.generated_noise_samples;
  out->total_processing_delay_us = s.total_processing_delay_us;
}

void np_neteq_operations_and_state(const NpNetEq* n,
                                   NpOperationsAndState* out) {
  webrtc::NetEqOperationsAndState s = n->neteq->GetOperationsAndState();
  out->preemptive_samples = s.preemptive_samples;
  out->accelerate_samples = s.accelerate_samples;
  out->packet_buffer_flushes = s.packet_buffer_flushes;
  out->last_waiting_time_ms = s.last_waiting_time_ms;
  out->current_buffer_size_ms = s.current_buffer_size_ms;
  out->current_frame_size_ms = s.current_frame_size_ms;
  out->next_packet_available = s.next_packet_available ? 1 : 0;
}

int np_neteq_target_delay_ms(const NpNetEq* n) {
  return n->neteq->TargetDelayMs();
}
int np_neteq_filtered_current_delay_ms(const NpNetEq* n) {
  return n->neteq->FilteredCurrentDelayMs();
}
int np_neteq_set_base_minimum_delay_ms(NpNetEq* n, int d) {
  return n->neteq->SetBaseMinimumDelayMs(d) ? 1 : 0;
}
int np_neteq_get_base_minimum_delay_ms(const NpNetEq* n) {
  return n->neteq->GetBaseMinimumDelayMs();
}
int np_neteq_set_minimum_delay_ms(NpNetEq* n, int d) {
  return n->neteq->SetMinimumDelay(d) ? 1 : 0;
}
int np_neteq_set_maximum_delay_ms(NpNetEq* n, int d) {
  return n->neteq->SetMaximumDelay(d) ? 1 : 0;
}
int np_neteq_sync_buffer_size_ms(const NpNetEq* n) {
  return n->neteq->SyncBufferSizeMs();
}
int np_neteq_last_output_sample_rate_hz(const NpNetEq* n) {
  return n->neteq->last_output_sample_rate_hz();
}
void np_neteq_flush(NpNetEq* n) { n->neteq->FlushBuffers(); }

NpOpusEncoder* np_opus_encoder_create(int rate, int channels, int application,
                                      int bitrate, int complexity,
                                      int use_dtx) {
  int err = 0;
  OpusEncoder* enc = opus_encoder_create(rate, channels, application, &err);
  if (err != OPUS_OK || enc == nullptr) return nullptr;
  opus_encoder_ctl(enc, OPUS_SET_BITRATE(bitrate));
  opus_encoder_ctl(enc, OPUS_SET_COMPLEXITY(complexity));
  opus_encoder_ctl(enc, OPUS_SET_DTX(use_dtx ? 1 : 0));
  // Deterministic output: no VBR constraint surprises, no in-band FEC.
  opus_encoder_ctl(enc, OPUS_SET_INBAND_FEC(0));
  auto* handle = new NpOpusEncoder;
  handle->enc = enc;
  return handle;
}

void np_opus_encoder_destroy(NpOpusEncoder* e) {
  if (e == nullptr) return;
  opus_encoder_destroy(e->enc);
  delete e;
}

int np_opus_encode(NpOpusEncoder* e, const int16_t* pcm, int frame_size,
                   uint8_t* out, int max_out) {
  return opus_encode(e->enc, pcm, frame_size, out, max_out);
}

NpCngEncoder* np_cng_encoder_create(int rate, int interval, int quality) {
  if (rate != 8000 && rate != 16000) return nullptr;
  if (quality < 1 || quality > 12) return nullptr;
  return new NpCngEncoder(rate, interval, quality);
}

void np_cng_encoder_destroy(NpCngEncoder* e) { delete e; }

int np_cng_encode(NpCngEncoder* e, const int16_t* pcm, size_t n, int force_sid,
                  uint8_t* out, size_t max_out) {
  if (n > 640) return -1;
  webrtc::Buffer sid;
  e->enc.Encode(std::span<const int16_t>(pcm, n), force_sid != 0, &sid);
  if (sid.size() > max_out) return -1;
  if (sid.size() > 0) std::memcpy(out, sid.data(), sid.size());
  return static_cast<int>(sid.size());
}

}  // extern "C"

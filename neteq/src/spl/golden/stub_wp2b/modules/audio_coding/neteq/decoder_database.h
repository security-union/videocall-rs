// WP2b golden stub: the CNG part of DecoderDatabase that ComfortNoise uses,
// with the real semantics of SetActiveCngDecoder / GetActiveCngDecoder for a
// database holding a single CN decoder at payload type 13.
#ifndef MODULES_AUDIO_CODING_NETEQ_DECODER_DATABASE_H_
#define MODULES_AUDIO_CODING_NETEQ_DECODER_DATABASE_H_
#include <cstdint>
#include <memory>
#include "modules/audio_coding/codecs/cng/webrtc_cng.h"
#include "modules/audio_coding/neteq/packet.h"
namespace webrtc {
class DecoderDatabase {
 public:
  enum DatabaseReturnCodes { kOK = 0, kDecoderNotFound = -5 };
  static constexpr uint8_t kCngPayloadType = 13;
  int SetActiveCngDecoder(uint8_t rtp_payload_type) {
    if (rtp_payload_type != kCngPayloadType) return kDecoderNotFound;
    if (active_cng_decoder_type_ >= 0 &&
        active_cng_decoder_type_ != rtp_payload_type) {
      active_cng_decoder_.reset();
    }
    active_cng_decoder_type_ = rtp_payload_type;
    return kOK;
  }
  ComfortNoiseDecoder* GetActiveCngDecoder() {
    if (active_cng_decoder_type_ < 0) return nullptr;
    if (!active_cng_decoder_) active_cng_decoder_.reset(new ComfortNoiseDecoder);
    return active_cng_decoder_.get();
  }
 private:
  int active_cng_decoder_type_ = -1;
  std::unique_ptr<ComfortNoiseDecoder> active_cng_decoder_;
};
}  // namespace webrtc
#endif

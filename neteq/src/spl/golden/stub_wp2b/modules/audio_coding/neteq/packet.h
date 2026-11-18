// WP2b golden stub: the two `Packet` fields ComfortNoise reads.
#ifndef MODULES_AUDIO_CODING_NETEQ_PACKET_H_
#define MODULES_AUDIO_CODING_NETEQ_PACKET_H_
#include <cstdint>
#include <vector>
namespace webrtc {
struct Packet {
  uint8_t payload_type = 0;
  std::vector<uint8_t> payload;
};
}  // namespace webrtc
#endif

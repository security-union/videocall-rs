/*
 * Link-time stand-ins for WebRTC symbols that the compiled NetEq sources
 * reference but never call in the parity harness.
 *
 * - CreateDefaultTaskQueueFactory: referenced by
 *   EnvironmentFactory::CreateWithDefaults(). The shim always supplies its own
 *   TaskQueueFactory, so the default is never constructed; this avoids
 *   compiling WebRTC's threading/task-queue stack.
 * - RtpPacket / RTP header-extension parsers: referenced only by
 *   RtpPacketInfo(const RtpPacketReceived&) in api/rtp_packet_info.cc. The
 *   shim builds RtpPacketInfo from an RTPHeader, so these are unreachable;
 *   stubbing them avoids compiling modules/rtp_rtcp.
 *
 * Every stub aborts via RTC_CHECK_NOTREACHED() if it is ever reached.
 */
#include <cstdint>
#include <memory>
#include <span>
#include <vector>

#include "api/field_trials_view.h"
#include "api/task_queue/default_task_queue_factory.h"
#include "api/task_queue/task_queue_factory.h"
#include "api/rtp_headers.h"
#include "modules/rtp_rtcp/source/rtp_header_extensions.h"
#include "modules/rtp_rtcp/source/rtp_packet.h"
#include "rtc_base/checks.h"

namespace webrtc {

std::unique_ptr<TaskQueueFactory> CreateDefaultTaskQueueFactory(
    const FieldTrialsView* /* field_trials */) {
  RTC_CHECK_NOTREACHED();
}

std::vector<uint32_t> RtpPacket::Csrcs() const {
  RTC_CHECK_NOTREACHED();
}

std::span<const uint8_t> RtpPacket::FindExtension(ExtensionType) const {
  RTC_CHECK_NOTREACHED();
}

bool AudioLevelExtension::Parse(std::span<const uint8_t>, AudioLevel*) {
  RTC_CHECK_NOTREACHED();
}

bool AbsoluteCaptureTimeExtension::Parse(std::span<const uint8_t>,
                                         AbsoluteCaptureTime*) {
  RTC_CHECK_NOTREACHED();
}

}  // namespace webrtc

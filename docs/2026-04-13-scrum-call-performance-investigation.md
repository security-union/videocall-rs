# Meeting Performance Investigation: Scrum Call 2026-04-13

**Meeting**: `meeting_sync`
**Time**: 09:58 - 10:22 EDT (~24 minutes)
**Participants**: 6 (Jay, TonyE, Tanya, anhelina, Palina, Yury_Ch)
**Server**: app.videocall.fnxlabs.com (HCL K3s)

## Data Sources

- Console logs from Jay, Tony, and anhelina
- Prometheus/Grafana metrics (videocall-investigation dashboard)
- Screenshots of the meeting UI and signal quality popup
- Tony's written observations (Observations.docx)

---

## Executive Summary

**The primary issue**: Screen share was blurry for all viewers (854x480 at 5fps instead of 1280x720 at 10fps or higher). Camera video also degraded to minimum quality for multiple participants. Audio quality was fine throughout.

**Root cause**: The adaptive quality controller uses a **worst-peer FPS feedback loop** that caused false degradation. The sender's encoder quality is driven by the single worst-performing receiver's reported FPS. When any one receiver reports low or zero FPS (due to decoder errors, reconnection, or their own network issues), the sender's quality crashes for ALL receivers.

**This is NOT a bandwidth issue.** Tony has 330/228 Mbps (Norwood Light Broadband, 9ms ping) on a capable Mac. Google Meet works excellently for him, including screen sharing. Our maximum combined bandwidth demand (~5 Mbps for camera + screen + audio at highest tiers) is less than 3% of his available upload. The logs prove it: Tony's first camera degradation happened with `fps_ratio=0.00` but `bitrate_ratio=0.90` -- zero received FPS despite 90% of ideal bitrate being available.

---

## The Smoking Gun: fps_ratio=0.00

Both Tony and Jay show the exact same pattern at the start of the meeting:

**Tony (WebSocket, 76ms RTT)**:
```
AdaptiveQuality: video stepped DOWN to tier 'low' (index 5), fps_ratio=0.00, bitrate_ratio=0.90
AdaptiveQuality: video stepped DOWN to tier 'very_low' (index 6), fps_ratio=0.00, bitrate_ratio=0.47
AdaptiveQuality: video stepped DOWN to tier 'minimal' (index 7), fps_ratio=0.00, bitrate_ratio=0.40
```

**Jay (WebTransport, 47.5ms RTT)**:
```
AdaptiveQuality: video stepped DOWN to tier 'low' (index 5), fps_ratio=0.00, bitrate_ratio=1.42
AdaptiveQuality: video stepped DOWN to tier 'very_low' (index 6), fps_ratio=0.00, bitrate_ratio=0.72
AdaptiveQuality: video stepped DOWN to tier 'minimal' (index 7), fps_ratio=0.17, bitrate_ratio=0.64
```

Key observations:
- **fps_ratio=0.00**: The controller thinks zero frames are being received, which is false
- **bitrate_ratio=1.42 (Jay)**: Jay's bitrate was 142% of ideal -- bandwidth was MORE than sufficient
- **bitrate_ratio=0.90 (Tony)**: Tony's bitrate was 90% of ideal -- perfectly healthy
- This happened for BOTH WebSocket and WebTransport transports
- This happened BEFORE screen share was even activated
- Camera was already at minimal tier before Tony started screen sharing

---

## How The Feedback Loop Works

The adaptive quality controller in `encoder_bitrate_controller.rs` follows this chain:

```
1. Sender encodes video/screen and sends to relay
2. Relay forwards to all receivers
3. Each receiver measures decoded FPS and reports it back via DiagnosticsPacket
4. Sender's EncoderBitrateController collects these packets
5. get_worst_fps_peer() finds the MINIMUM FPS across all receivers
6. That worst-case FPS drives tier selection for the sender's encoder
```

**The critical flaw**: Step 5 uses `get_worst_fps_peer()` -- the sender's quality is held hostage by its weakest receiver. In a 6-person meeting with heterogeneous connections, one struggling receiver tanks quality for everyone.

### Why fps_received = 0?

The `fps_received` field in diagnostic packets comes from the receiving peer's decoded frame count. It reads as zero when:
- A receiver's WebCodecs decoder errors and resets (we see this in Tony's logs: `[WORKER] WebCodecs decoder error: EncodingError: Decoding error.` followed by decoder replacement)
- A receiver disconnects/reconnects (Yury_Ch had at least one reconnection)
- A receiver's network causes enough packet loss that no complete frames arrive in the measurement window
- The diagnostic feedback loop itself has latency and reports stale zeros

### The Cascade

Once fps_ratio hits 0.00, the PID controller's output (`final_bitrate`) drops dramatically. This feeds back into the quality manager as a low `bitrate_ratio`, creating a double-whammy: BOTH fps_ratio AND bitrate_ratio signal degradation simultaneously, accelerating the descent to the floor tier.

For Tony's camera, the full cascade took 3 tier transitions in rapid succession:
- medium (720p/25fps) -> low (360p/20fps) -> very_low (270p/15fps) -> minimal (240p/10fps)

The camera hit the floor and never recovered for the duration of the meeting.

---

## Screen Share Degradation: Victim, Not Cause

Tony's screen share timeline (from his console log):

```
Line 612: Frame dimensions changed from 3840x2234 to 1920x1080, reconfiguring encoder
Line 613: AdaptiveQuality: forced video step UP to tier 'low' (index 5) for cross-stream coordination
Line 614: CameraEncoder: screen sharing ACTIVE -- camera tier coordination applied
Line 625: Updating screen bitrate to 600000  (600kbps -- medium tier ideal)
Line 627: Camera stepped DOWN: fps_ratio=0.69, bitrate_ratio=0.10
Line 632: Screen stepped DOWN to 'low': fps_ratio=0.00, bitrate_ratio=0.82
Line 634: ScreenEncoder: tier dimension change -> 854x480 (was 1920x1080)
```

What happened:
1. Tony started screen share. Camera was forced to "low" tier (cross-stream bandwidth coordination -- working as designed)
2. Within seconds, the screen encoder received fps_ratio=0.00 from the worst-peer feedback
3. Screen jumped directly from 1920x1080 to 854x480@5fps (the lowest screen tier)
4. Screen bitrate dropped from 600kbps to 211kbps to 163kbps to 120kbps
5. Screen share stayed at the floor for the rest of the meeting

**The screen encoder's bitrate was healthy (0.82 of ideal)**, but the fps_ratio from peer feedback was 0.00. The screen content (a GitHub project board) was readable at the source -- the blurriness was entirely caused by the false quality degradation.

### Note on the Tier Structure

Screen share quality tiers:

| Tier | Resolution | FPS | Ideal Bitrate |
|------|-----------|-----|--------------|
| high | 1920x1080 | 15 | 1500 kbps |
| medium | 1280x720 | 10 | 600 kbps |
| **low** | **854x480** | **5** | **250 kbps** |

854x480 at 5fps makes text on a project board or code review unreadable. There is no tier below this, so once it hits the floor, it stays there.

---

## Jay's WebTransport Datagram Drops (Separate Issue)

Jay's console shows massive `"datagram dropped (stream busy)"` messages. This is a WebTransport-specific issue:

In `videocall-transport/src/webtransport.rs:346-354`, the datagram send path checks `writable.locked()` before each write. If the previous datagram write hasn't completed, the new one is silently dropped. Under load, this can cascade.

However, datagrams are only used for **control packets** (heartbeats, RTT probes), not media. Media goes through reliable streams on both WebSocket and WebTransport. The datagram drops themselves are harmless, but they may be contributing to the false quality signals if the drop count feeds into any quality metric.

Jay's audio quality was fine throughout the call despite the datagram drops, confirming that media delivery (via reliable streams) was working properly.

---

## Audio Was Fine -- Confirming The Diagnosis

Jay confirmed that call quality (hearing others) was very good for everyone. This is consistent with the root cause analysis:

- Audio packets are small (~50kbps at high tier) and use reliable delivery
- Audio quality depends on actual network conditions, which were fine
- The false fps_ratio=0.00 DID cause audio tier degradation in the logs (audio stepped down to "emergency" 16kbps multiple times), but it repeatedly recovered
- Despite the tier oscillation, the audio codec (Opus) with FEC handled the brief dips gracefully

The fact that audio was perceptually fine while video/screen collapsed to minimum quality is strong evidence that the network was healthy and the video quality signals were wrong.

---

## Server Health: Confirmed Clean

Prometheus metrics from the relay during the meeting show:
- **Zero** relay packet drops
- **Zero** send queue buildup
- Consistent pod health
- All metrics flowing correctly

The relay server faithfully forwarded everything. This is purely a client-side feedback loop issue.

---

## Per-Participant Summary

| Participant | Transport | RTT | Camera Tier | Screen | Notes |
|------------|-----------|-----|-------------|--------|-------|
| Jay | WebTransport | 47.5ms | minimal (240p/10fps) | N/A | WT datagram drops; fps_ratio=0.00 from start |
| TonyE | WebSocket | 76ms | minimal (240p/10fps) | low (480p/5fps) | 330/228 Mbps, 9ms ping; fps_ratio=0.00 from start |
| anhelina | WebSocket | 160ms | unknown | N/A | PCM buffer high-watermark drops (latency cap) |
| Tanya | WebSocket | ~93ms | unknown | N/A | NetEQ target delay 1397ms avg (high jitter buffer) |
| Palina | WebSocket | ~120ms | unknown | N/A | High packet loss reported to/from some peers |
| Yury_Ch | WebSocket | ~158ms | unknown | N/A | At least one disconnect/reconnect during meeting |

---

## Root Cause Comparison: Us vs. Google Meet

Google Meet uses **simulcast** with SFU-selected quality:
- Sender encodes at 2-3 quality levels simultaneously
- The SFU (relay) picks the appropriate quality tier for each receiver independently
- A struggling receiver gets a lower tier; other receivers are unaffected
- The sender's encode quality is driven by the sender's own bandwidth, not receiver feedback

Our system:
- Sender encodes at a **single quality level**
- Quality level is driven by the **worst receiver's** FPS report
- One struggling receiver degrades the sender for ALL receivers
- The sender has excellent bandwidth but can't use it

This is why Tony gets perfect quality on Google Meet but 480p/5fps screen share on our platform with the same network.

---

## Recommendations

### 1. Replace worst-peer with robust aggregation (highest priority)

**Current**: `get_worst_fps_peer()` returns the minimum FPS across all receivers
**Proposed**: Use median or 75th percentile FPS instead of minimum

This single change would prevent one outlier receiver from tanking quality for everyone. If 4 out of 5 peers report 25fps and 1 reports 0fps, the sender should encode at a quality appropriate for the majority.

### 2. Filter out unreliable diagnostic reports

Ignore fps_received=0 from peers that:
- Have recently reconnected (within the last 5-10 seconds)
- Are reporting decoder errors
- Have stale diagnostic timestamps (feedback older than 2x the diagnostic interval)

These are transient conditions that shouldn't drive permanent quality degradation.

### 3. Separate camera and screen quality feedback

Currently both the camera and screen encoders react to the same worst-peer signal. The screen encoder should have its own feedback path that considers only screen-specific FPS reports, not camera FPS.

### 4. Add a content-aware screen share floor

For static content (code reviews, project boards), 854x480 at 5fps makes text unreadable. Consider:
- A higher minimum resolution for screen share (e.g., 1280x720 at 3fps rather than 854x480 at 5fps)
- Content-type detection: if frame-to-frame delta is low (static content), prefer resolution over FPS

### 5. Long-term: Implement simulcast

Encode at 2-3 quality tiers simultaneously and let the relay select per-receiver. This is the industry standard approach (used by Google Meet, Zoom, Teams) and eliminates the worst-peer problem entirely. The sender always encodes at the highest quality their bandwidth supports.

### 6. Investigate decoder errors as a feedback trigger

Tony's logs show `[WORKER] WebCodecs decoder error: EncodingError: Decoding error.` on the receiving side. When a receiver's decoder errors and resets, it momentarily reports 0 FPS. If this receiver happens to be the "worst peer," the sender's quality crashes. Decoder errors should not propagate as quality feedback.

---

## Appendix: Key Code Paths

| Component | File | Key Lines |
|-----------|------|-----------|
| Worst-peer FPS selection | `videocall-client/src/diagnostics/encoder_bitrate_controller.rs` | `get_worst_fps_peer()` at L192, used at L344 |
| Quality manager update | `videocall-client/src/diagnostics/adaptive_quality_manager.rs` | `update()` at L148, fps_ratio at L162 |
| Degradation thresholds | `videocall-client/src/adaptive_quality_constants.rs` | `VIDEO_TIER_DEGRADE_FPS_RATIO=0.50` at L283 |
| Screen quality tiers | `videocall-client/src/adaptive_quality_constants.rs` | `SCREEN_QUALITY_TIERS` at L198 |
| Screen-camera coordination | `videocall-client/src/diagnostics/encoder_bitrate_controller.rs` | `notify_screen_sharing()` at L524 |
| WT datagram drop | `videocall-transport/src/webtransport.rs` | `send_datagram()` at L346, locked check at L350 |
| Screen encoder diagnostics | `videocall-client/src/encode/screen_encoder.rs` | `process_diagnostics_packet()` at L171 |

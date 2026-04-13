# Server-Side Metrics Gap Analysis

**Context**: Investigation of the 2026-04-13 scrum call performance.
**Goal**: Evaluate whether Grafana/Prometheus metrics were useful for diagnosis, identify gaps, and recommend corrections.

---

## What Metrics We Have Today

### Relay Server Metrics (5 metrics, collected in-process)

| Metric | Type | Labels | What It Tells Us |
|--------|------|--------|-----------------|
| `relay_packet_drops_total` | Counter | room, transport, drop_reason | Packets dropped by the relay's outbound queue |
| `relay_outbound_queue_depth` | Gauge | room | Current outbound channel occupancy (WT only) |
| `relay_nats_publish_latency_ms` | Histogram | -- | Time to publish media to NATS |
| `relay_active_sessions_per_room` | Gauge | room, transport | Connections per meeting room |
| `relay_room_bytes_total` | Counter | room, direction | Bytes forwarded per room |

### Client Diagnostic Metrics (37 metrics, client -> NATS -> metrics_server -> Prometheus)

**Per-pair receiver metrics** (labels: meeting_id, session_id, from_peer, to_peer, reporter_name, peer_name):

| Metric | What It Tells Us |
|--------|-----------------|
| `videocall_video_fps` | FPS observed by receiver |
| `videocall_video_bitrate_kbps` | Bitrate observed by receiver |
| `videocall_video_quality_score` | Video quality 0-100 |
| `videocall_video_frames_dropped` | Frames dropped by receiver |
| `videocall_audio_packet_loss_pct` | Audio loss from NetEQ concealment |
| `videocall_audio_quality_score` | Audio quality 0-100 |
| `videocall_call_quality_score` | min(audio, video) quality |
| `videocall_neteq_target_delay_ms` | Jitter buffer target delay |
| `videocall_neteq_audio_buffer_ms` | Audio buffer level |
| `videocall_neteq_expand_ops_per_sec` | Audio concealment rate |
| `videocall_neteq_normal_ops_per_sec` | Normal audio decode rate |
| `videocall_neteq_accelerate_ops_per_sec` | Buffer acceleration rate |
| `videocall_neteq_packets_per_sec` | Packet arrival rate |
| `videocall_neteq_packets_awaiting_decode` | Decode queue depth |
| `videocall_peer_can_listen` | Receiver hearing sender? |
| `videocall_peer_can_see` | Receiver seeing sender? |
| `videocall_peer_audio_enabled` / `videocall_peer_video_enabled` | Peer media state |

**Per-sender self-reported metrics** (labels: meeting_id, session_id, peer_id, display_name):

| Metric | What It Tells Us |
|--------|-----------------|
| `videocall_adaptive_video_tier` | Current camera video tier (0=best, 7=minimal) |
| `videocall_adaptive_audio_tier` | Current audio tier (0=high, 3=emergency) |
| `videocall_datagram_drops_total` | Cumulative WT datagram drops |
| `videocall_websocket_drops_total` | Cumulative WS packet drops |
| `videocall_keyframe_requests_sent_total` | PLI requests sent |
| `videocall_client_active_server_rtt_ms` | RTT to active relay |
| `videocall_client_active_server` | Which relay (WS/WT) |
| `videocall_client_send_queue_bytes` | Send buffer occupancy |
| `videocall_client_packets_received_per_sec` / `videocall_client_packets_sent_per_sec` | Packet rates |
| `videocall_client_tab_visible` / `videocall_client_tab_throttled` | Browser tab state |
| `videocall_client_memory_used_bytes` / `videocall_client_memory_total_bytes` | Browser memory |
| `videocall_self_audio_enabled` / `videocall_self_video_enabled` | Self-reported media state |
| `videocall_active_sessions_total` / `videocall_meeting_participants` / `videocall_peer_connections_total` | Session counts |

### Server Connection Metrics (7 metrics, relay -> NATS -> metrics_server_snapshot)

| Metric | What It Tells Us |
|--------|-----------------|
| `videocall_server_connections_active` | Active connections per session/protocol |
| `videocall_server_unique_users_active` | Deduplicated user count |
| `videocall_server_protocol_connections` | Connections per protocol |
| `videocall_server_data_bytes_total` | Bytes per session/direction |
| `videocall_server_connection_duration_seconds` | Connection lifetime histogram |
| `videocall_server_connection_events_total` | Total connection events |
| `videocall_server_reconnections_total` | Reconnection count |

---

## What Metrics Told Us During the Investigation

### Helpful (correctly narrowed the problem space)

| What We Queried | Result | Value |
|----------------|--------|-------|
| `relay_packet_drops_total` | Zero drops | Ruled out server-side packet loss |
| `relay_outbound_queue_depth` | Zero buildup | Ruled out relay congestion |
| `videocall_client_active_server_rtt_ms` | Jay 48ms, Tony 76ms, anhelina 160ms | Established transport context |
| `videocall_client_active_server` | Jay=WT, Tony/others=WS | Identified transport per participant |
| `videocall_adaptive_video_tier` | Everyone degraded to 7 (minimal) | Confirmed widespread degradation |
| `videocall_server_reconnections_total` | Yury_Ch had reconnections | Explained metric gaps |

### Misleading (pointed investigation in wrong direction)

| What We Queried | Result | Why It Was Misleading |
|----------------|--------|----------------------|
| `videocall_audio_packet_loss_pct` | TonyE->Jay: 100%, Tanya->Yury_Ch: 91% | These numbers initially suggested severe network loss. In reality, they are **artifacts of sender-side quality degradation**. When the sender's encoder drops to minimal tier and skips frames, receivers see sequence number gaps and report them as "packet loss." The loss metric measures a symptom of the bug, not its cause. |
| `videocall_video_quality_score` | TonyE avg 60.6 (Jay's view), min 0.54 | Confirmed something was wrong but gave zero insight into WHY. A quality score of 0.54 says "bad" -- it doesn't say "bad because the adaptive quality controller received false fps_ratio=0.00 from a struggling receiver and crashed the encoder to minimal tier." |
| `videocall_video_fps` | TonyE->Jay avg 12.9fps, min 0.1fps | Shows received FPS was low but doesn't distinguish between: (a) sender encoded low FPS, (b) relay dropped packets, (c) receiver couldn't decode. The root cause was (a), but this metric can't tell you that. |
| `videocall_neteq_target_delay_ms` | Tanya->Palina avg 1397ms, max 2080ms | Alarming number that suggests massive jitter. But high target delay is partially caused by the encoder sending at irregular intervals (because it's oscillating between tiers), not just network jitter. |

### Could Not Answer (the critical questions) — ALL RESOLVED by PR #308

| Question | Why We Couldn't Answer It | Now Answered By |
|----------|--------------------------|-----------------|
| Why did the encoder degrade to minimal? | No metric for `fps_ratio` or `bitrate_ratio` -- the actual inputs to the tier decision | `videocall_encoder_fps_ratio`, `videocall_encoder_bitrate_ratio` |
| Which receiver caused the degradation? | No metric for worst-peer identity or worst-peer FPS | `videocall_encoder_worst_peer_fps` |
| Was screen share degraded? | No screen-specific tier metric. `videocall_adaptive_video_tier` only tracks camera tier. | `videocall_adaptive_screen_tier`, `videocall_screen_video_{fps,bitrate_kbps}` |
| What was the encoder's actual output FPS? | `videocall_video_fps` is receiver-side. No sender-side encoder output FPS metric. | `videocall_encoder_output_fps` |
| Did decoder errors on receivers trigger false feedback? | No metric for decoder error count or decoder state | `videocall_decoder_errors_total` |
| When did screen share start/stop? | No metric for screen sharing state | `videocall_screen_sharing_active` |
| What bitrate did the PID controller compute? | No metric for PID controller output | `videocall_encoder_target_bitrate_kbps` |

---

## Root Cause the Metrics Missed

The console logs revealed the smoking gun:

```
Tony:  fps_ratio=0.00, bitrate_ratio=0.90   (bandwidth fine, FPS signal wrong)
Jay:   fps_ratio=0.00, bitrate_ratio=1.42   (bandwidth 142% of ideal, FPS signal wrong)
```

The adaptive quality controller's `get_worst_fps_peer()` function picks the single lowest FPS report across all receivers and uses it to drive the sender's tier selection. One struggling receiver (decoder error, reconnection, bad network) tanks the sender's quality for ALL receivers.

**None of the 50 metrics we currently expose could have identified this.** The decision-making layer between "what receivers report" and "what the encoder does" is completely invisible to our monitoring.

---

## What Must Be Fixed

### Problem 1: Audio Packet Loss Metric Is Fundamentally Misleading

**Current**: `videocall_audio_packet_loss_pct` reports 100% loss when sender's encoder has degraded and is dropping frames before transmission. This is not network packet loss.

**Fix**: Rename or split this metric:
- `videocall_audio_concealment_pct` -- percentage of audio playout that used concealment (what it actually measures)
- `videocall_audio_network_loss_pct` (new) -- actual transport-layer packet loss, measured from sequence number gaps AFTER accounting for sender-side frame drops

Until this is fixed, any alert or dashboard based on `audio_packet_loss_pct` will fire false positives whenever the adaptive quality controller degrades.

### Problem 2: Video Quality Score Hides Root Cause

**Current**: `videocall_video_quality_score` is a composite score that says "quality is bad" but not why.

**Fix**: Expose the component inputs alongside the composite:
- `videocall_video_fps_ratio` (new) -- ratio of received FPS to target FPS, the primary degradation trigger
- `videocall_video_bitrate_ratio` (new) -- ratio of actual to ideal bitrate
- Keep the composite score for alerting, but add the components for diagnosis

### Problem 3: No Screen Share Metrics At All

**Current**: `videocall_adaptive_video_tier` tracks camera tier only. Screen share has its own separate `EncoderBitrateController` with its own tier, but it's not reported.

**Fix**: Add these metrics to the HealthPacket protobuf and metrics_server:
- `videocall_adaptive_screen_tier` -- screen share quality tier (0=high 1080p, 1=medium 720p, 2=low 480p)
- `videocall_screen_sharing_active` -- whether the user is currently screen sharing (boolean gauge)

Without these, we cannot even tell from Grafana whether screen share is happening, let alone diagnose its quality.

### Problem 4: Adaptive Quality Controller Decisions Are Invisible

**Current**: We can see the output (`videocall_adaptive_video_tier` = 7/minimal) but not the inputs or reasoning.

**Fix**: Add sender-side encoder metrics to HealthPacket:
- `videocall_encoder_fps_ratio` -- the fps_ratio value the controller is acting on
- `videocall_encoder_bitrate_ratio` -- the bitrate_ratio value
- `videocall_encoder_worst_peer_fps` -- the FPS from the worst-performing receiver
- `videocall_encoder_output_fps` -- actual frames/sec the encoder is producing (local measurement, not receiver-reported)
- `videocall_encoder_target_bitrate_kbps` -- what the PID controller told the encoder to target

These are the values that drive every tier transition. Without them, investigating quality degradation requires console logs.

### Problem 5: No Decoder Health Metrics

**Current**: Decoder errors on the receiving side (`[WORKER] WebCodecs decoder error`) are only visible in console logs. When a decoder errors and resets, it reports 0 FPS to the sender, triggering false degradation.

**Fix**: Add per-peer decoder metrics:
- `videocall_decoder_errors_total` -- cumulative decoder error count (per from_peer)
- `videocall_decoder_state` -- current decoder state (0=configured, 1=closed/replacing)

This would let us identify which receiver's decoder errors are causing sender degradation.

---

## What Should Be Enhanced

### Enhancement 1: Join-Time Network Probe Results

**Current**: We have `videocall_client_active_server_rtt_ms` (ongoing RTT) but no record of the participant's connection quality at join time.

**Proposed**: During the lobby/pre-join phase, run a brief bandwidth and quality probe to the relay server. Record:
- `videocall_join_bandwidth_kbps` -- measured upstream bandwidth to relay at join time
- `videocall_join_rtt_ms` -- RTT to relay at join time
- `videocall_join_jitter_ms` -- jitter measured during probe
- `videocall_join_packet_loss_pct` -- loss measured during probe

This serves two purposes:
1. **Diagnostics**: When investigating a call, we'd know each participant's actual connection quality to our server (not to speedtest.net)
2. **Smarter initial tier**: Instead of everyone starting at "medium" and potentially crashing, use probe results to pick a starting tier

### Enhancement 2: Tier Transition Event Log

**Current**: Tier changes are only in console logs (`AdaptiveQuality: video stepped DOWN to tier 'low'`).

**Proposed**: Emit a lightweight event metric on each tier transition:
- `videocall_tier_transition_total` (Counter, labels: direction=up/down, stream=camera/screen/audio, from_tier, to_tier, trigger=fps/bitrate)

This would let us query "how many tier transitions happened during this meeting?" and "which trigger caused the most degradation?" without console logs.

### Enhancement 3: Separate Camera vs Screen FPS in Receiver Reports

**Current**: `videocall_video_fps` uses `from_peer`/`to_peer` labels but doesn't distinguish camera from screen share. If a sender has both active, the FPS of each stream is conflated.

**Proposed**: Add a `stream_type` label (camera/screen) to `videocall_video_fps`, `videocall_video_bitrate_kbps`, `videocall_video_quality_score`, and `videocall_video_frames_dropped`.

### Enhancement 4: Grafana Dashboard Panel for Adaptive Quality Investigation

**Current**: The "Meeting Investigation" dashboard has 30+ panels but none that show the adaptive quality controller's decision chain.

**Proposed**: Add a panel group "Adaptive Quality Debugging" with:
- **Tier timeline**: `videocall_adaptive_video_tier` and `videocall_adaptive_screen_tier` over time per sender
- **Decision inputs**: `videocall_encoder_fps_ratio` and `videocall_encoder_bitrate_ratio` overlaid
- **Worst peer**: `videocall_encoder_worst_peer_fps` identifying the dragging receiver
- **Decoder health**: `videocall_decoder_errors_total` rate per receiver

This panel group would let us diagnose the exact scenario from 2026-04-13 entirely from Grafana, without needing console logs.

---

## Implementation Priority

| Priority | Item | Effort | Impact | Status |
|----------|------|--------|--------|--------|
| **P0** | Add `videocall_encoder_fps_ratio` and `videocall_encoder_worst_peer_fps` | Low | Directly exposes the root cause signal that was invisible | **Done** — PR #308 |
| **P0** | Add `videocall_adaptive_screen_tier` and `videocall_screen_sharing_active` | Low | Screen share is a primary use case with zero observability | **Done** — PR #308 |
| **P1** | Rename `audio_packet_loss_pct` to `audio_concealment_pct` | Low | Prevents the most common misdiagnosis | **Done** — PR #308 (emitted alongside old metric for backward compat) |
| **P1** | Add `videocall_encoder_output_fps` and `videocall_encoder_target_bitrate_kbps` | Low | Completes the encoder decision chain | **Done** — PR #308 |
| **P1** | Add `videocall_decoder_errors_total` | Low | Identifies receivers causing false feedback | **Done** — PR #308 |
| **P2** | Join-time network probe metrics | Medium | Enables smarter initial tier selection | **Deferred** — requires server-side relay probe endpoint; proto fields 33-36 reserved |
| **P2** | Tier transition event counter | Low | Aggregate view of quality stability | **Done** — PR #308 (`videocall_tier_transition_total`, 9-label CounterVec) |
| **P2** | Camera vs screen video metrics | Medium | Separates camera from screen in all queries | **Done** — PR #308 (separate `videocall_screen_video_{fps,bitrate_kbps}` metrics, non-breaking) |
| **P3** | Grafana dashboard panel group | Low | Depends on P0/P1 metrics being available | **Done** — PR #308 (collapsed "Adaptive Quality Debugging" row, 4 panels) |

---

## Implementation Notes (2026-04-13)

**PR #308** (`fix/metrics-observability-gaps` → `PR-staging`) implemented all items except the join-time network probe:

- **P0/P1 (9 new metrics)**: Encoder decision inputs (`fps_ratio`, `bitrate_ratio`, `worst_peer_fps`, `output_fps`, `target_bitrate_kbps`), screen share state (`screen_sharing_active`, `adaptive_screen_tier`), `decoder_errors_total`, and `audio_concealment_pct` (new name alongside old `audio_packet_loss_pct`).
- **P2 tier transitions**: `TierTransitionRecord` captured at all 6 transition points in `AdaptiveQualityManager`, drained through shared buffers to health packet, mapped to `videocall_tier_transition_total` Prometheus CounterVec with direction/stream/from_tier/to_tier/trigger labels.
- **P2 stream discrimination**: Split `VideoStats` into camera and screen buckets in health_reporter using `media_type` DiagEvent routing. Added `screen_video_stats` protobuf field. Separate `videocall_screen_video_fps` and `videocall_screen_video_bitrate_kbps` metrics (existing camera metrics unchanged — backward compatible).
- **P3 dashboard**: Collapsed "Adaptive Quality Debugging" row with 4 panels: Tier Timeline, Decision Inputs (with threshold lines), Worst Peer FPS, Decoder Errors.

**Review findings fixed during implementation**:
- Removed `> 0.0` guards on `encoder_fps_ratio`, `encoder_bitrate_ratio`, `encoder_target_bitrate_kbps` that suppressed the exact zero values needed to diagnose the scrum call scenario.
- Fixed `AUDIO_CONCEALMENT_PCT` gauge never resetting to zero when concealment clears.

**What remains**:
- **Join-time network probe** (P2, deferred): Requires new server-side relay probe endpoint. Proto fields 33-36 reserved for `join_bandwidth_kbps`, `join_rtt_ms`, `join_jitter_ms`, `join_packet_loss_pct`.
- **The worst-peer feedback loop bug itself**: This PR adds observability, not the algorithmic fix. Fixing the root cause (replacing `get_worst_fps_peer()` with median/percentile aggregation, filtering stale receivers, etc.) is separate future work.

---

## Summary

We have **50 Prometheus metrics** covering relay health, receiver-side quality, and connection state. For the 2026-04-13 investigation:

- **6 metrics were helpful** (ruled out server issues, established context)
- **4 metrics were misleading** (pointed toward bandwidth/network as root cause)
- **7 critical questions were unanswerable** (all related to WHY the encoder degraded)

The fundamental gap: our metrics observe what the **relay did** (clean) and what **receivers experienced** (bad), but the **encoder's decision-making** -- the layer where the actual bug lives -- was completely invisible.

**After PR #308**: All 7 previously unanswerable questions now have corresponding Prometheus metrics and Grafana panels. The 2026-04-13 incident can be diagnosed entirely from the "Adaptive Quality Debugging" dashboard row without console logs. The misleading `audio_packet_loss_pct` metric now has a correctly named counterpart (`audio_concealment_pct`). Total client metrics increased from ~50 to ~65.

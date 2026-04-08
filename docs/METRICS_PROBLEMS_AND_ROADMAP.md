# videocall-rs Metrics: Problems Uncovered, Work Done, and Path Forward

**Date**: 2026-03-09
**Authors**: Engineering / Jay Boyd
**Scope**: Covers original metric failures, all changes since origin/main, and what vcprobe + Grafana can now determine about call quality.

---

## 0. Transport Architecture

videocall-rs uses **WebTransport** (primary) with **WebSocket** as a fallback transport. Media flows client → server → client (server relay); there is no peer-to-peer connection between participants.

| | WebRTC (for reference) | videocall-rs |
|---|---|---|
| Transport | DTLS/SRTP over ICE | WebTransport / WebSocket |
| Topology | Peer-to-peer (or SFU via RTP) | Client-to-server-to-client |
| NAT traversal | ICE + STUN/TURN | Not needed (server relay) |
| Stats API | `RTCPeerConnection.getStats()` | None — custom diagnostic system |
| Congestion control | Browser-native (REMB/TWCC) | Application-level |

Because there is no `RTCPeerConnection`, WebRTC-specific stats APIs (`qualityLimitationReason`, RTP counters, ICE candidate stats, `nackCount`/`pliCount`/`firCount`, `availableOutgoingBitrate`) are not available. All quality signals are built from custom application-level instrumentation using the diagnostics that are available in the WebTransport/WebSocket/WASM stack.

### What Can Be Collected

| Metric | API/Source | Available |
|---|---|---|
| RTT to server | Custom timestamp + echo | Yes |
| Audio jitter (delay manager) | NetEQ `target_delay_ms` | Yes |
| Audio concealment rate | NetEQ `expand_per_sec` | Yes |
| Audio buffer depth | NetEQ `current_buffer_size_ms` | Yes |
| Audio packets/sec | NetEQ `packets_per_sec` | Yes |
| Video FPS | Custom frame counter in WebCodecs callbacks | Yes |
| Video bitrate | Custom byte counter per frame | Yes |
| Video decode errors/sec | Custom counter in decode path | Yes |
| Tab visibility | `document.hidden` | Yes (universal) |
| JS heap memory | `performance.memory` | Yes (Chrome only) |
| Send queue depth | `WebSocket.bufferedAmount` | Yes (WebSocket only; WebTransport lacks this) |
| Packets sent/received per sec | Custom atomic counters in connection layer | Yes |
| Encode latency | Custom `performance.now()` timestamps around WebCodecs `encode()` | Future |
| Decode latency | Custom timestamps in decode path | Future |
| CPU usage % | None | No browser API |
| Network quality estimate | None | No browser API |

---

## 1. Baseline: What origin/main Had

Before any of the work described below, the codebase had:

- A basic `HealthPacket` protobuf schema with RTT, FPS, bitrate (kbps), and raw NetEQ stats
- A `DiagnosticsManager` that tracked frames decoded and bitrate per peer
- A `HealthReporter` that aggregated those stats every 5 seconds and published to NATS
- The `metrics_server` that consumed NATS health packets and exposed a few Prometheus counters
- No `vcprobe` tool — quality monitoring required reading raw Prometheus or NATS messages directly
- No cross-participant quality correlation

**State of trust in existing metrics**: Low. Several metrics were broken or misleading in ways that were not documented and were actively causing misdiagnosis.

---

## 2. Bugs Found in the Original Metrics

### 2.1 Bitrate Always Reported as 0

**File**: `videocall-client/src/health_reporter.rs`

The `DiagnosticsManager` published bitrate as `MetricValue::F64`, but `HealthReporter` only matched `MetricValue::U64`. The `F64` arm was silently dropped.

**Effect**: Video bitrate showed 0 kbps even during active streaming sessions. This masked bandwidth consumption and made it impossible to tell whether a participant's poor video was due to low bitrate encoding vs. decode/render problems.

**Fix applied**: Added the `F64` match arm, casting to u64.

---

### 2.2 Session ID vs. Email Identity Mismatch

**Files**: `videocall-client/src/decode/peer_decode_manager.rs`, `vcprobe/src/state.rs`

The `DiagnosticsManager` tracked peers by `session_id` (a numeric u64, e.g. `10386015757777453282`) because `peer_decode_manager.rs` used `peer_session_id.to_string()` as the key. But all other participant tracking — meeting participant lists, MEDIA packets, event logs — used the email string (e.g. `"jay2@example.com"`).

**Effect**: The `peer_stats` map inside `HealthPacket` was keyed by session IDs, but any consumer trying to join quality data to a participant name could not do so without a separate mapping layer.

**Fix applied**: `vcprobe` builds a `session_to_email` HashMap by observing MEDIA packets (which carry both session_id and email) and uses it to resolve quality data to display names. The underlying client-side root cause (using session_id instead of email in the diagnostics key) is documented but not yet changed.

---

### 2.3 Lifetime Counters Frozen After Early Events

**Files**: `videocall-client/src/health_reporter.rs`, `videocall-client/src/diagnostics/diagnostics_manager.rs`

Two metrics used **cumulative lifetime counters** that never reset, producing permanently frozen values after any early incident:

#### Audio Packet Loss %

Original formula:
```
packet_loss_pct = (lifetime_concealment_events / lifetime_packets_received) * 100
```

NetEQ's lifetime counters never reset for the session. If a participant had 5% loss in the first 10 seconds and then the network recovered perfectly, the displayed percentage would be stuck at ~5% for the entire call — because the numerator and denominator only accumulated, never zeroed.

**Observed in the field**: Jay2 showed 39.7% packet loss constantly across 4+ minutes of stable network. The loss had occurred early in the session.

**Fix applied**: Changed to windowed rates that reset every ~1 second:
```
packet_loss_pct = (expand_per_sec / packets_per_sec) * 100
```
Both `expand_per_sec` (concealment operations/sec) and `packets_per_sec` are derived from NetEQ's rolling 1-second window counters.

#### Frames Dropped

Original: a global `Arc<AtomicU32>` that `fetch_add(1)` on every dropped frame, never reset.

**Effect**: A participant who dropped 2 frames at call start would show "2 dropped" for the entire call. The metric was useless for detecting current decode stress.

**Fix applied**: Moved to a per-peer `decode_errors_count` inside `FpsTracker` that resets every 1 second alongside the FPS calculation. Now reported as `decode_errors_per_sec` (a windowed rate). Note: this counts codec errors (keyframe miss, parse error, decoder reset), not CPU-pressure-driven throughput drops — see §6 Bug A.

---

### 2.4 Jitter Column Was Not Jitter

**File**: `vcprobe/src/proctor.rs`, `vcprobe/src/display.rs`

The column labeled "Jitter" in the vcprobe TUI was displaying `current_buffer_size_ms` — the **current occupancy of the NetEQ jitter buffer** (typically 80–120ms during normal audio playback). This is not a network jitter measurement. It is how much audio data is currently queued in the buffer, and it is expected to be non-zero.

**Effect**:
- Confused operators and developers into thinking calls had 80–160ms "jitter" at all times
- The value does not meaningfully increase during packet loss (the buffer drains, it does not fill)
- The VoIP industry threshold "< 30ms jitter for an excellent call" refers to **delay manager target delay**, not buffer occupancy

**The real jitter metric**: NetEQ's `target_delay_ms` — the **delay manager's estimate of how much buffering is needed to absorb observed network jitter**. This metric:
- Is near 0 on LAN/localhost when audio is flowing normally
- Increases when network jitter increases (variable packet arrival times)
- Corresponds to the VoIP thresholds: < 30ms = excellent, < 75ms = acceptable, ≥ 75ms = degraded

**Fix applied**:
- Added `target_delay_ms` field to `NetEqStats` in `health_packet.proto` (field 5)
- Added extraction in `health_reporter.rs` from the NetEQ JSON stats blob
- Updated `vcprobe` to display `target_delay_ms` as "Jitter" and show `current_buffer_size_ms` as "Buf" (secondary, gray)
- Updated color thresholds to match VoIP standards (green < 30ms, yellow < 75ms, red ≥ 75ms)

---

### 2.5 Audio Concealment Showed Alarming Values When Speaker Was Silent

**File**: `vcprobe/src/proctor.rs`, `neteq/src/bin/neteq_worker.rs`

The Opus codec uses **DTX (Discontinuous Transmission)**: when a speaker is silent, the encoder stops sending packets entirely. NetEQ, receiving nothing, runs its **expand** operation at ~100 per second to fill the silence. This causes `expand_per_sec` (the concealment rate) to read as 100+/s even when the network is perfect and the speaker is simply quiet.

**Effect**: Vcprobe showed red "100.0/s" concealment for a muted participant, making it look like severe packet loss when there was none.

**Distinguishing DTX silence from real packet loss**: When DTX is active, `packets_per_sec` drops to ~0. When real packet loss occurs, `packets_per_sec` remains elevated (packets are arriving) while `expand_per_sec` is also elevated (some are lost/late).

**Fix applied**: Added `audio_packets_per_sec` to `QualitySnapshot` in vcprobe. The concealment display now shows "silent" in gray when `packets_per_sec < 2.0 && expand_per_sec > 5.0`, instead of a red alarm value. All quality gating (jitter display, audio quality score, concealment issue detection) is also conditioned on `packets_per_sec >= 2.0`.

---

### 2.6 Quality Scores Persisted After Streams Disabled

**File**: `videocall-client/src/health_reporter.rs`

The `PeerHealthData` struct stored `last_neteq_stats` and `last_video_stats` but had only a single shared `last_update_ms` timestamp. The FPS tracker and NetEQ tracker only emit `DiagEvent`s when data is actively flowing (frames arriving, packets decoding). When a participant turned off their camera or microphone, those trackers simply stopped firing — no "fps = 0" or "packets_per_sec = 0" event was published.

**Effect**: `last_video_stats` retained the last observed fps (e.g., 28 fps) indefinitely. Quality scores remained at their last-good values (e.g., "Video 100, Call 89") for 30+ seconds after a participant disabled audio and video.

**Fix applied**: Added `last_audio_update_ms` and `last_video_update_ms` as separate timestamps to `PeerHealthData`. Quality scores are now gated on freshness: data older than 5 seconds is treated as absent. Within 5 seconds of a stream stopping, the score drops to `None` and vcprobe displays `--` / empty bar.

---

### 2.7 Tab Backgrounding Freezes All NetEQ Diagnostics

**File**: `neteq/src/bin/neteq_worker.rs` (architectural issue, not yet fixed)

The NetEQ WASM decoder runs in a Web Worker driven by a 5ms `setInterval`. When a browser tab is backgrounded (`document.hidden = true`), Chrome throttles JavaScript timers to a minimum of **1000ms**. The `setInterval` at 5ms fires once per second instead of 200 times per second, causing:

1. Audio decode starves — the participant hears silence or severe stuttering
2. The 1000ms stats `setInterval` also stops firing — vcprobe shows frozen/stale jitter values
3. FPS drops to 0-1, RTT may spike, but NetEQ metrics cease updating

This is not a bug in a counter — it is an architectural limitation: the audio receive path runs in a throttleable JavaScript context, unlike WebRTC which uses the OS audio thread.

**Fix documented**: `docs/ISSUE_NETEQ_AUDIOWORKLET_MIGRATION.md` describes migrating NetEQ from a Web Worker into an `AudioWorkletProcessor`, which runs on the OS audio clock and is explicitly exempt from browser throttling. **Not yet implemented** — estimated 1 week of effort.

**Current mitigation**: `is_tab_visible` and `is_tab_throttled` fields in `HealthPacket` allow vcprobe and Grafana to flag when a participant's degraded metrics are likely due to tab backgrounding rather than a real network or hardware problem.

---

## 3. Work Done (Relative to origin/main)

All changes described below are committed and on the local branch.

### 3.1 Initial Commits (origin/main baseline)

#### Commit `d564a38c` — Phase 1 WebTransport/WebSocket Performance Metrics

Added to `health_packet.proto` / `HealthPacket`:
- `is_tab_visible` (bool) — from `document.hidden`
- `memory_used_bytes` (optional u64) — JS heap via `performance.memory` (Chrome only)
- `memory_total_bytes` (optional u64) — heap limit (Chrome only)

Added to `PeerStats`:
- `frames_dropped` (uint64) — initial version, later renamed to `decode_errors_per_sec`
- `audio_packet_loss_pct` (double) — audio loss percentage

Client collection added in `health_reporter.rs`:
- Tab visibility read on each heartbeat
- Memory read via JS reflection
- Audio loss % calculated from NetEQ stats

#### Commit `c5a9767c` — Expose Phase 1 Metrics to Prometheus

Extended `actix-api/src/bin/metrics_server.rs` to extract and expose new Phase 1 fields as labeled Prometheus gauges:
- `videocall_client_tab_visible{meeting_id, session_id, peer_id}`
- `videocall_client_memory_used_bytes{...}`
- `videocall_video_frames_dropped{meeting_id, session_id, from_peer, to_peer}`
- `videocall_audio_packet_loss_pct{...}`

#### Commit `cfb1c20b` — vcprobe NATS Subscriber Mode

Added the entire `vcprobe` crate (did not exist in origin/main):
- NATS subscriber that passively observes a meeting without joining as a participant
- `MeetingState` / `Participant` / `QualitySnapshot` state model
- Session-to-email mapping built from MEDIA packets
- Proctor TUI (interactive terminal UI) for per-peer quality drilling
- Verbose display mode
- Dual transport: NATS passive mode and WebSocket participant mode

---

### 3.2 Quality Diagnosis Work (Subsequent Commits)

#### Commit `1accf603` — Proto: Rename decode errors, add quality scores

- `frames_dropped` → `decode_errors` in `PeerStats` (wire tag unchanged, backward compatible)
- `FrameDropped` → `DecodeError` event variant; `track_frame_dropped()` → `track_decode_error()`
- Added `audio_quality_score`, `video_quality_score`, `call_quality_score` to `PeerStats` (proto3 optional double, 0–100)
- Removed `is_speaking` (unused)

#### Commit `5ec1f2d9` — Diagnostics: decode error tracking, FpsTracker peer leak, dead code

- `frames_dropped_per_sec` → `decode_errors_per_sec` throughout
- Added `DiagnosticEvent::RemovePeer`, `DiagnosticManager::remove_peer()` — called from all three peer removal paths in `PeerDecodeManager`; fixes stale DiagEvents that defeated the freshness gate after peer departure
- Removed dead `get_frames_decoded()` accessor
- Changed `Ordering::SeqCst` → `Ordering::Relaxed` in WASM single-thread context

#### Commit `2ab8c3ff` — Health reporter: quality scores, freshness gates, audio formula fix

**Tier 0 communication load metrics** added to `connection_manager.rs`:
- `packets_received` / `packets_sent` atomic counters, incremented on every binary message
- `get_packets_received_per_sec()` / `get_packets_sent_per_sec()` — windowed rates
- `get_send_queue_depth()` — reads `WebSocket.bufferedAmount`

Reported in `HealthPacket`:
- `send_queue_bytes` (optional u64)
- `packets_received_per_sec` (optional double)
- `packets_sent_per_sec` (optional double)

**Quality scores computed client-side** (by the observer, for each peer they receive):

Audio quality formula (gated on `packets_per_sec >= 2.0`, `audio_fresh`):
```
audio_score = 100
    - min(expand_per_sec / 10.0, 1.0) * 70    # concealment penalty (max -70)
    - min(audio_packet_loss_pct / 5.0, 1.0) * 30  # loss penalty (max -30)
```
Jitter (`target_delay_ms`) was intentionally excluded from the formula: NetEQ's delay manager settles at ~120ms by default in this stack, creating a constant drag that penalizes every call regardless of network conditions. Concealment already captures the downstream effect of real jitter (packets arriving late get concealed).

Video quality formula (gated on `fps > 0.0`, `video_fresh`):
```
fps_score = fps >= 20 ? 100 : fps >= 10 ? 50 + (fps-10)/10*50 : fps/10*50
video_score = max(fps_score - min(decode_errors_per_sec / 10, 1) * 50, 0)
```

Call quality: `min(audio_quality_score, video_quality_score)` — whichever active stream is worse. This is the **primary alerting metric** for Grafana.

**Freshness gates**:
- `last_audio_update_ms` / `last_video_update_ms` per peer
- Scores return `None` (not 0) when data is > 5s old — Grafana shows gaps, not misleading zeros
- `audio_enabled` / `video_enabled` now correctly reflect the sender's self-reported heartbeat state

**`can_listen` / `can_see` removed** from `PeerHealthData` — now computed dynamically from freshness at health packet build time. Dead `mark_audio_timeout()` / `mark_video_timeout()` methods deleted.

#### Commit `8a6e2419` — Prometheus label leak fix

- Changed `tracker.insert()` → `tracker.entry().or_insert_with()` so `SessionInfo` is created once and `to_peers` sets accumulate across all health packets in a session (was being unconditionally overwritten on each packet)
- Fixed pre-existing crash: `peer_data.frames_dropped` (nonexistent proto field) → `peer_data.decode_errors_per_sec`

#### Commit `9fd3e3dc` — vcprobe: Aud/Vid/Call columns, quality sort, help screen

Main participant table layout (10 columns):

| Column | Width | Content |
|---|---|---|
| Name | Min 18 | Display name, colored by call quality |
| [V][M] | 11 | Video/audio enabled badges |
| RTT | 8 | Server round-trip time |
| Conc/s | 8 | Concealment/s or "silent" for DTX |
| FPS | 5 | Video frames/sec |
| kbps | 6 | Video bitrate |
| Aud | 5 | Audio quality score 0–100 |
| Vid | 5 | Video quality score 0–100 |
| Call | 5 | Call quality score 0–100 |
| Bar | 12 | Quality bar (█░) |

Scores color-coded: green ≥ 75, yellow ≥ 40, red < 40, gray `--` when absent.

Sort: by `call_quality_score` from health packet when present; falls back to local `quality_score()` for old clients.

Help screen updated with accurate metric descriptions, formulas, and quality thresholds.

#### Commit `5042952f` — Duplicate DiagEvent, packet rate symmetry

- Removed duplicate "decoder" subsystem `DiagEvent` (was a no-op debug sink after the `can_listen`/`can_see` refactor); kept only the "video" event, adding `media_type` field to it
- Added `packets_sent.fetch_add(1, Relaxed)` in `send_rtt_probe()` — RTT probes were counted in `packets_received` (inbound echoes) but not `packets_sent` (outbound probes), making the rates incomparable

---

## 4. What vcprobe Can Now Determine

vcprobe connects to NATS and passively observes a meeting. From that position, it can now answer:

### Per-Participant at a Glance (Main Table)

| What you see | What it means |
|---|---|
| Name color (green/yellow/red) | Overall call quality from an observer's perspective |
| Quality bar (█░) | `call_quality_score` 0–100 |
| [V][M] badges | Video and audio enabled (from HEARTBEAT packets) |
| WT | Transport type: ✓ if WebTransport, blank if WebSocket |
| RTT | Self-reported server round-trip time (active connection) |
| Conc/s or "silent" | Audio concealment rate; "silent" means DTX, not loss |
| FPS | Video frames/sec observed by a peer |
| kbps | Video bitrate observed by a peer |
| Aud | Audio quality score (0–100): concealment + packet loss |
| Vid | Video quality score (0–100): FPS + decode errors |
| Call | Call quality score (0–100): min(Aud, Vid) — primary alerting metric |

### Per-Participant Drill-Down (Detail Panel, press Enter)

- Score breakdown: Audio X / Video Y / Call Z (each 0–100, color-coded)
- Buf: `current_buffer_size_ms` — jitter buffer occupancy
- Jitter: `target_delay_ms` — delay manager's network jitter estimate
- Decode Errors/sec: windowed rate (resets every 1s)
- Audio Packet Loss %: windowed rate
- Tab: Visible/Hidden + throttling state
- Memory: JS heap used (Chrome only)
- Connection type: WebTransport vs WebSocket
- Packets received/sent per second (communication load)
- Send queue depth (bytes) — send-side backpressure indicator

### Meeting-Level Observations

- If 80%+ of participants have high RTT simultaneously: server or upstream network issue, not client issue
- If one participant shows high concealment + low audio quality while others are fine: client network problem
- If one participant shows low FPS + high decode errors + others see them fine: their decoder/CPU is overwhelmed
- If a participant's metrics are stale/frozen and `is_tab_visible = false`: backgrounded tab, ignore until they return

### What vcprobe Cannot Tell You (Current Limitations)

- **Root cause of video packet loss**: WebCodecs API is frame-level only; no packet counters for video
- **CPU %**: No browser API exposes this
- **Self-perceived quality**: The quality scores are observer-reported (how Jay1 receives Jay2), not what Jay2 experiences
- **Send-side WebTransport queue depth**: WebTransport API does not expose total `bufferedAmount`
- **Metrics for solo users**: `HealthPacket` is only sent when observing at least one peer

---

## 5. What Grafana Should Be Able to Do

With all current changes deployed:

### Time-Series Quality Dashboards

- `call_quality_score` per participant per meeting — primary health signal
- Audio and video quality scores separately for root cause analysis
- `target_delay_ms` trending — early warning for network degradation
- Audio concealment rate trending (gated, ignores DTX silence)
- Decode errors per second — GPU/decode stress indicator

### Gaps vs. Zeros

All quality score fields are `optional` in proto3. When a stream is inactive:
- `audio_quality_score` is absent — Grafana shows a **gap** in the time series
- Not 0, which would look like a catastrophic failure

### Alerting

Recommended alert rules:
1. `call_quality_score < 40` for > 30s → Warning
2. `call_quality_score < 20` for > 10s → Critical
3. `target_delay_ms > 150` for > 60s → Network jitter alert
4. `audio_packet_loss_pct > 10%` for > 20s → Packet loss alert
5. `send_queue_bytes > 100000` for > 30s → Client bandwidth saturation
6. Meeting-wide: avg RTT > 200ms across ≥ 80% of participants → Server/upstream issue

### Prometheus Metrics Status

Currently exposed (needs deployment of recent commits):
- `videocall_client_tab_visible{meeting_id, session_id, peer_id}`
- `videocall_client_memory_used_bytes{...}`
- `videocall_video_frames_dropped{...}` (note: still exposes lifetime counter — should be updated to rate)
- `videocall_audio_packet_loss_pct{...}`

**Not yet exposed to Prometheus**: `audio_quality_score`, `video_quality_score`, `call_quality_score`. Adding these to `actix-api/src/bin/metrics_server.rs` is the next highest-value instrumentation step.

---

## 6. Bugs Fixed (2026-03-10): vcprobe Identity and Display Name

### 6.0 HealthReporter Session ID Never Updated (4 rows for 2 users)

**Files**: `videocall-client/src/health_reporter.rs`, `videocall-client/src/client/video_call_client.rs`

**Root Cause**: `HealthReporter::new()` initializes `session_id` to a placeholder string of the form `"session_{unix_secs}"`. The server assigns the real numeric session_id via a `SESSION_ASSIGNED` response message. While `video_call_client.rs` received this event and called `reporter.set_session_id()` on the struct, the running health-reporting timer loop had already **captured the session_id string by value** inside its `spawn_local` closure at startup — so the struct field update was invisible to the loop.

**Effect**: Every `HealthPacket` carried `session_id = "session_1741234567"` (the placeholder), while every `PacketWrapper` in MEDIA traffic carried the real numeric session_id (e.g. `"10386015757777453282"`). vcprobe keyed participants by session_id and therefore created **two separate participant rows per user**: one from MEDIA traffic (correct numeric key) and one from HEALTH traffic (wrong placeholder key). A 2-user meeting showed 4 rows.

**Fix applied**: Changed `session_id: String` to `session_id: Rc<RefCell<String>>` in `HealthReporter`. The timer closure captures a `Weak<RefCell<String>>` and upgrades it each tick:

```rust
let session_id = Rc::downgrade(&self.session_id);
// Inside spawn_local loop:
let session_id_val = match Weak::upgrade(&session_id) {
    Some(s) => s.borrow().clone(),
    None => break,
};
```

`set_session_id()` now does `*self.session_id.borrow_mut() = session_id`, which the running loop sees on the next tick.

---

### 6.1 Display Names Not Appearing in vcprobe

**Files**: `vcprobe/src/state.rs`, `videocall-client/src/health_reporter.rs`, `protobuf/types/health_packet.proto`, `dioxus-ui/src/components/attendants.rs`, `dioxus-ui/src/pages/meeting.rs`

**Root Cause — two separate problems**:

1. **NATS replay gap**: NATS standard does not replay historical messages. When vcprobe starts mid-meeting, the `PARTICIPANT_JOINED` system events that carried participant display names have already been published and are gone. vcprobe would only receive display names for participants who joined *after* vcprobe started.

2. **Race condition**: Even when vcprobe starts before participants join, the PARTICIPANT_JOINED NATS event may arrive before vcprobe has created the participant entry in its `MeetingState` (which only happens on the first MEDIA or HEALTH packet from that session). The display name would be received but discarded with nowhere to store it.

**Fix applied — two-part**:

**Part A — `display_name` in every HealthPacket (field 19)**:

Added to `health_packet.proto`:
```protobuf
optional string display_name = 19;
```

`VideoCallClientOptions` gained a `pub display_name: String` field. `HealthReporter` carries the display name as a captured `String` in the timer closure and passes it to `create_health_packet()` as a `&str` parameter. Every health packet now carries the user's display name. vcprobe uses field 19 as the **primary** display name source — works even when vcprobe started after join events.

**Part B — `pending_display_names` side-map in vcprobe**:

Added `pending_display_names: HashMap<String, String>` to `MeetingState`. On every `PARTICIPANT_JOINED` event, the display name is stored in this map unconditionally (regardless of whether a participant entry exists yet). When a new participant is first seen via MEDIA or HEALTH, the pending map is checked and the name is applied immediately. This eliminates the race condition for participants who join while vcprobe is running.

**Fallback order in `process_health()`**:
```rust
if p.display_name.is_none() {
    if let Some(dn) = health.display_name.as_deref().filter(|s| !s.is_empty()) {
        p.display_name = Some(dn.to_string());   // Primary: field 19
    } else if let Some(dn) = self.pending_display_names.get(&session_id) {
        p.display_name = Some(dn.clone());         // Fallback: PARTICIPANT_JOINED
    }
}
```

**Note on display name changes**: When a user changes their display name in the dioxus UI, the meeting component reconstructs and they effectively leave and rejoin the meeting. vcprobe will receive a new `PARTICIPANT_JOINED` event and then new health packets with the updated `display_name` field — so name changes propagate correctly without any extra handling.

---

## 7. Known Bugs Fixed (from code review 2026-03-07)

See full review: `docs/CODE_REVIEW_METRICS_2026_03_07.md`

### ~~Bug A: `frames_dropped_per_sec` Is Actually Decode Errors~~ — **FIXED**

The metric was incremented on `peer.decode()` errors in `peer_decode_manager.rs`. A decode error triggers a codec reset, not a throughput-driven frame drop. Renamed throughout to `decode_errors_per_sec`. True CPU-driven frame drop detection would require `VideoDecoder.decodeQueueSize` monitoring (not yet implemented).

### ~~Bug B: FpsTracker Entries Never Removed for Departed Peers~~ — **FIXED**

`DiagnosticManager.fps_trackers` entries were added when frames arrived but never removed. When a peer left, their entry persisted and continued broadcasting stale DiagEvents every 500ms, defeating the 5-second freshness gate. Fixed by adding `DiagnosticEvent::RemovePeer` and calling `remove_peer()` from all three peer removal paths.

### ~~Bug C: Prometheus Per-Peer Metrics Never Cleaned Up~~ — **FIXED**

`tracker.insert()` unconditionally replaced `SessionInfo` (including `to_peers`) on every health packet, so the set only ever reflected the most recent packet. Fixed with `entry().or_insert_with()` so sets accumulate across the session lifetime and cleanup correctly removes all peers ever observed.

### ~~Bug D: `can_listen`/`can_see` Flags Never Reset on Stream Stop~~ — **FIXED (differently)**

`mark_audio_timeout()` / `mark_video_timeout()` existed but were never called. Fixed by removing the flags entirely — `can_listen`/`can_see` are now computed dynamically from freshness timestamps at health packet build time.

### ~~P2: Duplicate DiagEvent per Peer per Heartbeat~~ — **FIXED**

`send_diagnostic_packets()` was emitting both "decoder" and "video" subsystem events with identical data per peer. The "decoder" handler was a no-op after the `can_listen`/`can_see` refactor. Removed the "decoder" event; kept only the "video" event with `media_type` added.

### ~~P2: RTT Packet Counter Asymmetry~~ — **FIXED**

`packets_received` counted inbound RTT echoes but `packets_sent` did not count outbound RTT probes, making the send/receive rates incomparable. Added `packets_sent.fetch_add(1, Relaxed)` in `send_rtt_probe()`.

### Note: Audio Packet Loss % is a Proxy, Not True Loss

The formula `expand_per_sec / packets_per_sec` counts NetEQ expand operations (which include late arrivals and jitter underruns) as "loss." It will show transient false positives when a speaker unmutes after DTX silence. True packet loss is not observable at the transport layer in WebTransport/WebSocket. This is the best available signal in this architecture and should be labeled accordingly in dashboards.

---

## 8. Quality Scoring Philosophy (2026-03-11)

### The Core Distinction: Hardware Capability vs. Call Health

The original video quality score answered **"how good is your hardware?"** The call quality score should answer **"is your call working correctly?"** These are different questions.

| Situation | Old Score | New Score | Reason |
|---|---|---|---|
| Logitech camera, stable 15fps, audio perfect | 76/100 | ~100/100 | 15fps is hardware capability, not a problem |
| 30fps camera with 10% packet loss | ~85/100 | ~45/100 | Audio health tanks from loss → call is breaking |
| Camera near-frozen at 2fps | ~20/100 | ~20/100 | Near-frozen video is a real problem |
| No video (camera off) | — | — | Absent (not zero) — audio score alone drives call score |

The Logitech case is the archetypal example: a camera with auto-exposure reducing to 15fps in lower light conditions was scored 76/100, implying a degraded call. The call was working perfectly. The score was misleading.

### What Actually Degrades Call Experience

Ranked by user impact:

1. **Audio breaking up** — most disruptive. Users tolerate choppy video; they cannot tolerate choppy audio.
2. **Network congestion** — high RTT/jitter affects conversational feel (captured via concealment proxy in audio score).
3. **Video freezing** — not "low fps" but a *sudden drop* or frames being dropped mid-call.
4. **No video at all** — camera failed or disabled.

**Stable 15fps is not on this list.** A 15fps camera that has delivered 15fps since call start is behaving correctly for its hardware. The browser may auto-reduce FPS for any number of reasons (low light auto-exposure, USB bandwidth, thermal throttling) that are outside the user's control and not indicative of a call problem.

### Why Not Fix the Camera's FPS?

Adding `frameRate: { ideal: 30 }` to `getUserMedia` constraints was considered and rejected for this use case:

- **Low-light auto-exposure**: The camera firmware decides to drop FPS to allow longer exposure time for a brighter image. This is a hardware/driver decision that happens *after* the browser grants the stream. `getUserMedia` hints don't override it.
- **Could throttle good cameras**: `ideal: 30` might cap 60fps cameras at 30fps (the browser prefers values closest to the stated ideal).
- **Not the right abstraction layer**: WebRTC systems fix this via adaptive bitrate with RTCP feedback loops. We use WebTransport/WebSocket without that mechanism. The right answer is to not penalize hardware limitation in the quality score.

### Why Not Fix the getUserMedia Frame Rate?

`frameRate: { ideal: 30 }` was considered and rejected:

1. Low-light auto-exposure happens *after* stream negotiation — the camera firmware doesn't honor getUserMedia hints when making exposure decisions.
2. `ideal: 30` might inadvertently cap 60fps cameras at 30fps.
3. The score is the correct layer to fix — penalizing hardware capability is the wrong abstraction.

### Revised Scoring Formula

**Audio quality** (unchanged — already correctly reflects call health):

```
audio_score = 100
    - min(conceal_per_sec / 10.0, 1.0) * 70    # concealment penalty (max -70)
    - min(audio_loss_pct   /  5.0, 1.0) * 30    # loss penalty       (max -30)
```

Gated on `packets_per_sec >= 2.0` (ignores DTX silence) and `audio_fresh` (data < 5s old).

**Video quality** (revised — measures video health, not FPS quality):

```
video_health = fps >= 5.0 ? 100.0 : fps / 5.0 * 50.0
decode_penalty = min(decode_errors_per_sec / 10.0, 1.0) * 50.0
video_score = max(video_health - decode_penalty, 0.0)
```

Key change: any stable FPS ≥ 5 scores 100. Only near-frozen video (1–4fps) is penalized on a 0–50 scale. Decode errors (codec resets, keyframe misses) still apply a separate penalty.

Gated on `fps > 0.0` and `video_fresh` (data < 5s old).

**Call quality** (unchanged — worst of active streams):

```
call_score = min(audio_score, video_score)   # when both present
           = audio_score                      # when video absent (camera off)
           = video_score                      # when audio absent (mic off)
```

`min` is deliberate: the call is only as good as the worst active stream. A 100/100 audio score does not compensate for a near-frozen video stream.

### FPS as a Diagnostic, Not a Score Driver

FPS remains displayed in vcprobe as a raw metric — it is **diagnostic context**, not a quality penalty. When an operator sees:

```
Jay     15fps  45ms  100/100  ✅
```

They know: Jay's camera delivers 15fps (hardware context), the call is healthy (score). If they want to understand why the FPS is lower than expected, they can drill into the detail panel.

Contrast with a real problem:

```
Jay      2fps  45ms   20/100  🔴  Video near-frozen
```

Here, 2fps is below the threshold where it represents a functional call (score 20), and the diagnosis label explains why the score is low.

---

## 9. Open Work Items

| Item | Status | Effort |
|---|---|---|
| Push local branch to remote | Pending | trivial |
| ~~vcprobe shows 4 rows for 2 users (session_id placeholder bug)~~ | **Fixed 2026-03-10** | — |
| ~~Display names not appearing in vcprobe~~ | **Fixed 2026-03-10** | — |
| Add quality scores to Prometheus metrics_server | Not started | 2–3 hours |
| Fix session_id → email at source (`peer_decode_manager.rs`) | Not started | 2–4 hours |
| Send HEALTH when peer map is empty (solo monitoring) | Not started | 30 min |
| AudioWorklet migration for tab-throttling fix | Not started | ~1 week |
| Grafana dashboard with quality score panels and alert rules | Not started | 2–3 days |
| Diagnosis rule engine in vcprobe (network congestion, CPU overload, etc.) | Not started | 3–5 days |
| Expose `decode_errors_per_sec` rate in Prometheus (currently lifetime) | Not started | 1–2 hours |

---

## 10. Metrics Trust Summary

| Metric | Was it trustworthy on origin/main? | Status now |
|---|---|---|
| RTT | Yes | Unchanged |
| FPS | Yes | Unchanged |
| Bitrate (kbps) | **No — always 0** | Fixed (F64 arm added) |
| Jitter (buffer size) | **Misleading label** | Renamed to "Buf"; real jitter now shown |
| Real jitter (target_delay_ms) | **Not exposed** | Added to proto + client + vcprobe |
| Audio concealment rate | Yes (metric correct, display misleading for DTX) | DTX silence now distinguished |
| Audio packet loss % | **No — lifetime counter, frozen** | Fixed to windowed rate |
| Decode errors/sec (was "frames dropped") | **Misnamed — counted codec resets, not CPU drops** | Renamed; meaning now accurate |
| Audio quality score | **Did not exist** | Added (0–100, concealment 70% + loss 30%) |
| Video quality score | **Did not exist** | Added (0–100, FPS-based minus decode errors) |
| Call quality score | **Did not exist** | Added (min of audio + video) |
| Tab visibility | Did not exist | Added (Phase 1) |
| Memory usage | Did not exist | Added (Phase 1, Chrome only) |
| Packet counters (rx/tx/sec) | Did not exist | Added |
| Send queue depth | Did not exist | Added (WebSocket only) |

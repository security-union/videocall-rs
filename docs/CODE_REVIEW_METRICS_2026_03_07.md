# Code Review: Metrics & Diagnostics Changes
**Date**: 2026-03-07
**Scope**: All changes relative to origin/main — 3 committed-but-unpushed commits + current uncommitted working tree
**Focus**: Correctness of metric collection in WASM/WebTransport/WebSocket context, metric reliability, and diagnostic value

---

## Executive Summary

The work is directionally correct and the most important problems (lifetime counters, jitter mislabeling, quality score compute location) are solved well. However, there are **two significant correctness bugs** that affect metric reliability, and several smaller issues worth addressing before merging. The quality score architecture (client-side computation, observer-reported, optional on inactivity) is the right design for this stack.

Severity legend: **[CRITICAL]** data is wrong or misleading / **[MEDIUM]** correctness gap, workaround exists / **[LOW]** style, minor waste, or documentation

---

## Bug 1 [CRITICAL]: Frame Drop Metric Counts Decode Errors, Not Real Drops

**File**: `videocall-client/src/decode/peer_decode_manager.rs:544-550`

```rust
Err(e) => {
    // Phase 1 FIXED: Track frame drops per peer with windowed counter
    // Default to VIDEO since that's where frame drops typically occur
    if let Some(diagnostics) = &self.diagnostics {
        diagnostics.track_frame_dropped(&peer.sid_str, MediaType::VIDEO);
    }
    peer.reset().map_err(|_| e)
}
```

**Problem**: This counts any `peer.decode()` error as a "dropped frame." A decode error is a codec failure — typically a parse error, keyframe miss, or state machine error that triggers a decoder reset. It is not the same as a frame being dropped because the system can't keep up with the incoming rate.

Real "frame drops" that indicate client overload are:
- Decoder queue overflow (WebCodecs `VideoDecoder` explicitly drops when too many frames queued)
- Deliberate discards under CPU pressure
- Buffer timeout discards in a jitter buffer

A decode error causes a `peer.reset()` (decoder flush), then decoding continues normally. These are infrequent, correctional events. CPU-overload frame drops are high-frequency and sustained.

**Effect**: Under normal conditions `frames_dropped_per_sec` will read ~0 because decode errors are rare. Under severe packet loss (which causes keyframe miss → reset cycles), it will spike — correctly indicating a problem but attributing it to the wrong cause. The metric is not useless but is misnamed and misinterpreted.

**Additionally**: The media type is hardcoded to `MediaType::VIDEO` regardless of what actually failed. An audio decode error would be tagged as a video drop.

**What a correct implementation would require**: WebCodecs does not expose a "frames dropped" event from `VideoDecoder`. The closest available signal is `VideoDecoderConfig.latencyMode = "realtime"` causing the decoder to drop frames silently, which is not observable from JS/Rust. The honest options are:
1. Keep this as-is but rename to `decode_errors_per_sec`
2. Track `VideoDecoder.decodeQueueSize` over time — if consistently >0, frames are backing up
3. Document that this metric reflects decode errors, not throughput-driven drops

---

## Bug 2 [CRITICAL]: FpsTracker Entries Leak for Departed Peers

**File**: `videocall-client/src/diagnostics/diagnostics_manager.rs`

`FpsTracker` entries are added to `fps_trackers: HashMap<String, HashMap<MediaType, FpsTracker>>` when a `FrameReceived` event arrives:

```rust
let peer_trackers = self.fps_trackers.entry(peer_id.clone()).or_default();
let tracker = peer_trackers.entry(media_type).or_insert_with(|| FpsTracker::new(media_type));
```

They are **never removed**. When a peer leaves:

1. No `FrameReceived` events arrive for that peer
2. The `FpsTracker` entry remains, with stale last-computed fps/bitrate values
3. Every 500ms, `send_diagnostic_packets()` iterates all trackers and broadcasts a `DiagEvent` for every peer entry — including the departed peer with stale metrics
4. `health_reporter.rs` receives these events and updates `last_video_update_ms`, keeping the data "fresh" from the freshness gate's perspective
5. Result: quality scores are computed for peers that have left the call

`MeetingState.tick()` in vcprobe prunes departed participants after 10s, but the client-side `DiagnosticManager` has no equivalent cleanup.

**Partial mitigation in health_reporter.rs**: `remove_peer()` exists and is presumably called when a peer is removed, which removes the `PeerHealthData` entry. But the `DiagnosticManager` keeps the FpsTracker indefinitely and keeps broadcasting, so a future peer with the same session_id would accumulate stale data from the previous peer's tracker.

**Fix required**: Add a `remove_peer(peer_id: &str)` method to `DiagnosticManager` that deletes the peer's entry from `fps_trackers`. Call it when a peer is removed from `PeerDecodeManager`.

---

## Bug 3 [MEDIUM]: `can_listen`/`can_see` Never Reset on Stream Stop

**File**: `videocall-client/src/health_reporter.rs:79-87`

```rust
pub fn mark_audio_timeout(&mut self) {
    self.can_listen = false;
}

pub fn mark_video_timeout(&mut self) {
    self.can_see = false;
}
```

These methods are defined but never called anywhere in the codebase (`grep -r "mark_audio_timeout\|mark_video_timeout"` returns zero call sites).

`can_listen` is set to `true` whenever audio stats arrive. `can_see` is set to `true` whenever video stats arrive. Neither resets to `false` when the stream stops. The health packet therefore reports `can_listen = true` and `can_see = true` indefinitely after streams have stopped, until the `PeerHealthData` entry is removed entirely.

**Effect on quality scores**: Mitigated by the 5-second freshness gate introduced for quality scores. After 5s without fresh data, quality scores go `None`. But `ps.can_listen` and `ps.can_see` in the transmitted protobuf will still be `true`, and Grafana metrics based on these flags will show false "connected" state.

**Fix**: Either call `mark_audio_timeout()` / `mark_video_timeout()` when the FpsTracker's inactive check fires, or drive them from `peer_status` events (which do exist and do update these flags correctly).

---

## Bug 4 [MEDIUM]: Prometheus Session Cleanup Doesn't Track Per-Peer Labels

**File**: `actix-api/src/bin/metrics_server.rs:213-229`

When a new NATS health packet arrives, the session tracker is updated:

```rust
tracker.insert(
    session_key,
    SessionInfo {
        ...
        to_peers: HashSet::new(),   // ← always empty on insert
        peer_ids: HashSet::new(),
        active_servers: HashSet::new(),
    },
);
```

Each new packet **replaces** the existing `SessionInfo` with a fresh one with empty `to_peers`. The subsequent Prometheus metric updates write label values for each `peer_id` in `health_packet.peer_stats`, but the session's `to_peers` set is never updated (the code that would do this was not read, but the insert always creates empty sets).

**Effect**: When a session is cleaned up after 30s of inactivity, `remove_session_metrics()` iterates `session_info.to_peers` to remove per-peer Prometheus metrics. If `to_peers` is always empty, per-peer metrics (`VIDEO_FPS`, `NETEQ_EXPAND_OPS_PER_SEC`, `AUDIO_PACKET_LOSS_PCT`, etc.) are **never cleaned up** from Prometheus. Over time, Prometheus accumulates stale label combinations for every peer that ever participated in a call, consuming unbounded memory.

**Verify**: Check whether `to_peers` is ever populated after the insert. If not, this is a memory leak in the Prometheus registry.

---

## Bug 5 [MEDIUM]: Double DiagEvent Emission Per Peer Per Heartbeat

**File**: `videocall-client/src/diagnostics/diagnostics_manager.rs:460-494`

For every heartbeat (every 500ms), for every tracked peer and media type, `send_diagnostic_packets()` emits **two** DiagEvents:

```rust
// Event 1: subsystem "decoder"
metric!("fps", tracker.fps),
metric!("bitrate_kbps", tracker.current_bitrate),
metric!("frames_dropped_per_sec", tracker.frames_dropped_per_sec),

// Event 2: subsystem "video"
metric!("fps_received", tracker.fps),
metric!("bitrate_kbps", tracker.current_bitrate),
metric!("frames_dropped_per_sec", tracker.frames_dropped_per_sec),
```

Both carry essentially the same data. `health_reporter.rs` handles both separately:
- `"decoder"` subsystem → updates `can_see`/`can_listen` from fps
- `"video_decoder" || "video"` subsystem → updates `last_video_stats` with fps_received, bitrate, drops

This doubles the broadcast channel messages (already at capacity 100 for a 100-message channel) and the health reporter processes each packet twice per cycle. With 4 peers at 500ms intervals, that is 16 channel messages per second just for video metrics.

This isn't causing failures, but it is wasteful and the two code paths in health_reporter.rs for the same data source are a maintenance hazard.

---

## Observation [MEDIUM]: `packets_received` Counts RTT Probes; `packets_sent` Does Not

**File**: `videocall-client/src/connection/connection_manager.rs`

`packets_received` is incremented in `create_inbound_media_callback()` for **every** incoming packet including RTT responses that are handled internally and never reach the application:

```rust
packets_received.fetch_add(1, Ordering::Relaxed);
// ... RTT response check follows, returns early if RTT packet
```

`packets_sent` is incremented in `send_packet()` — the public API. But `send_rtt_probe()` calls `connection.send_packet(rtt_packet)` directly, bypassing the counter:

```rust
fn send_rtt_probe(&mut self, connection_id: &str) -> Result<()> {
    ...
    connection.send_packet(rtt_packet);  // ← NOT counted in packets_sent
```

During the election phase, RTT probes are sent at 200ms intervals to all candidate servers simultaneously. So `packets_received` includes internal RTT echoes while `packets_sent` excludes internal RTT probes. The asymmetry means the two rates are not comparable and their ratio doesn't reflect anything meaningful about the application's communication load.

**Recommendation**: Either count RTT probes in `packets_sent`, or filter RTT packets from `packets_received`, or document the asymmetry clearly and rename to `application_packets_received` vs. `rtt_inclusive_packets_received`.

---

## Observation [MEDIUM]: Quality Score Multiple-Observer Last-Write-Wins

**File**: `vcprobe/src/state.rs:282-296`

When multiple participants are in a meeting, each reports quality scores for each peer they observe. vcprobe stores the latest received `QualitySnapshot` for each participant:

```rust
p.quality = Some(QualitySnapshot {
    ...
    audio_quality_score: peer_stats.audio_quality_score,
    ...
});
```

If Jay1 observes Jay2 as 95/100 and Jay3 observes Jay2 as 45/100 (Jay3 is far from the server and sees Jay2 poorly), the vcprobe display depends entirely on which HEALTH packet arrived last. Scores could flicker between observers.

This is acceptable for a v1, but worth noting: the displayed quality score for a participant reflects one observer's perspective at a particular moment, not an aggregate. A "worst observer" model or an average would be more stable and more useful for alerting (the worst experience is what matters, not the average).

---

## Observation [LOW]: `frames_decoded` AtomicU32 Uses `SeqCst` Ordering Unnecessarily

**File**: `videocall-client/src/diagnostics/diagnostics_manager.rs:337`

```rust
self.frames_decoded.fetch_add(1, Ordering::SeqCst);
```

`SeqCst` is the most expensive atomic ordering — it guarantees a total global order of all atomic operations across all threads. In WASM, there is only one thread. Using `Relaxed` ordering here would be equivalent in behavior and avoids any potential memory fence overhead. Compare with the packet counters which correctly use `Relaxed`:

```rust
packets_received.fetch_add(1, Ordering::Relaxed);
```

Additionally, `get_frames_decoded()` is exposed as a public method but does not appear to be called by any of the diagnostic systems described in this review. If it's dead code, it should be removed.

---

## Observation [LOW]: `calculate_packet_rates()` Has Implicit Coupling

**File**: `videocall-client/src/health_reporter.rs:489-507`

The health reporter calls `cc.calculate_packet_rates()` immediately before reading the rates. This is a side-effectful getter masquerading as a calculation. The `calculate_packet_rates()` method updates internal state and has a 0.1-second guard:

```rust
if elapsed_sec < 0.1 {
    return;
}
```

If any other code calls `calculate_packet_rates()` more frequently (e.g., if the 1Hz ConnectionController timer is later extended to call it), the window between rate calculations shrinks and the rates become noisy. The API would be cleaner as a scheduled internal calculation with a separate read-only accessor.

---

## Architecture Validation: What Actually Works in WASM

The following metrics ARE being correctly collected in the WASM/WebTransport/WebSocket context:

| Metric | Collection Point | Reliability |
|---|---|---|
| RTT to server | RTT echo packet timestamps | Good — custom protocol |
| NetEQ jitter (`target_delay_ms`) | NetEQ WASM stats JSON | Good — from native Rust |
| Audio concealment rate | NetEQ `expand_per_sec` (windowed) | Good — fixed from lifetime |
| Audio buffer depth | NetEQ `current_buffer_size_ms` | Good — snapshot |
| Audio packets/sec | NetEQ `packets_per_sec` (windowed) | Good |
| Audio packet loss % | `expand_per_sec / packets_per_sec` | Proxy, not true loss % (see below) |
| Video FPS | `FpsTracker` frame counter (windowed) | Good |
| Video bitrate | `FpsTracker` byte counter (windowed) | Good |
| Video frames dropped | Decode error counter | Misnamed — see Bug 1 |
| Tab visibility | `document.hidden` | Good — universal |
| Memory usage | `performance.memory` | Good — Chrome only, graceful |
| Send queue depth | `WebSocket.bufferedAmount` | Good — WebSocket only |
| Packets received/sec | Atomic counter at connection layer | Good — includes RTT (see Bug observation) |
| Packets sent/sec | Atomic counter at connection layer | Good — excludes RTT probes |
| Quality scores (audio/video/call) | Client-computed from above | Good — correctly absent when inactive |

### Note on Audio Packet Loss %

The current formula `expand_per_sec / packets_per_sec` is a **proxy**, not true packet loss. NetEQ runs expand operations (fills silence) for multiple reasons beyond packet loss:
- Actual packet loss (correct case)
- Jitter buffer underrun (packet arrived too late, counted as lost by NetEQ)
- DTX silence (Opus pauses, then resumes — a brief underrun on resume)

At low packet rates (speaker just resumed talking), the first few packets cause high `expand/packets` ratios even with perfect network. The DTX gate (`packets_per_sec >= 2.0`) partially addresses this but a speaker who just unmuted will show transient false-positive "packet loss" for ~1 second.

True packet loss would require RTP sequence number tracking at the transport layer, which is not available in WebTransport/WebSocket. The proxy is the best available signal in this architecture.

---

## Positive Findings

1. **Windowed rate architecture is correct**: Using `elapsed_ms`-based resets inside `FpsTracker.track_frame_with_size()` is the right pattern. Metrics respond to current conditions within 1 second.

2. **Quality score placement is correct**: Computing scores client-side in the browser and transmitting via health packets avoids dual computation in vcprobe and Grafana. The `optional` proto3 fields correctly produce Grafana time-series gaps rather than misleading zeros.

3. **Freshness gate in health_reporter.rs is well-implemented**: The `last_audio_update_ms`/`last_video_update_ms` approach correctly solves the stale score problem within the 5-second window without any polling or timers.

4. **WebSocket `bufferedAmount` plumbing is clean**: `WebSocketTask.get_buffered_amount()` → `Task.get_send_queue_depth()` → `Connection.get_send_queue_depth()` → `ConnectionManager.get_send_queue_depth()` → `ConnectionController.get_send_queue_depth()` is straightforward delegation with correct `Option<u64>` propagation (WebTransport returns `None` honestly).

5. **DTX silence detection is correct**: Gating concealment display on `audio_packets_per_sec < 2.0` is the right threshold. Opus with DTX disabled sends ~50 packets/sec; Opus with DTX enabled during silence drops to 0. The 2.0 threshold cleanly separates the cases.

6. **Connection election and RTT-based best-server selection**: The ConnectionManager/ConnectionController split, with timer ownership in the controller, is correct for WASM's `Rc<RefCell<>>` memory model.

---

## Recommendations Priority

| Priority | Issue | Effort |
|---|---|---|
| ~~P0~~ ✅ | ~~Rename `frames_dropped_per_sec` → `decode_errors_per_sec`, update docs~~ **Fixed 2026-03-09** | — |
| ~~P0~~ ✅ | ~~Add `remove_peer()` to `DiagnosticManager`, call it from `PeerDecodeManager`~~ **Fixed 2026-03-09** | — |
| ~~P1~~ ✅ | ~~Verify and fix Prometheus `to_peers` tracking (cleanup leak)~~ **Fixed 2026-03-09**: replaced blind `tracker.insert()` (reset sets every packet) with `entry().or_insert_with()` + `last_seen` update (sets now accumulate). Also fixed pre-existing crash: `peer_data.frames_dropped` (non-existent field) → `peer_data.decode_errors_per_sec`. | — |
| ~~P1~~ ✅ | ~~Call `mark_audio_timeout()`/`mark_video_timeout()` when FpsTracker inactive~~ **Fixed differently 2026-03-09**: removed `can_listen`/`can_see` from `PeerHealthData`; now computed dynamically from freshness timestamps at packet-build time. Also fixed `audio_enabled`/`video_enabled` to use actual heartbeat self-report. | — |
| ~~P3~~ ✅ | ~~Switch `frames_decoded.fetch_add` to `Ordering::Relaxed`~~ **Fixed 2026-03-09**: also removed dead `get_frames_decoded()` accessor. | — |
| ~~Regression~~ ✅ | ~~`video_enabled` gate in quality score could suppress scores before first peer_status event~~ **Fixed 2026-03-09**: removed `video_enabled` from video quality gate; `fps > 0.0` already proves video is flowing. | — |
| ~~Scoring~~ ✅ | ~~Audio quality score included a constant jitter penalty (target_delay_ms stuck at 120ms = always -10.6 pts)~~ **Fixed 2026-03-09**: removed jitter penalty, redistributed to 70% concealment / 30% packet loss. | — |
| ~~Scoring~~ ✅ | ~~Sort key used old local quality_score() formula, inconsistent with bar which showed call_quality_score~~ **Fixed 2026-03-09**: sort now uses call_quality_score (fallback to old formula only when HEALTH packets absent). | — |
| ~~Display~~ ✅ | ~~Audio/Video/Call scores only visible in detail panel~~ **Fixed 2026-03-09**: added Aud/Vid/Call numeric columns to the main participant table; bar retained for Call quality. Jitter column removed (no diagnostic value). | — |
| ~~P2~~ ✅ | ~~Remove duplicate "decoder" + "video" DiagEvent emission, consolidate to one~~ **Fixed 2026-03-09**: removed the "decoder" DiagEvent entirely; the "video" event (which the health reporter actually uses) is now the sole emission per peer per heartbeat. Added `media_type` to the "video" event to preserve that field. Removed the dead "decoder" handler from health_reporter.rs. | — |
| ~~P2~~ ✅ | ~~Document `packets_received` vs. `packets_sent` asymmetry~~ **Fixed 2026-03-09**: added `packets_sent.fetch_add(1)` in `send_rtt_probe()`. RTT probes are now counted in `packets_sent`, symmetric with `packets_received` which already counted inbound RTT echoes. Rates are now comparable. | — |
| P3 | Consider worst-observer or averaged quality scores in vcprobe | Future |

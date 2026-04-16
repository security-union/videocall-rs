# Performance Optimization Plan

## Overview

This document outlines the findings from a comprehensive investigation of the videocall-rs codebase, focused on improving performance under adverse network conditions (high latency, packet loss, low bandwidth) while maintaining high quality when conditions are good.

The investigation covered five areas: adaptive bitrate control, network transport, codec/media pipeline, server-side media handling, and the decode/rendering pipeline.

---

## Current Architecture Summary

- **Transport:** WebTransport (preferred, QUIC-based) with WebSocket fallback
- **Server:** Distributed SFU using NATS pub/sub for media routing; server acts as transparent forwarder
- **Video Codec:** VP9 (VP8 fallback on Firefox) via WebCodecs API
- **Audio Codec:** Opus, 48 kHz, mono, 20ms frames
- **Adaptive Bitrate:** PID controller (`pidgeon 0.3.1`) adjusts video bitrate based on received FPS feedback
- **Jitter Buffer:** RFC 3550 jitter estimation, adaptive playout delay (10–500ms), keyframe-gated decoding
- **Encryption:** AES-128 per-packet encryption with RSA key exchange

---

## Key Findings

### A. Critical Gaps (High Impact)

| # | Finding | Current Behavior | Impact |
|---|---------|-------------------|--------|
| A1 | **No keyframe request (PLI/NACK) mechanism** | Decoder waits passively for next periodic keyframe (every 150 frames = 5s at 30fps) | Up to 5 seconds of frozen video on any packet loss |
| A2 | **No audio bitrate adaptation** | Microphone encoder receives diagnostics but ignores them (marked TODO). Audio fixed at 50 kbps | Missed bandwidth savings; audio could survive at 16–24 kbps under congestion |
| A3 | **No automatic reconnection** | Connection loss emits callback; no library-level reconnect. User must manually rejoin | Call drops on transient network blips |
| A4 | **No framerate adaptation** | PID controller only adjusts bitrate, never FPS | Under severe congestion, reducing FPS (30→15→10) would dramatically cut traffic |
| A5 | **No resolution downscaling** | Encoder always uses full camera resolution | Large frames sent even when bandwidth can't support them |
| A6 | **Screen share sends only 1 keyframe** | First frame only; all subsequent are delta frames | Any packet loss on screen share = permanent corruption until stream restart |

### B. Traffic & Efficiency Issues (Medium Impact)

| # | Finding | Current Behavior | Impact |
|---|---------|-------------------|--------|
| B1 | **1 Hz heartbeats even when idle** | Every client sends heartbeat every second regardless of state changes | In a 10-person call: 10 heartbeats/sec of overhead |
| B2 | **Health packets + heartbeats overlap** | Both report audio/video enabled state | Redundant payload data |
| B3 | **No DTX (Discontinuous Transmission)** | Full Opus audio frames sent during silence | Wasted bandwidth when nobody is speaking |
| B4 | **VAD polls every 50ms when muted** | 50ms timer runs even when microphone is muted | Unnecessary CPU/timer overhead |
| B5 | **All peers decoded regardless of visibility** | Off-screen or minimized peer tiles still fully decode | Wasted CPU in large calls |
| B6 | **WebTransport datagram/stream routing** | Media (VIDEO, AUDIO, SCREEN) uses reliable streams to avoid artifacts; control packets (heartbeats, RTT, diagnostics) use unreliable datagrams for lower overhead | Correct trade-off: media integrity preserved, expendable control traffic has minimal overhead |

### C. Resilience & Quality Gaps

| # | Finding | Current Behavior | Impact |
|---|---------|-------------------|--------|
| C1 | **No FEC (Forward Error Correction)** | No redundant packets sent | Single packet loss = frame loss |
| C2 | **No audio/video sync (lip-sync)** | Audio and video decoded independently | Noticeable desync under jitter |
| C3 | **No SVC (Scalable Video Coding)** | Single video bitstream only | Server cannot selectively forward quality layers per receiver |
| C4 | **Server drops packets silently** | WebTransport outbound channel drops when full (256 items), no feedback | Silent quality degradation for constrained receivers |
| C5 | **No connection quality re-election** | Once a server is elected, client stays even if RTT degrades | Stuck on degraded network path mid-call |

---

## Design Principles

### Fully Automatic Adaptation

All quality adjustments — both degradation under poor conditions and recovery under good conditions — **must happen automatically** with zero user intervention. The system continuously monitors network health signals (RTT, received FPS, jitter, packet loss) and transitions between quality tiers seamlessly. Users should never need to manually lower their video quality or restart a call to recover from a bad network episode.

**Step-down behavior:** When conditions degrade, the system reduces quality in a prioritized order — video resolution first, then framerate, then video bitrate, then audio bitrate last. Audio is always the last to degrade because intelligible audio is more critical than high-resolution video for communication.

**Step-up behavior:** When conditions improve, the system restores quality in reverse order — audio bitrate first, then video bitrate, then framerate, then resolution. Step-up uses a **stabilization window** (configurable) to avoid oscillation: conditions must remain good for a sustained period before upgrading. This prevents rapid toggling between tiers on an unstable connection.

**Hysteresis:** Degradation thresholds are lower than recovery thresholds to prevent oscillation. For example, video might step down to 480p when bandwidth drops below 600 kbps, but only step back up to 720p when bandwidth exceeds 900 kbps.

### Centralized Tuning Constants

All thresholds, tiers, timing values, and adaptation parameters **must be defined in a single constants file** (`videocall-client/src/adaptive_quality_constants.rs`) so that the entire adaptation behavior can be tuned by editing one file. No magic numbers scattered across encoders, the PID controller, or connection logic.

This constants file is the **single source of truth** for:
- Network condition classification thresholds (what RTT/loss/jitter values define "good", "fair", "poor", "critical")
- Video quality tiers (resolution, framerate, bitrate per tier)
- Audio quality tiers (bitrate per tier)
- Step-up / step-down hysteresis margins
- Stabilization window durations
- PID controller tuning parameters
- Keyframe intervals per tier
- Reconnection timing

---

## Adaptive Quality Constants Specification

The following constants will be defined in `videocall-client/src/adaptive_quality_constants.rs`. All adaptation logic across the codebase references this file.

### Network Condition Classification

```rust
/// RTT thresholds (milliseconds) for classifying network quality.
/// Measured as rolling average over RTT_AVERAGING_WINDOW_SAMPLES.
pub const RTT_GOOD_MS: f64 = 100.0;
pub const RTT_FAIR_MS: f64 = 200.0;
pub const RTT_POOR_MS: f64 = 400.0;
/// Above RTT_POOR_MS is classified as "critical"

/// Received FPS ratio thresholds (received_fps / target_fps).
/// 1.0 = perfect, 0.0 = nothing getting through.
pub const FPS_RATIO_GOOD: f64 = 0.90;
pub const FPS_RATIO_FAIR: f64 = 0.70;
pub const FPS_RATIO_POOR: f64 = 0.40;
/// Below FPS_RATIO_POOR is classified as "critical"

/// Jitter thresholds (milliseconds).
pub const JITTER_GOOD_MS: f64 = 20.0;
pub const JITTER_FAIR_MS: f64 = 50.0;
pub const JITTER_POOR_MS: f64 = 100.0;

/// Number of RTT samples to average for condition classification.
pub const RTT_AVERAGING_WINDOW_SAMPLES: usize = 10;
```

### Video Quality Tiers

```rust
/// Video quality tiers, ordered from highest to lowest.
/// The system automatically selects the appropriate tier based on network conditions.
/// Each tier defines resolution, framerate, and bitrate bounds.
///
/// Step-down: when conditions worsen, move to a lower tier.
/// Step-up: when conditions improve and stabilize, move to a higher tier.

pub struct VideoQualityTier {
    pub label: &'static str,
    pub max_width: u32,
    pub max_height: u32,
    pub target_fps: u32,
    pub ideal_bitrate_kbps: u32,
    pub min_bitrate_kbps: u32,
    pub max_bitrate_kbps: u32,
    pub keyframe_interval_frames: u32,
}

pub const VIDEO_QUALITY_TIERS: &[VideoQualityTier] = &[
    VideoQualityTier {
        label: "high",
        max_width: 1280,
        max_height: 720,
        target_fps: 30,
        ideal_bitrate_kbps: 1500,
        min_bitrate_kbps: 800,
        max_bitrate_kbps: 2500,
        keyframe_interval_frames: 150,  // ~5s at 30fps
    },
    VideoQualityTier {
        label: "medium",
        max_width: 854,
        max_height: 480,
        target_fps: 25,
        ideal_bitrate_kbps: 600,
        min_bitrate_kbps: 300,
        max_bitrate_kbps: 1000,
        keyframe_interval_frames: 125,  // ~5s at 25fps
    },
    VideoQualityTier {
        label: "low",
        max_width: 640,
        max_height: 360,
        target_fps: 15,
        ideal_bitrate_kbps: 300,
        min_bitrate_kbps: 150,
        max_bitrate_kbps: 500,
        keyframe_interval_frames: 75,   // ~5s at 15fps
    },
    VideoQualityTier {
        label: "minimal",
        max_width: 426,
        max_height: 240,
        target_fps: 10,
        ideal_bitrate_kbps: 150,
        min_bitrate_kbps: 50,
        max_bitrate_kbps: 250,
        keyframe_interval_frames: 50,   // ~5s at 10fps
    },
];

/// Index into VIDEO_QUALITY_TIERS for the default starting tier.
pub const DEFAULT_VIDEO_TIER_INDEX: usize = 1; // "medium" — avoids wasted encoding before first adaptation
```

### Screen Share Quality Tiers

```rust
pub const SCREEN_QUALITY_TIERS: &[VideoQualityTier] = &[
    VideoQualityTier {
        label: "high",
        max_width: 1920,
        max_height: 1080,
        target_fps: 15,
        ideal_bitrate_kbps: 1500,
        min_bitrate_kbps: 800,
        max_bitrate_kbps: 2500,
        keyframe_interval_frames: 75,   // ~5s at 15fps
    },
    VideoQualityTier {
        label: "medium",
        max_width: 1280,
        max_height: 720,
        target_fps: 10,
        ideal_bitrate_kbps: 600,
        min_bitrate_kbps: 300,
        max_bitrate_kbps: 1000,
        keyframe_interval_frames: 50,   // ~5s at 10fps
    },
    VideoQualityTier {
        label: "low",
        max_width: 854,
        max_height: 480,
        target_fps: 5,
        ideal_bitrate_kbps: 250,
        min_bitrate_kbps: 100,
        max_bitrate_kbps: 400,
        keyframe_interval_frames: 25,   // ~5s at 5fps
    },
];
```

### Audio Quality Tiers

```rust
/// Audio quality tiers, ordered from highest to lowest.
/// Audio is the LAST to degrade and FIRST to recover.

pub struct AudioQualityTier {
    pub label: &'static str,
    pub bitrate_kbps: u32,
    pub enable_dtx: bool,
    pub enable_fec: bool,
}

pub const AUDIO_QUALITY_TIERS: &[AudioQualityTier] = &[
    AudioQualityTier {
        label: "high",
        bitrate_kbps: 50,
        enable_dtx: true,
        enable_fec: false,
    },
    AudioQualityTier {
        label: "medium",
        bitrate_kbps: 32,
        enable_dtx: true,
        enable_fec: true,  // enable FEC under moderate loss
    },
    AudioQualityTier {
        label: "low",
        bitrate_kbps: 24,
        enable_dtx: true,
        enable_fec: true,
    },
    AudioQualityTier {
        label: "emergency",
        bitrate_kbps: 16,
        enable_dtx: true,
        enable_fec: true,
    },
];
```

### Tier Transition Thresholds

```rust
/// Hysteresis configuration for automatic tier transitions.
/// Step-down uses the "degrade" threshold; step-up uses the "recover" threshold.
/// The gap between them prevents oscillation.

/// FPS ratio (received/target) below which we step DOWN one video tier.
pub const VIDEO_TIER_DEGRADE_FPS_RATIO: f64 = 0.50;
/// FPS ratio above which we step UP one video tier (must be sustained).
pub const VIDEO_TIER_RECOVER_FPS_RATIO: f64 = 0.70;

/// Bitrate ratio (actual/ideal) below which we step DOWN one video tier.
pub const VIDEO_TIER_DEGRADE_BITRATE_RATIO: f64 = 0.40;
/// Bitrate ratio above which we step UP one video tier (must be sustained).
pub const VIDEO_TIER_RECOVER_BITRATE_RATIO: f64 = 0.75;

/// Audio degrades only when video is already at lowest tier AND these thresholds hit.
pub const AUDIO_TIER_DEGRADE_FPS_RATIO: f64 = 0.30;
pub const AUDIO_TIER_RECOVER_FPS_RATIO: f64 = 0.60;

/// How long conditions must remain "good" before stepping UP (milliseconds).
/// Prevents rapid oscillation on unstable connections.
pub const STEP_UP_STABILIZATION_WINDOW_MS: u64 = 5000;

/// How quickly we step DOWN (milliseconds). Degradation is faster than recovery.
pub const STEP_DOWN_REACTION_TIME_MS: u64 = 1500;

/// Minimum time between any two tier transitions (milliseconds).
/// Prevents rapid toggling even if thresholds are crossed quickly.
pub const MIN_TIER_TRANSITION_INTERVAL_MS: u64 = 3000;
```

### PID Controller Tuning

```rust
/// PID controller gains for bitrate adaptation.
pub const PID_KP: f64 = 0.2;   // Proportional gain
pub const PID_KI: f64 = 0.05;  // Integral gain
pub const PID_KD: f64 = 0.02;  // Derivative gain

/// PID deadband — no correction within ±DEADBAND FPS of target.
pub const PID_DEADBAND_FPS: f64 = 0.5;

/// PID output limits (maps to 0–90% bitrate reduction).
pub const PID_OUTPUT_MIN: f64 = 0.0;
pub const PID_OUTPUT_MAX: f64 = 50.0;

/// Maximum jitter-based bitrate penalty (0.0–1.0).
pub const PID_MAX_JITTER_PENALTY: f64 = 0.30;

/// Minimum interval between PID corrections (milliseconds).
pub const PID_CORRECTION_THROTTLE_MS: u64 = 1000;

/// Bitrate change threshold — only apply if change exceeds this ratio.
pub const BITRATE_CHANGE_THRESHOLD: f64 = 0.20;

/// PID FPS history size for jitter calculation.
pub const PID_FPS_HISTORY_SIZE: usize = 10;
```

### Keyframe & Error Recovery

```rust
/// Camera keyframe interval (frames). Also defined per-tier above.
pub const CAMERA_KEYFRAME_INTERVAL_FRAMES: u32 = 150;

/// Screen share keyframe interval (frames).
/// Current code only sends keyframe on first frame — this fixes that.
pub const SCREEN_KEYFRAME_INTERVAL_FRAMES: u32 = 150;

/// Max time to wait for keyframe before requesting one (milliseconds).
/// After a sequence gap, if no keyframe arrives within this window, send PLI.
pub const KEYFRAME_REQUEST_TIMEOUT_MS: u64 = 1000;

/// Minimum interval between keyframe requests to same sender (milliseconds).
/// Prevents flooding the sender with PLI requests.
pub const KEYFRAME_REQUEST_MIN_INTERVAL_MS: u64 = 500;
```

### Reconnection

```rust
/// Initial reconnection delay (milliseconds). Kept low so the first retry fires quickly.
pub const RECONNECT_INITIAL_DELAY_MS: u64 = 500;

/// Maximum reconnection delay (milliseconds). Capped at 2s to keep video calls responsive.
pub const RECONNECT_MAX_DELAY_MS: u64 = 2000;

/// Backoff multiplier per attempt.
pub const RECONNECT_BACKOFF_MULTIPLIER: f64 = 2.0;

/// Consecutive zero-connection attempts before aborting (likely auth/server rejection).
/// Replaces the old fixed RECONNECT_MAX_ATTEMPTS — the client now retries indefinitely
/// unless this safety valve fires or the user intentionally disconnects.
pub const RECONNECT_CONSECUTIVE_ZERO_LIMIT: u32 = 3;

/// RTT degradation multiplier to trigger connection re-election.
/// If current RTT > election_rtt * this multiplier, re-elect.
pub const REELECTION_RTT_MULTIPLIER: f64 = 2.0;

/// Number of consecutive degraded RTT samples before triggering re-election.
pub const REELECTION_CONSECUTIVE_SAMPLES: u32 = 5;
```

### Heartbeat & Polling

```rust
/// Heartbeat interval when using event-driven mode (keepalive only).
/// State changes trigger immediate heartbeats outside this interval.
pub const HEARTBEAT_KEEPALIVE_INTERVAL_MS: u64 = 5000;

/// VAD polling interval (milliseconds). Only active when mic is unmuted.
pub const VAD_POLL_INTERVAL_MS: u64 = 50;

/// Diagnostics reporting interval (milliseconds).
pub const DIAGNOSTICS_REPORT_INTERVAL_MS: u64 = 1000;

/// RTT probe interval during server election (milliseconds).
pub const RTT_PROBE_ELECTION_INTERVAL_MS: u64 = 200;

/// RTT probe interval after server election (milliseconds).
pub const RTT_PROBE_CONNECTED_INTERVAL_MS: u64 = 1000;
```

---

## Implementation Plan

### Phase 1: Adaptive Quality Foundation & Constants File

*Establish the centralized constants and the automatic adaptation state machine.*

#### 1.0 — Create Adaptive Quality Constants File

**Goal:** Single source of truth for all adaptation parameters.

**Solution:**
- Create `videocall-client/src/adaptive_quality_constants.rs` containing all constants defined in the specification above
- Migrate existing hardcoded values from `encoder_bitrate_controller.rs`, `camera_encoder.rs`, `screen_encoder.rs`, `microphone_encoder.rs`, and `connection.rs` to reference this file
- All future adaptation logic references this file exclusively

**Files involved:**
- `videocall-client/src/adaptive_quality_constants.rs` — **new file**, all constants
- `videocall-client/src/lib.rs` — declare the new module
- `videocall-client/src/diagnostics/encoder_bitrate_controller.rs` — replace hardcoded PID values
- `videocall-client/src/encode/camera_encoder.rs` — replace hardcoded keyframe interval, bitrate threshold
- `videocall-client/src/encode/screen_encoder.rs` — replace hardcoded values
- `videocall-client/src/encode/microphone_encoder.rs` — replace hardcoded VAD interval
- `videocall-client/src/connection/connection.rs` — replace hardcoded heartbeat interval

#### 1.0.1 — Implement Adaptive Quality State Machine

**Goal:** Central component that monitors network signals, classifies conditions, and automatically selects the appropriate video/audio tier.

**Solution:**
- Create `videocall-client/src/diagnostics/adaptive_quality_manager.rs`
- Consumes RTT, received FPS, jitter, and bitrate signals from existing diagnostics
- Classifies overall network condition as good/fair/poor/critical using thresholds from constants
- Maintains current video tier index and audio tier index
- Applies hysteresis: step-down is fast (STEP_DOWN_REACTION_TIME_MS), step-up requires sustained good conditions (STEP_UP_STABILIZATION_WINDOW_MS)
- Enforces MIN_TIER_TRANSITION_INTERVAL_MS between transitions
- Outputs: recommended `VideoQualityTier` and `AudioQualityTier` for encoders to apply
- Degradation order: video resolution → video framerate → video bitrate → audio bitrate
- Recovery order: audio bitrate → video bitrate → video framerate → video resolution

**Files involved:**
- `videocall-client/src/diagnostics/adaptive_quality_manager.rs` — **new file**, state machine
- `videocall-client/src/diagnostics/mod.rs` — declare new module
- `videocall-client/src/diagnostics/encoder_bitrate_controller.rs` — feed signals to manager

### Phase 1 (continued): Packet Loss Recovery & Encoder Adaptation

#### 1.1 — Keyframe Request Mechanism (PLI)

**Problem:** When packets are lost, the decoder waits up to 5 seconds for the next periodic keyframe, causing frozen video.

**Automatic behavior:** The decoder automatically detects sequence gaps in the jitter buffer. After waiting `KEYFRAME_REQUEST_TIMEOUT_MS` (1000ms) for a keyframe to arrive naturally, it sends a PLI request. Requests are rate-limited to `KEYFRAME_REQUEST_MIN_INTERVAL_MS` (500ms) per sender. The sender automatically generates a keyframe on receipt. No user action required.

**Solution:**
- Add a new `MediaType::KEYFRAME_REQUEST` protobuf message type
- When the decoder detects a sequence gap in the jitter buffer, wait up to `KEYFRAME_REQUEST_TIMEOUT_MS` then send a PLI request to the sender via the server
- Rate-limit requests per sender using `KEYFRAME_REQUEST_MIN_INTERVAL_MS`
- Sender immediately generates a keyframe on receipt
- Fix screen encoder to send periodic keyframes every `SCREEN_KEYFRAME_INTERVAL_FRAMES` instead of only the first frame

**Files involved:**
- `protobuf/types/media_packet.proto` — new MediaType enum value
- `videocall-codecs/src/jitter_buffer.rs` — trigger request on gap detection
- `videocall-client/src/encode/camera_encoder.rs` — handle incoming keyframe request
- `videocall-client/src/encode/screen_encoder.rs` — add periodic keyframes + handle requests
- `actix-api/src/actors/packet_handler.rs` — route keyframe requests to target sender
- `videocall-client/src/adaptive_quality_constants.rs` — `KEYFRAME_REQUEST_TIMEOUT_MS`, `KEYFRAME_REQUEST_MIN_INTERVAL_MS`, `SCREEN_KEYFRAME_INTERVAL_FRAMES`

#### 1.2 — Automatic Framerate & Resolution Adaptation

**Problem:** PID controller only adjusts bitrate. Under severe congestion, the same number of frames are sent at ever-lower quality, and resolution never changes.

**Automatic behavior:** The `AdaptiveQualityManager` (1.0.1) continuously monitors network signals and automatically selects the appropriate `VideoQualityTier`. Each tier bundles resolution, framerate, and bitrate together. When the FPS ratio drops below `VIDEO_TIER_DEGRADE_FPS_RATIO` (0.50) or bitrate ratio drops below `VIDEO_TIER_DEGRADE_BITRATE_RATIO` (0.40) for `STEP_DOWN_REACTION_TIME_MS` (1500ms), the system steps down one tier. When conditions recover above `VIDEO_TIER_RECOVER_FPS_RATIO` (0.70) / `VIDEO_TIER_RECOVER_BITRATE_RATIO` (0.75) and remain stable for `STEP_UP_STABILIZATION_WINDOW_MS` (5000ms), the system steps back up. Minimum `MIN_TIER_TRANSITION_INTERVAL_MS` (3000ms) between any transitions prevents oscillation.

**Solution:**
- `AdaptiveQualityManager` outputs a `VideoQualityTier` recommendation
- Camera encoder applies tier's `max_width`, `max_height`, `target_fps`, bitrate bounds, and `keyframe_interval_frames`
- Screen encoder applies the corresponding `SCREEN_QUALITY_TIERS` tier
- Resolution change: reconfigure `VideoEncoderConfig` dimensions (already supported in camera_encoder.rs lines 493–515)
- FPS change: reconfigure encoder or drop frames to match tier's `target_fps`

**Files involved:**
- `videocall-client/src/diagnostics/adaptive_quality_manager.rs` — tier selection state machine
- `videocall-client/src/diagnostics/encoder_bitrate_controller.rs` — feed signals, consume tier output
- `videocall-client/src/encode/camera_encoder.rs` — apply tier resolution/fps/bitrate
- `videocall-client/src/encode/screen_encoder.rs` — apply screen tier
- `videocall-client/src/adaptive_quality_constants.rs` — `VIDEO_QUALITY_TIERS`, `SCREEN_QUALITY_TIERS`, all transition thresholds

#### 1.3 — Automatic Audio Bitrate Adaptation

**Problem:** Microphone encoder has a TODO to process diagnostics packets but currently ignores them. Audio is always 50 kbps.

**Automatic behavior:** Audio degrades **only after video is already at the lowest tier** (`minimal`). The `AdaptiveQualityManager` tracks this dependency. When video is at minimum AND FPS ratio drops below `AUDIO_TIER_DEGRADE_FPS_RATIO` (0.30), audio steps down one tier (50→32→24→16 kbps). Audio recovers first when conditions improve: when FPS ratio exceeds `AUDIO_TIER_RECOVER_FPS_RATIO` (0.60) for `STEP_UP_STABILIZATION_WINDOW_MS`, audio steps up before video does.

**Solution:**
- `AdaptiveQualityManager` outputs an `AudioQualityTier` recommendation alongside the video tier
- Microphone encoder applies the tier's `bitrate_kbps`, `enable_dtx`, and `enable_fec` settings
- Implement `process_diagnostics_update()` in microphone_encoder.rs (currently a TODO at line 138)

**Files involved:**
- `videocall-client/src/encode/microphone_encoder.rs` — implement `process_diagnostics_update()`
- `videocall-client/src/diagnostics/adaptive_quality_manager.rs` — audio tier selection with video dependency
- `videocall-client/src/adaptive_quality_constants.rs` — `AUDIO_QUALITY_TIERS`, `AUDIO_TIER_DEGRADE_FPS_RATIO`, `AUDIO_TIER_RECOVER_FPS_RATIO`

---

### Phase 2: Network Resilience

*Keeping calls alive under adverse conditions.*

#### 2.1 — Automatic Reconnection with Exponential Backoff

**Problem:** Connection loss requires manual rejoin. Transient network blips kill the call.

**Automatic behavior:** On connection loss, reconnection begins immediately with no user interaction. The system attempts reconnection at exponentially increasing intervals (`RECONNECT_INITIAL_DELAY_MS` × `RECONNECT_BACKOFF_MULTIPLIER` per attempt), capped at `RECONNECT_MAX_DELAY_MS`. After `RECONNECT_MAX_ATTEMPTS` failures, the system gives up and surfaces a "disconnected" state to the UI. On successful reconnect, the session resumes automatically — peer list, encryption keys, and room membership are preserved.

**Solution:**
- Implement reconnection state machine in connection_manager with states: Connected → Reconnecting → Reconnected / Failed
- Preserve session state (room ID, peer list, encryption keys) during reconnect window
- Re-trigger server election on successful reconnect
- Emit connection state events to UI (reconnecting, reconnected, failed)
- All timing controlled by constants in `adaptive_quality_constants.rs`

**Files involved:**
- `videocall-client/src/connection/connection_manager.rs` — reconnection state machine
- `videocall-client/src/client/video_call_client.rs` — orchestrate reconnection
- `dioxus-ui/` — UI indicators for reconnection state
- `videocall-client/src/adaptive_quality_constants.rs` — `RECONNECT_INITIAL_DELAY_MS`, `RECONNECT_MAX_DELAY_MS`, `RECONNECT_BACKOFF_MULTIPLIER`, `RECONNECT_CONSECUTIVE_ZERO_LIMIT`

#### 2.2 — Automatic Connection Quality Re-election

**Problem:** Once a server/transport is elected, the client stays on it even if RTT degrades severely.

**Automatic behavior:** The system continuously monitors active connection RTT (already measured at 1 Hz). If RTT exceeds `REELECTION_RTT_MULTIPLIER` (2.0×) the election-time baseline for `REELECTION_CONSECUTIVE_SAMPLES` (5) consecutive measurements, a new server election is automatically triggered. The handoff is seamless: new connection established before old one torn down. No user action required.

**Solution:**
- Store baseline RTT at election time
- Compare rolling RTT average against baseline × `REELECTION_RTT_MULTIPLIER`
- After `REELECTION_CONSECUTIVE_SAMPLES` consecutive violations, trigger re-election
- Seamless handoff: establish new connection before tearing down old one

**Files involved:**
- `videocall-client/src/connection/connection_manager.rs` — re-election trigger logic
- `videocall-client/src/connection/connection_controller.rs` — RTT threshold monitoring
- `videocall-client/src/adaptive_quality_constants.rs` — `REELECTION_RTT_MULTIPLIER`, `REELECTION_CONSECUTIVE_SAMPLES`

#### 2.3 — WebTransport Datagram/Stream Routing Policy (DONE)

**Routing policy:**
- **Media (VIDEO, AUDIO, SCREEN) -> reliable unidirectional streams.** Media packets require reliable, ordered delivery. QUIC streams handle retransmission and ordering at the transport layer, eliminating packet-loss artifacts without application-level FEC or RED. This is why items 4.1 (Opus FEC) and 4.5 (RED audio) are unnecessary.
- **Control (HEARTBEAT, RTT, DIAGNOSTICS, HEALTH) -> unreliable datagrams.** Control packets are periodic and expendable — a missed heartbeat or RTT probe is harmless because the next one arrives shortly. Datagrams have lower overhead (no stream setup, no retransmission).
- **Large control packets -> reliable streams (fallback).** Any packet exceeding `DATAGRAM_MAX_SIZE` (1200 bytes) uses reliable streams regardless of type.

**Client-side routing:**
- `send_media_packet()` calls `controller.send_packet()` (reliable stream) for all VIDEO/AUDIO/SCREEN packets
- Heartbeats call `task.send_packet_datagram()` (unreliable datagram) in both periodic and immediate heartbeat paths
- RTT probes call `connection.send_packet_datagram()` (unreliable datagram)

**Server-side routing (`send_auto`):**
- `!is_media && data.len() <= DATAGRAM_MAX_SIZE` -> datagram (control packets)
- Everything else -> reliable unidirectional stream (media, or oversized control)

**Files involved:**
- `actix-api/src/actors/transports/wt_chat_session.rs` — server-side `send_auto()` routing
- `actix-api/src/actors/packet_handler.rs` — `DATAGRAM_MAX_SIZE` constant and test helpers
- `videocall-client/src/client/video_call_client.rs` — `send_media_packet()` uses reliable stream
- `videocall-client/src/connection/connection.rs` — heartbeats use datagrams
- `videocall-client/src/connection/connection_manager.rs` — RTT probes use datagrams

---

### Phase 3: Traffic Optimization

*Reducing unnecessary bandwidth usage without affecting quality.*

#### 3.1 — Opus DTX (Discontinuous Transmission)

**Problem:** Full audio frames are sent even during silence, wasting bandwidth.

**Automatic behavior:** DTX is controlled by the `AudioQualityTier.enable_dtx` flag. When enabled (all tiers by default), the Opus encoder automatically detects silence and sends comfort noise parameters instead of full frames (~1–2 packets/sec instead of 50/sec). This is fully automatic — the encoder handles silence detection internally.

**Solution:**
- Enable DTX in the Opus encoder configuration based on current `AudioQualityTier`
- Reduces audio bandwidth by 80–90% during silence periods automatically

**Files involved:**
- `videocall-client/src/encode/microphone_encoder.rs` — enable DTX in encoder config
- `videocall-client/src/adaptive_quality_constants.rs` — `AudioQualityTier.enable_dtx`

#### 3.2 — Event-Driven Heartbeats

**Problem:** Every client sends a heartbeat every second regardless of whether anything changed.

**Automatic behavior:** Heartbeats switch from fixed 1-second interval to event-driven. State changes (mute/unmute, camera on/off, speaking transitions) trigger an immediate heartbeat. A low-frequency keepalive at `HEARTBEAT_KEEPALIVE_INTERVAL_MS` (5000ms) ensures liveness detection. This is transparent — no user awareness needed.

**Solution:**
- Send heartbeat only on state changes (mute/unmute, camera on/off, speaking transitions)
- Add a low-frequency keepalive every `HEARTBEAT_KEEPALIVE_INTERVAL_MS` for liveness detection
- Reduces heartbeat traffic by ~80%

**Files involved:**
- `videocall-client/src/connection/connection.rs` — heartbeat interval and logic (line 127)
- `actix-api/src/constants.rs` — adjust server-side timeout expectations
- `videocall-client/src/adaptive_quality_constants.rs` — `HEARTBEAT_KEEPALIVE_INTERVAL_MS`

#### 3.3 — Skip VAD When Muted

**Problem:** The 50ms Voice Activity Detection timer runs even when the microphone is muted.

**Automatic behavior:** VAD polling automatically stops when the microphone is muted and resumes immediately on unmute. The polling interval is `VAD_POLL_INTERVAL_MS` from the constants file.

**Solution:**
- Stop the VAD polling interval when microphone is muted
- Resume immediately on unmute
- Saves ~20 timer callbacks/second per muted participant

**Files involved:**
- `videocall-client/src/encode/microphone_encoder.rs` — conditional VAD polling (line 470)
- `videocall-client/src/adaptive_quality_constants.rs` — `VAD_POLL_INTERVAL_MS`

#### 3.4 — Visibility-Based Decode Optimization

**Problem:** All peer tiles are decoded at full resolution regardless of whether they're visible on screen.

**Solution:**
- Use `IntersectionObserver` API to detect off-screen peer tiles
- Pause decoding for non-visible peers (stop feeding frames to decoder)
- Resume immediately when tile scrolls back into view
- Significant CPU savings in calls with many participants

**Files involved:**
- `videocall-client/src/decode/peer_decode_manager.rs` — visibility-gated decode
- `dioxus-ui/src/components/canvas_generator.rs` — IntersectionObserver setup

---

### Phase 4: Advanced Quality (Future)

*For when the fundamentals are solid.*

| # | Item | Description |
|---|------|-------------|
| 4.1 | ~~**Opus in-band FEC**~~ | ~~Enable Forward Error Correction in Opus encoder for moderate packet loss protection without extra bandwidth~~ -- **Unnecessary:** all media uses reliable QUIC streams, so there is no packet loss to correct. FEC would add encoding overhead with zero benefit. |
| 4.2 | **Audio/video lip-sync** | Cross-stream timestamp alignment in playout buffer to correct A/V desynchronization |
| 4.3 | **VP9 SVC temporal layers** | Enable 2-layer temporal scalability so server can selectively drop frames for constrained receivers |
| 4.4 | **Server congestion feedback** | When outbound channel fills, send explicit congestion signal to sender instead of silently dropping |
| 4.5 | ~~**Redundant audio packets**~~ | ~~Send previous audio frame alongside current (RED encoding) for loss recovery without retransmission~~ -- **Unnecessary:** reliable QUIC streams guarantee delivery, making RED pure overhead. RED doubles audio bandwidth (2x per stream) with zero benefit. At 100 participants it adds ~341 Mbps of unnecessary server outbound bandwidth. RED activates during congestion tiers — the worst time to double bandwidth. NetEQ handles gap concealment. `AUDIO_REDUNDANCY_ENABLED` set to `false`; code retained for potential re-enablement on unreliable transport. |

---

## Priority Matrix

| Priority | Items | Rationale |
|----------|-------|-----------|
| **P0 — Foundation** | 1.0 (Constants file), 1.0.1 (Quality state machine) | Must exist before any adaptation logic can be built. All other items depend on this. |
| **P1 — Immediate** | 1.1 (PLI), 1.2 (Framerate + resolution tiers), 1.3 (Audio adaptation), 3.1 (DTX) | Fix the most painful user-visible issues: frozen video, auto quality stepping, wasted silence bandwidth |
| **P2 — Next Sprint** | 2.1 (Reconnection), 2.2 (Re-election), 3.2 (Event heartbeats), 3.3 (VAD mute) | Network resilience + traffic reduction |
| **P3 — Following** | 3.4 (Visibility decode), 2.3 (Datagrams) | Optimization for large calls + protocol improvement |
| **P4 — Future** | 4.1–4.5 (FEC, lip-sync, SVC, server feedback, RED) | Advanced quality features, require more testing |

---

## Key Source Files Reference

| Component | File | Key Lines |
|-----------|------|-----------|
| **Adaptive Quality Constants** | `videocall-client/src/adaptive_quality_constants.rs` | **NEW** — single source of truth for all tuning parameters |
| **Adaptive Quality Manager** | `videocall-client/src/diagnostics/adaptive_quality_manager.rs` | **NEW** — automatic tier selection state machine |
| PID Controller | `videocall-client/src/diagnostics/encoder_bitrate_controller.rs` | 236–245 (config), 284–396 (processing) |
| Camera Encoder | `videocall-client/src/encode/camera_encoder.rs` | 430–438 (init), 472–481 (bitrate update), 518 (keyframe interval) |
| Screen Encoder | `videocall-client/src/encode/screen_encoder.rs` | 450–461 (bitrate update), 522 (single keyframe) |
| Microphone Encoder | `videocall-client/src/encode/microphone_encoder.rs` | 138–141 (TODO bitrate), 410–418 (config), 470 (VAD poll) |
| Connection Manager | `videocall-client/src/connection/connection_manager.rs` | 154–177 (election), 359–393 (loss handling), 546–587 (server selection) |
| Heartbeat | `videocall-client/src/connection/connection.rs` | 127 (1s interval), 191–212 (state change) |
| Jitter Buffer | `videocall-codecs/src/jitter_buffer.rs` | 26–44 (constants), 203–215 (keyframe recovery) |
| Peer Decode Manager | `videocall-client/src/decode/peer_decode_manager.rs` | 530–568 (decode entry), 345–394 (heartbeat processing) |
| Packet Handler (server) | `actix-api/src/actors/packet_handler.rs` | 34–78 (classification) |
| WebTransport (server) | `actix-api/src/actors/transports/wt_chat_session.rs` | 129–149 (channel send/drop) |
| Health Reporter | `videocall-client/src/health_reporter.rs` | 533–625 (metrics collection) |
| Constants (frontend) | `dioxus-ui/src/constants.rs` | 67–72 (bitrate config) |

---

## Timing Constants (Current)

| Component | Interval | Purpose |
|-----------|----------|---------|
| Client Heartbeat | 1000ms | Stream peer state |
| Client VAD Poll | 50ms | Voice activity detection |
| Client RTT Probe (Election) | 200ms | Latency measurement during server selection |
| Client RTT Probe (Connected) | 1000ms | Continuous RTT monitoring |
| Client Peer Monitor | 5000ms | Peer liveness callback |
| Server Heartbeat (WS) | 5000ms | Ping/pong keepalive |
| Server Heartbeat (WT) | 5000ms | Channel health check |
| Server Client Timeout | 10000ms | Disconnect threshold |
| PID Correction Throttle | 1000ms | Min interval between bitrate adjustments |
| Bitrate Change Threshold | 20% | Min change before applying new bitrate |
| Keyframe Interval (camera) | 150 frames (~5s) | Periodic I-frame insertion |
| Keyframe Interval (screen) | First frame only | No periodic keyframes |
| Jitter Buffer Max Delay | 500ms | Upper bound on playout delay |
| Diagnostic Window | 10s | FPS/bitrate averaging period |

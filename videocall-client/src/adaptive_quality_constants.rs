/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Centralized tuning constants for adaptive quality control.
//!
//! This file is the **single source of truth** for all adaptation parameters
//! across the videocall-client crate. All network condition classification,
//! quality tier definitions, PID controller tuning, keyframe intervals,
//! reconnection timing, and polling intervals are defined here.
//!
//! To tune the system's behavior, edit constants in this file only.
//! No magic numbers should exist in encoder, decoder, or connection code.

// ---------------------------------------------------------------------------
// Network Condition Classification
// ---------------------------------------------------------------------------

/// RTT thresholds (milliseconds) for classifying network quality.
/// Measured as rolling average over `RTT_AVERAGING_WINDOW_SAMPLES`.
pub const RTT_GOOD_MS: f64 = 100.0;
pub const RTT_FAIR_MS: f64 = 200.0;
pub const RTT_POOR_MS: f64 = 400.0;
// Above RTT_POOR_MS is classified as "critical".

/// Received FPS ratio thresholds (received_fps / target_fps).
/// 1.0 = perfect, 0.0 = nothing getting through.
pub const FPS_RATIO_GOOD: f64 = 0.90;
pub const FPS_RATIO_FAIR: f64 = 0.70;
pub const FPS_RATIO_POOR: f64 = 0.40;
// Below FPS_RATIO_POOR is classified as "critical".

/// Jitter thresholds (milliseconds).
pub const JITTER_GOOD_MS: f64 = 20.0;
pub const JITTER_FAIR_MS: f64 = 50.0;
pub const JITTER_POOR_MS: f64 = 100.0;

/// Number of RTT samples to average for condition classification.
pub const RTT_AVERAGING_WINDOW_SAMPLES: usize = 10;

// ---------------------------------------------------------------------------
// Video Quality Tiers
// ---------------------------------------------------------------------------

/// A video quality tier bundles resolution, framerate, and bitrate bounds.
///
/// The system automatically selects the appropriate tier based on network
/// conditions. Step-down moves to a lower tier when conditions worsen;
/// step-up moves to a higher tier when conditions improve and stabilize.
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

/// Video quality tiers, ordered from highest (index 0) to lowest.
pub const VIDEO_QUALITY_TIERS: &[VideoQualityTier] = &[
    VideoQualityTier {
        label: "high",
        max_width: 1280,
        max_height: 720,
        target_fps: 30,
        ideal_bitrate_kbps: 1500,
        min_bitrate_kbps: 800,
        max_bitrate_kbps: 2500,
        keyframe_interval_frames: 150, // ~5s at 30fps
    },
    VideoQualityTier {
        label: "medium",
        max_width: 854,
        max_height: 480,
        target_fps: 25,
        ideal_bitrate_kbps: 600,
        min_bitrate_kbps: 300,
        max_bitrate_kbps: 1000,
        keyframe_interval_frames: 125, // ~5s at 25fps
    },
    VideoQualityTier {
        label: "low",
        max_width: 640,
        max_height: 360,
        target_fps: 15,
        ideal_bitrate_kbps: 300,
        min_bitrate_kbps: 150,
        max_bitrate_kbps: 500,
        keyframe_interval_frames: 75, // ~5s at 15fps
    },
    VideoQualityTier {
        label: "minimal",
        max_width: 426,
        max_height: 240,
        target_fps: 10,
        ideal_bitrate_kbps: 150,
        min_bitrate_kbps: 50,
        max_bitrate_kbps: 250,
        keyframe_interval_frames: 50, // ~5s at 10fps
    },
];

/// Index into `VIDEO_QUALITY_TIERS` for the default starting tier.
pub const DEFAULT_VIDEO_TIER_INDEX: usize = 0; // "high"

// ---------------------------------------------------------------------------
// Screen Share Quality Tiers
// ---------------------------------------------------------------------------

/// Screen share quality tiers, ordered from highest (index 0) to lowest.
pub const SCREEN_QUALITY_TIERS: &[VideoQualityTier] = &[
    VideoQualityTier {
        label: "high",
        max_width: 1920,
        max_height: 1080,
        target_fps: 15,
        ideal_bitrate_kbps: 1500,
        min_bitrate_kbps: 800,
        max_bitrate_kbps: 2500,
        keyframe_interval_frames: 75, // ~5s at 15fps
    },
    VideoQualityTier {
        label: "medium",
        max_width: 1280,
        max_height: 720,
        target_fps: 10,
        ideal_bitrate_kbps: 600,
        min_bitrate_kbps: 300,
        max_bitrate_kbps: 1000,
        keyframe_interval_frames: 50, // ~5s at 10fps
    },
    VideoQualityTier {
        label: "low",
        max_width: 854,
        max_height: 480,
        target_fps: 5,
        ideal_bitrate_kbps: 250,
        min_bitrate_kbps: 100,
        max_bitrate_kbps: 400,
        keyframe_interval_frames: 25, // ~5s at 5fps
    },
];

// ---------------------------------------------------------------------------
// Audio Quality Tiers
// ---------------------------------------------------------------------------

/// An audio quality tier defines bitrate and resilience settings.
///
/// Audio is the LAST to degrade and FIRST to recover, because intelligible
/// audio is more critical than high-resolution video for communication.
pub struct AudioQualityTier {
    pub label: &'static str,
    pub bitrate_kbps: u32,
    pub enable_dtx: bool,
    pub enable_fec: bool,
}

/// Audio quality tiers, ordered from highest (index 0) to lowest.
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
        enable_fec: true, // enable FEC under moderate loss
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

// ---------------------------------------------------------------------------
// Tier Transition Thresholds
// ---------------------------------------------------------------------------

/// Hysteresis configuration for automatic tier transitions.
/// Step-down uses the "degrade" threshold; step-up uses the "recover" threshold.
/// The gap between them prevents oscillation.
///
/// FPS ratio (received/target) below which we step DOWN one video tier.
pub const VIDEO_TIER_DEGRADE_FPS_RATIO: f64 = 0.50;
/// FPS ratio above which we step UP one video tier (must be sustained).
pub const VIDEO_TIER_RECOVER_FPS_RATIO: f64 = 0.85;

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

// ---------------------------------------------------------------------------
// PID Controller Tuning
// ---------------------------------------------------------------------------

/// PID controller gains for bitrate adaptation.
pub const PID_KP: f64 = 0.2; // Proportional gain
pub const PID_KI: f64 = 0.05; // Integral gain
pub const PID_KD: f64 = 0.02; // Derivative gain

/// PID deadband -- no correction within +/-DEADBAND FPS of target.
pub const PID_DEADBAND_FPS: f64 = 0.5;

/// PID output limits (maps to 0-90% bitrate reduction).
pub const PID_OUTPUT_MIN: f64 = 0.0;
pub const PID_OUTPUT_MAX: f64 = 50.0;

/// Maximum jitter-based bitrate penalty (0.0-1.0).
pub const PID_MAX_JITTER_PENALTY: f64 = 0.30;

/// Minimum interval between PID corrections (milliseconds).
pub const PID_CORRECTION_THROTTLE_MS: f64 = 1000.0;

/// PID FPS history size for jitter calculation.
pub const PID_FPS_HISTORY_SIZE: usize = 10;

// ---------------------------------------------------------------------------
// Bitrate Change Threshold
// ---------------------------------------------------------------------------

/// Only apply a bitrate change if it exceeds this ratio of the current bitrate.
/// Prevents tiny fluctuations from causing unnecessary encoder reconfigurations.
pub const BITRATE_CHANGE_THRESHOLD: f64 = 0.20;

// ---------------------------------------------------------------------------
// Keyframe & Error Recovery
// ---------------------------------------------------------------------------

/// Camera keyframe interval (frames). Also defined per-tier in `VIDEO_QUALITY_TIERS`.
pub const CAMERA_KEYFRAME_INTERVAL_FRAMES: u32 = 150;

/// Screen share keyframe interval (frames).
/// Periodic keyframes ensure recovery from packet loss on screen share streams.
pub const SCREEN_KEYFRAME_INTERVAL_FRAMES: u32 = 150;

/// Max time to wait for a keyframe before requesting one (milliseconds).
/// After a sequence gap, if no keyframe arrives within this window, send PLI.
pub const KEYFRAME_REQUEST_TIMEOUT_MS: u64 = 1000;

/// Minimum interval between keyframe requests to the same sender (milliseconds).
/// Prevents flooding the sender with PLI requests.
pub const KEYFRAME_REQUEST_MIN_INTERVAL_MS: u64 = 500;

// ---------------------------------------------------------------------------
// Reconnection
// ---------------------------------------------------------------------------

/// Initial reconnection delay (milliseconds).
pub const RECONNECT_INITIAL_DELAY_MS: u64 = 1000;

/// Maximum reconnection delay (milliseconds). Exponential backoff caps here.
pub const RECONNECT_MAX_DELAY_MS: u64 = 16000;

/// Backoff multiplier per attempt.
pub const RECONNECT_BACKOFF_MULTIPLIER: f64 = 2.0;

/// Maximum number of reconnection attempts before giving up.
pub const RECONNECT_MAX_ATTEMPTS: u32 = 10;

/// Stop reconnection if this many consecutive attempts yield zero successful
/// connections (no server responds at all). This catches auth failures and
/// server rejections early, avoiding futile retries that waste resources and
/// may trigger server-side rate limiting.
pub const RECONNECT_CONSECUTIVE_ZERO_LIMIT: u32 = 3;

/// RTT degradation multiplier to trigger connection re-election.
/// If current RTT > election_rtt * this multiplier, re-elect.
pub const REELECTION_RTT_MULTIPLIER: f64 = 2.0;

/// Number of consecutive degraded RTT samples before triggering re-election.
pub const REELECTION_CONSECUTIVE_SAMPLES: u32 = 5;

// ---------------------------------------------------------------------------
// Heartbeat & Polling
// ---------------------------------------------------------------------------

/// Heartbeat keepalive interval (milliseconds).
///
/// In event-driven mode, state changes (mute/unmute, camera on/off, speaking
/// transitions) trigger an immediate heartbeat. This keepalive interval is
/// only for liveness detection -- ensuring the server knows the client is
/// still connected even when nothing changes. The server's CLIENT_TIMEOUT
/// is 10 seconds, so 5-second keepalives provide at least 2 heartbeats per
/// timeout window.
pub const HEARTBEAT_KEEPALIVE_INTERVAL_MS: u32 = 5000;

/// VAD polling interval (milliseconds). Only active when mic is unmuted.
/// The VAD callback checks the muted/enabled flag and returns early if the
/// microphone is disabled, avoiding unnecessary audio analysis work.
pub const VAD_POLL_INTERVAL_MS: u32 = 50;

/// Diagnostics reporting interval (milliseconds).
pub const DIAGNOSTICS_REPORT_INTERVAL_MS: u64 = 1000;

/// RTT probe interval during server election (milliseconds).
pub const RTT_PROBE_ELECTION_INTERVAL_MS: u64 = 200;

/// RTT probe interval after server election (milliseconds).
pub const RTT_PROBE_CONNECTED_INTERVAL_MS: u64 = 1000;

// ---------------------------------------------------------------------------
// WebTransport Datagram Configuration
// ---------------------------------------------------------------------------

/// Maximum payload size for WebTransport datagrams (bytes).
///
/// QUIC datagrams are limited by the path MTU. The typical minimum is ~1200
/// bytes after QUIC header overhead. We use a conservative value to avoid
/// fragmentation across diverse network paths. Packets larger than this
/// threshold fall back to reliable unidirectional streams.
pub const DATAGRAM_MAX_SIZE: usize = 1200;

// ---------------------------------------------------------------------------
// Audio Redundancy (RED-style encoding)
// ---------------------------------------------------------------------------

/// Enable redundant audio when FEC flag is set in AudioQualityTier.
/// Each audio packet carries the previous frame as redundancy.
/// Increases audio bandwidth by ~2x but provides loss recovery.
pub const AUDIO_REDUNDANCY_ENABLED: bool = true;

/// Audio format string signaling that a packet contains redundant data.
/// When this value appears in `AudioMetadata.audio_format`, the `data` field
/// uses the packed format: `[4-byte primary_len LE][primary_data][4-byte redundant_seq LE][redundant_data]`.
pub const AUDIO_RED_FORMAT: &str = "opus-red";

/// Number of recent audio sequence numbers to track on the receiver side
/// for deduplication of redundant frames. A small window suffices because
/// redundancy only covers the immediately previous frame.
pub const AUDIO_RED_SEQ_HISTORY_SIZE: usize = 64;

// ---------------------------------------------------------------------------
// Server Congestion Feedback
// ---------------------------------------------------------------------------

/// Number of dropped packets within `CONGESTION_WINDOW_MS` that triggers a
/// CONGESTION notification back to the sender.
pub const CONGESTION_DROP_THRESHOLD: u32 = 5;

/// Time window (milliseconds) over which drops are counted. Drop counters
/// reset after this window elapses without new drops.
pub const CONGESTION_WINDOW_MS: u64 = 1000;

/// Minimum interval between CONGESTION notifications sent to the same sender
/// (milliseconds). Prevents flooding the sender with congestion signals when
/// many packets are dropped in quick succession.
pub const CONGESTION_NOTIFY_MIN_INTERVAL_MS: u64 = 1000;

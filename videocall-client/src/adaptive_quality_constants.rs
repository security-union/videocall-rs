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
        label: "full_hd",
        max_width: 1920,
        max_height: 1080,
        target_fps: 30,
        ideal_bitrate_kbps: 2500,
        min_bitrate_kbps: 1500,
        max_bitrate_kbps: 2500,
        keyframe_interval_frames: 150, // ~5s at 30fps
    },
    VideoQualityTier {
        label: "hd_plus",
        max_width: 1600,
        max_height: 900,
        target_fps: 30,
        ideal_bitrate_kbps: 2000,
        min_bitrate_kbps: 1200,
        max_bitrate_kbps: 2500,
        keyframe_interval_frames: 150, // ~5s at 30fps
    },
    VideoQualityTier {
        label: "hd",
        max_width: 1280,
        max_height: 720,
        target_fps: 30,
        ideal_bitrate_kbps: 1500,
        min_bitrate_kbps: 800,
        max_bitrate_kbps: 2000,
        keyframe_interval_frames: 150, // ~5s at 30fps
    },
    VideoQualityTier {
        label: "standard",
        max_width: 960,
        max_height: 540,
        target_fps: 30,
        ideal_bitrate_kbps: 900,
        min_bitrate_kbps: 500,
        max_bitrate_kbps: 1500,
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
        target_fps: 20,
        ideal_bitrate_kbps: 400,
        min_bitrate_kbps: 200,
        max_bitrate_kbps: 600,
        keyframe_interval_frames: 100, // ~5s at 20fps
    },
    VideoQualityTier {
        label: "very_low",
        max_width: 480,
        max_height: 270,
        target_fps: 15,
        ideal_bitrate_kbps: 250,
        min_bitrate_kbps: 100,
        max_bitrate_kbps: 400,
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
///
/// Starting at "medium" (480p/25fps/600kbps) keeps initial bandwidth
/// under 1 Mbps, avoiding buffer bloat on constrained connections during
/// the 5s warmup period when tier transitions are suppressed. The PID
/// controller steps up to 720p/1080p if bandwidth permits, or steps
/// down to 360p/240p if constrained.
pub const DEFAULT_VIDEO_TIER_INDEX: usize = 4; // "medium"

/// Label of the video quality tier to use as camera ceiling during screen sharing.
///
/// When screen share starts, the camera is forced to this tier and capped here
/// to avoid bandwidth contention on the shared connection. Resolved by label
/// (not index) so the ceiling is correct regardless of how many tiers exist.
const SCREEN_SHARE_CAMERA_CEILING_LABEL: &str = "low";

/// Resolve the camera tier ceiling index for screen sharing.
///
/// Looks up `SCREEN_SHARE_CAMERA_CEILING_LABEL` in `VIDEO_QUALITY_TIERS`.
/// Falls back to second-lowest tier if the label isn't found.
pub fn screen_share_camera_ceiling_index() -> usize {
    VIDEO_QUALITY_TIERS
        .iter()
        .position(|t| t.label == SCREEN_SHARE_CAMERA_CEILING_LABEL)
        .unwrap_or_else(|| VIDEO_QUALITY_TIERS.len().saturating_sub(2))
}

/// Index into `SCREEN_QUALITY_TIERS` for the default starting tier.
///
/// Screen share starts at "medium" (720p/10fps) — readable content from
/// the first frame with room to step up to 1080p or down to 720p/5fps
/// based on network conditions. The floor stays at 720p to keep text
/// readable even at minimum quality.
pub const DEFAULT_SCREEN_TIER_INDEX: usize = 1; // "medium"

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
        max_width: 1280,
        max_height: 720,
        target_fps: 5,
        ideal_bitrate_kbps: 300,
        min_bitrate_kbps: 150,
        max_bitrate_kbps: 500,
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
/// Lenient FPS degradation threshold used when `effective_peer_count < 3`.
///
/// With fewer than 3 peers, p75 aggregation degenerates (for 1 peer it's
/// just that peer's value; for 2 peers it's the minimum). A single
/// struggling peer has outsized influence, so we use a more permissive
/// threshold to avoid false degradation in small meetings.
///
/// **Sender CPU tradeoff:** the lenient threshold keeps the sender encoding
/// at a higher tier for longer in 1:1 and 2-person calls. On low-power
/// devices (old Macs with VP9 software encode, budget Chromebooks) this
/// means more CPU time spent on the encoder before a step-down occurs.
/// If CPU-bound senders become a problem, tightening this value (toward
/// the standard 0.50 threshold) trades call quality for sender CPU.
pub const VIDEO_TIER_DEGRADE_FPS_RATIO_LENIENT: f64 = 0.30;
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

/// Warmup grace period after the quality manager is created (milliseconds).
///
/// During encoder startup, no frames have been produced yet so `fps_ratio`
/// reads as 0.0, which triggers aggressive step-downs (high -> low -> minimal).
/// Once frames start flowing the manager steps back up, causing visible
/// aspect-ratio glitches. This warmup period suppresses all tier transitions
/// until the encoder has had time to produce stable output.
pub const QUALITY_WARMUP_MS: f64 = 5000.0;

/// Default warmup duration used by `AdaptiveQualityManager::new()`.
/// Alias for `QUALITY_WARMUP_MS` — exists so future constructors cannot
/// silently inherit `0.0` by forgetting to set `warmup_ms`.
pub const DEFAULT_WARMUP_MS: f64 = QUALITY_WARMUP_MS;

/// Screen share warmup grace period (milliseconds).
///
/// Longer than camera warmup (5s) because receivers must initialize
/// on-demand screen decoders, receive the first screen keyframe, and
/// start reporting non-zero screen FPS. During this window the screen
/// encoder's feedback is all zeros, which would trigger aggressive
/// step-downs without the grace period.
pub const SCREEN_QUALITY_WARMUP_MS: f64 = 8000.0;

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
/// After packet loss is detected, if no keyframe arrives within this window, send PLI.
pub const KEYFRAME_REQUEST_TIMEOUT_MS: u64 = 1000;

/// Minimum interval between keyframe requests to the same sender (milliseconds).
/// Also used as the initial exponential backoff interval. Subsequent requests
/// double this interval up to `KEYFRAME_REQUEST_MAX_BACKOFF_MS`.
pub const KEYFRAME_REQUEST_MIN_INTERVAL_MS: u64 = 1000;

/// Maximum backoff interval between keyframe requests (milliseconds).
/// The backoff doubles from `KEYFRAME_REQUEST_MIN_INTERVAL_MS` and caps here.
pub const KEYFRAME_REQUEST_MAX_BACKOFF_MS: u64 = 8000;

/// Maximum number of unanswered keyframe requests before giving up.
/// After this many requests with no keyframe received, switch from
/// exponential backoff to slow periodic retry.
pub const KEYFRAME_REQUEST_MAX_UNANSWERED: u32 = 5;

/// Slow periodic retry interval (milliseconds) after the initial backoff
/// is exhausted. On lossy networks, keyframes (5-10x larger than delta
/// frames) have a higher drop probability, so giving up permanently
/// would leave the user with frozen video. A slow retry every 15 seconds
/// balances recovery against bandwidth cost.
pub const KEYFRAME_REQUEST_SLOW_RETRY_MS: u64 = 15000;

/// Time (milliseconds) with no packet loss before fully resetting PLI backoff
/// state. Prevents stale congestion history from penalizing genuinely new loss
/// events, while keeping backoff elevated during recovery windows where the
/// network is still fragile.
pub const KEYFRAME_BACKOFF_DECAY_MS: u64 = 30_000;

/// Minimum interval (milliseconds) between PLI-forced keyframes at the
/// encoder. Prevents the encoder from being dominated by back-to-back PLI
/// keyframes during a request storm. Periodic (tier-controlled) keyframes
/// are NOT subject to this cooldown.
pub const ENCODER_PLI_COOLDOWN_MS: f64 = 2000.0;

// ---------------------------------------------------------------------------
// Reconnection
// ---------------------------------------------------------------------------

/// Initial reconnection delay (milliseconds).
/// Kept low so the first retry fires quickly after a transient drop.
pub const RECONNECT_INITIAL_DELAY_MS: u64 = 500;

/// Progressive reconnection delay caps (milliseconds).
///
/// Instead of a single flat cap, the backoff limit increases with the attempt
/// count. This balances fast recovery for transient drops against server
/// protection during extended outages:
///
/// - Attempts 1-5:  cap at 2s  (quick recovery for WiFi blips)
/// - Attempts 6-15: cap at 10s (moderate backoff for longer disruptions)
/// - Attempts 16+:  cap at 30s (gentle polling during extended outages)
///
/// Over a 5-minute outage, a single client now produces ~15 attempts instead
/// of ~150, reducing server load by ~10x during widespread failures.
pub const RECONNECT_MAX_DELAY_PHASE1_MS: u64 = 2000;
pub const RECONNECT_MAX_DELAY_PHASE2_MS: u64 = 10000;
pub const RECONNECT_MAX_DELAY_PHASE3_MS: u64 = 30000;

/// Attempt thresholds for progressive backoff phases.
/// Attempts <= PHASE1 use PHASE1 cap, <= PHASE2 use PHASE2 cap, else PHASE3.
pub const RECONNECT_PHASE1_MAX_ATTEMPTS: u32 = 5;
pub const RECONNECT_PHASE2_MAX_ATTEMPTS: u32 = 15;

/// Backoff multiplier per attempt.
pub const RECONNECT_BACKOFF_MULTIPLIER: f64 = 2.0;

/// Stop reconnection if this many consecutive attempts yield zero successful
/// connections (no server responds at all). Because the client retries
/// indefinitely, this is the only hard stop: it catches auth failures and
/// server rejections early, avoiding futile retries that waste resources and
/// may trigger server-side rate limiting.
///
/// Set to 10 (not 3) to tolerate WiFi handoffs and network transitions that
/// can take 5-30 seconds. With the progressive backoff caps (2s -> 10s -> 30s),
/// 10 attempts spans ~30-60 seconds of retries, which covers most real-world
/// network disruptions.
pub const RECONNECT_CONSECUTIVE_ZERO_LIMIT: u32 = 10;

/// RTT degradation multiplier to trigger connection re-election.
/// If current RTT > max(election_rtt * this multiplier, REELECTION_RTT_MIN_THRESHOLD_MS),
/// re-elect.
pub const REELECTION_RTT_MULTIPLIER: f64 = 3.0;

/// Minimum absolute RTT degradation threshold (milliseconds).
///
/// On localhost or very fast networks the baseline RTT can be sub-millisecond
/// (e.g. 0.5ms), making a pure multiplier-based threshold trigger on normal
/// jitter (2-3ms). This floor guarantees that the threshold is never lower
/// than this value, regardless of the baseline. The effective threshold is:
///   `max(baseline * REELECTION_RTT_MULTIPLIER, REELECTION_RTT_MIN_THRESHOLD_MS)`
pub const REELECTION_RTT_MIN_THRESHOLD_MS: f64 = 50.0;

/// Number of consecutive degraded RTT samples before triggering re-election.
pub const REELECTION_CONSECUTIVE_SAMPLES: u32 = 5;

/// Minimum RTT improvement (ms) required for a re-election winner to beat the old active.
/// Prevents re-election from firing on noise when RTT values are close (hysteresis).
/// The winner must be at least this many milliseconds better than the old connection.
pub const REELECTION_MIN_IMPROVEMENT_MS: f64 = 20.0;

/// If the old active RTT exceeds this value (ms), accept any re-election winner
/// regardless of whether it is better. The connection is so degraded that any
/// alternative is worth trying.
pub const REELECTION_CATASTROPHIC_RTT_MS: f64 = 5000.0;

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

/// Minimum number of RTT samples a connection must have before it can be
/// considered for election. On high-latency connections (200ms+ RTT, common
/// in India, Africa, Southeast Asia, Australia), the QUIC/TLS or TCP+WS
/// handshake alone can take 400-900ms, leaving too few probes for a reliable
/// measurement within the default election period. Requiring multiple samples
/// ensures the elected transport is chosen on stable data, not a single
/// potentially anomalous measurement.
pub const ELECTION_MIN_RTT_SAMPLES: usize = 2;

/// Maximum number of 1-second deadline extensions allowed when the election
/// timer expires but no connection has accumulated `ELECTION_MIN_RTT_SAMPLES`.
/// This caps the total additional wait to avoid indefinitely delaying the
/// election on networks where connections never complete their handshake.
pub const ELECTION_MAX_EXTENSIONS: u32 = 2;

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
///
/// **Disabled.** Reliable QUIC streams guarantee delivery, so there is no
/// packet loss to recover from — RED provides zero benefit on this transport.
/// RED doubles audio bandwidth (2x per stream) with no corresponding gain.
/// At 100 participants this adds ~341 Mbps of unnecessary server outbound
/// bandwidth. Worse, RED activates during congestion (medium/low/emergency
/// tiers) which is exactly the wrong time to double bandwidth. NetEQ already
/// handles gap concealment on the receiver side.
///
/// The implementation is retained behind this constant so RED can be
/// re-enabled if the transport layer ever switches to unreliable delivery.
pub const AUDIO_REDUNDANCY_ENABLED: bool = false;

/// Default Opus frame duration in milliseconds.
///
/// Standard Opus frames are 20ms, which gives ~50 frames/second.
/// Used by RED unpacking to compute the recovered frame's timestamp.
pub const OPUS_FRAME_DURATION_MS: u32 = 20;

/// Audio format string signaling that a packet contains redundant data.
/// When this value appears in `AudioMetadata.audio_format`, the `data` field
/// uses the packed format: `[4-byte primary_len LE][primary_data][4-byte redundant_seq LE][redundant_data]`.
pub const AUDIO_RED_FORMAT: &str = "opus-red";

/// Number of recent audio sequence numbers to track on the receiver side
/// for deduplication of redundant frames. A small window suffices because
/// redundancy only covers the immediately previous frame.
pub const AUDIO_RED_SEQ_HISTORY_SIZE: usize = 64;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // =====================================================================
    // Video Quality Tier validation
    // =====================================================================

    #[test]
    fn test_video_tiers_not_empty() {
        assert!(
            !VIDEO_QUALITY_TIERS.is_empty(),
            "VIDEO_QUALITY_TIERS must have at least one tier"
        );
    }

    #[test]
    fn test_video_tier_bitrate_ordering() {
        for tier in VIDEO_QUALITY_TIERS {
            assert!(
                tier.min_bitrate_kbps < tier.max_bitrate_kbps,
                "tier '{}': min_bitrate ({}) must be less than max_bitrate ({})",
                tier.label,
                tier.min_bitrate_kbps,
                tier.max_bitrate_kbps,
            );
            assert!(
                tier.ideal_bitrate_kbps >= tier.min_bitrate_kbps,
                "tier '{}': ideal_bitrate ({}) must be >= min_bitrate ({})",
                tier.label,
                tier.ideal_bitrate_kbps,
                tier.min_bitrate_kbps,
            );
            assert!(
                tier.ideal_bitrate_kbps <= tier.max_bitrate_kbps,
                "tier '{}': ideal_bitrate ({}) must be <= max_bitrate ({})",
                tier.label,
                tier.ideal_bitrate_kbps,
                tier.max_bitrate_kbps,
            );
        }
    }

    #[test]
    fn test_video_tier_resolutions_positive() {
        for tier in VIDEO_QUALITY_TIERS {
            assert!(
                tier.max_width > 0 && tier.max_height > 0,
                "tier '{}': resolution must be positive ({}x{})",
                tier.label,
                tier.max_width,
                tier.max_height,
            );
        }
    }

    #[test]
    fn test_video_tier_fps_positive() {
        for tier in VIDEO_QUALITY_TIERS {
            assert!(
                tier.target_fps > 0,
                "tier '{}': target_fps must be positive",
                tier.label,
            );
        }
    }

    #[test]
    fn test_video_tier_keyframe_interval_positive() {
        for tier in VIDEO_QUALITY_TIERS {
            assert!(
                tier.keyframe_interval_frames > 0,
                "tier '{}': keyframe_interval_frames must be positive",
                tier.label,
            );
        }
    }

    #[test]
    fn test_video_tiers_descending_resolution() {
        // Tiers are ordered highest to lowest. Each tier should have
        // resolution <= the previous tier.
        for window in VIDEO_QUALITY_TIERS.windows(2) {
            let higher = &window[0];
            let lower = &window[1];
            let higher_pixels = higher.max_width as u64 * higher.max_height as u64;
            let lower_pixels = lower.max_width as u64 * lower.max_height as u64;
            assert!(
                higher_pixels >= lower_pixels,
                "tier '{}' ({}px) should have >= pixels than tier '{}' ({}px)",
                higher.label,
                higher_pixels,
                lower.label,
                lower_pixels,
            );
        }
    }

    #[test]
    fn test_video_tiers_descending_fps() {
        for window in VIDEO_QUALITY_TIERS.windows(2) {
            let higher = &window[0];
            let lower = &window[1];
            assert!(
                higher.target_fps >= lower.target_fps,
                "tier '{}' ({}fps) should have >= fps than tier '{}' ({}fps)",
                higher.label,
                higher.target_fps,
                lower.label,
                lower.target_fps,
            );
        }
    }

    #[test]
    fn test_default_video_tier_index_in_bounds() {
        assert!(
            DEFAULT_VIDEO_TIER_INDEX < VIDEO_QUALITY_TIERS.len(),
            "DEFAULT_VIDEO_TIER_INDEX ({}) out of bounds (len={})",
            DEFAULT_VIDEO_TIER_INDEX,
            VIDEO_QUALITY_TIERS.len(),
        );
    }

    #[test]
    fn test_screen_share_camera_ceiling_resolves_to_low() {
        let idx = screen_share_camera_ceiling_index();
        assert!(
            idx < VIDEO_QUALITY_TIERS.len(),
            "screen_share_camera_ceiling_index ({}) out of bounds (len={})",
            idx,
            VIDEO_QUALITY_TIERS.len(),
        );
        assert_eq!(
            VIDEO_QUALITY_TIERS[idx].label, "low",
            "ceiling should resolve to 'low' tier, got '{}' at index {}",
            VIDEO_QUALITY_TIERS[idx].label, idx,
        );
    }

    #[test]
    fn test_default_screen_tier_index_in_bounds() {
        assert!(
            DEFAULT_SCREEN_TIER_INDEX < SCREEN_QUALITY_TIERS.len(),
            "DEFAULT_SCREEN_TIER_INDEX ({}) out of bounds (len={})",
            DEFAULT_SCREEN_TIER_INDEX,
            SCREEN_QUALITY_TIERS.len(),
        );
    }

    // =====================================================================
    // Screen Share Quality Tier validation
    // =====================================================================

    #[test]
    fn test_screen_tiers_not_empty() {
        assert!(
            !SCREEN_QUALITY_TIERS.is_empty(),
            "SCREEN_QUALITY_TIERS must have at least one tier"
        );
    }

    #[test]
    fn test_screen_tier_bitrate_ordering() {
        for tier in SCREEN_QUALITY_TIERS {
            assert!(
                tier.min_bitrate_kbps < tier.max_bitrate_kbps,
                "screen tier '{}': min_bitrate ({}) must be < max_bitrate ({})",
                tier.label,
                tier.min_bitrate_kbps,
                tier.max_bitrate_kbps,
            );
            assert!(
                tier.ideal_bitrate_kbps >= tier.min_bitrate_kbps
                    && tier.ideal_bitrate_kbps <= tier.max_bitrate_kbps,
                "screen tier '{}': ideal_bitrate ({}) must be within [{}, {}]",
                tier.label,
                tier.ideal_bitrate_kbps,
                tier.min_bitrate_kbps,
                tier.max_bitrate_kbps,
            );
        }
    }

    #[test]
    fn test_screen_tiers_descending_resolution() {
        for window in SCREEN_QUALITY_TIERS.windows(2) {
            let higher = &window[0];
            let lower = &window[1];
            let h_px = higher.max_width as u64 * higher.max_height as u64;
            let l_px = lower.max_width as u64 * lower.max_height as u64;
            assert!(
                h_px >= l_px,
                "screen tier '{}' should have >= pixels than '{}'",
                higher.label,
                lower.label,
            );
        }
    }

    // =====================================================================
    // Audio Quality Tier validation
    // =====================================================================

    #[test]
    fn test_audio_tiers_not_empty() {
        assert!(
            !AUDIO_QUALITY_TIERS.is_empty(),
            "AUDIO_QUALITY_TIERS must have at least one tier"
        );
    }

    #[test]
    fn test_audio_tier_bitrate_positive() {
        for tier in AUDIO_QUALITY_TIERS {
            assert!(
                tier.bitrate_kbps > 0,
                "audio tier '{}': bitrate must be positive",
                tier.label,
            );
        }
    }

    #[test]
    fn test_audio_tiers_descending_bitrate() {
        for window in AUDIO_QUALITY_TIERS.windows(2) {
            let higher = &window[0];
            let lower = &window[1];
            assert!(
                higher.bitrate_kbps >= lower.bitrate_kbps,
                "audio tier '{}' ({}kbps) should have >= bitrate than '{}' ({}kbps)",
                higher.label,
                higher.bitrate_kbps,
                lower.label,
                lower.bitrate_kbps,
            );
        }
    }

    // =====================================================================
    // Tier transition threshold validation
    // =====================================================================

    #[test]
    fn test_hysteresis_gap_video() {
        // Recovery threshold must be higher than degrade threshold to prevent oscillation.
        assert!(
            VIDEO_TIER_RECOVER_FPS_RATIO > VIDEO_TIER_DEGRADE_FPS_RATIO,
            "recover FPS ratio ({}) must be > degrade FPS ratio ({})",
            VIDEO_TIER_RECOVER_FPS_RATIO,
            VIDEO_TIER_DEGRADE_FPS_RATIO,
        );
        assert!(
            VIDEO_TIER_RECOVER_BITRATE_RATIO > VIDEO_TIER_DEGRADE_BITRATE_RATIO,
            "recover bitrate ratio ({}) must be > degrade bitrate ratio ({})",
            VIDEO_TIER_RECOVER_BITRATE_RATIO,
            VIDEO_TIER_DEGRADE_BITRATE_RATIO,
        );
    }

    #[test]
    fn test_hysteresis_gap_audio() {
        assert!(
            AUDIO_TIER_RECOVER_FPS_RATIO > AUDIO_TIER_DEGRADE_FPS_RATIO,
            "audio recover FPS ratio ({}) must be > degrade FPS ratio ({})",
            AUDIO_TIER_RECOVER_FPS_RATIO,
            AUDIO_TIER_DEGRADE_FPS_RATIO,
        );
    }

    #[test]
    fn test_step_up_slower_than_step_down() {
        assert!(
            STEP_UP_STABILIZATION_WINDOW_MS > STEP_DOWN_REACTION_TIME_MS,
            "step-up window ({}) should be > step-down reaction time ({})",
            STEP_UP_STABILIZATION_WINDOW_MS,
            STEP_DOWN_REACTION_TIME_MS,
        );
    }

    // =====================================================================
    // PID controller constant validation
    // =====================================================================

    #[test]
    fn test_pid_gains_non_negative() {
        assert!(PID_KP >= 0.0, "PID_KP must be non-negative");
        assert!(PID_KI >= 0.0, "PID_KI must be non-negative");
        assert!(PID_KD >= 0.0, "PID_KD must be non-negative");
    }

    #[test]
    fn test_pid_output_limits() {
        assert!(
            PID_OUTPUT_MIN < PID_OUTPUT_MAX,
            "PID output min ({}) must be < max ({})",
            PID_OUTPUT_MIN,
            PID_OUTPUT_MAX,
        );
    }

    // =====================================================================
    // Congestion feedback constant validation
    // =====================================================================

    #[test]
    fn test_congestion_constants_positive() {
        assert!(CONGESTION_DROP_THRESHOLD > 0);
        assert!(CONGESTION_WINDOW_MS > 0);
        assert!(CONGESTION_NOTIFY_MIN_INTERVAL_MS > 0);
    }

    // =====================================================================
    // Tier index lookup
    // =====================================================================

    #[test]
    fn test_video_tier_lookup_by_index() {
        let tier = &VIDEO_QUALITY_TIERS[DEFAULT_VIDEO_TIER_INDEX];
        assert_eq!(tier.label, "medium", "default tier should be 'medium'");
    }

    #[test]
    fn test_all_video_tiers_have_unique_labels() {
        let labels: Vec<&str> = VIDEO_QUALITY_TIERS.iter().map(|t| t.label).collect();
        for (i, label) in labels.iter().enumerate() {
            for (j, other) in labels.iter().enumerate() {
                if i != j {
                    assert_ne!(label, other, "duplicate video tier label: {}", label);
                }
            }
        }
    }

    #[test]
    fn test_all_audio_tiers_have_unique_labels() {
        let labels: Vec<&str> = AUDIO_QUALITY_TIERS.iter().map(|t| t.label).collect();
        for (i, label) in labels.iter().enumerate() {
            for (j, other) in labels.iter().enumerate() {
                if i != j {
                    assert_ne!(label, other, "duplicate audio tier label: {}", label);
                }
            }
        }
    }
}

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

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

//! Adaptive quality state machine for automatic video/audio tier selection.
//!
//! This module monitors network health signals (received FPS, bitrate) and
//! automatically selects the appropriate [`VideoQualityTier`] and
//! [`AudioQualityTier`] from the centralized constants. It operates at a
//! higher level than the PID controller: the PID fine-tunes bitrate *within*
//! a tier, while this manager selects *which* tier to use.
//!
//! ## Degradation order
//! Video resolution/fps first, then audio bitrate (only when video is already
//! at the lowest tier).
//!
//! ## Recovery order
//! Audio recovers first, then video steps up.

use crate::adaptive_quality_constants::{
    AudioQualityTier, VideoQualityTier, AUDIO_QUALITY_TIERS, AUDIO_TIER_DEGRADE_FPS_RATIO,
    AUDIO_TIER_RECOVER_FPS_RATIO, CLIMB_COOLDOWN_BACKOFF, CLIMB_COOLDOWN_BASE_MS,
    CLIMB_COOLDOWN_MAX_MS, CRASH_MEMORY_RESET_MS, DEFAULT_SCREEN_TIER_INDEX,
    DEFAULT_VIDEO_TIER_INDEX, DEFAULT_WARMUP_MS, MIN_TIER_TRANSITION_INTERVAL_MS,
    RECOVERY_SLOWDOWN_DECAY_MS, RECOVERY_SLOWDOWN_FACTOR, REELECTION_CEILING_SUPPRESSION_MS,
    SCREEN_QUALITY_WARMUP_MS, STEP_DOWN_REACTION_TIME_MS, STEP_UP_STABILIZATION_WINDOW_MS,
    VIDEO_TIER_DEGRADE_BITRATE_RATIO, VIDEO_TIER_DEGRADE_FPS_RATIO,
    VIDEO_TIER_DEGRADE_FPS_RATIO_LENIENT, VIDEO_TIER_RECOVER_BITRATE_RATIO,
    VIDEO_TIER_RECOVER_FPS_RATIO, YOYO_DETECTION_WINDOW_MS,
};

/// Record of a single tier transition event, captured for health reporting.
#[derive(Debug, Clone)]
pub struct TierTransitionRecord {
    pub direction: &'static str, // "up" or "down"
    pub stream: &'static str, // "video", "audio"  (caller overrides to "screen" for screen share)
    pub from_tier: String,    // tier label
    pub to_tier: String,      // tier label
    pub trigger: &'static str, // "fps", "bitrate", "congestion", "coordination"
}

/// Adaptive quality manager that automatically selects video and audio tiers
/// based on network conditions. It enforces hysteresis to prevent oscillation
/// between tiers.
pub struct AdaptiveQualityManager {
    /// The tier array to index into (VIDEO_QUALITY_TIERS or SCREEN_QUALITY_TIERS).
    video_tiers: &'static [VideoQualityTier],

    /// Current video tier index (0 = highest quality, len-1 = lowest).
    video_tier_index: usize,

    /// Current audio tier index (0 = highest quality, len-1 = lowest).
    audio_tier_index: usize,

    /// Timestamp (ms) when the last tier transition occurred.
    last_transition_time_ms: f64,

    /// Timestamp (ms) when degraded conditions were first detected (for step-down).
    /// `None` when conditions are not in the degradation zone.
    degrade_start_ms: Option<f64>,

    /// Timestamp (ms) when recovery conditions were first detected (for step-up).
    /// `None` when conditions are not in the recovery zone.
    recover_start_ms: Option<f64>,

    /// Timestamp (ms) when audio degradation conditions were first detected.
    audio_degrade_start_ms: Option<f64>,

    /// Timestamp (ms) when audio recovery conditions were first detected.
    audio_recover_start_ms: Option<f64>,

    /// Timestamp (ms) when this manager was created. Used to enforce a warmup
    /// grace period during which no tier transitions occur, preventing
    /// false step-downs from zero-FPS readings at encoder startup.
    created_at_ms: f64,

    /// Warmup duration (ms). Camera uses `QUALITY_WARMUP_MS` (5s), screen
    /// uses `SCREEN_QUALITY_WARMUP_MS` (8s) because receivers need extra
    /// time to initialize on-demand screen decoders.
    warmup_ms: f64,

    /// Quality ceiling: step-up cannot go below (better quality than) this index.
    /// Used for cross-stream coordination — when screen share is active, the
    /// camera's quality manager is capped to prevent bandwidth contention.
    /// `None` means no ceiling (default).
    quality_ceiling_index: Option<usize>,

    // --- Climb-rate limiter state (PR-H) ---
    /// Crash ceiling: recovery cannot reach an index lower (better quality) than
    /// this. Armed when a yo-yo pattern is detected (two step-downs within
    /// `YOYO_DETECTION_WINDOW_MS`). Lifts one tier at a time after the decay
    /// period. `None` means no crash ceiling.
    crash_ceiling_index: Option<usize>,

    /// Timestamp (ms) when the crash ceiling lifts by one tier.
    ceiling_expires_at_ms: f64,

    /// Current ceiling decay period (ms). Starts at `CLIMB_COOLDOWN_BASE_MS`,
    /// doubles on each re-crash via `CLIMB_COOLDOWN_BACKOFF`, caps at
    /// `CLIMB_COOLDOWN_MAX_MS`.
    ceiling_decay_ms: f64,

    /// Timestamp (ms) of the most recent video step-down. Used for yo-yo
    /// detection: ceiling is only armed when two step-downs occur within
    /// `YOYO_DETECTION_WINDOW_MS`.
    last_step_down_ms: Option<f64>,

    /// Whether any step-up has occurred since the crash ceiling was armed.
    /// Distinguishes cascades (rapid step-downs without recovery) from
    /// re-crashes (step-down after recovery to the ceiling level).
    recovered_since_ceiling: bool,

    /// Timestamp (ms) when recovery slowdown was activated. The slowdown
    /// factor decays linearly from `RECOVERY_SLOWDOWN_FACTOR` to 1.0 over
    /// `RECOVERY_SLOWDOWN_DECAY_MS` relative to this timestamp.
    slowdown_activated_at_ms: Option<f64>,

    /// Timestamp (ms) when the most recent server re-election completed.
    /// Step-downs within `REELECTION_CEILING_SUPPRESSION_MS` of this
    /// timestamp do not arm the crash ceiling.
    reelection_completed_at_ms: Option<f64>,

    /// Running count of video step-downs since session start. Included in
    /// climb-limiter log messages to correlate ceiling events with the
    /// overall degradation history.
    step_down_count: u32,

    // --- Telemetry ---
    /// Counter: step-ups blocked because the crash ceiling prevented recovery.
    step_up_blocked_ceiling: u64,

    /// Counter: step-ups delayed because recovery slowdown extended the
    /// stabilization window beyond what would have triggered normally.
    step_up_blocked_slowdown: u64,

    /// Counter: step-ups blocked by the screen share coordination ceiling.
    step_up_blocked_screen_share: u64,

    /// Timestamp (ms) when the current video tier was entered. Used to compute
    /// dwell time when the tier changes.
    tier_entered_at_ms: f64,

    /// Accumulated dwell time samples: `(tier_label, dwell_ms)`. Drained by
    /// the health reporter for the `adaptive_quality_tier_dwell_ms` histogram.
    dwell_samples: Vec<(&'static str, f64)>,

    /// Buffer of tier transition events since last drain.
    transition_buffer: Vec<TierTransitionRecord>,
}

impl AdaptiveQualityManager {
    /// Internal constructor that centralizes field initialization.
    ///
    /// All public constructors delegate here so that adding a new field
    /// produces a compile error in one place, and `warmup_ms` can never
    /// silently default to `0.0`.
    fn new_with_warmup(
        video_tiers: &'static [VideoQualityTier],
        warmup_ms: f64,
        default_tier_index: usize,
    ) -> Self {
        let now = js_sys::Date::now();
        Self {
            video_tiers,
            video_tier_index: default_tier_index,
            audio_tier_index: 0,
            last_transition_time_ms: now,
            degrade_start_ms: None,
            recover_start_ms: None,
            audio_degrade_start_ms: None,
            audio_recover_start_ms: None,
            created_at_ms: now,
            warmup_ms,
            quality_ceiling_index: None,
            // Climb-rate limiter
            crash_ceiling_index: None,
            ceiling_expires_at_ms: 0.0,
            ceiling_decay_ms: CLIMB_COOLDOWN_BASE_MS,
            last_step_down_ms: None,
            recovered_since_ceiling: false,
            slowdown_activated_at_ms: None,
            reelection_completed_at_ms: None,
            step_down_count: 0,
            // Telemetry
            step_up_blocked_ceiling: 0,
            step_up_blocked_slowdown: 0,
            step_up_blocked_screen_share: 0,
            tier_entered_at_ms: now,
            dwell_samples: Vec::new(),
            transition_buffer: Vec::new(),
        }
    }

    /// Create a new manager for the given video tier array.
    ///
    /// Starts at `DEFAULT_VIDEO_TIER_INDEX` (minimal/lowest). Starting at the
    /// lowest tier ensures the system only ever upgrades, eliminating the
    /// visible dimension-change oscillation that occurs when the PID controller
    /// has not yet allocated enough bitrate for a higher tier.
    ///
    /// Use `VIDEO_QUALITY_TIERS` for camera, `SCREEN_QUALITY_TIERS` for screen share.
    ///
    /// # Lifecycle
    ///
    /// Each `CameraEncoder::start()` / `ScreenEncoder::start()` call creates a
    /// fresh `EncoderBitrateController` (which owns this manager). Climb-rate
    /// limiter state (`crash_ceiling_index`, `ceiling_decay_ms`,
    /// `slowdown_activated_at_ms`, counters, etc.) is therefore per-session
    /// and does not leak across meetings. No explicit `reset()` is needed.
    pub fn new(video_tiers: &'static [VideoQualityTier]) -> Self {
        Self::new_with_warmup(video_tiers, DEFAULT_WARMUP_MS, DEFAULT_VIDEO_TIER_INDEX)
    }

    /// Create a new manager for screen share.
    ///
    /// Starts at `DEFAULT_SCREEN_TIER_INDEX` (medium/720p) to match the
    /// camera strategy of only upgrading, never visibly downgrading. The
    /// PID controller will quickly ramp up resolution once it measures
    /// sufficient bandwidth.
    pub fn new_for_screen(video_tiers: &'static [VideoQualityTier]) -> Self {
        Self::new_with_warmup(
            video_tiers,
            SCREEN_QUALITY_WARMUP_MS,
            DEFAULT_SCREEN_TIER_INDEX,
        )
    }

    /// Process updated network signals and potentially transition tiers.
    ///
    /// Returns `true` if a tier changed (video or audio), meaning the caller
    /// should apply the new tier settings to the encoder.
    ///
    /// # Arguments
    /// * `received_fps` - FPS actually received (p75 aggregate across peers)
    /// * `target_fps` - Target FPS we are aiming for
    /// * `current_bitrate_kbps` - Actual bitrate being used
    /// * `ideal_bitrate_kbps` - Ideal bitrate for the current tier
    /// * `now_ms` - Current timestamp in milliseconds
    /// * `effective_peer_count` - Number of peers that contributed FPS data
    pub fn update(
        &mut self,
        received_fps: f64,
        target_fps: f64,
        current_bitrate_kbps: f64,
        ideal_bitrate_kbps: f64,
        now_ms: f64,
        effective_peer_count: usize,
    ) -> bool {
        // Warmup guard: during encoder startup, no frames have been produced yet
        // so fps_ratio reads as 0.0, triggering false step-downs. Suppress all
        // tier transitions until the encoder has had time to stabilize.
        if now_ms - self.created_at_ms < self.warmup_ms {
            return false;
        }

        // Guard: if target values are zero or negative, skip to avoid division by zero.
        if target_fps <= 0.0 || ideal_bitrate_kbps <= 0.0 {
            return false;
        }

        let fps_ratio = received_fps / target_fps;
        let bitrate_ratio = current_bitrate_kbps / ideal_bitrate_kbps;

        // Enforce minimum interval between any two transitions.
        let time_since_last_transition = now_ms - self.last_transition_time_ms;
        let can_transition = time_since_last_transition >= MIN_TIER_TRANSITION_INTERVAL_MS as f64;

        let mut changed = false;

        // --- Video tier logic ---
        changed |= self.update_video_tier(
            fps_ratio,
            bitrate_ratio,
            now_ms,
            can_transition,
            effective_peer_count,
        );

        // --- Audio tier logic ---
        changed |= self.update_audio_tier(fps_ratio, now_ms, can_transition);

        changed
    }

    /// Handle video tier step-down and step-up logic.
    fn update_video_tier(
        &mut self,
        fps_ratio: f64,
        bitrate_ratio: f64,
        now_ms: f64,
        can_transition: bool,
        effective_peer_count: usize,
    ) -> bool {
        let max_video_index = self.video_tiers.len().saturating_sub(1);

        // Climb-rate limiter: periodic ceiling maintenance.
        self.maybe_decay_ceiling(now_ms);
        self.maybe_reset_crash_memory(now_ms);

        // --- Step DOWN ---
        // With fewer than 3 peers, p75 aggregation degenerates, so use a more
        // lenient FPS threshold to avoid false degradation from a single outlier.
        let degrade_fps_threshold = if effective_peer_count < 3 {
            VIDEO_TIER_DEGRADE_FPS_RATIO_LENIENT
        } else {
            VIDEO_TIER_DEGRADE_FPS_RATIO
        };
        let should_degrade =
            fps_ratio < degrade_fps_threshold || bitrate_ratio < VIDEO_TIER_DEGRADE_BITRATE_RATIO;

        if should_degrade && self.video_tier_index < max_video_index {
            // Start or continue tracking degradation duration.
            let degrade_start = *self.degrade_start_ms.get_or_insert(now_ms);
            let degrade_duration = now_ms - degrade_start;

            if degrade_duration >= STEP_DOWN_REACTION_TIME_MS as f64 && can_transition {
                self.record_dwell(now_ms);
                let from_tier = self.video_tiers[self.video_tier_index].label.to_string();
                self.video_tier_index += 1;
                self.last_transition_time_ms = now_ms;
                self.degrade_start_ms = None;
                self.recover_start_ms = None;
                let to_tier = self.video_tiers[self.video_tier_index].label.to_string();
                // Primary trigger: fps drives degradation more than bitrate
                let trigger = if fps_ratio < degrade_fps_threshold {
                    "fps"
                } else {
                    "bitrate"
                };
                self.transition_buffer.push(TierTransitionRecord {
                    direction: "down",
                    stream: "video",
                    from_tier,
                    to_tier: to_tier.clone(),
                    trigger,
                });
                log::info!(
                    "AdaptiveQuality: video stepped DOWN to tier '{}' (index {}), fps_ratio={:.2}, bitrate_ratio={:.2}",
                    to_tier,
                    self.video_tier_index,
                    fps_ratio,
                    bitrate_ratio,
                );
                // Climb-rate limiter: evaluate yo-yo detection for ceiling arming.
                // N.B. must be called before updating last_step_down_ms so it reads
                // the *previous* step-down timestamp for yo-yo detection.
                self.step_down_count += 1;
                self.maybe_arm_ceiling(self.video_tier_index, now_ms);
                self.last_step_down_ms = Some(now_ms);
                return true;
            }
        } else {
            // Conditions are not in the degradation zone; reset the timer.
            self.degrade_start_ms = None;
        }

        // --- Step UP ---
        let should_recover = fps_ratio > VIDEO_TIER_RECOVER_FPS_RATIO
            && bitrate_ratio > VIDEO_TIER_RECOVER_BITRATE_RATIO;

        // Respect both the screen share coordination ceiling and crash ceiling.
        let min_allowed_index = self.effective_ceiling();

        if should_recover && self.video_tier_index > min_allowed_index {
            let recover_start = *self.recover_start_ms.get_or_insert(now_ms);
            let recover_duration = now_ms - recover_start;

            // Apply recovery slowdown: multiply the stabilization window after a yo-yo crash.
            let slowdown = self.current_slowdown_factor(now_ms);
            let effective_window = STEP_UP_STABILIZATION_WINDOW_MS as f64 * slowdown;

            if recover_duration >= effective_window && can_transition {
                self.record_dwell(now_ms);
                let from_tier = self.video_tiers[self.video_tier_index].label.to_string();
                self.video_tier_index -= 1;
                self.last_transition_time_ms = now_ms;
                self.recover_start_ms = None;
                self.degrade_start_ms = None;
                // Mark recovery for cascade vs re-crash detection.
                self.recovered_since_ceiling = true;
                let to_tier = self.video_tiers[self.video_tier_index].label.to_string();
                self.transition_buffer.push(TierTransitionRecord {
                    direction: "up",
                    stream: "video",
                    from_tier,
                    to_tier: to_tier.clone(),
                    trigger: "fps",
                });
                if slowdown > 1.01 {
                    log::info!(
                        "AdaptiveQuality: video stepped UP to tier '{}' (index {}), \
                         fps_ratio={:.2}, bitrate_ratio={:.2}, slowdown={:.2}x",
                        to_tier,
                        self.video_tier_index,
                        fps_ratio,
                        bitrate_ratio,
                        slowdown,
                    );
                } else {
                    log::info!(
                        "AdaptiveQuality: video stepped UP to tier '{}' (index {}), \
                         fps_ratio={:.2}, bitrate_ratio={:.2}",
                        to_tier,
                        self.video_tier_index,
                        fps_ratio,
                        bitrate_ratio,
                    );
                }
                return true;
            } else if slowdown > 1.01
                && recover_duration >= STEP_UP_STABILIZATION_WINDOW_MS as f64
                && can_transition
            {
                // Step-up would have triggered at normal speed but slowdown blocked it.
                self.step_up_blocked_slowdown += 1;
            }
        } else if should_recover && self.video_tier_index <= min_allowed_index {
            // At the ceiling with good conditions — track recovery time so we
            // can distinguish "would have stepped up this tick" from "conditions
            // just became good". Only count a blocked event when the stabilization
            // window is met (i.e., a step-up would have triggered absent the
            // ceiling). Without this gate, the counter increments every evaluation
            // tick, inflating telemetry.
            let recover_start = *self.recover_start_ms.get_or_insert(now_ms);
            let recover_duration = now_ms - recover_start;
            if recover_duration >= STEP_UP_STABILIZATION_WINDOW_MS as f64 && can_transition {
                let crash = self.crash_ceiling_index.unwrap_or(0);
                let coord = self.quality_ceiling_index.unwrap_or(0);
                if self.crash_ceiling_index.is_some() && crash >= coord {
                    self.step_up_blocked_ceiling += 1;
                } else if coord > 0 {
                    self.step_up_blocked_screen_share += 1;
                }
            }
        } else {
            // Conditions not good enough to recover — reset the timer.
            self.recover_start_ms = None;
        }

        false
    }

    /// Handle audio tier step-down and step-up logic.
    ///
    /// Audio only degrades when video is already at the lowest tier.
    /// Audio recovers first (before video steps up).
    fn update_audio_tier(&mut self, fps_ratio: f64, now_ms: f64, can_transition: bool) -> bool {
        let max_video_index = self.video_tiers.len().saturating_sub(1);
        let max_audio_index = AUDIO_QUALITY_TIERS.len().saturating_sub(1);
        let video_at_lowest = self.video_tier_index >= max_video_index;

        // --- Audio step DOWN ---
        // Only degrade audio when video is already at the lowest tier.
        let should_degrade_audio = video_at_lowest && fps_ratio < AUDIO_TIER_DEGRADE_FPS_RATIO;

        if should_degrade_audio && self.audio_tier_index < max_audio_index {
            let degrade_start = *self.audio_degrade_start_ms.get_or_insert(now_ms);
            let degrade_duration = now_ms - degrade_start;

            if degrade_duration >= STEP_DOWN_REACTION_TIME_MS as f64 && can_transition {
                let from_tier = AUDIO_QUALITY_TIERS[self.audio_tier_index].label.to_string();
                self.audio_tier_index += 1;
                self.last_transition_time_ms = now_ms;
                self.audio_degrade_start_ms = None;
                self.audio_recover_start_ms = None;
                let to_tier = AUDIO_QUALITY_TIERS[self.audio_tier_index].label.to_string();
                self.transition_buffer.push(TierTransitionRecord {
                    direction: "down",
                    stream: "audio",
                    from_tier,
                    to_tier: to_tier.clone(),
                    trigger: "fps",
                });
                log::info!(
                    "AdaptiveQuality: audio stepped DOWN to tier '{}' (index {}), fps_ratio={:.2}",
                    to_tier,
                    self.audio_tier_index,
                    fps_ratio,
                );
                return true;
            }
        } else {
            self.audio_degrade_start_ms = None;
        }

        // --- Audio step UP ---
        let should_recover_audio = fps_ratio > AUDIO_TIER_RECOVER_FPS_RATIO;

        if should_recover_audio && self.audio_tier_index > 0 {
            let recover_start = *self.audio_recover_start_ms.get_or_insert(now_ms);
            let recover_duration = now_ms - recover_start;

            if recover_duration >= STEP_UP_STABILIZATION_WINDOW_MS as f64 && can_transition {
                let from_tier = AUDIO_QUALITY_TIERS[self.audio_tier_index].label.to_string();
                self.audio_tier_index -= 1;
                self.last_transition_time_ms = now_ms;
                self.audio_recover_start_ms = None;
                self.audio_degrade_start_ms = None;
                let to_tier = AUDIO_QUALITY_TIERS[self.audio_tier_index].label.to_string();
                self.transition_buffer.push(TierTransitionRecord {
                    direction: "up",
                    stream: "audio",
                    from_tier,
                    to_tier: to_tier.clone(),
                    trigger: "fps",
                });
                log::info!(
                    "AdaptiveQuality: audio stepped UP to tier '{}' (index {}), fps_ratio={:.2}",
                    to_tier,
                    self.audio_tier_index,
                    fps_ratio,
                );
                return true;
            }
        } else {
            self.audio_recover_start_ms = None;
        }

        false
    }

    /// Get the current video quality tier recommendation.
    pub fn current_video_tier(&self) -> &'static VideoQualityTier {
        &self.video_tiers[self.video_tier_index]
    }

    /// Get the current audio quality tier recommendation.
    pub fn current_audio_tier(&self) -> &'static AudioQualityTier {
        &AUDIO_QUALITY_TIERS[self.audio_tier_index]
    }

    /// Get the current video tier index.
    pub fn video_tier_index(&self) -> usize {
        self.video_tier_index
    }

    /// Get the current audio tier index.
    pub fn audio_tier_index(&self) -> usize {
        self.audio_tier_index
    }

    /// Force an immediate step-down of the video quality tier.
    ///
    /// Used when the server sends a CONGESTION signal indicating that outbound
    /// packets to a receiver are being dropped. This bypasses the normal
    /// reaction-time delay and hysteresis, but still respects the minimum
    /// transition interval to avoid cascading step-downs.
    ///
    /// Returns `true` if the tier actually changed (not already at the lowest).
    pub fn force_video_step_down(&mut self, now_ms: f64) -> bool {
        // Warmup guard: same as update() — suppress forced step-downs during
        // encoder startup when zero-FPS readings would be misleading.
        if now_ms - self.created_at_ms < self.warmup_ms {
            return false;
        }

        let max_video_index = self.video_tiers.len().saturating_sub(1);
        if self.video_tier_index >= max_video_index {
            return false;
        }

        let time_since_last = now_ms - self.last_transition_time_ms;
        if time_since_last < MIN_TIER_TRANSITION_INTERVAL_MS as f64 {
            log::debug!(
                "AdaptiveQuality: congestion step-down blocked by min transition interval ({:.0}ms < {}ms)",
                time_since_last,
                MIN_TIER_TRANSITION_INTERVAL_MS,
            );
            return false;
        }

        self.record_dwell(now_ms);
        let from_tier = self.video_tiers[self.video_tier_index].label.to_string();
        self.video_tier_index += 1;
        self.last_transition_time_ms = now_ms;
        self.degrade_start_ms = None;
        self.recover_start_ms = None;
        let to_tier = self.video_tiers[self.video_tier_index].label.to_string();
        self.transition_buffer.push(TierTransitionRecord {
            direction: "down",
            stream: "video",
            from_tier,
            to_tier: to_tier.clone(),
            trigger: "congestion",
        });
        log::warn!(
            "AdaptiveQuality: CONGESTION forced video step-down to tier '{}' (index {})",
            to_tier,
            self.video_tier_index,
        );
        // Climb-rate limiter: congestion step-downs participate in yo-yo detection.
        self.step_down_count += 1;
        self.maybe_arm_ceiling(self.video_tier_index, now_ms);
        self.last_step_down_ms = Some(now_ms);
        true
    }

    /// Set a quality ceiling that prevents step-up from going below (better
    /// quality than) the given index.
    ///
    /// Used for cross-stream bandwidth coordination: when screen share is
    /// active, the camera's quality manager is capped so the combined
    /// bandwidth stays within safe limits.
    ///
    /// Pass `None` to remove the ceiling (e.g., when screen share stops).
    pub fn set_quality_ceiling(&mut self, ceiling: Option<usize>) {
        self.quality_ceiling_index = ceiling;
        if ceiling.is_some() {
            // Reset recovery timer so the ceiling takes effect immediately
            // rather than allowing a pending step-up to fire.
            self.recover_start_ms = None;
        }
        // When clearing (None), intentionally preserve recover_start_ms so
        // the camera can begin stepping up without re-waiting the full
        // STEP_UP_STABILIZATION_WINDOW_MS.
    }

    /// Force the video tier to a specific index, bypassing the one-step-at-a-time
    /// limit and the minimum transition interval.
    ///
    /// This is a coordination signal (e.g., screen share started/stopped), not a
    /// congestion reaction. It allows jumping multiple tiers in a single call and
    /// intentionally bypasses the warmup guard — coordination must take effect
    /// immediately regardless of encoder startup state.
    ///
    /// Note: if the current tier is already *below* the target (worse quality),
    /// this will step UP to the target. This is intentional for screen share
    /// coordination — it normalizes the camera to the ceiling tier.
    ///
    /// Returns `true` if the tier actually changed.
    pub fn force_video_step_to(&mut self, target: usize, now_ms: f64) -> bool {
        let max_index = self.video_tiers.len().saturating_sub(1);
        let clamped = target.min(max_index);

        if clamped == self.video_tier_index {
            return false;
        }

        // Higher index = worse quality = "DOWN"; lower index = better quality = "UP"
        let from_tier = self.video_tiers[self.video_tier_index].label.to_string();
        let direction = if clamped > self.video_tier_index {
            "down"
        } else {
            "up"
        };
        self.video_tier_index = clamped;
        self.last_transition_time_ms = now_ms;
        self.degrade_start_ms = None;
        self.recover_start_ms = None;
        let to_tier = self.video_tiers[self.video_tier_index].label.to_string();
        self.transition_buffer.push(TierTransitionRecord {
            direction,
            stream: "video",
            from_tier,
            to_tier: to_tier.clone(),
            trigger: "coordination",
        });
        log::info!(
            "AdaptiveQuality: forced video step {} to tier '{}' (index {}) for cross-stream coordination",
            direction.to_uppercase(),
            to_tier,
            self.video_tier_index,
        );
        true
    }

    /// Drain and return all tier transition records since the last drain.
    pub fn drain_transitions(&mut self) -> Vec<TierTransitionRecord> {
        std::mem::take(&mut self.transition_buffer)
    }

    // -----------------------------------------------------------------
    // Climb-rate limiter helpers
    // -----------------------------------------------------------------

    /// Compute the effective ceiling index — the tighter (higher index,
    /// meaning lower quality) of the screen share coordination ceiling and
    /// the crash ceiling. Returns 0 when neither ceiling is active.
    fn effective_ceiling(&self) -> usize {
        let coord = self.quality_ceiling_index.unwrap_or(0);
        let crash = self.crash_ceiling_index.unwrap_or(0);
        coord.max(crash)
    }

    /// Compute the current recovery slowdown factor. Decays linearly from
    /// `RECOVERY_SLOWDOWN_FACTOR` to 1.0 over `RECOVERY_SLOWDOWN_DECAY_MS`.
    pub fn current_slowdown_factor(&self, now_ms: f64) -> f64 {
        match self.slowdown_activated_at_ms {
            None => 1.0,
            Some(activated) => {
                let elapsed = now_ms - activated;
                let remaining = (1.0 - elapsed / RECOVERY_SLOWDOWN_DECAY_MS).max(0.0);
                1.0 + (RECOVERY_SLOWDOWN_FACTOR - 1.0) * remaining
            }
        }
    }

    /// Check if the crash ceiling should decay (lift by one tier) and apply.
    fn maybe_decay_ceiling(&mut self, now_ms: f64) {
        if let Some(ceiling) = self.crash_ceiling_index {
            if now_ms >= self.ceiling_expires_at_ms {
                let old_ceiling = ceiling;
                if ceiling <= 1 {
                    // Ceiling at 0 or 1 — lifting it removes the restriction.
                    self.crash_ceiling_index = None;
                    log::info!(
                        "ClimbLimiter: crash ceiling REMOVED (was tier '{}' index {}). \
                         Decay period was {:.0}s. step_downs={}",
                        self.video_tiers[old_ceiling].label,
                        old_ceiling,
                        self.ceiling_decay_ms / 1000.0,
                        self.step_down_count,
                    );
                } else {
                    let new_ceiling = ceiling - 1;
                    self.crash_ceiling_index = Some(new_ceiling);
                    self.ceiling_expires_at_ms = now_ms + self.ceiling_decay_ms;
                    log::info!(
                        "ClimbLimiter: crash ceiling LIFTED from '{}' (index {}) to '{}' (index {}). \
                         Next lift in {:.0}s. step_downs={}",
                        self.video_tiers[old_ceiling].label,
                        old_ceiling,
                        self.video_tiers[new_ceiling].label,
                        new_ceiling,
                        self.ceiling_decay_ms / 1000.0,
                        self.step_down_count,
                    );
                }
            }
        }
    }

    /// Reset crash memory (ceiling decay period + slowdown) after prolonged
    /// stability — no step-downs for `CRASH_MEMORY_RESET_MS`.
    fn maybe_reset_crash_memory(&mut self, now_ms: f64) {
        if let Some(last_down) = self.last_step_down_ms {
            if now_ms - last_down >= CRASH_MEMORY_RESET_MS {
                let had_ceiling = self.crash_ceiling_index.is_some();
                let old_decay = self.ceiling_decay_ms;
                self.ceiling_decay_ms = CLIMB_COOLDOWN_BASE_MS;
                self.slowdown_activated_at_ms = None;
                self.last_step_down_ms = None;
                if had_ceiling || old_decay > CLIMB_COOLDOWN_BASE_MS {
                    log::info!(
                        "ClimbLimiter: crash memory RESET after {:.0}s stable. \
                         Decay period reset from {:.0}s to {:.0}s. step_downs={}",
                        CRASH_MEMORY_RESET_MS / 1000.0,
                        old_decay / 1000.0,
                        CLIMB_COOLDOWN_BASE_MS / 1000.0,
                        self.step_down_count,
                    );
                }
            }
        }
    }

    /// Called after a video step-down to potentially arm or update the crash
    /// ceiling based on yo-yo detection.
    ///
    /// Design decisions:
    /// - 1b: Only arm on yo-yo (two step-downs within `YOYO_DETECTION_WINDOW_MS`)
    /// - 3: During a cascade, don't tighten — only tighten on re-crash after recovery
    /// - Re-election suppression: don't arm within 10s of a server swap
    fn maybe_arm_ceiling(&mut self, to_index: usize, now_ms: f64) {
        // Check re-election suppression window.
        if let Some(reelection_at) = self.reelection_completed_at_ms {
            if now_ms - reelection_at < REELECTION_CEILING_SUPPRESSION_MS {
                log::debug!(
                    "ClimbLimiter: ceiling arming suppressed — within {:.0}s of re-election",
                    REELECTION_CEILING_SUPPRESSION_MS / 1000.0,
                );
                return;
            }
        }

        // Yo-yo detection (decision 1b): only arm when a prior step-down
        // occurred within the detection window.
        let is_yoyo = self
            .last_step_down_ms
            .is_some_and(|prev| now_ms - prev < YOYO_DETECTION_WINDOW_MS);

        if !is_yoyo {
            // First-time degradation — record timestamp but don't arm ceiling.
            return;
        }

        match self.crash_ceiling_index {
            None => {
                // First yo-yo detection: arm the ceiling.
                self.crash_ceiling_index = Some(to_index);
                self.ceiling_expires_at_ms = now_ms + self.ceiling_decay_ms;
                self.recovered_since_ceiling = false;
                self.slowdown_activated_at_ms = Some(now_ms);
                log::info!(
                    "ClimbLimiter: crash ceiling ARMED at '{}' (index {}). \
                     Decay period {:.0}s. Recovery slowdown {:.1}x. step_downs={}",
                    self.video_tiers[to_index].label,
                    to_index,
                    self.ceiling_decay_ms / 1000.0,
                    RECOVERY_SLOWDOWN_FACTOR,
                    self.step_down_count,
                );
            }
            Some(_existing) => {
                if self.recovered_since_ceiling {
                    // Re-crash after recovery: tighten ceiling and escalate backoff.
                    //
                    // Edge case: if the ceiling was at index 2 and recovery only
                    // went 4 -> 3 (one step up), then a new crash goes 3 -> 4,
                    // `recovered_since_ceiling` is true so we re-arm at index 4
                    // (the new `to_index`) with 2x backoff. This is intentional:
                    // even a partial recovery that crashes again is evidence that
                    // the network cannot sustain the higher tier, and the looser
                    // ceiling plus longer backoff is the correct response.
                    self.crash_ceiling_index = Some(to_index);
                    self.ceiling_decay_ms =
                        (self.ceiling_decay_ms * CLIMB_COOLDOWN_BACKOFF).min(CLIMB_COOLDOWN_MAX_MS);
                    self.ceiling_expires_at_ms = now_ms + self.ceiling_decay_ms;
                    self.recovered_since_ceiling = false;
                    self.slowdown_activated_at_ms = Some(now_ms);
                    log::info!(
                        "ClimbLimiter: crash ceiling RE-ARMED at '{}' (index {}). \
                         Backoff escalated to {:.0}s. Recovery slowdown {:.1}x. step_downs={}",
                        self.video_tiers[to_index].label,
                        to_index,
                        self.ceiling_decay_ms / 1000.0,
                        RECOVERY_SLOWDOWN_FACTOR,
                        self.step_down_count,
                    );
                }
                // else: cascade (rapid step-downs without recovery) — don't
                // tighten the ceiling (design decision 3).
            }
        }
    }

    /// Record dwell time for the current tier and update the entry timestamp.
    ///
    /// The dwell sample is labeled with the *outgoing* tier (the tier we are
    /// about to leave), not the tier we are entering. This method must be
    /// called *before* `video_tier_index` is mutated.
    fn record_dwell(&mut self, now_ms: f64) {
        let dwell = now_ms - self.tier_entered_at_ms;
        if dwell > 0.0 {
            let label = self.video_tiers[self.video_tier_index].label;
            self.dwell_samples.push((label, dwell));
        }
        self.tier_entered_at_ms = now_ms;
    }

    // -----------------------------------------------------------------
    // Climb-rate limiter public API
    // -----------------------------------------------------------------

    /// Notify the quality manager that a server re-election completed.
    /// Suppresses crash ceiling arming for `REELECTION_CEILING_SUPPRESSION_MS`
    /// to avoid penalizing the FPS collapse that occurs during a server swap.
    pub fn notify_reelection_completed(&mut self, now_ms: f64) {
        self.reelection_completed_at_ms = Some(now_ms);
        log::debug!(
            "ClimbLimiter: re-election completed at {:.0}ms, ceiling suppressed for {:.0}s",
            now_ms,
            REELECTION_CEILING_SUPPRESSION_MS / 1000.0,
        );
    }

    /// Return the current crash ceiling state, if active.
    /// `(ceiling_index, tier_label, current_decay_ms)`
    pub fn crash_ceiling_info(&self) -> Option<(usize, &'static str, f64)> {
        self.crash_ceiling_index.map(|idx| {
            let label = self.video_tiers[idx].label;
            (idx, label, self.ceiling_decay_ms)
        })
    }

    /// Return the step-up blocked counters: `(ceiling, slowdown, screen_share)`.
    pub fn step_up_blocked_counts(&self) -> (u64, u64, u64) {
        (
            self.step_up_blocked_ceiling,
            self.step_up_blocked_slowdown,
            self.step_up_blocked_screen_share,
        )
    }

    /// Drain accumulated dwell time samples: `Vec<(tier_label, dwell_ms)>`.
    pub fn drain_dwell_samples(&mut self) -> Vec<(&'static str, f64)> {
        std::mem::take(&mut self.dwell_samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adaptive_quality_constants::{
        screen_share_camera_ceiling_index, SCREEN_QUALITY_TIERS, VIDEO_QUALITY_TIERS,
    };
    use wasm_bindgen_test::*;

    /// Create a manager with `created_at_ms` and `last_transition_time_ms` set
    /// to 0.0 so that tests using small `now_ms` values (e.g. 10000) are well
    /// past the warmup period and the min-transition guard.
    fn new_test_manager(video_tiers: &'static [VideoQualityTier]) -> AdaptiveQualityManager {
        let mut mgr = AdaptiveQualityManager::new(video_tiers);
        mgr.created_at_ms = 0.0;
        mgr.last_transition_time_ms = 0.0;
        mgr
    }

    #[wasm_bindgen_test]
    fn test_starts_at_default_tier() {
        let mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        assert_eq!(mgr.video_tier_index(), DEFAULT_VIDEO_TIER_INDEX);
        assert_eq!(mgr.audio_tier_index(), 0);
        assert_eq!(
            mgr.current_video_tier().label,
            VIDEO_QUALITY_TIERS[DEFAULT_VIDEO_TIER_INDEX].label
        );
        assert_eq!(mgr.current_audio_tier().label, "high");
    }

    #[wasm_bindgen_test]
    fn test_screen_starts_at_lowest_tier() {
        let mgr = AdaptiveQualityManager::new_for_screen(SCREEN_QUALITY_TIERS);
        assert_eq!(mgr.video_tier_index(), DEFAULT_SCREEN_TIER_INDEX);
        assert_eq!(mgr.audio_tier_index(), 0);
        assert_eq!(
            mgr.current_video_tier().label,
            SCREEN_QUALITY_TIERS[DEFAULT_SCREEN_TIER_INDEX].label
        );
        // Screen share starts at "medium" (720p) to avoid downgrade oscillation
        assert_eq!(mgr.current_video_tier().label, "medium");
    }

    #[wasm_bindgen_test]
    fn test_warmup_blocks_transitions() {
        let mut mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        // Override created_at_ms to a known value so we can test relative to it.
        mgr.created_at_ms = 1000.0;
        mgr.last_transition_time_ms = 0.0;
        mgr.video_tier_index = 0;

        // During warmup (now < created_at + QUALITY_WARMUP_MS), even terrible
        // conditions should not cause a transition.
        let changed = mgr.update(0.0, 30.0, 0.0, 1500.0, 2000.0, 5);
        assert!(!changed, "Should not transition during warmup");
        assert_eq!(mgr.video_tier_index(), 0);

        // Still during warmup (4999ms after creation)
        let changed = mgr.update(0.0, 30.0, 0.0, 1500.0, 5999.0, 5);
        assert!(!changed, "Should not transition during warmup");
        assert_eq!(mgr.video_tier_index(), 0);

        // After warmup (5000ms after creation), transitions should work.
        // First call starts the degrade timer.
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, 6000.0, 5);
        assert!(!changed, "Degrade timer just started");

        // After reaction time, should step down.
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, 7600.0, 5);
        assert!(changed, "Should transition after warmup + reaction time");
        assert_eq!(mgr.video_tier_index(), 1);
    }

    #[wasm_bindgen_test]
    fn test_warmup_blocks_step_up() {
        let mut mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        mgr.created_at_ms = 1000.0;
        mgr.last_transition_time_ms = 0.0;
        mgr.video_tier_index = 2; // start at "low" to test step-up

        // During warmup, good conditions should not cause a step-up.
        let changed = mgr.update(28.0, 30.0, 1350.0, 1500.0, 3000.0, 5);
        assert!(!changed, "Should not step up during warmup");
        assert_eq!(mgr.video_tier_index(), 2);
    }

    #[wasm_bindgen_test]
    fn test_screen_warmup_uses_longer_window() {
        let mut mgr = AdaptiveQualityManager::new_for_screen(SCREEN_QUALITY_TIERS);
        mgr.created_at_ms = 1000.0;
        mgr.last_transition_time_ms = 0.0;
        mgr.video_tier_index = 0; // start at highest screen tier

        // At 6s (past camera 5s warmup but still in screen 8s warmup), should block.
        let changed = mgr.update(0.0, 15.0, 0.0, 1500.0, 7000.0, 5);
        assert!(!changed, "Should not transition during 8s screen warmup");
        assert_eq!(mgr.video_tier_index(), 0);

        // 7999ms elapsed (within 8000ms screen warmup window)
        let changed = mgr.update(0.0, 15.0, 0.0, 1500.0, 8999.0, 5);
        assert!(
            !changed,
            "Should not transition at 7999ms into screen warmup"
        );
        assert_eq!(mgr.video_tier_index(), 0);

        // At 9000ms (warmup expired: 9000 - 1000 = 8000 = SCREEN_QUALITY_WARMUP_MS).
        // First call starts the degrade timer.
        let changed = mgr.update(0.0, 15.0, 0.0, 1500.0, 9000.0, 5);
        assert!(!changed, "Degrade timer just started");

        // After reaction time, should step down.
        let changed = mgr.update(0.0, 15.0, 0.0, 1500.0, 10600.0, 5);
        assert!(
            changed,
            "Should transition after 8s screen warmup + reaction time"
        );
        assert_eq!(mgr.video_tier_index(), 1);
    }

    #[wasm_bindgen_test]
    fn test_initial_last_transition_time_prevents_instant_transition() {
        // Verify that constructors initialize last_transition_time_ms to now,
        // not 0.0, so the first transition respects MIN_TIER_TRANSITION_INTERVAL_MS.
        let mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        assert!(
            mgr.last_transition_time_ms > 0.0,
            "last_transition_time_ms should be initialized to current time, not 0.0"
        );
        assert_eq!(
            mgr.last_transition_time_ms, mgr.created_at_ms,
            "last_transition_time_ms and created_at_ms should both be set to the same now() value"
        );
    }

    #[wasm_bindgen_test]
    fn test_no_change_under_good_conditions() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        // Start from a known state for this test
        mgr.video_tier_index = 0;
        // fps_ratio=1.0, bitrate_ratio=1.0 -- perfect conditions
        let changed = mgr.update(30.0, 30.0, 1500.0, 1500.0, 10000.0, 5);
        assert!(!changed);
        assert_eq!(mgr.video_tier_index(), 0);
    }

    #[wasm_bindgen_test]
    fn test_video_step_down_after_reaction_time() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        // Start at highest tier to test step-down
        mgr.video_tier_index = 0;
        let base = 10000.0;

        // fps_ratio = 0.3 (below 0.50 threshold)
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, base, 5);
        assert!(!changed, "Should not step down immediately");

        // Still degraded but not enough time elapsed
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 1000.0, 5);
        assert!(!changed);

        // After STEP_DOWN_REACTION_TIME_MS (1500ms), should step down
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 1600.0, 5);
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 1);
        assert_eq!(mgr.current_video_tier().label, "hd_plus");
    }

    #[wasm_bindgen_test]
    fn test_video_step_down_on_bitrate_ratio() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        // Start at highest tier to test step-down
        mgr.video_tier_index = 0;
        let base = 10000.0;

        // Good FPS but bitrate_ratio = 0.3 (below 0.40 threshold)
        let changed = mgr.update(28.0, 30.0, 450.0, 1500.0, base, 5);
        assert!(!changed);

        let changed = mgr.update(28.0, 30.0, 450.0, 1500.0, base + 1600.0, 5);
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 1);
    }

    #[wasm_bindgen_test]
    fn test_video_step_up_after_stabilization() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        // Force to tier 1
        mgr.video_tier_index = 1;
        let base = 10000.0;

        // Recovery conditions: fps_ratio=0.93 > 0.85, bitrate_ratio=0.90 > 0.75
        let changed = mgr.update(28.0, 30.0, 1350.0, 1500.0, base, 5);
        assert!(!changed, "Should not step up immediately");

        // Not enough time yet (4 seconds < 5000ms)
        let changed = mgr.update(28.0, 30.0, 1350.0, 1500.0, base + 4000.0, 5);
        assert!(!changed);

        // After STEP_UP_STABILIZATION_WINDOW_MS (5000ms), should step up
        let changed = mgr.update(28.0, 30.0, 1350.0, 1500.0, base + 5100.0, 5);
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 0);
        assert_eq!(mgr.current_video_tier().label, "full_hd");
    }

    #[wasm_bindgen_test]
    fn test_min_transition_interval_enforced() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        // Start at highest tier to test step-down behavior
        mgr.video_tier_index = 0;
        let base = 10000.0;

        // Step down once
        let _ = mgr.update(9.0, 30.0, 1500.0, 1500.0, base, 5);
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 1600.0, 5);
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 1);

        // Try to step down again immediately (still bad conditions)
        // Even though reaction time could be met, MIN_TIER_TRANSITION_INTERVAL_MS blocks it
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 2000.0, 5);
        assert!(!changed);

        // After MIN_TIER_TRANSITION_INTERVAL_MS (3000ms) from last transition + reaction time
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 4700.0, 5);
        // degrade_start was reset at transition; 4700 - 1600 = 3100 > 3000 (interval ok)
        // but degrade_start was None after transition, so it started fresh at the next
        // degraded update. Let's trace: after step-down at 11600, degrade_start=None.
        // At 12000 (base+2000): should_degrade=true, degrade_start set to 12000.
        //   can_transition: 12000-11600=400 < 3000, so no.
        // At 14700 (base+4700): should_degrade=true, degrade_start=12000, duration=2700 > 1500.
        //   can_transition: 14700-11600=3100 > 3000, yes!
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 2);
    }

    #[wasm_bindgen_test]
    fn test_audio_only_degrades_at_lowest_video() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        let base = 10000.0;

        // Not at lowest video tier -- audio should NOT degrade even with terrible FPS
        mgr.video_tier_index = 0;
        let changed = mgr.update(3.0, 30.0, 1500.0, 1500.0, base, 5);
        assert!(!changed || mgr.audio_tier_index() == 0);

        // Set video to lowest tier
        let max_idx = VIDEO_QUALITY_TIERS.len() - 1;
        mgr.video_tier_index = max_idx;
        mgr.last_transition_time_ms = 0.0; // allow transitions

        // fps_ratio = 0.1 < AUDIO_TIER_DEGRADE_FPS_RATIO (0.30)
        let _ = mgr.update(1.0, 10.0, 150.0, 150.0, base, 5);
        let changed = mgr.update(1.0, 10.0, 150.0, 150.0, base + 1600.0, 5);
        assert!(changed);
        assert_eq!(mgr.audio_tier_index(), 1);
        assert_eq!(mgr.current_audio_tier().label, "medium");
    }

    #[wasm_bindgen_test]
    fn test_audio_recovers_before_video() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        // Set both to degraded state
        let max_video = VIDEO_QUALITY_TIERS.len() - 1;
        mgr.video_tier_index = max_video;
        mgr.audio_tier_index = 2; // "low"
        mgr.last_transition_time_ms = 0.0;

        let base = 10000.0;

        // fps_ratio = 0.7 > AUDIO_TIER_RECOVER_FPS_RATIO (0.60) but < VIDEO_TIER_RECOVER_FPS_RATIO (0.85)
        // Audio should recover, video should not.
        let _ = mgr.update(7.0, 10.0, 150.0, 150.0, base, 5);
        let changed = mgr.update(7.0, 10.0, 150.0, 150.0, base + 5100.0, 5);
        assert!(changed);
        assert_eq!(mgr.audio_tier_index(), 1); // Audio stepped up
        assert_eq!(mgr.video_tier_index, max_video); // Video unchanged
    }

    #[wasm_bindgen_test]
    fn test_degrade_timer_resets_on_good_conditions() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        let base = 10000.0;

        // Start degrading
        let _ = mgr.update(9.0, 30.0, 1500.0, 1500.0, base, 5);
        assert!(mgr.degrade_start_ms.is_some());

        // Conditions improve before reaction time
        let changed = mgr.update(28.0, 30.0, 1500.0, 1500.0, base + 1000.0, 5);
        assert!(!changed);
        assert!(
            mgr.degrade_start_ms.is_none(),
            "Timer should reset on good conditions"
        );
    }

    #[wasm_bindgen_test]
    fn test_zero_target_fps_returns_false() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        let changed = mgr.update(0.0, 0.0, 0.0, 0.0, 10000.0, 5);
        assert!(!changed);
    }

    // =====================================================================
    // Quality ceiling tests (cross-stream coordination)
    // =====================================================================

    #[wasm_bindgen_test]
    fn test_quality_ceiling_blocks_step_up() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        // Start at index 2 ("low")
        mgr.video_tier_index = 2;
        // Set ceiling at index 2 — can't go above "low"
        mgr.set_quality_ceiling(Some(2));

        // Good conditions that would normally trigger step-up
        let base = 10000.0;
        // Sustain recovery for STEP_UP_STABILIZATION_WINDOW_MS
        for i in 0..=6 {
            let t = base + (i as f64 * 1000.0);
            mgr.update(29.0, 30.0, 1400.0, 1500.0, t, 5);
        }
        assert_eq!(
            mgr.video_tier_index(),
            2,
            "Step-up should be blocked by quality ceiling"
        );
    }

    #[wasm_bindgen_test]
    fn test_quality_ceiling_allows_step_down() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        // Start at index 1 ("medium")
        mgr.video_tier_index = 1;
        // Set ceiling at index 1 — ceiling only blocks step-up, not step-down
        mgr.set_quality_ceiling(Some(1));

        // Bad conditions to trigger step-down
        let base = 10000.0;
        // Sustain degradation for STEP_DOWN_REACTION_TIME_MS
        for i in 0..=3 {
            let t = base + (i as f64 * 1000.0);
            mgr.update(5.0, 30.0, 100.0, 600.0, t, 5);
        }
        assert!(
            mgr.video_tier_index() > 1,
            "Step-down should not be blocked by quality ceiling"
        );
    }

    #[wasm_bindgen_test]
    fn test_quality_ceiling_removal_allows_step_up() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 2;
        mgr.set_quality_ceiling(Some(2));

        // Good conditions — blocked by ceiling. These updates also accumulate
        // recovery time in recover_start_ms.
        let base = 10000.0;
        for i in 0..=6 {
            let t = base + (i as f64 * 1000.0);
            mgr.update(29.0, 30.0, 1400.0, 1500.0, t, 5);
        }
        assert_eq!(mgr.video_tier_index(), 2, "Should be blocked by ceiling");

        // Remove ceiling. The recovery timer accumulated above is preserved
        // (not reset), so the camera doesn't have to re-wait the full
        // stabilization window.
        mgr.set_quality_ceiling(None);

        // Continue with good conditions — recovery should happen within the
        // natural MIN_TIER_TRANSITION_INTERVAL_MS + stabilization window,
        // without artificially resetting last_transition_time_ms.
        let base2 = base + 20000.0; // well past min-interval from any ceiling-phase transition
        for i in 0..=8 {
            let t = base2 + (i as f64 * 1000.0);
            mgr.update(29.0, 30.0, 1400.0, 1500.0, t, 5);
        }
        assert!(
            mgr.video_tier_index() < 2,
            "Step-up should work after ceiling removal without artificial guard reset"
        );
    }

    #[wasm_bindgen_test]
    fn test_audio_stays_high_at_ceiling_tier() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        // Force camera to "low" ceiling (same as screen share coordination)
        let ceiling = screen_share_camera_ceiling_index();
        mgr.force_video_step_to(ceiling, 0.0);
        mgr.set_quality_ceiling(Some(ceiling));

        // Sustain moderate conditions — camera at ceiling, not at minimal
        let base = 10000.0;
        for i in 0..=10 {
            let t = base + (i as f64 * 1000.0);
            mgr.update(20.0, 30.0, 350.0, 400.0, t, 5);
        }
        assert_eq!(
            mgr.audio_tier_index(),
            0,
            "Audio should stay at 'high' (index 0) while camera is at ceiling tier '{}', \
             not at lowest video tier. Got audio tier index {}",
            VIDEO_QUALITY_TIERS[ceiling].label,
            mgr.audio_tier_index(),
        );
    }

    #[wasm_bindgen_test]
    fn test_force_video_step_to() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 0;

        // Force to index 2
        let changed = mgr.force_video_step_to(2, 10000.0);
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 2);

        // Force to same index — no change
        let changed = mgr.force_video_step_to(2, 10001.0);
        assert!(!changed);
    }

    #[wasm_bindgen_test]
    fn test_force_video_step_to_clamps() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        // Force to index far beyond array bounds
        let changed = mgr.force_video_step_to(999, 10000.0);
        assert!(changed);
        let max_index = VIDEO_QUALITY_TIERS.len() - 1;
        assert_eq!(
            mgr.video_tier_index(),
            max_index,
            "Should clamp to last valid index"
        );
    }

    // =====================================================================
    // Lenient threshold tests (effective_peer_count < 3)
    // =====================================================================

    #[wasm_bindgen_test]
    fn test_single_peer_uses_lenient_fps_threshold() {
        // fps_ratio=0.40 is between LENIENT (0.30) and STANDARD (0.50).
        // With 1 peer (< 3), should NOT degrade on FPS alone.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 0;
        let base = 10000.0;

        // fps_ratio=0.40, bitrate_ratio=0.90 (good bitrate)
        let changed = mgr.update(12.0, 30.0, 1350.0, 1500.0, base, 1);
        assert!(!changed, "Degrade timer should start but not fire yet");
        let changed = mgr.update(12.0, 30.0, 1350.0, 1500.0, base + 2000.0, 1);
        assert!(
            !changed,
            "fps_ratio=0.40 with 1 peer should NOT trigger degradation (lenient threshold 0.30)"
        );
        assert_eq!(mgr.video_tier_index(), 0);
    }

    #[wasm_bindgen_test]
    fn test_single_peer_degrades_below_lenient_threshold() {
        // fps_ratio=0.20 is below LENIENT (0.30). Should degrade even with 1 peer.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 0;
        let base = 10000.0;

        // fps_ratio=0.20, good bitrate
        let _ = mgr.update(6.0, 30.0, 1350.0, 1500.0, base, 1);
        let changed = mgr.update(6.0, 30.0, 1350.0, 1500.0, base + 1600.0, 1);
        assert!(
            changed,
            "fps_ratio=0.20 should degrade even with lenient threshold"
        );
        assert_eq!(mgr.video_tier_index(), 1);
    }

    #[wasm_bindgen_test]
    fn test_two_peers_uses_lenient_fps_threshold() {
        // 2 peers also uses lenient threshold (< 3).
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 0;
        let base = 10000.0;

        // fps_ratio=0.40, bitrate_ratio=0.90
        let _ = mgr.update(12.0, 30.0, 1350.0, 1500.0, base, 2);
        let changed = mgr.update(12.0, 30.0, 1350.0, 1500.0, base + 2000.0, 2);
        assert!(
            !changed,
            "fps_ratio=0.40 with 2 peers should NOT trigger degradation (lenient threshold 0.30)"
        );
        assert_eq!(mgr.video_tier_index(), 0);
    }

    #[wasm_bindgen_test]
    fn test_three_peers_uses_standard_fps_threshold() {
        // 3 peers uses the standard threshold (>= 3).
        // fps_ratio=0.40 is below STANDARD (0.50). Should degrade.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 0;
        let base = 10000.0;

        // fps_ratio=0.40, bitrate_ratio=0.90 (good bitrate)
        let _ = mgr.update(12.0, 30.0, 1350.0, 1500.0, base, 3);
        let changed = mgr.update(12.0, 30.0, 1350.0, 1500.0, base + 1600.0, 3);
        assert!(
            changed,
            "fps_ratio=0.40 with 3 peers should degrade using standard threshold (0.50)"
        );
        assert_eq!(mgr.video_tier_index(), 1);
    }

    // =====================================================================
    // Climb-rate limiter tests (PR-H)
    // =====================================================================

    /// Helper: create a test manager at a specific tier with all timers zeroed.
    fn new_test_manager_at(
        video_tiers: &'static [VideoQualityTier],
        tier_index: usize,
    ) -> AdaptiveQualityManager {
        let mut mgr = new_test_manager(video_tiers);
        mgr.video_tier_index = tier_index;
        mgr
    }

    #[wasm_bindgen_test]
    fn test_crash_ceiling_not_armed_on_first_step_down() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 0);
        let base = 10000.0;

        // Single step-down
        mgr.update(9.0, 30.0, 1500.0, 1500.0, base, 5);
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 1600.0, 5);
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 1);

        // Ceiling should NOT be armed — yo-yo detection requires two step-downs.
        assert!(
            mgr.crash_ceiling_info().is_none(),
            "Ceiling should not arm on a single step-down"
        );
    }

    #[wasm_bindgen_test]
    fn test_crash_ceiling_armed_on_yoyo() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 0);
        let base = 10000.0;

        // First step-down: 0 → 1
        mgr.update(9.0, 30.0, 1500.0, 1500.0, base, 5);
        mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 1600.0, 5);
        assert_eq!(mgr.video_tier_index(), 1);

        // Step back up: 1 → 0 (need to wait min transition + stabilization)
        let t_up_start = base + 4700.0; // past min_transition_interval
        mgr.update(28.0, 30.0, 1350.0, 1500.0, t_up_start, 5);
        let t_up = t_up_start + 5100.0;
        let changed = mgr.update(28.0, 30.0, 1350.0, 1500.0, t_up, 5);
        assert!(changed, "Should step up");
        assert_eq!(mgr.video_tier_index(), 0);

        // Second step-down: 0 → 1 (within YOYO_DETECTION_WINDOW_MS of first)
        let t_down2_start = t_up + 3100.0; // past min_transition_interval
        mgr.update(9.0, 30.0, 1500.0, 1500.0, t_down2_start, 5);
        let t_down2 = t_down2_start + 1600.0;
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, t_down2, 5);
        assert!(changed, "Should step down again");
        assert_eq!(mgr.video_tier_index(), 1);

        // Ceiling should now be armed at index 1
        let info = mgr.crash_ceiling_info();
        assert!(info.is_some(), "Ceiling should be armed after yo-yo");
        let (idx, label, decay_ms) = info.unwrap();
        assert_eq!(idx, 1);
        assert_eq!(label, "hd_plus");
        assert!((decay_ms - CLIMB_COOLDOWN_BASE_MS).abs() < 1.0);
    }

    #[wasm_bindgen_test]
    fn test_crash_ceiling_blocks_step_up() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 2);
        // Manually arm a crash ceiling at index 2
        mgr.crash_ceiling_index = Some(2);
        mgr.ceiling_expires_at_ms = 1_000_000.0; // far future
        let base = 10000.0;

        // Good conditions that would normally trigger step-up from 2 → 1
        for i in 0..=8 {
            let t = base + (i as f64 * 1000.0);
            mgr.update(29.0, 30.0, 1400.0, 1500.0, t, 5);
        }
        assert_eq!(
            mgr.video_tier_index(),
            2,
            "Step-up should be blocked by crash ceiling at index 2"
        );
    }

    #[wasm_bindgen_test]
    fn test_crash_ceiling_decays_over_time() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 3);
        // Arm ceiling at index 3, will expire at base + CLIMB_COOLDOWN_BASE_MS
        let base = 10000.0;
        mgr.crash_ceiling_index = Some(3);
        mgr.ceiling_decay_ms = CLIMB_COOLDOWN_BASE_MS;
        mgr.ceiling_expires_at_ms = base + CLIMB_COOLDOWN_BASE_MS;

        // Before decay: ceiling at 3
        assert_eq!(mgr.crash_ceiling_info().unwrap().0, 3);

        // Trigger decay by calling update after the expiry.
        let t = base + CLIMB_COOLDOWN_BASE_MS + 100.0;
        mgr.update(28.0, 30.0, 1350.0, 1500.0, t, 5);

        // Ceiling should have lifted to 2
        let info = mgr.crash_ceiling_info();
        assert!(info.is_some(), "Ceiling should still exist after one lift");
        assert_eq!(info.unwrap().0, 2, "Ceiling should have lifted from 3 to 2");
    }

    #[wasm_bindgen_test]
    fn test_crash_ceiling_fully_removed_after_decay() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 1);
        // Arm ceiling at index 1 — next decay should remove it entirely
        let base = 10000.0;
        mgr.crash_ceiling_index = Some(1);
        mgr.ceiling_decay_ms = CLIMB_COOLDOWN_BASE_MS;
        mgr.ceiling_expires_at_ms = base + CLIMB_COOLDOWN_BASE_MS;

        let t = base + CLIMB_COOLDOWN_BASE_MS + 100.0;
        mgr.update(28.0, 30.0, 1350.0, 1500.0, t, 5);

        assert!(
            mgr.crash_ceiling_info().is_none(),
            "Ceiling at index 1 should be fully removed after decay"
        );
    }

    #[wasm_bindgen_test]
    fn test_recovery_slowdown_extends_stabilization_window() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 2);
        let base = 10000.0;

        // Activate slowdown (as if a yo-yo was detected)
        mgr.slowdown_activated_at_ms = Some(base);

        // Normal stabilization window is 5000ms. With 2.0x slowdown, it should be 10000ms.
        // At 5100ms (normal window met), should NOT step up due to slowdown.
        mgr.update(28.0, 30.0, 1350.0, 1500.0, base, 5);
        let t_normal = base + 5100.0;
        let changed = mgr.update(28.0, 30.0, 1350.0, 1500.0, t_normal, 5);
        assert!(
            !changed,
            "Should NOT step up at normal window when slowdown is active"
        );
        assert_eq!(mgr.video_tier_index(), 2);

        // At 10100ms (slowed window met), should step up.
        // Need can_transition to be true: last_transition_time_ms was 0, so 10100 > 3000.
        let t_slow = base + 10100.0;
        let changed = mgr.update(28.0, 30.0, 1350.0, 1500.0, t_slow, 5);
        assert!(changed, "Should step up after slowed window is met");
        assert_eq!(mgr.video_tier_index(), 1);
    }

    #[wasm_bindgen_test]
    fn test_recovery_slowdown_decays_over_time() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        let base = 10000.0;

        // Activate slowdown
        mgr.slowdown_activated_at_ms = Some(base);

        // At time=base, slowdown should be RECOVERY_SLOWDOWN_FACTOR (2.0)
        let factor = mgr.current_slowdown_factor(base);
        assert!(
            (factor - RECOVERY_SLOWDOWN_FACTOR).abs() < 0.01,
            "Slowdown at t=0 should be {RECOVERY_SLOWDOWN_FACTOR}, got {factor}"
        );

        // At half the decay window, slowdown should be ~1.5
        let half_decay = base + RECOVERY_SLOWDOWN_DECAY_MS / 2.0;
        let factor = mgr.current_slowdown_factor(half_decay);
        assert!(
            (factor - 1.5).abs() < 0.05,
            "Slowdown at half decay should be ~1.5, got {factor}"
        );

        // At full decay, slowdown should be 1.0
        let full_decay = base + RECOVERY_SLOWDOWN_DECAY_MS;
        let factor = mgr.current_slowdown_factor(full_decay);
        assert!(
            (factor - 1.0).abs() < 0.01,
            "Slowdown after full decay should be 1.0, got {factor}"
        );

        // After decay, should still be 1.0 (not negative)
        let past_decay = base + RECOVERY_SLOWDOWN_DECAY_MS * 2.0;
        let factor = mgr.current_slowdown_factor(past_decay);
        assert!(
            (factor - 1.0).abs() < 0.01,
            "Slowdown past decay should remain 1.0, got {factor}"
        );
    }

    #[wasm_bindgen_test]
    fn test_reelection_suppression() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 0);
        let base = 10000.0;

        // First step-down at base
        mgr.update(9.0, 30.0, 1500.0, 1500.0, base, 5);
        mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 1600.0, 5);
        assert_eq!(mgr.video_tier_index(), 1);

        // Simulate re-election completed 100ms after the step-down
        mgr.notify_reelection_completed(base + 1700.0);

        // Second step-down within YOYO window AND within re-election suppression
        let t2 = base + 4800.0; // past min_transition_interval
        mgr.update(9.0, 30.0, 1500.0, 1500.0, t2, 5);
        let t2_trigger = t2 + 1600.0; // total ~8000ms, within YOYO_DETECTION_WINDOW
        mgr.update(9.0, 30.0, 1500.0, 1500.0, t2_trigger, 5);
        assert_eq!(mgr.video_tier_index(), 2);

        // Ceiling should NOT be armed because the step-down is within the
        // re-election suppression window (10s from 1700.0 = until 11700.0,
        // and t2_trigger is 6400.0 which is < 11700.0).
        assert!(
            mgr.crash_ceiling_info().is_none(),
            "Ceiling should not arm within re-election suppression window"
        );
    }

    #[wasm_bindgen_test]
    fn test_cascade_does_not_tighten_ceiling() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 0);
        let base = 10000.0;

        // First step-down: 0 → 1
        mgr.update(9.0, 30.0, 1500.0, 1500.0, base, 5);
        mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 1600.0, 5);
        assert_eq!(mgr.video_tier_index(), 1);

        // Second step-down: 1 → 2 (yo-yo arms ceiling at 2)
        let t2_start = base + 4700.0;
        mgr.update(9.0, 30.0, 1500.0, 1500.0, t2_start, 5);
        mgr.update(9.0, 30.0, 1500.0, 1500.0, t2_start + 1600.0, 5);
        assert_eq!(mgr.video_tier_index(), 2);
        let info = mgr.crash_ceiling_info();
        assert!(info.is_some(), "Ceiling should be armed at 2");
        assert_eq!(info.unwrap().0, 2);
        let initial_decay = info.unwrap().2;

        // Third step-down: 2 → 3 (cascade — no recovery happened)
        // recovered_since_ceiling should still be false
        let t3_start = t2_start + 4700.0;
        mgr.update(9.0, 30.0, 1500.0, 1500.0, t3_start, 5);
        mgr.update(9.0, 30.0, 1500.0, 1500.0, t3_start + 1600.0, 5);
        assert_eq!(mgr.video_tier_index(), 3);

        // Ceiling should NOT have tightened — decay period unchanged
        let info = mgr.crash_ceiling_info().unwrap();
        assert_eq!(info.0, 2, "Cascade should NOT tighten ceiling");
        assert!(
            (info.2 - initial_decay).abs() < 1.0,
            "Cascade should NOT escalate backoff"
        );
    }

    #[wasm_bindgen_test]
    fn test_recrash_tightens_and_escalates_backoff() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 2);
        let base = 10000.0;

        // Arm ceiling at index 2 with base decay
        mgr.crash_ceiling_index = Some(2);
        mgr.ceiling_decay_ms = CLIMB_COOLDOWN_BASE_MS;
        mgr.ceiling_expires_at_ms = base + CLIMB_COOLDOWN_BASE_MS;
        mgr.last_step_down_ms = Some(base);
        mgr.recovered_since_ceiling = false;

        // Step up to recover (simulates quality improvement)
        mgr.video_tier_index = 2; // at ceiling
        mgr.update(28.0, 30.0, 1350.0, 1500.0, base + 100.0, 5);
        // Manually step up past ceiling (ceiling just holds the index, not directly blocks here
        // since the test manipulates state directly)
        mgr.video_tier_index = 1;
        mgr.recovered_since_ceiling = true;
        mgr.last_transition_time_ms = base + 6000.0;

        // Now step down again (re-crash). Use two step-downs for yo-yo.
        // First step-down: 1 → 2
        let t_down1 = base + 20000.0;
        mgr.last_step_down_ms = Some(t_down1);
        mgr.video_tier_index = 2;

        // Second step-down: 2 → 3 (within YOYO window of first)
        let t_down2 = t_down1 + 5000.0; // within 180s YOYO_DETECTION_WINDOW_MS
        mgr.video_tier_index = 3;
        mgr.maybe_arm_ceiling(3, t_down2);
        mgr.last_step_down_ms = Some(t_down2);

        // Ceiling should have tightened to 3, and backoff should have doubled
        let info = mgr.crash_ceiling_info().unwrap();
        assert_eq!(
            info.0, 3,
            "Re-crash should tighten ceiling to step-down tier"
        );
        let expected_decay =
            (CLIMB_COOLDOWN_BASE_MS * CLIMB_COOLDOWN_BACKOFF).min(CLIMB_COOLDOWN_MAX_MS);
        assert!(
            (info.2 - expected_decay).abs() < 1.0,
            "Re-crash should escalate backoff: expected {expected_decay}, got {}",
            info.2
        );
    }

    #[wasm_bindgen_test]
    fn test_crash_memory_resets_after_stability() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 3);
        let base = 10000.0;

        // Set escalated state: ceiling at 3, decay doubled
        mgr.crash_ceiling_index = Some(3);
        mgr.ceiling_decay_ms = CLIMB_COOLDOWN_BASE_MS * 2.0;
        mgr.ceiling_expires_at_ms = base + 100_000.0; // far future
        mgr.last_step_down_ms = Some(base);
        mgr.slowdown_activated_at_ms = Some(base);

        // After CRASH_MEMORY_RESET_MS of stability (no step-downs), memory should reset.
        let t = base + CRASH_MEMORY_RESET_MS + 100.0;
        mgr.update(28.0, 30.0, 400.0, 400.0, t, 5); // neutral conditions at tier 3

        // ceiling_decay_ms should have reset to base
        // Note: crash_ceiling_index itself is not cleared by memory reset — only the
        // decay period resets. The ceiling continues its normal decay schedule.
        assert!(
            (mgr.ceiling_decay_ms - CLIMB_COOLDOWN_BASE_MS).abs() < 1.0,
            "Crash memory should reset decay to base after {:.0}s stability",
            CRASH_MEMORY_RESET_MS / 1000.0,
        );
        assert!(
            mgr.slowdown_activated_at_ms.is_none(),
            "Slowdown should be cleared after crash memory reset"
        );
        assert!(
            mgr.last_step_down_ms.is_none(),
            "last_step_down_ms should be cleared after crash memory reset"
        );
    }

    #[wasm_bindgen_test]
    fn test_effective_ceiling_uses_tighter_of_both() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);

        // No ceilings: effective ceiling = 0
        assert_eq!(mgr.effective_ceiling(), 0);

        // Screen share ceiling only
        mgr.quality_ceiling_index = Some(3);
        assert_eq!(mgr.effective_ceiling(), 3);

        // Crash ceiling only (screen share cleared)
        mgr.quality_ceiling_index = None;
        mgr.crash_ceiling_index = Some(2);
        assert_eq!(mgr.effective_ceiling(), 2);

        // Both — takes the tighter (higher index = worse quality = more restrictive)
        mgr.quality_ceiling_index = Some(3);
        mgr.crash_ceiling_index = Some(5);
        assert_eq!(
            mgr.effective_ceiling(),
            5,
            "Should use crash ceiling (5 > 3)"
        );

        mgr.quality_ceiling_index = Some(5);
        mgr.crash_ceiling_index = Some(3);
        assert_eq!(
            mgr.effective_ceiling(),
            5,
            "Should use screen share ceiling (5 > 3)"
        );
    }

    #[wasm_bindgen_test]
    fn test_force_step_down_updates_yoyo_state() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 0);
        let base = 10000.0;

        // First forced step-down
        let changed = mgr.force_video_step_down(base);
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 1);
        assert!(mgr.last_step_down_ms.is_some());
        assert!(
            mgr.crash_ceiling_info().is_none(),
            "First step-down should not arm ceiling"
        );

        // Second forced step-down within YOYO window
        let t2 = base + 3100.0; // past min_transition_interval
        let changed = mgr.force_video_step_down(t2);
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 2);

        // Ceiling should be armed (two step-downs within YOYO window)
        assert!(
            mgr.crash_ceiling_info().is_some(),
            "Ceiling should arm after two forced step-downs in YOYO window"
        );
    }

    #[wasm_bindgen_test]
    fn test_dwell_samples_recorded() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 0);
        mgr.tier_entered_at_ms = 5000.0;
        let base = 10000.0;

        // Step down: should record dwell for tier 0
        mgr.update(9.0, 30.0, 1500.0, 1500.0, base, 5);
        mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 1600.0, 5);
        assert_eq!(mgr.video_tier_index(), 1);

        let samples = mgr.drain_dwell_samples();
        assert_eq!(samples.len(), 1, "Should have one dwell sample");
        assert_eq!(
            samples[0].0, "full_hd",
            "Dwell label should be the LEFT tier"
        );
        // Dwell should be approximately now_ms - tier_entered_at_ms
        let expected_dwell = (base + 1600.0) - 5000.0;
        assert!(
            (samples[0].1 - expected_dwell).abs() < 100.0,
            "Dwell should be ~{expected_dwell}ms, got {}ms",
            samples[0].1,
        );

        // Drain again should be empty
        assert!(
            mgr.drain_dwell_samples().is_empty(),
            "Drain should be empty after first drain"
        );
    }

    #[wasm_bindgen_test]
    fn test_step_up_blocked_ceiling_counter() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 2);
        // Arm crash ceiling at index 2 — can't go below (better than) 2
        mgr.crash_ceiling_index = Some(2);
        mgr.ceiling_expires_at_ms = 1_000_000.0;
        let base = 10000.0;

        // Feed good conditions that would trigger step-up
        for i in 0..=8 {
            let t = base + (i as f64 * 1000.0);
            mgr.update(29.0, 30.0, 1400.0, 1500.0, t, 5);
        }
        assert_eq!(mgr.video_tier_index(), 2, "Should still be at ceiling");

        let (ceiling_blocked, _slowdown_blocked, _screen_blocked) = mgr.step_up_blocked_counts();
        assert!(
            ceiling_blocked > 0,
            "step_up_blocked_ceiling should be incremented"
        );
    }

    #[wasm_bindgen_test]
    fn test_step_up_blocked_slowdown_counter() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 2);
        let base = 10000.0;

        // Activate slowdown (2.0x factor)
        mgr.slowdown_activated_at_ms = Some(base);

        // Feed good conditions for normal window (5s) + a bit more
        // but NOT enough for slowed window (10s).
        mgr.update(28.0, 30.0, 1350.0, 1500.0, base, 5);
        // At 5100ms — normal window met, slowed window not met
        let t = base + 5100.0;
        mgr.update(28.0, 30.0, 1350.0, 1500.0, t, 5);

        let (_ceiling, slowdown_blocked, _screen) = mgr.step_up_blocked_counts();
        assert!(
            slowdown_blocked > 0,
            "step_up_blocked_slowdown should be incremented"
        );
    }

    #[wasm_bindgen_test]
    fn test_yoyo_detection_window_boundary() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 0);
        let base = 10000.0;

        // First step-down
        mgr.update(9.0, 30.0, 1500.0, 1500.0, base, 5);
        mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 1600.0, 5);
        assert_eq!(mgr.video_tier_index(), 1);

        // Second step-down AFTER YOYO_DETECTION_WINDOW_MS — should NOT arm ceiling
        // Need to step up first, then step down again after the window
        mgr.video_tier_index = 0;
        mgr.last_transition_time_ms = base + 1600.0;

        // Step down outside the YOYO window
        let t_outside = base + 1600.0 + YOYO_DETECTION_WINDOW_MS + 3100.0;
        mgr.last_transition_time_ms = t_outside - 3100.0; // ensure can_transition
        mgr.update(9.0, 30.0, 1500.0, 1500.0, t_outside, 5);
        mgr.update(9.0, 30.0, 1500.0, 1500.0, t_outside + 1600.0, 5);
        assert_eq!(mgr.video_tier_index(), 1);

        // Ceiling should NOT be armed — outside the yo-yo window
        assert!(
            mgr.crash_ceiling_info().is_none(),
            "Ceiling should not arm when step-downs are outside YOYO_DETECTION_WINDOW_MS"
        );
    }

    #[wasm_bindgen_test]
    fn test_recovered_since_ceiling_tracks_step_up() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 3);
        let base = 10000.0;

        // Arm ceiling at 3
        mgr.crash_ceiling_index = Some(3);
        mgr.ceiling_expires_at_ms = 1_000_000.0;
        mgr.recovered_since_ceiling = false;

        // Step up from 3 → 2 (should set recovered_since_ceiling = true)
        // Tier 3 is worse than ceiling 3, so stepping to 2... wait, ceiling is at 3.
        // effective_ceiling() = 3. video_tier_index=3, 3 > 3 is false, so can't step up.
        // Need to decay the ceiling first or start at a worse tier.
        mgr.video_tier_index = 4;
        mgr.update(28.0, 30.0, 400.0, 400.0, base, 5);
        let t_up = base + 5100.0;
        let changed = mgr.update(28.0, 30.0, 400.0, 400.0, t_up, 5);
        assert!(changed, "Should step up from 4 to 3");
        assert_eq!(mgr.video_tier_index(), 3);
        assert!(
            mgr.recovered_since_ceiling,
            "recovered_since_ceiling should be true after a step-up"
        );
    }

    #[wasm_bindgen_test]
    fn test_no_slowdown_without_ceiling() {
        let mgr = new_test_manager(VIDEO_QUALITY_TIERS);

        // No slowdown activated — factor should be 1.0
        let factor = mgr.current_slowdown_factor(10000.0);
        assert!(
            (factor - 1.0).abs() < 0.001,
            "Without slowdown, factor should be 1.0, got {factor}"
        );
    }

    #[wasm_bindgen_test]
    fn test_ceiling_and_screen_share_coexist() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 5);
        let base = 10000.0;

        // Set screen share ceiling at 5 (low tier)
        mgr.set_quality_ceiling(Some(5));
        // Arm crash ceiling at 3 (less restrictive)
        mgr.crash_ceiling_index = Some(3);
        mgr.ceiling_expires_at_ms = 1_000_000.0;

        // Effective ceiling should be max(5, 3) = 5
        assert_eq!(mgr.effective_ceiling(), 5);

        // Now remove screen share ceiling
        mgr.set_quality_ceiling(None);

        // Effective ceiling should fall to crash ceiling = 3
        assert_eq!(mgr.effective_ceiling(), 3);

        // Step-up should now be allowed up to index 3
        mgr.update(28.0, 30.0, 400.0, 400.0, base, 5);
        let t_up = base + 5100.0;
        let changed = mgr.update(28.0, 30.0, 400.0, 400.0, t_up, 5);
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 4);

        // Continue stepping up — should stop at ceiling 3
        mgr.last_transition_time_ms = t_up;
        let t_up2 = t_up + 5100.0;
        mgr.update(28.0, 30.0, 400.0, 400.0, t_up + 100.0, 5);
        let changed = mgr.update(28.0, 30.0, 400.0, 400.0, t_up2, 5);
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 3);

        // Should not step up past ceiling 3
        mgr.last_transition_time_ms = t_up2;
        let t_up3 = t_up2 + 5100.0;
        mgr.update(28.0, 30.0, 400.0, 400.0, t_up2 + 100.0, 5);
        let changed = mgr.update(28.0, 30.0, 400.0, 400.0, t_up3, 5);
        assert!(!changed, "Should be blocked at crash ceiling 3");
        assert_eq!(mgr.video_tier_index(), 3);
    }

    #[wasm_bindgen_test]
    fn test_peer_count_boundary_no_flapping() {
        // Verify that a peer joining (2→3) or dropping (3→2) near the threshold
        // boundary doesn't cause spurious tier transitions. fps_ratio=0.40 is
        // between LENIENT (0.30) and STANDARD (0.50), so it only triggers
        // degradation at ≥3 peers.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 0;
        let base = 10000.0;

        // Phase 1: 2 peers, fps_ratio=0.40 — lenient threshold, no degradation.
        let _ = mgr.update(12.0, 30.0, 1350.0, 1500.0, base, 2);
        let changed = mgr.update(12.0, 30.0, 1350.0, 1500.0, base + 1600.0, 2);
        assert!(
            !changed,
            "Should NOT degrade at 2 peers with fps_ratio=0.40"
        );
        assert_eq!(mgr.video_tier_index(), 0);

        // Phase 2: 3rd peer joins — standard threshold, degrade timer starts.
        let changed = mgr.update(12.0, 30.0, 1350.0, 1500.0, base + 2000.0, 3);
        assert!(!changed, "Degrade timer just started after peer join");

        // Phase 3: 3rd peer drops before reaction time elapses — back to lenient.
        // The degrade timer was running but the condition is no longer met under
        // the lenient threshold, so it should reset.
        let changed = mgr.update(12.0, 30.0, 1350.0, 1500.0, base + 3000.0, 2);
        assert!(
            !changed,
            "Should NOT degrade: back to 2 peers (lenient threshold)"
        );
        assert_eq!(mgr.video_tier_index(), 0);

        // Phase 4: Even after enough time, no degradation at 2 peers.
        let changed = mgr.update(12.0, 30.0, 1350.0, 1500.0, base + 5000.0, 2);
        assert!(!changed, "Still no degradation at 2 peers after long wait");
        assert_eq!(mgr.video_tier_index(), 0);
    }

    // =====================================================================
    // End-to-end climb-rate limiter test via update() API (PR-H review fix #6)
    // =====================================================================

    /// Walk the public `update()` API through the full climb-rate limiter
    /// lifecycle: arm -> recover -> re-crash, without manipulating internal
    /// state directly. This catches wiring regressions that the unit test
    /// `test_recrash_tightens_and_escalates_backoff` (which mutates fields
    /// directly) would miss.
    #[wasm_bindgen_test]
    fn test_climb_limiter_e2e_arm_recover_recrash() {
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, 2); // start at "hd" (index 2)
        let peers = 5;

        // === Phase 1: First step-down (index 2 -> 3) ===
        // Drive bad conditions for STEP_DOWN_REACTION_TIME_MS.
        let t = 10000.0;
        mgr.update(9.0, 30.0, 1500.0, 1500.0, t, peers); // start degrade timer
        let t = t + 1600.0; // 1600ms > 1500ms reaction time
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, t, peers);
        assert!(changed, "Phase 1: should step down 2->3");
        assert_eq!(mgr.video_tier_index(), 3);
        assert!(
            mgr.crash_ceiling_info().is_none(),
            "Phase 1: single step-down should NOT arm ceiling"
        );

        // === Phase 2: Second step-down (index 3 -> 4) — arms ceiling ===
        // Wait past MIN_TIER_TRANSITION_INTERVAL_MS (3000ms) then degrade again.
        let t = t + 3100.0; // past min-interval
        mgr.update(9.0, 30.0, 1500.0, 1500.0, t, peers); // start degrade timer
        let t = t + 1600.0;
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, t, peers);
        assert!(changed, "Phase 2: should step down 3->4");
        assert_eq!(mgr.video_tier_index(), 4);

        // Ceiling should now be armed at index 4 (yo-yo: two step-downs within window).
        let info = mgr.crash_ceiling_info();
        assert!(
            info.is_some(),
            "Phase 2: ceiling should be armed after yo-yo"
        );
        let (ceiling_idx, _, initial_decay) = info.unwrap();
        assert_eq!(
            ceiling_idx, 4,
            "Phase 2: ceiling should be at step-down tier"
        );
        assert!(
            (initial_decay - CLIMB_COOLDOWN_BASE_MS).abs() < 1.0,
            "Phase 2: decay should be base value"
        );

        // === Phase 3: Recover up to the ceiling (index 4 -> ... -> 4) ===
        // With ceiling at 4, recovery can only reach index 4 (already there),
        // but the act of stepping up sets recovered_since_ceiling = true.
        // We need to be *below* the ceiling to step up to it. Step down once
        // more so we can recover.
        let t = t + 3100.0; // past min-interval
        mgr.update(9.0, 30.0, 1500.0, 1500.0, t, peers); // degrade timer
        let t = t + 1600.0;
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, t, peers);
        assert!(changed, "Phase 3a: should step down 4->5");
        assert_eq!(mgr.video_tier_index(), 5);

        // Now recover: step up from 5 -> 4 (ceiling is at 4, so 4 is allowed).
        let t = t + 3100.0; // past min-interval
        mgr.update(28.0, 30.0, 400.0, 400.0, t, peers); // start recover timer
        let t = t + 5100.0; // past stabilization window (may be slowed by recovery slowdown)
                            // Slowdown is active (armed from Phase 2), so effective window ~= 5000 * 2.0 = 10000.
                            // We may need to wait longer. Use enough time for slowed window.
        let t = t + 5000.0; // total 10100ms from recover start — past 2.0x slowed window
        let changed = mgr.update(28.0, 30.0, 400.0, 400.0, t, peers);
        assert!(
            changed,
            "Phase 3b: should step up 5->4 (recovering to ceiling)"
        );
        assert_eq!(mgr.video_tier_index(), 4);

        // Verify we can't step past the ceiling.
        let t = t + 3100.0; // past min-interval
        mgr.update(28.0, 30.0, 400.0, 400.0, t, peers); // start recover timer
        let t = t + 10100.0; // long enough even with slowdown
        let changed = mgr.update(28.0, 30.0, 400.0, 400.0, t, peers);
        assert!(
            !changed,
            "Phase 3c: should NOT step past ceiling at index 4"
        );
        assert_eq!(mgr.video_tier_index(), 4);

        // === Phase 4: Re-crash — step down from ceiling (index 4 -> 5) ===
        // This should tighten the ceiling and escalate the backoff.
        let t = t + 3100.0;
        mgr.update(9.0, 30.0, 1500.0, 1500.0, t, peers); // degrade timer
        let t = t + 1600.0;
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, t, peers);
        assert!(changed, "Phase 4a: first step-down of re-crash, 4->5");
        assert_eq!(mgr.video_tier_index(), 5);

        // Second step-down for yo-yo detection within the re-crash.
        let t = t + 3100.0;
        mgr.update(9.0, 30.0, 1500.0, 1500.0, t, peers);
        let t = t + 1600.0;
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, t, peers);
        assert!(changed, "Phase 4b: second step-down of re-crash, 5->6");
        assert_eq!(mgr.video_tier_index(), 6);

        // Ceiling should have tightened (moved to 6) and backoff escalated (2x).
        let info = mgr.crash_ceiling_info();
        assert!(
            info.is_some(),
            "Phase 4: ceiling should still be armed after re-crash"
        );
        let (ceiling_idx, _, new_decay) = info.unwrap();
        assert_eq!(
            ceiling_idx, 6,
            "Phase 4: ceiling should have tightened to re-crash tier"
        );
        let expected_decay =
            (CLIMB_COOLDOWN_BASE_MS * CLIMB_COOLDOWN_BACKOFF).min(CLIMB_COOLDOWN_MAX_MS);
        assert!(
            (new_decay - expected_decay).abs() < 1.0,
            "Phase 4: backoff should have escalated from {:.0}s to {:.0}s, got {:.0}s",
            initial_decay / 1000.0,
            expected_decay / 1000.0,
            new_decay / 1000.0,
        );
    }
}

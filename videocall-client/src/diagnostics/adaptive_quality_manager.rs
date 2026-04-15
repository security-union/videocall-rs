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
    AUDIO_TIER_RECOVER_FPS_RATIO, DEFAULT_SCREEN_TIER_INDEX, DEFAULT_VIDEO_TIER_INDEX,
    MIN_TIER_TRANSITION_INTERVAL_MS, QUALITY_WARMUP_MS, STEP_DOWN_REACTION_TIME_MS,
    STEP_UP_STABILIZATION_WINDOW_MS, VIDEO_TIER_DEGRADE_BITRATE_RATIO,
    VIDEO_TIER_DEGRADE_FPS_RATIO, VIDEO_TIER_DEGRADE_FPS_RATIO_LENIENT,
    VIDEO_TIER_RECOVER_BITRATE_RATIO, VIDEO_TIER_RECOVER_FPS_RATIO,
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

    /// Quality ceiling: step-up cannot go below (better quality than) this index.
    /// Used for cross-stream coordination — when screen share is active, the
    /// camera's quality manager is capped to prevent bandwidth contention.
    /// `None` means no ceiling (default).
    quality_ceiling_index: Option<usize>,

    /// Buffer of tier transition events since last drain.
    transition_buffer: Vec<TierTransitionRecord>,
}

impl AdaptiveQualityManager {
    /// Create a new manager for the given video tier array.
    ///
    /// Starts at `DEFAULT_VIDEO_TIER_INDEX` (minimal/lowest). Starting at the
    /// lowest tier ensures the system only ever upgrades, eliminating the
    /// visible dimension-change oscillation that occurs when the PID controller
    /// has not yet allocated enough bitrate for a higher tier.
    ///
    /// Use `VIDEO_QUALITY_TIERS` for camera, `SCREEN_QUALITY_TIERS` for screen share.
    pub fn new(video_tiers: &'static [VideoQualityTier]) -> Self {
        let now = js_sys::Date::now();
        Self {
            video_tiers,
            video_tier_index: DEFAULT_VIDEO_TIER_INDEX,
            audio_tier_index: 0,
            last_transition_time_ms: now,
            degrade_start_ms: None,
            recover_start_ms: None,
            audio_degrade_start_ms: None,
            audio_recover_start_ms: None,
            created_at_ms: now,
            quality_ceiling_index: None,
            transition_buffer: Vec::new(),
        }
    }

    /// Create a new manager for screen share.
    ///
    /// Starts at `DEFAULT_SCREEN_TIER_INDEX` (medium/720p) to match the
    /// camera strategy of only upgrading, never visibly downgrading. The
    /// PID controller will quickly ramp up resolution once it measures
    /// sufficient bandwidth.
    pub fn new_for_screen(video_tiers: &'static [VideoQualityTier]) -> Self {
        let now = js_sys::Date::now();
        Self {
            video_tiers,
            video_tier_index: DEFAULT_SCREEN_TIER_INDEX,
            audio_tier_index: 0,
            last_transition_time_ms: now,
            degrade_start_ms: None,
            recover_start_ms: None,
            audio_degrade_start_ms: None,
            audio_recover_start_ms: None,
            created_at_ms: now,
            quality_ceiling_index: None,
            transition_buffer: Vec::new(),
        }
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
        if now_ms - self.created_at_ms < QUALITY_WARMUP_MS {
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
                return true;
            }
        } else {
            // Conditions are not in the degradation zone; reset the timer.
            self.degrade_start_ms = None;
        }

        // --- Step UP ---
        let should_recover = fps_ratio > VIDEO_TIER_RECOVER_FPS_RATIO
            && bitrate_ratio > VIDEO_TIER_RECOVER_BITRATE_RATIO;

        // Respect the quality ceiling set by cross-stream coordination.
        let min_allowed_index = self.quality_ceiling_index.unwrap_or(0);

        if should_recover && self.video_tier_index > min_allowed_index {
            let recover_start = *self.recover_start_ms.get_or_insert(now_ms);
            let recover_duration = now_ms - recover_start;

            if recover_duration >= STEP_UP_STABILIZATION_WINDOW_MS as f64 && can_transition {
                let from_tier = self.video_tiers[self.video_tier_index].label.to_string();
                self.video_tier_index -= 1;
                self.last_transition_time_ms = now_ms;
                self.recover_start_ms = None;
                self.degrade_start_ms = None;
                let to_tier = self.video_tiers[self.video_tier_index].label.to_string();
                self.transition_buffer.push(TierTransitionRecord {
                    direction: "up",
                    stream: "video",
                    from_tier,
                    to_tier: to_tier.clone(),
                    trigger: "fps",
                });
                log::info!(
                    "AdaptiveQuality: video stepped UP to tier '{}' (index {}), fps_ratio={:.2}, bitrate_ratio={:.2}",
                    to_tier,
                    self.video_tier_index,
                    fps_ratio,
                    bitrate_ratio,
                );
                return true;
            }
        } else {
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
        if now_ms - self.created_at_ms < QUALITY_WARMUP_MS {
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
}

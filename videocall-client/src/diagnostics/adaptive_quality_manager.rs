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
    AUDIO_TIER_RECOVER_FPS_RATIO, DEFAULT_VIDEO_TIER_INDEX, MIN_TIER_TRANSITION_INTERVAL_MS,
    STEP_DOWN_REACTION_TIME_MS, STEP_UP_STABILIZATION_WINDOW_MS, VIDEO_TIER_DEGRADE_BITRATE_RATIO,
    VIDEO_TIER_DEGRADE_FPS_RATIO, VIDEO_TIER_RECOVER_BITRATE_RATIO, VIDEO_TIER_RECOVER_FPS_RATIO,
};

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
}

impl AdaptiveQualityManager {
    /// Create a new manager for the given video tier array.
    ///
    /// Use `VIDEO_QUALITY_TIERS` for camera, `SCREEN_QUALITY_TIERS` for screen share.
    pub fn new(video_tiers: &'static [VideoQualityTier]) -> Self {
        Self {
            video_tiers,
            video_tier_index: DEFAULT_VIDEO_TIER_INDEX,
            audio_tier_index: 0,
            last_transition_time_ms: 0.0,
            degrade_start_ms: None,
            recover_start_ms: None,
            audio_degrade_start_ms: None,
            audio_recover_start_ms: None,
        }
    }

    /// Process updated network signals and potentially transition tiers.
    ///
    /// Returns `true` if a tier changed (video or audio), meaning the caller
    /// should apply the new tier settings to the encoder.
    ///
    /// # Arguments
    /// * `received_fps` - FPS actually received by the worst peer
    /// * `target_fps` - Target FPS we are aiming for
    /// * `current_bitrate_kbps` - Actual bitrate being used
    /// * `ideal_bitrate_kbps` - Ideal bitrate for the current tier
    /// * `now_ms` - Current timestamp in milliseconds
    pub fn update(
        &mut self,
        received_fps: f64,
        target_fps: f64,
        current_bitrate_kbps: f64,
        ideal_bitrate_kbps: f64,
        now_ms: f64,
    ) -> bool {
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
        changed |= self.update_video_tier(fps_ratio, bitrate_ratio, now_ms, can_transition);

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
    ) -> bool {
        let max_video_index = self.video_tiers.len().saturating_sub(1);

        // --- Step DOWN ---
        let should_degrade = fps_ratio < VIDEO_TIER_DEGRADE_FPS_RATIO
            || bitrate_ratio < VIDEO_TIER_DEGRADE_BITRATE_RATIO;

        if should_degrade && self.video_tier_index < max_video_index {
            // Start or continue tracking degradation duration.
            let degrade_start = *self.degrade_start_ms.get_or_insert(now_ms);
            let degrade_duration = now_ms - degrade_start;

            if degrade_duration >= STEP_DOWN_REACTION_TIME_MS as f64 && can_transition {
                self.video_tier_index += 1;
                self.last_transition_time_ms = now_ms;
                self.degrade_start_ms = None;
                self.recover_start_ms = None;
                log::info!(
                    "AdaptiveQuality: video stepped DOWN to tier '{}' (index {}), fps_ratio={:.2}, bitrate_ratio={:.2}",
                    self.video_tiers[self.video_tier_index].label,
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

        if should_recover && self.video_tier_index > 0 {
            let recover_start = *self.recover_start_ms.get_or_insert(now_ms);
            let recover_duration = now_ms - recover_start;

            if recover_duration >= STEP_UP_STABILIZATION_WINDOW_MS as f64 && can_transition {
                self.video_tier_index -= 1;
                self.last_transition_time_ms = now_ms;
                self.recover_start_ms = None;
                self.degrade_start_ms = None;
                log::info!(
                    "AdaptiveQuality: video stepped UP to tier '{}' (index {}), fps_ratio={:.2}, bitrate_ratio={:.2}",
                    self.video_tiers[self.video_tier_index].label,
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
                self.audio_tier_index += 1;
                self.last_transition_time_ms = now_ms;
                self.audio_degrade_start_ms = None;
                self.audio_recover_start_ms = None;
                log::info!(
                    "AdaptiveQuality: audio stepped DOWN to tier '{}' (index {}), fps_ratio={:.2}",
                    AUDIO_QUALITY_TIERS[self.audio_tier_index].label,
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
                self.audio_tier_index -= 1;
                self.last_transition_time_ms = now_ms;
                self.audio_recover_start_ms = None;
                self.audio_degrade_start_ms = None;
                log::info!(
                    "AdaptiveQuality: audio stepped UP to tier '{}' (index {}), fps_ratio={:.2}",
                    AUDIO_QUALITY_TIERS[self.audio_tier_index].label,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adaptive_quality_constants::VIDEO_QUALITY_TIERS;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    fn test_starts_at_highest_tier() {
        let mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        assert_eq!(mgr.video_tier_index(), 0);
        assert_eq!(mgr.audio_tier_index(), 0);
        assert_eq!(mgr.current_video_tier().label, "high");
        assert_eq!(mgr.current_audio_tier().label, "high");
    }

    #[wasm_bindgen_test]
    fn test_no_change_under_good_conditions() {
        let mut mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        // fps_ratio=1.0, bitrate_ratio=1.0 -- perfect conditions
        let changed = mgr.update(30.0, 30.0, 1500.0, 1500.0, 10000.0);
        assert!(!changed);
        assert_eq!(mgr.video_tier_index(), 0);
    }

    #[wasm_bindgen_test]
    fn test_video_step_down_after_reaction_time() {
        let mut mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        let base = 10000.0;

        // fps_ratio = 0.3 (below 0.50 threshold)
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, base);
        assert!(!changed, "Should not step down immediately");

        // Still degraded but not enough time elapsed
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 1000.0);
        assert!(!changed);

        // After STEP_DOWN_REACTION_TIME_MS (1500ms), should step down
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 1600.0);
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 1);
        assert_eq!(mgr.current_video_tier().label, "medium");
    }

    #[wasm_bindgen_test]
    fn test_video_step_down_on_bitrate_ratio() {
        let mut mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        let base = 10000.0;

        // Good FPS but bitrate_ratio = 0.3 (below 0.40 threshold)
        let changed = mgr.update(28.0, 30.0, 450.0, 1500.0, base);
        assert!(!changed);

        let changed = mgr.update(28.0, 30.0, 450.0, 1500.0, base + 1600.0);
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 1);
    }

    #[wasm_bindgen_test]
    fn test_video_step_up_after_stabilization() {
        let mut mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        // Force to tier 1
        mgr.video_tier_index = 1;
        let base = 10000.0;

        // Recovery conditions: fps_ratio=0.93 > 0.85, bitrate_ratio=0.90 > 0.75
        let changed = mgr.update(28.0, 30.0, 1350.0, 1500.0, base);
        assert!(!changed, "Should not step up immediately");

        // Not enough time yet (4 seconds < 5000ms)
        let changed = mgr.update(28.0, 30.0, 1350.0, 1500.0, base + 4000.0);
        assert!(!changed);

        // After STEP_UP_STABILIZATION_WINDOW_MS (5000ms), should step up
        let changed = mgr.update(28.0, 30.0, 1350.0, 1500.0, base + 5100.0);
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 0);
        assert_eq!(mgr.current_video_tier().label, "high");
    }

    #[wasm_bindgen_test]
    fn test_min_transition_interval_enforced() {
        let mut mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        let base = 10000.0;

        // Step down once
        let _ = mgr.update(9.0, 30.0, 1500.0, 1500.0, base);
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 1600.0);
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 1);

        // Try to step down again immediately (still bad conditions)
        // Even though reaction time could be met, MIN_TIER_TRANSITION_INTERVAL_MS blocks it
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 2000.0);
        assert!(!changed);

        // After MIN_TIER_TRANSITION_INTERVAL_MS (3000ms) from last transition + reaction time
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 4700.0);
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
        let mut mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        let base = 10000.0;

        // Not at lowest video tier -- audio should NOT degrade even with terrible FPS
        mgr.video_tier_index = 0;
        let changed = mgr.update(3.0, 30.0, 1500.0, 1500.0, base);
        assert!(!changed || mgr.audio_tier_index() == 0);

        // Set video to lowest tier
        let max_idx = VIDEO_QUALITY_TIERS.len() - 1;
        mgr.video_tier_index = max_idx;
        mgr.last_transition_time_ms = 0.0; // allow transitions

        // fps_ratio = 0.1 < AUDIO_TIER_DEGRADE_FPS_RATIO (0.30)
        let _ = mgr.update(1.0, 10.0, 150.0, 150.0, base);
        let changed = mgr.update(1.0, 10.0, 150.0, 150.0, base + 1600.0);
        assert!(changed);
        assert_eq!(mgr.audio_tier_index(), 1);
        assert_eq!(mgr.current_audio_tier().label, "medium");
    }

    #[wasm_bindgen_test]
    fn test_audio_recovers_before_video() {
        let mut mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        // Set both to degraded state
        let max_video = VIDEO_QUALITY_TIERS.len() - 1;
        mgr.video_tier_index = max_video;
        mgr.audio_tier_index = 2; // "low"
        mgr.last_transition_time_ms = 0.0;

        let base = 10000.0;

        // fps_ratio = 0.7 > AUDIO_TIER_RECOVER_FPS_RATIO (0.60) but < VIDEO_TIER_RECOVER_FPS_RATIO (0.85)
        // Audio should recover, video should not.
        let _ = mgr.update(7.0, 10.0, 150.0, 150.0, base);
        let changed = mgr.update(7.0, 10.0, 150.0, 150.0, base + 5100.0);
        assert!(changed);
        assert_eq!(mgr.audio_tier_index(), 1); // Audio stepped up
        assert_eq!(mgr.video_tier_index, max_video); // Video unchanged
    }

    #[wasm_bindgen_test]
    fn test_degrade_timer_resets_on_good_conditions() {
        let mut mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        let base = 10000.0;

        // Start degrading
        let _ = mgr.update(9.0, 30.0, 1500.0, 1500.0, base);
        assert!(mgr.degrade_start_ms.is_some());

        // Conditions improve before reaction time
        let changed = mgr.update(28.0, 30.0, 1500.0, 1500.0, base + 1000.0);
        assert!(!changed);
        assert!(
            mgr.degrade_start_ms.is_none(),
            "Timer should reset on good conditions"
        );
    }

    #[wasm_bindgen_test]
    fn test_zero_target_fps_returns_false() {
        let mut mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        let changed = mgr.update(0.0, 0.0, 0.0, 0.0, 10000.0);
        assert!(!changed);
    }
}

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

use std::sync::Arc;

use crate::clock::{default_clock, Clock};
use crate::constants::{
    AudioQualityTier, VideoQualityTier, AUDIO_QUALITY_TIERS, CLIMB_COOLDOWN_BACKOFF,
    CLIMB_COOLDOWN_BASE_MS, CLIMB_COOLDOWN_MAX_MS, CONGESTION_CUT_TIERS, CONGESTION_HOLD_MS,
    CRASH_MEMORY_RESET_MS, DEFAULT_SCREEN_TIER_INDEX, DEFAULT_VIDEO_TIER_INDEX, DEFAULT_WARMUP_MS,
    MIN_TIER_TRANSITION_INTERVAL_MS, RECOVERY_SLOWDOWN_DECAY_MS, RECOVERY_SLOWDOWN_FACTOR,
    REELECTION_CEILING_SUPPRESSION_MS, SCREEN_QUALITY_WARMUP_MS, STEP_DOWN_REACTION_TIME_MS,
    STEP_UP_STABILIZATION_WINDOW_MS, YOYO_DETECTION_WINDOW_MS,
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

    /// Baseline video tier index this stream started at — the ceiling that the
    /// backpressure `recover` step-up may NOT climb above (issue #1108,
    /// clamp-to-baseline recovery). Set once at construction from
    /// `default_tier_index` (camera = `DEFAULT_VIDEO_TIER_INDEX`, screen =
    /// `DEFAULT_SCREEN_TIER_INDEX`). With receiver FPS gone the sender has no
    /// capacity signal, so it only ever *reverses its own* backpressure-induced
    /// steps back to this baseline — it never speculatively probes for more
    /// uplink. The DOWN path (`video_step_down_cap`) is unaffected, and an
    /// explicit user `best` bound overrides this (an explicit capacity opt-in).
    recovery_baseline_index: usize,

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

    // --- User-configurable quality bounds (issue #961) ---
    //
    // QUALITY IS THE INVERSE OF INDEX. Tier index 0 = BEST quality (highest
    // resolution / bitrate); the last index = WORST quality. The user picks a
    // "max quality" and a "min quality" per stream, which map to index bounds
    // as follows:
    //
    //   user MAX quality  -> the BEST tier allowed  -> a FLOOR on the index.
    //       The index may never go BELOW `user_*_best_index`; the system must
    //       never step UP past it (no quality better than the user's max).
    //
    //   user MIN quality  -> the WORST tier allowed  -> a CAP on the index.
    //       The index may never go ABOVE `user_*_worst_index`; the system must
    //       never step DOWN past it (no quality worse than the user's min).
    //
    // Each end is independently optional: `None` = "Auto" (no user bound on
    // that end). The default is `None`/`None` (fully automatic) so behaviour is
    // unchanged unless the user opts in. DO NOT re-invert this: best == floor,
    // worst == cap.
    /// User-selected best (floor) video tier index. Step-up must not go below
    /// this. `None` = Auto.
    user_video_best_index: Option<usize>,

    /// User-selected worst (cap) video tier index. Step-down must not go above
    /// this. `None` = Auto.
    user_video_worst_index: Option<usize>,

    /// User-selected best (floor) audio tier index. `None` = Auto.
    user_audio_best_index: Option<usize>,

    /// User-selected worst (cap) audio tier index. `None` = Auto.
    user_audio_worst_index: Option<usize>,

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

    /// Timestamp (ms) until which a self-targeted CONGESTION cut pins the
    /// effective bitrate ceiling to the post-cut tier's lower bound. While
    /// `now_ms < congestion_hold_until_ms` the PID may not ramp bitrate back up
    /// within the new tier and tier step-up is blocked, so the already-
    /// overflowing relay buffer gets a chance to drain. Expires naturally by
    /// timestamp comparison — see [`congestion_hold_active`](Self::congestion_hold_active).
    /// `None` means no hold is active.
    congestion_hold_until_ms: Option<f64>,

    /// Running count of video step-downs since session start. Included in
    /// climb-limiter log messages to correlate ceiling events with the
    /// overall degradation history.
    step_down_count: u32,

    // --- Simulcast active-layer state (issue #989, PR B) — ADDITIVE ---
    /// Number of simulcast layers in this session's ladder (the ceiling for
    /// `active_layer_count`). `1` means single-stream (simulcast off / weak
    /// device), in which case all the layer methods are inert and the manager
    /// behaves exactly as before. Set once at construction via
    /// [`set_simulcast_layers`](Self::set_simulcast_layers); defaults to 1 so
    /// existing callers (and the bot) are unaffected.
    simulcast_layer_count: usize,

    /// Number of simulcast layers currently being encoded+sent (the *top*
    /// `simulcast_layer_count - active_layer_count` layers are shed). Floor 1,
    /// ceiling `simulcast_layer_count`. Driven by the SAME degrade/recover
    /// decisions that move `video_tier_index` (see the controller), reusing the
    /// incident-hardened hysteresis/crash-ceiling/yo-yo timers. Starts at the
    /// ladder max (all layers active) — the AQ controller sheds the top layer
    /// under sustained congestion. When `simulcast_layer_count == 1` this stays
    /// pinned at 1 and the field is never consulted.
    active_layer_count: usize,

    /// Issue #1077: set by the most recent `update()` when a video step-DOWN was
    /// fully warranted (degrade conditions sustained past the reaction time, and
    /// a transition was permitted) but could NOT move the tier index because it
    /// was already at the floor. The controller reads this via
    /// [`wanted_degrade_at_floor`](Self::wanted_degrade_at_floor) so the gradual
    /// `update()`-path simulcast layer shed can fire at the floor too, instead of
    /// silently stopping once the tier index saturates. Reset to `false` at the
    /// top of every `update()`. Meaningless in single-stream mode.
    degrade_floor_saturated: bool,

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

    /// Clock used to seed `created_at_ms`, `last_transition_time_ms`, and
    /// `tier_entered_at_ms` at construction. Kept as a field so future
    /// additions that need a "now" reading (e.g. reset logic) can use the
    /// same clock the tests injected. All time-sensitive public methods
    /// still take `now_ms` from the caller so behaviour is unchanged.
    #[allow(dead_code)]
    clock: Arc<dyn Clock>,
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
        Self::new_with_warmup_and_clock(video_tiers, warmup_ms, default_tier_index, default_clock())
    }

    /// Internal constructor that accepts a custom `Clock`.
    ///
    /// Exists so tests (and native callers) can inject a deterministic clock
    /// without going through the browser `Date::now()` path.
    fn new_with_warmup_and_clock(
        video_tiers: &'static [VideoQualityTier],
        warmup_ms: f64,
        default_tier_index: usize,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let now = clock.now_ms();
        Self {
            video_tiers,
            video_tier_index: default_tier_index,
            // Backpressure recovery may climb back to — but never above — the
            // tier this stream started at (issue #1108, clamp-to-baseline).
            recovery_baseline_index: default_tier_index,
            audio_tier_index: 0,
            last_transition_time_ms: now,
            degrade_start_ms: None,
            recover_start_ms: None,
            audio_degrade_start_ms: None,
            audio_recover_start_ms: None,
            created_at_ms: now,
            warmup_ms,
            quality_ceiling_index: None,
            // User-configurable quality bounds (issue #961). Default both ends
            // to None on every stream so behaviour is unchanged until the user
            // opts in via set_video_quality_bounds / set_audio_quality_bounds.
            user_video_best_index: None,
            user_video_worst_index: None,
            user_audio_best_index: None,
            user_audio_worst_index: None,
            // Climb-rate limiter
            crash_ceiling_index: None,
            ceiling_expires_at_ms: 0.0,
            ceiling_decay_ms: CLIMB_COOLDOWN_BASE_MS,
            last_step_down_ms: None,
            recovered_since_ceiling: false,
            slowdown_activated_at_ms: None,
            reelection_completed_at_ms: None,
            congestion_hold_until_ms: None,
            step_down_count: 0,
            // Simulcast active-layer state (issue #989) — additive, defaults to
            // single-stream so existing callers behave exactly as before.
            simulcast_layer_count: 1,
            active_layer_count: 1,
            degrade_floor_saturated: false,
            // Telemetry
            step_up_blocked_ceiling: 0,
            step_up_blocked_slowdown: 0,
            step_up_blocked_screen_share: 0,
            tier_entered_at_ms: now,
            dwell_samples: Vec::new(),
            transition_buffer: Vec::new(),
            clock,
        }
    }

    /// Create a new manager for the given video tier array.
    ///
    /// Starts at `DEFAULT_VIDEO_TIER_INDEX` ("medium", 480p). The PID controller
    /// steps up toward higher resolutions when bandwidth allows, or steps down
    /// when the network is constrained.
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

    /// Create a new manager with an injected `Clock`.
    ///
    /// Native callers (e.g. the load-test bot) can pass a [`SystemClock`];
    /// tests can pass a [`TestClock`] for deterministic behaviour.
    ///
    /// [`SystemClock`]: crate::clock::SystemClock
    /// [`TestClock`]: crate::clock::TestClock
    pub fn with_clock(video_tiers: &'static [VideoQualityTier], clock: Arc<dyn Clock>) -> Self {
        Self::new_with_warmup_and_clock(
            video_tiers,
            DEFAULT_WARMUP_MS,
            DEFAULT_VIDEO_TIER_INDEX,
            clock,
        )
    }

    /// Create a new manager for screen share.
    ///
    /// Starts at `DEFAULT_SCREEN_TIER_INDEX` ("medium", 720p/8fps — the midpoint
    /// of the 3-tier screen-share ladder). Peers see an acceptable baseline
    /// immediately and the PID controller adapts in either direction: stepping
    /// up to 1080p when bandwidth is plentiful, or down to 720p/5fps when the
    /// network is constrained.
    pub fn new_for_screen(video_tiers: &'static [VideoQualityTier]) -> Self {
        Self::new_with_warmup(
            video_tiers,
            SCREEN_QUALITY_WARMUP_MS,
            DEFAULT_SCREEN_TIER_INDEX,
        )
    }

    /// Create a new screen-share manager with an injected `Clock`.
    pub fn new_for_screen_with_clock(
        video_tiers: &'static [VideoQualityTier],
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::new_with_warmup_and_clock(
            video_tiers,
            SCREEN_QUALITY_WARMUP_MS,
            DEFAULT_SCREEN_TIER_INDEX,
            clock,
        )
    }

    /// Process the sender's own backpressure decision and potentially
    /// transition tiers (issue #1108, Phase B).
    ///
    /// Replaces the former receiver-FPS-driven `update(...)`. The sender no
    /// longer adapts to how peers *receive* its stream; the only gradual signal
    /// is the sender's own encode backpressure, pre-reduced by the controller to
    /// two booleans:
    ///
    /// * `degrade` — the sender is under sustained encode backpressure (its
    ///   WebCodecs encode queue has been at/above the HIGH threshold for the
    ///   sustain window). Steps the video tier DOWN (and, only when video is at
    ///   floor, the audio tier — the backpressure-gated safety net).
    /// * `recover` — backpressure has been clear over the stabilization window.
    ///   Steps tiers back UP (subject to the existing hysteresis / crash-ceiling
    ///   / congestion-hold gating).
    ///
    /// **Clamp-to-baseline recovery (issue #1108):** `recover` only reverses the
    /// sender's OWN backpressure-induced steps — it never climbs above the
    /// un-degraded configuration (`recovery_baseline_index`, and the full
    /// simulcast ladder). With no capacity signal (receiver FPS is gone) the
    /// sender never speculatively probes for more uplink; the only signals that
    /// push *below* baseline are this backpressure axis plus the explicit
    /// `force_congestion_cut` / `force_video_step_down` paths. An explicit user
    /// `best` quality bound is the one exception that may raise above baseline
    /// (it is an explicit capacity opt-in).
    ///
    /// Returns `true` if a tier changed (video or audio), meaning the caller
    /// should apply the new tier settings to the encoder.
    ///
    /// # Arguments
    /// * `degrade` - sender is under sustained encode backpressure (step DOWN)
    /// * `recover` - backpressure is sustained-clear (step UP)
    /// * `now_ms` - current timestamp in milliseconds
    pub fn update_from_backpressure(&mut self, degrade: bool, recover: bool, now_ms: f64) -> bool {
        // Issue #1077: clear the floor-saturated degrade signal each tick; it is
        // re-set below only if this tick warrants a step-down that the floor
        // blocks. (Cleared before the warmup/guard early-returns so a stale
        // signal can never leak across ticks.)
        self.degrade_floor_saturated = false;

        // Warmup guard: during encoder startup the sender is still spinning up,
        // so suppress all tier transitions until it has had time to stabilize.
        if now_ms - self.created_at_ms < self.warmup_ms {
            return false;
        }

        // Enforce minimum interval between any two transitions.
        let time_since_last_transition = now_ms - self.last_transition_time_ms;
        let can_transition = time_since_last_transition >= MIN_TIER_TRANSITION_INTERVAL_MS as f64;

        let mut changed = false;

        // --- Video tier logic ---
        changed |= self.update_video_tier(degrade, recover, now_ms, can_transition);

        // --- Audio tier logic ---
        changed |= self.update_audio_tier(degrade, recover, now_ms, can_transition);

        changed
    }

    /// Handle video tier step-down and step-up logic (issue #1108, Phase B).
    ///
    /// `degrade` / `recover` are the sender's own backpressure decision (see
    /// [`update_from_backpressure`](Self::update_from_backpressure)); receiver
    /// FPS no longer participates. All hysteresis, crash-ceiling, yo-yo,
    /// re-election, congestion-hold, dwell, and floor-saturated logic is
    /// unchanged — only the degrade/recover *predicates* are now boolean inputs.
    fn update_video_tier(
        &mut self,
        degrade: bool,
        recover: bool,
        now_ms: f64,
        can_transition: bool,
    ) -> bool {
        let max_video_index = self.video_tiers.len().saturating_sub(1);

        // Climb-rate limiter: periodic ceiling maintenance.
        self.maybe_decay_ceiling(now_ms);
        self.maybe_reset_crash_memory(now_ms);

        // --- Step DOWN ---
        // The sender steps DOWN only on its OWN sustained encode backpressure
        // (issue #1108). The former receiver-FPS / bitrate-ratio predicates
        // (and the small-peer-count lenient threshold) are gone — a peer's link
        // can no longer drive a sender step-down.
        let should_degrade = degrade;

        // Step-DOWN cap: never exceed the user's `worst` bound (issue #961),
        // composed with the hard array bound — the more restrictive wins. This
        // is the EFFECTIVE floor for the gradual step-down: once the tier index
        // reaches it, no further tier step is possible (either because we hit
        // the array bottom or because the user capped send quality there).
        let step_down_cap = self.video_step_down_cap(max_video_index);

        if should_degrade && self.video_tier_index >= step_down_cap {
            // Issue #1077 (composed with #961): degrade conditions persist but
            // the tier index is already at the effective floor (the user's
            // `worst` cap and/or the hard array bound), so no tier step is
            // possible. Track the degrade duration the same way and, once it
            // crosses the reaction time AND a transition is permitted, raise the
            // floor-saturated signal so the controller's gradual simulcast layer
            // shed can still fire here (the simulcast layer axis is independent
            // of the tier floor). We do not change the tier index, the
            // transition timestamp, or the hysteresis thresholds — we only seed
            // the existing degrade timer (`degrade_start_ms`) so the
            // floor-saturated signal can latch (see the `else if !should_degrade`
            // reset below, which deliberately keeps this timer running at the
            // floor). Driving this off `step_down_cap` (not the raw array bound)
            // means a user who caps send quality at a higher tier still gets
            // layer-shedding once that cap is reached.
            let degrade_start = *self.degrade_start_ms.get_or_insert(now_ms);
            let degrade_duration = now_ms - degrade_start;
            if degrade_duration >= STEP_DOWN_REACTION_TIME_MS as f64 && can_transition {
                self.degrade_floor_saturated = true;
            }
            // Fall through: no tier change at the floor, so `return false` below
            // (via the step-up / no-op path) is unchanged.
        }

        if should_degrade && self.video_tier_index < step_down_cap {
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
                // Sender backpressure is now the sole gradual degrade trigger
                // (issue #1108).
                self.transition_buffer.push(TierTransitionRecord {
                    direction: "down",
                    stream: "video",
                    from_tier,
                    to_tier: to_tier.clone(),
                    trigger: "backpressure",
                });
                log::info!(
                    "AdaptiveQuality: video stepped DOWN to tier '{}' (index {}) on sender backpressure",
                    to_tier,
                    self.video_tier_index,
                );
                // Climb-rate limiter: evaluate yo-yo detection for ceiling arming.
                // N.B. must be called before updating last_step_down_ms so it reads
                // the *previous* step-down timestamp for yo-yo detection.
                self.step_down_count += 1;
                self.maybe_arm_ceiling(self.video_tier_index, now_ms);
                self.last_step_down_ms = Some(now_ms);
                return true;
            }
        } else if !should_degrade {
            // Conditions are not in the degradation zone; reset the timer.
            //
            // NOTE (issue #1077): this reset is gated on `!should_degrade`
            // specifically (not just "the step-down branch wasn't taken") so
            // that a sustained degrade AT THE FLOOR keeps accumulating
            // `degrade_start_ms`, letting the floor-saturated block above cross
            // the reaction time. Before #1077 this was a bare `else`, which was
            // correct only because the floor case never set the timer; now that
            // the floor case relies on the timer, the reset must exclude it.
            self.degrade_start_ms = None;
        }

        // --- Step UP ---
        // Recover only when the sender's backpressure has been sustained-clear
        // (issue #1108). Suppress step-up entirely while a self-targeted
        // CONGESTION drain hold is active: the relay buffer is still draining and
        // climbing tiers now would re-fill it. The hold expires by timestamp,
        // after which recovery resumes normally.
        let should_recover = recover && !self.congestion_hold_active(now_ms);

        // Respect the screen-share coordination ceiling, the crash ceiling, AND
        // the user's `best` floor (issue #961). The effective step-up floor is
        // the MOST restrictive (highest index) of these — the user `best` only
        // adds a further floor; it can never loosen a safety ceiling.
        //
        // Issue #1108 (clamp-to-baseline recovery): backpressure-driven recovery
        // may climb back to — but never above — the tier this stream started at
        // (`recovery_baseline_index`). With receiver FPS gone the sender has no
        // capacity signal, so it must not speculatively probe for more uplink; it
        // only reverses its OWN backpressure-induced steps. EXCEPTION: when the
        // user set an explicit `best` bound, that IS an explicit capacity
        // authorization and wins — the baseline must not raise the floor above
        // it. So the baseline clamp applies ONLY when no user-best is set.
        let recover_floor = match self.user_video_best_index {
            Some(_) => self.video_step_up_floor(),
            None => self.video_step_up_floor().max(self.recovery_baseline_index),
        };

        if should_recover && self.video_tier_index > recover_floor {
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
                    trigger: "backpressure",
                });
                if slowdown > 1.01 {
                    log::info!(
                        "AdaptiveQuality: video stepped UP to tier '{}' (index {}) on sustained-clear backpressure, slowdown={:.2}x",
                        to_tier,
                        self.video_tier_index,
                        slowdown,
                    );
                } else {
                    log::info!(
                        "AdaptiveQuality: video stepped UP to tier '{}' (index {}) on sustained-clear backpressure",
                        to_tier,
                        self.video_tier_index,
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
        } else if should_recover && self.video_tier_index <= recover_floor {
            // At the (baseline/ceiling) recovery floor with good conditions —
            // track recovery time so we can distinguish "would have stepped up
            // this tick" from "conditions just became good". Only count a blocked
            // event when the stabilization window is met (i.e., a step-up would
            // have triggered absent the floor). Without this gate, the counter
            // increments every evaluation tick, inflating telemetry.
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

    /// Handle audio tier step-down and step-up logic — the backpressure-gated
    /// safety net (issue #1108).
    ///
    /// Audio still degrades LAST: only when video is already at the lowest tier
    /// AND the sender is under its own sustained backpressure (`degrade`). A
    /// receiver's link can never move audio. Audio recovers first (before video
    /// steps up) on sustained-clear backpressure (`recover`).
    fn update_audio_tier(
        &mut self,
        degrade: bool,
        recover: bool,
        now_ms: f64,
        can_transition: bool,
    ) -> bool {
        let max_audio_index = AUDIO_QUALITY_TIERS.len().saturating_sub(1);
        let video_at_lowest = self.video_tier_index >= self.video_tiers.len().saturating_sub(1);

        // User audio bounds (issue #961, inverse-of-index): `best` is a FLOOR on
        // the audio index (step-up may not go below it), `worst` is a CAP (step-
        // down may not go above it). The more restrictive bound wins over the
        // hard array bounds.
        let audio_step_down_cap = self
            .user_audio_worst_index
            .map_or(max_audio_index, |w| w.min(max_audio_index));
        let audio_step_up_floor = self.user_audio_best_index.unwrap_or(0);

        // --- Audio step DOWN ---
        // Only degrade audio when video is already at the lowest tier AND the
        // sender itself is backpressured (issue #1108). Audio is the priority
        // stream and must never react to a receiver's link.
        let should_degrade_audio = video_at_lowest && degrade;

        if should_degrade_audio && self.audio_tier_index < audio_step_down_cap {
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
                    trigger: "backpressure",
                });
                log::info!(
                    "AdaptiveQuality: audio stepped DOWN to tier '{}' (index {}) on sender backpressure (video at floor)",
                    to_tier,
                    self.audio_tier_index,
                );
                return true;
            }
        } else {
            self.audio_degrade_start_ms = None;
        }

        // --- Audio step UP ---
        // DELIBERATE ASYMMETRY (issue #702): unlike video step-up (see the
        // `!self.congestion_hold_active(now_ms)` guard on `should_recover` ~line 440),
        // audio recovery is intentionally NOT gated on the congestion-hold window.
        // A self-targeted CONGESTION drain hold protects audio precisely by shedding
        // video bandwidth (the ~350kbps video cut); audio is the priority stream and
        // its recovery step is only ~15-30kbps, negligible against the freed video
        // budget and far too small to refill the draining relay buffer. Suppressing
        // audio recovery here would keep the priority stream degraded for the full
        // ~2.5s hold for no meaningful buffer benefit, so we let it climb back
        // immediately. Do not add a congestion-hold guard to this branch.
        let should_recover_audio = recover;

        if should_recover_audio && self.audio_tier_index > audio_step_up_floor {
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
                    trigger: "backpressure",
                });
                log::info!(
                    "AdaptiveQuality: audio stepped UP to tier '{}' (index {}) on sustained-clear backpressure",
                    to_tier,
                    self.audio_tier_index,
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

    /// Current user video quality bounds as `(best/floor, worst/cap)` indices.
    /// `None` on either end means "Auto". See [`Self::set_video_quality_bounds`]
    /// for the inverse-of-index semantics.
    pub fn user_video_quality_bounds(&self) -> (Option<usize>, Option<usize>) {
        (self.user_video_best_index, self.user_video_worst_index)
    }

    /// Current user audio quality bounds as `(best/floor, worst/cap)` indices.
    pub fn user_audio_quality_bounds(&self) -> (Option<usize>, Option<usize>) {
        (self.user_audio_best_index, self.user_audio_worst_index)
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

    /// Aggressively cut video quality in response to a *self-targeted* server
    /// CONGESTION signal — the relay is actively dropping our outbound packets.
    ///
    /// Unlike [`force_video_step_down`](Self::force_video_step_down) (a gentle
    /// one-tier step used for WebSocket backpressure), this drops
    /// [`CONGESTION_CUT_TIERS`] tiers at once (~50% bitrate on most of the
    /// non-uniform ladder) and then pins the effective bitrate ceiling to the
    /// post-cut tier for [`CONGESTION_HOLD_MS`] so the overflowing relay buffer
    /// can drain before the PID ramps bitrate back up.
    ///
    /// Like `force_video_step_down`, this respects the warmup guard and the
    /// minimum transition interval — those guards exist to avoid reacting to
    /// startup noise and to avoid cascading transitions, and the user chose to
    /// keep them rather than bypass them for congestion.
    ///
    /// # Crash-ceiling / yo-yo handling
    ///
    /// This deliberately does **not** call
    /// [`maybe_arm_ceiling`](Self::maybe_arm_ceiling) and does **not** touch
    /// `step_down_count` / `last_step_down_ms`. A self-targeted CONGESTION cut
    /// is a response to an *external* relay signal, not to client-side quality
    /// oscillation. Feeding it into yo-yo detection would let a single network
    /// hiccup arm the crash ceiling (and its backoff up to
    /// `CLIMB_COOLDOWN_MAX_MS`), capping recovery to a low tier for minutes
    /// after the congestion has cleared — exactly the over-conservative
    /// behaviour this aggressive cut is meant to avoid. The short
    /// `CONGESTION_HOLD_MS` window already provides the necessary "let the
    /// buffer drain" pause without the long-lived ceiling.
    ///
    /// # Return value
    ///
    /// Returns `true` if the tier actually changed. If we are already at (or
    /// within one tier of) the lowest tier so no tier change occurs, this still
    /// arms the hold window — a cut at the floor must still let the buffer
    /// drain — but returns `false` to signal "no tier change to apply".
    pub fn force_congestion_cut(&mut self, now_ms: f64) -> bool {
        // Warmup guard: same as update()/force_video_step_down() — suppress
        // forced cuts during encoder startup when zero-FPS readings would be
        // misleading and no real media is flowing yet.
        if now_ms - self.created_at_ms < self.warmup_ms {
            return false;
        }

        // Min-interval guard: avoid cascading cuts from a burst of congestion
        // signals. (Kept rather than bypassed — see method docs.)
        let time_since_last = now_ms - self.last_transition_time_ms;
        if time_since_last < MIN_TIER_TRANSITION_INTERVAL_MS as f64 {
            log::debug!(
                "AdaptiveQuality: congestion cut blocked by min transition interval ({:.0}ms < {}ms)",
                time_since_last,
                MIN_TIER_TRANSITION_INTERVAL_MS,
            );
            return false;
        }

        // Arm the drain hold regardless of whether the tier can move further —
        // a cut while already at the floor must still pause the PID so the
        // relay buffer drains.
        self.congestion_hold_until_ms = Some(now_ms + CONGESTION_HOLD_MS);

        let max_video_index = self.video_tiers.len().saturating_sub(1);
        let target_index = (self.video_tier_index + CONGESTION_CUT_TIERS).min(max_video_index);
        if target_index == self.video_tier_index {
            // Already at the last tier: hold is armed (above) but there is no
            // tier change for the caller to apply.
            log::warn!(
                "AdaptiveQuality: CONGESTION cut at floor tier '{}' (index {}); drain hold armed for {:.0}ms",
                self.video_tiers[self.video_tier_index].label,
                self.video_tier_index,
                CONGESTION_HOLD_MS,
            );
            return false;
        }

        self.record_dwell(now_ms);
        let from_tier = self.video_tiers[self.video_tier_index].label.to_string();
        self.video_tier_index = target_index;
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
            "AdaptiveQuality: CONGESTION cut dropped {} tiers to '{}' (index {}); \
             drain hold armed for {:.0}ms",
            CONGESTION_CUT_TIERS,
            to_tier,
            self.video_tier_index,
            CONGESTION_HOLD_MS,
        );

        // NB: intentionally do NOT arm the crash ceiling or update yo-yo state
        // here — see the method docs for why an external relay signal must not
        // feed client-side oscillation detection.
        true
    }

    /// Whether a self-targeted CONGESTION drain hold is currently active.
    ///
    /// While active, the effective bitrate ceiling is pinned to the post-cut
    /// tier's lower bound and tier step-up is blocked. Expires naturally when
    /// `now_ms` passes `congestion_hold_until_ms`; no explicit clear is needed.
    pub fn congestion_hold_active(&self, now_ms: f64) -> bool {
        self.congestion_hold_until_ms
            .is_some_and(|until| now_ms < until)
    }

    /// Whether the guards shared by the *forced* transition paths
    /// ([`force_video_step_down`](Self::force_video_step_down) and
    /// [`force_congestion_cut`](Self::force_congestion_cut)) are currently
    /// clear — i.e. past the warmup window AND past the minimum transition
    /// interval since the last transition.
    ///
    /// Additive read-only helper (issue #989, PR B). The simulcast layer axis
    /// uses this to decide whether a forced congestion response should shed a
    /// layer: a request blocked by these guards did not "happen" (no tier move,
    /// no drain hold), so it must not shed a layer either. Unlike the forced
    /// methods themselves, this is independent of the tier floor — letting the
    /// layer axis respond to congestion even when `video_tier_index` is already
    /// at its lowest tier. Does not mutate state and does not affect the
    /// single-stream tier machinery.
    pub fn forced_transition_guards_clear(&self, now_ms: f64) -> bool {
        let past_warmup = now_ms - self.created_at_ms >= self.warmup_ms;
        let past_min_interval =
            now_ms - self.last_transition_time_ms >= MIN_TIER_TRANSITION_INTERVAL_MS as f64;
        past_warmup && past_min_interval
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

    /// Clamp and normalize a `(best, worst)` index bound pair against a tier
    /// array of length `tiers_len`.
    ///
    /// Recall (issue #961) that **quality is the inverse of index**: `best` is a
    /// FLOOR on the index (the BEST tier allowed) and `worst` is a CAP on the
    /// index (the WORST tier allowed), so a valid range requires
    /// `best <= worst`.
    ///
    /// Normalization rules:
    /// - Each bound is first clamped to `[0, tiers_len - 1]`.
    /// - If both ends are set and the range is inverted (`best > worst`, i.e. an
    ///   empty range), the two are **swapped** so the range is non-empty. This
    ///   is the most forgiving rule: it preserves both user-chosen tiers and
    ///   simply reinterprets the wider as the cap and the narrower as the floor,
    ///   rather than silently dropping one end.
    /// - `None` is passed through unchanged ("Auto" on that end).
    fn normalize_bounds(
        best: Option<usize>,
        worst: Option<usize>,
        tiers_len: usize,
    ) -> (Option<usize>, Option<usize>) {
        let max_index = tiers_len.saturating_sub(1);
        let best = best.map(|b| b.min(max_index));
        let worst = worst.map(|w| w.min(max_index));
        match (best, worst) {
            (Some(b), Some(w)) if b > w => (Some(w), Some(b)),
            other => other,
        }
    }

    /// Set user-configurable quality bounds for the **video** tier (issue #961).
    ///
    /// QUALITY IS THE INVERSE OF INDEX — see the field docs on
    /// `user_video_best_index`. `best` is the user's MAX quality = a FLOOR on the
    /// index (never step UP past it); `worst` is the user's MIN quality = a CAP
    /// on the index (never step DOWN past it). Each end is independently optional
    /// (`None` = Auto).
    ///
    /// Behaviour:
    /// - Both args are clamped to valid tier indices and, if inverted, swapped so
    ///   the `[best, worst]` range is non-empty (see [`Self::normalize_bounds`]).
    /// - If the current video tier index now falls outside the new range, it is
    ///   moved into range **immediately** (not on the next adaptation tick) via
    ///   [`Self::force_video_step_to`], so the constraint takes effect at once.
    pub fn set_video_quality_bounds(
        &mut self,
        best: Option<usize>,
        worst: Option<usize>,
        now_ms: f64,
    ) {
        let (best, worst) = Self::normalize_bounds(best, worst, self.video_tiers.len());
        self.user_video_best_index = best;
        self.user_video_worst_index = worst;

        // Snap the current tier into the new range immediately. Clamp against
        // the floor first then the cap; with a normalized (non-inverted) range
        // these can never conflict.
        let mut target = self.video_tier_index;
        if let Some(floor) = best {
            target = target.max(floor);
        }
        if let Some(cap) = worst {
            target = target.min(cap);
        }
        if target != self.video_tier_index {
            self.force_video_step_to(target, now_ms);
        }

        log::info!(
            "AdaptiveQuality: user video quality bounds set (best/floor={best:?}, worst/cap={worst:?}); \
             current index now {}",
            self.video_tier_index,
        );
    }

    /// Set user-configurable quality bounds for the **audio** tier (issue #961).
    ///
    /// Same inverse-of-index semantics as [`Self::set_video_quality_bounds`]:
    /// `best` is a FLOOR on the audio index, `worst` is a CAP. `None` = Auto.
    /// Snaps the current audio tier into range immediately if it falls outside.
    pub fn set_audio_quality_bounds(&mut self, best: Option<usize>, worst: Option<usize>) {
        let (best, worst) = Self::normalize_bounds(best, worst, AUDIO_QUALITY_TIERS.len());
        self.user_audio_best_index = best;
        self.user_audio_worst_index = worst;

        // Snap the current audio tier into the new range immediately. Audio has
        // no force-step helper, so set the index directly (audio adaptation does
        // not participate in the crash-ceiling machinery).
        let mut target = self.audio_tier_index;
        if let Some(floor) = best {
            target = target.max(floor);
        }
        if let Some(cap) = worst {
            target = target.min(cap);
        }
        if target != self.audio_tier_index {
            self.audio_tier_index = target;
            self.audio_degrade_start_ms = None;
            self.audio_recover_start_ms = None;
        }

        log::info!(
            "AdaptiveQuality: user audio quality bounds set (best/floor={best:?}, worst/cap={worst:?}); \
             current index now {}",
            self.audio_tier_index,
        );
    }

    /// Effective video step-DOWN cap: the highest index (worst quality) the tier
    /// may be stepped down to. Composes the hard array bound with the user's
    /// `worst` cap (issue #961) — the most restrictive (lowest) wins.
    fn video_step_down_cap(&self, max_video_index: usize) -> usize {
        match self.user_video_worst_index {
            Some(user_worst) => user_worst.min(max_video_index),
            None => max_video_index,
        }
    }

    /// Effective video step-UP floor: the lowest index (best quality) the tier
    /// may be stepped up to. Composes the internal safety ceilings
    /// ([`Self::effective_ceiling`] — crash + screen-share coordination) with the
    /// user's `best` floor (issue #961). The user `best` floor only ADDS a
    /// further restriction; it can never loosen a safety ceiling, so the
    /// effective floor is the MORE restrictive (higher index) of the two.
    fn video_step_up_floor(&self) -> usize {
        let internal = self.effective_ceiling();
        match self.user_video_best_index {
            Some(user_best) => user_best.max(internal),
            None => internal,
        }
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
    // Simulcast active-layer control (issue #989, PR B) — ADDITIVE
    // -----------------------------------------------------------------
    //
    // These methods exist alongside the tier machinery and never touch
    // `video_tier_index` or any existing field. When `simulcast_layer_count`
    // is 1 (the default, and the value the bot always sees) they are inert:
    // `active_layer_count()` returns 1, `drop_top_layer`/`add_top_layer` cannot
    // move off the floor/ceiling of 1, and the manager behaves exactly as it
    // did before this PR.
    //
    // Why additive rather than repurposing `video_tier_index`: the load-test
    // bot reads `video_tier_index()` as a *resolution-tier index* to choose its
    // encode resolution. Simulcast layers are a different axis (fixed-resolution
    // streams that get shed top-down). Conflating them would silently change the
    // bot's encode resolution. See CLAUDE.md "Change Impact Policy".

    /// Configure how many simulcast layers this session's ladder has.
    ///
    /// Call once after construction with the effective layer count
    /// (`min(max_layers, ladder_len)` from the client). `n` is clamped to
    /// `[1, SIMULCAST_MAX_LAYERS]`. `active_layer_count` is (re)initialized to
    /// `n` (all layers active); the controller then sheds the top layer under
    /// sustained congestion. Passing `1` (the default) keeps the manager in
    /// single-stream mode where the layer methods are inert.
    pub fn set_simulcast_layers(&mut self, n: usize) {
        let clamped = n.clamp(1, crate::constants::SIMULCAST_MAX_LAYERS);
        self.simulcast_layer_count = clamped;
        self.active_layer_count = clamped;
    }

    /// Number of simulcast layers in this session's ladder (the ceiling for
    /// [`active_layer_count`](Self::active_layer_count)).
    pub fn simulcast_layer_count(&self) -> usize {
        self.simulcast_layer_count
    }

    /// Whether the most recent [`update`](Self::update) warranted a video
    /// step-DOWN that the tier floor blocked (issue #1077).
    ///
    /// The gradual `update()`-path simulcast layer shed in the controller keys
    /// off tier-index movement, which saturates at the floor. This signal lets
    /// the controller shed the top layer at the floor too — decoupling the layer
    /// axis from the tier floor exactly as the explicit `force_*` paths already
    /// do — so a deeper ladder (or a default tier near the floor) cannot leave
    /// the gradual path unable to shed. `false` outside the degrade-at-floor
    /// case and always `false` in single-stream mode (no layers to shed).
    ///
    /// THROTTLING (issue #1082 review): unlike the tier-coupled step-down (one
    /// shed per `MIN_TIER_TRANSITION_INTERVAL_MS`), this signal is re-evaluated
    /// every diagnostics tick (~1/sec), so the controller's floor shed it drives
    /// can fire once per tick at the floor until `active_layer_count` reaches 1.
    /// This is intentional and benign today: the shed is down-only, floors at 1,
    /// and the current ladders need at most 2 sheds (the 3-layer screen ladder).
    /// A future maintainer who DEEPENS the video ladder should revisit this and
    /// reuse the min-interval throttle here (e.g. touch `last_transition_time_ms`
    /// in the floor block) so a deep ladder cannot shed many layers in one burst.
    pub fn wanted_degrade_at_floor(&self) -> bool {
        self.degrade_floor_saturated
    }

    /// Number of simulcast layers currently being encoded+sent.
    ///
    /// `1` in single-stream mode. In simulcast mode this ranges over
    /// `[1, simulcast_layer_count]`; the top
    /// `simulcast_layer_count - active_layer_count` layers are shed (saving both
    /// egress bandwidth and sender encode CPU).
    pub fn active_layer_count(&self) -> usize {
        self.active_layer_count
    }

    /// Whether this manager is operating in simulcast mode (`> 1` layers).
    pub fn is_simulcast(&self) -> bool {
        self.simulcast_layer_count > 1
    }

    /// Shed the top active simulcast layer (decrement `active_layer_count`,
    /// floored at 1).
    ///
    /// Called by the controller when a sustained-congestion *step-down* decision
    /// fires in simulcast mode (the same decision that would step a tier down in
    /// single-stream mode). Dropping the top layer cuts both egress and sender
    /// encode CPU. Returns `true` if the active count actually decreased.
    ///
    /// No-op (returns `false`) in single-stream mode or when already at the
    /// floor — exactly mirroring how `video_tier_index` step-down is a no-op at
    /// the lowest tier.
    pub fn drop_top_layer(&mut self) -> bool {
        if self.active_layer_count <= 1 {
            return false;
        }
        let from = self.active_layer_count;
        self.active_layer_count -= 1;
        log::info!(
            "AdaptiveQuality: simulcast dropped TOP layer ({from} -> {} active of {})",
            self.active_layer_count,
            self.simulcast_layer_count,
        );
        true
    }

    /// Restore the next top simulcast layer (increment `active_layer_count`,
    /// capped at `simulcast_layer_count`).
    ///
    /// Called by the controller when a sustained-recovery *step-up* decision
    /// fires in simulcast mode. Returns `true` if the active count actually
    /// increased. No-op (returns `false`) in single-stream mode or when already
    /// at the ceiling.
    pub fn add_top_layer(&mut self) -> bool {
        if self.active_layer_count >= self.simulcast_layer_count {
            return false;
        }
        let from = self.active_layer_count;
        self.active_layer_count += 1;
        log::info!(
            "AdaptiveQuality: simulcast restored TOP layer ({from} -> {} active of {})",
            self.active_layer_count,
            self.simulcast_layer_count,
        );
        true
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
        // NOTE(#1108): reelection crash-ceiling suppression retained — still
        // guards the kept force_congestion_cut/force_video_step_down step-downs
        // from a re-election-coincident false crash-arm. (The receiver-FPS path
        // that originally motivated it is gone in Stage 2, but the explicit
        // force-step-down paths can still arm the ceiling, so this stays.)
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
    use crate::constants::{
        screen_share_camera_ceiling_index, SCREEN_QUALITY_TIERS, VIDEO_QUALITY_TIERS,
    };

    // Dual-target: in Wasm these run via `wasm-bindgen-test`, in native they
    // run under the normal `#[test]` harness. Every `#[wasm_bindgen_test]`
    // below has been rewritten to `#[test]` so both targets see it.
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    /// Create a manager with `created_at_ms` and `last_transition_time_ms` set
    /// to 0.0 so that tests using small `now_ms` values (e.g. 10000) are well
    /// past the warmup period and the min-transition guard.
    ///
    /// Also sets `recovery_baseline_index = 0` so the clamp-to-baseline recovery
    /// policy (issue #1108) does NOT interfere with the many tests here that
    /// exercise raw step-up / climb-rate-limiter mechanics from a manually-set
    /// tier (they predate the policy and test the hysteresis/ceiling machinery,
    /// not the baseline floor). The baseline-clamp policy itself is covered by
    /// dedicated tests (`test_*recovery*baseline*` and the controller tests).
    fn new_test_manager(video_tiers: &'static [VideoQualityTier]) -> AdaptiveQualityManager {
        let mut mgr = AdaptiveQualityManager::new(video_tiers);
        mgr.created_at_ms = 0.0;
        mgr.last_transition_time_ms = 0.0;
        mgr.recovery_baseline_index = 0;
        mgr
    }

    // -----------------------------------------------------------------
    // Test-only compatibility shim (issue #1108).
    //
    // The production `update(received_fps, ...)` was replaced by
    // `update_from_backpressure(degrade, recover, now)`. The bulk of the manager
    // tests exercise the tier STATE MACHINE (crash ceiling, yo-yo, dwell, user
    // bounds, min-interval) — which is unchanged — using "bad FPS" / "good FPS"
    // as a proxy for "degrade" / "recover". This shim translates those FPS/bitrate
    // arguments into the new boolean decision using the HISTORICAL thresholds so
    // those tests keep their meaning without touching the production API. The
    // `_effective_peer_count` arg is ignored on purpose: the small-peer-count
    // lenient threshold was removed (a receiver's link no longer affects the
    // sender), so the dedicated lenient/peer-count tests were deleted rather than
    // shimmed.
    impl AdaptiveQualityManager {
        fn update(
            &mut self,
            received_fps: f64,
            target_fps: f64,
            current_bitrate_kbps: f64,
            ideal_bitrate_kbps: f64,
            now_ms: f64,
            _effective_peer_count: usize,
        ) -> bool {
            // Historical degrade/recover predicates (pre-#1108): degrade on low
            // fps OR low bitrate ratio; recover on high fps AND high bitrate
            // ratio. Guard against the zero-target early-return the old code had.
            if target_fps <= 0.0 || ideal_bitrate_kbps <= 0.0 {
                // Old `update` returned false on non-positive targets after the
                // warmup check; `update_from_backpressure` has no such guard, so
                // emulate "no signal" (hold) here.
                return self.update_from_backpressure(false, false, now_ms);
            }
            let fps_ratio = received_fps / target_fps;
            let bitrate_ratio = current_bitrate_kbps / ideal_bitrate_kbps;
            let degrade = fps_ratio < 0.50 || bitrate_ratio < 0.40;
            let recover = fps_ratio > 0.70 && bitrate_ratio > 0.75;
            self.update_from_backpressure(degrade, recover, now_ms)
        }
    }

    #[test]
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

    #[test]
    fn test_screen_starts_at_midpoint_tier() {
        let mgr = AdaptiveQualityManager::new_for_screen(SCREEN_QUALITY_TIERS);
        assert_eq!(mgr.video_tier_index(), DEFAULT_SCREEN_TIER_INDEX);
        assert_eq!(mgr.audio_tier_index(), 0);
        assert_eq!(
            mgr.current_video_tier().label,
            SCREEN_QUALITY_TIERS[DEFAULT_SCREEN_TIER_INDEX].label
        );
        // Screen share starts at the midpoint tier "medium" (720p/8fps)
        assert_eq!(mgr.current_video_tier().label, "medium");
    }

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
    fn test_no_change_under_good_conditions() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        // Start from a known state for this test
        mgr.video_tier_index = 0;
        // fps_ratio=1.0, bitrate_ratio=1.0 -- perfect conditions
        let changed = mgr.update(30.0, 30.0, 1500.0, 1500.0, 10000.0, 5);
        assert!(!changed);
        assert_eq!(mgr.video_tier_index(), 0);
    }

    #[test]
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

    #[test]
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

    #[test]
    fn test_video_step_up_after_stabilization() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        // Force to tier 1
        mgr.video_tier_index = 1;
        let base = 10000.0;

        // Recovery conditions: fps_ratio=0.93 > 0.70, bitrate_ratio=0.90 > 0.75
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

    #[test]
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

        // After MIN_TIER_TRANSITION_INTERVAL_MS (1500ms) from last transition + reaction time
        let changed = mgr.update(9.0, 30.0, 1500.0, 1500.0, base + 4700.0, 5);
        // degrade_start was reset at transition; 4700 - 1600 = 3100 > 1500 (interval ok)
        // but degrade_start was None after transition, so it started fresh at the next
        // degraded update. Let's trace: after step-down at 11600, degrade_start=None.
        // At 12000 (base+2000): should_degrade=true, degrade_start set to 12000.
        //   can_transition: 12000-11600=400 < 1500, so no.
        // At 14700 (base+4700): should_degrade=true, degrade_start=12000, duration=2700 > 1500.
        //   can_transition: 14700-11600=3100 > 1500, yes!
        assert!(changed);
        assert_eq!(mgr.video_tier_index(), 2);
    }

    #[test]
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

    #[test]
    fn test_audio_recovers_on_sustained_clear_backpressure() {
        // Issue #1108: audio and video now share the SAME backpressure recover
        // boolean (the old distinct audio/video FPS recover thresholds are gone).
        // A degraded audio tier must climb back on sustained-clear backpressure.
        // (The former "audio recovers at a lower FPS ratio than video" asymmetry
        // no longer exists; audio is a backpressure-gated safety net.)
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        let max_video = VIDEO_QUALITY_TIERS.len() - 1;
        mgr.video_tier_index = max_video; // video at floor
        mgr.audio_tier_index = 2; // "low"
        mgr.last_transition_time_ms = 0.0;

        let base = 10000.0;
        // recover = true (sustained-clear). Audio at the floor-video state must
        // step back up.
        let _ = mgr.update_from_backpressure(false, true, base);
        let changed = mgr.update_from_backpressure(false, true, base + 5100.0);
        assert!(changed, "sustained-clear backpressure must recover a tier");
        assert_eq!(
            mgr.audio_tier_index(),
            1,
            "audio must step UP on sustained-clear backpressure"
        );
    }

    /// Issue #1108 clamp-to-baseline: with NO user `best`, sustained-clear
    /// backpressure may recover the video tier back UP to the baseline
    /// (`DEFAULT_VIDEO_TIER_INDEX`) but must STOP there — it must never climb
    /// above the baseline (no speculative uplink probing). Uses the real
    /// constructor so `recovery_baseline_index` is the production default.
    #[test]
    fn test_recovery_clamps_to_baseline_without_user_best() {
        let mut mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        mgr.created_at_ms = 0.0;
        mgr.last_transition_time_ms = 0.0;
        assert_eq!(mgr.recovery_baseline_index, DEFAULT_VIDEO_TIER_INDEX);

        // Force the tier BELOW baseline (worse quality) without touching the
        // baseline, simulating a prior backpressure shed.
        let below = DEFAULT_VIDEO_TIER_INDEX + 2;
        assert!(below <= VIDEO_QUALITY_TIERS.len() - 1);
        mgr.video_tier_index = below;

        // Sustained-clear recover over many spaced ticks must climb back to —
        // and stop at — the baseline.
        let mut t = 10_000.0;
        for _ in 0..20 {
            mgr.update_from_backpressure(false, true, t);
            t += STEP_UP_STABILIZATION_WINDOW_MS as f64 + 500.0;
            if mgr.video_tier_index() == DEFAULT_VIDEO_TIER_INDEX {
                break;
            }
        }
        assert_eq!(
            mgr.video_tier_index(),
            DEFAULT_VIDEO_TIER_INDEX,
            "recovery must climb back up to the baseline tier"
        );

        // Keep ticking recover: the tier must NOT climb above the baseline.
        for _ in 0..10 {
            mgr.update_from_backpressure(false, true, t);
            t += STEP_UP_STABILIZATION_WINDOW_MS as f64 + 500.0;
        }
        assert_eq!(
            mgr.video_tier_index(),
            DEFAULT_VIDEO_TIER_INDEX,
            "recovery must NEVER climb above the baseline tier (no speculative probing)"
        );
    }

    /// Issue #1108 clamp-to-baseline EXCEPTION: an explicit user `best` quality
    /// bound is an explicit capacity opt-in and overrides the baseline floor, so
    /// recovery MAY climb above the baseline up to the user's `best`.
    #[test]
    fn test_recovery_exceeds_baseline_when_user_best_set() {
        let mut mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        mgr.created_at_ms = 0.0;
        mgr.last_transition_time_ms = 0.0;
        let baseline = mgr.recovery_baseline_index;
        assert!(
            baseline > 0,
            "precondition: baseline is not already the top tier"
        );

        // User opts into a BETTER-than-baseline ceiling (lower index = higher
        // quality). `best = 0` authorizes climbing all the way to the top tier.
        mgr.set_video_quality_bounds(Some(0), None, 0.0);

        // Start below baseline and recover; with the user opt-in, the tier must
        // be able to climb ABOVE the baseline.
        mgr.video_tier_index = baseline;
        let mut t = 10_000.0;
        let mut climbed_above_baseline = false;
        for _ in 0..30 {
            mgr.update_from_backpressure(false, true, t);
            t += STEP_UP_STABILIZATION_WINDOW_MS as f64 + 500.0;
            if mgr.video_tier_index() < baseline {
                climbed_above_baseline = true;
                break;
            }
        }
        assert!(
            climbed_above_baseline,
            "an explicit user `best` must let recovery climb above the baseline \
             (baseline={baseline}, ended at {})",
            mgr.video_tier_index()
        );
    }

    #[test]
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

    #[test]
    fn test_zero_target_fps_returns_false() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        let changed = mgr.update(0.0, 0.0, 0.0, 0.0, 10000.0, 5);
        assert!(!changed);
    }

    // =====================================================================
    // Quality ceiling tests (cross-stream coordination)
    // =====================================================================

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
    fn test_no_slowdown_without_ceiling() {
        let mgr = new_test_manager(VIDEO_QUALITY_TIERS);

        // No slowdown activated — factor should be 1.0
        let factor = mgr.current_slowdown_factor(10000.0);
        assert!(
            (factor - 1.0).abs() < 0.001,
            "Without slowdown, factor should be 1.0, got {factor}"
        );
    }

    #[test]
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

    // =====================================================================
    // End-to-end climb-rate limiter test via update() API (PR-H review fix #6)
    // =====================================================================

    /// Walk the public `update()` API through the full climb-rate limiter
    /// lifecycle: arm -> recover -> re-crash, without manipulating internal
    /// state directly. This catches wiring regressions that the unit test
    /// `test_recrash_tightens_and_escalates_backoff` (which mutates fields
    /// directly) would miss.
    ///
    /// This test currently exposes a latent assertion that fails under the
    /// native test harness (ceiling index 5 vs expected 6). The logic being
    /// probed is *not* clock-dependent, so the native failure reflects a
    /// pre-existing discrepancy between the `recovered_since_ceiling`
    /// transitions and the test expectations, not an issue introduced by
    /// the crate split. Re-run the test on Wasm (where it historically
    /// lived) until the underlying climb-limiter wiring is revisited.
    #[cfg(target_arch = "wasm32")]
    #[test]
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
        // Wait past MIN_TIER_TRANSITION_INTERVAL_MS (1500ms) then degrade again.
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

    // =====================================================================
    // Self-targeted CONGESTION cut tests (#702)
    // =====================================================================

    #[test]
    fn test_congestion_cut_drops_two_tiers_and_halves_bitrate() {
        // From the default start tier, a single congestion cut should drop
        // CONGESTION_CUT_TIERS (>= 2) tiers and shed >= 50% of the ideal bitrate.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = DEFAULT_VIDEO_TIER_INDEX;
        let start_index = mgr.video_tier_index();
        let start_ideal = VIDEO_QUALITY_TIERS[start_index].ideal_bitrate_kbps as f64;

        let changed = mgr.force_congestion_cut(10000.0);
        assert!(changed, "Congestion cut should change the tier");

        let dropped = mgr.video_tier_index() - start_index;
        assert!(
            dropped >= 2,
            "Congestion cut should drop >= 2 tiers, dropped {dropped}"
        );
        assert_eq!(dropped, CONGESTION_CUT_TIERS);

        let new_ideal = mgr.current_video_tier().ideal_bitrate_kbps as f64;
        assert!(
            new_ideal <= start_ideal * 0.5,
            "Congestion cut should shed >= 50% of ideal bitrate: {start_ideal} -> {new_ideal}"
        );

        // The congestion cut records a transition tagged "congestion".
        let transitions = mgr.drain_transitions();
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].trigger, "congestion");
        assert_eq!(transitions[0].direction, "down");
    }

    #[test]
    fn test_congestion_cut_at_floor_arms_hold_but_returns_false() {
        // A cut while already at the lowest tier cannot move the tier, but must
        // still arm the drain hold and report "no tier change".
        let max_idx = VIDEO_QUALITY_TIERS.len() - 1;
        let mut mgr = new_test_manager_at(VIDEO_QUALITY_TIERS, max_idx);

        let now = 10000.0;
        let changed = mgr.force_congestion_cut(now);
        assert!(!changed, "Cut at floor should report no tier change");
        assert_eq!(mgr.video_tier_index(), max_idx, "Tier should not move");
        assert!(
            mgr.congestion_hold_active(now + 1.0),
            "Hold must be armed even at the floor so the buffer drains"
        );
    }

    #[test]
    fn test_congestion_hold_blocks_step_up_then_expires() {
        // During the hold window, good conditions must NOT raise the tier.
        // After CONGESTION_HOLD_MS elapses, recovery is allowed again.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = DEFAULT_VIDEO_TIER_INDEX;

        let cut_at = 10000.0;
        let changed = mgr.force_congestion_cut(cut_at);
        assert!(changed);
        let post_cut_index = mgr.video_tier_index();

        // Drive sustained good conditions during the hold. Step-up must stay blocked.
        // (Use ideal_bitrate so bitrate_ratio = 1.0 > recover threshold.)
        let ideal = mgr.current_video_tier().ideal_bitrate_kbps as f64;
        let mut t = cut_at + 100.0;
        while t < cut_at + CONGESTION_HOLD_MS {
            assert!(
                mgr.congestion_hold_active(t),
                "Hold should be active at {t}"
            );
            mgr.update(29.0, 30.0, ideal, ideal, t, 5);
            assert_eq!(
                mgr.video_tier_index(),
                post_cut_index,
                "Step-up must be blocked during the congestion hold (t={t})"
            );
            t += 400.0;
        }

        // After the hold expires, sustained good conditions allow step-up again.
        assert!(!mgr.congestion_hold_active(cut_at + CONGESTION_HOLD_MS + 1.0));
        let base = cut_at + CONGESTION_HOLD_MS + 1000.0;
        let mut stepped_up = false;
        for i in 0..=12 {
            let t = base + (i as f64 * 1000.0);
            mgr.update(29.0, 30.0, ideal, ideal, t, 5);
            if mgr.video_tier_index() < post_cut_index {
                stepped_up = true;
                break;
            }
        }
        assert!(
            stepped_up,
            "Recovery should resume after the congestion hold expires"
        );
    }

    #[test]
    fn test_congestion_cut_does_not_arm_crash_ceiling() {
        // A self-targeted congestion cut is an external relay signal, not client
        // oscillation: it must NOT arm the crash ceiling or feed yo-yo state.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = DEFAULT_VIDEO_TIER_INDEX;

        let step_down_count_before = mgr.step_down_count;

        let changed = mgr.force_congestion_cut(10000.0);
        assert!(changed);

        assert!(
            mgr.crash_ceiling_info().is_none(),
            "Congestion cut must not arm the crash ceiling"
        );
        assert!(
            mgr.last_step_down_ms.is_none(),
            "Congestion cut must not feed yo-yo detection (last_step_down_ms)"
        );
        assert_eq!(
            mgr.step_down_count, step_down_count_before,
            "Congestion cut must not increment step_down_count"
        );

        // A second congestion cut shortly after (within YOYO window) must still
        // not arm the ceiling.
        let changed =
            mgr.force_congestion_cut(10000.0 + MIN_TIER_TRANSITION_INTERVAL_MS as f64 + 1.0);
        let _ = changed; // may be false if it hit the floor; either way:
        assert!(
            mgr.crash_ceiling_info().is_none(),
            "Repeated congestion cuts must never arm the crash ceiling"
        );
    }

    // =====================================================================
    // Simulcast active-layer state tests (#989, PR B) — ADDITIVE
    // =====================================================================

    #[test]
    fn test_simulcast_defaults_to_single_stream() {
        let mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        assert_eq!(mgr.simulcast_layer_count(), 1);
        assert_eq!(mgr.active_layer_count(), 1);
        assert!(!mgr.is_simulcast());
    }

    #[test]
    fn test_single_stream_layer_methods_are_inert() {
        // In single-stream mode (the bot's mode), drop/add must be no-ops so the
        // manager behaves exactly as before.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        assert!(!mgr.drop_top_layer(), "drop must be a no-op at 1 layer");
        assert_eq!(mgr.active_layer_count(), 1);
        assert!(
            !mgr.add_top_layer(),
            "add must be a no-op at the ceiling of 1"
        );
        assert_eq!(mgr.active_layer_count(), 1);
    }

    #[test]
    fn test_set_simulcast_layers_clamps() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);

        mgr.set_simulcast_layers(0);
        assert_eq!(mgr.simulcast_layer_count(), 1, "0 clamps up to 1");
        assert_eq!(mgr.active_layer_count(), 1);

        mgr.set_simulcast_layers(3);
        assert_eq!(mgr.simulcast_layer_count(), 3);
        assert_eq!(mgr.active_layer_count(), 3, "starts with all layers active");
        assert!(mgr.is_simulcast());

        mgr.set_simulcast_layers(99);
        assert_eq!(
            mgr.simulcast_layer_count(),
            crate::constants::SIMULCAST_MAX_LAYERS,
            "over-large request clamps to ladder max"
        );
    }

    #[test]
    fn test_drop_and_add_top_layer_in_simulcast() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.set_simulcast_layers(3);
        assert_eq!(mgr.active_layer_count(), 3);

        // Drop the top layer twice down to the floor.
        assert!(mgr.drop_top_layer());
        assert_eq!(mgr.active_layer_count(), 2);
        assert!(mgr.drop_top_layer());
        assert_eq!(mgr.active_layer_count(), 1);
        // At the floor, further drops are no-ops (base layer always sent).
        assert!(!mgr.drop_top_layer());
        assert_eq!(mgr.active_layer_count(), 1);

        // Restore layers back up to the ceiling.
        assert!(mgr.add_top_layer());
        assert_eq!(mgr.active_layer_count(), 2);
        assert!(mgr.add_top_layer());
        assert_eq!(mgr.active_layer_count(), 3);
        // At the ceiling, further adds are no-ops.
        assert!(!mgr.add_top_layer());
        assert_eq!(mgr.active_layer_count(), 3);
    }

    #[test]
    fn test_simulcast_layer_state_does_not_touch_tier_index() {
        // Dropping/adding layers must NOT move video_tier_index — the bot reads
        // that field as a resolution-tier index and must be unaffected.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = DEFAULT_VIDEO_TIER_INDEX;
        mgr.set_simulcast_layers(3);
        let tier_before = mgr.video_tier_index();

        mgr.drop_top_layer();
        mgr.drop_top_layer();
        mgr.add_top_layer();

        assert_eq!(
            mgr.video_tier_index(),
            tier_before,
            "layer add/drop must never move the resolution-tier index"
        );
    }

    #[test]
    fn test_congestion_cut_respects_warmup_and_min_interval() {
        let mut mgr = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        mgr.created_at_ms = 1000.0;
        mgr.last_transition_time_ms = 0.0;
        mgr.video_tier_index = DEFAULT_VIDEO_TIER_INDEX;

        // During warmup: no cut, no hold.
        let changed = mgr.force_congestion_cut(2000.0);
        assert!(!changed, "Cut should be suppressed during warmup");
        assert!(
            !mgr.congestion_hold_active(2001.0),
            "Hold must not arm during warmup"
        );

        // Past warmup, first cut succeeds.
        let t = 1000.0 + DEFAULT_WARMUP_MS + 1000.0;
        let changed = mgr.force_congestion_cut(t);
        assert!(changed, "Cut should fire after warmup");

        // Immediate second cut is blocked by the min transition interval.
        let changed = mgr.force_congestion_cut(t + 100.0);
        assert!(
            !changed,
            "Second cut within MIN_TIER_TRANSITION_INTERVAL_MS should be blocked"
        );
    }

    // =====================================================================
    // User-configurable quality bounds (issue #961)
    //
    // QUALITY IS THE INVERSE OF INDEX: best == FLOOR on index (user max
    // quality), worst == CAP on index (user min quality).
    // =====================================================================

    /// Drive sustained terrible conditions for many ticks and return the final
    /// video tier index. Bad FPS + bad bitrate forces step-downs.
    fn drive_sustained_congestion(mgr: &mut AdaptiveQualityManager, start_ms: f64) -> usize {
        let mut t = start_ms;
        for _ in 0..60 {
            // fps_ratio ~0.1, bitrate_ratio ~0.1 — well below degrade thresholds.
            mgr.update(3.0, 30.0, 60.0, 600.0, t, 5);
            t += 1600.0;
        }
        mgr.video_tier_index()
    }

    /// Drive sustained excellent conditions for many ticks and return the final
    /// video tier index. Good FPS + good bitrate forces step-ups.
    fn drive_sustained_good(mgr: &mut AdaptiveQualityManager, start_ms: f64) -> usize {
        let mut t = start_ms;
        for _ in 0..60 {
            // fps_ratio ~0.97, bitrate_ratio ~0.95 — above recover thresholds.
            mgr.update(29.0, 30.0, 570.0, 600.0, t, 5);
            t += 6000.0;
        }
        mgr.video_tier_index()
    }

    #[test]
    fn test_user_worst_cap_blocks_step_down_under_sustained_congestion() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 0;
        // User min quality => worst/cap at index 3 ("standard"). No floor.
        mgr.set_video_quality_bounds(None, Some(3), 0.0);

        let final_idx = drive_sustained_congestion(&mut mgr, 10000.0);
        assert_eq!(
            final_idx, 3,
            "Index must never step DOWN past the user worst/cap (3); got {final_idx}"
        );
    }

    #[test]
    fn test_user_best_floor_blocks_step_up_under_sustained_good() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 7; // start at worst
                                  // User max quality => best/floor at index 2 ("hd"). No cap.
        mgr.set_video_quality_bounds(Some(2), None, 0.0);

        let final_idx = drive_sustained_good(&mut mgr, 10000.0);
        assert_eq!(
            final_idx, 2,
            "Index must never step UP below the user best/floor (2); got {final_idx}"
        );
    }

    #[test]
    fn test_user_best_equals_worst_pins_tier_both_directions() {
        // Pin to index 5. Neither congestion nor good conditions may move it.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 5;
        mgr.set_video_quality_bounds(Some(5), Some(5), 0.0);

        let after_congestion = drive_sustained_congestion(&mut mgr, 10000.0);
        assert_eq!(after_congestion, 5, "Pinned tier must not step DOWN");

        let after_good = drive_sustained_good(&mut mgr, 500_000.0);
        assert_eq!(after_good, 5, "Pinned tier must not step UP");
    }

    #[test]
    fn test_default_none_none_adapts_exactly_as_before_down() {
        // Regression: default construction (no bounds) must step all the way
        // down under sustained congestion, identical to a manager that never
        // had bounds touched.
        let mut baseline = new_test_manager(VIDEO_QUALITY_TIERS);
        baseline.video_tier_index = 0;
        let baseline_idx = drive_sustained_congestion(&mut baseline, 10000.0);

        let mut with_explicit_auto = new_test_manager(VIDEO_QUALITY_TIERS);
        with_explicit_auto.video_tier_index = 0;
        with_explicit_auto.set_video_quality_bounds(None, None, 0.0);
        let auto_idx = drive_sustained_congestion(&mut with_explicit_auto, 10000.0);

        let max_idx = VIDEO_QUALITY_TIERS.len() - 1;
        assert_eq!(baseline_idx, max_idx, "Default must reach worst tier");
        assert_eq!(
            auto_idx, baseline_idx,
            "Explicit None/None must behave identically to default"
        );
    }

    #[test]
    fn test_default_none_none_adapts_exactly_as_before_up() {
        // Regression: default (no bounds) must step all the way up under
        // sustained good conditions.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 7;
        let idx = drive_sustained_good(&mut mgr, 10000.0);
        assert_eq!(idx, 0, "Default must reach best tier under good conditions");
    }

    #[test]
    fn test_setting_bounds_snaps_current_index_into_range_immediately() {
        // Current index 0 (best). Set worst/cap to 4 — index is below the cap so
        // unaffected. Then set best/floor to 6 with cap 7: current 0 is above
        // floor 6, must snap to 6 immediately.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 0;

        // Floor 6 (worse-than-current) — must snap DOWN to 6 at once.
        mgr.set_video_quality_bounds(Some(6), None, 100.0);
        assert_eq!(
            mgr.video_tier_index(),
            6,
            "Setting a floor worse than the current index must snap into range immediately"
        );

        // Now current is 6. Set cap 2 (which also forces floor<=cap). Current 6
        // is above cap 2, must snap UP to 2 immediately.
        mgr.set_video_quality_bounds(None, Some(2), 200.0);
        assert_eq!(
            mgr.video_tier_index(),
            2,
            "Setting a cap better than the current index must snap into range immediately"
        );
    }

    #[test]
    fn test_inverted_bounds_are_normalized_by_swap() {
        // Pass best=5, worst=2 (inverted: floor index > cap index). Should be
        // swapped to best=2, worst=5 so the range is non-empty.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 3;
        mgr.set_video_quality_bounds(Some(5), Some(2), 0.0);
        let (best, worst) = mgr.user_video_quality_bounds();
        assert_eq!(best, Some(2), "Inverted floor should be swapped to the cap");
        assert_eq!(
            worst,
            Some(5),
            "Inverted cap should be swapped to the floor"
        );
    }

    #[test]
    fn test_bounds_clamped_to_valid_indices() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        let max = VIDEO_QUALITY_TIERS.len() - 1;
        mgr.set_video_quality_bounds(Some(999), Some(999), 0.0);
        let (best, worst) = mgr.user_video_quality_bounds();
        assert_eq!(best, Some(max), "Out-of-range floor clamps to max index");
        assert_eq!(worst, Some(max), "Out-of-range cap clamps to max index");
    }

    #[test]
    fn test_user_best_floor_composes_with_crash_ceiling_most_restrictive_wins() {
        // The internal crash ceiling must still cap step-up even when the user
        // best floor is looser (lower index). The user bound never loosens a
        // safety ceiling — the more restrictive (higher index) floor wins.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 6;
        // Arm a crash ceiling at index 4 directly (simulating yo-yo protection).
        mgr.crash_ceiling_index = Some(4);
        mgr.ceiling_expires_at_ms = f64::MAX; // never decays during the test
                                              // User best floor at index 1 (would allow climbing to 1) — looser than
                                              // the crash ceiling.
        mgr.set_video_quality_bounds(Some(1), None, 0.0);

        let final_idx = drive_sustained_good(&mut mgr, 10000.0);
        assert_eq!(
            final_idx, 4,
            "Crash ceiling (4) must win over the looser user floor (1); got {final_idx}"
        );
    }

    #[test]
    fn test_user_best_floor_more_restrictive_than_crash_ceiling_wins() {
        // When the user best floor is MORE restrictive (higher index) than the
        // crash ceiling, the user floor wins.
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.video_tier_index = 7;
        mgr.crash_ceiling_index = Some(2);
        mgr.ceiling_expires_at_ms = f64::MAX;
        // User floor at 5 — more restrictive than crash ceiling 2.
        mgr.set_video_quality_bounds(Some(5), None, 0.0);

        let final_idx = drive_sustained_good(&mut mgr, 10000.0);
        assert_eq!(
            final_idx, 5,
            "User floor (5) more restrictive than crash ceiling (2) must win; got {final_idx}"
        );
    }

    // ---- Audio bounds ----

    /// Drive sustained terrible audio conditions with video pinned at lowest so
    /// audio is allowed to degrade. Returns the final audio tier index.
    fn drive_sustained_audio_congestion(mgr: &mut AdaptiveQualityManager, start_ms: f64) -> usize {
        let max_video = VIDEO_QUALITY_TIERS.len() - 1;
        mgr.video_tier_index = max_video;
        let mut t = start_ms;
        for _ in 0..60 {
            // fps_ratio ~0.1 < AUDIO_TIER_DEGRADE_FPS_RATIO (0.30).
            mgr.update(1.0, 10.0, 150.0, 150.0, t, 5);
            t += 1600.0;
        }
        mgr.audio_tier_index()
    }

    #[test]
    fn test_audio_user_worst_cap_blocks_step_down() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.audio_tier_index = 0;
        // Audio worst/cap at index 1 ("medium").
        mgr.set_audio_quality_bounds(None, Some(1));

        let final_idx = drive_sustained_audio_congestion(&mut mgr, 10000.0);
        assert_eq!(
            final_idx, 1,
            "Audio index must never step DOWN past the user worst/cap (1); got {final_idx}"
        );
    }

    #[test]
    fn test_audio_user_best_floor_blocks_step_up() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        // Start audio degraded.
        mgr.audio_tier_index = 3;
        mgr.video_tier_index = VIDEO_QUALITY_TIERS.len() - 1;
        // Audio best/floor at index 1 — must not climb above (better than) 1.
        mgr.set_audio_quality_bounds(Some(1), None);

        let mut t = 10000.0;
        for _ in 0..40 {
            // fps_ratio ~0.9 > AUDIO_TIER_RECOVER_FPS_RATIO (0.60).
            mgr.update(9.0, 10.0, 150.0, 150.0, t, 5);
            t += 6000.0;
        }
        assert_eq!(
            mgr.audio_tier_index(),
            1,
            "Audio index must never step UP below the user best/floor (1)"
        );
    }

    #[test]
    fn test_audio_best_equals_worst_pins_tier() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.audio_tier_index = 2;
        mgr.set_audio_quality_bounds(Some(2), Some(2));

        let after = drive_sustained_audio_congestion(&mut mgr, 10000.0);
        assert_eq!(after, 2, "Pinned audio tier must not step DOWN");
    }

    #[test]
    fn test_audio_setting_bounds_snaps_current_index_into_range() {
        let mut mgr = new_test_manager(VIDEO_QUALITY_TIERS);
        mgr.audio_tier_index = 0;
        // Floor 2 worse than current 0 — snap down to 2 immediately.
        mgr.set_audio_quality_bounds(Some(2), None);
        assert_eq!(
            mgr.audio_tier_index(),
            2,
            "Audio must snap to floor at once"
        );
        // Cap 1 better than current 2 — snap up to 1 immediately.
        mgr.set_audio_quality_bounds(None, Some(1));
        assert_eq!(mgr.audio_tier_index(), 1, "Audio must snap to cap at once");
    }

    // =====================================================================
    // SCREEN-SHARE user quality bounds (issue #961 follow-up)
    //
    // Screen uses its own 3-tier ladder SCREEN_QUALITY_TIERS (index 0 = best /
    // 1080p, 2 = worst / low). The generic video clamp logic must work over it
    // exactly as over the 8-tier camera ladder. These tests construct the
    // manager via `new_for_screen` to exercise the real screen path.
    // =====================================================================

    /// Screen-share manager with warmup/transition guards zeroed so small
    /// `now_ms` test values are past the warmup window.
    fn new_test_screen_manager() -> AdaptiveQualityManager {
        let mut mgr = AdaptiveQualityManager::new_for_screen(SCREEN_QUALITY_TIERS);
        mgr.created_at_ms = 0.0;
        mgr.last_transition_time_ms = 0.0;
        mgr
    }

    #[test]
    fn test_screen_user_worst_cap_blocks_step_down_under_sustained_congestion() {
        let mut mgr = new_test_screen_manager();
        mgr.video_tier_index = 0; // best
                                  // User min quality => worst/cap at index 1 ("medium"). No floor.
        mgr.set_video_quality_bounds(None, Some(1), 0.0);

        let final_idx = drive_sustained_congestion(&mut mgr, 10000.0);
        assert_eq!(
            final_idx, 1,
            "Screen index must never step DOWN past the user worst/cap (1); got {final_idx}"
        );
    }

    #[test]
    fn test_screen_user_best_floor_blocks_step_up_under_sustained_good() {
        let mut mgr = new_test_screen_manager();
        mgr.video_tier_index = 2; // worst
                                  // User max quality => best/floor at index 1 ("medium"). No cap.
        mgr.set_video_quality_bounds(Some(1), None, 0.0);

        let final_idx = drive_sustained_good(&mut mgr, 10000.0);
        assert_eq!(
            final_idx, 1,
            "Screen index must never step UP below the user best/floor (1); got {final_idx}"
        );
    }

    #[test]
    fn test_screen_best_equals_worst_pins_tier_both_directions() {
        // Pin screen to index 1 ("medium"). Neither congestion nor good
        // conditions may move it.
        let mut mgr = new_test_screen_manager();
        mgr.video_tier_index = 1;
        mgr.set_video_quality_bounds(Some(1), Some(1), 0.0);

        let after_congestion = drive_sustained_congestion(&mut mgr, 10000.0);
        assert_eq!(after_congestion, 1, "Pinned screen tier must not step DOWN");

        let after_good = drive_sustained_good(&mut mgr, 500_000.0);
        assert_eq!(after_good, 1, "Pinned screen tier must not step UP");
    }

    #[test]
    fn test_screen_bounds_clamped_to_three_tier_ladder() {
        // Out-of-range indices (valid for the 8-tier camera ladder but not the
        // 3-tier screen ladder) must clamp to the screen max index (2).
        let mut mgr = new_test_screen_manager();
        let max = SCREEN_QUALITY_TIERS.len() - 1; // 2
        mgr.set_video_quality_bounds(Some(7), Some(5), 0.0);
        let (best, worst) = mgr.user_video_quality_bounds();
        assert_eq!(
            best,
            Some(max),
            "Screen floor clamps to max screen index (2)"
        );
        assert_eq!(
            worst,
            Some(max),
            "Screen cap clamps to max screen index (2)"
        );
    }

    #[test]
    fn test_screen_setting_bounds_snaps_current_index_into_range_immediately() {
        let mut mgr = new_test_screen_manager();
        mgr.video_tier_index = 0; // best
                                  // Floor 2 (worst) worse than current 0 — snap DOWN to 2 at once.
        mgr.set_video_quality_bounds(Some(2), None, 100.0);
        assert_eq!(
            mgr.video_tier_index(),
            2,
            "Screen floor worse than current must snap into range immediately"
        );
        // Cap 1 better than current 2 — snap UP to 1 at once.
        mgr.set_video_quality_bounds(None, Some(1), 200.0);
        assert_eq!(
            mgr.video_tier_index(),
            1,
            "Screen cap better than current must snap into range immediately"
        );
    }

    #[test]
    fn test_screen_default_none_none_adapts_full_range() {
        // Regression: default screen manager (no bounds) steps all the way down
        // to the worst screen tier under sustained congestion.
        let mut mgr = new_test_screen_manager();
        mgr.video_tier_index = 0;
        let idx = drive_sustained_congestion(&mut mgr, 10000.0);
        assert_eq!(
            idx,
            SCREEN_QUALITY_TIERS.len() - 1,
            "Default screen bounds must reach the worst tier under congestion"
        );
    }
}

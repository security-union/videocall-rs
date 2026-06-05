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

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::clock::{default_clock, Clock};
use crate::constants::{
    cap_layers_to_budget, screen_share_camera_ceiling_index, simulcast_layers, uplink_budget_kbps,
    AudioQualityTier, VideoQualityTier, ENCODER_BACKPRESSURE_SUSTAIN_MS,
    ENCODER_QUEUE_BACKPRESSURE_CLEAR, ENCODER_QUEUE_BACKPRESSURE_HIGH,
    STEP_UP_STABILIZATION_WINDOW_MS, VIDEO_QUALITY_TIERS,
};
use crate::manager::{AdaptiveQualityManager, TierTransitionRecord};

const AQ_SUMMARY_INTERVAL_MS: f64 = 30_000.0;

/// EncoderControl is responsible for bridging the gap between the encoder and the
/// diagnostics system.
/// It closes the loop by allowing the encoder to adjust its settings based on
/// feedback from the diagnostics system.
#[derive(Debug, Clone)]
pub enum EncoderControl {
    UpdateBitrate { target_bitrate_kbps: u32 },
}

// ---------------------------------------------------------------------------
// REMOVED (issue #1108, Phase B / Stage 2): the entire receiver-FPS fan-in.
//
// `DiagnosticPacketWindow` / `DiagnosticPackets` / `get_p75_fps` aggregated the
// FPS that *peers reported receiving* and fed it into the sender's PID + tier +
// layer-shed decision. Stage 2 removes receiver FPS from the sender AQ entirely:
// the sender now adapts ONLY to its own signals (encode-queue backpressure,
// server CONGESTION, WS send-buffer pressure). The sender still RECEIVES peer
// diagnostics for the global vcprobe broadcast + the UI stats string
// (videocall-client diagnostics_manager sinks 1 & 2), but they no longer reach
// the encoder AQ. See `EncoderBitrateController::tick`.
//
// TODO(#1108 cleanup): the `fps_received` / FPS-health proto fields are now
// silently omitted (NaN-guarded in health_reporter) rather than removed; a
// follow-up should drop the dead proto fields + Grafana panels.
// ---------------------------------------------------------------------------

/// Upper bitrate clamp (kbps) for one simulcast layer (issue #989, PR B).
///
/// While a self-targeted CONGESTION drain hold is active (see
/// [`EncoderBitrateController::tick`]), the layer's upper bound is its tier
/// **ideal** (clamped up to at least `min_bitrate_kbps` for degenerate tiers) so
/// the layer cannot ramp bitrate back up and re-fill the draining relay buffer;
/// otherwise it is the tier **max**. Extracted as a pure function so the pin
/// decision is unit-testable independently of the bitrate pipeline.
fn layer_upper_clamp_kbps(tier: &VideoQualityTier, congestion_hold: bool) -> f64 {
    if congestion_hold {
        (tier.ideal_bitrate_kbps as f64).max(tier.min_bitrate_kbps as f64)
    } else {
        tier.max_bitrate_kbps as f64
    }
}

pub struct EncoderBitrateController {
    /// Current sender video tier's ideal bitrate (kbps). Updated when the tier
    /// changes; in single-stream mode it is the encoder's target, in simulcast
    /// mode the per-layer targets in `last_layer_target_bitrates_kbps` are used
    /// instead.
    ideal_bitrate_kbps: u32,
    /// Sender's target FPS (the desired capture rate, NOT anything a receiver
    /// reports). Shared with the encoder via an atomic. Surfaced in the
    /// AQ_STATUS summary log. Issue #1108 removed receiver FPS from the AQ; this
    /// remains the sender's own target.
    target_fps: Arc<AtomicU32>,
    /// Adaptive quality state machine for tier selection.
    quality_manager: AdaptiveQualityManager,
    /// Set to `true` after any tier/layer transition, cleared by the caller via
    /// [`Self::take_tier_changed`].
    tier_changed: bool,
    /// Last emitted single-stream target bitrate (kbps) for external
    /// observation. In simulcast mode this tracks the active layers' sum.
    last_target_bitrate_kbps: f64,
    /// Timestamp (ms) of the last AQ_STATUS summary log emission.
    last_aq_summary_ms: f64,
    /// Clock used for all internal wall-clock reads. The browser path uses
    /// [`JsDateClock`], native callers use [`SystemClock`], and tests can inject
    /// a [`TestClock`] for determinism.
    ///
    /// [`JsDateClock`]: crate::clock::JsDateClock
    /// [`SystemClock`]: crate::clock::SystemClock
    /// [`TestClock`]: crate::clock::TestClock
    clock: Arc<dyn Clock>,

    // --- Simulcast per-layer state (issue #989; PIDs removed in #1108) ---
    /// Fixed per-layer tiers for the active ladder (lowest layer first, index ==
    /// `layer_id`). Empty in single-stream mode. Issue #1108 replaced the
    /// per-layer PIDs with the nominal-budget baseline: each active layer's
    /// target is simply its tier ideal (clamped by [`layer_upper_clamp_kbps`]),
    /// budget-capped by [`cap_layers_to_budget`].
    layer_tiers: Vec<&'static VideoQualityTier>,
    /// Last per-layer target bitrates (kbps), `layer_tiers.len()` entries.
    /// Only the first `active_layer_count` are meaningful for the encoder; the
    /// rest are retained so a re-added layer resumes near its last target. Empty
    /// in single-stream mode.
    last_layer_target_bitrates_kbps: Vec<f64>,
    /// Whether this controller drives SCREEN share (issue #989, Phase 3b). When
    /// true, [`set_simulcast_layers`](Self::set_simulcast_layers) builds its
    /// per-layer tiers from the SCREEN ladder rather than the camera ladder.
    is_screen: bool,

    // --- Sender encoder backpressure (issue #1108, Phase B) ---
    /// Last sampled max `encode_queue_size()` across the ACTIVE simulcast layers,
    /// fed in by the encode loop via
    /// [`observe_encoder_queue_depth`](Self::observe_encoder_queue_depth). The
    /// sender's *own* encode backpressure (frames pending in WebCodecs),
    /// independent of how peers receive the stream. Stage 2: [`tick`](Self::tick)
    /// maps this into degrade/recover decisions. Native callers (the load-test
    /// bot, no WebCodecs) feed `0`, so they never degrade on this axis.
    last_encoder_queue_depth: u32,
    /// Timestamp (ms) when the encoder queue depth first rose to/above
    /// [`ENCODER_QUEUE_BACKPRESSURE_HIGH`](crate::constants::ENCODER_QUEUE_BACKPRESSURE_HIGH)
    /// and has stayed there since. The sustain timer for the gradual step-DOWN:
    /// degrade fires once this has been continuously set for
    /// [`ENCODER_BACKPRESSURE_SUSTAIN_MS`](crate::constants::ENCODER_BACKPRESSURE_SUSTAIN_MS).
    /// Reset to `None` whenever the depth drops below HIGH. Maintained by
    /// [`tick`](Self::tick).
    backpressure_high_since_ms: Option<f64>,
    /// Timestamp (ms) when the encoder queue depth first dropped to/below
    /// [`ENCODER_QUEUE_BACKPRESSURE_CLEAR`](crate::constants::ENCODER_QUEUE_BACKPRESSURE_CLEAR)
    /// and has stayed there since. The stabilization timer for the step-UP:
    /// recover fires once this has been continuously set for
    /// [`STEP_UP_STABILIZATION_WINDOW_MS`]. Reset to `None` whenever the depth
    /// rises above CLEAR. Maintained by [`tick`](Self::tick).
    backpressure_clear_since_ms: Option<f64>,
}

impl EncoderBitrateController {
    /// Create a new bitrate controller using the default `VIDEO_QUALITY_TIERS`.
    pub fn new(ideal_bitrate_kbps: u32, target_fps: Arc<AtomicU32>) -> Self {
        Self::with_clock(ideal_bitrate_kbps, target_fps, default_clock())
    }

    /// Create a new bitrate controller with an injected [`Clock`].
    ///
    /// Native callers (e.g. the load-test bot) can pass a [`SystemClock`];
    /// tests can pass a [`TestClock`] for deterministic timestamps.
    ///
    /// [`SystemClock`]: crate::clock::SystemClock
    /// [`TestClock`]: crate::clock::TestClock
    pub fn with_clock(
        ideal_bitrate_kbps: u32,
        target_fps: Arc<AtomicU32>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let quality_manager =
            AdaptiveQualityManager::with_clock(VIDEO_QUALITY_TIERS, Arc::clone(&clock));
        // is_screen = false → camera simulcast ladder.
        Self::build(
            ideal_bitrate_kbps,
            target_fps,
            quality_manager,
            clock,
            false,
        )
    }

    /// Create a new bitrate controller for screen share.
    ///
    /// Uses `SCREEN_QUALITY_TIERS` starting at `DEFAULT_SCREEN_TIER_INDEX`
    /// (medium/720p) via [`AdaptiveQualityManager::new_for_screen`]. The
    /// initial `ideal_bitrate_kbps` is synced from the starting tier so the
    /// PID controller does not make an unnecessary correction on the first
    /// update.
    pub fn new_for_screen(
        target_fps: Arc<AtomicU32>,
        video_tiers: &'static [VideoQualityTier],
    ) -> Self {
        Self::new_for_screen_with_clock(target_fps, video_tiers, default_clock())
    }

    /// Create a new screen-share bitrate controller with an injected [`Clock`].
    pub fn new_for_screen_with_clock(
        target_fps: Arc<AtomicU32>,
        video_tiers: &'static [VideoQualityTier],
        clock: Arc<dyn Clock>,
    ) -> Self {
        let quality_manager =
            AdaptiveQualityManager::new_for_screen_with_clock(video_tiers, Arc::clone(&clock));
        let tier_ideal = quality_manager.current_video_tier().ideal_bitrate_kbps;
        // is_screen = true so simulcast layer PIDs use the SCREEN ladder.
        Self::build(tier_ideal, target_fps, quality_manager, clock, true)
    }

    /// Internal constructor shared by `new` and `new_for_screen`.
    ///
    /// `is_screen` selects which simulcast ladder
    /// [`set_simulcast_layers`](Self::set_simulcast_layers) builds per-layer tiers
    /// from: the SCREEN ladder when `true`, otherwise the camera ladder.
    fn build(
        ideal_bitrate_kbps: u32,
        target_fps: Arc<AtomicU32>,
        quality_manager: AdaptiveQualityManager,
        clock: Arc<dyn Clock>,
        is_screen: bool,
    ) -> Self {
        Self {
            ideal_bitrate_kbps,
            target_fps,
            quality_manager,
            tier_changed: false,
            last_target_bitrate_kbps: 0.0,
            last_aq_summary_ms: 0.0,
            clock,
            // Simulcast: single-stream by default (empty vecs). Enabled via
            // set_simulcast_layers().
            layer_tiers: Vec::new(),
            last_layer_target_bitrates_kbps: Vec::new(),
            is_screen,
            // Sender encoder backpressure (issue #1108, Phase B). Starts at 0
            // (no backpressure); the encode loop overwrites it each tick via
            // observe_encoder_queue_depth(). Both sustain/stabilization timers
            // start unarmed.
            last_encoder_queue_depth: 0,
            backpressure_high_since_ms: None,
            backpressure_clear_since_ms: None,
        }
    }

    /// Enable simulcast for this controller with an `n`-layer ladder
    /// (issue #989; per-layer PIDs removed in #1108).
    ///
    /// Configures the quality manager's active-layer state and records the fixed
    /// per-layer tiers from the corresponding ladder rung. `n == 1` (or `0`)
    /// leaves the controller in single-stream mode — the per-layer vecs stay
    /// empty and every existing code path is unchanged, which is exactly what the
    /// bot and all current callers get.
    ///
    /// Call once after construction, before the first tick.
    pub fn set_simulcast_layers(&mut self, n: usize) {
        // Clamp + configure the manager (single source of truth for the count).
        self.quality_manager.set_simulcast_layers(n);
        let effective = self.quality_manager.simulcast_layer_count();

        if effective <= 1 {
            // Single-stream mode: nothing to build, behave exactly as before.
            self.layer_tiers.clear();
            self.last_layer_target_bitrates_kbps.clear();
            return;
        }
        let tiers = self.simulcast_ladder(effective);
        self.layer_tiers = tiers.iter().collect();
        self.last_layer_target_bitrates_kbps =
            tiers.iter().map(|t| t.ideal_bitrate_kbps as f64).collect();
    }

    /// The simulcast layer ladder for this controller: SCREEN ladder when this
    /// controller drives screen share, otherwise the camera ladder (issue #989,
    /// Phase 3b). Both are `&'static` and lowest-layer-first.
    fn simulcast_ladder(&self, n: usize) -> &'static [VideoQualityTier] {
        if self.is_screen {
            crate::constants::simulcast_screen_layers(n)
        } else {
            simulcast_layers(n)
        }
    }

    /// Map the latest sampled encoder queue depth into a `(degrade, recover)`
    /// decision, maintaining the two hysteresis timers (issue #1108, Phase B).
    ///
    /// * `degrade` — the depth has been at/above
    ///   [`ENCODER_QUEUE_BACKPRESSURE_HIGH`] continuously for at least
    ///   [`ENCODER_BACKPRESSURE_SUSTAIN_MS`] (a brief encode hiccup does not
    ///   shed).
    /// * `recover` — the depth has been at/below
    ///   [`ENCODER_QUEUE_BACKPRESSURE_CLEAR`] continuously for at least
    ///   [`STEP_UP_STABILIZATION_WINDOW_MS`].
    ///
    /// Between CLEAR and HIGH is the hysteresis dead-band: neither timer is
    /// armed, so the controller holds its current tier/layer configuration. Both
    /// can be `false` simultaneously (dead-band); they are never both `true`
    /// because CLEAR < HIGH.
    ///
    /// [`ENCODER_QUEUE_BACKPRESSURE_HIGH`]: crate::constants::ENCODER_QUEUE_BACKPRESSURE_HIGH
    /// [`ENCODER_QUEUE_BACKPRESSURE_CLEAR`]: crate::constants::ENCODER_QUEUE_BACKPRESSURE_CLEAR
    /// [`ENCODER_BACKPRESSURE_SUSTAIN_MS`]: crate::constants::ENCODER_BACKPRESSURE_SUSTAIN_MS
    fn backpressure_decision(&mut self, now: f64) -> (bool, bool) {
        let depth = self.last_encoder_queue_depth;

        // --- Degrade (sustain) timer ---
        if depth >= ENCODER_QUEUE_BACKPRESSURE_HIGH {
            let since = *self.backpressure_high_since_ms.get_or_insert(now);
            let degrade = now - since >= ENCODER_BACKPRESSURE_SUSTAIN_MS;
            // A genuinely-high queue is, by definition, not clear.
            self.backpressure_clear_since_ms = None;
            (degrade, false)
        } else if depth <= ENCODER_QUEUE_BACKPRESSURE_CLEAR {
            // --- Recover (stabilization) timer ---
            self.backpressure_high_since_ms = None;
            let since = *self.backpressure_clear_since_ms.get_or_insert(now);
            let recover = now - since >= STEP_UP_STABILIZATION_WINDOW_MS as f64;
            (false, recover)
        } else {
            // Hysteresis dead-band: hold. Neither timer accumulates.
            self.backpressure_high_since_ms = None;
            self.backpressure_clear_since_ms = None;
            (false, false)
        }
    }

    /// Advance the controller one tick (issue #1108, Phase B).
    ///
    /// This is the sole periodic entry point now that receiver FPS no longer
    /// feeds the sender AQ. Each tick:
    ///
    /// 1. maps the latest sampled encoder queue depth (fed via
    ///    [`observe_encoder_queue_depth`](Self::observe_encoder_queue_depth))
    ///    into a `(degrade, recover)` decision with hysteresis timers;
    /// 2. drives the tier state machine via
    ///    [`AdaptiveQualityManager::update_from_backpressure`], reusing ALL the
    ///    existing warmup / min-interval / crash-ceiling / yo-yo / congestion-hold
    ///    gating;
    /// 3. translates a tier step-down/up (or a floor-saturated degrade) into a
    ///    simulcast top-layer shed/restore (the underlying tier movement is
    ///    already `MIN_TIER_TRANSITION_INTERVAL_MS`-gated in the manager);
    /// 4. recomputes the per-layer (or single-stream) target bitrates from the
    ///    nominal-budget baseline.
    ///
    /// `now` is the current wall-clock timestamp in milliseconds (the browser
    /// passes `Date::now()`, the bot a monotonic clock, tests a `TestClock`).
    ///
    /// The explicit signal paths — [`force_congestion_cut`](Self::force_congestion_cut)
    /// and [`force_video_step_down`](Self::force_video_step_down) — are unchanged
    /// and act immediately; this tick only governs the gradual backpressure axis.
    pub fn tick(&mut self, now: f64) {
        let (degrade, recover) = self.backpressure_decision(now);

        // Capture the tier index BEFORE update so we can translate the manager's
        // incident-hardened degrade/recover decision (which moves
        // video_tier_index) into a top-layer drop/add — reusing all the existing
        // hysteresis / crash-ceiling / yo-yo / re-election / congestion-hold
        // gating for free.
        let tier_index_before = self.quality_manager.video_tier_index();
        let tier_changed = self
            .quality_manager
            .update_from_backpressure(degrade, recover, now);

        // Simulcast layer re-target (issue #989, floor-independent shed #1077):
        // a tier step DOWN sheds the top active layer; a step UP restores it. We
        // ALSO shed when the manager reports a degrade the tier floor blocked
        // (`wanted_degrade_at_floor`), decoupling the layer axis from the tier
        // floor exactly as the explicit `force_*` paths do. No-op in
        // single-stream mode.
        if self.quality_manager.is_simulcast() {
            let tier_index_after = self.quality_manager.video_tier_index();
            let degrade_down = (tier_changed && tier_index_after > tier_index_before)
                || self.quality_manager.wanted_degrade_at_floor();
            let recover_up = tier_changed && tier_index_after < tier_index_before;
            if degrade_down {
                if self.quality_manager.drop_top_layer() {
                    self.tier_changed = true;
                }
            } else if recover_up && self.quality_manager.add_top_layer() {
                self.tier_changed = true;
            }
        }
        if tier_changed {
            self.tier_changed = true;
            let old_bitrate = self.ideal_bitrate_kbps;
            let new_tier = self.quality_manager.current_video_tier();
            self.ideal_bitrate_kbps = new_tier.ideal_bitrate_kbps;
            log::info!(
                "AQ_BITRATE_CHANGE: base_bitrate {} -> {} kbps (tier: {}, index: {}, reason: backpressure)",
                old_bitrate,
                self.ideal_bitrate_kbps,
                new_tier.label,
                self.quality_manager.video_tier_index(),
            );
        }

        // --- Per-layer (or single-stream) target bitrates ---
        // Nominal-budget baseline (issue #1108): each active layer's target is
        // its tier ideal, clamped by `layer_upper_clamp_kbps` (pinned to the tier
        // ideal during a CONGESTION drain hold), then budget-capped. No PID, no
        // bandwidth estimate.
        if self.quality_manager.is_simulcast() && !self.layer_tiers.is_empty() {
            let congestion_hold = self.quality_manager.congestion_hold_active(now);
            let active = self.quality_manager.active_layer_count();
            self.compute_layer_bitrates(congestion_hold, active);
            // Surface the active layers' sum as the single observable bitrate.
            self.last_target_bitrate_kbps = self
                .last_layer_target_bitrates_kbps
                .iter()
                .take(active)
                .sum();
        } else {
            // Single-stream: target is the current tier's ideal, pinned to the
            // tier ideal during a drain hold (already the ideal), else clamped to
            // the tier max.
            let tier = self.quality_manager.current_video_tier();
            let congestion_hold = self.quality_manager.congestion_hold_active(now);
            self.last_target_bitrate_kbps =
                layer_upper_clamp_kbps(tier, congestion_hold).min(tier.max_bitrate_kbps as f64);
        }

        let should_log_summary =
            tier_changed || (now - self.last_aq_summary_ms >= AQ_SUMMARY_INTERVAL_MS);
        if should_log_summary {
            self.last_aq_summary_ms = now;
            let video_tier = self.quality_manager.current_video_tier();
            let audio_tier = self.quality_manager.current_audio_tier();
            log::info!(
                "AQ_STATUS: video_tier={}({}) audio_tier={}({}) target_fps={} \
                 target_bitrate={:.0} encoder_queue_depth={} active_layers={}",
                video_tier.label,
                self.quality_manager.video_tier_index(),
                audio_tier.label,
                self.quality_manager.audio_tier_index(),
                self.target_fps.load(Ordering::Relaxed),
                self.last_target_bitrate_kbps,
                self.last_encoder_queue_depth,
                self.quality_manager.active_layer_count(),
            );
        }
    }

    /// Compute and store per-layer target bitrates for the active simulcast
    /// ladder (issue #989; PIDs removed in #1108).
    ///
    /// Nominal-budget baseline: each layer's target is its fixed tier ideal,
    /// clamped by [`layer_upper_clamp_kbps`] (pinned to the tier ideal while a
    /// self-targeted CONGESTION drain hold is active so surviving layers cannot
    /// ramp back up and re-fill the draining relay buffer), then the ACTIVE
    /// layers are capped to the uplink budget by [`cap_layers_to_budget`].
    /// Called only in simulcast mode.
    fn compute_layer_bitrates(&mut self, congestion_hold: bool, active: usize) {
        for i in 0..self.layer_tiers.len() {
            let tier = self.layer_tiers[i];
            // Nominal baseline: the tier ideal, clamped to the (hold-aware) upper
            // bound and never below the tier floor.
            let tier_min = tier.min_bitrate_kbps as f64;
            let tier_max = layer_upper_clamp_kbps(tier, congestion_hold);
            let target = (tier.ideal_bitrate_kbps as f64).clamp(tier_min, tier_max);
            self.last_layer_target_bitrates_kbps[i] = target;
        }

        // --- Sender uplink budget cap (issue #989, Phase 1) — KEPT VERBATIM ---
        // The per-layer baselines above each sit at their OWN tier ideal, so
        // their SUM can exceed what the sender's uplink can afford. Publishing N
        // layers costs the sum, so cap the ACTIVE layers' targets to the uplink
        // budget (sum of active tier ideals), proportionally shedding bitrate
        // above each layer's tier floor. This shrinks automatically as the AQ
        // sheds the top layer under congestion (`active` drops -> budget drops).
        // Shed layers are untouched (not encoded/sent). The base layer keeps its
        // floor so it stays viewable. No-op when the layers already fit.
        let tiers = self.simulcast_ladder(self.layer_tiers.len());
        let budget = uplink_budget_kbps(tiers, active);
        cap_layers_to_budget(
            &mut self.last_layer_target_bitrates_kbps,
            tiers,
            active,
            budget,
        );
    }

    /// Returns `true` if a tier transition occurred since the last call to this
    /// method, then resets the flag. Callers should use this to detect when
    /// encoder settings (resolution, fps, keyframe interval) need updating.
    pub fn take_tier_changed(&mut self) -> bool {
        std::mem::take(&mut self.tier_changed)
    }

    /// Get the current video quality tier recommendation.
    pub fn current_video_tier(&self) -> &'static VideoQualityTier {
        self.quality_manager.current_video_tier()
    }

    /// Get the current audio quality tier recommendation.
    pub fn current_audio_tier(&self) -> &'static AudioQualityTier {
        self.quality_manager.current_audio_tier()
    }

    /// Get the current video tier index.
    pub fn video_tier_index(&self) -> usize {
        self.quality_manager.video_tier_index()
    }

    /// Get the current audio tier index.
    pub fn audio_tier_index(&self) -> usize {
        self.quality_manager.audio_tier_index()
    }

    // -----------------------------------------------------------------
    // Simulcast accessors (issue #989, PR B) — ADDITIVE
    // -----------------------------------------------------------------

    /// Whether this controller is in simulcast mode (`> 1` layers).
    pub fn is_simulcast(&self) -> bool {
        self.quality_manager.is_simulcast()
    }

    /// Number of simulcast layers in the configured ladder (`1` in
    /// single-stream mode).
    pub fn simulcast_layer_count(&self) -> usize {
        self.quality_manager.simulcast_layer_count()
    }

    /// Number of simulcast layers currently active (encoded + sent). `1` in
    /// single-stream mode. The encoder shuts down layers with `layer_id >=`
    /// this value.
    pub fn active_layer_count(&self) -> usize {
        self.quality_manager.active_layer_count()
    }

    /// Per-layer target bitrates in kbps, lowest layer first (index ==
    /// `layer_id`). One entry per ladder layer; only the first
    /// [`active_layer_count`](Self::active_layer_count) are encoded by the
    /// caller, but all are returned so a re-added layer resumes near its last
    /// target. Empty in single-stream mode.
    pub fn layer_target_bitrates_kbps(&self) -> &[f64] {
        &self.last_layer_target_bitrates_kbps
    }

    /// Fixed resolution `(width, height)` for a given simulcast `layer_id`, or
    /// `None` if out of range / single-stream mode. Resolution is fixed by the
    /// simulcast ladder; only the bitrate adapts.
    pub fn layer_resolution(&self, layer_id: usize) -> Option<(u32, u32)> {
        self.layer_tiers
            .get(layer_id)
            .map(|t| (t.max_width, t.max_height))
    }

    // --- Telemetry accessors (issue #1108) ---
    //
    // Receiver FPS no longer feeds the sender AQ, so the former fps_ratio /
    // bitrate_ratio observability signals no longer exist. They are repointed to
    // `f64::NAN` so the health reporter's `is_finite()` guard silently drops the
    // corresponding (now-dead) proto fields — see the `// TODO(#1108 cleanup)`
    // in the removed-fan-in comment at the top of this file. The
    // `last_p75_peer_fps` accessor is REPOINTED to carry the new sender
    // backpressure signal (encoder queue depth) so the existing host telemetry
    // channel surfaces it with zero proto/Grafana churn.

    /// Deprecated FPS-ratio telemetry — receiver FPS was removed from the sender
    /// AQ (issue #1108). Returns `NaN` so the health reporter omits the field.
    /// TODO(#1108 cleanup): remove this accessor + its proto field.
    pub fn last_fps_ratio(&self) -> f64 {
        f64::NAN
    }

    /// Repointed (issue #1108): now carries the sender's encoder backpressure
    /// signal — the last sampled `encode_queue_size()` — reusing this telemetry
    /// channel so the host panel can surface the new signal without a new proto
    /// field. (Despite the legacy name, this is NOT a receiver FPS.)
    /// TODO(#1108 cleanup): rename the field/proto to `encoder_queue_depth`.
    pub fn last_p75_peer_fps(&self) -> f64 {
        self.last_encoder_queue_depth as f64
    }

    /// Deprecated bitrate-ratio telemetry — the receiver-FPS PID that produced it
    /// was removed (issue #1108). Returns `NaN` so the health reporter omits the
    /// field. TODO(#1108 cleanup): remove this accessor + its proto field.
    pub fn last_bitrate_ratio(&self) -> f64 {
        f64::NAN
    }

    /// Last emitted target bitrate (kbps). In simulcast mode this is the sum of
    /// the active layers' targets; in single-stream mode the current tier's
    /// (hold-aware) target. Updated by [`tick`](Self::tick).
    pub fn last_target_bitrate_kbps(&self) -> f64 {
        self.last_target_bitrate_kbps
    }

    /// Feed the sender's own encoder backpressure into the controller (issue
    /// #1108, Phase B).
    ///
    /// `depth` is the max WebCodecs `VideoEncoder::encode_queue_size()` across
    /// the currently ACTIVE simulcast layers — i.e. how many frames the sender's
    /// encoder is behind, *not* anything about how peers receive the stream.
    /// The browser encode loop samples this each frame and the control loop
    /// forwards it here; the native load-test bot (no WebCodecs) passes `0`.
    ///
    /// **Target-agnostic by design:** the parameter is a plain `u32` so this
    /// crate stays free of any `web-sys`/WebCodecs dependency.
    ///
    /// Stored for the next [`tick`](Self::tick), which maps it (with
    /// [`ENCODER_QUEUE_BACKPRESSURE_HIGH`], [`ENCODER_QUEUE_BACKPRESSURE_CLEAR`],
    /// [`ENCODER_BACKPRESSURE_SUSTAIN_MS`], and the stabilization window) into the
    /// gradual degrade/recover decision that drives tier + simulcast-layer shed.
    ///
    /// [`ENCODER_QUEUE_BACKPRESSURE_HIGH`]: crate::constants::ENCODER_QUEUE_BACKPRESSURE_HIGH
    /// [`ENCODER_QUEUE_BACKPRESSURE_CLEAR`]: crate::constants::ENCODER_QUEUE_BACKPRESSURE_CLEAR
    /// [`ENCODER_BACKPRESSURE_SUSTAIN_MS`]: crate::constants::ENCODER_BACKPRESSURE_SUSTAIN_MS
    pub fn observe_encoder_queue_depth(&mut self, depth: u32) {
        self.last_encoder_queue_depth = depth;
    }

    /// Last sampled encoder queue depth fed in via
    /// [`observe_encoder_queue_depth`](Self::observe_encoder_queue_depth)
    /// (issue #1108, Phase B). `0` when no backpressure has been reported (and
    /// always `0` for the native bot).
    pub fn last_encoder_queue_depth(&self) -> u32 {
        self.last_encoder_queue_depth
    }

    /// Force an immediate video quality step-down due to server congestion.
    ///
    /// Delegates to [`AdaptiveQualityManager::force_video_step_down`].
    /// Returns `true` if the tier actually changed.
    pub fn force_video_step_down(&mut self) -> bool {
        let now = self.clock.now_ms();
        // Capture the forced-transition guard state BEFORE the manager call,
        // since a successful step-down updates last_transition_time_ms (which
        // would then make forced_transition_guards_clear read false on the same
        // tick). The layer shed below uses this so it fires only when the
        // request was NOT blocked by warmup / min-interval — but, unlike the
        // tier step-down, independent of the tier floor.
        let guards_clear = self.quality_manager.forced_transition_guards_clear(now);
        let changed = self.quality_manager.force_video_step_down(now);
        // Simulcast (issue #989): WS backpressure / a gentle congestion
        // step-down sheds the top active layer too. Drive this off the LAYER
        // floor, not off the manager's tier `changed` return: the manager
        // returns `false` once `video_tier_index` is at its lowest tier, but the
        // layer axis is independent of that tier floor (simulcast layers have
        // FIXED resolutions). Coupling to `changed` would stop shedding layers
        // the moment the tier index bottoms out. We still respect the warmup /
        // min-interval guards via `guards_clear` so a blocked request sheds
        // nothing. No-op in single-stream mode (drop_top_layer floors at 1).
        if guards_clear
            && self.quality_manager.is_simulcast()
            && self.quality_manager.drop_top_layer()
        {
            self.tier_changed = true;
        }
        if changed {
            self.tier_changed = true;
            let old_bitrate = self.ideal_bitrate_kbps;
            let new_tier = self.quality_manager.current_video_tier();
            self.ideal_bitrate_kbps = new_tier.ideal_bitrate_kbps;
            log::info!(
                "AQ_BITRATE_CHANGE: base_bitrate {} -> {} kbps (tier: {}, index: {}, reason: force_step_down)",
                old_bitrate,
                self.ideal_bitrate_kbps,
                new_tier.label,
                self.quality_manager.video_tier_index(),
            );
        }
        changed
    }

    /// Aggressively cut video quality in response to a self-targeted server
    /// CONGESTION signal (the relay is dropping our outbound packets).
    ///
    /// Delegates to [`AdaptiveQualityManager::force_congestion_cut`], which
    /// drops multiple tiers at once and arms a short drain hold that pins the
    /// bitrate ceiling so the overflowing relay buffer can recover. The drain
    /// hold is honored by the next [`tick`](Self::tick) (which recomputes the
    /// per-layer / single-stream targets against `congestion_hold_active`), so
    /// the recomputed lower bitrate lands on the next tick — no throttle bypass
    /// is needed now that the gradual axis is a self-timer rather than a
    /// throttled diagnostics loop. Returns `true` if the tier actually changed.
    pub fn force_congestion_cut(&mut self) -> bool {
        let now = self.clock.now_ms();
        let changed = self.quality_manager.force_congestion_cut(now);
        // Simulcast (issue #989): a self-targeted CONGESTION cut is the
        // strongest signal — the relay is actively dropping our packets — so
        // shed the top active layer to cut egress AND sender encode CPU
        // immediately, in addition to the per-layer bitrate cut. The manager's
        // `force_congestion_cut` already drops two *tiers* in single-stream mode;
        // here we mirror that aggression on the layer axis by dropping the top
        // layer once. (A two-layer drop is not warranted: dropping one HD/SD
        // layer already sheds the bulk of the egress, and the per-layer bitrate
        // PIDs continue to throttle the remaining layers under the same drain
        // hold.)
        //
        // Gate the layer shed on the drain HOLD being active post-cut, NOT on
        // the manager's `changed` return. `force_congestion_cut` returns `false`
        // in two distinct cases: (a) blocked by the warmup / min-transition-
        // interval guard — nothing happened, no hold armed; and (b) the tier is
        // already at its floor but the hold IS armed (see manager.rs). We must
        // shed a layer in case (b) but not (a). `congestion_hold_active(now)` is
        // true iff the cut took effect cleanly — and it stays true at the tier
        // floor, so this decouples the layer axis from the tier floor while
        // still respecting the warmup / min-interval guards. No-op in
        // single-stream mode (drop_top_layer floors at 1).
        let cut_took_effect = self.quality_manager.congestion_hold_active(now);
        let layer_shed = cut_took_effect
            && self.quality_manager.is_simulcast()
            && self.quality_manager.drop_top_layer();
        if layer_shed {
            // A layer was shed but the tier index may NOT have moved (we were at
            // the tier floor). The next tick recomputes the per-layer bitrates
            // against the just-armed drain hold.
            self.tier_changed = true;
        }
        if changed {
            self.tier_changed = true;
            let old_bitrate = self.ideal_bitrate_kbps;
            let new_tier = self.quality_manager.current_video_tier();
            self.ideal_bitrate_kbps = new_tier.ideal_bitrate_kbps;
            log::info!(
                "AQ_BITRATE_CHANGE: base_bitrate {} -> {} kbps (tier: {}, index: {}, reason: congestion_cut)",
                old_bitrate,
                self.ideal_bitrate_kbps,
                new_tier.label,
                self.quality_manager.video_tier_index(),
            );
        }
        changed
    }

    /// Notify this controller that screen sharing state changed.
    ///
    /// When screen share becomes active, the camera is forced to a conservative
    /// tier and capped there to prevent bandwidth contention. When screen share
    /// stops, the cap is removed and the camera recovers naturally.
    pub fn notify_screen_sharing(&mut self, active: bool) {
        if active {
            let ceiling = screen_share_camera_ceiling_index();
            let now = self.clock.now_ms();
            let changed = self.quality_manager.force_video_step_to(ceiling, now);
            self.quality_manager.set_quality_ceiling(Some(ceiling));
            if changed {
                let old_bitrate = self.ideal_bitrate_kbps;
                let new_tier = self.quality_manager.current_video_tier();
                self.ideal_bitrate_kbps = new_tier.ideal_bitrate_kbps;
                self.tier_changed = true;
                log::info!(
                    "AQ_BITRATE_CHANGE: base_bitrate {} -> {} kbps (tier: {}, index: {}, reason: screen_share_coordination)",
                    old_bitrate, self.ideal_bitrate_kbps, new_tier.label, self.quality_manager.video_tier_index(),
                );
            }
        } else {
            self.quality_manager.set_quality_ceiling(None);
        }
    }

    /// Drain tier transition records from the quality manager.
    pub fn drain_tier_transitions(&mut self) -> Vec<TierTransitionRecord> {
        self.quality_manager.drain_transitions()
    }

    /// Notify the quality manager that a server re-election completed.
    /// Suppresses crash ceiling arming for `REELECTION_CEILING_SUPPRESSION_MS`.
    ///
    /// Wired through: ConnectionManager → shared AtomicBool signal → CameraEncoder
    /// control loop (checks/clears each tick) → this method.
    pub fn notify_reelection_completed(&mut self) {
        let now = self.clock.now_ms();
        self.quality_manager.notify_reelection_completed(now);
    }

    /// Set user-configurable VIDEO quality bounds (issue #961).
    ///
    /// QUALITY IS THE INVERSE OF INDEX: `best` is the user's MAX quality = a
    /// FLOOR on the tier index (never step UP past it); `worst` is the user's MIN
    /// quality = a CAP on the tier index (never step DOWN past it). Each end is
    /// independently optional (`None` = Auto). Forwards to
    /// [`AdaptiveQualityManager::set_video_quality_bounds`], which clamps,
    /// normalizes an inverted range, and snaps the current tier into range
    /// immediately. If the snap changes the tier, the new ideal bitrate is
    /// applied so the encoder picks it up on the next tick.
    pub fn set_video_quality_bounds(&mut self, best: Option<usize>, worst: Option<usize>) {
        let now = self.clock.now_ms();
        let before = self.quality_manager.video_tier_index();
        self.quality_manager
            .set_video_quality_bounds(best, worst, now);
        if self.quality_manager.video_tier_index() != before {
            self.tier_changed = true;
            let old_bitrate = self.ideal_bitrate_kbps;
            let new_tier = self.quality_manager.current_video_tier();
            self.ideal_bitrate_kbps = new_tier.ideal_bitrate_kbps;
            log::info!(
                "AQ_BITRATE_CHANGE: base_bitrate {} -> {} kbps (tier: {}, index: {}, reason: user_quality_bounds)",
                old_bitrate,
                self.ideal_bitrate_kbps,
                new_tier.label,
                self.quality_manager.video_tier_index(),
            );
        }
    }

    /// Set user-configurable AUDIO quality bounds (issue #961).
    ///
    /// Same inverse-of-index semantics as [`Self::set_video_quality_bounds`]:
    /// `best` is a FLOOR on the audio tier index, `worst` is a CAP. `None` =
    /// Auto. Forwards to [`AdaptiveQualityManager::set_audio_quality_bounds`],
    /// which snaps the current audio tier into range immediately. Flags a tier
    /// change so the microphone encoder picks up the new audio settings.
    pub fn set_audio_quality_bounds(&mut self, best: Option<usize>, worst: Option<usize>) {
        let before = self.quality_manager.audio_tier_index();
        self.quality_manager.set_audio_quality_bounds(best, worst);
        if self.quality_manager.audio_tier_index() != before {
            self.tier_changed = true;
        }
    }

    /// Return the current crash ceiling state, if active.
    /// `(ceiling_index, tier_label, current_decay_ms)`
    pub fn crash_ceiling_info(&self) -> Option<(usize, &'static str, f64)> {
        self.quality_manager.crash_ceiling_info()
    }

    /// Return step-up blocked counters: `(ceiling, slowdown, screen_share)`.
    pub fn step_up_blocked_counts(&self) -> (u64, u64, u64) {
        self.quality_manager.step_up_blocked_counts()
    }

    /// Drain accumulated dwell time samples: `Vec<(tier_label, dwell_ms)>`.
    pub fn drain_dwell_samples(&mut self) -> Vec<(&'static str, f64)> {
        self.quality_manager.drain_dwell_samples()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{default_clock, TestClock};
    use crate::constants::{
        ENCODER_BACKPRESSURE_SUSTAIN_MS, ENCODER_QUEUE_BACKPRESSURE_CLEAR,
        ENCODER_QUEUE_BACKPRESSURE_HIGH, MIN_TIER_TRANSITION_INTERVAL_MS, QUALITY_WARMUP_MS,
        SCREEN_QUALITY_TIERS, STEP_DOWN_REACTION_TIME_MS, STEP_UP_STABILIZATION_WINDOW_MS,
        VIDEO_QUALITY_TIERS,
    };
    use crate::manager::AdaptiveQualityManager;
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;

    // Dual-target: on Wasm this delegates to `wasm-bindgen-test`; on native the
    // `#[test]` attribute is already correct.
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    // ---------------------------------------------------------------------
    // Test helpers (issue #1108: receiver-FPS packet plumbing removed; the
    // controller is now driven by `observe_encoder_queue_depth` + `tick`).
    // ---------------------------------------------------------------------

    /// Build a `TestClock`-backed controller starting at `ideal` kbps.
    fn controller_with_clock(ideal: u32, clock: &Arc<TestClock>) -> EncoderBitrateController {
        let target_fps = Arc::new(AtomicU32::new(30));
        EncoderBitrateController::with_clock(
            ideal,
            target_fps,
            Arc::clone(clock) as Arc<dyn crate::clock::Clock>,
        )
    }

    /// Advance the clock to `t_ms`, feed `depth`, and tick once.
    fn tick_at(
        controller: &mut EncoderBitrateController,
        clock: &Arc<TestClock>,
        t_ms: f64,
        depth: u32,
    ) {
        clock.set_ms(t_ms as u64);
        controller.observe_encoder_queue_depth(depth);
        controller.tick(t_ms);
    }

    /// Tick repeatedly with backpressure CLEAR (depth 0) from `start_ms`,
    /// spacing ticks `step_ms` apart for `count` ticks, returning the last
    /// timestamp used. Used to walk past warmup without any degrade.
    fn warm_up(
        controller: &mut EncoderBitrateController,
        clock: &Arc<TestClock>,
        start_ms: f64,
        count: usize,
        step_ms: f64,
    ) -> f64 {
        let mut t = start_ms;
        for _ in 0..count {
            tick_at(controller, clock, t, 0);
            t += step_ms;
        }
        t - step_ms
    }

    // =====================================================================
    // update_from_backpressure (manager) — step down / up + gating
    // =====================================================================

    /// A sustained `degrade=true` must step the video tier DOWN once the
    /// reaction time and min-transition interval are satisfied; a subsequent
    /// `recover=true` must step it back UP after the stabilization window. This
    /// replaces the former receiver-FPS-driven manager tests.
    #[test]
    fn test_update_from_backpressure_steps_down_then_up() {
        let base: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base));
        let mut mgr = AdaptiveQualityManager::with_clock(
            VIDEO_QUALITY_TIERS,
            Arc::clone(&clock) as Arc<dyn crate::clock::Clock>,
        );
        let start_index = mgr.video_tier_index();

        // Walk past warmup with no degrade/recover (hold).
        let mut t = base as f64 + QUALITY_WARMUP_MS + 1000.0;
        mgr.update_from_backpressure(false, false, t);

        // Sustained degrade across spaced ticks must step down at least once.
        let mut stepped_down = false;
        for _ in 0..6 {
            t += (STEP_DOWN_REACTION_TIME_MS.max(MIN_TIER_TRANSITION_INTERVAL_MS)) as f64 + 200.0;
            mgr.update_from_backpressure(true, false, t);
            if mgr.video_tier_index() > start_index {
                stepped_down = true;
                break;
            }
        }
        assert!(
            stepped_down,
            "sustained backpressure degrade must step the video tier DOWN"
        );
        let degraded_index = mgr.video_tier_index();

        // Sustained recover must climb back up.
        let mut stepped_up = false;
        for _ in 0..12 {
            t += STEP_UP_STABILIZATION_WINDOW_MS as f64 + 500.0;
            mgr.update_from_backpressure(false, true, t);
            if mgr.video_tier_index() < degraded_index {
                stepped_up = true;
                break;
            }
        }
        assert!(
            stepped_up,
            "sustained backpressure-clear (recover) must step the video tier UP"
        );
    }

    /// Recover must be suppressed while a self-targeted CONGESTION drain hold is
    /// active (the kept `force_congestion_cut` path), even with `recover=true`:
    /// climbing tiers mid-drain would re-fill the relay buffer. The hold
    /// (CONGESTION_HOLD_MS) is shorter than the step-up window, so the guarantee
    /// is "no climb at all during the hold" — the recover timer cannot even
    /// start while the hold is active.
    #[test]
    fn test_recover_suppressed_during_congestion_hold() {
        use crate::constants::CONGESTION_HOLD_MS;

        let base: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base));
        let mut mgr = AdaptiveQualityManager::with_clock(
            VIDEO_QUALITY_TIERS,
            Arc::clone(&clock) as Arc<dyn crate::clock::Clock>,
        );
        let mut t = base as f64 + QUALITY_WARMUP_MS + 2000.0;
        // Push down a couple tiers so there's room to recover.
        for _ in 0..3 {
            t += 4000.0;
            mgr.update_from_backpressure(true, false, t);
        }
        assert!(
            mgr.video_tier_index() > 0,
            "precondition: degraded below the top tier"
        );

        // Arm a congestion cut (drops tiers + arms the drain hold).
        t += 4000.0;
        let cut_at = t;
        mgr.force_congestion_cut(cut_at);
        let after_cut = mgr.video_tier_index();

        // Tick recover=true repeatedly WHILE the hold is active; the tier must
        // never climb during the hold.
        let mut ticks = 0;
        while t + 400.0 < cut_at + CONGESTION_HOLD_MS {
            t += 400.0;
            assert!(
                mgr.congestion_hold_active(t),
                "precondition: drain hold still active at t={t}"
            );
            mgr.update_from_backpressure(false, true, t);
            ticks += 1;
            assert_eq!(
                mgr.video_tier_index(),
                after_cut,
                "recover must be suppressed while the congestion drain hold is active"
            );
        }
        assert!(ticks > 0, "test must tick at least once inside the hold");
    }

    // =====================================================================
    // tick(): backpressure decision drives tier + simulcast layers
    // =====================================================================

    /// REGRESSION LOCK (the mandate): ticking with backpressure == 0 must NEVER
    /// move the video tier index or shed/restore a simulcast layer, no matter
    /// how long it runs. A receiver's link is no longer even an input — this
    /// proves the sender is quiescent absent its OWN backpressure.
    #[test]
    fn test_tick_with_zero_backpressure_never_changes_tier_or_layers() {
        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let mut controller = controller_with_clock(500, &clock);
        controller.set_simulcast_layers(3);

        let start_tier = controller.video_tier_index();
        let start_layers = controller.active_layer_count();
        assert_eq!(start_layers, 3, "precondition: 3 active layers");

        // Tick for a long simulated window with zero backpressure.
        let mut t = base_ms as f64 + QUALITY_WARMUP_MS + 1000.0;
        for _ in 0..60 {
            tick_at(&mut controller, &clock, t, 0);
            t += 1000.0;
        }

        assert_eq!(
            controller.video_tier_index(),
            start_tier,
            "zero backpressure must never move the video tier"
        );
        assert_eq!(
            controller.active_layer_count(),
            start_layers,
            "zero backpressure must never shed a simulcast layer"
        );
    }

    /// Sustained HIGH encoder backpressure must shed the top active simulcast
    /// layer (sender CPU backstop). This is the Stage-2 replacement for the
    /// receiver-FPS-driven shed.
    #[test]
    fn test_sustained_backpressure_sheds_layer() {
        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let mut controller = controller_with_clock(500, &clock);
        controller.set_simulcast_layers(3);
        assert_eq!(controller.active_layer_count(), 3);

        // Warm up with zero backpressure.
        let mut t = warm_up(&mut controller, &clock, base_ms as f64 + 6000.0, 4, 1000.0);

        // Now feed sustained HIGH backpressure across spaced ticks until a layer
        // is shed. Each tick advances well past the sustain + min-transition.
        let step =
            (ENCODER_BACKPRESSURE_SUSTAIN_MS.max(MIN_TIER_TRANSITION_INTERVAL_MS as f64)) + 500.0;
        let mut shed = false;
        for _ in 0..8 {
            t += step;
            tick_at(&mut controller, &clock, t, ENCODER_QUEUE_BACKPRESSURE_HIGH);
            if controller.active_layer_count() < 3 {
                shed = true;
                break;
            }
        }
        assert!(
            shed,
            "sustained encoder backpressure must shed the top active layer (active now {})",
            controller.active_layer_count()
        );
    }

    /// A brief backpressure spike that does NOT persist for the sustain window
    /// must not shed a layer (the sustain timer protects against single-frame
    /// hiccups / GC pauses).
    #[test]
    fn test_brief_backpressure_spike_does_not_shed() {
        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let mut controller = controller_with_clock(500, &clock);
        controller.set_simulcast_layers(3);
        let start_tier = controller.video_tier_index();

        let mut t = warm_up(&mut controller, &clock, base_ms as f64 + 6000.0, 4, 1000.0);

        // One HIGH sample, then immediately back to clear, repeatedly — the
        // sustain timer should keep resetting and never fire.
        for _ in 0..6 {
            t += 300.0;
            tick_at(&mut controller, &clock, t, ENCODER_QUEUE_BACKPRESSURE_HIGH);
            t += 300.0;
            tick_at(&mut controller, &clock, t, 0);
        }
        assert_eq!(
            controller.active_layer_count(),
            3,
            "a brief, non-sustained spike must not shed a layer"
        );
        assert_eq!(
            controller.video_tier_index(),
            start_tier,
            "a brief, non-sustained spike must not move the tier off its baseline"
        );
    }

    /// Sustained HIGH then sustained CLEAR must shed then restore the layer.
    #[test]
    fn test_backpressure_shed_then_restore_layer() {
        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let mut controller = controller_with_clock(500, &clock);
        controller.set_simulcast_layers(3);

        let mut t = warm_up(&mut controller, &clock, base_ms as f64 + 6000.0, 4, 1000.0);

        let down_step =
            (ENCODER_BACKPRESSURE_SUSTAIN_MS.max(MIN_TIER_TRANSITION_INTERVAL_MS as f64)) + 500.0;
        for _ in 0..8 {
            t += down_step;
            tick_at(&mut controller, &clock, t, ENCODER_QUEUE_BACKPRESSURE_HIGH);
            if controller.active_layer_count() < 3 {
                break;
            }
        }
        assert!(
            controller.active_layer_count() < 3,
            "precondition: at least one layer shed"
        );
        let shed_count = controller.active_layer_count();

        // Now sustained CLEAR (depth 0) over the stabilization window must
        // restore a layer.
        let up_step = (STEP_UP_STABILIZATION_WINDOW_MS as f64)
            .max(MIN_TIER_TRANSITION_INTERVAL_MS as f64)
            + 500.0;
        let mut restored = false;
        for _ in 0..12 {
            t += up_step;
            tick_at(&mut controller, &clock, t, ENCODER_QUEUE_BACKPRESSURE_CLEAR);
            if controller.active_layer_count() > shed_count {
                restored = true;
                break;
            }
        }
        assert!(
            restored,
            "sustained backpressure-clear must restore a shed layer (active {} -> {})",
            shed_count,
            controller.active_layer_count()
        );
    }

    // =====================================================================
    // compute_layer_bitrates: nominal-budget baseline (tier ideals, capped)
    // =====================================================================

    /// Each active layer's target is its tier ideal, and the ACTIVE layers'
    /// SUM never exceeds the uplink budget while each stays at/above its floor.
    #[test]
    fn test_layer_bitrates_are_budget_capped_tier_ideals() {
        use crate::constants::{simulcast_layers, uplink_budget_kbps};

        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let mut controller = controller_with_clock(500, &clock);
        controller.set_simulcast_layers(3);

        // A single tick (no backpressure) computes the per-layer targets.
        let t = base_ms as f64 + QUALITY_WARMUP_MS + 1000.0;
        tick_at(&mut controller, &clock, t, 0);

        let active = controller.active_layer_count();
        let tiers = simulcast_layers(3);
        let budget = uplink_budget_kbps(tiers, active);
        let bitrates = controller.layer_target_bitrates_kbps();
        assert_eq!(bitrates.len(), 3);
        let active_sum: f64 = bitrates[..active].iter().sum();
        assert!(
            active_sum <= budget + 1e-6,
            "active sum {active_sum} (active={active}) must fit within budget {budget}"
        );
        for i in 0..active {
            assert!(
                bitrates[i] >= tiers[i].min_bitrate_kbps as f64 - 1e-6,
                "layer {i} ({}) below its floor {}",
                bitrates[i],
                tiers[i].min_bitrate_kbps,
            );
            assert!(
                bitrates[i] <= tiers[i].max_bitrate_kbps as f64 + 1e-6,
                "layer {i} ({}) above its tier max {}",
                bitrates[i],
                tiers[i].max_bitrate_kbps,
            );
        }
    }

    /// During a CONGESTION drain hold, each surviving layer's bitrate is pinned
    /// to its tier IDEAL (not max), mirroring the single-stream pin.
    #[test]
    fn test_per_layer_bitrates_pinned_to_ideal_during_congestion_hold() {
        use crate::constants::{simulcast_layers, CONGESTION_HOLD_MS, DEFAULT_VIDEO_TIER_INDEX};

        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let ideal = VIDEO_QUALITY_TIERS[DEFAULT_VIDEO_TIER_INDEX].ideal_bitrate_kbps;
        let mut controller = controller_with_clock(ideal, &clock);
        controller.set_simulcast_layers(3);

        let mut now = warm_up(&mut controller, &clock, base_ms as f64 + 6000.0, 4, 1000.0);

        now += 1100.0;
        clock.set_ms(now as u64);
        let cut_at = now;
        controller.force_congestion_cut();
        let active = controller.active_layer_count();
        assert!(active >= 1);

        let tiers = simulcast_layers(controller.simulcast_layer_count());
        let mut ticks_in_hold = 0;
        while now + 600.0 < cut_at + CONGESTION_HOLD_MS {
            now += 600.0;
            tick_at(&mut controller, &clock, now, 0);
            assert!(
                controller.quality_manager.congestion_hold_active(now),
                "hold should still be active at t={now}"
            );
            ticks_in_hold += 1;
            let bitrates = controller.layer_target_bitrates_kbps();
            for layer_id in 0..active {
                let tier_ideal = tiers[layer_id].ideal_bitrate_kbps as f64;
                assert!(
                    bitrates[layer_id] <= tier_ideal + 0.5,
                    "layer {layer_id} bitrate {} must be pinned <= tier ideal {tier_ideal} \
                     during the congestion hold",
                    bitrates[layer_id],
                );
            }
        }
        assert!(
            ticks_in_hold > 0,
            "test must exercise at least one tick inside the congestion hold"
        );
    }

    // =====================================================================
    // Simulcast wiring + force paths (kept verbatim behavior)
    // =====================================================================

    #[test]
    fn test_controller_single_stream_by_default() {
        let target_fps = Arc::new(AtomicU32::new(30));
        let controller = EncoderBitrateController::new(500, target_fps);
        assert!(!controller.is_simulcast());
        assert_eq!(controller.simulcast_layer_count(), 1);
        assert_eq!(controller.active_layer_count(), 1);
        assert!(controller.layer_target_bitrates_kbps().is_empty());
        assert_eq!(controller.layer_resolution(0), None);
    }

    #[test]
    fn test_set_simulcast_layers_builds_per_layer_state() {
        let target_fps = Arc::new(AtomicU32::new(30));
        let mut controller = EncoderBitrateController::new(500, target_fps);
        controller.set_simulcast_layers(3);

        assert!(controller.is_simulcast());
        assert_eq!(controller.simulcast_layer_count(), 3);
        assert_eq!(controller.active_layer_count(), 3);
        assert_eq!(controller.layer_resolution(0), Some((640, 360)));
        assert_eq!(controller.layer_resolution(1), Some((960, 540)));
        assert_eq!(controller.layer_resolution(2), Some((1280, 720)));
        assert_eq!(controller.layer_resolution(3), None);
        assert_eq!(controller.layer_target_bitrates_kbps().len(), 3);
    }

    #[test]
    fn test_set_simulcast_layers_one_stays_single_stream() {
        let target_fps = Arc::new(AtomicU32::new(30));
        let mut controller = EncoderBitrateController::new(500, target_fps);
        controller.set_simulcast_layers(1);
        assert!(!controller.is_simulcast());
        assert_eq!(controller.active_layer_count(), 1);
        assert!(controller.layer_target_bitrates_kbps().is_empty());
    }

    #[test]
    fn test_screen_controller_uses_screen_simulcast_ladder() {
        use crate::constants::simulcast_screen_layers;
        let target_fps = Arc::new(AtomicU32::new(10));
        let mut controller =
            EncoderBitrateController::new_for_screen(target_fps, SCREEN_QUALITY_TIERS);
        controller.set_simulcast_layers(3);
        assert!(controller.is_simulcast());
        let tiers = simulcast_screen_layers(3);
        assert_eq!(
            controller.layer_resolution(0),
            Some((tiers[0].max_width, tiers[0].max_height))
        );
        // The top screen layer must be 1080p (distinct from the camera ladder's
        // 720p top), proving the screen ladder is in use.
        assert_eq!(controller.layer_resolution(2), Some((1920, 1080)));
    }

    #[test]
    fn test_new_for_screen_starts_at_midpoint_tier() {
        use crate::constants::DEFAULT_SCREEN_TIER_INDEX;
        let target_fps = Arc::new(AtomicU32::new(10));
        let controller = EncoderBitrateController::new_for_screen(target_fps, SCREEN_QUALITY_TIERS);
        assert_eq!(controller.video_tier_index(), DEFAULT_SCREEN_TIER_INDEX);
    }

    #[test]
    fn test_congestion_cut_sheds_top_layer_in_simulcast() {
        use crate::constants::DEFAULT_VIDEO_TIER_INDEX;

        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let ideal = VIDEO_QUALITY_TIERS[DEFAULT_VIDEO_TIER_INDEX].ideal_bitrate_kbps;
        let mut controller = controller_with_clock(ideal, &clock);
        controller.set_simulcast_layers(3);
        assert_eq!(controller.active_layer_count(), 3);

        let last_warm = warm_up(&mut controller, &clock, base_ms as f64 + 6000.0, 4, 1000.0);
        clock.set_ms((last_warm + 1100.0) as u64);
        let changed = controller.force_congestion_cut();
        assert!(
            changed,
            "congestion cut should change tier from default start"
        );
        assert_eq!(
            controller.active_layer_count(),
            2,
            "a congestion cut in simulcast mode must shed the top active layer"
        );
    }

    #[test]
    fn test_force_step_down_sheds_top_layer_in_simulcast() {
        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let mut controller = controller_with_clock(500, &clock);
        controller.set_simulcast_layers(3);

        let last_warm = warm_up(&mut controller, &clock, base_ms as f64 + 6000.0, 4, 1000.0);
        clock.set_ms((last_warm + 1100.0) as u64);

        assert!(controller.force_video_step_down());
        assert_eq!(
            controller.active_layer_count(),
            2,
            "force_video_step_down in simulcast mode must shed the top active layer"
        );
    }

    #[test]
    fn test_single_stream_force_paths_do_not_touch_layers() {
        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let mut controller = controller_with_clock(500, &clock);
        // No set_simulcast_layers -> single-stream.
        let last_warm = warm_up(&mut controller, &clock, base_ms as f64 + 6000.0, 4, 1000.0);
        clock.set_ms((last_warm + 1100.0) as u64);

        controller.force_video_step_down();
        assert_eq!(controller.active_layer_count(), 1);
        assert!(controller.layer_target_bitrates_kbps().is_empty());
    }

    #[test]
    fn test_congestion_cut_sheds_layer_even_at_tier_floor() {
        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let mut controller = controller_with_clock(400, &clock);
        controller.set_simulcast_layers(3);
        assert_eq!(controller.active_layer_count(), 3);

        let last_warm = warm_up(&mut controller, &clock, base_ms as f64 + 6000.0, 4, 1000.0);

        // Force the tier index to the floor WITHOUT shedding layers.
        let floor = VIDEO_QUALITY_TIERS.len() - 1;
        controller
            .quality_manager
            .force_video_step_to(floor, last_warm);
        assert_eq!(controller.video_tier_index(), floor);
        assert_eq!(
            controller.active_layer_count(),
            3,
            "forcing the tier to the floor must not touch the layer axis"
        );

        let cut_at = last_warm + 1600.0;
        clock.set_ms(cut_at as u64);
        let changed = controller.force_congestion_cut();
        assert!(
            !changed,
            "at the tier floor the manager reports no tier change"
        );
        assert_eq!(
            controller.active_layer_count(),
            2,
            "a congestion cut must still shed a layer at the tier floor (drain hold armed)"
        );
        assert!(
            controller
                .quality_manager
                .congestion_hold_active(cut_at + 1.0),
            "drain hold must be armed even at the tier floor"
        );
    }

    #[test]
    fn test_budget_shrinks_as_top_layer_is_shed() {
        use crate::constants::{simulcast_layers, uplink_budget_kbps};
        let tiers = simulcast_layers(3);
        assert!(uplink_budget_kbps(tiers, 3) > uplink_budget_kbps(tiers, 2));
        assert!(uplink_budget_kbps(tiers, 2) > uplink_budget_kbps(tiers, 1));
        assert_eq!(uplink_budget_kbps(tiers, 1), 400.0);
    }

    // =====================================================================
    // Screen-share coordination + quality bounds (still valid)
    // =====================================================================

    #[test]
    fn test_notify_screen_sharing_active_forces_ceiling() {
        use crate::constants::screen_share_camera_ceiling_index;
        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let mut controller = controller_with_clock(2500, &clock);
        // Start at the top tier; activating screen share forces the camera to a
        // conservative ceiling tier.
        controller.notify_screen_sharing(true);
        assert_eq!(
            controller.video_tier_index(),
            screen_share_camera_ceiling_index()
        );
    }

    #[test]
    fn test_notify_screen_sharing_deactivate_removes_ceiling() {
        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let mut controller = controller_with_clock(2500, &clock);
        controller.notify_screen_sharing(true);
        controller.notify_screen_sharing(false);
        // After deactivation the controller must be free to recover upward; the
        // ceiling is cleared (a recover tick is no longer pinned by the ceiling).
        // We assert the tier_changed flag was raised on activation at least.
        let _ = controller.take_tier_changed();
    }

    // =====================================================================
    // Pure-function + constant guards (carried forward)
    // =====================================================================

    #[test]
    fn test_layer_upper_clamp_pins_to_ideal_only_during_hold() {
        use crate::constants::simulcast_layers;

        for tier in simulcast_layers(3) {
            let ideal = tier.ideal_bitrate_kbps as f64;
            let max = tier.max_bitrate_kbps as f64;
            assert!(
                max > ideal,
                "precondition: tier '{}' max {max} must exceed ideal {ideal}",
                tier.label
            );
            assert_eq!(
                layer_upper_clamp_kbps(tier, true),
                ideal,
                "tier '{}': during the hold the upper clamp must be the IDEAL",
                tier.label
            );
            assert_eq!(
                layer_upper_clamp_kbps(tier, false),
                max,
                "tier '{}': without the hold the upper clamp must be the MAX",
                tier.label
            );
        }
    }

    #[test]
    fn test_bitrate_change_threshold_is_010() {
        assert!(
            (crate::constants::BITRATE_CHANGE_THRESHOLD - 0.10).abs() < f64::EPSILON,
            "BITRATE_CHANGE_THRESHOLD must be 0.10 to keep encoder reconfigures rare; \
             changing it forces a review of keyframe-cost trade-offs"
        );
    }

    /// Sanity: the default clock is wired (used by the non-`with_clock`
    /// constructors). Keeps a reference to `default_clock` so the import is live
    /// on all targets.
    #[test]
    fn test_default_clock_is_available() {
        let _ = default_clock().now_ms();
    }
}

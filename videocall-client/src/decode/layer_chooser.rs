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

//! Receiver-driven per-peer simulcast layer chooser (issue #989, Phase 2).
//!
//! For each remote VIDEO source the local client decodes, this module decides
//! which simulcast layer THIS receiver's OWN downlink can sustain, and adapts it
//! continuously and independently of the sender. A congested receiver pulls a
//! lower layer for the peers it struggles with; a receiver with headroom climbs
//! higher. The decision is purely local: it never touches the sender's encoder
//! and never affects what other receivers get.
//!
//! ## Why this is a separate, pure module
//!
//! The decision logic is pure arithmetic over per-peer receive signals, so it
//! lives here free of `web_sys` / wasm so it can be host-unit-tested
//! exhaustively (the hazards of a flapping or runaway selector on a real-time
//! call are exactly what the project's Change Impact Policy warns about). The
//! wasm-only glue — reading live per-peer loss/PLI rates and sending the
//! resulting `LAYER_PREFERENCE` packet — lives in `peer_decode_manager.rs` and
//! `video_call_client.rs`, which drive this state machine.
//!
//! ## Signals (THIS receiver's downlink for THIS source)
//!
//! The receive path tracks per peer-stream loss/PLI rates, which the client
//! folds into the chooser once per **monitor tick — every 5s**
//! (`connection.rs`'s `heartbeat_monitor = Interval::new(5000, …)` drives
//! `run_peer_monitor` → `tick_layer_choosers`). Each `DownlinkSample` therefore
//! represents ~5s of reception, NOT ~1s; the constants below are tuned for that
//! 5s cadence (e.g. `STEP_UP_CLEAN_WINDOWS = 3` ≈ 15s of sustained headroom).
//! The two per-window rate signals are:
//!   * `loss_per_sec` — packets that shifted off the reorder window unseen.
//!     Direct evidence the downlink is dropping this source's video.
//!   * `kf_per_sec` — keyframe-requests (PLI) this receiver emitted for the
//!     source. A receiver that cannot keep up freezes and storms PLIs, so a
//!     sustained PLI rate is a strong "can't sustain this layer" signal.
//!
//! Both rise under THIS receiver's congestion regardless of the sender's state,
//! which is exactly the property the feature requires. Throughput/decode-keep-up
//! is captured implicitly: a layer the downlink cannot carry manifests as loss
//! and PLIs; sustained CLEAN windows are the headroom signal that licenses a
//! step up.
//!
//! ## Availability learning
//!
//! The relay does not advertise which layers a source produces, so availability
//! is learned empirically: [`LayerAvailability`] records the distinct
//! `simulcast_layer_id`s observed from a source within a rolling window. The
//! chooser never targets a layer above the highest observed-available one.
//!
//! ## Hysteresis (anti-flap)
//!
//! Mirroring the spirit of the sender AQ (responsive down, conservative up):
//!   * **Down** is fast — a single bad window over threshold steps down (drop
//!     immediately when loss/PLI spikes).
//!   * **Up** requires `STEP_UP_CLEAN_WINDOWS` consecutive clean windows AND a
//!     dwell of at least [`LAYER_STEP_UP_DWELL_MS`] since the last change, so a
//!     brief lull cannot bait an immediate re-climb into a layer the downlink
//!     just proved it cannot carry.
//!
//! ## P4 seam (user receive thresholds)
//!
//! [`LayerChooser::choose`] returns the *raw* desired layer the downlink can
//! sustain. P4 will clamp that into `[user_min, user_max]` at the call site
//! (see [`clamp_to_user_range`]) without changing this module's logic. The clamp
//! is per-(peer, [`PrefMediaKind`]), so a user can cap screen and camera
//! independently.

/// Media kind a layer preference / chooser applies to (issue #989, Phase 3).
///
/// Camera VIDEO, SCREEN-share, and AUDIO of the same peer are independent
/// streams, each with its own availability, downlink health, and chosen layer.
/// This enum keys the per-(peer, kind) chooser state on the receiver and the
/// per-(peer, kind) entry in the `LAYER_PREFERENCE` packet. The discriminants
/// match the wire `PacketWrapper.MediaKind` / proto `EntryMediaKind`
/// (VIDEO=1, AUDIO=2, SCREEN=3) so mapping to the wire is a trivial cast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PrefMediaKind {
    /// Camera video (`MediaKind::VIDEO` == 1).
    Video = 1,
    /// Microphone audio (`MediaKind::AUDIO` == 2).
    Audio = 2,
    /// Screen share (`MediaKind::SCREEN` == 3).
    Screen = 3,
}

impl PrefMediaKind {
    /// The wire discriminant for the proto `EntryMediaKind` / `MediaKind`.
    pub fn wire_value(self) -> i32 {
        self as i32
    }
}

/// Consecutive clean (sub-threshold) windows required before a step UP.
///
/// Conservative on the way up: the downlink must prove sustained headroom, not
/// just one lucky window, before we ask for a costlier layer. The chooser is fed
/// once per 5s monitor tick (see the module-level "Signals" note), so three
/// clean windows ≈ 15s of clean reception before each rung climb.
pub const STEP_UP_CLEAN_WINDOWS: u32 = 3;

/// Minimum dwell (ms) at the current layer before a step UP is allowed.
///
/// Belt-and-suspenders with [`STEP_UP_CLEAN_WINDOWS`]: even if windows roll
/// fast, we will not climb again until this much wall-clock has elapsed since
/// the last layer change, preventing rapid oscillation on a marginal link.
pub const LAYER_STEP_UP_DWELL_MS: u64 = 3000;

/// Loss rate (lost packets/sec) at or above which the chooser steps DOWN.
///
/// Sustained loss means the downlink is dropping this source's video; a lower
/// layer is cheaper and more resilient. Tuned conservatively so ordinary jitter
/// (the reorder window already tolerates reordering) does not trigger a drop.
pub const LOSS_STEP_DOWN_PER_SEC: f64 = 5.0;

/// Loss rate below which a window counts as "clean" for step-up accounting.
///
/// Strictly below the step-down threshold to create a neutral band
/// `[LOSS_CLEAN_PER_SEC, LOSS_STEP_DOWN_PER_SEC)` where the chooser neither
/// climbs nor drops — the hysteresis dead-zone that prevents flapping right at
/// the boundary.
pub const LOSS_CLEAN_PER_SEC: f64 = 1.0;

/// Keyframe-request (PLI) rate (per sec) at or above which the chooser steps
/// DOWN. A receiver that cannot keep up freezes and storms PLIs; treat that as
/// downlink congestion for this source independent of actual sequence loss.
pub const PLI_STEP_DOWN_PER_SEC: f64 = 2.0;

/// PLI rate below which a window counts as "clean" for step-up accounting.
pub const PLI_CLEAN_PER_SEC: f64 = 0.5;

// --- Sticky-low convergence (issue #1179) ---
//
// The plain fast-down / conservative-up loop above converges to a RESTING point
// that is one rung BELOW the highest sustainable layer on a chronically marginal
// link: every time the streak finally climbs back to the top, the next congested
// window knocks it down again, so the receiver yo-yos and ~18% of the call is
// spent decoding (and advertising) a layer lower than the link can actually
// carry. The sticky-low state machine fixes the resting point: once congestion
// is *chronic* (not a one-off spike), the chooser latches a `sticky_floor` and
// refuses to climb back above it until the link proves sustained recovery, then
// raises the floor exactly ONE rung at a time. This makes the resting point the
// floor itself (stable) instead of "floor + 1, re-dropping forever" (yo-yo).

/// Number of congested windows (accumulated via the decaying congestion score)
/// that flips the chooser into the **sticky** state. One isolated congested
/// window must NOT stick (that is the normal fast-down's job); only a sustained
/// pattern latches a floor. With the 5s tick this is ~15s of repeated congestion.
pub const STICKY_CONGESTION_EVENTS: u32 = 3;

/// Saturation cap for the congestion score so a long bad stretch cannot bank
/// unbounded credit. Once sticky, the score is what `STICKY_RECOVERY_CLEAN_TICKS`
/// of clean windows must decay/earn against; capping it bounds how long a
/// recovered link is held down after an extended outage.
pub const STICKY_CONGESTION_SCORE_CAP: u32 = 6;

/// Consecutive clean windows required while sticky before the floor is raised by
/// ONE rung (the **cautious** recovery strategy, issue #1179). At the 5s monitor
/// tick this is ~60s of sustained-clean reception per rung — deliberately slow so
/// a chronically marginal link does not immediately re-attempt the layer that
/// keeps collapsing. Exposed as a named constant so a future bot-netsim sweep can
/// retune the recovery aggressiveness without touching the state-machine logic.
pub const STICKY_RECOVERY_CLEAN_TICKS: u32 = 12;

/// A single window's receive-health sample for one source (THIS receiver's
/// downlink), as produced by the receive-side sequence tracker on ~1s rollover.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DownlinkSample {
    /// Windowed packet-loss rate for this source (lost packets/sec).
    pub loss_per_sec: f64,
    /// Windowed keyframe-request (PLI) rate this receiver emitted (per sec).
    pub kf_per_sec: f64,
}

impl DownlinkSample {
    /// Over the step-DOWN threshold on either signal → the downlink cannot
    /// sustain the current layer.
    fn is_congested(&self) -> bool {
        self.loss_per_sec >= LOSS_STEP_DOWN_PER_SEC || self.kf_per_sec >= PLI_STEP_DOWN_PER_SEC
    }

    /// Under the CLEAN threshold on BOTH signals → this window contributes to
    /// the sustained-headroom evidence required for a step up.
    fn is_clean(&self) -> bool {
        self.loss_per_sec < LOSS_CLEAN_PER_SEC && self.kf_per_sec < PLI_CLEAN_PER_SEC
    }
}

/// Tracks which simulcast layers a source is currently producing, learned
/// empirically from observed `simulcast_layer_id`s (issue #989, Phase 2).
///
/// The relay does not advertise availability, so this is the only source of
/// truth for "which layers can I even ask for". Layers are observed within a
/// rolling window so that a source that stops emitting a top layer (its sender
/// AQ shed it, Phase 1) is eventually forgotten and we stop targeting it.
#[derive(Debug, Clone)]
pub struct LayerAvailability {
    /// Last-seen timestamp (ms) per observed layer id. A layer is "available"
    /// while its last observation is within [`Self::window_ms`].
    last_seen_ms: std::collections::HashMap<u32, u64>,
    /// How long (ms) an unobserved layer remains considered available.
    window_ms: u64,
}

impl LayerAvailability {
    /// Default availability window. Generous relative to the sender's frame
    /// cadence so a momentary gap (a few dropped frames, a keyframe-only lull)
    /// does not retract a layer, but short enough that a genuinely-shed top
    /// layer is forgotten within a few seconds.
    pub const DEFAULT_WINDOW_MS: u64 = 4000;

    pub fn new() -> Self {
        Self::with_window(Self::DEFAULT_WINDOW_MS)
    }

    pub fn with_window(window_ms: u64) -> Self {
        Self {
            last_seen_ms: std::collections::HashMap::new(),
            window_ms,
        }
    }

    /// Record that a packet tagged `layer_id` arrived from this source at `now`.
    pub fn observe(&mut self, layer_id: u32, now_ms: u64) {
        self.last_seen_ms.insert(layer_id, now_ms);
    }

    /// Highest layer id observed within the window as of `now`. Returns 0 when
    /// nothing has been observed recently (base-only / un-upgraded publisher),
    /// which is the bandwidth-safe default. Expired entries are pruned lazily.
    pub fn highest_available(&mut self, now_ms: u64) -> u32 {
        let window = self.window_ms;
        self.last_seen_ms
            .retain(|_, &mut seen| now_ms.saturating_sub(seen) <= window);
        self.last_seen_ms.keys().copied().max().unwrap_or(0)
    }
}

impl Default for LayerAvailability {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-peer layer-selection state machine (issue #989, Phase 2).
///
/// Owns the current selected layer plus the hysteresis bookkeeping (consecutive
/// clean-window count and the timestamp of the last change). One instance per
/// remote source; they are fully independent so a struggling source does not
/// affect a healthy one.
#[derive(Debug, Clone)]
pub struct LayerChooser {
    /// Currently selected layer for this source (== the decode guard value).
    ///
    /// NOTE this is the DECODE layer, not necessarily the advertised preference.
    /// While `constrained == false` the chooser tracks `highest_available` (decode
    /// the best available) and advertises NOTHING — see
    /// [`Self::desired_preference`].
    current: u32,
    /// Whether the chooser is ACTIVELY holding `current` BELOW the highest
    /// available layer because of observed congestion (issue #1079 M2).
    ///
    /// - `false` (the cold-start / healthy default): follow `highest_available`
    ///   (decode the best available) and advertise NO preference, so the relay
    ///   fail-open forwards every layer and a fresh/just-reconnected receiver
    ///   keeps full quality instead of being pinned to base while the old
    ///   conservative-up ramp climbed.
    /// - `true`: a congested window dropped us below the top; we hold `current`
    ///   and advertise it as a concrete `desired_layer` until sustained clean
    ///   windows climb us back to the top, at which point we clear this flag.
    constrained: bool,
    /// Consecutive clean windows accumulated toward a step up.
    clean_windows: u32,
    /// Timestamp (ms) of the last layer change, for the step-up dwell guard.
    last_change_ms: u64,

    // --- Sticky-low convergence (issue #1179) ---
    /// Decaying count of congested windows. Each congested window increments it
    /// (saturating at [`STICKY_CONGESTION_SCORE_CAP`]); each clean window decays
    /// it by one. A single isolated spike therefore decays away and never sticks;
    /// only sustained congestion accumulates to [`STICKY_CONGESTION_EVENTS`] and
    /// latches [`Self::sticky`]. Transparent + testable (integer score + decay)
    /// rather than a hidden timer.
    congestion_score: u32,
    /// `true` once congestion has been *chronic* (score reached
    /// [`STICKY_CONGESTION_EVENTS`]): the chooser then refuses to climb back above
    /// [`Self::sticky_floor`] until [`STICKY_RECOVERY_CLEAN_TICKS`] of sustained
    /// clean windows raise the floor one rung. Cleared only when the floor is
    /// raised all the way back to `highest_available` (full recovery).
    sticky: bool,
    /// The layer the sticky state is currently holding as its resting point. The
    /// chooser will not climb above this while `sticky`; cautious recovery raises
    /// it one rung per sustained-clean period until it reaches the top.
    sticky_floor: u32,
    /// Consecutive clean windows accumulated toward the NEXT one-rung floor raise
    /// while `sticky`. Reset to 0 by any non-clean (congested or neutral) window,
    /// so recovery requires an *uninterrupted* clean streak.
    recovery_clean_ticks: u32,
}

impl LayerChooser {
    /// Construct a chooser in the **unconstrained** state (issue #1079 M2): it
    /// decodes the highest available layer and advertises no preference until a
    /// congested window forces it to constrain down. This means a freshly-joined
    /// or just-reconnected receiver keeps full quality (relay forwards all
    /// layers) instead of pinning peers to base while a conservative-up ramp
    /// climbs — which caused a visible HD dip after every (re)connect.
    pub fn new(now_ms: u64) -> Self {
        Self {
            current: 0,
            constrained: false,
            clean_windows: 0,
            last_change_ms: now_ms,
            congestion_score: 0,
            sticky: false,
            sticky_floor: 0,
            recovery_clean_ticks: 0,
        }
    }

    /// The currently-selected DECODE layer (the decode-guard value).
    pub fn current(&self) -> u32 {
        self.current
    }

    /// The layer to advertise to the relay as a `LAYER_PREFERENCE`, or `None`
    /// when the chooser has no preference (issue #1079 M1/M2).
    ///
    /// Returns `Some(current)` ONLY while `constrained` — i.e. the chooser has
    /// actively decided to hold below the highest available layer because of
    /// congestion. Otherwise `None` ("no preference"): the caller omits the entry
    /// so the relay forwards ALL layers (fail-open) and the receiver decodes the
    /// best available. This is what prevents (a) cold-start pinning to base (M2)
    /// and (b) emitting a preference packet when there is nothing to constrain
    /// (M1 — an all-`None` map produces no entries).
    pub fn desired_preference(&self) -> Option<u32> {
        if self.constrained {
            Some(self.current)
        } else {
            None
        }
    }

    /// Fold one downlink window sample into the decision and return the new
    /// DECODE layer for this source.
    ///
    /// `highest_available` is the cap learned empirically by
    /// [`LayerAvailability`]; the chooser never targets above it.
    ///
    /// Behavior:
    ///   * **Unconstrained (default):** track `highest_available` (decode best),
    ///     advertise nothing. A congested window flips to constrained and steps
    ///     down from the top.
    ///   * **Down (fast):** a single congested window steps down one layer and
    ///     resets the clean-window counter (floored at base 0), and marks the
    ///     chooser constrained so it advertises the held layer.
    ///   * **Up (conservative):** requires [`STEP_UP_CLEAN_WINDOWS`] consecutive
    ///     clean windows AND [`LAYER_STEP_UP_DWELL_MS`] dwell since the last
    ///     change, then climbs one layer toward `highest_available`; reaching the
    ///     top clears `constrained` (back to no-preference / decode-best).
    ///   * **Neutral band:** a window that is neither congested nor clean holds
    ///     the layer and resets the clean streak.
    ///
    /// ## Sticky-low convergence (issue #1179)
    ///
    /// On a *chronically* marginal link the plain loop above resting-points one
    /// rung too high and yo-yos. Layered on top:
    ///   * **Score accounting (every window):** a decaying `congestion_score`
    ///     counts sustained congestion. Reaching [`STICKY_CONGESTION_EVENTS`]
    ///     latches the **sticky** state and pins `sticky_floor` to the current
    ///     (already-dropped) layer.
    ///   * **While sticky:** the conservative-up climb is capped at `sticky_floor`
    ///     (never climbs above it), and `constrained` is never cleared (we keep
    ///     advertising the held floor) — so the resting point is the *floor*, not
    ///     "floor + 1, re-dropping forever".
    ///   * **Cautious recovery:** [`STICKY_RECOVERY_CLEAN_TICKS`] of *uninterrupted*
    ///     clean windows raise `sticky_floor` by exactly ONE rung. When the floor
    ///     reaches `highest_available`, sticky clears and the chooser returns to
    ///     the normal decode-best / no-preference behavior.
    pub fn choose(&mut self, sample: DownlinkSample, highest_available: u32, now_ms: u64) -> u32 {
        // --- Score accounting (issue #1179) — runs in EVERY state/window. ---
        // A congested window banks credit (saturating at the cap); a clean window
        // decays it. Only sustained congestion accumulates to the latch threshold,
        // so a single isolated spike can never make the chooser sticky.
        // `just_latched` records the transition into sticky on THIS window so the
        // step-down branches below can pin `sticky_floor` to the layer we land on.
        let mut just_latched = false;
        if sample.is_congested() {
            self.congestion_score = (self.congestion_score + 1).min(STICKY_CONGESTION_SCORE_CAP);
            if !self.sticky && self.congestion_score >= STICKY_CONGESTION_EVENTS {
                // Latch: chronic congestion. The floor is pinned AFTER this
                // window's step-down (see the congested branches below) so it
                // reflects the proven-bad layer, not the pre-step one.
                self.sticky = true;
                just_latched = true;
            }
        } else if sample.is_clean() {
            self.congestion_score = self.congestion_score.saturating_sub(1);
        }

        // Unconstrained: simply track the highest available layer (decode best,
        // advertise nothing) until a congested window forces us to constrain.
        if !self.constrained {
            // A congested window drops us into the constrained state, stepping
            // down ONE layer from the current top.
            if sample.is_congested() {
                self.constrained = true;
                let from = self.current.min(highest_available);
                let dropped = from.saturating_sub(1);
                self.set_layer(dropped, now_ms);
                self.clean_windows = 0;
                // If this very window latched sticky (only when the cap is mis-set
                // ≤ events; defensive), pin the floor to where we land.
                if just_latched {
                    self.sticky_floor = self.current;
                    self.recovery_clean_ticks = 0;
                }
                return self.current;
            }
            // Otherwise follow the top (no constraint, full quality).
            self.set_layer(highest_available, now_ms);
            return self.current;
        }

        // --- Constrained state: the existing fast-down / conservative-up loop. ---

        // Availability can only shrink our target: if the top layer we were on
        // is no longer being produced, drop to the highest still-available one
        // immediately (it is no longer decodable anyway).
        if self.current > highest_available {
            self.set_layer(highest_available, now_ms);
            // A shrinking ceiling also drags the sticky floor down — we can never
            // hold a floor above what the source still produces.
            if self.sticky && self.sticky_floor > highest_available {
                self.sticky_floor = highest_available;
            }
            // If the ceiling itself collapsed to where we sit, we are no longer
            // constraining below it — clear so we resume decode-best/no-pref.
            // While sticky we keep holding/advertising the floor (issue #1179),
            // so do NOT clear constrained then.
            if self.current >= highest_available && !self.sticky {
                self.constrained = false;
            }
            return self.current;
        }

        if sample.is_congested() {
            // Responsive step-down: drop one layer now, reset the climb streak.
            if self.current > 0 {
                self.set_layer(self.current - 1, now_ms);
            }
            self.clean_windows = 0;
            // Sustained congestion broke any recovery streak.
            self.recovery_clean_ticks = 0;
            // Pin the floor to where we now sit when (a) this window latched
            // sticky, or (b) we were already sticky and congestion dragged us
            // BELOW the prior floor. Either way the floor tracks the lowest
            // proven-bad layer so recovery climbs up from there, never above it.
            if self.sticky && (just_latched || self.current < self.sticky_floor) {
                self.sticky_floor = self.current;
            }
            return self.current;
        }

        if sample.is_clean() {
            // Cautious recovery (issue #1179): while sticky, an uninterrupted
            // clean streak of STICKY_RECOVERY_CLEAN_TICKS raises the floor ONE
            // rung. This is separate from the normal step-up streak so the two
            // cadences (15s climb vs ~60s floor-raise) are independent.
            if self.sticky {
                self.recovery_clean_ticks = self.recovery_clean_ticks.saturating_add(1);
                if self.recovery_clean_ticks >= STICKY_RECOVERY_CLEAN_TICKS {
                    self.recovery_clean_ticks = 0;
                    if self.sticky_floor < highest_available {
                        self.sticky_floor += 1;
                    }
                    // Floor recovered to the top → leave sticky; the normal loop
                    // (below) resumes and will clear `constrained` once at the top.
                    if self.sticky_floor >= highest_available {
                        self.sticky = false;
                        self.congestion_score = 0;
                    }
                }
            }

            self.clean_windows = self.clean_windows.saturating_add(1);
            let dwell_ok = now_ms.saturating_sub(self.last_change_ms) >= LAYER_STEP_UP_DWELL_MS;
            let streak_ok = self.clean_windows >= STEP_UP_CLEAN_WINDOWS;
            // While sticky the climb is capped at the floor: we may climb back UP
            // TO `sticky_floor` (e.g. after a transient extra drop) but never
            // above it — that is the whole point of the floor (issue #1179).
            let climb_cap = if self.sticky {
                self.sticky_floor.min(highest_available)
            } else {
                highest_available
            };
            if dwell_ok && streak_ok && self.current < climb_cap {
                self.set_layer(self.current + 1, now_ms);
                // Require a fresh streak before the NEXT climb so we ascend one
                // rung per sustained-headroom period, not all at once.
                self.clean_windows = 0;
            }
            // Climbed (or already) back to the top → no longer constraining:
            // clear the flag so we advertise nothing and decode best again.
            // While sticky we keep holding the floor, so never clear then.
            if self.current >= highest_available && !self.sticky {
                self.constrained = false;
            }
            return self.current;
        }

        // Neutral band (between clean and congested): hold, but the streak
        // breaks so we do not climb on intermittent marginal windows. A neutral
        // window also breaks the recovery streak — recovery requires uninterrupted
        // clean reception, not merely "not congested".
        self.clean_windows = 0;
        self.recovery_clean_ticks = 0;
        self.current
    }

    /// Early-seed a constrain from a sample taken OUTSIDE the normal 5s monitor
    /// tick (issue #1179, Part B).
    ///
    /// ## Why
    /// `choose` is only fed every 5s (`connection.rs` `Interval::new(5000, …)`).
    /// A freshly-joined peer whose downlink is already congested therefore
    /// decodes the FULL-quality top layer for up to ~5s before the first monitor
    /// tick can react — long enough to stall a constrained receiver at join. For
    /// WebTransport peers (where reliable-unistream fan-out makes the join spike
    /// worst, per the 2026-06-09 simulcast-congestion meeting analysis), a
    /// short-lived fast sampler calls this on a fresh downlink sample so the FIRST
    /// congested sample constrains immediately instead of waiting for the tick.
    ///
    /// ## Semantics (pure)
    /// * Only acts while **unconstrained** (the cold-start decode-best state): if
    ///   `choose` has already constrained, the normal loop now owns adaptation and
    ///   this is a no-op (returns `false`).
    /// * A **congested** sample flips to constrained and steps down ONE rung from
    ///   the current top — identical to the unconstrained-congested arm of
    ///   `choose`, so the two entry points converge on the same state. Returns
    ///   `true` (the caller should emit the resulting preference and stop sampling).
    /// * A **clean / neutral** sample is a no-op (returns `false`): the seed only
    ///   reacts to actual early congestion; it never pre-emptively lowers a healthy
    ///   join (M2 cold-start is preserved untouched).
    ///
    /// Does NOT touch the congestion score or sticky machinery — a single early
    /// sample must not by itself latch sticky; that remains the job of sustained
    /// congestion observed by `choose`.
    pub fn observe_early_congestion(
        &mut self,
        sample: DownlinkSample,
        highest_available: u32,
        now_ms: u64,
    ) -> bool {
        if self.constrained || !sample.is_congested() {
            return false;
        }
        self.constrained = true;
        let from = self.current.min(highest_available);
        let dropped = from.saturating_sub(1);
        self.set_layer(dropped, now_ms);
        self.clean_windows = 0;
        true
    }

    /// Apply a layer change and reset the dwell/clean bookkeeping.
    fn set_layer(&mut self, layer: u32, now_ms: u64) {
        if layer != self.current {
            self.current = layer;
            self.last_change_ms = now_ms;
        }
    }
}

#[cfg(test)]
impl LayerChooser {
    /// Test-only view of the sticky-low latch (issue #1179).
    fn is_sticky(&self) -> bool {
        self.sticky
    }
    /// Test-only view of the held sticky floor (issue #1179).
    fn sticky_floor(&self) -> u32 {
        self.sticky_floor
    }
}

/// Clamp a chooser's desired layer into a user-configured receive range
/// (issue #989, Phase 4 seam).
///
/// P2 calls this with the full `[0, u32::MAX]` range (a no-op). P4 will pass the
/// user's `[min, max]` so the automatic selection is bounded by an explicit
/// preference without changing the chooser's adaptation logic. Kept here, pure
/// and tested, so P4 is a one-line wiring change at the call site.
pub fn clamp_to_user_range(desired: u32, user_min: u32, user_max: u32) -> u32 {
    desired.clamp(user_min.min(user_max), user_max.max(user_min))
}

/// User-configured RECEIVE-side layer bounds for ONE media kind (issue #989,
/// Phase 4).
///
/// ## Layer index convention (IMPORTANT for the UI author)
/// Bounds are **simulcast LAYER indices**, where **0 = base = LOWEST quality**
/// and a HIGHER index = HIGHER quality. This is the *opposite* of the 8-tier
/// SEND index convention (where tier 0 is the *best*). Per kind:
///   * video  — layers `0..=2` (low / standard / hd)
///   * screen — layers `0..=2` (low / medium / high)
///   * audio  — layers `0..=1` (low / high)
///
/// ## Semantics
/// `min`/`max` are inclusive bounds applied to EVERY incoming peer of this kind
/// ("never receive any peer's video below `min` or above `max`"). `None` means
/// "no bound" (open end). The default `(None, None)` is the full range → pure
/// auto-adaptation, no clamping. Out-of-order bounds (`min > max`) are normalized
/// by [`clamp_to_user_range`] (defensive; the UI should never send them).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KindLayerBounds {
    /// Inclusive minimum layer index, or `None` for "no lower bound" (0).
    pub min: Option<u32>,
    /// Inclusive maximum layer index, or `None` for "no upper bound".
    pub max: Option<u32>,
}

impl KindLayerBounds {
    /// `true` when no bound is set on either end → the chooser runs unclamped.
    pub fn is_open(&self) -> bool {
        self.min.is_none() && self.max.is_none()
    }

    /// Clamp a chooser's desired layer into these bounds. An absent `min`
    /// defaults to 0 (base); an absent `max` defaults to `u32::MAX` (open). When
    /// both are absent this is the identity (pure auto).
    pub fn clamp(&self, desired: u32) -> u32 {
        if self.is_open() {
            return desired;
        }
        clamp_to_user_range(desired, self.min.unwrap_or(0), self.max.unwrap_or(u32::MAX))
    }
}

/// All three per-kind receive-layer bounds (issue #989, Phase 4). Default is
/// fully open (no clamping on any kind). Stored on the client and applied to
/// each per-(peer, kind) chooser's desired layer at the monitor-tick call site.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReceiveLayerBounds {
    pub video: KindLayerBounds,
    pub screen: KindLayerBounds,
    pub audio: KindLayerBounds,
}

impl ReceiveLayerBounds {
    /// The bounds for a given media kind.
    pub fn for_kind(&self, kind: PrefMediaKind) -> KindLayerBounds {
        match kind {
            PrefMediaKind::Video => self.video,
            PrefMediaKind::Screen => self.screen,
            PrefMediaKind::Audio => self.audio,
        }
    }

    /// Set (or clear) the bounds for a given media kind.
    pub fn set_kind(&mut self, kind: PrefMediaKind, min: Option<u32>, max: Option<u32>) {
        let b = KindLayerBounds { min, max };
        match kind {
            PrefMediaKind::Video => self.video = b,
            PrefMediaKind::Screen => self.screen = b,
            PrefMediaKind::Audio => self.audio = b,
        }
    }
}

/// A real-time snapshot of the simulcast layer this receiver is CURRENTLY
/// decoding for one media kind, for the P5 quality needles (issue #989, Phase 4).
///
/// This reflects the **post-clamp** selected layer (what is actually decoded),
/// so it can never exceed the user's `max` bound — matching the needle's stated
/// expectation. `width`/`height` (and `kbps`) are resolved from the per-kind
/// layer ladder via [`received_layer_snapshot`]. `fps` is left `None` here
/// (the ladder's target fps is a publisher hint, not the received rate; the UI
/// already has received-fps elsewhere). Cheap to construct and poll per render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceivedLayerSnapshot {
    /// Which media kind this snapshot describes.
    pub kind: PrefMediaKind,
    /// The currently-decoded layer index (0 = base/lowest).
    pub layer_index: u32,
    /// Total layers available in this kind's ladder for `layer_count` layers
    /// (e.g. how many distinct layers the source ladder defines). Lets the UI
    /// render "layer 1 of 3".
    pub layer_count: u32,
    /// Resolution of the decoded layer in pixels (0 for audio).
    pub width: u32,
    pub height: u32,
    /// Approximate bitrate of the decoded layer in kbps, from the ladder.
    pub kbps: u32,
}

/// Audio simulcast bitrates (kbps) by layer, lowest-first (issue #989, Phase 3c
/// / 4; extended to 3 rungs in issue #1082). Mirrors the publisher's 3-layer
/// model (low 24 / mid 32 / high 50). Kept here so the snapshot resolver has no
/// dependency on the encoder module. This slice's length is the single source of
/// truth for the receiver-side audio ladder size (see [`AUDIO_LAYER_CAP`]).
const AUDIO_LAYER_KBPS: &[u32] = &[24, 32, 50];

/// Length of the receiver-side audio layer ladder, exposed as a `const fn` so
/// the publisher (`microphone_encoder.rs`) can tie its own ladder to this with a
/// compile-time assert and the two can never silently diverge (issue #1077).
pub const fn audio_layer_kbps_len() -> usize {
    AUDIO_LAYER_KBPS.len()
}

/// Receiver-side per-kind layer ceilings (issue #1082). Video and Screen are
/// tied at compile time to the AQ ladder sizes (`videocall_aq`'s
/// `SIMULCAST_MAX_LAYERS` / `SCREEN_SIMULCAST_MAX_LAYERS`); Audio is tied to
/// [`AUDIO_LAYER_KBPS`]'s length. Tying them here means a publisher↔receiver
/// ladder-size mismatch is impossible to silently introduce (issue #1077): bump
/// the source const and the receiver cap follows automatically.
const VIDEO_LAYER_CAP: u32 = videocall_aq::constants::SIMULCAST_MAX_LAYERS as u32;
const SCREEN_LAYER_CAP: u32 = videocall_aq::constants::SCREEN_SIMULCAST_MAX_LAYERS as u32;
const AUDIO_LAYER_CAP: u32 = AUDIO_LAYER_KBPS.len() as u32;

/// Number of simulcast layers the ladder defines for a media kind (issue #989;
/// per-kind decoupling + cross-crate tie in issues #1082 / #1077): video/screen
/// = `SIMULCAST_MAX_LAYERS`/`SCREEN_SIMULCAST_MAX_LAYERS`, audio =
/// `AUDIO_LAYER_KBPS.len()`. Single source of truth for the per-kind ladder size
/// used by the snapshot resolver and the availability-id clamp.
pub fn max_layers_for_kind(kind: PrefMediaKind) -> u32 {
    match kind {
        // Video and Screen share the same value today but are independent arms
        // (issue #1082) so a future per-kind divergence is a one-line change.
        PrefMediaKind::Video => VIDEO_LAYER_CAP,
        PrefMediaKind::Screen => SCREEN_LAYER_CAP,
        PrefMediaKind::Audio => AUDIO_LAYER_CAP,
    }
}

/// Clamp a raw incoming `simulcast_layer_id` to the highest valid layer index
/// for `kind` (issue #989, security follow-up). The layer id rides OUTSIDE the
/// AEAD seal, so a malicious publisher could cycle arbitrary/unbounded ids; if
/// fed straight into [`LayerAvailability::observe`] each unique id would add a
/// distinct map entry, inflating availability cardinality between prunes.
/// Clamping to `[0, max_layers_for_kind - 1]` bounds the map to the ladder size
/// regardless of what arrives on the wire, with no effect on honest publishers
/// (whose ids are already in range).
pub fn clamp_observed_layer_id(kind: PrefMediaKind, raw_layer_id: u32) -> u32 {
    raw_layer_id.min(max_layers_for_kind(kind).saturating_sub(1))
}

/// Resolve a [`ReceivedLayerSnapshot`] for `kind` at the given decoded
/// `layer_index`, mapping the layer to its resolution/bitrate via the per-kind
/// ladder (issue #989, Phase 4). `layer_count` is the number of layers the
/// source ladder is producing (>= 1). Pure + panic-safe: `layer_index` and
/// `layer_count` are clamped into range, so the 1-layer (flag-off) default
/// always yields a valid layer-0 snapshot.
pub fn received_layer_snapshot(
    kind: PrefMediaKind,
    layer_index: u32,
    layer_count: u32,
) -> ReceivedLayerSnapshot {
    // Clamp the ladder size to the supported range for this kind, and the index
    // into [0, count-1], so a degenerate input can never panic the resolver.
    let max_layers = max_layers_for_kind(kind);
    let audio = matches!(kind, PrefMediaKind::Audio);
    let count = layer_count.clamp(1, max_layers);
    let idx = layer_index.min(count.saturating_sub(1));

    if audio {
        let kbps = AUDIO_LAYER_KBPS
            .get(idx as usize)
            .copied()
            .unwrap_or(AUDIO_LAYER_KBPS[0]);
        return ReceivedLayerSnapshot {
            kind,
            layer_index: idx,
            layer_count: count,
            width: 0,
            height: 0,
            kbps,
        };
    }

    // Video / screen: resolve from the AQ ladder (lowest-first, index == layer).
    let tiers = match kind {
        PrefMediaKind::Screen => {
            crate::adaptive_quality_constants::simulcast_screen_layers(count as usize)
        }
        _ => crate::adaptive_quality_constants::simulcast_layers(count as usize),
    };
    let tier = tiers
        .get(idx as usize)
        .or_else(|| tiers.first())
        .expect("ladder is non-empty for count >= 1");
    ReceivedLayerSnapshot {
        kind,
        layer_index: idx,
        layer_count: count,
        width: tier.max_width,
        height: tier.max_height,
        kbps: tier.ideal_bitrate_kbps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean window (well under both clean thresholds).
    fn clean() -> DownlinkSample {
        DownlinkSample {
            loss_per_sec: 0.0,
            kf_per_sec: 0.0,
        }
    }

    /// A congested window (over the loss step-down threshold).
    fn congested() -> DownlinkSample {
        DownlinkSample {
            loss_per_sec: LOSS_STEP_DOWN_PER_SEC + 1.0,
            kf_per_sec: 0.0,
        }
    }

    /// A neutral window (in the dead-zone: above clean, below step-down).
    fn neutral() -> DownlinkSample {
        DownlinkSample {
            loss_per_sec: (LOSS_CLEAN_PER_SEC + LOSS_STEP_DOWN_PER_SEC) / 2.0,
            kf_per_sec: 0.0,
        }
    }

    /// Drive `n` clean windows spaced `dt_ms` apart starting at `start_ms`,
    /// returning the final timestamp used.
    fn feed_clean(c: &mut LayerChooser, avail: u32, start_ms: u64, n: u32, dt_ms: u64) -> u64 {
        let mut t = start_ms;
        for _ in 0..n {
            c.choose(clean(), avail, t);
            t += dt_ms;
        }
        t
    }

    /// Drive `n` congested windows spaced `dt_ms` apart starting at `start_ms`,
    /// returning the final timestamp used (issue #1179 sticky-low tests).
    fn feed_congested(c: &mut LayerChooser, avail: u32, start_ms: u64, n: u32, dt_ms: u64) -> u64 {
        let mut t = start_ms;
        for _ in 0..n {
            c.choose(congested(), avail, t);
            t += dt_ms;
        }
        t
    }

    #[test]
    fn starts_at_base_layer() {
        // The raw `current` field initializes to 0 before any sample is folded.
        let c = LayerChooser::new(0);
        assert_eq!(c.current(), 0);
    }

    #[test]
    fn cold_start_is_unconstrained_no_preference_and_decodes_top() {
        // M2 (#1079): a fresh chooser must NOT pin to base. With no preference
        // advertised (so the relay forwards all layers) the receiver decodes the
        // highest available layer immediately — no HD dip after (re)connect.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        // Before any sample: no preference.
        assert_eq!(
            c.desired_preference(),
            None,
            "cold start advertises no preference"
        );
        // First (even clean) window: decode the top, still no preference.
        let l = c.choose(clean(), avail, 1000);
        assert_eq!(l, 2, "unconstrained chooser decodes the highest available");
        assert_eq!(
            c.desired_preference(),
            None,
            "healthy receiver at the top advertises no preference"
        );
    }

    #[test]
    fn cold_start_with_layers_unobserved_advertises_nothing() {
        // M2: even before any higher layer is observed (avail still 0), the
        // chooser advertises no preference (not a concrete `0` = base-only).
        let mut c = LayerChooser::new(0);
        c.choose(clean(), 0, 1000);
        assert_eq!(c.current(), 0, "only base available → decode base");
        assert_eq!(
            c.desired_preference(),
            None,
            "must NOT advertise 0; absence = no constraint = forward all"
        );
    }

    #[test]
    fn constrains_only_after_congestion_then_clears_on_climb_back() {
        // M2: a preference is advertised ONLY while actively constrained, and is
        // cleared once the chooser climbs back to the top.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        c.choose(clean(), avail, 1000); // top, no pref
        assert_eq!(c.desired_preference(), None);
        // Congestion → constrained, drops to 1, advertises Some(1).
        c.choose(congested(), avail, 2000);
        assert_eq!(c.current(), 1);
        assert_eq!(
            c.desired_preference(),
            Some(1),
            "constrained chooser advertises its held layer"
        );
        // Sustained clean re-climbs to the top → preference clears.
        let mut t = 3000u64;
        for _ in 0..20 {
            c.choose(clean(), avail, t);
            t += 1100;
        }
        assert_eq!(c.current(), 2, "re-climbs to top");
        assert_eq!(
            c.desired_preference(),
            None,
            "back at the top → no preference again (clears the relay filter)"
        );
    }

    #[test]
    fn sustained_good_downlink_climbs_to_top_available() {
        // 3 layers available (0,1,2). Sustained clean windows with adequate
        // dwell must climb all the way to the top, one rung at a time.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        // Each window 1100ms apart so dwell (3000ms) is satisfied after the
        // 3-clean-window streak that licenses each climb.
        let mut t = 1000u64;
        // Climb 0 -> 1: need 3 clean windows AND dwell since last change.
        for _ in 0..20 {
            c.choose(clean(), avail, t);
            t += 1100;
        }
        assert_eq!(c.current(), 2, "sustained headroom must reach top layer");
    }

    #[test]
    fn loss_spike_steps_down_fast() {
        // Climb to top, then a single congested window must drop immediately.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        let mut t = 1000u64;
        for _ in 0..20 {
            c.choose(clean(), avail, t);
            t += 1100;
        }
        assert_eq!(c.current(), 2);
        // One bad window → down one rung, right now (no dwell required).
        let after = c.choose(congested(), avail, t);
        assert_eq!(after, 1, "a single congested window must step down at once");
    }

    #[test]
    fn hysteresis_prevents_flap_on_neutral_windows() {
        // After a congestion drop into the constrained state, neutral windows
        // must neither climb (streak resets) nor drop — the layer is stable.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        // Unconstrained start tracks the top; one congested window drops to 1 and
        // enters the constrained state.
        c.choose(clean(), avail, 1000); // decode top (2), no preference
        assert_eq!(c.current(), 2);
        c.choose(congested(), avail, 2000);
        assert_eq!(c.current(), 1, "congestion drops one rung into constrained");
        let mut t = 3000u64;
        for _ in 0..10 {
            let l = c.choose(neutral(), avail, t);
            assert_eq!(l, 1, "neutral windows must hold the current layer");
            t += 1100;
        }
    }

    #[test]
    fn only_base_available_stays_base() {
        // Availability cap of 0 (base-only / un-upgraded publisher): no amount
        // of clean headroom may climb above base.
        let mut c = LayerChooser::new(0);
        let mut t = 1000u64;
        for _ in 0..20 {
            let l = c.choose(clean(), 0, t);
            assert_eq!(l, 0, "cannot climb above the only available layer");
            t += 1100;
        }
    }

    #[test]
    fn step_up_requires_sustained_headroom() {
        // Re-climb after a congestion drop is conservative: fewer than
        // STEP_UP_CLEAN_WINDOWS clean windows must NOT climb back up.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        // Drop to the floor (0) via repeated congestion → constrained.
        c.choose(clean(), avail, 500); // top
        c.choose(congested(), avail, 1000); // -> 1
        c.choose(congested(), avail, 1500); // -> 0
        assert_eq!(c.current(), 0);
        // A few clean windows, but fewer than the streak → no re-climb.
        let mut t = 2000u64;
        for _ in 0..(STEP_UP_CLEAN_WINDOWS - 1) {
            c.choose(clean(), avail, t);
            t += 1100;
        }
        assert_eq!(
            c.current(),
            0,
            "must not re-climb before the clean-window streak is met"
        );
    }

    #[test]
    fn step_up_requires_dwell_even_with_streak() {
        // Re-climb after a drop also needs dwell: enough clean windows but bunched
        // within the dwell period (small dt) → the dwell guard blocks the climb.
        let mut c = LayerChooser::new(1000);
        let avail = 2;
        // Drop to 0 first (constrained).
        c.choose(clean(), avail, 500); // top
        c.choose(congested(), avail, 1000); // -> 1
        c.choose(congested(), avail, 1500); // -> 0
        assert_eq!(c.current(), 0);
        // 5 clean windows only 100ms apart → streak satisfied but dwell not met.
        let mut t = 2000u64;
        for _ in 0..5 {
            c.choose(clean(), avail, t);
            t += 100;
        }
        assert_eq!(
            c.current(),
            0,
            "dwell guard must block a re-climb even with a clean streak"
        );
    }

    #[test]
    fn availability_shrink_drops_selection_immediately() {
        // On the top layer, the source stops producing it (availability drops
        // to 1): the chooser must drop to the highest still-available layer at
        // once, regardless of downlink health.
        let mut c = LayerChooser::new(0);
        let mut t = 1000u64;
        for _ in 0..20 {
            c.choose(clean(), 2, t);
            t += 1100;
        }
        assert_eq!(c.current(), 2);
        let after = c.choose(clean(), 1, t);
        assert_eq!(after, 1, "must drop to highest available when top vanishes");
    }

    #[test]
    fn never_drops_below_base() {
        // Repeated congestion at base must floor at 0, never underflow.
        let mut c = LayerChooser::new(0);
        let mut t = 1000u64;
        for _ in 0..10 {
            let l = c.choose(congested(), 2, t);
            assert_eq!(l, 0, "base layer is the floor");
            t += 1100;
        }
    }

    #[test]
    fn pli_storm_steps_down_independent_of_loss() {
        // High PLI rate with zero sequence loss must still step down — a
        // receiver that cannot keep up freezes and storms PLIs.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        let t = feed_clean(&mut c, avail, 1000, 20, 1100);
        assert_eq!(c.current(), 2);
        let pli_only = DownlinkSample {
            loss_per_sec: 0.0,
            kf_per_sec: PLI_STEP_DOWN_PER_SEC + 1.0,
        };
        assert_eq!(c.choose(pli_only, avail, t), 1, "PLI storm must step down");
    }

    #[test]
    fn per_peer_independence() {
        // Two choosers: one fed congestion, one fed clean headroom. They must
        // diverge — the struggling peer drops, the healthy peer climbs.
        let mut bad = LayerChooser::new(0);
        let mut good = LayerChooser::new(0);
        let avail = 2;
        // Prime both to the top via clean headroom.
        let mut t = feed_clean(&mut bad, avail, 1000, 20, 1100);
        t = feed_clean(&mut good, avail, 1000, 20, 1100).max(t);
        assert_eq!(bad.current(), 2);
        assert_eq!(good.current(), 2);
        // Now diverge: bad gets congestion, good stays clean.
        bad.choose(congested(), avail, t);
        good.choose(clean(), avail, t + 5000); // dwell satisfied, already at top
        assert_eq!(bad.current(), 1, "struggling peer drops");
        assert_eq!(good.current(), 2, "healthy peer holds the top");
    }

    // -----------------------------------------------------------------
    // Issue #1179: sticky-low convergence (resting-point fix)
    //
    // Without the sticky state machine, a chronically marginal link
    // resting-points one rung ABOVE what it can sustain and yo-yos: the
    // conservative-up streak climbs back to the top, the next congested
    // window knocks it down, repeat. These tests pin the fixed behavior:
    // chronic congestion latches a floor the chooser refuses to climb above
    // until sustained recovery, raising the floor one cautious rung at a time.
    // -----------------------------------------------------------------

    #[test]
    fn chronic_congestion_latches_sticky_and_holds_floor() {
        // After STICKY_CONGESTION_EVENTS congested windows the chooser latches
        // sticky and pins a floor. Then, even with brief clean lulls that would
        // normally bait the conservative-up climb, it must NOT climb above the
        // floor — that is the resting-point fix.
        //
        // MUTATION CHECK: this test fails if the `!self.sticky` guard is removed
        // from the clean-branch climb cap (then `climb_cap` would be
        // `highest_available` and the chooser would climb above the floor).
        let mut c = LayerChooser::new(0);
        let avail = 2;
        c.choose(clean(), avail, 1000); // decode top (2), unconstrained
                                        // 3 congested windows → drops to 0 and latches sticky at floor 0.
        let t = feed_congested(&mut c, avail, 2000, STICKY_CONGESTION_EVENTS, 1100);
        assert!(c.is_sticky(), "chronic congestion must latch sticky");
        assert_eq!(c.sticky_floor(), 0, "floor pinned to the proven-bad layer");
        assert_eq!(c.current(), 0);
        // Feed clean windows but FEWER than a full recovery period each time it
        // would matter — interleave a congested window to reset recovery so the
        // floor is never raised. The chooser must stay pinned at 0.
        let mut tt = t;
        for _ in 0..5 {
            // A short clean burst (well under STICKY_RECOVERY_CLEAN_TICKS)…
            tt = feed_clean(&mut c, avail, tt, STICKY_RECOVERY_CLEAN_TICKS - 1, 1100);
            // …then one congested window resets the recovery streak.
            tt = feed_congested(&mut c, avail, tt, 1, 1100);
            assert_eq!(
                c.current(),
                0,
                "sticky chooser must hold the floor (no climb above it)"
            );
            assert_eq!(c.sticky_floor(), 0, "floor must not rise without recovery");
        }
        assert!(
            c.is_sticky(),
            "still sticky — link never sustained recovery"
        );
        assert_eq!(
            c.desired_preference(),
            Some(0),
            "sticky chooser keeps advertising its held floor"
        );
    }

    #[test]
    fn sticky_does_not_climb_above_floor_without_recovery() {
        // Pure no-climb-above-floor: latch sticky at floor 0, then feed a long
        // UNINTERRUPTED clean streak that is exactly one short of a recovery
        // period. The floor (and decode layer) must stay at 0.
        //
        // MUTATION CHECK: fails if the recovery `>= STICKY_RECOVERY_CLEAN_TICKS`
        // threshold is lowered/removed, or if the climb cap ignores sticky.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        c.choose(clean(), avail, 500);
        feed_congested(&mut c, avail, 1000, STICKY_CONGESTION_EVENTS, 1100);
        assert!(c.is_sticky());
        assert_eq!(c.sticky_floor(), 0);
        // One window short of the recovery period → no floor raise, no climb.
        let mut t = 10_000u64;
        for _ in 0..(STICKY_RECOVERY_CLEAN_TICKS - 1) {
            c.choose(clean(), avail, t);
            t += 1100;
        }
        assert_eq!(c.current(), 0, "must not climb above the sticky floor");
        assert_eq!(
            c.sticky_floor(),
            0,
            "floor unchanged before recovery period"
        );
        assert!(c.is_sticky());
    }

    #[test]
    fn sticky_recovers_one_rung_after_sustained_clean() {
        // After exactly STICKY_RECOVERY_CLEAN_TICKS uninterrupted clean windows,
        // the floor rises by ONE rung (cautious recovery) and the chooser may
        // climb to the new floor — but not beyond it in the same period.
        //
        // MUTATION CHECK: fails if the floor-raise `sticky_floor += 1` is removed
        // (floor stays 0 forever) or if it raises by more than one rung.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        c.choose(clean(), avail, 500);
        feed_congested(&mut c, avail, 1000, STICKY_CONGESTION_EVENTS, 1100);
        assert_eq!(c.sticky_floor(), 0);
        // Exactly one recovery period of uninterrupted clean.
        let mut t = 10_000u64;
        for _ in 0..STICKY_RECOVERY_CLEAN_TICKS {
            c.choose(clean(), avail, t);
            t += 1100;
        }
        assert_eq!(
            c.sticky_floor(),
            1,
            "one recovery period raises the floor exactly one rung"
        );
        assert!(c.is_sticky(), "still sticky: floor (1) below top (2)");
        // The decode layer climbs up TO the new floor (1) but not above it.
        // Keep feeding clean within this period (recovery just reset) so the
        // normal step-up streak licenses the climb to the floor.
        let mut t2 = t;
        for _ in 0..STEP_UP_CLEAN_WINDOWS + 1 {
            c.choose(clean(), avail, t2);
            t2 += 1100;
        }
        assert_eq!(c.current(), 1, "climbs up to the raised floor, not above");

        // A SECOND full recovery period raises the floor to the top → sticky
        // clears and the chooser returns to decode-best / no-preference.
        let mut t3 = t2;
        for _ in 0..STICKY_RECOVERY_CLEAN_TICKS {
            c.choose(clean(), avail, t3);
            t3 += 1100;
        }
        // Drive a few more clean windows so the now-unsticky loop climbs to top.
        for _ in 0..10 {
            c.choose(clean(), avail, t3);
            t3 += 1100;
        }
        assert!(!c.is_sticky(), "floor reached top → sticky clears");
        assert_eq!(c.current(), 2, "fully recovered to the top layer");
        assert_eq!(
            c.desired_preference(),
            None,
            "back at the top → no preference"
        );
    }

    #[test]
    fn sticky_recovery_streak_resets_on_neutral_window() {
        // Recovery requires UNINTERRUPTED clean reception: a neutral (dead-zone)
        // window mid-streak must reset the recovery counter so the floor does not
        // rise on a stop-start link.
        //
        // MUTATION CHECK: fails if the neutral branch stops resetting
        // `recovery_clean_ticks`.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        c.choose(clean(), avail, 500);
        feed_congested(&mut c, avail, 1000, STICKY_CONGESTION_EVENTS, 1100);
        assert_eq!(c.sticky_floor(), 0);
        let mut t = 10_000u64;
        // One window short of recovery…
        for _ in 0..(STICKY_RECOVERY_CLEAN_TICKS - 1) {
            c.choose(clean(), avail, t);
            t += 1100;
        }
        // …a neutral window resets the streak…
        c.choose(neutral(), avail, t);
        t += 1100;
        // …then a full-minus-one clean streak again: still no raise.
        for _ in 0..(STICKY_RECOVERY_CLEAN_TICKS - 1) {
            c.choose(clean(), avail, t);
            t += 1100;
        }
        assert_eq!(
            c.sticky_floor(),
            0,
            "a neutral window resets the recovery streak; floor must not rise"
        );
        assert!(c.is_sticky());
    }

    #[test]
    fn single_congested_window_does_not_stick() {
        // The fast-down path (one congested window steps down) must NOT latch
        // sticky — only sustained congestion does. A lone spike stays in the
        // ordinary constrained loop and re-climbs normally.
        //
        // MUTATION CHECK: fails if the latch threshold is lowered to 1, or if the
        // score increments without the >= STICKY_CONGESTION_EVENTS gate.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        c.choose(clean(), avail, 1000); // top
        c.choose(congested(), avail, 2000); // one spike → 1, constrained
        assert_eq!(c.current(), 1, "single spike steps down one rung");
        assert!(
            !c.is_sticky(),
            "a single congested window must NOT latch sticky"
        );
        // Sustained clean must re-climb all the way to the top (no floor pinning).
        let mut t = 3000u64;
        for _ in 0..30 {
            c.choose(clean(), avail, t);
            t += 1100;
        }
        assert_eq!(c.current(), 2, "non-sticky chooser re-climbs to the top");
        assert_eq!(c.desired_preference(), None);
    }

    #[test]
    fn score_decay_prevents_permanent_stick() {
        // Congested windows SPACED OUT by enough clean windows must never
        // accumulate to the latch threshold, because each clean window decays the
        // score. This is the anti-false-positive property: an occasionally-lossy
        // but fundamentally healthy link must never get stuck.
        //
        // MUTATION CHECK: fails if the clean-window score decay
        // (`congestion_score.saturating_sub(1)`) is removed — then spaced spikes
        // would still accumulate to the threshold and wrongly latch.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        c.choose(clean(), avail, 500);
        let mut t = 1000u64;
        // Pattern: 1 congested, then 2 clean (net score change per cycle: +1-2,
        // saturating at 0). Repeat many times — score can never reach 3.
        for _ in 0..20 {
            c.choose(congested(), avail, t);
            t += 1100;
            c.choose(clean(), avail, t);
            t += 1100;
            c.choose(clean(), avail, t);
            t += 1100;
        }
        assert!(
            !c.is_sticky(),
            "decay must keep an occasionally-lossy link from latching sticky"
        );
    }

    #[test]
    fn cold_join_never_sticks() {
        // A freshly-joined receiver fed only clean windows must never go sticky
        // and must keep full quality (decode-best, no preference) — the sticky
        // machinery must be inert on a healthy cold start (M2 preserved).
        //
        // MUTATION CHECK: fails if the score ever increments on clean windows or
        // if sticky can latch without congestion.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        let mut t = 1000u64;
        for _ in 0..50 {
            c.choose(clean(), avail, t);
            t += 1100;
        }
        assert!(!c.is_sticky(), "a clean cold-join must never latch sticky");
        assert_eq!(c.current(), 2, "decodes the top");
        assert_eq!(c.desired_preference(), None, "advertises no preference");
    }

    // -----------------------------------------------------------------
    // Issue #1179, Part B: observe_early_congestion early seed
    // -----------------------------------------------------------------

    #[test]
    fn early_congestion_seeds_constrain_on_congested_sample() {
        // A congested early sample on an unconstrained (cold-join) chooser must
        // constrain immediately and step down one rung, returning true so the
        // glue knows to emit a preference and stop sampling — instead of waiting
        // up to 5s for the first monitor tick.
        //
        // MUTATION CHECK: fails if `observe_early_congestion` returns false on a
        // congested sample, or does not set `constrained` / step down.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        // Cold start: decode-best at the top, no preference.
        c.choose(clean(), avail, 1000);
        assert_eq!(c.current(), 2);
        assert_eq!(c.desired_preference(), None);
        // Early congested sample seeds the constrain.
        let seeded = c.observe_early_congestion(congested(), avail, 1500);
        assert!(seeded, "congested early sample must seed a constrain");
        assert_eq!(
            c.current(),
            1,
            "early seed steps down one rung from the top"
        );
        assert_eq!(
            c.desired_preference(),
            Some(1),
            "seeded constrain advertises the held layer"
        );
        // A single early sample must NOT latch sticky.
        assert!(!c.is_sticky(), "one early sample never latches sticky");
    }

    #[test]
    fn early_congestion_is_noop_on_clean_or_already_constrained() {
        // A clean early sample is a no-op (cold-start decode-best preserved), and
        // once the chooser is already constrained the normal loop owns adaptation
        // so the early seed must not fire again.
        //
        // MUTATION CHECK: fails if the `self.constrained || !is_congested()` guard
        // is removed (then a clean sample would constrain, or it would re-fire
        // after the chooser is already constrained).
        let mut c = LayerChooser::new(0);
        let avail = 2;
        c.choose(clean(), avail, 1000); // decode-best at top
                                        // Clean early sample → no-op.
        let seeded_clean = c.observe_early_congestion(clean(), avail, 1200);
        assert!(!seeded_clean, "clean early sample must not constrain");
        assert_eq!(c.current(), 2, "healthy join keeps full quality");
        assert_eq!(c.desired_preference(), None);
        // Now constrain via a real congested sample…
        assert!(c.observe_early_congestion(congested(), avail, 1400));
        assert_eq!(c.current(), 1);
        // …a SECOND early call (even congested) is a no-op: already constrained.
        let seeded_again = c.observe_early_congestion(congested(), avail, 1600);
        assert!(
            !seeded_again,
            "early seed must not re-fire once constrained — the 5s loop owns it"
        );
        assert_eq!(
            c.current(),
            1,
            "no extra step-down from a repeated early seed"
        );
    }

    #[test]
    fn availability_window_forgets_unseen_layers() {
        let mut a = LayerAvailability::with_window(1000);
        a.observe(0, 100);
        a.observe(1, 100);
        a.observe(2, 100);
        assert_eq!(a.highest_available(100), 2);
        // Re-observe only the base within the window; layers 1,2 expire.
        a.observe(0, 1200);
        assert_eq!(
            a.highest_available(1200),
            0,
            "unseen top layers must expire out of availability"
        );
    }

    #[test]
    fn availability_defaults_to_base_when_nothing_seen() {
        let mut a = LayerAvailability::new();
        assert_eq!(a.highest_available(0), 0);
    }

    #[test]
    fn clamp_to_user_range_is_noop_on_full_range() {
        assert_eq!(clamp_to_user_range(2, 0, u32::MAX), 2);
        assert_eq!(clamp_to_user_range(0, 0, u32::MAX), 0);
    }

    #[test]
    fn clamp_to_user_range_bounds_p4_preference() {
        // P4: clamp desired into [min, max].
        assert_eq!(clamp_to_user_range(2, 0, 1), 1, "clamped down to user max");
        assert_eq!(clamp_to_user_range(0, 1, 2), 1, "clamped up to user min");
        // Defensive: inverted bounds are normalized, never panic.
        assert_eq!(clamp_to_user_range(5, 2, 1), 2);
    }

    // -----------------------------------------------------------------
    // Phase 4: KindLayerBounds / ReceiveLayerBounds
    // -----------------------------------------------------------------

    #[test]
    fn kind_bounds_default_is_open_and_identity() {
        let b = KindLayerBounds::default();
        assert!(b.is_open(), "default bounds are fully open");
        // Open bounds are the identity → pure auto, no clamping.
        for d in 0..=2 {
            assert_eq!(b.clamp(d), d);
        }
    }

    #[test]
    fn kind_bounds_max_clamps_down() {
        let b = KindLayerBounds {
            min: None,
            max: Some(1),
        };
        assert!(!b.is_open());
        assert_eq!(b.clamp(2), 1, "desired above max is clamped down");
        assert_eq!(b.clamp(1), 1);
        assert_eq!(b.clamp(0), 0, "below max is untouched");
    }

    #[test]
    fn kind_bounds_min_clamps_up() {
        let b = KindLayerBounds {
            min: Some(1),
            max: None,
        };
        assert_eq!(b.clamp(0), 1, "desired below min is clamped up");
        assert_eq!(b.clamp(2), 2);
    }

    #[test]
    fn kind_bounds_pin_to_single_layer() {
        // min == max pins every peer to exactly that layer.
        let b = KindLayerBounds {
            min: Some(1),
            max: Some(1),
        };
        assert_eq!(b.clamp(0), 1);
        assert_eq!(b.clamp(2), 1);
    }

    #[test]
    fn receive_bounds_per_kind_independent() {
        let mut rb = ReceiveLayerBounds::default();
        rb.set_kind(PrefMediaKind::Video, Some(0), Some(0)); // video pinned to base
        rb.set_kind(PrefMediaKind::Screen, None, Some(2)); // screen open up to 2
        assert_eq!(rb.for_kind(PrefMediaKind::Video).clamp(2), 0);
        assert_eq!(rb.for_kind(PrefMediaKind::Screen).clamp(2), 2);
        // Audio untouched → open.
        assert!(rb.for_kind(PrefMediaKind::Audio).is_open());
    }

    // -----------------------------------------------------------------
    // Phase 4: received_layer_snapshot layer→resolution mapping
    // -----------------------------------------------------------------

    #[test]
    fn snapshot_video_maps_layer_to_ladder_resolution() {
        // 3-layer video ladder, top layer (2) = 1280x720 hd.
        let s = received_layer_snapshot(PrefMediaKind::Video, 2, 3);
        assert_eq!(s.kind, PrefMediaKind::Video);
        assert_eq!(s.layer_index, 2);
        assert_eq!(s.layer_count, 3);
        assert_eq!((s.width, s.height), (1280, 720));
        assert!(s.kbps > 0);
        // Base layer (0) = lowest resolution.
        let base = received_layer_snapshot(PrefMediaKind::Video, 0, 3);
        assert_eq!((base.width, base.height), (640, 360));
        assert!(base.kbps < s.kbps, "base bitrate < top bitrate");
    }

    #[test]
    fn snapshot_screen_top_layer_is_1080p() {
        let s = received_layer_snapshot(PrefMediaKind::Screen, 2, 3);
        assert_eq!((s.width, s.height), (1920, 1080));
    }

    #[test]
    fn snapshot_audio_has_no_resolution_and_kbps_by_layer() {
        // Audio is now a 3-rung ladder (issue #1082): low 24 / mid 32 / high 50.
        let low = received_layer_snapshot(PrefMediaKind::Audio, 0, 3);
        assert_eq!((low.width, low.height), (0, 0));
        assert_eq!(low.kbps, 24);
        let mid = received_layer_snapshot(PrefMediaKind::Audio, 1, 3);
        assert_eq!(mid.kbps, 32);
        let high = received_layer_snapshot(PrefMediaKind::Audio, 2, 3);
        assert_eq!(high.kbps, 50);
        assert_eq!(high.layer_count, 3);
    }

    #[test]
    fn snapshot_is_panic_safe_on_out_of_range() {
        // Degenerate inputs are clamped, never panic.
        let s = received_layer_snapshot(PrefMediaKind::Video, 99, 99);
        assert_eq!(s.layer_count, 3, "ladder size capped to 3 for video");
        assert_eq!(s.layer_index, 2, "index clamped to count-1");
        // Audio capped to 3 (issue #1082).
        let a = received_layer_snapshot(PrefMediaKind::Audio, 99, 99);
        assert_eq!(a.layer_count, 3);
        assert_eq!(a.layer_index, 2);
    }

    #[test]
    fn snapshot_single_layer_default_is_base() {
        // 1-layer (flag-off) default: layer 0 / base, valid for every kind.
        for kind in [
            PrefMediaKind::Video,
            PrefMediaKind::Screen,
            PrefMediaKind::Audio,
        ] {
            let s = received_layer_snapshot(kind, 0, 1);
            assert_eq!(s.layer_index, 0);
            assert_eq!(s.layer_count, 1);
        }
    }

    // -----------------------------------------------------------------
    // Security follow-up: clamp_observed_layer_id bounds availability cardinality
    // -----------------------------------------------------------------

    #[test]
    fn max_layers_for_kind_matches_ladders() {
        // Tied to the publisher-side ladder sizes at compile time (issues #1082 /
        // #1077): video/screen = AQ SIMULCAST ceilings, audio = AUDIO_LAYER_KBPS.
        assert_eq!(
            max_layers_for_kind(PrefMediaKind::Video),
            videocall_aq::constants::SIMULCAST_MAX_LAYERS as u32
        );
        assert_eq!(
            max_layers_for_kind(PrefMediaKind::Screen),
            videocall_aq::constants::SCREEN_SIMULCAST_MAX_LAYERS as u32
        );
        assert_eq!(
            max_layers_for_kind(PrefMediaKind::Audio),
            AUDIO_LAYER_KBPS.len() as u32
        );
        // Concrete values for the current ladders.
        assert_eq!(max_layers_for_kind(PrefMediaKind::Video), 3);
        assert_eq!(max_layers_for_kind(PrefMediaKind::Screen), 3);
        assert_eq!(max_layers_for_kind(PrefMediaKind::Audio), 3);
    }

    #[test]
    fn audio_chooser_traverses_three_rungs() {
        // Phase C verification (issue #1082): with audio now a 3-rung ladder, the
        // (kind-agnostic) chooser must climb to the top audio layer (index 2 =
        // max_layers_for_kind(Audio) - 1) under sustained clean downlink, then
        // step down rung-by-rung under congestion. This exercises the exact
        // selector the receiver drives for audio.
        let top_audio = max_layers_for_kind(PrefMediaKind::Audio) - 1;
        assert_eq!(top_audio, 2);

        let mut c = LayerChooser::new(0);
        let mut t = 1000u64;
        // Sustained headroom climbs all the way to the top audio rung.
        for _ in 0..30 {
            c.choose(clean(), top_audio, t);
            t += 1100;
        }
        assert_eq!(c.current(), 2, "audio must climb to the top 3-rung layer");

        // Congestion steps down one rung at a time: 2 -> 1 -> 0.
        assert_eq!(c.choose(congested(), top_audio, t), 1);
        t += 1100;
        assert_eq!(c.choose(congested(), top_audio, t), 0);
        t += 1100;
        assert_eq!(c.choose(congested(), top_audio, t), 0, "floors at base");
    }

    #[test]
    fn audio_observed_layer_id_climb_to_top_rung() {
        // The receiver learns availability from observed layer ids; a publisher
        // emitting all 3 audio rungs must let the chooser reach index 2. The
        // clamp keeps an out-of-range id from inflating availability beyond the
        // ladder, but in-range ids 0/1/2 must all be learnable (issue #1082).
        let mut avail = LayerAvailability::new();
        let now = 1_000u64;
        for raw in 0u32..=2 {
            avail.observe(clamp_observed_layer_id(PrefMediaKind::Audio, raw), now);
        }
        assert_eq!(
            avail.highest_available(now),
            2,
            "all three audio rungs must be learnable"
        );
        // A bogus higher id is clamped down to the top audio index, not learned
        // as a 4th rung.
        avail.observe(clamp_observed_layer_id(PrefMediaKind::Audio, 99), now);
        assert_eq!(avail.highest_available(now), 2);
    }

    #[test]
    fn clamp_observed_layer_id_caps_to_ladder() {
        // In-range ids pass through; out-of-range ids clamp to the top index.
        assert_eq!(clamp_observed_layer_id(PrefMediaKind::Video, 0), 0);
        assert_eq!(clamp_observed_layer_id(PrefMediaKind::Video, 2), 2);
        assert_eq!(clamp_observed_layer_id(PrefMediaKind::Video, 3), 2);
        assert_eq!(clamp_observed_layer_id(PrefMediaKind::Video, u32::MAX), 2);
        // Audio now caps at index 2 (3-rung ladder, issue #1082).
        assert_eq!(clamp_observed_layer_id(PrefMediaKind::Audio, 2), 2);
        assert_eq!(clamp_observed_layer_id(PrefMediaKind::Audio, 5), 2);
    }

    #[test]
    fn clamped_observe_bounds_availability_cardinality() {
        // Simulate an attacker cycling many UNIQUE out-of-range layer ids: with
        // the clamp, availability can never hold more than the ladder size, and
        // highest_available never exceeds the top index — no inflation between
        // prunes. (Without the clamp this map would grow to ~1000 entries.)
        let mut avail = LayerAvailability::new();
        let now = 1_000u64;
        for raw in 0u32..1000 {
            let clamped = clamp_observed_layer_id(PrefMediaKind::Video, raw);
            avail.observe(clamped, now);
        }
        // highest_available also prunes; with all observations at `now` it is the
        // top ladder index, not some giant attacker value.
        assert_eq!(
            avail.highest_available(now),
            2,
            "clamped observe keeps availability within the 3-layer video ladder"
        );
    }
}

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
//! `run_peer_monitor` → `tick_layer_choosers`). The loss/PLI window itself rolls
//! over every ~1s (`peer_decode_manager.rs`'s `observe_window`), and
//! `last_video_downlink` is OVERWRITTEN with each new ~1s window's rates — it is
//! NOT accumulated. So the chooser is fed the LATEST ~1s sample once per 5s tick
//! (it does not aggregate 5s of reception). The constants below are still tuned
//! for that 5s decision cadence because `choose` is CALLED every 5s — e.g.
//! `STEP_UP_CLEAN_WINDOWS = 3` requires 3 consecutive clean ticks ≈ 15s of
//! sustained headroom.
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

// --- Screen layer-oscillation damping (issue #1899) ---
//
// On a STATIC screen share the plain fast-down / conservative-up loop above
// self-oscillates: a step DOWN advertises a preference, the relay stops
// forwarding the top layer, availability forgets it, `constrained` clears (we
// reached the shrunken top), the relay fail-opens, availability RE-learns the
// top, and the unconstrained follow-top climbs back UP — and every one of those
// switches re-arms a keyframe wait the new layer cannot paint through until ITS
// keyframe arrives. On a static screen that keyframe is scarce, so the wait
// freezes playout and storms PLIs (`kf_per_sec`), which reads as congestion and
// forces the NEXT step down. The oscillation is self-sustaining (each switch
// manufactures the congestion that drives the next), which is why a static
// 720p@8fps share logged 563 screen LAYER_SWITCH + 444 freeze-after-switch skips
// in one meeting. The sticky-low machinery (#1179) would settle it, but never
// latches here because the congestion is not *continuously* sustained — the
// `congestion_score` decays during the clean lulls between switch-induced PLI
// windows, so it never reaches STICKY_CONGESTION_EVENTS.
//
// The damping closes exactly that gap for SCREEN only: a step DOWN that closely
// follows a step UP (a "yo-yo") is treated as chronic and force-latches the
// existing sticky floor at the just-dropped rung, so the chooser stops
// re-climbing into the same freeze and SETTLES. Recovery is the SAME
// time-bounded STICKY_RECOVERY_CLEAN_TICKS path (~60s of uninterrupted clean
// raises the floor one rung), so a genuinely-recovered link still climbs out —
// it cannot wedge. Camera/audio choosers set `screen_mode == false` and are
// provably unaffected (the camera↔screen divergence is deliberate: camera has
// continuous motion so it never keyframe-starves the way a static screen does).

/// Max age (ms) of a preceding UP switch for a following DOWN switch to count as
/// a screen "yo-yo" and force the sticky latch (issue #1899). The oscillation's
/// up-switch → freeze → PLI → down-switch leg completes within ~1-2 monitor ticks
/// (the 5s `choose` cadence), so this is sized to a few ticks to catch it
/// robustly while staying well short of any legitimate minutes-apart adaptation.
/// A false match is benign for screen: it only holds one rung lower and
/// re-probes on the time-bounded recovery path.
pub const SCREEN_LAYER_OSCILLATION_LATCH_MS: u64 = 15_000;

// --- Keyframe-starved screen rung retraction (issue #2328) -------------------
//
// `LayerAvailability` learns which rungs a source offers purely from PACKET
// ARRIVAL, and its doc calls out that "a keyframe-only lull does not retract a
// layer". The INVERSE case is the blind spot #2328 hit: a rung that keeps
// delivering DELTAS but no decodable KEYFRAME looks perfectly healthy to
// availability (packets keep landing, `last_seen_ms` keeps refreshing) while the
// receiver sitting on it is frozen with nothing to decode from. The chooser
// therefore never downshifts off it, and — because the relay ALWAYS forwards
// layer 0 regardless of preference (`chat_server.rs`'s `simulcast_layer_id != 0`
// guard) — the base rung's keyframes are on the wire the whole time, unused.
//
// The fix is a TIME-BOUNDED quarantine, deliberately not a success counter: the
// repo's recovery-hysteresis rule forbids strictly-consecutive counters because
// they reset under ongoing contention and can pin an entity indefinitely. Here a
// counter would be worse than merely fragile — it would be unsatisfiable. Once
// the receiver leaves the rung, that rung MAY stop arriving entirely (if the
// chooser is `constrained` it publishes a LAYER_PREFERENCE and the relay stops
// forwarding it), so "re-admit once it demonstrably delivers a keyframe" could
// never be met. Expiry is therefore purely wall-clock: the rung becomes eligible
// again after the window whatever happened in between, and if it is still
// starved the next hold re-quarantines it. That is the only shape that cannot
// wedge.

/// How long a SCREEN rung must be BOTH decoder-starved and keyframe-less before
/// this receiver retracts it from its own availability view (issue #2328).
///
/// Tied to [`videocall_codecs::jitter_buffer::MAX_KEYFRAME_LESS_HOLD_MS`] — the
/// ceiling the decoder's own jitter buffer uses to declare a keyframe-less hold
/// unrecoverable and escalate to a pipeline reset — so the two cannot silently
/// diverge. Semantics match: by the time the buffer has been starved that long,
/// the publisher has missed at least two `SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS`
/// (3s) GOP cadences on this rung, which is no longer a lull.
pub const SCREEN_KEYFRAME_STARVED_RETRACT_MS: u64 =
    videocall_codecs::jitter_buffer::MAX_KEYFRAME_LESS_HOLD_MS as u64;

/// How long a retracted SCREEN rung stays excluded from this receiver's
/// availability view before it is re-admitted and may be climbed back to
/// (issue #2328).
///
/// Sized against the two cadences it has to clear:
/// * the chooser only re-evaluates on the **5s monitor tick**, so the window
///   must span several ticks or the rung would be re-admitted before the
///   downshift it caused had even been applied;
/// * [`STICKY_RECOVERY_CLEAN_TICKS`] (12 ticks ≈ 60s) is the existing "cautious
///   re-climb" cadence, so a shorter window here re-probes strictly more eagerly
///   than the chooser's own recovery path and cannot be the limiting factor.
///
/// 30s ⇒ a still-broken rung costs at most ~6s of freeze per ~36s cycle (~17%)
/// instead of a permanent one, while a rung broken only transiently is back
/// within half a minute. Deliberately NOT reused from
/// [`SCREEN_LAYER_OSCILLATION_LATCH_MS`] (15s): that is a *detection* window for
/// the #1899 yo-yo, not a *recovery* dwell, and 15s here would only leave ~2
/// monitor ticks on the base rung before re-probing.
pub const SCREEN_KEYFRAME_STARVED_QUARANTINE_MS: u64 = 30_000;

/// The window a keyframe-starvation observation stays actionable (issue #2328).
///
/// The starvation clock is stamped from `now_ms()`, a WALL clock, so a
/// backgrounded tab (or an NTP step) can resume with an arbitrarily old
/// timestamp and make the retract test trip instantly on the first packet after
/// resume — a rung retracted for a freeze that has already ended. Past this
/// bound the observation is treated as stale and simply RE-ARMED to `now`, which
/// costs one fresh hold window and cannot wedge (it never latches anything).
pub const SCREEN_KEYFRAME_STARVED_STALE_MS: u64 = SCREEN_KEYFRAME_STARVED_QUARANTINE_MS;

/// The retraction bound must sit strictly below the staleness bound, or every
/// observation would be classified stale before it could ever retract and the
/// whole path would be dead code.
const _: () = assert!(SCREEN_KEYFRAME_STARVED_RETRACT_MS < SCREEN_KEYFRAME_STARVED_STALE_MS);
/// The quarantine must outlast the retraction bound, or a rung could be
/// re-admitted before the receiver had spent even one hold window off it.
const _: () = assert!(SCREEN_KEYFRAME_STARVED_QUARANTINE_MS > SCREEN_KEYFRAME_STARVED_RETRACT_MS);

/// What the receiver should do about the current SCREEN keyframe-starvation
/// observation. Returned by [`screen_starvation_action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenStarvationAction {
    /// Nothing to do: no starvation recorded, the hold has not reached the
    /// bound yet, or the receiver is already on the base rung (which the relay
    /// always forwards — there is no lower rung to fall back to, so this is the
    /// decoder-escalation path's problem, not the chooser's).
    None,
    /// The selected rung has been starved past the bound WHILE the base rung was
    /// still arriving: quarantine it so the chooser downshifts to a rung that is
    /// actually being delivered.
    Retract,
    /// The observation is older than [`SCREEN_KEYFRAME_STARVED_STALE_MS`] and
    /// cannot be trusted (suspended tab / clock step). Re-arm the clock to
    /// `now_ms` and require a fresh hold window.
    Rearm,
}

/// Decide what to do about a SCREEN rung that the decoder reports as
/// keyframe-starved (issue #2328). Pure, so the encode-loop-free host tests pin
/// it; `peer_decode_manager` supplies the live inputs.
///
/// * `selected_layer` — the rung this receiver currently decodes for this
///   source. `0` always yields [`ScreenStarvationAction::None`]: the base rung
///   is unconditionally forwarded by the relay and there is nothing below it, so
///   retracting it could only make things worse.
/// * `starved_since_ms` — when the CURRENT unbroken keyframe-less hold began
///   (`None` ⇒ not starved). Cleared by the caller on any keyframe arrival, so a
///   rung that is delivering keyframes can never reach the bound.
/// * `base_rung_arriving` — whether layer 0 is still being delivered to this
///   receiver. This is the ASYMMETRY test, and it is what separates the defect
///   from a symmetric pressure episode; see below.
///
/// ## Why the asymmetry test is required
/// A starved selected rung is only actionable if there is a HEALTHIER rung to
/// fall back to. `actix-api/src/actors/priority_drop.rs` drops SCREEN media once
/// the outbound channel is at `PRIORITY_DROP_SCREEN_FILL_RATIO` (0.90) and that
/// drop is LAYER-AGNOSTIC — it never inspects `simulcast_layer_id`, so under
/// severe outbound pressure the base rung's keyframes are dropped right along
/// with everything else (that is #1977's design, a symmetric pressure path, and
/// it is NOT the per-layer asymmetry #2328 fixes). Retracting a rung during such
/// an episode buys nothing — every rung is equally starved — and only adds
/// LAYER_SWITCH churn to a link that is already saturated. Requiring layer 0 to
/// still be arriving confines the retraction to the case it was built for: THIS
/// rung is starved while another one is being delivered.
///
/// ## What `base_rung_arriving` can and cannot prove
/// The caller sources it from packet ARRIVAL
/// ([`LayerAvailability::layer_available_peek`] on layer 0), which is sound
/// because the relay forwards `simulcast_layer_id == 0` unconditionally,
/// whatever LAYER_PREFERENCE this receiver published — so base packets are on
/// the wire and `observe`d even while the receiver decodes rung 2. It proves
/// "the stream has not stopped wholesale"; it does NOT prove layer 0 is
/// delivering KEYFRAMES specifically. It cannot: `frame_type` lives inside the
/// AEAD seal and the #1066 cleartext gate returns `SKIPPED` before decrypt for
/// every non-selected layer, so a receiver on rung 2 never parses a layer-0
/// packet. Proving base-keyframe delivery would mean decrypting and parsing every
/// layer-0 SCREEN packet — reinstating exactly the per-layer receiver CPU cost
/// #1066 removed. The residual failure mode is bounded and benign: during a
/// PARTIAL priority-drop episode (base packets still trickling) this can still
/// retract once, and the receiver lands on the base rung — which is also the
/// bandwidth-safe response to a 90%-fill condition.
///
/// Note the converse case — base NOT arriving while the selected rung is —
/// yields `None`. That would mean the publisher stopped producing its base rung
/// entirely, which the relay's unconditional layer-0 forwarding makes a
/// publisher-side bug; there is nothing better for this receiver to switch to,
/// and the decoder's own keyframe-less escalation still runs.
///
/// A backwards clock step (`now_ms < starved_since_ms`) saturates to a 0-age
/// hold and yields `None` rather than tripping either bound.
pub fn screen_starvation_action(
    selected_layer: u32,
    starved_since_ms: Option<u64>,
    base_rung_arriving: bool,
    now_ms: u64,
    retract_after_ms: u64,
    stale_after_ms: u64,
) -> ScreenStarvationAction {
    let Some(since) = starved_since_ms else {
        return ScreenStarvationAction::None;
    };
    let age_ms = now_ms.saturating_sub(since);
    if age_ms > stale_after_ms {
        return ScreenStarvationAction::Rearm;
    }
    if selected_layer > 0 && base_rung_arriving && age_ms >= retract_after_ms {
        ScreenStarvationAction::Retract
    } else {
        ScreenStarvationAction::None
    }
}

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
    /// Layers this receiver has RETRACTED until the given wall-clock ms, despite
    /// their packets still arriving (issue #2328). Arrival-based availability
    /// cannot see a rung that delivers deltas but no decodable keyframe; this is
    /// the override that lets the receiver stop targeting one. Expiry is purely
    /// time-based, so a quarantine can never become permanent — see
    /// [`SCREEN_KEYFRAME_STARVED_QUARANTINE_MS`].
    quarantined_until_ms: std::collections::HashMap<u32, u64>,
}

impl LayerAvailability {
    /// Default availability window. Generous relative to the sender's frame
    /// cadence so a momentary gap (a few dropped frames, a keyframe-only lull)
    /// does not retract a layer, but short enough that a genuinely-shed top
    /// layer is forgotten within a few seconds.
    pub const DEFAULT_WINDOW_MS: u64 = videocall_aq::constants::LAYER_AVAILABILITY_WINDOW_MS;

    pub fn new() -> Self {
        Self::with_window(Self::DEFAULT_WINDOW_MS)
    }

    pub fn with_window(window_ms: u64) -> Self {
        Self {
            last_seen_ms: std::collections::HashMap::new(),
            window_ms,
            quarantined_until_ms: std::collections::HashMap::new(),
        }
    }

    /// Record that a packet tagged `layer_id` arrived from this source at `now`.
    pub fn observe(&mut self, layer_id: u32, now_ms: u64) {
        self.last_seen_ms.insert(layer_id, now_ms);
    }

    /// Retract `layer_id` from this receiver's view until `until_ms`, even while
    /// its packets keep arriving (issue #2328).
    ///
    /// Used when the rung is delivering DELTAS but no decodable keyframe, which
    /// arrival-based availability cannot distinguish from health. Extends (never
    /// shortens) an existing quarantine, so a rung that is still starved when the
    /// receiver re-probes it simply serves another full window rather than
    /// flapping. Layer 0 is deliberately still accepted here — it is the caller
    /// (`screen_starvation_action`) that refuses to retract the base rung, and
    /// keeping this primitive unconditional keeps it honest for future callers.
    pub fn quarantine(&mut self, layer_id: u32, until_ms: u64) {
        let entry = self
            .quarantined_until_ms
            .entry(layer_id)
            .or_insert(until_ms);
        *entry = (*entry).max(until_ms);
    }

    /// Whether `layer_id` is currently retracted as of `now_ms`.
    pub fn is_quarantined(&self, layer_id: u32, now_ms: u64) -> bool {
        self.quarantined_until_ms
            .get(&layer_id)
            .is_some_and(|&until| now_ms < until)
    }

    /// Highest layer id observed within the window as of `now`, EXCLUDING any
    /// currently-quarantined rung. Returns 0 when nothing qualifies (base-only /
    /// un-upgraded publisher), which is the bandwidth-safe default. Expired
    /// observations and expired quarantines are both pruned lazily.
    ///
    /// The quarantine is applied HERE, at the single choke point every consumer
    /// already reads (the chooser tick, the #1179 early seed, the
    /// LAYER_PREFERENCE publisher and the diagnostics snapshots), so a retraction
    /// cannot be seen by some of them and missed by others.
    pub fn highest_available(&mut self, now_ms: u64) -> u32 {
        let window = self.window_ms;
        self.last_seen_ms
            .retain(|_, &mut seen| now_ms.saturating_sub(seen) <= window);
        self.quarantined_until_ms
            .retain(|_, &mut until| now_ms < until);
        self.last_seen_ms
            .keys()
            .copied()
            .filter(|layer| !self.quarantined_until_ms.contains_key(layer))
            .max()
            .unwrap_or(0)
    }

    /// Whether `layer_id` is within the availability window as of `now_ms` and
    /// not quarantined, without pruning. Read-only so `&self` diagnostic paths
    /// can ask; the pruning variant above stays the one selection uses.
    pub fn layer_available_peek(&self, layer_id: u32, now_ms: u64) -> bool {
        !self.is_quarantined(layer_id, now_ms)
            && self
                .last_seen_ms
                .get(&layer_id)
                .is_some_and(|&seen| now_ms.saturating_sub(seen) <= self.window_ms)
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

    // --- Screen layer-oscillation damping (issue #1899) ---
    /// SCREEN kind only: enables the yo-yo oscillation latch below. `false` for
    /// camera VIDEO and AUDIO, which are provably unaffected (the latch and the
    /// `last_up_switch_ms` bookkeeping are inert). Set at construction via
    /// [`LayerChooser::new_screen`]; the camera↔screen divergence is deliberate.
    screen_mode: bool,
    /// Monotonic: `true` once ANY congested window has been observed. Gates the
    /// recording of `last_up_switch_ms` so a pristine cold-start acquisition
    /// (0 → highest_available on the first clean tick, before any congestion) is
    /// NOT mistaken for an oscillation re-climb — preserving the #1079 M2 "keep
    /// full quality at join" behavior for screen. Only the yo-yo latch reads it.
    ever_congested: bool,
    /// Timestamp (ms) of the most recent UP switch, recorded only after
    /// `ever_congested` (issue #1899). `0` = none yet. A DOWN switch within
    /// [`SCREEN_LAYER_OSCILLATION_LATCH_MS`] of this is the screen yo-yo that
    /// force-latches the sticky floor. Consumed (reset to 0) on a latch.
    last_up_switch_ms: u64,
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
            // Camera/audio default: the screen oscillation latch is inert.
            screen_mode: false,
            ever_congested: false,
            last_up_switch_ms: 0,
        }
    }

    /// Construct a SCREEN-kind chooser (issue #1899): identical to [`Self::new`]
    /// except the yo-yo oscillation latch is enabled (`screen_mode = true`). A
    /// static screen share re-arms a keyframe wait on every layer switch, which
    /// on scarce-keyframe static content self-oscillates; this mode force-latches
    /// the existing sticky floor when a DOWN switch closely follows an UP switch,
    /// so the share settles to one rung. Camera VIDEO / AUDIO use [`Self::new`]
    /// and are unaffected — the divergence is deliberate (see the module-level
    /// oscillation-damping note).
    pub fn new_screen(now_ms: u64) -> Self {
        Self {
            screen_mode: true,
            ..Self::new(now_ms)
        }
    }

    /// The currently-selected DECODE layer (the decode-guard value).
    pub fn current(&self) -> u32 {
        self.current
    }

    /// Whether the chooser is ACTIVELY holding `current` BELOW the highest
    /// available layer because of observed downlink congestion (issue #1079 M2).
    ///
    /// This is the same `constrained` flag that gates [`Self::desired_preference`]
    /// (which returns `Some` iff constrained), exposed under a self-documenting
    /// name for the degradation-reason derivation (issue #1131): a `Network`
    /// reason is only attributable when the chooser is genuinely holding down due
    /// to congestion, not when the user/sender capped quality. Cheap getter.
    pub fn is_constrained(&self) -> bool {
        self.constrained
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
            // Monotonic marker (issue #1899): once congestion has been seen, later
            // UP switches are eligible to record `last_up_switch_ms` for the screen
            // yo-yo latch. Kept out of the cold-start acquisition path so a pristine
            // 0 → top climb (no prior congestion) never arms the latch.
            self.ever_congested = true;
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
                // Screen yo-yo latch (issue #1899): this is the arm the static-share
                // oscillation lands in — after `constrained` cleared and the
                // unconstrained follow-top re-climbed, the switch-induced PLI shows
                // up here as the re-constrain. If it closely follows that re-climb,
                // force-latch the sticky floor so we stop re-climbing into the same
                // keyframe-wait freeze. No-op for camera/audio.
                self.maybe_latch_screen_oscillation(now_ms);
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
            // Screen yo-yo latch (issue #1899): also cover the purely-constrained
            // oscillation (conservative-up climb to the top, keyframe-wait PLI,
            // then this fast step-down) so a static share that never fully clears
            // `constrained` still settles. No-op for camera/audio.
            self.maybe_latch_screen_oscillation(now_ms);
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
            // Issue #1899: remember the time of an UP switch so a closely-following
            // DOWN switch can be recognized as the screen yo-yo. Gated on
            // `ever_congested` so a pristine cold-start acquisition (0 → top before
            // any congestion) is NOT armed — only re-climbs after real congestion
            // count. Recorded for every kind but read only by the screen-gated
            // latch, so camera/audio behavior is unchanged.
            if layer > self.current && self.ever_congested {
                self.last_up_switch_ms = now_ms;
            }
            self.current = layer;
            self.last_change_ms = now_ms;
        }
    }

    /// Screen-only oscillation guard (issue #1899): force the sticky latch when a
    /// step DOWN closely follows a step UP — the static-share yo-yo.
    ///
    /// Called from the two congested step-down arms of [`Self::choose`] AFTER the
    /// layer has been lowered. On a static screen share every layer switch re-arms
    /// a keyframe wait; the new layer cannot paint until its (scarce) keyframe
    /// arrives, so playout freezes and storms PLIs, which reads as congestion and
    /// drives the next step down — a self-sustaining oscillation the decaying
    /// `congestion_score` never latches on because the clean lulls between
    /// switch-induced PLI windows decay it below [`STICKY_CONGESTION_EVENTS`].
    ///
    /// When the just-completed DOWN follows the most recent UP within
    /// [`SCREEN_LAYER_OSCILLATION_LATCH_MS`], latch the EXISTING sticky floor at
    /// the layer we just dropped to. The chooser then holds that rung (climb capped
    /// at the floor, `constrained` never cleared) and recovers ONLY via the
    /// time-bounded [`STICKY_RECOVERY_CLEAN_TICKS`] path — so it settles to one rung
    /// yet cannot wedge (a genuinely-recovered link still climbs out ~one rung per
    /// ~60s of uninterrupted clean).
    ///
    /// No-op for camera VIDEO / AUDIO (`screen_mode == false`): the divergence is
    /// deliberate — camera has continuous motion and does not keyframe-starve on a
    /// switch the way a static screen does, so its normal fast-down / conservative-up
    /// adaptation must remain untouched.
    fn maybe_latch_screen_oscillation(&mut self, now_ms: u64) {
        if !self.screen_mode {
            return;
        }
        let recent_up = self.last_up_switch_ms != 0
            && now_ms.saturating_sub(self.last_up_switch_ms) <= SCREEN_LAYER_OSCILLATION_LATCH_MS;
        if !recent_up {
            return;
        }
        // Force the sticky latch at the just-dropped layer, mirroring the pin the
        // score-driven latch performs: sticky on, floor = current, recovery reset,
        // and the score raised to the latch threshold so the state stays consistent
        // (sticky implies chronic). This reuses the #1179 hold-and-recover machinery
        // wholesale — nothing about the recovery path changes.
        self.sticky = true;
        self.sticky_floor = self.current;
        self.recovery_clean_ticks = 0;
        self.congestion_score = self.congestion_score.max(STICKY_CONGESTION_EVENTS);
        // Consume the up-switch so a single re-climb cannot latch twice.
        self.last_up_switch_ms = 0;
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
///   * audio  — layers `0..=2`; the receive UI offers only `0` (#2279)
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
    /// Why this stream is below the FULL-ladder top, when it is (issue #1131).
    /// `None` when the reception is optimal (top of the full ladder, or a
    /// single-rung ladder). Set by the snapshot producers via [`degrade_reason`];
    /// the bare [`received_layer_snapshot`] resolver leaves it `None` (callers
    /// that know the user bound / chooser state fill it in). The UI shows a tinted
    /// reason chip only when this is `Some`.
    pub reason: Option<DegradeReason>,
}

/// Why a received stream is below the full simulcast ladder's top layer (issue
/// #1131). Mutually exclusive; the *tightest binding limit* wins, with an exact
/// tie broken by [`degrade_reason`]'s precedence (Setting > Network > Sender).
///
/// Only meaningful when the reception is NOT optimal — see [`quality_state`]; an
/// optimal stream carries `reason == None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradeReason {
    /// Your downlink can't sustain a higher layer right now (chooser is actively
    /// constrained below what the sender offers).
    Network,
    /// You capped receive quality below the maximum for this stream (your own
    /// receive `max` bound is the binding limit).
    Setting,
    /// The sender simply isn't publishing a higher layer; you are taking
    /// everything offered. Includes non-simulcast peers (you genuinely receive
    /// low quality from them — not your fault).
    Sender,
}

/// Absolute reception quality of a decoded layer relative to the FULL simulcast
/// ladder for its kind (issue #1131). "Full ladder" means
/// [`max_layers_for_kind`] — NOT the empirically-learned `highest_available + 1`
/// — so a stream a sender pins to base still reads as `Low` (red), matching the
/// issue's "low quality in red" intent rather than going green just because the
/// sender's offered top happens to coincide with what you decode.
///
/// * top of the full ladder (`layer_index >= full_ladder_len - 1`) → `Optimal`
/// * base of a multi-rung ladder (`layer_index == 0 && full_ladder_len > 1`) → `Low`
/// * anything between → `Medium`
/// * a single-rung ladder (`full_ladder_len <= 1`) → `Optimal` (nothing better exists)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityState {
    Optimal,
    Medium,
    Low,
}

/// Classify a decoded layer's absolute quality against the full ladder length.
/// Pure / panic-safe; see [`QualityState`] for the rules. Single source of truth
/// for the receive quality color in the perf panel.
pub fn quality_state(layer_index: u32, full_ladder_len: u32) -> QualityState {
    if full_ladder_len <= 1 {
        // Nothing better exists; decoding the only rung is optimal.
        return QualityState::Optimal;
    }
    let top = full_ladder_len - 1;
    if layer_index >= top {
        QualityState::Optimal
    } else if layer_index == 0 {
        QualityState::Low
    } else {
        QualityState::Medium
    }
}

/// Derive the [`DegradeReason`] for a received stream that is below the full
/// ladder top (issue #1131). Returns `None` when the reception is optimal
/// (caller should pass the real state in; this fn also returns `None` when no
/// limit is attributable). PURE — host-tested — so the heuristic is not duplicated
/// inline at the two snapshot producers.
///
/// Inputs (all on the DIRECT layer-index convention, 0 = base/lowest):
///   * `sel` — the post-clamp selected/decoded layer.
///   * `avail_top` — highest layer the sender is currently offering
///     (`highest_available`).
///   * `full_ladder_top` — `max_layers_for_kind(kind) - 1`.
///   * `user_max` — the user's receive `max` bound for this kind (`None` =
///     Auto/uncapped).
///   * `constrained` — the chooser's [`LayerChooser::is_constrained`] flag.
///
/// Precedence: the TIGHTEST binding limit wins; on an exact tie prefer
/// **Setting > Network > Sender** (an explicit user choice is the most
/// informative attribution, then your network, then the sender). Concretely:
///   * **Setting** — `user_max == Some(m)`, `m < full_ladder_top`, and `sel == m`
///     (your own cap is what's holding you down).
///   * **Network** — `constrained` AND `sel < full_ladder_top` (the chooser is
///     ACTIVELY holding `sel` below the achievable top because of observed
///     downlink congestion, so YOUR network is the proximate cause). We test
///     against `full_ladder_top`, NOT `avail_top`, on purpose (issue #1553): when
///     the receiver is constrained it stops PULLING the higher layers, the relay
///     stops forwarding them to this receiver, and so `avail_top`
///     (`highest_available` = highest layer OBSERVED arriving recently) decays
///     toward `sel`. The old `sel < avail_top` test therefore went FALSE the
///     moment `avail_top` collapsed to `sel` and fell through to **Sender**,
///     falsely blaming the publisher for the receiver's OWN downlink constraint.
///     `avail_top` is not a trustworthy proxy for "what the sender publishes"
///     precisely WHEN the receiver is constrained. The `constrained` flag is the
///     reliable disambiguator: it is held true only while the chooser is actively
///     downlink-limited and is cleared the moment `current` reaches the offered
///     ceiling (`choose`), so a genuine sender limit never reads `constrained`.
///   * **Sender** — `constrained == false`, `avail_top < full_ladder_top`, AND
///     `sel == avail_top` (you're NOT downlink-limited, you're taking everything
///     offered, and the sender just isn't publishing higher). A non-simulcast
///     peer (`avail_top == 0`, `sel == 0`, `constrained == false`) decoded at
///     base lands here.
pub fn degrade_reason(
    sel: u32,
    avail_top: u32,
    full_ladder_top: u32,
    user_max: Option<u32>,
    constrained: bool,
) -> Option<DegradeReason> {
    // Already at (or above) the full ladder top → optimal, no reason.
    if sel >= full_ladder_top {
        return None;
    }

    // Setting wins outright when the user's own cap is the binding limit, since a
    // tie must resolve Setting > Network > Sender and this is the only branch that
    // attributes the user's explicit choice.
    let setting = matches!(user_max, Some(m) if m < full_ladder_top && sel == m);
    if setting {
        return Some(DegradeReason::Setting);
    }

    // Network: the chooser is ACTIVELY holding `sel` below the achievable top
    // because of observed downlink congestion (issue #1553). We test against
    // `full_ladder_top`, NOT `avail_top`: when this receiver is constrained it
    // stops pulling the higher layers, so `avail_top` (= highest layer recently
    // OBSERVED) decays toward `sel`. The old `sel < avail_top` went false the
    // instant `avail_top` collapsed to `sel` and mis-attributed to Sender —
    // falsely blaming the publisher for the receiver's OWN downlink limit. The
    // `constrained` flag (cleared the moment we reach the offered ceiling) is the
    // reliable signal that the receiver's network is the proximate cause.
    if constrained && sel < full_ladder_top {
        return Some(DegradeReason::Network);
    }

    // Sender: you are NOT downlink-constrained, you're taking everything offered,
    // and the sender isn't publishing higher (covers non-simulcast peers where
    // avail_top == 0 and sel == 0). The `constrained` check above already owns the
    // receiver-limited case, so reaching here means avail_top is a trustworthy
    // proxy for the sender's actual top.
    if avail_top < full_ladder_top && sel == avail_top {
        return Some(DegradeReason::Sender);
    }

    // Below the top but none of the specific limits is attributable (e.g. a
    // transient state where the chooser sits below both the user cap and the
    // sender's top without being flagged constrained). Leave unattributed.
    None
}

/// Audio simulcast bitrates (kbps) by layer, lowest-first (issue #989, Phase 3c
/// / 4; extended to 3 rungs in issue #1082; retuned lighter in issue #1768).
/// Mirrors the publisher's 3-layer model (low 12 / mid 24 / high 48). Kept here
/// so the snapshot resolver has no dependency on the encoder module. This
/// slice's length is the single source of truth for the receiver-side audio
/// ladder size (see [`AUDIO_LAYER_CAP`]).
const AUDIO_LAYER_KBPS: &[u32] = &[12, 24, 48];

/// Length of the receiver-side audio layer ladder, exposed as a `const fn` so
/// the publisher (`microphone_encoder.rs`) can tie its own ladder to this with a
/// compile-time assert and the two can never silently diverge (issue #1077).
pub const fn audio_layer_kbps_len() -> usize {
    AUDIO_LAYER_KBPS.len()
}

/// Nominal kbps of the BASE received-audio layer (layer 0, the rung the relay's
/// #989 layer filter never drops).
///
/// This is a LADDER nominal, not an observed bitrate: a publisher emitting one
/// audio layer runs at its own AQ audio tier, so layer 0's arrival says nothing
/// about the rate. No UI readout may derive a remote peer's audio bitrate from it.
pub const fn base_audio_layer_kbps() -> u32 {
    AUDIO_LAYER_KBPS[0]
}

/// Nominal kbps of a SPECIFIC received-audio rung, or `None` if `layer` is off
/// the ladder. This is what a per-peer readout needs (#2132); the base-only
/// accessor above under-reports whenever the receiver selected rung 1 or 2.
pub fn audio_layer_kbps(layer: u32) -> Option<u32> {
    AUDIO_LAYER_KBPS.get(layer as usize).copied()
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
/// used by the snapshot resolver and the decode path's off-ladder rejection.
pub fn max_layers_for_kind(kind: PrefMediaKind) -> u32 {
    match kind {
        // Video and Screen share the same value today but are independent arms
        // (issue #1082) so a future per-kind divergence is a one-line change.
        PrefMediaKind::Video => VIDEO_LAYER_CAP,
        PrefMediaKind::Screen => SCREEN_LAYER_CAP,
        PrefMediaKind::Audio => AUDIO_LAYER_CAP,
    }
}

/// A per-peer rendered-tile-size hint pushed by the UI (issue #1256 Phase 1).
/// `Capped { device_px_h }` = the tile is a fixed-size grid thumbnail of this
/// device-pixel height, so the receiver may LID the requested simulcast layer to
/// the smallest layer whose native height covers the tile. `Uncapped` = the tile
/// is pinned / a screen-share panel (or the peer is unknown), so no size lid is
/// applied and the chooser's full downlink-driven selection stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileHint {
    Capped { device_px_h: u32 },
    Uncapped,
}

/// Margin by which a tile may exceed a layer's native height before the next
/// layer up is forced (issue #1256). The tile may be up to 10% taller than a
/// layer's native height before forcing the next layer up — absorbs boundary
/// hover/flap so a tile sitting right at a layer boundary does not oscillate.
pub const SIZE_CAP_MARGIN: f64 = 0.10;

/// Map a rendered device-pixel tile height to the smallest simulcast layer index
/// that "covers" it (issue #1256 Phase 1). Pure + host-tested.
///
/// **This is the layer SELECTION path.** Simulcast in this codebase is EXACT-MATCH
/// — a guard/relay layer-id mismatch DROPS packets and freezes video — so the lid
/// resolves rung heights from the one camera ladder, never from anything a
/// deployment can vary.
///
/// Returns the smallest layer `i` in `0..=highest_available` whose native height
/// (from [`received_layer_snapshot`]) times `(1.0 + SIZE_CAP_MARGIN)` is >=
/// `tile_h_px`. If no layer satisfies it (tile taller than the top layer) returns
/// `highest_available`. Always within `[0, highest_available]`.
///
/// - AUDIO is never size-capped: returns `highest_available` unconditionally
///   (defensive; the caller never invokes this for audio).
/// - `tile_h_px == 0` (unknown size) returns `highest_available` (don't cap).
///
/// Re-exported at the crate root so `dioxus-ui` can verify its lid values against this
/// function rather than restating the layer heights.
pub fn size_cap_layer(
    tile_h_px: u32,
    highest_available: u32,
    layer_count: u32,
    kind: PrefMediaKind,
) -> u32 {
    if matches!(kind, PrefMediaKind::Audio) || tile_h_px == 0 {
        return highest_available;
    }
    for i in 0..=highest_available {
        let native_h = received_layer_snapshot(kind, i, layer_count).height;
        if native_h as f64 * (1.0 + SIZE_CAP_MARGIN) >= tile_h_px as f64 {
            return i;
        }
    }
    highest_available
}

/// Resolve a [`ReceivedLayerSnapshot`] for `kind` at the given decoded
/// `layer_index`, mapping the layer to its resolution/bitrate via the per-kind
/// ladder (issue #989, Phase 4). `layer_count` is the number of layers the
/// source ladder is producing (>= 1). Pure + panic-safe: `layer_index` and
/// `layer_count` are clamped into range, so an explicit 1-layer (flag-off) input
/// always yields a valid layer-0 snapshot.
///
/// Used for both layer SELECTION (notably [`size_cap_layer`]'s #1256 tile-size lid)
/// and the readouts the user sees, because there is one camera ladder:
/// `videocall_aq::constants::simulcast_layers` is the single source of ladder truth.
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

    // AUDIO and SCREEN carry no ladder geometry OR bitrate. Screen's is enriched from
    // the publisher's own reported target (`enrich_screen_snapshot`); audio has no
    // such report, and its rate is set by the publisher's AQ audio tier rather than
    // by the layer a receiver decodes.
    if audio || matches!(kind, PrefMediaKind::Screen) {
        return ReceivedLayerSnapshot {
            kind,
            layer_index: idx,
            layer_count: count,
            width: 0,
            height: 0,
            kbps: 0,
            reason: None,
        };
    }

    let tiers = crate::adaptive_quality_constants::simulcast_layers(count as usize);
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
        // The bare resolver doesn't know the user bound / chooser state, so it
        // leaves the reason unset; the snapshot producers fill it in.
        reason: None,
    }
}

/// Build a [`ReceivedLayerSnapshot`] for a peer's `kind` AND attach its
/// degradation `reason` from one consistent layer (issue #1131 follow-up B).
///
/// This is the exact assembly the per-peer snapshot producer performs, lifted to
/// a PURE fn so the "derive the reason from the CLAMPED decoded layer, not the
/// raw selected layer" contract is host-testable without constructing a full
/// `Peer`. `raw_selected` is the peer's `selected_*_layer` (which a receive `min`
/// can raise ABOVE what the sender offers); the resolver clamps it to
/// `min(raw_selected, avail_top)` (its `count` is `avail_top + 1`), and the
/// reason is derived from THAT clamped `layer_index` so the row's quality dot and
/// its reason chip always agree. `user_max` / `constrained` feed
/// [`degrade_reason`].
///
/// **DISPLAY-ONLY** — every caller is inside
/// `PeerDecodeManager::per_peer_received_snapshots`, whose output drives readouts
/// (the perf panel's per-peer rows, the diagnostics drawer, the signal popup) and no
/// selection decision.
pub fn received_layer_snapshot_with_reason(
    kind: PrefMediaKind,
    raw_selected: u32,
    avail_top: u32,
    user_max: Option<u32>,
    constrained: bool,
) -> ReceivedLayerSnapshot {
    let mut snap = received_layer_snapshot(kind, raw_selected, avail_top + 1);
    let full_ladder_top = max_layers_for_kind(kind).saturating_sub(1);
    // Derive from the CLAMPED layer the snapshot actually carries, never the raw
    // selected layer — see the fn doc.
    snap.reason = degrade_reason(
        snap.layer_index,
        avail_top,
        full_ladder_top,
        user_max,
        constrained,
    );
    snap
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
    // Issue #1899: screen layer-oscillation damping (yo-yo latch)
    //
    // On a STATIC screen share, every layer switch re-arms a keyframe wait that
    // freezes playout and storms PLIs; that reads as congestion and forces the
    // next switch, so the share self-oscillates (563 switches in one field
    // meeting). The SCREEN chooser (`new_screen`) treats a DOWN switch that
    // closely follows an UP switch as chronic and force-latches the existing
    // sticky floor, so the share SETTLES. Camera/audio (`new`) are unaffected.
    // -----------------------------------------------------------------

    /// Drive the exact "kick → clear → re-climb → drop" sequence that a static
    /// screen share produces at the chooser boundary, using `avail` to model the
    /// availability feedback (the top is forgotten while we advertise a preference,
    /// and re-learned once we fail-open). Returns the chooser after the yo-yo DOWN.
    fn drive_screen_yo_yo(c: &mut LayerChooser) {
        // t=1000 clean, avail=2 → cold-start acquire of the top (0→2). No prior
        // congestion, so this UP must NOT arm the yo-yo (M2 cold-start preserved).
        c.choose(clean(), 2, 1000);
        assert_eq!(c.current(), 2, "acquires the top on the first clean window");
        // t=2000 congested, avail=2 → the initial kick down to 1 (constrained).
        // First-ever congestion: NOT a yo-yo (no prior UP was armed).
        c.choose(congested(), 2, 2000);
        assert_eq!(c.current(), 1, "a congested window kicks down one rung");
        // t=3000 clean, avail=1 → availability forgot the top (we advertise pref=1),
        // so `current >= highest` clears `constrained` (back to no-preference).
        c.choose(clean(), 1, 3000);
        // t=4000 clean, avail=2 → relay fail-opened, the top is re-learned, and the
        // unconstrained follow-top RE-CLIMBS 1→2. This UP arms the yo-yo.
        c.choose(clean(), 2, 4000);
        assert_eq!(
            c.current(),
            2,
            "unconstrained follow-top re-climbs to the top"
        );
        // t=6000 congested, avail=2 → the re-climb's keyframe-wait PLI shows up as a
        // congested window and drops us back to 1 — the yo-yo DOWN, 2s after the UP.
        c.choose(congested(), 2, 6000);
        assert_eq!(c.current(), 1, "the switch-induced PLI drops us back down");
    }

    #[test]
    fn screen_yo_yo_down_after_up_latches_sticky() {
        // The core regression: on the SCREEN chooser, a DOWN that closely follows
        // an UP force-latches the sticky floor so the share stops re-climbing into
        // the same keyframe-wait freeze and settles.
        //
        // MUTATION CHECK: fails on the un-fixed code — without the yo-yo latch the
        // congestion score after this sequence is only 1 (well below
        // STICKY_CONGESTION_EVENTS=3), so the chooser would NOT be sticky and would
        // keep oscillating. Reverting `maybe_latch_screen_oscillation` (or flipping
        // `screen_mode` off) breaks this assertion.
        let mut c = LayerChooser::new_screen(0);
        drive_screen_yo_yo(&mut c);
        assert!(
            c.is_sticky(),
            "a screen yo-yo (down right after up) must latch sticky (#1899)"
        );
        assert_eq!(
            c.sticky_floor(),
            1,
            "the floor is pinned to the settled rung we dropped to"
        );
        // Now settled: further clean windows must NOT climb back above the floor,
        // so no more switch-induced freezes. (avail=2 = the real top is available,
        // but the sticky floor caps the climb at 1.)
        let mut t = 7000u64;
        for _ in 0..STEP_UP_CLEAN_WINDOWS + 2 {
            c.choose(clean(), 2, t);
            t += 1100;
        }
        assert_eq!(
            c.current(),
            1,
            "a settled screen share holds its rung (no re-climb into the freeze)"
        );
    }

    #[test]
    fn camera_yo_yo_does_not_latch_sticky() {
        // Scoping guard: the SAME yo-yo sequence on the CAMERA chooser (`new`) must
        // NOT latch — the damping is screen-only (deliberate camera↔screen
        // divergence). Camera keeps its normal fast-down / conservative-up loop.
        //
        // MUTATION CHECK: fails if the latch is not gated on `screen_mode` (i.e. if
        // it fired for every kind).
        let mut c = LayerChooser::new(0);
        drive_screen_yo_yo(&mut c);
        assert!(
            !c.is_sticky(),
            "camera must NOT latch on a yo-yo — the damping is screen-scoped"
        );
    }

    #[test]
    fn screen_cold_start_first_congestion_does_not_latch() {
        // The `ever_congested` gate: a pristine cold-start acquisition (0→top on the
        // first clean window, before ANY congestion) must NOT arm the yo-yo, so the
        // first-ever congested window is a plain fast-down, not a latch. This
        // preserves the #1079 M2 "keep full quality at join" intent for screen.
        //
        // MUTATION CHECK: fails if `set_layer` records `last_up_switch_ms` without
        // the `ever_congested` guard — then the cold-start acquire would arm the
        // latch and this first congestion would wrongly stick.
        let mut c = LayerChooser::new_screen(0);
        c.choose(clean(), 2, 1000); // cold-start acquire 0→2 (unarmed)
        assert_eq!(c.current(), 2);
        c.choose(congested(), 2, 2000); // first-ever congestion → plain fast-down
        assert_eq!(c.current(), 1, "first congestion steps down one rung");
        assert!(
            !c.is_sticky(),
            "cold-start acquire + first congestion must NOT latch (M2 preserved)"
        );
    }

    #[test]
    fn screen_latched_floor_recovers_time_bounded_no_wedge() {
        // No-wedge requirement: after the yo-yo latch, a genuinely-recovered link
        // must still climb out via the SAME time-bounded STICKY_RECOVERY_CLEAN_TICKS
        // path — the latch settles the share but cannot pin it forever.
        //
        // MUTATION CHECK: fails if the forced latch left the sticky machinery in an
        // inconsistent state that blocks recovery (e.g. never raising the floor).
        let mut c = LayerChooser::new_screen(0);
        drive_screen_yo_yo(&mut c);
        assert!(c.is_sticky());
        assert_eq!(c.sticky_floor(), 1);
        // One uninterrupted recovery period of clean windows (avail=2 = real top)
        // raises the floor one rung to the top → sticky clears.
        let mut t = 7000u64;
        for _ in 0..STICKY_RECOVERY_CLEAN_TICKS {
            c.choose(clean(), 2, t);
            t += 1100;
        }
        assert!(
            !c.is_sticky(),
            "sustained clean must clear the latch (time-bounded recovery, no wedge)"
        );
        // And the chooser can now climb back to the top.
        for _ in 0..10 {
            c.choose(clean(), 2, t);
            t += 1100;
        }
        assert_eq!(c.current(), 2, "recovered link climbs back to the top");
        assert_eq!(
            c.desired_preference(),
            None,
            "back at the top → no preference"
        );
    }

    #[test]
    fn screen_oscillation_input_settles_camera_does_not() {
        // Before/after regression (the #1899 brief's "N switches before, few after").
        // Feed the IDENTICAL self-coupled oscillating input to a SCREEN chooser and a
        // CAMERA chooser and compare. The camera (baseline = un-fixed behavior) keeps
        // yo-yoing every few windows; the screen chooser latches after the first
        // yo-yo and then holds one rung for long stretches, re-probing only on the
        // time-bounded sticky-recovery cadence (the deliberate no-wedge exit) — so it
        // switches far less and its longest settled run is far longer.
        //
        // MUTATION CHECK: with the latch removed the screen chooser behaves exactly
        // like the camera one, so the counts/runs are equal and the strict `<` / `>`
        // comparisons fail.
        //
        // Returns (total layer switches, longest run of consecutive windows with NO
        // switch). The run length is the metric that matters for freeze-per-minute:
        // a settled static share holds its rung for a long uninterrupted stretch.
        fn run_oscillation(c: &mut LayerChooser) -> (u32, u32) {
            let avail = 2;
            let mut t = 1000u64;
            let mut switches = 0u32;
            let mut prev = c.current();
            let mut cur_run = 0u32;
            let mut longest_run = 0u32;
            // 40 periods. Each period is 4 clean windows (enough to satisfy the
            // 3-window streak + 3s dwell so the constrained loop climbs a rung), then
            // — ONLY if the chooser actually climbed this period — one congested
            // window modeling the switch-induced keyframe-wait PLI the up-switch
            // re-armed. No up-switch ⇒ no fresh keyframe wait ⇒ that window is clean:
            // this self-coupling is the whole mechanism (a settled share stops
            // generating the very congestion that drove the oscillation).
            for _ in 0..40 {
                let period_start = c.current();
                for i in 0..5 {
                    let sample = if i == 4 && c.current() > period_start {
                        congested()
                    } else {
                        clean()
                    };
                    let now = c.choose(sample, avail, t);
                    if now != prev {
                        switches += 1;
                        cur_run = 0;
                    } else {
                        cur_run += 1;
                        longest_run = longest_run.max(cur_run);
                    }
                    prev = now;
                    t += 1100;
                }
            }
            (switches, longest_run)
        }

        let mut screen = LayerChooser::new_screen(0);
        let mut camera = LayerChooser::new(0);
        let (screen_switches, screen_run) = run_oscillation(&mut screen);
        let (camera_switches, camera_run) = run_oscillation(&mut camera);

        // The damped screen chooser switches materially less than the un-damped
        // baseline over the same input…
        assert!(
            screen_switches * 2 <= camera_switches,
            "screen damping must at least halve the switch count vs the un-damped \
             camera baseline (screen={screen_switches}, camera={camera_switches})"
        );
        // …and holds a far longer uninterrupted settled stretch (the actual UX win:
        // long freeze-free runs punctuated only by the time-bounded no-wedge probe).
        assert!(
            screen_run > camera_run * 2,
            "a settled screen share must hold its rung far longer than the churning \
             baseline (screen_run={screen_run}, camera_run={camera_run})"
        );
        // Sanity: the un-damped baseline really does churn (guards against the test
        // passing because BOTH settled for some unrelated reason).
        assert!(
            camera_switches >= 20,
            "the un-damped baseline must keep oscillating, got {camera_switches}"
        );
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
        // Base layer (0) = lowest resolution (issue #1768: 320x180).
        let base = received_layer_snapshot(PrefMediaKind::Video, 0, 3);
        assert_eq!((base.width, base.height), (320, 180));
        assert!(base.kbps < s.kbps, "base bitrate < top bitrate");
    }

    /// MUTATION: resolve this arm from `simulcast_screen_layers` again.
    #[test]
    fn snapshot_screen_carries_no_ladder_geometry() {
        for raw_count in [1u32, 3] {
            let s = received_layer_snapshot(PrefMediaKind::Screen, 0, raw_count);
            assert_eq!((s.width, s.height), (0, 0));
            assert_eq!(s.kbps, 0);
            assert_eq!(s.layer_count, 1);
            assert_eq!(s.layer_index, 0);
        }
    }

    /// MUTATION: resolve this arm from `AUDIO_LAYER_KBPS` again.
    #[test]
    fn snapshot_audio_carries_no_ladder_geometry_or_bitrate() {
        // The decoded layer id does not determine a publisher's audio bitrate, so no
        // layer may resolve to its ladder nominal. Nominals read from the production
        // table so a retune moves with it.
        for layer in 0..AUDIO_LAYER_KBPS.len() as u32 {
            let s = received_layer_snapshot(PrefMediaKind::Audio, layer, 3);
            assert_eq!((s.width, s.height), (0, 0));
            assert_eq!(s.kbps, 0, "layer {layer} must report no bitrate");
            assert_ne!(
                s.kbps, AUDIO_LAYER_KBPS[layer as usize],
                "layer {layer} resolved to its ladder nominal again"
            );
        }
        assert_eq!(
            received_layer_snapshot(PrefMediaKind::Audio, 2, 3).layer_count,
            3
        );
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
        // Explicit 1-layer (flag-off) input: layer 0 / base for every kind.
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
    // Issue #1131: quality_state (absolute, full-ladder) + degrade_reason
    // -----------------------------------------------------------------

    #[test]
    fn quality_state_full_ladder_boundaries() {
        // 3-rung full ladder: top (2) optimal, base (0) low, middle (1) medium.
        assert_eq!(quality_state(2, 3), QualityState::Optimal);
        assert_eq!(quality_state(1, 3), QualityState::Medium);
        assert_eq!(quality_state(0, 3), QualityState::Low);
        // Above-top index still clamps to optimal (panic-safe).
        assert_eq!(quality_state(9, 3), QualityState::Optimal);
        // Single-rung ladder: the only rung is optimal (nothing better exists),
        // so a non-simulcast peer's color is NOT red.
        assert_eq!(quality_state(0, 1), QualityState::Optimal);
        // 2-rung ladder: base is low, top is optimal, no medium band.
        assert_eq!(quality_state(0, 2), QualityState::Low);
        assert_eq!(quality_state(1, 2), QualityState::Optimal);
    }

    #[test]
    fn degrade_reason_optimal_is_none() {
        // At the full-ladder top → optimal → no reason, regardless of inputs.
        assert_eq!(degrade_reason(2, 2, 2, Some(1), true), None);
        assert_eq!(degrade_reason(3, 2, 2, None, true), None);
    }

    #[test]
    fn degrade_reason_setting_in_isolation() {
        // User capped receive at layer 1 of a 3-rung ladder (top=2); decoding
        // exactly at the cap, sender offers more (avail_top=2), NOT constrained.
        // Only the user's own setting is binding.
        assert_eq!(
            degrade_reason(1, 2, 2, Some(1), false),
            Some(DegradeReason::Setting)
        );
    }

    #[test]
    fn degrade_reason_network_in_isolation() {
        // Chooser is constrained and holding below what the sender offers
        // (sel 1 < avail_top 2), no user cap. Network owns it.
        assert_eq!(
            degrade_reason(1, 2, 2, None, true),
            Some(DegradeReason::Network)
        );
    }

    #[test]
    fn degrade_reason_sender_in_isolation() {
        // Sender only offers up to layer 1 (avail_top=1 < top=2); we take all of
        // it (sel==avail_top), not constrained, no user cap. Sender owns it.
        assert_eq!(
            degrade_reason(1, 1, 2, None, false),
            Some(DegradeReason::Sender)
        );
    }

    #[test]
    fn degrade_reason_sender_for_non_simulcast_peer() {
        // Non-simulcast peer: avail_top == 0, decoded at base (sel == 0), full
        // ladder top is 2. You genuinely receive low quality from them — that is
        // the SENDER's doing, not your network or setting.
        assert_eq!(
            degrade_reason(0, 0, 2, None, false),
            Some(DegradeReason::Sender)
        );
        // And the absolute color for that is Low (red) on a 3-rung full ladder.
        assert_eq!(quality_state(0, 3), QualityState::Low);
    }

    #[test]
    fn degrade_reason_tiebreak_setting_beats_network_and_sender() {
        // All three limits coincide at sel == 1 (a 3-rung ladder, top=2):
        //   * user_max == 1 == sel        → Setting candidate
        //   * constrained AND sel<avail   → would be Network if avail_top>1...
        //   * avail_top == 1 == sel       → Sender candidate
        // To make all three genuinely live at once, the sender must offer above
        // sel (so Network's `sel < avail_top` holds) AND equal sel (so Sender's
        // `sel == avail_top` holds) — impossible simultaneously. So we test the
        // documented precedence in the two realistic tie shapes:

        // (a) Setting vs Network tie: user cap == sel AND chooser constrained
        // below a higher offered top. Setting must win.
        assert_eq!(
            degrade_reason(1, 2, 2, Some(1), true),
            Some(DegradeReason::Setting),
            "Setting beats Network when the user cap coincides with a constrained hold"
        );

        // (b) Setting vs Sender tie: user cap == sel == avail_top (sender's top
        // also sits at the cap). Setting must win over Sender.
        assert_eq!(
            degrade_reason(1, 1, 2, Some(1), false),
            Some(DegradeReason::Setting),
            "Setting beats Sender when the user cap coincides with the sender's top"
        );

        // (c) Network vs Sender: chooser constrained below a higher offered top —
        // Network wins because Setting isn't binding (no/Auto cap) and Sender's
        // `sel == avail_top` is false (sel < avail_top).
        assert_eq!(
            degrade_reason(1, 2, 2, None, true),
            Some(DegradeReason::Network),
            "Network beats Sender when the chooser holds below what the sender offers"
        );
    }

    #[test]
    fn degrade_reason_user_max_at_or_above_top_is_not_setting() {
        // A user cap that is NOT below the full-ladder top is not a binding
        // setting; if we're at the top it's optimal (None), and if below the top
        // for another reason the Setting branch must not fire spuriously.
        assert_eq!(degrade_reason(2, 2, 2, Some(2), false), None);
        // sel below top, user_max == top (not < top) → Setting must NOT claim it;
        // sender attribution applies instead (avail_top==sel<top).
        assert_eq!(
            degrade_reason(1, 1, 2, Some(2), false),
            Some(DegradeReason::Sender)
        );
    }

    #[test]
    fn dot_and_reason_agree_on_the_clamped_layer() {
        // Issue #1131 follow-up B: the row's dot color and its reason chip must be
        // computed from the SAME clamped layer the snapshot is built with.
        //
        // Scenario: a user sets receive `min` = 2, so the chooser's selected layer
        // is raised to 2, but the sender is BASE-ONLY (avail_top = 0). The snapshot
        // resolver clamps the decoded layer to `min(2, avail_top) = 0`, so the row
        // shows a base/Low (red) dot. The reason MUST therefore be derived from the
        // clamped 0 — yielding a Sender chip — NOT from the raw 2 (which is >= the
        // full top and would wrongly produce `None`, a red dot with no explanation).
        let full_ladder_top = max_layers_for_kind(PrefMediaKind::Video) - 1; // 2
        let avail_top = 0u32;
        let raw_selected = 2u32; // raised by the user's receive `min`

        // The resolver clamps exactly as the producer feeds it: count = avail_top+1.
        let snap = received_layer_snapshot(PrefMediaKind::Video, raw_selected, avail_top + 1);
        assert_eq!(snap.layer_index, 0, "decoded layer is clamped to the base");

        // Color from the clamped layer: Low on a 3-rung full ladder.
        assert_eq!(
            quality_state(snap.layer_index, max_layers_for_kind(PrefMediaKind::Video)),
            QualityState::Low
        );

        // Reason from the SAME clamped layer → Sender (a chip IS present).
        // Deriving from the RAW selected (2 >= full_ladder_top) would give None —
        // the contradictory "red dot, no reason" the fix removes.
        assert_eq!(
            degrade_reason(snap.layer_index, avail_top, full_ladder_top, None, false),
            Some(DegradeReason::Sender),
            "clamped layer must yield a reason so the dot is explained"
        );
        assert_eq!(
            degrade_reason(raw_selected, avail_top, full_ladder_top, None, false),
            None,
            "raw (unclamped) selected layer would wrongly drop the reason — this is the bug guarded against"
        );
    }

    #[test]
    fn snapshot_with_reason_uses_clamped_layer_for_dot_and_reason() {
        // Issue #1131 follow-up B, at the PRODUCER seam: the assembled snapshot's
        // `layer_index` (the dot's color basis) and its `reason` must come from the
        // SAME clamped layer. Receive `min`=2 raises the raw selected to 2, but a
        // base-only sender (avail_top=0) clamps the decoded layer to 0.
        let s = received_layer_snapshot_with_reason(
            PrefMediaKind::Video,
            2, // raw_selected (raised by the user's receive min)
            0, // avail_top (base-only sender)
            None,
            false,
        );
        // Clamped to base — the dot will be Low, and the reason explains it.
        assert_eq!(
            s.layer_index, 0,
            "decoded layer clamped to the sender's offering"
        );
        assert_eq!(
            s.reason,
            Some(DegradeReason::Sender),
            "reason derived from the clamped layer → Sender (a chip is present)"
        );

        // A healthy full-quality stream: top of the full ladder, no reason.
        let top = received_layer_snapshot_with_reason(PrefMediaKind::Video, 2, 2, None, false);
        assert_eq!(top.layer_index, 2);
        assert_eq!(top.reason, None, "optimal reception carries no reason chip");

        // A user cap at the decoded layer below the full top → Setting.
        let capped =
            received_layer_snapshot_with_reason(PrefMediaKind::Video, 1, 2, Some(1), false);
        assert_eq!(capped.layer_index, 1);
        assert_eq!(capped.reason, Some(DegradeReason::Setting));
    }

    #[test]
    fn degrade_reason_setting_requires_sel_at_cap_not_merely_a_cap() {
        // GUARD on the Setting branch's `&& sel == m`: a user cap exists and is
        // below the full top, but the DECODED layer sits BELOW that cap — so the
        // cap is NOT the binding limit and Setting must NOT be attributed. These
        // cases fail if `&& sel == m` is dropped (the branch would wrongly fire on
        // "any cap below top" and steal the attribution from Network/Sender).

        // (a) Network is the real limit: the chooser is constrained and holding
        // sel=0 BELOW what the sender offers (avail_top=2), even though the user
        // also set a cap at 1. Network owns it, NOT Setting.
        assert_eq!(
            degrade_reason(0, 2, 2, Some(1), true),
            Some(DegradeReason::Network),
            "sel below the user cap (not AT it) must attribute Network, not Setting"
        );

        // (b) Sender is the real limit: sel=0 == avail_top=0 (base-only sender),
        // user cap at 1 is above what's offered so it isn't binding. Sender owns
        // it, NOT Setting.
        assert_eq!(
            degrade_reason(0, 0, 2, Some(1), false),
            Some(DegradeReason::Sender),
            "sel below the user cap with a base-only sender must attribute Sender, not Setting"
        );
    }

    #[test]
    fn degrade_reason_collapsed_avail_top_while_constrained_is_network_not_sender() {
        // PINS the issue #1553 fix. The exact field-reported misattribution: a
        // receiver in a busy meeting congests on its OWN downlink, so its chooser
        // holds at base (sel == 0) AND marks itself `constrained`. Because it stops
        // pulling the higher layers, the relay stops forwarding them to this
        // receiver and `avail_top` (= highest layer OBSERVED arriving recently)
        // decays all the way down to 0 — collapsing to equal `sel`.
        //
        // Under the OLD `constrained && sel < avail_top` test this went false
        // (0 < 0 is false) and fell through to the Sender branch
        // (avail_top 0 < full_top 2 && sel 0 == avail_top 0 → true), FALSELY
        // blaming the publisher for the receiver's own downlink limit. The fix
        // tests `constrained && sel < full_ladder_top` (0 < 2 → true), so this is
        // correctly attributed to the receiver's network.
        //
        // This test FAILS (reverts to Sender) if the Network branch is changed back
        // to `sel < avail_top`.
        assert_eq!(
            degrade_reason(0, 0, 2, None, true),
            Some(DegradeReason::Network),
            "constrained receiver pinned to base with a collapsed avail_top is Network, not Sender"
        );

        // The same shape one rung up: held at sel == 1 by congestion with avail_top
        // also collapsed to 1, below the full top 2 → still the receiver's network.
        assert_eq!(
            degrade_reason(1, 1, 2, None, true),
            Some(DegradeReason::Network),
            "constrained hold at sel==avail_top below the full top is Network, not Sender"
        );
    }

    #[test]
    fn degrade_reason_genuine_sender_limit_at_sel_eq_avail_top_unconstrained_is_sender() {
        // The COUNTERWEIGHT to the #1553 fix: a genuinely sender-limited stream
        // must stay Sender so the fix does not over-attribute to Network. The
        // receiver is NOT downlink-limited (`constrained == false`), it is taking
        // everything the sender offers (`sel == avail_top`), and the sender simply
        // isn't publishing higher (`avail_top < full_ladder_top`). Per the chooser
        // lifecycle, `constrained` is cleared the moment `current` reaches the
        // offered ceiling, so a genuine sender limit always presents as
        // `constrained == false` here — the new Network branch never fires for it.
        //
        // This test FAILS if the Network branch is made to ignore `constrained`
        // (e.g. firing on `sel < full_ladder_top` alone): it would steal this case.
        assert_eq!(
            degrade_reason(1, 1, 2, None, false),
            Some(DegradeReason::Sender),
            "unconstrained receiver at sel==avail_top below the full top is Sender, not Network"
        );
        // Base-only (non-simulcast) variant of the same: still Sender.
        assert_eq!(
            degrade_reason(0, 0, 2, None, false),
            Some(DegradeReason::Sender),
            "unconstrained base-only reception is Sender, not Network"
        );
    }

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
        assert_eq!(max_layers_for_kind(PrefMediaKind::Screen), 1);
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
        // emitting all 3 audio rungs must let the chooser reach index 2 (#1082).
        let mut avail = LayerAvailability::new();
        let now = 1_000u64;
        for raw in 0u32..=2 {
            avail.observe(raw, now);
        }
        assert_eq!(
            avail.highest_available(now),
            2,
            "all three audio rungs must be learnable"
        );
    }

    // --- #1256 Phase 1: size_cap_layer boundary table (T1) ---
    //
    // Camera ladder (lowest-first, issue #1768): L0 = 320x180, L1 = 640x360,
    // L2 = 1280x720 (confirmed in videocall-aq/src/constants.rs). SIZE_CAP_MARGIN
    // = 0.10, so the L0 boundary is 180 * 1.1 = 198px, the L1 boundary is
    // 360 * 1.1 = 396px, the L2 boundary is 720 * 1.1 = 792px. Expected indices
    // are asserted as LITERALS (not recomputed via received_layer_snapshot) so
    // the assertion is an independent source of truth.
    #[test]
    fn size_cap_layer_boundary_table() {
        // Tile == L0 native height -> L0 covers it comfortably (198 >= 180).
        assert_eq!(size_cap_layer(180, 2, 3, PrefMediaKind::Video), 0);

        // Tile EXACTLY at the L0 margin boundary (180 * 1.1 = 198): L0 still covers
        // it BECAUSE of the margin. This is the tightest margin guard — dropping the
        // `* (1.0 + SIZE_CAP_MARGIN)` factor makes the bare comparison `180 >= 198`
        // false, so L0 falls through to L1 (=> 1) and this assertion fails first.
        // (NOTE: the `>=` vs `>` distinction is NOT observable for integer tile
        // heights — `180.0 * 1.1` evaluates to 198.00000000000003 in f64, strictly
        // greater than 198.0, so both comparisons return L0 here.)
        assert_eq!(size_cap_layer(198, 2, 3, PrefMediaKind::Video), 0);

        // 180 * 1.1 = 198 >= 189 -> the 10% margin absorbs the overshoot, L0 still
        // covers. Guards the margin: dropping `* (1.0 + SIZE_CAP_MARGIN)` (i.e.
        // comparing bare native_h=180 >= 189) makes L0 fail -> L1 (=> 1).
        assert_eq!(size_cap_layer(189, 2, 3, PrefMediaKind::Video), 0);

        // 198 < 216 -> L0 fails even WITH the margin; L1 (360*1.1=396 >= 216) covers.
        assert_eq!(size_cap_layer(216, 2, 3, PrefMediaKind::Video), 1);

        // Exact L1 native height -> L1 covers.
        assert_eq!(size_cap_layer(360, 2, 3, PrefMediaKind::Video), 1);

        // Exact L2 native height -> only L2 (720*1.1=792 >= 720) covers.
        assert_eq!(size_cap_layer(720, 2, 3, PrefMediaKind::Video), 2);

        // Taller than the top layer -> falls through to highest_available. Guards
        // the fall-through return: a `return 0` (always-cap-to-base) mutation breaks
        // this (would yield 0 instead of 2).
        assert_eq!(size_cap_layer(9999, 2, 3, PrefMediaKind::Video), 2);

        // highest_available == 0: only L0 is allowed, so even a huge tile is pinned
        // to 0 (the loop range `0..=0` only ever yields 0). Guards the clamp to the
        // available top.
        assert_eq!(size_cap_layer(180, 0, 3, PrefMediaKind::Video), 0);

        // Wants L2 by size but highest_available == 1 -> clamped to 1 (the loop
        // never reaches index 2). Guards the [0, highest_available] clamp.
        assert_eq!(size_cap_layer(720, 1, 3, PrefMediaKind::Video), 1);

        // AUDIO passthrough: never size-capped, returns highest_available even for a
        // tiny tile. Guards the audio early-return: removing it would cap audio to 0.
        assert_eq!(size_cap_layer(50, 2, 3, PrefMediaKind::Audio), 2);

        // tile_h_px == 0 (unknown size) -> don't cap, return highest_available.
        // Guards the `tile_h_px == 0` early-return.
        assert_eq!(size_cap_layer(0, 2, 3, PrefMediaKind::Video), 2);
    }

    // The result is ALWAYS within [0, highest_available] for any tile height —
    // size_cap_layer must never under- or over-shoot the available range.
    #[test]
    fn size_cap_layer_always_within_available_range() {
        let highest = 2u32;
        // Sample around the issue #1768 rung boundaries (180 / 360 / 720).
        for h in [
            0u32,
            1,
            100,
            179,
            180,
            181,
            359,
            360,
            719,
            720,
            5000,
            u32::MAX,
        ] {
            let chosen = size_cap_layer(h, highest, 3, PrefMediaKind::Video);
            assert!(
                chosen <= highest,
                "tile {h}px chose layer {chosen} > highest_available {highest}"
            );
        }
        // And for a 1-layer-available peer, every tile maps to layer 0.
        for h in [0u32, 1, 360, 720, u32::MAX] {
            assert_eq!(
                size_cap_layer(h, 0, 3, PrefMediaKind::Video),
                0,
                "tile {h}px with only L0 available must clamp to 0"
            );
        }
    }

    // --- Issue #2328: keyframe-starved SCREEN rung retraction ------------------

    /// A rung that keeps DELIVERING PACKETS but no decodable keyframe past the bound must be
    /// retracted, and the chooser must downshift off it.
    ///
    /// This is the exact #2328 shape: the relay always forwards layer 0 (`chat_server.rs` drops
    /// only `simulcast_layer_id != 0`), so BOTH rung 0 and the selected rung 2 keep arriving and
    /// keep refreshing `last_seen_ms`. Arrival-based availability therefore reports 2 as healthy
    /// forever while the receiver on it is frozen. Every step below calls the production path —
    /// `screen_starvation_action` for the decision, `LayerAvailability::quarantine` for the
    /// retraction, `highest_available` for the ceiling, `LayerChooser::choose` for the downshift.
    ///
    /// MUTATION CHECKS:
    /// * make `highest_available` ignore `quarantined_until_ms` (i.e. revert the retraction to a
    ///   no-op) → `highest_available` stays 2 and BOTH the ceiling and the chooser assertions fail;
    /// * make `screen_starvation_action` return `None` at the boundary → the `Retract` assertion
    ///   fails (the exact `>=`/`>` boundary is pinned by the bounds test below);
    /// * the pre-quarantine `highest_available == 2` assertion pins that the rung really is still
    ///   arriving, so the test cannot pass for the wrong reason (a rung that simply went silent).
    #[test]
    fn keyframe_starved_screen_rung_is_retracted_and_the_chooser_downshifts() {
        let start = 100_000u64;
        let mut avail = LayerAvailability::new();
        // Rungs 0 and 2 are BOTH on the wire (relay fail-open on base + the receiver's selection).
        avail.observe(0, start);
        avail.observe(2, start);
        assert_eq!(
            avail.highest_available(start),
            2,
            "precondition: packets are arriving on rung 2, so arrival-based availability offers it"
        );

        // The receiver picked rung 2 and has been in an unbroken keyframe-less hold since `start`.
        // Both rungs keep arriving throughout the hold — the relay forwards layer 0 unconditionally
        // and the receiver's selection keeps rung 2 coming, so packet arrival alone cannot tell the
        // two apart. That is precisely the blind spot #2328 closes.
        let selected = 2u32;
        let now = start + SCREEN_KEYFRAME_STARVED_RETRACT_MS;
        avail.observe(0, now);
        avail.observe(2, now);
        assert_eq!(
            screen_starvation_action(
                selected,
                Some(start),
                // The base rung IS still arriving — this is the asymmetry that makes the
                // retraction actionable, and it is read from the same `LayerAvailability` the
                // production caller reads.
                avail.layer_available_peek(0, now),
                now,
                SCREEN_KEYFRAME_STARVED_RETRACT_MS,
                SCREEN_KEYFRAME_STARVED_STALE_MS,
            ),
            ScreenStarvationAction::Retract,
            "an unbroken hold at the bound, while the base rung is still being delivered, must \
             retract the rung"
        );

        avail.quarantine(selected, now + SCREEN_KEYFRAME_STARVED_QUARANTINE_MS);
        // Rung 2's packets keep arriving throughout — the quarantine, not silence, removes it.
        avail.observe(0, now);
        avail.observe(2, now);
        assert_eq!(
            avail.highest_available(now),
            0,
            "a quarantined rung is excluded from the ceiling even while its packets keep arriving"
        );
        assert!(
            !avail.layer_available_peek(2, now),
            "the read-only peek must agree with the pruning read"
        );

        // The chooser, fed the retracted ceiling, drops to the base rung — which the relay
        // forwards unconditionally and which the publisher's floor always re-keys.
        let mut c = LayerChooser::new_screen(start);
        c.choose(clean(), 2, start);
        assert_eq!(
            c.current(),
            2,
            "precondition: chooser was sitting on rung 2"
        );
        let chosen = c.choose(clean(), avail.highest_available(now), now);
        assert_eq!(
            chosen, 0,
            "with rung 2 retracted the chooser must downshift to a rung that delivers keyframes"
        );
    }

    /// The retraction CANNOT WEDGE: the quarantine expires on wall-clock alone and the rung is
    /// re-admitted, after which the chooser climbs back.
    ///
    /// This is the property the repo's recovery-hysteresis rule demands, and here it is stronger
    /// than "prefer time-bounded": a success-counter exit would be UNSATISFIABLE, because once the
    /// receiver leaves the rung the rung may stop arriving at all and could never demonstrate a
    /// keyframe. Re-admission is therefore unconditional at expiry.
    ///
    /// MUTATION CHECKS: dropping the `retain` prune in `highest_available`, or making
    /// `is_quarantined` ignore `until`, leaves the ceiling at 0 and the re-admission assertion
    /// fails; flipping `now_ms < until` to `<=` fails the exclusive-expiry assertion.
    #[test]
    fn a_quarantined_screen_rung_is_readmitted_when_the_window_expires() {
        let start = 100_000u64;
        let mut avail = LayerAvailability::new();
        let until = start + SCREEN_KEYFRAME_STARVED_QUARANTINE_MS;
        avail.quarantine(2, until);

        // Still quarantined one ms before expiry, even with a fresh observation.
        avail.observe(0, until - 1);
        avail.observe(2, until - 1);
        assert!(avail.is_quarantined(2, until - 1));
        assert_eq!(
            avail.highest_available(until - 1),
            0,
            "the rung is held out for the full window"
        );

        // At `until` the quarantine is over (exclusive upper bound).
        avail.observe(0, until);
        avail.observe(2, until);
        assert!(
            !avail.is_quarantined(2, until),
            "expiry is exclusive: at `until` the rung is eligible again"
        );
        assert_eq!(
            avail.highest_available(until),
            2,
            "an expired quarantine must re-admit the rung — otherwise the receiver is pinned to \
             base forever, which is the wedge this design forbids"
        );

        // And the chooser actually climbs back onto it.
        let mut c = LayerChooser::new_screen(start);
        c.choose(clean(), 0, start);
        assert_eq!(
            c.current(),
            0,
            "precondition: parked on base while retracted"
        );
        assert_eq!(
            c.choose(clean(), avail.highest_available(until), until),
            2,
            "an unconstrained chooser follows the re-admitted ceiling straight back up"
        );

        // Re-quarantining EXTENDS but never shortens, so a still-broken rung serves a full new
        // window instead of flapping on a shorter one.
        avail.quarantine(2, until + 10_000);
        avail.quarantine(2, until + 1);
        assert!(
            avail.is_quarantined(2, until + 5_000),
            "a shorter re-quarantine must not shorten the standing window"
        );
    }

    /// Boundary + guard semantics of the production decision function (issue #2328).
    ///
    /// MUTATION CHECKS, per assertion: `>= retract_after_ms` → `>` fails the exact-boundary case;
    /// dropping the `selected_layer > 0` guard fails the base-rung case (and would let the
    /// receiver retract the one rung the relay guarantees); dropping the staleness bound fails the
    /// suspended-tab case; dropping `saturating_sub` fails the backwards-clock case.
    #[test]
    fn screen_starvation_action_bounds_and_guards() {
        let retract = SCREEN_KEYFRAME_STARVED_RETRACT_MS;
        let stale = SCREEN_KEYFRAME_STARVED_STALE_MS;
        let t0 = 1_000_000u64;

        assert_eq!(
            screen_starvation_action(2, None, true, t0, retract, stale),
            ScreenStarvationAction::None,
            "no recorded starvation → nothing to do"
        );
        assert_eq!(
            screen_starvation_action(2, Some(t0), true, t0 + retract - 1, retract, stale),
            ScreenStarvationAction::None,
            "one ms short of the bound must not retract"
        );
        assert_eq!(
            screen_starvation_action(2, Some(t0), true, t0 + retract, retract, stale),
            ScreenStarvationAction::Retract,
            "the bound is inclusive (`>=`)"
        );
        assert_eq!(
            screen_starvation_action(0, Some(t0), true, t0 + retract, retract, stale),
            ScreenStarvationAction::None,
            "the BASE rung is never retracted — the relay always forwards it and there is nothing \
             below it to fall back to"
        );
        assert_eq!(
            screen_starvation_action(2, Some(t0), true, t0 + stale, retract, stale),
            ScreenStarvationAction::Retract,
            "the staleness bound is exclusive: exactly at it the observation is still actionable"
        );
        assert_eq!(
            screen_starvation_action(2, Some(t0), true, t0 + stale + 1, retract, stale),
            ScreenStarvationAction::Rearm,
            "past the staleness bound (suspended tab / clock step) the observation is re-armed, \
             not acted on"
        );
        assert_eq!(
            screen_starvation_action(2, Some(t0), true, t0 - 5_000, retract, stale),
            ScreenStarvationAction::None,
            "a backwards clock step saturates to a zero-age hold rather than tripping either bound"
        );
    }

    /// Issue #2328 + #1977: a SYMMETRIC screen stall must NOT retract a rung.
    ///
    /// `actix-api/src/actors/priority_drop.rs` sheds SCREEN media at
    /// `PRIORITY_DROP_SCREEN_FILL_RATIO` (0.90) channel fill, and that shed is LAYER-AGNOSTIC — it
    /// never inspects `simulcast_layer_id`, so base-rung keyframes go down with everything else.
    /// That is a different (and deliberate) pressure path, not the per-layer asymmetry #2328 fixes.
    /// Retracting during such an episode buys nothing — every rung is equally starved — and only
    /// adds LAYER_SWITCH churn to an already-saturated link.
    ///
    /// The two calls below are IDENTICAL except for `base_rung_arriving`, so the assertion isolates
    /// exactly that input.
    ///
    /// MUTATION CHECK: dropping `base_rung_arriving` from the `Retract` condition makes the second
    /// call return `Retract` and fails this test.
    #[test]
    fn a_symmetric_screen_stall_does_not_retract_a_rung() {
        let retract = SCREEN_KEYFRAME_STARVED_RETRACT_MS;
        let stale = SCREEN_KEYFRAME_STARVED_STALE_MS;
        let t0 = 1_000_000u64;
        let past_bound = t0 + retract;

        assert_eq!(
            screen_starvation_action(2, Some(t0), true, past_bound, retract, stale),
            ScreenStarvationAction::Retract,
            "control: with the base rung arriving, the same hold DOES retract — so the only \
             difference driving the next assertion is the asymmetry input"
        );
        assert_eq!(
            screen_starvation_action(2, Some(t0), false, past_bound, retract, stale),
            ScreenStarvationAction::None,
            "with the base rung ALSO not arriving the stall is room-wide (the layer-agnostic 90% \
             fill shed), there is no healthier rung to fall back to, and a retraction would be \
             pure churn"
        );

        // The staleness guard still wins over the asymmetry guard: an un-actionable observation is
        // re-armed rather than left to accumulate, whatever the base rung is doing.
        assert_eq!(
            screen_starvation_action(2, Some(t0), false, t0 + stale + 1, retract, stale),
            ScreenStarvationAction::Rearm,
            "a stale stamp re-arms even when the base rung is down, so it cannot latch"
        );
    }

    /// Issue #2328: `layer_available_peek(0, ..)` — the production source of `base_rung_arriving` —
    /// tracks real base-rung arrival and is NOT perturbed by quarantining a higher rung.
    ///
    /// This is the seam between the two halves of the receiver fix: if retracting rung 2 also made
    /// layer 0 read as unavailable, the very next evaluation would see a "symmetric stall" and the
    /// asymmetry guard would suppress every subsequent retraction.
    ///
    /// MUTATION CHECK: making `quarantine` apply to all layers, or `layer_available_peek` ignore
    /// the observation window, fails one of these.
    #[test]
    fn base_rung_arrival_is_independent_of_a_higher_rung_quarantine() {
        let t = 500_000u64;
        let mut avail = LayerAvailability::new();
        avail.observe(0, t);
        avail.observe(2, t);
        avail.quarantine(2, t + SCREEN_KEYFRAME_STARVED_QUARANTINE_MS);

        assert!(
            avail.layer_available_peek(0, t),
            "quarantining rung 2 must not make the base rung read as gone"
        );
        assert!(
            !avail.layer_available_peek(2, t),
            "the quarantined rung itself reads as unavailable"
        );

        // Base arrival still decays normally once packets genuinely stop — which is what makes the
        // asymmetry guard able to detect a room-wide stall at all.
        let after_window = t + LayerAvailability::DEFAULT_WINDOW_MS + 1;
        assert!(
            !avail.layer_available_peek(0, after_window),
            "a base rung that has stopped arriving must eventually read as gone"
        );
    }
}

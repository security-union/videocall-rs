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
/// Starting at "medium" (480p/25fps/600kbps). The PID controller steps up
/// toward higher resolutions when bandwidth allows, or down toward lower
/// resolutions when the network is constrained.
pub const DEFAULT_VIDEO_TIER_INDEX: usize = 4; // "medium"

// ---------------------------------------------------------------------------
// Simulcast Layer Catalog (issue #989, Phase 1b)
// ---------------------------------------------------------------------------

/// Indices into [`VIDEO_QUALITY_TIERS`] that define the simulcast layer ladder.
///
/// Simulcast (issue #989) lets a publisher encode the *same* camera feed at
/// several fixed quality layers simultaneously, tagging each encoded chunk with
/// a cleartext layer id. This is the **single source of truth** for which
/// resolution/bitrate each simulcast layer uses — the layers are expressed as
/// indices into the existing `VIDEO_QUALITY_TIERS` rather than duplicating the
/// tier values, so there is one place to tune.
///
/// The full 3-layer ladder, ordered **lowest layer first** (`layer_id` ==
/// position in the slice), is:
///
/// - layer 0 = `low`      (idx 5: 640×360 / ideal 400 kbps)
/// - layer 1 = `standard` (idx 3: 960×540 / ideal 900 kbps)
/// - layer 2 = `hd`       (idx 2: 1280×720 / ideal 1500 kbps)
///
/// Ordering lowest-first matches the receiver guard (PR A) that defaults to
/// decoding the lowest layer (`layer_id == 0`): the base layer is the cheapest
/// to decode and the most resilient under congestion, and dropping the **top**
/// active layer under congestion sheds the highest-cost stream first.
const SIMULCAST_LAYER_TIER_INDICES: &[usize] = &[5, 3, 2];

/// Maximum number of simulcast layers in the full ladder.
///
/// Equal to `SIMULCAST_LAYER_TIER_INDICES.len()`. Mirrors
/// `SIMULCAST_MAX_SUPPORTED_LAYERS` in `videocall-client`'s `camera_encoder.rs`
/// (kept in sync deliberately — the client clamps requested layers, the AQ
/// crate owns the tier mapping).
pub const SIMULCAST_MAX_LAYERS: usize = 3;

/// Pick `n` well-spaced indices from a `len`-element ladder, **lowest first**.
///
/// Generic, ladder-length-driven replacement for the old hand-authored
/// `match n { 1|2|3 }` selection (issue #1082). The selection rule is:
///
/// Always include the **base** (position 0) and, when `n >= 2`, the **top**
/// (position `len - 1`), then space the remaining picks evenly across the
/// interior. This reproduces the existing contract exactly for the current
/// 3-rung ladder:
///
/// - `n == 1` → `[0]`       → `[low]`
/// - `n == 2` → `[0, 2]`    → `[low, hd]`   (deliberate middle-skip kept)
/// - `n == 3` → `[0, 1, 2]` → `[low, standard, hd]`
///
/// It generalizes cleanly to a deeper ladder so raising
/// [`SIMULCAST_MAX_LAYERS`] later "just works" with no code change here.
///
/// `n` is clamped into `1..=len` so it can never index out of range (matches
/// the client's `clamp_layer_count`; an out-of-range request is degraded, not a
/// crash). Returns positions into the ladder slice, lowest layer first.
fn spaced_ladder_positions(n: usize, len: usize) -> Vec<usize> {
    // `max(1)` is the real guard (a 0-length ladder is a caller bug, but we
    // degrade rather than panic): it prevents both the `n == 1` divide-by-zero
    // below and out-of-range indexing in release builds.
    let len = len.max(1);
    let n = n.clamp(1, len);
    if n == 1 {
        return vec![0];
    }
    if n == len {
        return (0..len).collect();
    }
    // n in 2..len: anchor the base (0) and the top (len-1), distribute the
    // interior picks evenly. Round to the nearest interior position so the
    // spread is symmetric; dedup defensively (cannot collide for n <= len).
    let mut positions: Vec<usize> = (0..n)
        .map(|i| {
            // Map i in [0, n-1] linearly onto [0, len-1].
            let pos = (i as f64) * ((len - 1) as f64) / ((n - 1) as f64);
            pos.round() as usize
        })
        .collect();
    positions.dedup();
    positions
}

/// Resolve the simulcast layer tiers for an `n`-layer ladder.
///
/// Returns a slice of [`VideoQualityTier`] references, **lowest layer first**
/// (index in the returned slice == `layer_id`). The mapping is derived from
/// [`SIMULCAST_LAYER_TIER_INDICES`] via [`spaced_ladder_positions`], so it is
/// driven entirely by the ladder length — for the current 3-rung ladder:
///
/// - `n == 1` → `[low]` (single base layer — used when simulcast is off or the
///   device is too weak; the AQ controller treats this exactly like today's
///   single-stream path, so this tier is *not* used to override the adaptive
///   single-stream resolution — see `camera_encoder.rs`).
/// - `n == 2` → `[low, hd]` (skip the middle `standard` tier so the two layers
///   are well separated in resolution/bitrate).
/// - `n == 3` → `[low, standard, hd]` (full ladder).
///
/// `n` is clamped into `1..=SIMULCAST_MAX_LAYERS`; it never panics (a `0` or
/// out-of-range request degrades to the nearest valid ladder rather than
/// crashing a live call — issue #1082). Callers should still clamp upstream
/// (the client's `clamp_layer_count`).
pub fn simulcast_layers(n: usize) -> &'static [VideoQualityTier] {
    // Static, build-once tables so the function can return `&'static`. We build
    // one cached `Vec<VideoQualityTier>` per ladder size lazily via `OnceLock`.
    use std::sync::OnceLock;

    fn ladder(n: usize) -> Vec<VideoQualityTier> {
        // Derive the lowest-`n` well-spaced rungs generically from the ladder
        // definition (issue #1082): no per-`n` `match` arm, so raising
        // SIMULCAST_MAX_LAYERS requires no change here.
        spaced_ladder_positions(n, SIMULCAST_LAYER_TIER_INDICES.len())
            .into_iter()
            .map(|pos| {
                let t = &VIDEO_QUALITY_TIERS[SIMULCAST_LAYER_TIER_INDICES[pos]];
                // VideoQualityTier is Copy-able plain data; clone field-by-field
                // so the returned vec owns 'static-compatible values.
                VideoQualityTier {
                    label: t.label,
                    max_width: t.max_width,
                    max_height: t.max_height,
                    target_fps: t.target_fps,
                    ideal_bitrate_kbps: t.ideal_bitrate_kbps,
                    min_bitrate_kbps: t.min_bitrate_kbps,
                    max_bitrate_kbps: t.max_bitrate_kbps,
                    keyframe_interval_frames: t.keyframe_interval_frames,
                }
            })
            .collect()
    }

    // One OnceLock cache cell per supported ladder size. Indexed by clamped n.
    static LADDERS: [OnceLock<Vec<VideoQualityTier>>; SIMULCAST_MAX_LAYERS] =
        [const { OnceLock::new() }; SIMULCAST_MAX_LAYERS];

    let clamped = n.clamp(1, SIMULCAST_MAX_LAYERS);
    LADDERS[clamped - 1]
        .get_or_init(|| ladder(clamped))
        .as_slice()
}

/// Sender uplink budget (kbps) for the currently-active simulcast layers
/// (issue #989, Phase 1).
///
/// Publishing N simultaneous layers costs the *sum* of their bitrates on the
/// sender's uplink, not the cost of one layer. The sender's AQ must therefore
/// account for that sum, not just the per-layer tier band. We define the budget
/// as the **sum of the active layers' tier ideals** — i.e. the bitrate the
/// ladder was authored to fit comfortably when all `active` layers run at their
/// nominal quality. Examples for the standard ladder (`[low, standard, hd]`,
/// ideals 400 / 900 / 1500):
///
/// - 1 active layer  → 400 kbps
/// - 2 active layers → 1300 kbps
/// - 3 active layers → 2800 kbps
///
/// `active` is `active_layer_count` (the top shed layers cost nothing), so as
/// the sender's AQ sheds the top layer under congestion the budget shrinks with
/// it. `tiers` is the full ladder, lowest layer first; only the first `active`
/// entries are summed. Pure function so the budget rule is unit-testable.
///
/// # Panics
/// Never panics; `active` is clamped to `[0, tiers.len()]`.
pub fn uplink_budget_kbps(tiers: &[VideoQualityTier], active: usize) -> f64 {
    let active = active.min(tiers.len());
    tiers[..active]
        .iter()
        .map(|t| t.ideal_bitrate_kbps as f64)
        .sum()
}

/// Cap a set of per-layer target bitrates to the sender's uplink budget,
/// preserving each layer's tier floor (issue #989, Phase 1).
///
/// Takes the per-layer targets the per-layer PIDs produced (`targets[i]`, kbps,
/// lowest layer first) for the **active** layers and, if their sum exceeds
/// [`uplink_budget_kbps`], scales them down so the total fits — but never pushes
/// any layer below its tier `min_bitrate_kbps` (the per-layer floor). The base
/// layer (index 0) is the most resilient and the one every receiver decodes, so
/// floors guarantee it stays viewable even when the budget is tight.
///
/// Algorithm (proportional headroom scaling above the floors):
///   1. Compute `floor = Σ min_bitrate_kbps` over the active layers and the
///      requested `sum = Σ targets`. If `sum <= budget`, return unchanged.
///   2. If even the floors exceed the budget (`floor >= budget`), the budget is
///      unsatisfiable without dropping a layer (that is the AQ layer-shed's job,
///      not this function's); return every active layer pinned to its floor —
///      the minimum-cost configuration for the current active set.
///   3. Otherwise distribute the affordable `budget - floor` headroom across the
///      layers in proportion to each layer's own headroom request
///      (`target - min`), so layers that asked for more give up more, and no
///      layer drops below its floor.
///
/// Only the first `active` entries of `targets` are considered; the rest (shed
/// layers) are returned unchanged (they are not encoded/sent). Operates in
/// place. Pure (no I/O / clock), so it is host-unit-testable.
pub fn cap_layers_to_budget(
    targets: &mut [f64],
    tiers: &[VideoQualityTier],
    active: usize,
    budget_kbps: f64,
) {
    let active = active.min(tiers.len()).min(targets.len());
    if active == 0 {
        return;
    }

    let sum: f64 = targets[..active].iter().sum();
    if sum <= budget_kbps {
        return; // Already within budget — no scaling needed.
    }

    let floor: f64 = tiers[..active]
        .iter()
        .map(|t| t.min_bitrate_kbps as f64)
        .sum();

    if floor >= budget_kbps {
        // Budget cannot fit even the floors; pin every active layer to its
        // floor. Shedding a layer to actually fit the budget is the AQ
        // top-layer-drop's responsibility, not this cap's.
        for (i, t) in targets[..active].iter_mut().enumerate() {
            *t = tiers[i].min_bitrate_kbps as f64;
        }
        return;
    }

    // Affordable headroom above the floors, and the total headroom requested.
    let affordable = budget_kbps - floor;
    let requested: f64 = tiers[..active]
        .iter()
        .zip(targets[..active].iter())
        .map(|(tier, &want)| (want - tier.min_bitrate_kbps as f64).max(0.0))
        .sum();

    if requested <= 0.0 {
        // Every layer already at/below its floor (degenerate); pin to floors.
        for (i, t) in targets[..active].iter_mut().enumerate() {
            *t = tiers[i].min_bitrate_kbps as f64;
        }
        return;
    }

    let scale = affordable / requested;
    for (i, t) in targets[..active].iter_mut().enumerate() {
        let min = tiers[i].min_bitrate_kbps as f64;
        let want_headroom = (*t - min).max(0.0);
        *t = min + want_headroom * scale;
    }
}

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
/// Starting at "medium" (720p/8fps/1200kbps) — the midpoint of the 3-tier
/// screen-share ladder (indices 0–2) — gives new screen shares an acceptable
/// baseline immediately. The PID controller then adapts in either direction:
/// stepping up to 1080p when bandwidth is plentiful, or stepping down to
/// 720p/5fps when the network is constrained.
pub const DEFAULT_SCREEN_TIER_INDEX: usize = 1; // "medium"

// ---------------------------------------------------------------------------
// Screen Share Quality Tiers
// ---------------------------------------------------------------------------

/// Screen share quality tiers, ordered from highest (index 0) to lowest.
///
/// Screen content (text, code, diagrams) needs significantly higher bitrates
/// than camera video to remain readable during scrolling and motion. The
/// encoder is configured with `contentHint = 'detail'` and variable bitrate
/// mode to accommodate burst demand during scroll events.
pub const SCREEN_QUALITY_TIERS: &[VideoQualityTier] = &[
    VideoQualityTier {
        label: "high",
        max_width: 1920,
        max_height: 1080,
        target_fps: 10,
        ideal_bitrate_kbps: 2500,
        min_bitrate_kbps: 1500,
        max_bitrate_kbps: 4000,
        keyframe_interval_frames: 30, // ~3s at 10fps — frequent keyframes for text readability
    },
    VideoQualityTier {
        label: "medium",
        max_width: 1280,
        max_height: 720,
        target_fps: 8,
        ideal_bitrate_kbps: 1200,
        min_bitrate_kbps: 700,
        max_bitrate_kbps: 2000,
        keyframe_interval_frames: 24, // ~3s at 8fps
    },
    VideoQualityTier {
        label: "low",
        max_width: 1280,
        max_height: 720,
        target_fps: 5,
        ideal_bitrate_kbps: 500,
        min_bitrate_kbps: 250,
        max_bitrate_kbps: 1000,
        keyframe_interval_frames: 15, // ~3s at 5fps
    },
];

/// Maximum number of SCREEN simulcast layers (issue #989, Phase 3).
pub const SCREEN_SIMULCAST_MAX_LAYERS: usize = 3;

/// Resolve the SCREEN simulcast layer tiers for an `n`-layer ladder
/// (issue #989, Phase 3), **lowest layer first** (index == `layer_id`).
///
/// Derived from [`SCREEN_QUALITY_TIERS`] (which is ordered high→low):
/// - `n == 1` → `[low]` (single base; screen single-stream path is unchanged
///   and does not consult this).
/// - `n == 2` → `[low, high]` (720p/500 base + 1080p/2500 top — well separated).
/// - `n == 3` → `[low, medium, high]` (full ladder; low and medium share 720p
///   but differ in fps/bitrate, so a receiver can still pull the cheaper rung).
///
/// # Panics
/// Panics if `n` is not in `{1, 2, 3}`; callers must clamp first.
pub fn simulcast_screen_layers(n: usize) -> &'static [VideoQualityTier] {
    use std::sync::OnceLock;

    // SCREEN_QUALITY_TIERS indices: 0=high, 1=medium, 2=low. Authored
    // lowest-first per layer ladder below.
    fn ladder(n: usize) -> Vec<VideoQualityTier> {
        let indices: &[usize] = match n {
            1 => &[2],       // [low]
            2 => &[2, 0],    // [low, high]
            3 => &[2, 1, 0], // [low, medium, high]
            other => panic!("simulcast_screen_layers: n must be in {{1,2,3}}, got {other}"),
        };
        indices
            .iter()
            .map(|&i| {
                let t = &SCREEN_QUALITY_TIERS[i];
                VideoQualityTier {
                    label: t.label,
                    max_width: t.max_width,
                    max_height: t.max_height,
                    target_fps: t.target_fps,
                    ideal_bitrate_kbps: t.ideal_bitrate_kbps,
                    min_bitrate_kbps: t.min_bitrate_kbps,
                    max_bitrate_kbps: t.max_bitrate_kbps,
                    keyframe_interval_frames: t.keyframe_interval_frames,
                }
            })
            .collect()
    }

    static SCREEN_LADDER_1: OnceLock<Vec<VideoQualityTier>> = OnceLock::new();
    static SCREEN_LADDER_2: OnceLock<Vec<VideoQualityTier>> = OnceLock::new();
    static SCREEN_LADDER_3: OnceLock<Vec<VideoQualityTier>> = OnceLock::new();

    match n {
        1 => SCREEN_LADDER_1.get_or_init(|| ladder(1)).as_slice(),
        2 => SCREEN_LADDER_2.get_or_init(|| ladder(2)).as_slice(),
        3 => SCREEN_LADDER_3.get_or_init(|| ladder(3)).as_slice(),
        other => panic!("simulcast_screen_layers: n must be in {{1,2,3}}, got {other}"),
    }
}

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
///
/// Lowered from 0.85 to 0.70 for recovery parity with audio (0.60).
/// At 0.85, video stayed stuck at minimal while audio recovered to high —
/// the 0.35 gap between degrade (0.50) and recover (0.85) was too wide.
/// At 0.70 the hysteresis gap is 0.20 (degrade 0.50, recover 0.70),
/// which still prevents oscillation while allowing video to recover
/// within a similar window as audio.
pub const VIDEO_TIER_RECOVER_FPS_RATIO: f64 = 0.70;

/// Fraction of `target_fps` at or above which a single peer is considered
/// "healthy" for the small-peer-count outlier guard in
/// `DiagnosticPackets::get_p75_fps` (issue #1012).
///
/// With 2 reporting peers the p75 aggregation degenerates to the minimum, so a
/// single constrained receiver (e.g. a peer on a 5.8 Mbps link with a
/// 640×480@15fps camera, per discussion #980) would otherwise define the PID
/// setpoint and drag the sender's bitrate down for everyone. The guard only
/// rescues the setpoint toward the *higher* reporter when at least one peer is
/// genuinely healthy — i.e. at/above this fraction of target. If NO peer clears
/// this bar, all peers are struggling: that is real congestion, and the
/// conservative minimum is kept so the sender still steps down.
///
/// Defaults to the recover ratio (0.70) so "healthy enough to not be an
/// outlier" is tied to "healthy enough for the tier to recover". First-guess
/// value — pending a performance-reviewer pass. DO NOT treat as final.
pub const AQ_OUTLIER_HEALTH_FPS_RATIO: f64 = VIDEO_TIER_RECOVER_FPS_RATIO;

/// Maximum ratio of the lower peer's FPS to the higher peer's FPS for the lower
/// one to count as a clear outlier in the small-peer-count guard (issue #1012).
///
/// At 2 peers `[a ≤ b]`, the guard treats `a` as an outlier only when
/// `a < b * AQ_OUTLIER_GAP_FPS_RATIO` — i.e. `a` is more than ~40% below `b`.
/// This prevents rescuing on ordinary jitter (two healthy peers a few fps
/// apart) and fires only on the genuine "one fine, one badly degraded" split.
///
/// First-guess value — pending a performance-reviewer pass. DO NOT treat as
/// final.
pub const AQ_OUTLIER_GAP_FPS_RATIO: f64 = 0.60;

/// Bitrate ratio (actual/ideal) below which we step DOWN one video tier.
pub const VIDEO_TIER_DEGRADE_BITRATE_RATIO: f64 = 0.40;
/// Bitrate ratio above which we step UP one video tier (must be sustained).
pub const VIDEO_TIER_RECOVER_BITRATE_RATIO: f64 = 0.75;

/// Audio degrades only when video is already at lowest tier AND these thresholds hit.
pub const AUDIO_TIER_DEGRADE_FPS_RATIO: f64 = 0.30;
pub const AUDIO_TIER_RECOVER_FPS_RATIO: f64 = 0.60;

/// How long conditions must remain "good" before stepping UP (milliseconds).
/// Prevents rapid oscillation on unstable connections.
/// Note: during active recovery slowdown (after a yo-yo crash), this window
/// is multiplied by `RECOVERY_SLOWDOWN_FACTOR` — see the climb-rate limiter
/// constants below.
pub const STEP_UP_STABILIZATION_WINDOW_MS: u64 = 5000;

/// How quickly we step DOWN (milliseconds). Degradation is faster than recovery.
pub const STEP_DOWN_REACTION_TIME_MS: u64 = 1500;

/// Minimum time between any two tier transitions (milliseconds).
/// Prevents rapid toggling even if thresholds are crossed quickly.
pub const MIN_TIER_TRANSITION_INTERVAL_MS: u64 = 1500;

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
// Climb-Rate Limiter (PR-H)
// ---------------------------------------------------------------------------
// Prevents the adaptive quality system from yo-yoing between max and min
// quality by imposing two complementary mechanisms:
//
// 1. **Crash ceiling** (Option A): after a detected yo-yo (two step-downs
//    within `YOYO_DETECTION_WINDOW_MS`), a temporary ceiling prevents
//    recovering past the failure tier. The ceiling lifts one tier at a time
//    after each decay period, with exponential backoff on repeated crashes.
//
// 2. **Recovery slowdown** (Option B): after any ceiling-arming event, the
//    step-up stabilization window is multiplied by `RECOVERY_SLOWDOWN_FACTOR`,
//    giving each tier genuine soak time before climbing higher. The slowdown
//    decays linearly back to 1.0 over `RECOVERY_SLOWDOWN_DECAY_MS`.

/// Base decay period (ms) before the crash ceiling lifts by one tier.
/// After the ceiling is armed, this is how long the system waits before
/// allowing recovery to attempt the next-higher tier.
pub const CLIMB_COOLDOWN_BASE_MS: f64 = 120_000.0; // 2 min

/// Maximum ceiling decay period (ms) after repeated crashes.
/// The decay period doubles on each re-crash via `CLIMB_COOLDOWN_BACKOFF`
/// but caps here to prevent indefinite quality lockout.
pub const CLIMB_COOLDOWN_MAX_MS: f64 = 600_000.0; // 10 min

/// Backoff multiplier applied to the ceiling decay period on each re-crash.
/// Sequence: 2 min → 4 min → 8 min → 10 min (capped).
pub const CLIMB_COOLDOWN_BACKOFF: f64 = 2.0;

/// Multiplier applied to `STEP_UP_STABILIZATION_WINDOW_MS` after a yo-yo
/// crash is detected. Gives each tier longer soak time during recovery,
/// catching degradation at intermediate tiers before climbing higher.
pub const RECOVERY_SLOWDOWN_FACTOR: f64 = 2.0;

/// Time (ms) for the recovery slowdown factor to decay linearly from
/// `RECOVERY_SLOWDOWN_FACTOR` back to 1.0 (normal speed).
/// Aligned with `CLIMB_COOLDOWN_BASE_MS` cadence so the slowdown expires
/// around the time the ceiling lifts, avoiding wasted lift attempts.
pub const RECOVERY_SLOWDOWN_DECAY_MS: f64 = 180_000.0; // 3 min

/// Time (ms) of stable operation (no step-downs) after which crash memory
/// resets: `ceiling_decay_ms` returns to `CLIMB_COOLDOWN_BASE_MS` and the
/// slowdown clears. Represents "this meeting is fine now."
pub const CRASH_MEMORY_RESET_MS: f64 = 600_000.0; // 10 min

/// Window (ms) for yo-yo detection (design decision 1b). A crash ceiling
/// is only armed when a step-down occurs within this window of a prior
/// step-down, indicating an oscillation pattern rather than a one-shot
/// degradation from a legitimate capacity change.
/// Set to 3 minutes — production yo-yo cycles are 30-60s, so this catches
/// the pattern within one cycle without false positives on single events.
pub const YOYO_DETECTION_WINDOW_MS: f64 = 180_000.0; // 3 min

/// Grace period (ms) after a successful server re-election during which
/// step-downs do NOT arm the crash ceiling. Re-elections cause an FPS
/// collapse during the server swap that looks like a crash to AQ; without
/// this suppression the ceiling would cap a genuinely-better path.
pub const REELECTION_CEILING_SUPPRESSION_MS: f64 = 10_000.0; // 10s

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
// Sender Encoder Backpressure (issue #1108, Phase B)
// ---------------------------------------------------------------------------
// The receiver-FPS-driven layer shed reacts to how peers *receive* our stream,
// but it cannot see the *sender's own* encoder falling behind (a CPU-bound
// laptop whose WebCodecs encode queue is backing up). These constants describe
// the sender-side backstop: when the active encoders' `encode_queue_size()`
// stays high for a sustained window, Stage 2 will shed a layer to relieve
// encode CPU. Stage 1 only *samples and stores* the queue depth — these
// thresholds are not yet read by any control path (see
// `EncoderBitrateController::observe_encoder_queue_depth`).

/// Encoder queue depth (frames pending in the WebCodecs `VideoEncoder`) at or
/// above which the sender is considered to be in encode backpressure. Sampled
/// as the max `encode_queue_size()` across active simulcast layers. A healthy
/// realtime encoder drains to ~0–1 each tick, so a sustained depth of 3 means
/// the encoder is consistently a few frames behind capture.
///
/// First-guess value — pending a performance-reviewer pass. DO NOT treat as
/// final.
#[allow(dead_code)]
pub const ENCODER_QUEUE_BACKPRESSURE_HIGH: u32 = 3;

/// Encoder queue depth at or below which sender encode backpressure is
/// considered cleared (hysteresis floor against the HIGH threshold). Once the
/// queue drains back to this depth the sustain timer resets.
///
/// First-guess value — pending a performance-reviewer pass. DO NOT treat as
/// final.
#[allow(dead_code)]
pub const ENCODER_QUEUE_BACKPRESSURE_CLEAR: u32 = 1;

/// How long (milliseconds) the encoder queue depth must stay at/above
/// [`ENCODER_QUEUE_BACKPRESSURE_HIGH`] before Stage 2 acts on it. Sized in the
/// same ballpark as `STEP_DOWN_REACTION_TIME_MS` so a brief encode hiccup (a
/// single slow frame, a GC pause) does not trigger a shed.
///
/// First-guess value — pending a performance-reviewer pass. DO NOT treat as
/// final.
#[allow(dead_code)]
pub const ENCODER_BACKPRESSURE_SUSTAIN_MS: f64 = 1500.0;

// ---------------------------------------------------------------------------
// Bitrate Change Threshold
// ---------------------------------------------------------------------------

/// Only apply a bitrate change if it exceeds this ratio of the current bitrate.
/// Prevents tiny fluctuations from causing unnecessary encoder reconfigurations.
/// Smaller drifts apply gradually rather than accumulating into larger jumps
/// that force encoder keyframes on each reconfigure.
pub const BITRATE_CHANGE_THRESHOLD: f64 = 0.10;

/// Maximum rate at which the AQ controller may change its output bitrate, in
/// kbps per second. Prevents the controller from jumping between very low and
/// very high bitrates within one tick, which would force the encoder to
/// reconfigure (and emit a keyframe) on every cycle. Set conservatively to
/// match VP9 realtime's ability to adapt rate-control state smoothly.
pub const MAX_BITRATE_SLEW_KBPS_PER_SEC: u32 = 500;

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
// Screen Share Initial Tier Selection
// ---------------------------------------------------------------------------

/// Select the starting screen-share quality tier given the available network signals.
///
/// # Signals
/// - `rtt_ms`: Most-recent average server RTT, or `None` if unknown (e.g. first meeting
///   before any RTT probes have completed, or WebSocket-only deployment).
/// - `camera_tier_index`: Current camera AQ tier index (0 = full-HD, higher = degraded).
///   Pass `None` if camera is not started (screen-only share).
///
/// # Returns
/// An index into `SCREEN_QUALITY_TIERS` (0 = high/1080p, 1 = medium/720p, 2 = low).
///
/// # Failure mode — cold start
/// When `rtt_ms` is `None` (first meeting, no prior probes) and the camera has not yet
/// been degraded, this function returns 0 (high/1080p).  The PID loop will ramp down
/// within a few seconds if the uplink cannot sustain 2500 kbps.  The "readable
/// first-frame within 3 s" guarantee only applies when at least one signal is present.
pub fn initial_screen_tier(rtt_ms: Option<f64>, camera_tier_index: Option<usize>) -> usize {
    // Cold start: no signals available, default to optimistic high tier
    if rtt_ms.is_none() && camera_tier_index.is_none() {
        return 0; // high
    }

    // RTT-based thresholds
    let rtt_poor = rtt_ms.map(|rtt| rtt >= RTT_POOR_MS).unwrap_or(false);
    let rtt_fair = rtt_ms.map(|rtt| rtt >= RTT_FAIR_MS).unwrap_or(false);

    // Camera tier degradation indicators
    // Camera tiers: 0=full_hd, 1=hd_plus, 2=hd, 3=standard, 4=medium, 5=low, 6=very_low, 7=minimal
    // Threshold: ≥3 (sd/low) means camera is already degraded
    let camera_degraded = camera_tier_index.map(|idx| idx >= 3).unwrap_or(false);

    // Decision table:
    // RTT >= POOR (400ms)     → low (2)
    // RTT >= FAIR (200ms)     → medium (1)
    // RTT < FAIR, camera ≥ sd → medium (1)  (camera already degraded)
    // RTT < FAIR, camera < sd → high (0)    (good conditions)
    // RTT None, camera ≥ sd   → medium (1)  (camera signal only)
    // RTT None, camera < sd   → high (0)    (camera signal only, optimistic)

    if rtt_poor {
        return 2; // low
    }

    if rtt_fair || camera_degraded {
        return 1; // medium
    }

    0 // high
}

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

/// Number of *consecutive* implausible-RTT discards on the active connection
/// before treating sustained discards as a re-election trigger.
///
/// The plausibility filter (`RTT_SANITY_MAX_MS`) silently drops measurements
/// when `recv - sent` is outside `[0, 10s]`. Without this watchdog the
/// existing RTT-degradation detector is starved of samples, leaving the user
/// stuck on a broken connection (see discussion #539, JRG_dirs incident:
/// 255 implausible discards over 6 minutes due to server-side clock drift).
///
/// 10 is chosen so that, at the 1Hz post-election RTT probe rate, sustained
/// discards trigger re-election after roughly 10 seconds of clock-drift /
/// time-base brokenness — long enough to ride out transient one-shot anomalies
/// (such as a single late ACK or a one-off NTP slew) but short enough to
/// recover before users perceive the connection as dead.
pub const REELECTION_IMPLAUSIBLE_DISCARDS_THRESHOLD: u32 = 10;

/// Freshness window (ms) for the old active connection when deciding whether
/// to preserve it after total candidate failure.
///
/// When a re-election starts and ALL candidates fail before producing valid
/// RTT measurements, we check whether the old active connection has had any
/// inbound traffic (media packet, RTT echo, heartbeat ACK, or session-assigned
/// frame) within this window. If yes, the candidates' failure is taken to be
/// a transient relay-side outage and the old connection is preserved. If no,
/// the old connection is presumed dead and the user is disconnected through
/// the existing path.
///
/// 5 s is chosen because:
/// - it is long enough to span a few server heartbeat intervals (the server
///   sends data at >= 1 Hz when the call is active), so a healthy old
///   connection is virtually guaranteed to register inbound traffic inside it
/// - it is short enough that genuinely silent connections (server crash, NAT
///   rebind, route flap on the live path) do NOT get preserved as ghosts
/// - it matches the connection-lost callback's typical detection lag of
///   1-3 s on degraded networks, leaving headroom for jitter
pub const REELECTION_PRESERVATION_FRESHNESS_MS: f64 = 5_000.0;

/// Delay (ms) before retrying a re-election after the old active connection
/// has been preserved due to total candidate failure.
///
/// 30 s gives the relay time to recover from the kind of brief outage that
/// caused both candidates to fail (the JRG_dirs Tony S1 incident on
/// 2026-05-05 saw both candidates flame out in 14 ms, suggesting a
/// short-lived relay-side event). Retrying too soon risks hitting the same
/// outage; waiting too long delays moving off a degraded baseline.
pub const REELECTION_PRESERVATION_RETRY_MS: u64 = 30_000;

/// Delay (milliseconds) before checking whether a post-rebase re-election
/// retry should fire.
///
/// When RTT has degraded but only one server is configured at the connection
/// manager's level, the rebase path silently adapts the baseline to the new
/// RTT instead of triggering re-election (because the only candidate would
/// be the same already-degraded server). This timer schedules a re-evaluation
/// 30 seconds later: if by then the URL list has expanded (e.g. the UI
/// refilled it via `update_server_urls`) so a meaningful election is
/// possible, the standard election machinery is invoked. The 30-second value
/// is long enough to absorb transient relay-availability blips without
/// cascading into a per-second retry storm on real-world networks.
pub const POST_REBASE_RETRY_DELAY_MS: u64 = 30_000;

/// Maximum number of consecutive post-rebase retry attempts before giving up.
///
/// Each attempt that finds the URL list still single-server schedules another
/// retry at `POST_REBASE_RETRY_DELAY_MS`. Capping at 3 means total wall-clock
/// retry coverage is ~90 seconds before the system stops polling — preventing
/// unbounded background timers if the server-side condition never resolves.
/// The counter is reset whenever a successful election or a manual
/// reconnection lands so a fresh meeting session gets a fresh retry budget.
pub const POST_REBASE_RETRY_MAX_ATTEMPTS: u32 = 3;

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

    // =====================================================================
    // Simulcast layer catalog validation (issue #989)
    // =====================================================================

    #[test]
    fn test_simulcast_layer_indices_in_bounds() {
        for &idx in SIMULCAST_LAYER_TIER_INDICES {
            assert!(
                idx < VIDEO_QUALITY_TIERS.len(),
                "simulcast layer tier index {idx} out of bounds (len={})",
                VIDEO_QUALITY_TIERS.len(),
            );
        }
    }

    #[test]
    fn test_simulcast_max_layers_matches_ladder_len() {
        assert_eq!(
            SIMULCAST_MAX_LAYERS,
            SIMULCAST_LAYER_TIER_INDICES.len(),
            "SIMULCAST_MAX_LAYERS must equal the ladder length"
        );
    }

    #[test]
    fn test_simulcast_layers_returns_expected_labels() {
        // n=1 → [low]
        let l1 = simulcast_layers(1);
        assert_eq!(l1.len(), 1);
        assert_eq!(l1[0].label, "low");

        // n=2 → [low, hd] (skip standard)
        let l2 = simulcast_layers(2);
        assert_eq!(l2.len(), 2);
        assert_eq!(l2[0].label, "low");
        assert_eq!(l2[1].label, "hd");

        // n=3 → [low, standard, hd]
        let l3 = simulcast_layers(3);
        assert_eq!(l3.len(), 3);
        assert_eq!(l3[0].label, "low");
        assert_eq!(l3[1].label, "standard");
        assert_eq!(l3[2].label, "hd");
    }

    #[test]
    fn test_simulcast_layers_resolutions_positive_and_ascending() {
        // Layers are ordered lowest→highest, so resolution must be
        // non-decreasing as layer_id increases (the opposite of the main
        // VIDEO_QUALITY_TIERS ordering, which is highest→lowest).
        for n in [1usize, 2, 3] {
            let layers = simulcast_layers(n);
            for layer in layers {
                assert!(
                    layer.max_width > 0 && layer.max_height > 0,
                    "n={n}: layer '{}' resolution must be positive ({}x{})",
                    layer.label,
                    layer.max_width,
                    layer.max_height,
                );
            }
            for window in layers.windows(2) {
                let lower = &window[0];
                let higher = &window[1];
                let lower_px = lower.max_width as u64 * lower.max_height as u64;
                let higher_px = higher.max_width as u64 * higher.max_height as u64;
                assert!(
                    higher_px >= lower_px,
                    "n={n}: layer '{}' ({}px) must have >= pixels than lower layer '{}' ({}px)",
                    higher.label,
                    higher_px,
                    lower.label,
                    lower_px,
                );
            }
        }
    }

    #[test]
    fn test_simulcast_layers_bitrate_ordering_within_and_across() {
        for n in [1usize, 2, 3] {
            let layers = simulcast_layers(n);
            for layer in layers {
                // Within-tier bitrate sanity (mirrors test_video_tier_bitrate_ordering).
                assert!(
                    layer.min_bitrate_kbps < layer.max_bitrate_kbps,
                    "n={n}: layer '{}' min_bitrate ({}) must be < max_bitrate ({})",
                    layer.label,
                    layer.min_bitrate_kbps,
                    layer.max_bitrate_kbps,
                );
                assert!(
                    layer.ideal_bitrate_kbps >= layer.min_bitrate_kbps
                        && layer.ideal_bitrate_kbps <= layer.max_bitrate_kbps,
                    "n={n}: layer '{}' ideal_bitrate ({}) must be within [{}, {}]",
                    layer.label,
                    layer.ideal_bitrate_kbps,
                    layer.min_bitrate_kbps,
                    layer.max_bitrate_kbps,
                );
            }
            // Across layers: ideal bitrate must be non-decreasing with layer_id.
            for window in layers.windows(2) {
                assert!(
                    window[1].ideal_bitrate_kbps >= window[0].ideal_bitrate_kbps,
                    "n={n}: layer '{}' ideal ({}) must be >= lower layer '{}' ideal ({})",
                    window[1].label,
                    window[1].ideal_bitrate_kbps,
                    window[0].label,
                    window[0].ideal_bitrate_kbps,
                );
            }
        }
    }

    #[test]
    fn test_simulcast_layers_clamps_zero_to_base() {
        // Issue #1082: `0` is meaningless (there is always a base layer) and now
        // clamps up to the single base layer instead of panicking, so a degenerate
        // request can never crash a live call.
        let l0 = simulcast_layers(0);
        assert_eq!(l0.len(), 1);
        assert_eq!(l0[0].label, "low");
        // Identical to the n=1 result.
        assert_eq!(l0[0].label, simulcast_layers(1)[0].label);
    }

    #[test]
    fn test_simulcast_layers_clamps_too_many_to_max() {
        // Issue #1082: an over-large request now clamps down to the full ladder
        // (SIMULCAST_MAX_LAYERS) rather than panicking.
        let over = simulcast_layers(SIMULCAST_MAX_LAYERS + 1);
        assert_eq!(over.len(), SIMULCAST_MAX_LAYERS);
        // Identical to the max-layer ladder.
        let full = simulcast_layers(SIMULCAST_MAX_LAYERS);
        assert_eq!(over.len(), full.len());
        for (a, b) in over.iter().zip(full.iter()) {
            assert_eq!(a.label, b.label);
        }
    }

    #[test]
    fn test_spaced_ladder_positions_matches_current_contract() {
        // Pin the generic selection against the existing 3-rung contract
        // (issue #1082): n=1 → [0]; n=2 → [0, 2] (middle-skip); n=3 → [0, 1, 2].
        assert_eq!(spaced_ladder_positions(1, 3), vec![0]);
        assert_eq!(spaced_ladder_positions(2, 3), vec![0, 2]);
        assert_eq!(spaced_ladder_positions(3, 3), vec![0, 1, 2]);
        // Clamp: 0 → base only; over-large → full ladder.
        assert_eq!(spaced_ladder_positions(0, 3), vec![0]);
        assert_eq!(spaced_ladder_positions(99, 3), vec![0, 1, 2]);
    }

    #[test]
    fn test_spaced_ladder_positions_generalizes_to_deeper_ladder() {
        // Forward-looking (issue #1082): on a future 5-rung ladder the selection
        // must always anchor base+top and space the interior evenly, with no
        // collisions for n <= len. This proves a 3→5 bump needs no code change.
        for len in 3..=5usize {
            for n in 1..=len {
                let pos = spaced_ladder_positions(n, len);
                assert_eq!(pos.len(), n, "len={len} n={n}: must pick exactly n rungs");
                assert_eq!(pos[0], 0, "len={len} n={n}: base must be first");
                if n >= 2 {
                    assert_eq!(
                        *pos.last().unwrap(),
                        len - 1,
                        "len={len} n={n}: top rung must be last"
                    );
                }
                // Strictly ascending (no duplicates, lowest-first).
                for w in pos.windows(2) {
                    assert!(
                        w[1] > w[0],
                        "len={len} n={n}: positions must ascend: {pos:?}"
                    );
                }
            }
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

    // -----------------------------------------------------------------
    // Uplink budget tests (issue #989, Phase 1)
    // -----------------------------------------------------------------

    #[test]
    fn test_uplink_budget_is_sum_of_active_tier_ideals() {
        // Standard ladder [low, standard, hd] = ideals 400 / 900 / 1500.
        let tiers = simulcast_layers(3);
        assert_eq!(uplink_budget_kbps(tiers, 1), 400.0);
        assert_eq!(uplink_budget_kbps(tiers, 2), 1300.0);
        assert_eq!(uplink_budget_kbps(tiers, 3), 2800.0);
        // active is clamped to the ladder length (cannot over-count).
        assert_eq!(uplink_budget_kbps(tiers, 99), 2800.0);
        // Zero active layers → zero budget.
        assert_eq!(uplink_budget_kbps(tiers, 0), 0.0);
    }

    #[test]
    fn test_cap_noop_when_within_budget() {
        // Targets that already fit must be returned unchanged (the common case
        // at low tiers and the byte-identical guarantee for N=1).
        let tiers = simulcast_layers(3);
        let budget = uplink_budget_kbps(tiers, 3); // 2800
        let mut targets = [300.0, 700.0, 1200.0]; // sum 2200 <= 2800
        let before = targets;
        cap_layers_to_budget(&mut targets, tiers, 3, budget);
        assert_eq!(targets, before, "within-budget targets must not change");
    }

    #[test]
    fn test_cap_scales_down_to_budget_and_respects_floors() {
        // Targets that exceed the budget must be scaled so the active sum fits,
        // and no layer may drop below its tier floor (200 / 500 / 800).
        let tiers = simulcast_layers(3);
        let floors: Vec<f64> = tiers.iter().map(|t| t.min_bitrate_kbps as f64).collect();
        let budget = uplink_budget_kbps(tiers, 3); // 2800
                                                   // All layers asking for their tier max: 600 + 1500 + 2000 = 4100 > 2800.
        let mut targets = [600.0, 1500.0, 2000.0];
        cap_layers_to_budget(&mut targets, tiers, 3, budget);

        let sum: f64 = targets.iter().sum();
        assert!(
            sum <= budget + 1e-6,
            "active sum {sum} must fit within budget {budget}"
        );
        for (i, &t) in targets.iter().enumerate() {
            assert!(
                t >= floors[i] - 1e-6,
                "layer {i} ({t}) must stay at/above its floor {}",
                floors[i]
            );
        }
    }

    #[test]
    fn test_cap_pins_to_floors_when_budget_below_floor_sum() {
        // If the budget cannot fit even the floors, pin every active layer to
        // its floor (shedding a layer to actually fit is the AQ's job, not the
        // cap's). Floors sum = 200+500+800 = 1500; pass a budget below that.
        let tiers = simulcast_layers(3);
        let mut targets = [600.0, 1500.0, 2000.0];
        cap_layers_to_budget(&mut targets, tiers, 3, 1000.0);
        assert_eq!(targets[0], tiers[0].min_bitrate_kbps as f64);
        assert_eq!(targets[1], tiers[1].min_bitrate_kbps as f64);
        assert_eq!(targets[2], tiers[2].min_bitrate_kbps as f64);
    }

    #[test]
    fn test_cap_only_touches_active_layers() {
        // Shed (inactive) top layers must be left untouched: with active=1 only
        // index 0 is considered, even if its lone target exceeds the 1-layer
        // budget; indices 1..2 keep their stale values.
        let tiers = simulcast_layers(3);
        let budget = uplink_budget_kbps(tiers, 1); // 400 (= low ideal)
        let mut targets = [600.0, 9999.0, 8888.0];
        cap_layers_to_budget(&mut targets, tiers, 1, budget);
        // Active layer 0 capped to its floor-respecting share of 400.
        assert!(targets[0] <= budget + 1e-6 && targets[0] >= 200.0 - 1e-6);
        // Shed layers untouched.
        assert_eq!(targets[1], 9999.0);
        assert_eq!(targets[2], 8888.0);
    }

    // -----------------------------------------------------------------
    // SCREEN simulcast ladder + budget (issue #989, Phase 3)
    // -----------------------------------------------------------------

    #[test]
    fn test_simulcast_screen_layers_labels_and_ordering() {
        // n=1 → [low]; n=2 → [low, high]; n=3 → [low, medium, high].
        let l1 = simulcast_screen_layers(1);
        assert_eq!(l1.len(), 1);
        assert_eq!(l1[0].label, "low");

        let l2 = simulcast_screen_layers(2);
        assert_eq!(l2.len(), 2);
        assert_eq!(l2[0].label, "low");
        assert_eq!(l2[1].label, "high");

        let l3 = simulcast_screen_layers(3);
        assert_eq!(l3.len(), 3);
        assert_eq!(
            [l3[0].label, l3[1].label, l3[2].label],
            ["low", "medium", "high"]
        );
        // Bitrate ideals must be non-decreasing lowest→highest.
        assert!(l3[0].ideal_bitrate_kbps <= l3[1].ideal_bitrate_kbps);
        assert!(l3[1].ideal_bitrate_kbps <= l3[2].ideal_bitrate_kbps);
    }

    #[test]
    #[should_panic(expected = "n must be in")]
    fn test_simulcast_screen_layers_rejects_zero() {
        let _ = simulcast_screen_layers(0);
    }

    #[test]
    fn test_screen_budget_caps_active_sum() {
        // The budget cap is ladder-agnostic; verify it works over the SCREEN
        // ladder. 3-layer ideals: low 500 + medium 1200 + high 2500 = 4200.
        let tiers = simulcast_screen_layers(3);
        let budget = uplink_budget_kbps(tiers, 3);
        assert_eq!(budget, 4200.0);
        // Push each layer to its max → sum exceeds budget → scaled down.
        let mut targets = [
            tiers[0].max_bitrate_kbps as f64,
            tiers[1].max_bitrate_kbps as f64,
            tiers[2].max_bitrate_kbps as f64,
        ];
        cap_layers_to_budget(&mut targets, tiers, 3, budget);
        let sum: f64 = targets.iter().sum();
        assert!(
            sum <= budget + 1e-6,
            "screen active sum {sum} within {budget}"
        );
        for (i, &t) in targets.iter().enumerate() {
            assert!(t >= tiers[i].min_bitrate_kbps as f64 - 1e-6, "floor held");
        }
    }

    #[test]
    fn test_screen_budget_shrinks_with_active_layers() {
        let tiers = simulcast_screen_layers(3);
        assert!(uplink_budget_kbps(tiers, 3) > uplink_budget_kbps(tiers, 2));
        assert!(uplink_budget_kbps(tiers, 2) > uplink_budget_kbps(tiers, 1));
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
    // Climb-rate limiter constant validation
    // =====================================================================

    #[test]
    fn test_climb_rate_limiter_constants() {
        assert!(
            CLIMB_COOLDOWN_BASE_MS > 0.0,
            "base cooldown must be positive"
        );
        assert!(
            CLIMB_COOLDOWN_MAX_MS >= CLIMB_COOLDOWN_BASE_MS,
            "max cooldown ({}) must be >= base ({})",
            CLIMB_COOLDOWN_MAX_MS,
            CLIMB_COOLDOWN_BASE_MS,
        );
        assert!(
            CLIMB_COOLDOWN_BACKOFF > 1.0,
            "backoff multiplier must be > 1.0"
        );
        assert!(
            RECOVERY_SLOWDOWN_FACTOR >= 1.0,
            "slowdown factor must be >= 1.0"
        );
        assert!(
            RECOVERY_SLOWDOWN_DECAY_MS > 0.0,
            "slowdown decay must be positive"
        );
        assert!(
            CRASH_MEMORY_RESET_MS >= CLIMB_COOLDOWN_MAX_MS,
            "crash memory reset ({}) should be >= max cooldown ({}) so ceiling decays before memory resets",
            CRASH_MEMORY_RESET_MS,
            CLIMB_COOLDOWN_MAX_MS,
        );
        assert!(
            YOYO_DETECTION_WINDOW_MS > 0.0,
            "yo-yo window must be positive"
        );
        assert!(
            REELECTION_CEILING_SUPPRESSION_MS > 0.0,
            "re-election suppression must be positive"
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

    // =====================================================================
    // initial_screen_tier decision function
    // =====================================================================

    #[test]
    fn initial_screen_tier_cold_start_returns_high() {
        // No signals at all → optimistic high tier (existing behaviour unchanged).
        assert_eq!(initial_screen_tier(None, None), 0);
    }

    #[test]
    fn initial_screen_tier_good_rtt_good_camera_returns_high() {
        // RTT well below FAIR threshold, camera not degraded → high tier.
        assert_eq!(initial_screen_tier(Some(50.0), Some(1)), 0);
        assert_eq!(initial_screen_tier(Some(RTT_GOOD_MS), Some(2)), 0);
    }

    #[test]
    fn initial_screen_tier_fair_rtt_returns_medium() {
        // RTT exactly at FAIR threshold → medium tier, regardless of camera.
        assert_eq!(initial_screen_tier(Some(RTT_FAIR_MS), Some(0)), 1);
        assert_eq!(initial_screen_tier(Some(RTT_FAIR_MS), None), 1);
        // Above FAIR but below POOR → still medium.
        assert_eq!(initial_screen_tier(Some(300.0), Some(1)), 1);
    }

    #[test]
    fn initial_screen_tier_poor_rtt_returns_low() {
        // RTT at or above POOR threshold → low tier regardless of camera.
        assert_eq!(initial_screen_tier(Some(RTT_POOR_MS), Some(0)), 2);
        assert_eq!(initial_screen_tier(Some(RTT_POOR_MS), None), 2);
        assert_eq!(initial_screen_tier(Some(1000.0), Some(2)), 2);
    }

    #[test]
    fn initial_screen_tier_degraded_camera_no_rtt_returns_medium() {
        // Camera already at sd (3) or low (4) tier, RTT unknown → medium.
        assert_eq!(initial_screen_tier(None, Some(3)), 1);
        assert_eq!(initial_screen_tier(None, Some(4)), 1);
    }

    #[test]
    fn initial_screen_tier_good_rtt_degraded_camera_returns_medium() {
        // Good RTT but camera already degraded → conservative medium tier.
        assert_eq!(initial_screen_tier(Some(50.0), Some(3)), 1);
        assert_eq!(initial_screen_tier(Some(RTT_GOOD_MS), Some(4)), 1);
    }

    #[test]
    fn initial_screen_tier_camera_only_not_degraded_returns_high() {
        // Camera not degraded (tier ≤ 2), no RTT → high tier.
        assert_eq!(initial_screen_tier(None, Some(0)), 0);
        assert_eq!(initial_screen_tier(None, Some(2)), 0);
    }

    #[test]
    fn initial_screen_tier_result_always_in_bounds() {
        // Whatever inputs are given, result must be a valid SCREEN_QUALITY_TIERS index.
        let cases = [
            (None, None),
            (Some(0.0), None),
            (Some(RTT_FAIR_MS), Some(0)),
            (Some(RTT_POOR_MS), Some(4)),
            (Some(9999.0), Some(99)),
        ];
        for (rtt, cam) in cases {
            let idx = initial_screen_tier(rtt, cam);
            assert!(
                idx < SCREEN_QUALITY_TIERS.len(),
                "initial_screen_tier({:?}, {:?}) = {} is out of bounds (len={})",
                rtt,
                cam,
                idx,
                SCREEN_QUALITY_TIERS.len(),
            );
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

/// Number of quality tiers to drop in a single self-targeted CONGESTION cut.
///
/// A self-targeted CONGESTION signal means the relay is actively dropping *our*
/// outbound packets — the buffer is already overflowing. A gentle one-tier
/// step-down (as used for WebSocket backpressure) is too slow: it sheds only
/// ~20-30% of bitrate per step and waits `MIN_TIER_TRANSITION_INTERVAL_MS`
/// between steps, so the relay buffer keeps overflowing for several seconds.
///
/// Dropping two tiers at once maps to roughly a 50% bitrate cut across most of
/// the (non-uniform) camera ladder — e.g. from the default "medium" tier
/// (index 4, ideal 600 kbps) two tiers down to index 6 (ideal 250 kbps) is a
/// ~58% reduction, and "hd" (index 2, ideal 1500) → index 4 (ideal 600) is a
/// 60% reduction. This sheds enough bitrate immediately to let the relay buffer
/// drain instead of bleeding it down one slow step at a time.
pub const CONGESTION_CUT_TIERS: usize = 2;

/// Duration (milliseconds) to pin the PID bitrate ceiling to the post-cut tier
/// after a self-targeted CONGESTION cut.
///
/// After the cut we must keep the effective bitrate low long enough for the
/// already-overflowing relay buffer to drain. Without a hold the PID — which
/// fine-tunes bitrate *within* a tier — would immediately ramp back toward the
/// new tier's max, re-filling the buffer before it has drained. Pinning the
/// ceiling to the post-cut tier's lower bound for this window guarantees the
/// buffer gets a real chance to recover. 2.5s comfortably covers a typical
/// relay buffer drain even on high-latency links while remaining short enough
/// that recovery is not penalized for long.
pub const CONGESTION_HOLD_MS: f64 = 2500.0;

// ---------------------------------------------------------------------------
// Client-Side WebSocket Backpressure Self-Detection
// ---------------------------------------------------------------------------

/// Number of client-side WebSocket send-buffer drops within
/// [`WS_SELF_CONGESTION_WINDOW_MS`] that triggers a local AQ step-down.
///
/// Lower than the server-side threshold (5) because client-side drops are a
/// more direct signal — each drop means the browser TCP send buffer is full.
pub const WS_SELF_CONGESTION_DROP_THRESHOLD: u64 = 3;

/// Tumbling window (ms) for counting client-side WS drops.
pub const WS_SELF_CONGESTION_WINDOW_MS: f64 = 1000.0;

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
        keyframe_interval_frames: 150, // ~5s at 30fps; wall-clock cap guarantees ≤5s
    },
    VideoQualityTier {
        label: "hd_plus",
        max_width: 1600,
        max_height: 900,
        target_fps: 30,
        ideal_bitrate_kbps: 2000,
        min_bitrate_kbps: 1200,
        max_bitrate_kbps: 2500,
        keyframe_interval_frames: 150, // ~5s at 30fps; wall-clock cap guarantees ≤5s
    },
    VideoQualityTier {
        label: "hd",
        max_width: 1280,
        max_height: 720,
        target_fps: 30,
        ideal_bitrate_kbps: 1500,
        min_bitrate_kbps: 800,
        max_bitrate_kbps: 2000,
        keyframe_interval_frames: 150, // ~5s at 30fps; wall-clock cap guarantees ≤5s
    },
    VideoQualityTier {
        label: "standard",
        max_width: 960,
        max_height: 540,
        target_fps: 30,
        ideal_bitrate_kbps: 900,
        min_bitrate_kbps: 500,
        max_bitrate_kbps: 1500,
        keyframe_interval_frames: 150, // ~5s at 30fps; wall-clock cap guarantees ≤5s
    },
    VideoQualityTier {
        label: "medium",
        max_width: 854,
        max_height: 480,
        target_fps: 25,
        ideal_bitrate_kbps: 600,
        min_bitrate_kbps: 300,
        max_bitrate_kbps: 1000,
        keyframe_interval_frames: 125, // ~5s at 25fps; wall-clock cap guarantees ≤5s
    },
    VideoQualityTier {
        label: "low",
        max_width: 640,
        max_height: 360,
        target_fps: 20,
        ideal_bitrate_kbps: 400,
        min_bitrate_kbps: 200,
        max_bitrate_kbps: 600,
        keyframe_interval_frames: 100, // ~5s at 20fps; wall-clock cap guarantees ≤5s
    },
    VideoQualityTier {
        label: "very_low",
        max_width: 480,
        max_height: 270,
        target_fps: 15,
        ideal_bitrate_kbps: 250,
        min_bitrate_kbps: 100,
        max_bitrate_kbps: 400,
        keyframe_interval_frames: 75, // ~5s at 15fps; wall-clock cap guarantees ≤5s
    },
    VideoQualityTier {
        label: "minimal",
        max_width: 426,
        max_height: 240,
        target_fps: 10,
        ideal_bitrate_kbps: 150,
        min_bitrate_kbps: 50,
        max_bitrate_kbps: 250,
        keyframe_interval_frames: 50, // ~5s at 10fps; wall-clock cap guarantees ≤5s
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

/// The camera simulcast layer ladder — **THE tuning point** for camera
/// simulcast (issue #1768).
///
/// Simulcast (issue #989) lets a publisher encode the *same* camera feed at
/// several fixed quality layers simultaneously, tagging each encoded chunk with
/// a cleartext layer id. Generating and provisioning those layers costs CPU
/// (one full encode per active layer) and uplink bandwidth (the *sum* of the
/// active layers' bitrates). This table is the **single source of truth** for
/// the camera simulcast ladder: to make the layers lighter or heavier, edit the
/// rung values here and nowhere else.
///
/// ## Why a dedicated table (not indices into `VIDEO_QUALITY_TIERS`)
///
/// The simulcast ladder deliberately reaches lower and lighter than the
/// adaptive single-stream ladder ([`VIDEO_QUALITY_TIERS`]): its base rung is
/// `320×180 @ 7 fps`, well below that ladder's constrained-but-usable bottom.
/// Folding these rungs into `VIDEO_QUALITY_TIERS` would either break that
/// ladder's monotonic descent or drag its adaptive floor down for every
/// single-stream publisher — an unrelated regression. Keeping the simulcast
/// ladder in its own table isolates simulcast tuning from adaptive tuning while
/// still being one obvious place to edit. The rung *labels* (`low` / `standard`
/// / `hd`) are the simulcast layer names and are intentionally independent of
/// the like-named adaptive tiers.
///
/// ## Ladder (issue #1768): lighter, resolution-independent-of-fps
///
/// Ordered **lowest layer first** (`layer_id` == position in the slice):
///
/// - layer 0 = `low`      — 320×180 @ 7 fps  / ideal ~120 kbps (constrained-net rescue)
/// - layer 1 = `standard` — 640×360 @ 15 fps / ideal ~350 kbps
/// - layer 2 = `hd`       — 1280×720 @ 30 fps / ideal ~1500 kbps
///
/// Each rung's `max_width`/`max_height` and `target_fps` are set independently:
/// resolution and framerate do not have to move together. The framerate is
/// delivered per-layer at encode time by dropping frames that arrive faster than
/// the rung's `target_fps` (see `camera_encoder.rs` and
/// [`SIMULCAST_LAYER_FPS_THROTTLE_SLACK`]).
///
/// ## Real-time over smoothness (issue #1768)
///
/// The imperative is that each encoded frame is as close to *now* as possible,
/// even at the cost of smoothness or resolution. Two mechanisms deliver this and
/// nothing in the capture→encode path buffers a backlog:
///   1. Every encoder is configured `LatencyMode::Realtime` (camera + screen),
///      so the codec never trades latency for compression efficiency.
///   2. The per-layer framerate cap DROPS the intervening frames rather than
///      queuing them — a layer running below the source cadence always encodes
///      the *newest* eligible frame, never a stale backlog. Encoder-queue depth
///      is monitored (`encode_queue_size()`), and sustained depth sheds the top
///      layer / steps quality down instead of letting latency grow.
///
/// ## These are INDEPENDENT simulcast encodes — NOT nested SVC layers
///
/// Each `layer_id` is a SEPARATE, self-contained encode of the whole frame at
/// that resolution/bitrate. Layer 2 is NOT layer 0 + 1 + an enhancement on top;
/// it is its own complete stream. Decode and relay-forwarding are therefore
/// **exact-match on `layer_id`, not cumulative ("layer N and below")**:
///   * the relay forwards ONLY the one `layer_id` a receiver requested for a
///     source (see the `chat_server.rs` forwarding filter), never `0..=N`; and
///   * the receiver decode guard accepts ONLY packets whose `layer_id` equals
///     its currently-selected layer (see `peer_decode_manager.rs`); a packet of
///     any other layer is dropped.
///
/// Consequence — do NOT reason about this as SVC: if a receiver's selected
/// layer and the layer the relay is forwarding ever DISAGREE, the receiver gets
/// NOTHING decodable and the tile FREEZES on its last-good frame; it does NOT
/// fall back to a lower-quality frame. So a selected layer must never lead the
/// requested-layer wire state (issue #1695). The "base layer is always
/// forwarded / shed the top layer" language elsewhere is the PUBLISHER's view
/// (it produces a stack and sheds from the top) — it does not make the wire
/// layers nested.
///
/// Ordering lowest-first matches the receiver guard (PR A) that defaults to
/// decoding the lowest layer (`layer_id == 0`): the base layer is the cheapest
/// to decode and the most resilient under congestion, and dropping the **top**
/// active layer under congestion sheds the highest-cost stream first.
pub const SIMULCAST_VIDEO_LAYERS: &[VideoQualityTier] = &[
    VideoQualityTier {
        label: "low",
        max_width: 320,
        max_height: 180,
        target_fps: 7,
        ideal_bitrate_kbps: 120,
        min_bitrate_kbps: 60, // achievable on ~100-200 kbps constrained links
        max_bitrate_kbps: 200,
        keyframe_interval_frames: 35, // ~5s at 7fps; wall-clock cap guarantees ≤5s
    },
    VideoQualityTier {
        label: "standard",
        max_width: 640,
        max_height: 360,
        target_fps: 15,
        ideal_bitrate_kbps: 350,
        min_bitrate_kbps: 150,
        max_bitrate_kbps: 600,
        keyframe_interval_frames: 75, // ~5s at 15fps; wall-clock cap guarantees ≤5s
    },
    VideoQualityTier {
        label: "hd",
        max_width: 1280,
        max_height: 720,
        target_fps: 30,
        ideal_bitrate_kbps: 1500,
        min_bitrate_kbps: 800,
        max_bitrate_kbps: 2000,
        keyframe_interval_frames: 150, // ~5s at 30fps; wall-clock cap guarantees ≤5s
    },
];

/// Per-layer framerate-cap slack for the simulcast encode throttle (issue
/// #1768), as a fraction of the rung's nominal frame interval.
///
/// A simulcast layer encodes the newest source frame only once at least
/// `(1 - SLACK) × (1000 / target_fps)` ms have elapsed since its last encode;
/// intervening frames are DROPPED (real-time over smoothness — never queued).
/// The slack lets a frame that arrives slightly early still count, so a 7 fps
/// rung fed by a 30 fps capture lands near 7 fps instead of quantizing down to
/// 6 fps (it would otherwise always wait for the 5th 33.3 ms capture tick).
/// Keyframes (periodic GOP or a PLI) bypass the cap entirely so every layer's
/// GOP stays coherent.
pub const SIMULCAST_LAYER_FPS_THROTTLE_SLACK: f64 = 0.15;

/// Maximum number of simulcast layers in the full ladder.
///
/// Equal to `SIMULCAST_VIDEO_LAYERS.len()`. Mirrors
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
/// (index in the returned slice == `layer_id`). The rungs come from
/// [`SIMULCAST_VIDEO_LAYERS`] selected via [`spaced_ladder_positions`], so it is
/// driven entirely by the ladder length — for the current 3-rung ladder:
///
/// - `n == 1` → `[low]` (single base layer — used when simulcast is off or the
///   device is too weak). The AQ controller still drives the single stream's
///   resolution/bitrate ADAPTIVELY in the common case, so this `low` tier is not
///   an unconditional override. **Exception (issue #1136):** when a single-layer
///   publisher is in a call with **more than 3 other peers**, `camera_encoder.rs`
///   pins the single stream to THIS `low` rung (320×180 / low ideal) as a
///   ceiling — one adaptive medium-tier stream is too heavy on every receiver's
///   decoder at that scale. With ≤3 peers the single stream stays fully
///   adaptive. See the single-layer low-rung pin in `camera_encoder.rs`.
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
        // SIMULCAST_MAX_LAYERS requires no change here. The ladder is already
        // lowest-first, so a selected position IS the layer id (issue #1768).
        spaced_ladder_positions(n, SIMULCAST_VIDEO_LAYERS.len())
            .into_iter()
            .map(|pos| {
                let t = &SIMULCAST_VIDEO_LAYERS[pos];
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
/// nominal quality. Examples for the camera ladder (`[low, standard, hd]`,
/// ideals 120 / 350 / 1500, issue #1768):
///
/// - 1 active layer  → 120 kbps
/// - 2 active layers → 470 kbps
/// - 3 active layers → 1970 kbps
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
        keyframe_interval_frames: 30, // ~3s at 10fps (text readability); wall-clock cap ≤3s
    },
    VideoQualityTier {
        label: "medium",
        max_width: 1280,
        max_height: 720,
        target_fps: 8,
        ideal_bitrate_kbps: 1200,
        min_bitrate_kbps: 700,
        max_bitrate_kbps: 2000,
        keyframe_interval_frames: 24, // ~3s at 8fps (text readability); wall-clock cap ≤3s
    },
    VideoQualityTier {
        label: "low",
        max_width: 1280,
        max_height: 720,
        target_fps: 5,
        ideal_bitrate_kbps: 500,
        min_bitrate_kbps: 250,
        max_bitrate_kbps: 1000,
        keyframe_interval_frames: 15, // ~3s at 5fps (text readability); wall-clock cap ≤3s
    },
];

/// Maximum number of SCREEN simulcast layers (issue #989, Phase 3).
pub const SCREEN_SIMULCAST_MAX_LAYERS: usize = 3;

/// Initial number of ACTIVE screen simulcast layers seeded at (re)share start
/// (issue #1553).
///
/// # Why this exists (issue #1553)
/// The screen path used to seed `active_layer_count == 1` (base rung only) and
/// relied on the headroom-probe ramp ([`LAYER_PROBE_CLEAR_WINDOW_MS`]) to earn
/// every upper rung. That ramp demands the encoder queue be **uninterruptedly**
/// clear for 6 s per rung; on a busy share in a large (~15-peer) meeting the
/// queue never stays clear that long, so the share stalled permanently on the
/// base rung — `low` (720p / 500 kbps / 5 fps) — and looked FUZZY forever.
///
/// # Decision: start OPTIMISTIC, shed DOWN on real backpressure (Option B)
/// Seed the screen ladder at this many active rungs instead of 1, so a clear
/// share gets a solid baseline from frame one without waiting on the 6 s ramp.
/// At the `2`-rung seed the ladder is `[low, high]` (see
/// [`simulcast_screen_layers`] just below: `n == 2 => [low, high]`), so the
/// publisher emits the base `low` (720p / 500 kbps) AND the top `high`
/// (1080p / 2500 kbps) rung — ≈ 3000 kbps across TWO simultaneous encodes (one
/// of them the full 1080p) immediately. The EXISTING shed-down machinery
/// (`drop_top_layer` under sustained encoder backpressure / congestion) still
/// reduces active toward the floor (1) under genuine congestion, and the ramp
/// can still earn the deferred MIDDLE rung up to the full 3-rung ceiling when
/// uplink allows.
///
/// # Why `2` and not the full ladder (the #1200 tradeoff)
/// Issue #1200 deliberately removed the "all rungs hot from frame one" cold
/// start (active == n == 3 → `[low, medium, high]`, ~4.2 Mbps across THREE
/// simultaneous encodes the instant a share begins) because that slam was too
/// aggressive. Seeding at `2` is the middle ground that honors BOTH issues:
/// - **#1553**: publishing the sharp `high` (1080p) rung immediately is exactly
///   what de-fuzzes the shared content for a healthy receiver — the whole point
///   of the issue — instead of stalling at the base rung waiting on the 6 s ramp.
/// - **#1200**: 2 is strictly fewer than the 3-rung ladder, so it does NOT
///   reintroduce the all-rungs-hot slam. What the seed leaves OFF is the THIRD
///   simultaneous encode — the MIDDLE `medium` rung (720p / 1200 kbps), present
///   only in the full `[low, medium, high]` ladder — NOT the 1080p top. The
///   honest comparison is "2 encodes (incl. the 1080p `high`) / ≈ 3000 kbps" at
///   the seed vs "3 encodes / ≈ 4200 kbps" for the #1200 slam; the deferred
///   `medium` rung is earned by the ramp (or restored after a shed).
///
/// Clamped against the actual ladder size by the seed method (a `1`-layer /
/// single-stream session stays at active 1), so this never exceeds the ceiling.
pub const SCREEN_INITIAL_ACTIVE_LAYERS: usize = 2;

// The optimistic seed must be ≥ 1 (the base rung is always published) and must
// not exceed the screen ladder ceiling (otherwise the "middle ground vs #1200"
// intent collapses into the full all-rungs-hot slam #1200 removed). Asserting at
// COMPILE time so a future retune that violates either bound fails the build.
const _: () = assert!(
    SCREEN_INITIAL_ACTIVE_LAYERS >= 1,
    "screen initial-active seed must include at least the base rung"
);
const _: () = assert!(
    SCREEN_INITIAL_ACTIVE_LAYERS <= SCREEN_SIMULCAST_MAX_LAYERS,
    "screen initial-active seed must not exceed the screen ladder ceiling"
);
const _: () = assert!(
    SCREEN_INITIAL_ACTIVE_LAYERS < SCREEN_SIMULCAST_MAX_LAYERS,
    "screen initial-active seed must be strictly below the ceiling — seeding the \
     full ladder reintroduces the all-rungs-hot cold-start slam removed by #1200"
);

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
    /// Expected packet-loss percentage (0-100) passed to the Opus encoder
    /// (`OPUS_SET_PACKET_LOSS_PERC`). libopus scales how much redundant FEC
    /// data it embeds by this hint, so it is only meaningful when
    /// `enable_fec` is true. The top ("high") tier keeps 0 (FEC off); the
    /// degraded tiers escalate the hint so the encoder embeds proportionally
    /// more recovery data as the network worsens.
    pub packet_loss_perc: u32,
}

/// Audio quality tiers, ordered from highest (index 0) to lowest.
pub const AUDIO_QUALITY_TIERS: &[AudioQualityTier] = &[
    // Audio ladder (issue #1768): the three named levels a receiver can select
    // are high=48 / medium=24 / low=12 kbps; `emergency` is a publisher-only
    // rescue rung below the receiver ladder's base, escalated under the worst
    // links. Keep this coherent with the receiver ladder `AUDIO_LAYER_KBPS`
    // ([12, 24, 48]) and the publisher simulcast ladder
    // `AUDIO_SIMULCAST_LAYER_KBPS` in `microphone_encoder.rs`.
    AudioQualityTier {
        label: "high",
        bitrate_kbps: 48,
        enable_dtx: true,
        enable_fec: false,
        // No FEC at the top tier: the link is healthy, so spend no overhead.
        packet_loss_perc: 0,
    },
    AudioQualityTier {
        label: "medium",
        bitrate_kbps: 24,
        enable_dtx: true,
        enable_fec: true, // enable FEC under moderate loss
        // First degraded tier: tell Opus to expect ~10% loss (issue #619 range).
        packet_loss_perc: 10,
    },
    AudioQualityTier {
        label: "low",
        bitrate_kbps: 12,
        enable_dtx: true,
        enable_fec: true,
        packet_loss_perc: 15,
    },
    AudioQualityTier {
        label: "emergency",
        bitrate_kbps: 8,
        enable_dtx: true,
        enable_fec: true,
        // Deepest rescue rung (below the receiver ladder's 12 kbps base): the
        // link is failing, so hint Opus toward maximum FEC redundancy.
        packet_loss_perc: 25,
    },
];

// ---------------------------------------------------------------------------
// Tier Transition Thresholds
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// REMOVED (issue #1108, Phase B / Stage 2): receiver-FPS / bitrate-ratio tier
// hysteresis constants.
//
// `VIDEO_TIER_DEGRADE_FPS_RATIO[_LENIENT]`, `VIDEO_TIER_RECOVER_FPS_RATIO`,
// `VIDEO_TIER_DEGRADE/RECOVER_BITRATE_RATIO`, `AUDIO_TIER_DEGRADE/RECOVER_FPS_RATIO`,
// and `AQ_OUTLIER_HEALTH_FPS_RATIO` / `AQ_OUTLIER_GAP_FPS_RATIO` all gated tier
// transitions on the FPS that *peers reported receiving*. The sender now adapts
// only to its own signals, so the gradual degrade/recover decision is a boolean
// from the encoder-backpressure timers (see the Sender Encoder Backpressure
// constants below and `EncoderBitrateController::tick`). The step-DOWN reaction
// time and step-UP stabilization WINDOW (timing, not thresholds) are unchanged.
// ---------------------------------------------------------------------------

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
// The sender's gradual quality axis is now driven by its OWN encode
// backpressure (issue #1108, Stage 2 removed receiver FPS from the sender AQ).
// When the active encoders' `encode_queue_size()` stays high for a sustained
// window the controller sheds a layer / steps a tier down to relieve encode
// CPU; once it drains back to clear over the stabilization window it recovers.
// Consumed by `EncoderBitrateController::tick`.

/// Encoder queue depth (frames pending in the WebCodecs `VideoEncoder`) at or
/// above which the sender is considered to be in encode backpressure. Sampled
/// as the max `encode_queue_size()` across active simulcast layers. A healthy
/// realtime encoder drains to ~0–1 each tick, so a sustained depth of 3 means
/// the encoder is consistently a few frames behind capture.
///
/// First-guess value — pending a performance-reviewer pass. DO NOT treat as
/// final.
pub const ENCODER_QUEUE_BACKPRESSURE_HIGH: u32 = 3;

/// Encoder queue depth at or below which sender encode backpressure is
/// considered cleared (hysteresis floor against the HIGH threshold). Once the
/// queue drains back to this depth the recover (step-up) timer can accumulate.
///
/// First-guess value — pending a performance-reviewer pass. DO NOT treat as
/// final.
pub const ENCODER_QUEUE_BACKPRESSURE_CLEAR: u32 = 1;

/// How long (milliseconds) the encoder queue depth must stay at/above
/// [`ENCODER_QUEUE_BACKPRESSURE_HIGH`] before the controller steps down. Sized
/// in the same ballpark as `STEP_DOWN_REACTION_TIME_MS` so a brief encode hiccup
/// (a single slow frame, a GC pause) does not trigger a shed.
///
/// First-guess value — pending a performance-reviewer pass. DO NOT treat as
/// final.
pub const ENCODER_BACKPRESSURE_SUSTAIN_MS: f64 = 1500.0;

/// Cadence (milliseconds) at which the encoder control loop calls
/// `EncoderBitrateController::tick` (issue #1108). Now that the sender AQ is a
/// self-timer (receiver-FPS diagnostics no longer drive it), the browser encode
/// control loops and the native bot both tick at this rate. Chosen at ~1 Hz to
/// match the historical diagnostics cadence so the AQ timing constants
/// (`MIN_TIER_TRANSITION_INTERVAL_MS`, the sustain/stabilization windows) keep
/// their effective behavior.
pub const AQ_TICK_INTERVAL_MS: u64 = 1000;

// ---------------------------------------------------------------------------
// Runtime simulcast layer ramp-up (issue #1140 / #1141)
// ---------------------------------------------------------------------------
// The cold CPU benchmark no longer gates simulcast layer count. Every camera
// publisher starts at 1 active layer (the legacy single-stream path) and the
// `EncoderBitrateController` *earns* additional layers up to the device ceiling
// at runtime, based on observed encoder-queue backpressure headroom + uplink
// budget. These constants govern that conservative, self-limiting probe.

/// How long (milliseconds) the encoder queue depth must stay sustained-CLEAR
/// (at/below [`ENCODER_QUEUE_BACKPRESSURE_CLEAR`]) before the controller probes
/// adding ONE simulcast layer (issue #1141).
///
/// **Deliberately asymmetric**: this dwell is LONGER than both the shed sustain
/// ([`ENCODER_BACKPRESSURE_SUSTAIN_MS`] = 1.5 s) and the tier step-up window
/// ([`STEP_UP_STABILIZATION_WINDOW_MS`] = 5 s). Adding a layer is ~N× the encode
/// CPU + uplink of a tier-bitrate step, so a wrong add is far more expensive to
/// recover from than a wrong tier nudge — we want the device to prove it has
/// been comfortably idle for a stable window before committing more CPU.
/// 6 s is 4× the shed sustain (still add-slow / shed-fast) and exceeds the 5 s
/// tier step-up window, while keeping the cold-start ramp brisk: ~one rung every
/// 6 s rather than every 12 s, so a capable publisher reaches 2 layers in ~11 s
/// and 3 in ~17 s instead of stalling on the base rung for ~half a minute.
///
/// **Provenance: REASONED, not measured (issue #1141 / #1159).** The 6 s value is
/// derived from the constant-relationship invariants above (it must exceed the
/// shed sustain and the tier step-up window), NOT from a multi-device capture of
/// real ramp behavior on weak uplinks / low-power CPUs. Treat it as a first cut
/// pending a performance-reviewer pass with field data; do not mistake the
/// detailed rationale for empirical validation.
pub const LAYER_PROBE_CLEAR_WINDOW_MS: f64 = 6_000.0;

/// Headroom (fraction, 0.0–1.0) the summed active-layer uplink budget must have
/// below the budget for the layer that would be ADDED before a probe-up is
/// allowed (issue #1141).
///
/// `encode_queue_size()` backpressure detects CPU/encoder saturation, NOT "my
/// uplink cannot carry another rung even though my CPU is bored". A probe-up is
/// only permitted when the uplink budget for `active + 1` layers exceeds the
/// budget for `active` layers by at least this fraction — i.e. there is genuine
/// uplink room for the new rung. (The budget is the sum of active tier ideals;
/// adding a layer raises it by that layer's ideal, so this is effectively
/// "would the next rung's nominal cost fit".) See [`uplink_budget_kbps`].
pub const LAYER_PROBE_MIN_UPLINK_HEADROOM_FRAC: f64 = 0.0;

/// Window (milliseconds) after a probe-added layer within which a shed of that
/// layer counts as an OSCILLATION, arming the anti-flap penalty box (issue
/// #1141). A probe that survives longer than this is considered a good bet and
/// does NOT lengthen the next backoff.
///
/// Sized just above the probe clear window (which is now 6 s) so "added, then
/// almost immediately shed" is caught while a layer that held for a meaningful
/// span is not penalized. A compile-time invariant below pins
/// `LAYER_PROBE_OSCILLATION_WINDOW_MS >= LAYER_PROBE_CLEAR_WINDOW_MS` so the two
/// can never drift apart silently if the clear window is retuned.
///
/// **Provenance: REASONED, not measured (issue #1141 / #1159).** The 8 s value is
/// chosen relative to the clear window, not from a capture of real add-then-shed
/// intervals. First-guess pending a performance-reviewer pass.
pub const LAYER_PROBE_OSCILLATION_WINDOW_MS: f64 = 8_000.0;

/// Initial penalty-box backoff (milliseconds) imposed after a probed-up layer
/// is shed within [`LAYER_PROBE_OSCILLATION_WINDOW_MS`] (issue #1141). The next
/// probe-up is suppressed until this long after the shed; each subsequent
/// oscillation doubles it (capped at [`LAYER_PROBE_PENALTY_MAX_MS`]), so a
/// device that flaps repeatedly settles low for the session.
///
/// **Provenance: REASONED, not measured (issue #1141 / #1159).** 15 s mirrors the
/// climb-rate limiter's escalation shape (15→30→60 s) rather than a measured flap
/// period; it is comfortably longer than the [`CONGESTION_HOLD_MS`] (2.5 s) drain
/// hold so an uplink-driven cut backs off instead of re-flapping each hold.
/// First-guess pending a performance-reviewer pass with field data.
pub const LAYER_PROBE_PENALTY_BASE_MS: f64 = 15_000.0;

/// Exponential backoff multiplier for the layer-probe penalty box on each repeat
/// oscillation (issue #1141): 15 s → 30 s → 60 s (capped). Mirrors the
/// climb-rate limiter's `CLIMB_COOLDOWN_BACKOFF`.
pub const LAYER_PROBE_PENALTY_BACKOFF: f64 = 2.0;

/// Maximum penalty-box backoff (milliseconds) for the layer probe (issue
/// #1141). Caps the 15 → 30 → 60 s escalation so a flapping device retries at
/// most once a minute rather than locking out forever.
pub const LAYER_PROBE_PENALTY_MAX_MS: f64 = 60_000.0;

// --- Compile-time invariants (issue #1108) ---
// These relationships are load-bearing for the backpressure hysteresis and the
// degrade-faster-than-recover asymmetry; assert them at COMPILE time so a bad
// edit fails the build (and stays clippy-clean — `assert!` on a constant in a
// runtime test trips `assertions_on_constants`).

/// The CLEAR (recover) threshold must be strictly below the HIGH (degrade)
/// threshold so there is a hysteresis dead-band between them, preventing
/// oscillation around a single encoder-queue depth.
const _: () = assert!(
    ENCODER_QUEUE_BACKPRESSURE_CLEAR < ENCODER_QUEUE_BACKPRESSURE_HIGH,
    "backpressure CLEAR must be < HIGH to leave a hysteresis dead-band"
);

/// The backpressure sustain window must be positive (a non-positive window would
/// make every transient spike fire a step-down immediately).
const _: () = assert!(
    ENCODER_BACKPRESSURE_SUSTAIN_MS > 0.0,
    "backpressure sustain window must be positive"
);

/// Step-up must be slower than step-down (degradation reacts faster than
/// recovery) to avoid tier flapping on unstable senders.
const _: () = assert!(
    STEP_UP_STABILIZATION_WINDOW_MS > STEP_DOWN_REACTION_TIME_MS,
    "step-up stabilization window must exceed step-down reaction time"
);

/// Adding a simulcast layer must require a LONGER clear dwell than shedding one
/// requires of sustained HIGH backpressure (issue #1141): the add is asymmetric
/// — far more expensive to get wrong — so it must be the slower direction.
const _: () = assert!(
    LAYER_PROBE_CLEAR_WINDOW_MS > ENCODER_BACKPRESSURE_SUSTAIN_MS,
    "layer-probe clear window must exceed the backpressure shed sustain (add slower than shed)"
);

/// A probe add must also dwell longer than a tier step-up (a layer add commits
/// ~N× more CPU/uplink than a within-tier bitrate nudge), so it is the most
/// conservative climb of all (issue #1141).
const _: () = assert!(
    LAYER_PROBE_CLEAR_WINDOW_MS >= STEP_UP_STABILIZATION_WINDOW_MS as f64,
    "layer-probe clear window must be at least the tier step-up window"
);

/// The oscillation window must be at least the clear window (issue #1141): a
/// probe that is shed before it even completed a fresh clear dwell is, by
/// definition, an oscillation. Keeping OSCILLATION >= CLEAR ensures the two stay
/// in the documented "oscillation window sits just above the clear window"
/// relationship and cannot silently drift apart when either is retuned.
const _: () = assert!(
    LAYER_PROBE_OSCILLATION_WINDOW_MS >= LAYER_PROBE_CLEAR_WINDOW_MS,
    "layer-probe oscillation window must be >= the clear window"
);

/// The penalty-box escalation must actually grow (issue #1141), and the base
/// must not already exceed the cap.
const _: () = assert!(
    LAYER_PROBE_PENALTY_BACKOFF > 1.0 && LAYER_PROBE_PENALTY_BASE_MS <= LAYER_PROBE_PENALTY_MAX_MS,
    "layer-probe penalty must escalate and start at/below its cap"
);

// --- Constant-relationship invariants (compile-time) ---
// Previously runtime `assert!`s in the test module; moved here so they are
// checked on every build and stay clippy-clean (`assertions_on_constants`).

// PID gains must be non-negative.
const _: () = assert!(PID_KP >= 0.0, "PID_KP must be non-negative");
const _: () = assert!(PID_KI >= 0.0, "PID_KI must be non-negative");
const _: () = assert!(PID_KD >= 0.0, "PID_KD must be non-negative");

// PID output range must be a valid (non-empty) interval.
const _: () = assert!(
    PID_OUTPUT_MIN < PID_OUTPUT_MAX,
    "PID output min must be < max"
);

// Climb-rate limiter relationships.
const _: () = assert!(
    CLIMB_COOLDOWN_BASE_MS > 0.0,
    "base cooldown must be positive"
);
const _: () = assert!(
    CLIMB_COOLDOWN_MAX_MS >= CLIMB_COOLDOWN_BASE_MS,
    "max cooldown must be >= base cooldown"
);
const _: () = assert!(
    CLIMB_COOLDOWN_BACKOFF > 1.0,
    "backoff multiplier must be > 1.0"
);
const _: () = assert!(
    RECOVERY_SLOWDOWN_FACTOR >= 1.0,
    "slowdown factor must be >= 1.0"
);
const _: () = assert!(
    RECOVERY_SLOWDOWN_DECAY_MS > 0.0,
    "slowdown decay must be positive"
);
const _: () = assert!(
    CRASH_MEMORY_RESET_MS >= CLIMB_COOLDOWN_MAX_MS,
    "crash memory reset should be >= max cooldown so the ceiling decays before memory resets"
);
const _: () = assert!(
    YOYO_DETECTION_WINDOW_MS > 0.0,
    "yo-yo window must be positive"
);
const _: () = assert!(
    REELECTION_CEILING_SUPPRESSION_MS > 0.0,
    "re-election suppression must be positive"
);

// Congestion feedback thresholds must be positive.
const _: () = assert!(
    CONGESTION_DROP_THRESHOLD > 0,
    "congestion drop threshold must be positive"
);
const _: () = assert!(
    CONGESTION_WINDOW_MS > 0,
    "congestion window must be positive"
);
const _: () = assert!(
    CONGESTION_NOTIFY_MIN_INTERVAL_MS > 0,
    "congestion notify interval must be positive"
);

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

/// Wall-clock ceiling on the camera periodic keyframe interval (milliseconds).
/// The frame-counted `keyframe_interval_frames` only guarantees ~5s at the tier's
/// nominal fps. Under CPU load or at low AQ tiers the actual fps drops and the
/// frame-counted floor stretches to 10–17s. This wall-clock cap guarantees a
/// periodic keyframe at least every 5s regardless of actual encode rate (issue #1510).
pub const PERIODIC_KEYFRAME_MAX_INTERVAL_MS: f64 = 5000.0;

/// Wall-clock ceiling for screen-share periodic keyframes (milliseconds).
/// Screen tiers use a ~3s nominal GOP for text readability. The screen-specific
/// cap preserves that 3s design intent under low-fps conditions (issue #1510).
pub const SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS: f64 = 3000.0;

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

/// Cooldown (milliseconds) the AUDIO simulcast publisher waits — with NO new
/// self-targeted CONGESTION signal — before climbing its congestion layer
/// ceiling back up by ONE rung (issue #621).
///
/// On a self-targeted CONGESTION the audio publisher cuts its congestion ceiling
/// straight to base-only (layer 0 / 24 kbps) — the aggressive analogue of the
/// video [`CONGESTION_CUT_TIERS`] cut, but expressed through the simulcast
/// layer-ceiling lever (the Opus AudioWorklet cannot reconfigure bitrate live, so
/// dropping the upper simulcast layers is the only available downshift). Recovery
/// then climbs ONE rung per cooldown window, so on a 3-rung ladder full restore
/// after a single congestion event takes `2 × cooldown`.
///
/// **Hysteresis interaction with the VIDEO/SCREEN downshift cadence.** Video and
/// audio share the same self-targeted CONGESTION trigger but recover on
/// deliberately DIFFERENT timescales:
///   * VIDEO/SCREEN cut via the PID controller and are pinned for the short
///     [`CONGESTION_HOLD_MS`] (2.5 s) drain window, then the PID re-ramps
///     bitrate *within* a tier over seconds — video is the high-bandwidth stream
///     the relay buffer cares about, so it recovers quickly once the buffer
///     drains.
///   * AUDIO is ~1-3% of call bandwidth, so re-adding an audio layer barely moves
///     the relay buffer; there is no urgency to restore it. We therefore use a
///     MUCH longer per-rung cooldown so a flapping link cannot thrash the audio
///     ladder (each re-add/re-cut would briefly perturb every receiver's RED
///     chain). Picking a window FAR longer than the video drain also guarantees
///     audio never climbs back *during* an active congestion episode that video
///     is still fighting.
///
/// Set to [`CLIMB_COOLDOWN_BASE_MS`] (2 min) so the audio per-rung recovery
/// cadence is aligned with the video crash-ceiling decay cadence rather than an
/// invented magic number; both express "wait a sustained-stable window before
/// trusting headroom again."
pub const AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS: f64 = CLIMB_COOLDOWN_BASE_MS;

/// Poll cadence (milliseconds) of the AUDIO congestion-recovery timer (issue
/// #621). Deliberately COARSE: the CONGESTION cut itself takes effect on the
/// next audio frame (the publish gate reads the ceiling atom live — the timer is
/// NOT on the cut path), so this interval governs only how promptly recovery
/// NOTICES that a cut happened and how granularly it climbs back. With a
/// [`AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS`] of 2 min, sub-second polling is
/// pointless; a 1 Hz tick keeps the per-rung climb timing effectively exact while
/// adding a negligible wakeup load on battery-constrained devices (vs. riding the
/// 20 Hz VAD interval, which would wake 20× as often for a minutes-long cooldown).
pub const AUDIO_CONGESTION_RECOVERY_TICK_MS: u32 = 1000;

/// Poll cadence (milliseconds) of the live Opus FEC ctl-reconfig timer (issue
/// #1567). The mic encoder runs a 1 Hz timer that reads the current audio tier,
/// derives `(enable_fec, packet_loss_perc)`, and — ONLY when that pair changed
/// since the last reconfig — posts a `reconfigOpus` message to the live encoder
/// worklet so inband FEC actually engages on a mid-call AQ tier drop (and
/// disengages on recovery).
///
/// 1 Hz is the chosen RATE-LIMIT: it caps reconfigs at one per second, so a
/// flapping tier cannot flood the worklet, while still engaging FEC within ~1 s
/// of a drop — far faster than packet-loss concealment matters at human
/// timescales. Combined with the change-detection in `audio_fec_reconfig_change`
/// (suppress when unchanged), a stable tier sends ZERO reconfigs. Matches
/// [`AUDIO_CONGESTION_RECOVERY_TICK_MS`] so the two mic-side 1 Hz timers share a
/// cadence and a wakeup budget on battery-constrained devices.
pub const AUDIO_FEC_RECONFIG_TICK_MS: u32 = 1000;

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

// ---------------------------------------------------------------------------
// Client-Side WebTransport Uplink-Backpressure Self-Detection (#1104)
// ---------------------------------------------------------------------------
//
// Background (2026-06-09 meeting_sync analysis): the sender AQ already adapts
// to producer-local signals — encode-queue backpressure, server CONGESTION,
// and (on WebSocket) the client TCP send-buffer drop counter. The OWN-UPLINK
// client-side trigger, however, was WebSocket-ONLY: `websocket_drop_count()`
// returns 0 on WebTransport, so a WT sender whose uplink is saturated got no
// proactive client-side shed and had to wait for the slower, indirect server
// CONGESTION signal.
//
// Signal choice — why unistream drops, NOT datagram drops:
//   On WebTransport the client sends audio/video/screen over PERSISTENT
//   unidirectional QUIC streams (`send_on_persistent_stream`); datagrams carry
//   ONLY periodic control traffic (heartbeats every 5s, RTT probes). So
//   `datagram_drop_count()` is a sparse, indirect proxy that never observes a
//   media frame. `unistream_drop_count()` increments when an actual media
//   frame fails to leave the uplink (QUIC stream reset / fatal write error) —
//   the true client-side WT analogue of the WS send-buffer drop. We key the
//   trigger off the unistream counter for that reason.
//
// Threshold rationale (deliberately NOT a copy of the WS values):
//   A unistream drop is a HARD event (stream reset / fatal write failure), not
//   a soft "send buffer momentarily full" the way a WS `bufferedAmount`
//   overflow is. On a lossy/high-latency link (200ms+ RTT, jitter) a single
//   transient reset — or a reset during re-election / a brief network blip —
//   must NOT shed a layer. We therefore require a SUSTAINED cluster of drops
//   within the window before shedding. The window is widened to 2000ms so the
//   evidence must persist across at least ~2 AQ ticks rather than a single
//   spike, and the threshold is set so an isolated reset cannot trip it.
//
// Double-shed avoidance:
//   A saturated WT uplink will eventually also raise the server CONGESTION
//   signal (relay drops -> CONGESTION back to sender). The encoder maintains
//   an INDEPENDENT window/snapshot for this counter (separate from the WS
//   window and from the server-congestion flag), and each axis sheds at most
//   one layer per window, so the paths cannot compound into a runaway
//   double step-down within a single window.

/// Number of client-side WebTransport persistent-unistream media-frame drops
/// (see `videocall_transport::webtransport::unistream_drop_count`) within
/// [`WT_SELF_CONGESTION_WINDOW_MS`] that triggers a local AQ step-down.
///
/// Set to 3 (not 1): a unistream drop is a hard stream-reset/write-failure
/// event, so a single one can be a transient glitch or a re-election artifact
/// on a lossy link. Requiring 3 within the window means only a SUSTAINED
/// inability to push media out trips the self-shed, leaving isolated resets to
/// the stream's own auto-reopen path.
pub const WT_SELF_CONGESTION_DROP_THRESHOLD: u64 = 3;

/// Tumbling window (ms) for counting client-side WT unistream drops.
///
/// Wider than the WS window (1000ms) because unistream drops are sparser and
/// harder-edged than WS send-buffer overflows; a 2000ms window requires the
/// drops to persist across multiple AQ ticks before shedding, which suppresses
/// false triggers from jitter / momentary loss on high-latency links.
pub const WT_SELF_CONGESTION_WINDOW_MS: f64 = 2000.0;

// --- Compile-time invariants (#1104) ---
// Checked on every build (clippy-clean, unlike a runtime assert on a const).
const _: () = assert!(
    WT_SELF_CONGESTION_WINDOW_MS >= WS_SELF_CONGESTION_WINDOW_MS,
    "WT self-congestion window must be at least as wide as the WS window: WT \
     unistream drops are harder-edged and sparser than WS send-buffer overflows, \
     so they must persist longer before shedding."
);
const _: () = assert!(
    WT_SELF_CONGESTION_DROP_THRESHOLD >= 2,
    "WT self-congestion threshold must require more than one drop so a single \
     transient stream reset (e.g. a re-election artifact or a brief network \
     blip on a lossy link) cannot shed a layer."
);

// ---------------------------------------------------------------------------
// Client-Side WebTransport Uplink-SATURATION Self-Detection (#1219 prerequisite)
// ---------------------------------------------------------------------------
//
// Why this is SEPARATE from the unistream-DROP detection above:
//   The drop counter (`unistream_drop_count`) only increments on stream/
//   connection TEARDOWN (STOP_SENDING / RESET_STREAM / session close). It does
//   NOT move on a slow-but-alive uplink: a WHATWG WritableStream signals
//   backpressure by leaving `writer.ready()` PENDING, not by rejecting the
//   write, and the WT media send path is fully `.await`-blocking. So on a
//   genuine BANDWIDTH cliff (link slow, ACKs still flowing, no reset) the drop
//   counter stays flat and a WT publisher would never self-shed. The transport
//   therefore also exposes a monotonic "slow-ready event" counter
//   (`unistream_ready_stall_count`): it increments once each time a single
//   `writer.ready().await` on an established media stream blocks longer than the
//   producer-side `READY_STALL_THRESHOLD_MS`. This block consumes THAT counter
//   the same way the drop block consumes the drop counter — a tumbling-window
//   delta test via `evaluate_self_congestion`.
//
// This is the prerequisite that lets the relay's sender-keyed CONGESTION
// behavior (which collapses a publisher's encoder for the WHOLE room when ONE
// receiver's downlink overflows, bug #1219) be REMOVED: with this signal a WT
// publisher detects its OWN uplink saturation directly, instead of leaning on
// the relay's mis-scoped, room-wide signal.

/// Number of client-side WebTransport slow-`ready()` (uplink-saturation) events
/// (see `videocall_transport::webtransport::unistream_ready_stall_count`) within
/// [`WT_SATURATION_WINDOW_MS`] that triggers a local AQ step-down.
///
/// Netsim-tunable. Set to 3 (matching the drop threshold, deliberately not 1):
/// a single slow `ready()` can be a one-off — a reordered/retransmitted packet
/// or a momentary congestion-window dip on a high-RTT link — so requiring 3
/// crossings within the window raises the bar above one isolated stall.
///
/// IMPORTANT — what "3 events" actually means (see the increment mechanism in
/// `webtransport.rs`): increments do NOT correspond to 3 separate stall episodes.
/// Because frame sends are spawned concurrently and share one `ready()` promise,
/// a SINGLE sustained stall that has K frames in flight produces ~K increments
/// at once when the promise resolves. At 25-30 fps a stall ≥ ~350-400ms easily
/// has ≥3 frames queued, so one fat-but-isolated stall episode WILL trip the
/// shed. The dominant false-positive guard is therefore the producer-side
/// `READY_STALL_THRESHOLD_MS` (250ms) — the wait must be genuinely long — NOT
/// the count of 3. A bursty-but-recovering link that never parks a frame past
/// 250ms will not shed; one that parks several frames past 250ms then recovers
/// WILL shed one rung (arguably a correct early shed, but a real quality drop).
/// The threshold IS now frame-rate-aware (issue #1618): when dual-streaming
/// (camera + screen), the producer-side `READY_STALL_THRESHOLD_MS` is raised
/// to a fixed `8 × screen_top_tier_frame_interval_ms` (800ms for 10fps top tier),
/// preventing K-amplification false positives on healthy links. This is a FIXED
/// bound, not recomputed as either stream degrades.
/// VALIDATE the bursty-recovery case on the #1080 netsim before relying on this
/// to replace the relay CONGESTION signal (#1219).
pub const WT_SATURATION_STALL_THRESHOLD: u64 = 3;

/// Tumbling window (ms) for counting client-side WT slow-`ready()` events.
///
/// Netsim-tunable. Matches [`WT_SELF_CONGESTION_WINDOW_MS`] (2000ms): the
/// evidence must persist across at least ~2 AQ ticks (`AQ_TICK_INTERVAL_MS` =
/// 1000ms) before shedding, so a single tick that happened to catch one slow
/// `ready()` cannot fire. Wider than the WS window for the same reason the WT
/// drop window is: WT signals are harder-edged and must persist longer.
pub const WT_SATURATION_WINDOW_MS: f64 = 2000.0;

// --- Compile-time invariants (#1219 prerequisite) ---
const _: () = assert!(
    WT_SATURATION_WINDOW_MS >= WS_SELF_CONGESTION_WINDOW_MS,
    "WT saturation window must be at least as wide as the WS window: a slow \
     ready() is a coarse, sparse signal and must persist longer before shedding."
);
const _: () = assert!(
    WT_SATURATION_STALL_THRESHOLD >= 2,
    "WT saturation threshold must require more than one slow ready() so a single \
     transient stall (a reordered packet / brief cwnd dip on a lossy link) \
     cannot shed a layer."
);

// ---------------------------------------------------------------------------
// Single-Layer AUDIO Uplink-Distress Self-Detection (#1398)
// ---------------------------------------------------------------------------
//
// A SINGLE-LAYER audio publisher (device capability-gated to 1 audio layer, or
// audio simulcast disabled) has no upper layer to shed under congestion, so the
// only available downshift is lowering the ONE running Opus stream's bitrate
// live (#1398). The CAMERA's AQ loop already self-detects publisher-uplink
// distress directly from the process-global transport counters — slow
// `writer.ready()` (`unistream_ready_stall_count`, WT bandwidth cliff) and WS
// send-buffer overflows (`websocket_drop_count`) — via `evaluate_self_congestion`
// with the WT/WS constants above. But that loop only runs while the CAMERA is
// on; an AUDIO-ONLY publisher has no such detector. #1398 therefore re-targets
// the audio bitrate floor onto the SAME live uplink counters, evaluated by a
// mic-side detector that runs even when the camera is off.
//
// These constants are the AUDIO analogue of the video WT/WS constants, but
// deliberately NOT a verbatim copy: AUDIO must shed AFTER VIDEO. The relationship
// is expressed against the existing video constants (drift-resistant, with the
// compile-time invariants below) rather than as bare literals.
//
// Why "after video", and why the WINDOW (not the count) does the heavy lifting:
//   The WT stall counter is a WEAK discriminator. A single sustained `ready()`
//   stall with K frames in flight produces ~K increments at once (see
//   WT_SATURATION_STALL_THRESHOLD's note), so a slightly higher COUNT threshold
//   (+2) is only a coarse nudge — it does not reliably make audio fire after
//   video. The dominant lever is the WINDOW: a longer tumbling window requires
//   the distress to PERSIST across more ticks before the audio detector fires,
//   so a transient cliff that the video detector already shed for (over its
//   shorter window) is given time to recover before audio — which costs more
//   per-bit to the call — is touched. Hence the audio windows are MULTIPLES of
//   the video windows (2x saturation, 4x WS), and that temporal ordering — not
//   the +2 count — is what implements "audio sheds after video".

/// Number of client-side WT slow-`ready()` (uplink-saturation) events within
/// [`AUDIO_UPLINK_SATURATION_WINDOW_MS`] that trips the SINGLE-LAYER audio
/// bitrate downshift (#1398). Two above the video saturation threshold so audio
/// requires marginally more evidence than video; the longer window does the
/// real "after video" work (see the module note above).
pub const AUDIO_UPLINK_SATURATION_STALL_THRESHOLD: u64 = WT_SATURATION_STALL_THRESHOLD + 2;

/// Number of client-side WS send-buffer drops within
/// [`AUDIO_UPLINK_WS_WINDOW_MS`] that trips the SINGLE-LAYER audio bitrate
/// downshift (#1398). Two above the WS self-congestion threshold.
pub const AUDIO_UPLINK_WS_DROP_THRESHOLD: u64 = WS_SELF_CONGESTION_DROP_THRESHOLD + 2;

/// Tumbling window (ms) for the audio WT-saturation detector (#1398). TWICE the
/// video saturation window: the audio detector must see the cliff persist across
/// roughly double the ticks the video detector needs before it sheds, so a
/// transient cliff already handled by video recovers before audio is touched.
pub const AUDIO_UPLINK_SATURATION_WINDOW_MS: f64 = 2.0 * WT_SATURATION_WINDOW_MS;

/// Tumbling window (ms) for the audio WS-backpressure detector (#1398). FOUR
/// times the video WS window: WS overflows are softer/faster-edged than WT
/// stalls, so a wider multiplier is needed to give the same "audio after video"
/// temporal separation.
pub const AUDIO_UPLINK_WS_WINDOW_MS: f64 = 4.0 * WS_SELF_CONGESTION_WINDOW_MS;

/// Number of client-side WT persistent-unistream media-frame DROPS within
/// [`AUDIO_UPLINK_WT_DROP_WINDOW_MS`] that trips the SINGLE-LAYER audio bitrate
/// downshift (#1398). Two above the video WT-drop threshold
/// ([`WT_SELF_CONGESTION_DROP_THRESHOLD`]) so audio requires marginally more
/// evidence than video; the wider window does the real "after video" work (see
/// the module note above). This is the AUDIO analogue of the camera AQ's
/// WT-DROP self-shed axis (`wt_drop_step_down_decision`) — the THIRD uplink
/// axis (alongside saturation and WS) that the mic-side detector ORs over, so a
/// hard unistream-reset cliff (drop counter climbing, not slow-`ready()`) sheds
/// audio just as the camera sheds video on the same counter.
pub const AUDIO_UPLINK_WT_DROP_THRESHOLD: u64 = WT_SELF_CONGESTION_DROP_THRESHOLD + 2;

/// Tumbling window (ms) for the audio WT-DROP detector (#1398). TWICE the video
/// WT-drop window ([`WT_SELF_CONGESTION_WINDOW_MS`]), NOT 4x like the WS window.
/// RATIONALE (matching the saturation 2x note): a WT unistream DROP is a
/// HARD-EDGED stream-reset/write-failure event — the same hard-edged class as
/// the WT slow-`ready()` saturation signal, and unlike the soft/faster-edged WS
/// send-buffer overflow. Hard-edged WT signals are already sparse and persist
/// across multiple ticks before they fire video, so a 2x window gives the same
/// "audio after video" temporal separation that the saturation axis gets at 2x;
/// the 4x multiplier the WS axis needs is specific to WS's softer edge.
pub const AUDIO_UPLINK_WT_DROP_WINDOW_MS: f64 = 2.0 * WT_SELF_CONGESTION_WINDOW_MS;

// --- Compile-time invariants (#1398) ---
// Audio must shed STRICTLY AFTER video on both axes. These pin the
// "audio-after-video" contract at build time so a future edit to the video
// constants that would let audio shed first (or simultaneously) fails the build,
// not a meeting. Mirrors the #1104 / #1219-prereq invariant asserts above.
const _: () = assert!(
    AUDIO_UPLINK_SATURATION_STALL_THRESHOLD > WT_SATURATION_STALL_THRESHOLD,
    "audio saturation threshold must exceed the video one so audio requires \
     more stall evidence than video before shedding (audio sheds after video)."
);
const _: () = assert!(
    AUDIO_UPLINK_WS_DROP_THRESHOLD > WS_SELF_CONGESTION_DROP_THRESHOLD,
    "audio WS drop threshold must exceed the video one (audio sheds after video)."
);
const _: () = assert!(
    AUDIO_UPLINK_SATURATION_WINDOW_MS > WT_SATURATION_WINDOW_MS,
    "audio saturation window must be WIDER than the video one: the longer window \
     (not the +2 count) is what makes audio shed after video, because the WT \
     stall counter is a weak count discriminator (see the module note)."
);
const _: () = assert!(
    AUDIO_UPLINK_WS_WINDOW_MS > WS_SELF_CONGESTION_WINDOW_MS,
    "audio WS window must be WIDER than the video one (audio sheds after video)."
);
const _: () = assert!(
    AUDIO_UPLINK_WT_DROP_THRESHOLD > WT_SELF_CONGESTION_DROP_THRESHOLD,
    "audio WT-drop threshold must exceed the video one so audio requires more \
     unistream-drop evidence than video before shedding (audio sheds after video)."
);
const _: () = assert!(
    AUDIO_UPLINK_WT_DROP_WINDOW_MS > WT_SELF_CONGESTION_WINDOW_MS,
    "audio WT-drop window must be WIDER than the video one (audio sheds after \
     video). It is 2x (not 4x like WS): a WT drop is hard-edged like the WT \
     saturation signal, so it gets the same 2x separation the saturation axis \
     gets — see the AUDIO_UPLINK_WT_DROP_WINDOW_MS doc."
);

/// Pure decision helper for the client-side self-congestion self-trigger
/// (used by the WebTransport uplink-backpressure block; #1104).
///
/// Implements the tumbling-window delta test in one testable place so the
/// encoder's inline block (which depends on wasm-only `js_sys::Date::now()`)
/// stays thin and the threshold/window/delta logic can be unit-tested off-wasm.
///
/// Given the monotonic cumulative drop counter (`current_drops`), the snapshot
/// taken at the start of the current window (`snapshot_drops`), how long the
/// window has been open (`elapsed_ms`), and the configured `window_ms` /
/// `threshold`, returns a [`SelfCongestionDecision`] telling the caller whether
/// to (a) step down now and (b) roll the window snapshot forward.
///
/// The window only "closes" (and thus can fire / roll) once `elapsed_ms >=
/// window_ms`; before that the caller keeps accumulating. The delta uses
/// `saturating_sub` so a counter that somehow appears to go backwards (it
/// should not — the counters are monotonic `AtomicU64`) can never underflow
/// into a spurious huge delta.
#[inline]
pub fn evaluate_self_congestion(
    current_drops: u64,
    snapshot_drops: u64,
    elapsed_ms: f64,
    window_ms: f64,
    threshold: u64,
) -> SelfCongestionDecision {
    if elapsed_ms < window_ms {
        // Window still open — keep accumulating, do not roll or fire.
        return SelfCongestionDecision {
            step_down: false,
            roll_window: false,
            new_snapshot: snapshot_drops,
        };
    }
    let delta = current_drops.saturating_sub(snapshot_drops);
    SelfCongestionDecision {
        step_down: delta >= threshold,
        roll_window: true,
        new_snapshot: current_drops,
    }
}

/// Outcome of [`evaluate_self_congestion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfCongestionDecision {
    /// True when the drop delta met or exceeded the threshold within the
    /// closed window — the caller should force a video step-down.
    pub step_down: bool,
    /// True when the window has closed and the caller should reset its window
    /// start time and adopt `new_snapshot` as the new baseline.
    pub roll_window: bool,
    /// The snapshot value the caller should store for the next window. Equals
    /// the input snapshot while the window is still open, or the current drop
    /// count once the window rolls.
    pub new_snapshot: u64,
}

// =====================================================================
// Per-receiver cross-sender proactive-PLI budget (issue #1479, option b)
// =====================================================================
//
// These bound how many PROACTIVE keyframe requests (PLIs) a single receiver may
// emit ACROSS ALL of its senders within a sliding window. They back the
// `PliBudget` in `videocall_client::decode::pli_budget`, which sits ABOVE the
// transport-agnostic `emit_keyframe_request` packet builder (so it applies
// identically to WebTransport and WebSocket).
//
// DEFENSE-IN-DEPTH, NOT A TIGHT THROTTLE. The authoritative limiter is the
// RELAY's per-receiver `KEYFRAME_REQUEST_MAX_PER_SEC = 32` cap (server-side,
// `actix-api/src/constants.rs`), which already coalesces this receiver's PLIs
// across senders. This client budget deliberately mirrors that 32/s cap rather
// than tightening it: the server stays the binding limit, and the client is a
// co-equal shadow that is a NO-OP in normal multi-sender recovery (a benign
// ceiling that only sheds genuinely-redundant same-window 2nd+ pokes once the
// shared cap is reached). It must NEVER be tighter than the relay, or it would
// shadow a guarantee the server upholds and risk shedding legitimate recovery.

/// Sliding window (ms) over which the per-receiver cross-sender proactive-PLI
/// budget counts ALLOWED requests (issue #1479). Matches the relay's 1s
/// `KEYFRAME_REQUEST` limiter window so the client shadow ages entries out on
/// the same cadence the server does. Wall-clock based, so a reconnect /
/// cold-start / tab-resume self-heals: once `now_ms` jumps past the window,
/// every stale entry is pruned and the budget is effectively empty again.
pub const KEYFRAME_REQUEST_WINDOW_MS: u64 = 1000;

/// Maximum ALLOWED proactive PLIs per receiver across ALL senders within
/// `KEYFRAME_REQUEST_WINDOW_MS` (issue #1479). Deliberately EQUAL to the relay's
/// `KEYFRAME_REQUEST_MAX_PER_SEC = 32` (NOT tighter): the SERVER remains the
/// authoritative/binding limit and this client cap is a co-equal defense-in-depth
/// ceiling. In normal multi-sender recovery this is a NO-OP — a sender's FIRST
/// request in each window is ALWAYS allowed (wedge-proof + the #1662 escalation
/// exemption), and each sender is already paced to <=1 proactive PLI/s by the
/// #1494 per-sender backoff in `jitter_buffer.rs`, so reaching 32 distinct
/// senders' firsts within one window requires a 32-way simultaneous freeze. Only
/// a SECOND+ same-window poke from a sender that already fired this window can be
/// shed, and only once the cap is reached — at which point staleness priority
/// preserves the stalest contender. It is a benign ceiling, never a tight throttle.
pub const KEYFRAME_REQUEST_MAX_PER_WINDOW: usize = 32;

#[cfg(test)]
mod tests {
    use super::*;

    // =====================================================================
    // Client-side self-congestion self-trigger (#1104)
    //
    // These exercise the WT uplink-backpressure decision helper. Each case
    // maps directly to a Definition-of-Done requirement and is written so it
    // FAILS if the helper logic is broken (verified by mutation).
    // =====================================================================

    /// (a) Sustained WT drops at/above threshold within a CLOSED window must
    /// fire a step-down. Mutation check: if the helper dropped the
    /// `delta >= threshold` comparison (e.g. always returned step_down=false),
    /// this fails.
    #[test]
    fn wt_sustained_drops_above_threshold_fire_step_down() {
        // 4 drops accumulated since the snapshot, window has fully elapsed.
        let decision = evaluate_self_congestion(
            /* current_drops */ 4,
            /* snapshot_drops */ 0,
            /* elapsed_ms */ WT_SELF_CONGESTION_WINDOW_MS,
            WT_SELF_CONGESTION_WINDOW_MS,
            WT_SELF_CONGESTION_DROP_THRESHOLD,
        );
        assert!(
            decision.step_down,
            "delta 4 >= threshold {WT_SELF_CONGESTION_DROP_THRESHOLD} in a closed window must step down"
        );
        assert!(decision.roll_window, "a closed window must roll");
        assert_eq!(
            decision.new_snapshot, 4,
            "rolled snapshot must adopt the current drop count"
        );

        // Exactly-at-threshold must also fire (boundary).
        let at = evaluate_self_congestion(
            WT_SELF_CONGESTION_DROP_THRESHOLD,
            0,
            WT_SELF_CONGESTION_WINDOW_MS,
            WT_SELF_CONGESTION_WINDOW_MS,
            WT_SELF_CONGESTION_DROP_THRESHOLD,
        );
        assert!(at.step_down, "delta == threshold must step down");
    }

    /// (b) Sparse/transient drops BELOW threshold within the window must NOT
    /// fire — a few resets on a lossy/high-latency link cannot shed a layer.
    /// Mutation check: if the threshold were ignored, a below-threshold delta
    /// would wrongly fire and this fails.
    #[test]
    fn wt_sparse_drops_below_threshold_do_not_fire() {
        // 2 drops < threshold (3), window closed.
        let decision = evaluate_self_congestion(
            2,
            0,
            WT_SELF_CONGESTION_WINDOW_MS,
            WT_SELF_CONGESTION_WINDOW_MS,
            WT_SELF_CONGESTION_DROP_THRESHOLD,
        );
        assert!(
            !decision.step_down,
            "delta 2 < threshold {WT_SELF_CONGESTION_DROP_THRESHOLD} must NOT step down"
        );
        // Window still rolls so the next window starts fresh from the new count.
        assert!(decision.roll_window);
        assert_eq!(decision.new_snapshot, 2);

        // A single transient drop also must not fire.
        let one = evaluate_self_congestion(
            1,
            0,
            WT_SELF_CONGESTION_WINDOW_MS,
            WT_SELF_CONGESTION_WINDOW_MS,
            WT_SELF_CONGESTION_DROP_THRESHOLD,
        );
        assert!(!one.step_down, "a single transient drop must not step down");
    }

    /// While the window is still OPEN, drops must accumulate without firing or
    /// rolling — even if the delta already exceeds the threshold — so a burst
    /// at the very start of a window is still measured over the full window
    /// rather than firing on the first tick. Mutation check: if the helper
    /// ignored `elapsed_ms < window_ms` it would fire/roll early and this
    /// fails.
    #[test]
    fn wt_open_window_does_not_fire_or_roll() {
        let decision = evaluate_self_congestion(
            100, // far above threshold
            0,
            WT_SELF_CONGESTION_WINDOW_MS / 2.0, // window only half-elapsed
            WT_SELF_CONGESTION_WINDOW_MS,
            WT_SELF_CONGESTION_DROP_THRESHOLD,
        );
        assert!(
            !decision.step_down,
            "must not fire before the window closes, even above threshold"
        );
        assert!(!decision.roll_window, "must not roll an open window");
        assert_eq!(
            decision.new_snapshot, 0,
            "snapshot must be preserved while the window is open"
        );
    }

    /// (c) A WebSocket user has zero unistream sends, so the WT counter stays
    /// flat at 0 forever: snapshot == current on every closed window, delta is
    /// 0, and the trigger NEVER fires no matter how many windows elapse.
    /// Mutation check: if the helper treated a zero delta as a fire, or read
    /// the absolute count instead of the delta, this fails.
    #[test]
    fn wt_flat_counter_ws_user_never_fires() {
        let mut snapshot: u64 = 0;
        // Simulate many consecutive closed windows with a counter pinned at 0.
        for _ in 0..1000 {
            let decision = evaluate_self_congestion(
                0, // counter never moves for a WS user
                snapshot,
                WT_SELF_CONGESTION_WINDOW_MS,
                WT_SELF_CONGESTION_WINDOW_MS,
                WT_SELF_CONGESTION_DROP_THRESHOLD,
            );
            assert!(
                !decision.step_down,
                "a flat-at-0 counter (WS user) must never trigger a WT step-down"
            );
            assert!(decision.roll_window);
            snapshot = decision.new_snapshot;
        }
        assert_eq!(snapshot, 0, "snapshot must remain pinned at 0");
    }

    /// A monotonic counter that appears to go backwards (must not happen, but
    /// guard anyway) must saturate to a zero delta rather than underflow into
    /// a huge delta that spuriously fires.
    #[test]
    fn wt_backwards_counter_saturates_to_no_fire() {
        let decision = evaluate_self_congestion(
            5,  // current
            10, // snapshot somehow larger
            WT_SELF_CONGESTION_WINDOW_MS,
            WT_SELF_CONGESTION_WINDOW_MS,
            WT_SELF_CONGESTION_DROP_THRESHOLD,
        );
        assert!(
            !decision.step_down,
            "saturating_sub must yield 0, not underflow into a firing delta"
        );
    }

    // NOTE(#1104): the "WT window >= WS window" and "WT threshold > 1"
    // invariants are enforced as COMPILE-TIME `const _: () = assert!(…)`
    // checks next to the constants themselves (a runtime `assert!` on a
    // constant trips clippy's `assertions_on_constants`), matching the
    // convention used for the #1108 backpressure invariants above.

    // =====================================================================
    // Client-side WebTransport uplink-SATURATION self-trigger
    // (#1219 prerequisite)
    //
    // These exercise the SAME pure helper (`evaluate_self_congestion`) but
    // parameterised with the saturation constants, because the saturation
    // consumer reuses that helper to do a tumbling-window delta test over the
    // monotonic `unistream_ready_stall_count` (slow-ready events) instead of
    // the drop counter. Each case maps to a Definition-of-Done requirement and
    // is written to FAIL if the threshold/window logic is inverted or broken
    // (mutation-verified — see the per-test mutation notes).
    // =====================================================================

    /// Sustained slow-ready() events at/above threshold within a CLOSED
    /// saturation window MUST fire a step-down — the WT uplink is saturated but
    /// alive (no stream reset, so the drop counter would be flat) and the
    /// publisher must self-shed.
    ///
    /// Mutation check: if `evaluate_self_congestion` were mutated to
    /// `delta < threshold` (inverted) or to always return `step_down=false`,
    /// the `decision.step_down` assertion fails. If it ignored the threshold and
    /// always fired, the boundary case below would still pass but the
    /// `wt_saturation_below_threshold_does_not_fire` test fails.
    #[test]
    fn wt_saturation_above_threshold_fires_step_down() {
        // 4 slow-ready events accrued since the snapshot, window fully elapsed.
        let decision = evaluate_self_congestion(
            /* current_stalls */ 4,
            /* snapshot_stalls */ 0,
            /* elapsed_ms */ WT_SATURATION_WINDOW_MS,
            WT_SATURATION_WINDOW_MS,
            WT_SATURATION_STALL_THRESHOLD,
        );
        assert!(
            decision.step_down,
            "delta 4 >= threshold {WT_SATURATION_STALL_THRESHOLD} in a closed window must step down"
        );
        assert!(decision.roll_window, "a closed window must roll");
        assert_eq!(decision.new_snapshot, 4);

        // Exactly-at-threshold is the firing boundary and MUST fire. Mutation
        // check: a `delta > threshold` (strict) mutation makes this fail.
        let at = evaluate_self_congestion(
            WT_SATURATION_STALL_THRESHOLD,
            0,
            WT_SATURATION_WINDOW_MS,
            WT_SATURATION_WINDOW_MS,
            WT_SATURATION_STALL_THRESHOLD,
        );
        assert!(
            at.step_down,
            "delta == saturation threshold must step down (boundary)"
        );
    }

    /// Sparse slow-ready() events BELOW threshold within the window MUST NOT
    /// fire — a bursty-but-recovering link produces only a scatter of slow
    /// `ready()`s as a transient burst drains, which must not shed a layer.
    ///
    /// Mutation check: if the threshold comparison were dropped (always fire),
    /// the delta-2 and delta-1 assertions fail.
    #[test]
    fn wt_saturation_below_threshold_does_not_fire() {
        // 2 slow-ready events < threshold (3), window closed.
        let decision = evaluate_self_congestion(
            2,
            0,
            WT_SATURATION_WINDOW_MS,
            WT_SATURATION_WINDOW_MS,
            WT_SATURATION_STALL_THRESHOLD,
        );
        assert!(
            !decision.step_down,
            "delta 2 < threshold {WT_SATURATION_STALL_THRESHOLD} must NOT step down"
        );
        assert!(decision.roll_window);
        assert_eq!(decision.new_snapshot, 2);

        // A single transient slow ready() (one reordered packet) must not fire.
        let one = evaluate_self_congestion(
            1,
            0,
            WT_SATURATION_WINDOW_MS,
            WT_SATURATION_WINDOW_MS,
            WT_SATURATION_STALL_THRESHOLD,
        );
        assert!(
            !one.step_down,
            "a single transient slow ready() must not step down"
        );
    }

    /// While the saturation window is still OPEN, slow-ready() events must
    /// accumulate without firing or rolling — even above threshold — so a burst
    /// at the very start of a window is measured over the full window rather
    /// than firing on the first tick.
    ///
    /// Mutation check: if the helper ignored `elapsed_ms < window_ms` it would
    /// fire/roll early and both assertions fail.
    #[test]
    fn wt_saturation_open_window_does_not_fire_or_roll() {
        let decision = evaluate_self_congestion(
            100, // far above threshold
            0,
            WT_SATURATION_WINDOW_MS / 2.0, // window only half-elapsed
            WT_SATURATION_WINDOW_MS,
            WT_SATURATION_STALL_THRESHOLD,
        );
        assert!(
            !decision.step_down,
            "must not fire before the saturation window closes, even above threshold"
        );
        assert!(!decision.roll_window, "must not roll an open window");
        assert_eq!(decision.new_snapshot, 0);
    }

    /// A WebSocket user (or a WT user on a healthy uplink that never crosses the
    /// producer-side READY_STALL_THRESHOLD_MS) has a flat stall counter: delta
    /// is 0 on every closed window and the trigger NEVER fires.
    ///
    /// Mutation check: if a zero delta were treated as a fire, this fails.
    #[test]
    fn wt_saturation_flat_counter_never_fires() {
        let mut snapshot: u64 = 0;
        for _ in 0..1000 {
            let decision = evaluate_self_congestion(
                0, // counter never moves: WS user or healthy WT uplink
                snapshot,
                WT_SATURATION_WINDOW_MS,
                WT_SATURATION_WINDOW_MS,
                WT_SATURATION_STALL_THRESHOLD,
            );
            assert!(
                !decision.step_down,
                "a flat-at-0 stall counter must never trigger a WT saturation step-down"
            );
            assert!(decision.roll_window);
            snapshot = decision.new_snapshot;
        }
        assert_eq!(snapshot, 0);
    }

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
    fn test_simulcast_video_layers_valid_and_monotonic() {
        // The dedicated camera simulcast ladder (issue #1768) is ordered
        // lowest-first: each rung must have positive dims/fps and be
        // non-decreasing in pixels, fps, and ideal bitrate as layer_id rises.
        assert!(
            !SIMULCAST_VIDEO_LAYERS.is_empty(),
            "SIMULCAST_VIDEO_LAYERS must be non-empty"
        );
        for t in SIMULCAST_VIDEO_LAYERS {
            assert!(
                t.max_width > 0 && t.max_height > 0 && t.target_fps > 0,
                "simulcast rung '{}' must have positive dims/fps",
                t.label
            );
            assert!(
                t.min_bitrate_kbps < t.max_bitrate_kbps
                    && t.ideal_bitrate_kbps >= t.min_bitrate_kbps
                    && t.ideal_bitrate_kbps <= t.max_bitrate_kbps,
                "simulcast rung '{}' bitrate band invalid",
                t.label
            );
        }
        for w in SIMULCAST_VIDEO_LAYERS.windows(2) {
            let lo_px = w[0].max_width as u64 * w[0].max_height as u64;
            let hi_px = w[1].max_width as u64 * w[1].max_height as u64;
            assert!(hi_px >= lo_px, "simulcast rungs must ascend in pixels");
            assert!(
                w[1].target_fps >= w[0].target_fps,
                "simulcast rungs must ascend in fps"
            );
            assert!(
                w[1].ideal_bitrate_kbps >= w[0].ideal_bitrate_kbps,
                "simulcast rungs must ascend in ideal bitrate"
            );
        }
    }

    #[test]
    fn test_simulcast_max_layers_matches_ladder_len() {
        assert_eq!(
            SIMULCAST_MAX_LAYERS,
            SIMULCAST_VIDEO_LAYERS.len(),
            "SIMULCAST_MAX_LAYERS must equal the ladder length"
        );
    }

    #[test]
    fn test_simulcast_video_layers_exact_values() {
        // Issue #1768: pin the retuned camera simulcast ladder through the
        // PRODUCTION resolver (`simulcast_layers`) so reverting the ladder
        // (e.g. back to 640×360@20 / 960×540@30 / 1280×720@30 with
        // 400/900/1500 ideals) FAILS here. Values are (w, h, fps, ideal_kbps).
        let l = simulcast_layers(3);
        let got: Vec<(u32, u32, u32, u32)> = l
            .iter()
            .map(|t| {
                (
                    t.max_width,
                    t.max_height,
                    t.target_fps,
                    t.ideal_bitrate_kbps,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (320, 180, 7, 120),
                (640, 360, 15, 350),
                (1280, 720, 30, 1500),
            ],
            "camera simulcast ladder must be the issue #1768 rungs"
        );
        // Keyframe intervals track ~5s wall-clock at each rung's NEW fps.
        assert_eq!(l[0].keyframe_interval_frames, 35); // 5s × 7fps
        assert_eq!(l[1].keyframe_interval_frames, 75); // 5s × 15fps
        assert_eq!(l[2].keyframe_interval_frames, 150); // 5s × 30fps
    }

    #[test]
    fn test_audio_tiers_exact_bitrates() {
        // Issue #1768: pin the retuned audio ladder (high 48 / medium 24 /
        // low 12 kbps named levels + emergency 8 kbps rescue) so reverting to
        // 50/32/24/16 FAILS here. Read straight from the production table.
        let got: Vec<(&str, u32)> = AUDIO_QUALITY_TIERS
            .iter()
            .map(|t| (t.label, t.bitrate_kbps))
            .collect();
        assert_eq!(
            got,
            vec![("high", 48), ("medium", 24), ("low", 12), ("emergency", 8),]
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
        // Camera ladder [low, standard, hd] = ideals 120 / 350 / 1500 (#1768).
        let tiers = simulcast_layers(3);
        assert_eq!(uplink_budget_kbps(tiers, 1), 120.0);
        assert_eq!(uplink_budget_kbps(tiers, 2), 470.0);
        assert_eq!(uplink_budget_kbps(tiers, 3), 1970.0);
        // active is clamped to the ladder length (cannot over-count).
        assert_eq!(uplink_budget_kbps(tiers, 99), 1970.0);
        // Zero active layers → zero budget.
        assert_eq!(uplink_budget_kbps(tiers, 0), 0.0);
    }

    #[test]
    fn test_cap_noop_when_within_budget() {
        // Targets that already fit must be returned unchanged (the common case
        // at low tiers and the byte-identical guarantee for N=1).
        let tiers = simulcast_layers(3);
        let budget = uplink_budget_kbps(tiers, 3); // 1970
        let mut targets = [100.0, 300.0, 1200.0]; // sum 1600 <= 1970
        let before = targets;
        cap_layers_to_budget(&mut targets, tiers, 3, budget);
        assert_eq!(targets, before, "within-budget targets must not change");
    }

    #[test]
    fn test_cap_scales_down_to_budget_and_respects_floors() {
        // Targets that exceed the budget must be scaled so the active sum fits,
        // and no layer may drop below its tier floor (60 / 150 / 800) (#1768).
        let tiers = simulcast_layers(3);
        let floors: Vec<f64> = tiers.iter().map(|t| t.min_bitrate_kbps as f64).collect();
        let budget = uplink_budget_kbps(tiers, 3); // 1970
                                                   // All layers asking for their tier max: 200 + 600 + 2000 = 2800 > 1970.
        let mut targets = [200.0, 600.0, 2000.0];
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
        // cap's). Floors sum = 60+150+800 = 1010 (#1768); pass a budget below that.
        let tiers = simulcast_layers(3);
        let mut targets = [200.0, 600.0, 2000.0];
        cap_layers_to_budget(&mut targets, tiers, 3, 900.0);
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
        let budget = uplink_budget_kbps(tiers, 1); // 120 (= low ideal, #1768)
        let mut targets = [600.0, 9999.0, 8888.0];
        cap_layers_to_budget(&mut targets, tiers, 1, budget);
        // Active layer 0 capped to its floor-respecting share of 120 (floor 60).
        assert!(targets[0] <= budget + 1e-6 && targets[0] >= 60.0 - 1e-6);
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

    // NOTE(#1108): the backpressure-hysteresis and step-up-slower-than-step-down
    // invariants are now COMPILE-TIME `const _: () = assert!(…)` checks next to
    // the constants themselves (a runtime `assert!` on a constant trips clippy's
    // `assertions_on_constants`). See the "Compile-time invariants" block above.

    // NOTE: the PID / climb-rate-limiter / congestion constant-relationship
    // invariants below were runtime `assert!`s in `#[test]` fns; they are now
    // COMPILE-TIME `const _: () = assert!(…)` checks at module scope (see
    // "Constant-relationship invariants" near the end of this file). A runtime
    // `assert!` on a constant trips clippy's `assertions_on_constants` and only
    // fires if the test is run; the const form is checked on every build.

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

    // --- issue #619: Opus FEC + packet-loss-% tier wiring -------------------

    #[test]
    fn test_audio_tier_packet_loss_perc_in_range() {
        // OPUS_SET_PACKET_LOSS_PERC accepts 0-100; an out-of-range value would
        // be silently clamped/rejected by libopus, so pin it here.
        for tier in AUDIO_QUALITY_TIERS {
            assert!(
                tier.packet_loss_perc <= 100,
                "audio tier '{}': packet_loss_perc {} must be 0-100",
                tier.label,
                tier.packet_loss_perc,
            );
        }
    }

    #[test]
    fn test_audio_tier_loss_perc_implies_fec() {
        // A non-zero packet-loss hint only does anything when inband FEC is on
        // (libopus uses it to scale FEC redundancy). If a tier ever sets a loss
        // hint without enabling FEC, that's wasted intent — fail loudly.
        for tier in AUDIO_QUALITY_TIERS {
            if tier.packet_loss_perc > 0 {
                assert!(
                    tier.enable_fec,
                    "audio tier '{}' has packet_loss_perc {} but FEC is off; \
                     the loss hint only matters with FEC enabled",
                    tier.label, tier.packet_loss_perc,
                );
            }
        }
    }

    #[test]
    fn test_audio_top_tier_is_healthy() {
        // The top (index 0) tier represents a healthy link: no FEC overhead and
        // a 0% loss hint. This is also the tier the mic encoder inits at, so it
        // defines default-state audio. Pin it so a future edit can't silently
        // turn on FEC overhead for everyone at init.
        let top = &AUDIO_QUALITY_TIERS[0];
        assert!(!top.enable_fec, "top audio tier must keep FEC off");
        assert_eq!(
            top.packet_loss_perc, 0,
            "top audio tier must have a 0% loss hint"
        );
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

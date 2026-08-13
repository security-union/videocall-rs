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

/// REDUCED-top camera simulcast ladder, selected by [`LadderVariant::Reduced`]
/// (issue #1768).
///
/// Identical to [`SIMULCAST_VIDEO_LAYERS`] except the top rung is **540p instead
/// of 720p**. Same length (so the layer-id space and every count clamp are
/// unchanged) and same lowest-first ordering.
///
/// # Why the TOP rung and not the floor
///
/// At each rung's own target fps the encode cost is wildly lopsided —
/// `low` 320×180@7 = 0.40 Mpx/s (1.3% of a 3-layer encode), `standard`
/// 640×360@15 = 3.46 (11.0%), `hd` 1280×720@30 = **27.65 (87.8%)**. Lowering the
/// floor saves essentially nothing; lowering the top is the entire win. Swapping
/// 720p→540p takes the top rung to 15.55 Mpx/s, cutting the 3-layer total from
/// ~31.5 to ~19.4 Mpx/s (≈38%).
///
/// # Why 540p specifically
///
/// It matches Google Meet's 540p rung (their documented ladder is
/// 180p/360p/540p/720p, and their documented typical group-meeting inbound of
/// ~1.3 Mbps shows their receivers sit low in the grid, which is why our video
/// looks sharper in the same tile). It also keeps us **above** the 180p floor
/// where a full-screen or 2-person view would go soft — going below 180p was
/// explicitly rejected. And 960×540 is already validated geometry in this
/// codebase: it is the `standard` rung of [`VIDEO_QUALITY_TIERS`], whose
/// 900/500/1500 kbps band is reused here rather than inventing values.
///
/// # The `n == 2` case
///
/// [`spaced_ladder_positions`] anchors base+top and skips the interior, so a
/// 2-layer publisher (any device sniffing 6–9 cores) gets `[low, top]` — `[180p,
/// 720p]` by default, `[180p, 540p]` here. The gap between the two rungs narrows on
/// both measures: **pixel area 16× → 9×**, and **throughput 68.6× → 38.6× Mpx/s**
/// (the throughput figure folds in the 7 vs 30 fps difference; it is the "69×"
/// number quoted in the #2143 analysis, and it is NOT the pixel ratio — the two are
/// easily conflated).
///
/// **This does NOT give the #1256 tile-size lid a new target**, and an earlier
/// revision of this doc claiming it did was wrong. `size_cap_layer` returns the
/// FIRST rung whose height covers the tile and otherwise falls through to
/// `highest_available`, so reaching the top index means every lower rung already
/// failed — the top rung's height never enters the decision. A 2-layer publisher's
/// receiver therefore selects the SAME index under either ladder;
/// `size_cap_layer_is_insensitive_to_the_reduced_ladder_top_rung` (videocall-client)
/// proves that exhaustively. The benefit of the narrower gap is that a receiver
/// forced to the top rung now pays 540p instead of 720p — cheaper DECODE and less
/// downlink for the SAME selection, not a better selection.
///
/// # The receiver-side saving, quantified
///
/// This is the headline result for the low-power devices this project targets (no
/// hardware VP8/VP9 decode, where software decode is the binding constraint), so it
/// is stated in numbers rather than as "cheaper": 960×540 is **0.5625×** the pixels
/// of 1280×720, so a receiver on the top rung does **~43.8% less** decode work per
/// frame — **15.55 vs 27.65 Mpx/s** at the rung's 30 fps. Downlink for that rung
/// falls with it (ideal 900 vs 1500 kbps, −40%). Pinned by
/// `reduced_ladder_cuts_top_rung_decode_cost` so the receiver claim is tied to the
/// tables exactly as the publisher-side encode claim is.
pub const SIMULCAST_VIDEO_LAYERS_REDUCED: &[VideoQualityTier] = &[
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
        // Named distinctly from the default ladder's `hd` so a log/diagnostic
        // line naming the rung is unambiguous about which ladder produced it.
        label: "hd540",
        max_width: 960,
        max_height: 540,
        target_fps: 30,
        // Band reused from the `standard` rung of VIDEO_QUALITY_TIERS (960×540@30),
        // which is already the tuned 540p operating point in this codebase.
        ideal_bitrate_kbps: 900,
        min_bitrate_kbps: 500,
        max_bitrate_kbps: 1500,
        keyframe_interval_frames: 150, // ~5s at 30fps; wall-clock cap guarantees ≤5s
    },
];

/// Compile-time guard: both camera ladders must be the SAME DEPTH.
///
/// The wire `simulcast_layer_id` space, `spaced_ladder_positions` selection, and
/// every layer-count clamp (`SIMULCAST_MAX_LAYERS`, the client's
/// `clamp_layer_count`, the relay's id bucketing) are shared across variants. A
/// variant of a different length would silently change the meaning of a layer id
/// between publishers, so this is asserted at build time rather than trusted.
const _: () = assert!(
    SIMULCAST_VIDEO_LAYERS_REDUCED.len() == SIMULCAST_VIDEO_LAYERS.len(),
    "the reduced camera ladder must have the same rung count as the default ladder"
);

/// Compile-time guard: the reduced ladder's top rung must actually be SMALLER
/// than the default's, or the variant is pointless and a future retune that
/// inverts them would silently make `Reduced` the heavier option.
const _: () = assert!(
    SIMULCAST_VIDEO_LAYERS_REDUCED[SIMULCAST_VIDEO_LAYERS_REDUCED.len() - 1].max_height
        < SIMULCAST_VIDEO_LAYERS[SIMULCAST_VIDEO_LAYERS.len() - 1].max_height,
    "the reduced ladder's top rung must be lower-resolution than the default top rung"
);

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

/// How long a simulcast rung stays "arriving" after its last packet, for receivers
/// deciding which rungs a source is currently offering.
///
/// Lives here because BOTH receivers must agree or they select different rungs from
/// the same source: `videocall-client`'s `LayerAvailability::DEFAULT_WINDOW_MS` and the
/// load-test bot's `RUNG_WINDOW` both read this. Neither crate depends on the other, so
/// this shared crate is the only place the equality can be compile-enforced rather than
/// left to a comment (#2206).
pub const LAYER_AVAILABILITY_WINDOW_MS: u64 = 4000;

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
    simulcast_layers_for(n, LadderVariant::Default)
}

/// Which camera simulcast ladder a publisher is encoding against (issue #1768).
///
/// The rung RESOLUTIONS differ; the rung COUNT does not — both variants are
/// [`SIMULCAST_MAX_LAYERS`] deep, so `spaced_ladder_positions` selection, the
/// wire `simulcast_layer_id` space, and every layer-count clamp are unchanged.
/// Only the pixels/bitrate behind a given index move.
///
/// # Why a runtime variant rather than an edit or a cargo feature
///
/// Re-cutting the ladder needs a wasm rebuild + full deploy to evaluate, which
/// makes each experiment a multi-day cycle. A runtime variant, delivered through
/// the same `runtimeConfig` → `config.js` → `window.__APP_CONFIG` path as
/// `experimentalSimulcastMaxLayers`, lets an operator flip the ladder with a
/// `helm upgrade`. A cargo feature would be compile-time and defeat that.
///
/// # This is a DEPLOYMENT-WIDE switch, not a per-user A/B
///
/// The wire carries only a layer INDEX and no geometry, so a receiver resolves a
/// rung's dimensions/bitrate from its OWN copy of the ladder. Which copy depends on
/// what the answer is FOR (the #2156 split): `layer_chooser::received_layer_snapshot`
/// — the SELECTION resolver — stays pinned to [`LadderVariant::Default`], while
/// `received_layer_snapshot_for_display` takes the deployment's variant. Two
/// consequences, and they differ in severity:
///
/// * **DECODE and layer SELECTION are unaffected.** The decoder sizes from the
///   decoded frame itself (`peer_decoder.rs`, `video_frame.display_width()`), never
///   from this table. And the #1256 tile-size lid is *provably* insensitive to a
///   TOP-rung change: `size_cap_layer` returns the first rung covering the tile and
///   otherwise falls through to `highest_available`. Reaching the top means every
///   lower rung already failed, so the result is the top index whether that rung
///   covers or the loop falls through. Pinned
///   exhaustively by `size_cap_layer_is_insensitive_to_the_reduced_ladder_top_rung`
///   (videocall-client), which turns red if a future retune moves a non-top rung
///   and the lid therefore DOES need the variant plumbed.
/// * **Receiver-side DISPLAY follows the variant (fixed in issue #2156).** The
///   receiver's READOUTS — the performance panel's `{w}x{h}` line, the peer-row
///   `540p · ~900k` metric, the receive slider's rung labels and their
///   `aria-valuetext`, the diagnostics drawer's per-kind line, and the signal-quality
///   popup — resolve rung geometry through
///   `layer_chooser::received_layer_snapshot_for_display`, which is handed the
///   deployment's variant via `VideoCallClientOptions::camera_ladder_variant` →
///   `PeerDecodeManager::set_camera_ladder_variant`. Before #2156 they were pinned to
///   the shipped ladder, so a `Reduced` deployment labelled a 960×540 @ ~900 kbps
///   stream "720p · ~1.5M" — wrong by 67% on the bitrate operators judge a run by.
///   Receiver rung labels are therefore ground truth when every publisher uses the
///   deployment's variant. In a mixed room containing native `bot/` publishers,
///   which remain [`LadderVariant::Default`]-pinned, use each publisher's
///   `AQ_STATUS ladder=` field to interpret its rung. For browser publishers,
///   `videocall_encoder_active_layers` remains authoritative for SEND-side layer
///   counts; native bots expose that count through `AQ_STATUS active_layers=`.
///
///   This changed **no metric**: `health_reporter`'s `received_*_layer` maps carry
///   layer INDICES only, with no geometry or bitrate, so Prometheus was never wrong.
///
/// So a mixed-variant room is not a correctness break; keep the variant uniform
/// across a deployment anyway so rung labels and selection stay interpretable.
///
/// # The publisher-side halves MUST agree
///
/// The encoder derives per-layer GEOMETRY from these rungs; the AQ controller
/// derives the per-layer BITRATE TARGETS from them (and hands those to the encoder
/// via `shared_layer_bitrates_bps` → `set_bitrate`). Gating only one half would
/// therefore encode one ladder's resolutions at the OTHER ladder's bitrates — e.g.
/// 960×540 configured for 1500 kbps — so the variant is threaded to both from a
/// single read of the flag.
///
/// It would **not** cause a spurious layer shed. [`uplink_budget_kbps`] sums the
/// same tiers whose ideals become the targets, so `sum == budget` by construction
/// and [`cap_layers_to_budget`] no-ops under either variant; shed decisions read
/// encoder-queue backpressure, the union/user caps and tier movement, none of which
/// consults this table. (An earlier revision of this doc claimed a "phantom
/// ceiling" shed — that mechanism does not exist in the code.)
///
/// # The native `bot` crate cannot see this
///
/// `bot/` links this crate but has no `window.__APP_CONFIG`, so it always
/// encodes against [`LadderVariant::Default`]. A Rust bot sharing a room with
/// gated-on browsers publishes a different top rung — harmless per the decode
/// note above, but it makes a measurement run non-uniform. The browser bots-app
/// (`e2e/bots-app`) is unaffected: it drives the real client, so it inherits
/// whatever the deployment serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LadderVariant {
    /// The shipped ladder: `[low 320×180@7, standard 640×360@15, hd 1280×720@30]`.
    #[default]
    Default,
    /// Reduced-top ladder: `[low 320×180@7, standard 640×360@15, 540p 960×540@30]`.
    ///
    /// Drops the 720p top to 540p. Rationale (issue #1768 + the #2143 run):
    /// the top rung is ~87.8% of the whole 3-layer encode cost (27.65 of 31.5
    /// Mpx/s), so lowering it — not touching the near-free 180p floor — is where
    /// the CPU is. It also narrows the `n == 2` gap on BOTH measures:
    /// **pixel area 16× → 9×** and **throughput 68.6× → 38.6× Mpx/s**
    /// (`[180p, 720p]` → `[180p, 540p]`), so a 6–9-core publisher's receivers pay
    /// 540p rather than 720p when they land on the top rung. It does NOT change
    /// WHICH rung the #1256 lid selects — see the `n == 2` section on
    /// [`SIMULCAST_VIDEO_LAYERS_REDUCED`].
    ///
    /// The "**69×**" figure quoted in the #2143 analysis is the THROUGHPUT ratio
    /// (it folds in 30 vs 7 fps), **not** pixel area — the two are easily
    /// conflated, and both are pinned by
    /// `reduced_ladder_n2_keeps_the_middle_skip_but_shrinks_the_cliff`.
    Reduced,
}

/// Resolve the simulcast layer tiers for an `n`-layer ladder in a specific
/// [`LadderVariant`] (issue #1768).
///
/// [`simulcast_layers`] is this function pinned to [`LadderVariant::Default`];
/// see that function for the `n`-selection contract, which is identical here
/// (same [`spaced_ladder_positions`] rule, same clamping, same never-panics
/// guarantee).
pub fn simulcast_layers_for(n: usize, variant: LadderVariant) -> &'static [VideoQualityTier] {
    // Static, build-once tables so the function can return `&'static`. We build
    // one cached `Vec<VideoQualityTier>` per (variant, ladder size) lazily via
    // `OnceLock`.
    use std::sync::OnceLock;

    fn ladder(n: usize, source: &[VideoQualityTier]) -> Vec<VideoQualityTier> {
        // Derive the lowest-`n` well-spaced rungs generically from the ladder
        // definition (issue #1082): no per-`n` `match` arm, so raising
        // SIMULCAST_MAX_LAYERS requires no change here. The ladder is already
        // lowest-first, so a selected position IS the layer id (issue #1768).
        spaced_ladder_positions(n, source.len())
            .into_iter()
            .map(|pos| {
                let t = &source[pos];
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

    // One OnceLock cache cell per supported ladder size, PER VARIANT. Indexed by
    // clamped n. Separate arrays (not one keyed array) so a cache cell can never
    // be filled by the wrong variant.
    static LADDERS_DEFAULT: [OnceLock<Vec<VideoQualityTier>>; SIMULCAST_MAX_LAYERS] =
        [const { OnceLock::new() }; SIMULCAST_MAX_LAYERS];
    static LADDERS_REDUCED: [OnceLock<Vec<VideoQualityTier>>; SIMULCAST_MAX_LAYERS] =
        [const { OnceLock::new() }; SIMULCAST_MAX_LAYERS];

    let clamped = n.clamp(1, SIMULCAST_MAX_LAYERS);
    let (cache, source) = match variant {
        LadderVariant::Default => (&LADDERS_DEFAULT, SIMULCAST_VIDEO_LAYERS),
        LadderVariant::Reduced => (&LADDERS_REDUCED, SIMULCAST_VIDEO_LAYERS_REDUCED),
    };
    cache[clamped - 1]
        .get_or_init(|| ladder(clamped, source))
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

/// Compile-time `&str` equality, so the label↔index invariants below can be
/// asserted at build time instead of only in a `#[test]`.
///
/// `str::eq` is not `const`, so this compares the UTF-8 bytes directly.
const fn const_str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

// ---------------------------------------------------------------------------
// Screen tier LABELS (the stable contract; indices shift when rungs are added)
// ---------------------------------------------------------------------------
//
// Issue #2179 extended `SCREEN_QUALITY_TIERS` upward with a 1440p and a native
// 2160p rung, which SHIFTED every pre-existing index by two. Everything that
// used to hard-code `0 = high / 1 = medium / 2 = low` therefore resolves its
// rung by LABEL through the helpers below, so a future rung insertion can never
// silently repoint a decision at the wrong resolution/bitrate.

/// Label of the screen rung used as the "bandwidth-conservative baseline"
/// (720p / 8 fps / 1200 kbps).
pub const SCREEN_TIER_LABEL_BASELINE: &str = "medium";

/// Label of the WORST screen rung (720p / 5 fps / 500 kbps) — the floor the AQ
/// ladder and the simulcast base rung both use.
pub const SCREEN_TIER_LABEL_FLOOR: &str = "low";

/// Label of the 1080p screen rung.
pub const SCREEN_TIER_LABEL_1080P: &str = "high";

/// Label of the 1440p screen rung (issue #2179).
pub const SCREEN_TIER_LABEL_1440P: &str = "1440p";

/// Resolve a `SCREEN_QUALITY_TIERS` index by tier label.
///
/// Falls back to the WORST (last) rung when the label is absent, which is the
/// conservative direction: a mis-resolved rung must never spend MORE bandwidth
/// than the ladder's floor.
pub fn screen_tier_index_by_label(label: &str) -> usize {
    SCREEN_QUALITY_TIERS
        .iter()
        .position(|t| t.label == label)
        .unwrap_or_else(|| SCREEN_QUALITY_TIERS.len().saturating_sub(1))
}

/// Index into `SCREEN_QUALITY_TIERS` for the default starting tier.
///
/// Points at the [`SCREEN_TIER_LABEL_BASELINE`] rung ("medium", 720p/8fps/
/// 1200kbps) — a bandwidth-conservative baseline that is used ONLY as a
/// fallback: it is what the AQ manager/controller construct at, and what the
/// screen encoder seeds its shared atomics with before any capture exists.
///
/// The value a real share actually starts at is resolved at RUNTIME from the
/// captured source resolution by [`resolve_initial_screen_tier`] (issue #2179),
/// because a compile-time constant cannot express "match whatever the user
/// chose to share". This constant remains the answer when the source dimensions
/// are unknown (a track that has not reported `getSettings()` yet).
///
/// Pinned to the label (not a hand-counted index) by the compile-time assert
/// below, so adding a rung to the ladder cannot silently repoint the default.
pub const DEFAULT_SCREEN_TIER_INDEX: usize = 3; // "medium"

const _: () = assert!(
    DEFAULT_SCREEN_TIER_INDEX < SCREEN_QUALITY_TIERS.len(),
    "DEFAULT_SCREEN_TIER_INDEX out of bounds for SCREEN_QUALITY_TIERS"
);
const _: () = assert!(
    const_str_eq(
        SCREEN_QUALITY_TIERS[DEFAULT_SCREEN_TIER_INDEX].label,
        SCREEN_TIER_LABEL_BASELINE
    ),
    "DEFAULT_SCREEN_TIER_INDEX must point at the 'medium' rung — a rung was \
     added/removed without updating the index"
);

// ---------------------------------------------------------------------------
// Screen Share Quality Tiers
// ---------------------------------------------------------------------------

/// Screen share quality tiers, ordered from highest (index 0) to lowest.
///
/// Screen content (text, code, diagrams) needs significantly higher bitrates
/// than camera video to remain readable during scrolling and motion. The
/// encoder is configured with `contentHint = 'detail'` and variable bitrate
/// mode to accommodate burst demand during scroll events.
///
/// # The two top rungs (issue #2179)
/// The ladder used to top out at 1080p, which silently destroyed text on any
/// HiDPI display: a browser window measuring 1248x720 CSS px on a DPR-2 panel
/// is 2496x1440 REAL pixels, and `fit_within_preserving_aspect` downscaled that
/// into the 1920x1080 rung and then (once AQ stepped to "medium") into
/// 1280x720 — a quarter of the source pixels through two fractional resamples,
/// which is exactly what shreds glyph stems and antialiasing.
///
/// `1440p` (2560x1440) holds a DPR-2 Retina laptop window 1:1, and `native`
/// (3840x2160) holds a 4K panel and the 21:9 ultra-wides (3840x1600) 1:1.
/// Because `fit_within_preserving_aspect` NEVER upscales, a small source on a
/// big rung is still encoded at its own size — the rungs only remove a ceiling,
/// they never inflate a stream.
///
/// # Bitrates
/// Tuned for text at ~10 fps under VBR with `contentHint = 'detail'`: static
/// text is highly temporally redundant so the `ideal` is a modest steady-state
/// figure, while `max` is generous so a scroll burst is not smeared. These are
/// SETPOINTS for the PID, not a reservation — a static share converges far
/// below `ideal`.
///
/// # Cost control
/// Reaching the top rungs is gated, not automatic:
/// - a PERSISTENT ceiling ([`resolve_screen_tier_ceiling`]) is installed on the
///   AQ controller for the life of every share, composing the captured source
///   size, the sender's CPU class and the stream count. Because it is a
///   persistent bound rather than a start value, the PID cannot climb a 720p
///   window up to the `native` rung's 8000 kbps setpoint after the fact — which
///   is exactly what the start-only gate allowed before the #2179 review;
/// - the single-stream path only reaches them when the CAPTURED SOURCE is
///   actually that large ([`resolve_initial_screen_tier`]) and the network
///   signals do not veto it, and never past `1440p` at all
///   ([`screen_tier_single_stream_floor`]) because its receivers have no
///   lower rung to fall back to;
/// - the simulcast path keeps its 2-rung cold-start seed at
///   `[low, high]` (unchanged from #1553, ≈3000 kbps), and the 1440p rung is
///   only ever the THIRD rung, earned by the existing 6 s clear-queue headroom
///   probe (see [`simulcast_screen_layers`]).
pub const SCREEN_QUALITY_TIERS: &[VideoQualityTier] = &[
    VideoQualityTier {
        label: "native",
        max_width: 3840,
        max_height: 2160,
        target_fps: 10,
        ideal_bitrate_kbps: 8000,
        min_bitrate_kbps: 4000,
        max_bitrate_kbps: 14000,
        keyframe_interval_frames: 30, // ~3s at 10fps (text readability); wall-clock cap ≤3s
    },
    VideoQualityTier {
        label: "1440p",
        max_width: 2560,
        max_height: 1440,
        target_fps: 10,
        ideal_bitrate_kbps: 5000,
        min_bitrate_kbps: 2500,
        max_bitrate_kbps: 8000,
        keyframe_interval_frames: 30, // ~3s at 10fps (text readability); wall-clock cap ≤3s
    },
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
/// start (active == n == 3, every rung encoding the instant a share begins)
/// because that slam was too aggressive. Seeding at `2` is the middle ground
/// that honors BOTH issues:
/// - **#1553**: publishing the sharp `high` (1080p) rung immediately is exactly
///   what de-fuzzes the shared content for a healthy receiver — the whole point
///   of the issue — instead of stalling at the base rung waiting on the 6 s ramp.
/// - **#1200**: 2 is strictly fewer than the 3-rung ladder, so it does NOT
///   reintroduce the all-rungs-hot slam. What the seed leaves OFF is the THIRD
///   simultaneous encode — the TOP rung, which since issue #2179 is `1440p`
///   (2560x1440 / 5000 kbps) rather than the old middle `medium` rung. The
///   honest comparison is "2 encodes (incl. the 1080p `high`) / ≈ 3000 kbps" at
///   the seed vs "3 encodes / ≈ 8000 kbps" for the full ladder; the deferred top
///   rung is earned by the ramp (or restored after a shed).
///
/// Issue #2179 note: because the ladder is now a strict prefix chain
/// (`[low, high]` ⊂ `[low, high, 1440p]`), this seed publishes EXACTLY the same
/// two rungs at exactly the same bitrates as before the ladder was extended —
/// the cold-start cost of a share is unchanged.
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

/// The SCREEN simulcast rung LABELS for an `n`-layer ladder, lowest layer first
/// (index == `layer_id`). Split out from [`simulcast_screen_layers`] so the
/// selection is pinned by label — the rung a layer publishes must not move when
/// a tier is inserted into [`SCREEN_QUALITY_TIERS`] (issue #2179 added two).
///
/// # The n == 3 RECEIVE points moved (issue #2179 — deliberate)
/// The 3-rung ladder used to publish `[low, medium, high]` = **500 / 1200 /
/// 2500 kbps**; it now publishes `[low, high, 1440p]` = **500 / 2500 /
/// 5000 kbps**. The consequence for RECEIVERS is explicit and accepted:
///
/// - a receiver on ~0.5–2.5 Mbps that used to land on the 1200 kbps `medium`
///   rung now falls back to the 500 kbps `low` base rung — a real downgrade for
///   that band;
/// - a receiver with headroom gains a 1440p rung that did not exist before,
///   which is the sharpness issue #2179 was filed for.
///
/// This is the tradeoff the issue asks for: restoring `medium` would re-cap
/// every simulcast receiver at 1080p no matter how sharp the source is. The
/// cost is bounded on the SEND side — the third rung is never seeded, only
/// earned by the headroom probe, and it is shed first under backpressure.
///
/// # Panics
/// Panics if `n` is not in `{1, 2, 3}`; callers must clamp first.
pub fn simulcast_screen_layer_labels(n: usize) -> &'static [&'static str] {
    match n {
        1 => &[SCREEN_TIER_LABEL_FLOOR],
        2 => &[SCREEN_TIER_LABEL_FLOOR, SCREEN_TIER_LABEL_1080P],
        3 => &[
            SCREEN_TIER_LABEL_FLOOR,
            SCREEN_TIER_LABEL_1080P,
            SCREEN_TIER_LABEL_1440P,
        ],
        other => panic!("simulcast_screen_layers: n must be in {{1,2,3}}, got {other}"),
    }
}

/// Resolve the SCREEN simulcast layer tiers for an `n`-layer ladder
/// (issue #989, Phase 3), **lowest layer first** (index == `layer_id`).
///
/// Derived from [`SCREEN_QUALITY_TIERS`] by LABEL (see
/// [`simulcast_screen_layer_labels`]):
/// - `n == 1` → `[low]` (single base; screen single-stream path is unchanged
///   and does not consult this).
/// - `n == 2` → `[low, high]` (720p/500 base + 1080p/2500 top — well separated).
///   UNCHANGED by issue #2179: this is the ladder the `SCREEN_INITIAL_ACTIVE_LAYERS`
///   cold-start seed publishes, so a fresh share still costs ≈3000 kbps across
///   two encodes exactly as it did before the ladder was extended.
/// - `n == 3` → `[low, high, 1440p]` (full ladder).
///
/// # Why the third rung moved (issue #2179)
/// It used to be `[low, medium, high]` — `low` and `medium` are BOTH 1280x720
/// (they differ only in fps/bitrate), so the ladder spent its middle rung on a
/// resolution the base rung already carried and topped out at 1080p. That
/// capped simulcast receivers at 1080p no matter how sharp the source was,
/// which is the defect issue #2179 reports. The ladder is now spaced by
/// RESOLUTION — 720p → 1080p → 1440p — so the top rung can carry a DPR-2
/// Retina window at its native size.
///
/// Two properties make this affordable rather than a bandwidth slam:
/// - **Prefix stability.** `n == 3` is now `n == 2` plus one rung on top, so
///   earning the third rung ADDS 1440p instead of also re-pointing layer 1 from
///   `high` to `medium` (which is what the old `[low, medium, high]` did, and
///   which forced a mid-share reconfigure of an already-published rung).
/// - **Earned, not seeded.** The cold-start seed is `SCREEN_INITIAL_ACTIVE_LAYERS`
///   (= 2) active rungs, so the 1440p rung is published ONLY after the
///   headroom probe sees the encoder queue uninterruptedly clear for
///   `LAYER_PROBE_CLEAR_WINDOW_MS`, and it is shed first under backpressure by
///   the existing top-down `drop_top_layer` machinery.
///
/// # Panics
/// Panics if `n` is not in `{1, 2, 3}`; callers must clamp first.
pub fn simulcast_screen_layers(n: usize) -> &'static [VideoQualityTier] {
    use std::sync::OnceLock;

    fn ladder(n: usize) -> Vec<VideoQualityTier> {
        simulcast_screen_layer_labels(n)
            .iter()
            .map(|&label| {
                let t = &SCREEN_QUALITY_TIERS[screen_tier_index_by_label(label)];
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

/// Minimum RELATIVE benefit the rung being added must carry, as a fraction of
/// the egress already flowing, before a probe-up is allowed (issue #1141).
///
/// # This is NOT an uplink-headroom gate, and it cannot be made into one
/// The predicate it feeds is
/// `budget_next - budget_now > budget_now * FRAC` (see
/// [`crate::controller::EncoderBitrateController`]'s `uplink_precondition_for_add`),
/// where both budgets are sums of LADDER IDEALS. `budget_next - budget_now` is
/// simply the next rung's own ideal, so the predicate reduces to
/// "next rung's ideal > FRAC × sum of active ideals" — a MINIMUM-BENEFIT test
/// whose direction is the opposite of headroom: a MORE expensive rung passes
/// MORE easily. No measured capacity enters it, because this controller has no
/// bandwidth estimate at all ("Nominal-budget baseline … No PID, no bandwidth
/// estimate", `tick`).
///
/// The shipped ratios `next_ideal / budget_now` are:
///
/// | ladder                     | 1→2  | 2→3  |
/// |----------------------------|------|------|
/// | camera default `120/350/1500` | 2.92 | 3.19 |
/// | camera reduced `120/350/900`  | 2.92 | 1.91 |
/// | screen `500/2500/5000`        | 5.00 | 1.67 |
///
/// So every `FRAC < 1.67` is provably inert, and the FIRST value that rejects
/// anything (1.67) rejects exactly the screen ladder's third rung — the 1440p
/// rung issue #2179 exists to earn — permanently, on every device. Raising this
/// number is therefore either a no-op or a silent removal of the 1440p rung; it
/// is deliberately left at `0.0` rather than dressed up as a tuned safety gate.
///
/// **What actually guards the +5000 kbps marginal rung** is the tier-QUIET
/// precondition added alongside this note: the probe refuses to add a rung
/// within [`LAYER_PROBE_CLEAR_WINDOW_MS`] of a video step-DOWN. A step-down is
/// stamped by sustained encoder backpressure AND by the out-of-band
/// `force_video_step_down` that the WS send-buffer, WS stale-delta and WT
/// unistream drop axes call — the three axes the probe's other gates are blind
/// to, because none of them touches `encode_queue_size()`. (The fifth axis,
/// a self-targeted server CONGESTION cut, deliberately does not stamp it — see
/// `AdaptiveQualityManager::force_congestion_cut` — and is instead covered by
/// the probe's existing congestion-hold gate.) See
/// `EncoderBitrateController::probe_add_allowed`.
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

/// Idle timeout before a nonzero camera encoder output FPS is decayed to zero.
///
/// The producer floor is one chunk per second (`fps = chunks_in_last_second`,
/// which is always at least 1 while output is alive), so a live 1 fps stream can
/// have roughly 1000ms plus scheduling jitter between chunks. 2000ms is a safe
/// ~2x margin over that ~1000ms floor gap (plus jitter), chosen so a live 1 fps
/// stream never false-decays.
pub const ENCODER_FPS_IDLE_DECAY_MS: f64 = 2000.0;

/// Idle timeout before a nonzero screen encoder output FPS is decayed to zero.
///
/// This deliberately differs from [`ENCODER_FPS_IDLE_DECAY_MS`]: a fully static
/// screen track can emit no captured frames, while the retained-frame recovery
/// path follows the longer ~3s screen GOP cadence. 5000ms avoids false-decaying
/// a static but healthy screen share across those ~3s layer-0 keyframe chunks.
/// Note: the static-keyframe floor is itself budget-bounded
/// (`SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET`), so a share that stays fully static
/// past ~12s stops emitting layer-0 chunks entirely and WILL then decay to 0 —
/// which is honest (no new content is being produced).
///
/// **No longer cosmetic (issue #2147).** An earlier revision of this doc said
/// "screen fps is log-only, so this is cosmetic either way." That is now FALSE:
/// the same atom this decays is exported as `screen_encoder_output_fps`
/// (health-packet field 109) → the `videocall_screen_encoder_output_fps` gauge,
/// which is deliberately NOT `> 0`-gated. So this constant is what decides how
/// long a quiesced share keeps reporting its last nonzero fps before the gauge
/// reads 0 — retune it only with that consumer in mind. A 0 from this decay is
/// honest, not a fault; read it against `screen_sharing_active`.
pub const SCREEN_ENCODER_FPS_IDLE_DECAY_MS: f64 = 5000.0;

// ---------------------------------------------------------------------------
// Keyframe & Error Recovery
// ---------------------------------------------------------------------------

/// Camera keyframe interval (frames). Also defined per-tier in `VIDEO_QUALITY_TIERS`.
pub const CAMERA_KEYFRAME_INTERVAL_FRAMES: u32 = 150;

/// Screen share keyframe interval (frames).
/// Periodic keyframes ensure recovery from packet loss on screen share streams.
pub const SCREEN_KEYFRAME_INTERVAL_FRAMES: u32 = 150;

/// **Base** wall-clock ceiling on the camera periodic keyframe interval
/// (milliseconds) — the #1510 freeze-recovery guarantee for the tiers where it
/// matters most (full_hd … low).
///
/// The frame-counted `keyframe_interval_frames` only guarantees ~5s at the tier's
/// nominal fps. Under CPU load or at low AQ tiers the actual fps drops and the
/// frame-counted floor stretches to 10–17s. This wall-clock cap guarantees a
/// periodic keyframe at least every 5s regardless of actual encode rate (issue #1510).
///
/// Issue #1531 makes the *effective* ceiling tier- and transport-aware via
/// [`camera_periodic_keyframe_max_interval_ms`]: the two lowest tiers relax the cap
/// (they are the most bandwidth-scarce, so a forced I-frame every 5s costs a larger
/// share of the tier budget), and a lossless (WS) transport extends that relief band
/// one tier higher. This constant is the value returned for every non-relaxed tier,
/// so the #1510 ≤5s guarantee is preserved unchanged on full_hd … low over WebTransport.
pub const PERIODIC_KEYFRAME_MAX_INTERVAL_MS: f64 = 5000.0;

/// Relaxed camera periodic-keyframe ceiling (ms) for the second-lowest AQ tier
/// (`very_low`, 480×270 / 15 fps / ~250 kbps) — and, on a lossless transport, also
/// for the `low` tier (issue #1531).
///
/// At the very lowest tiers bandwidth is scarcest, so a forced I-frame every 5s is a
/// disproportionate share of the tier budget. Relaxing to ~7s cuts that overhead
/// while keeping freeze recovery bounded. See [`camera_periodic_keyframe_max_interval_ms`].
pub const PERIODIC_KEYFRAME_MAX_INTERVAL_VERY_LOW_TIER_MS: f64 = 7000.0;

/// Relaxed camera periodic-keyframe ceiling (ms) for the lowest AQ tier
/// (`minimal`, 426×240 / 10 fps / ~150 kbps) — the scarcest link (issue #1531).
///
/// Bounded at 8s: this is the **absolute** camera periodic-keyframe ceiling across
/// all tiers/transports. It is deliberately kept close to the receiver-side
/// keyframe-less-hold escalation (`MAX_KEYFRAME_LESS_HOLD_MS` = 6s, issue #1662 in
/// `videocall-codecs`). Relaxing past ~8s would widen the window in which a desynced
/// receiver resets its decoder (#1662) and then still waits for the periodic keyframe;
/// 8s keeps that post-reset wait ≤2s. See [`camera_periodic_keyframe_max_interval_ms`].
pub const PERIODIC_KEYFRAME_MAX_INTERVAL_MINIMAL_TIER_MS: f64 = 8000.0;

/// Effective camera periodic-keyframe wall-clock ceiling (ms) for the current AQ
/// tier and transport (issue #1531). Pure so a host test pins the per-tier /
/// per-transport selection off the wasm-only encode loop.
///
/// `tier_index` indexes [`VIDEO_QUALITY_TIERS`] (0 = full_hd … `len-1` = minimal),
/// resolved positionally-from-the-bottom so a future ladder-length change keeps
/// working. `lossless_transport` is `true` on a reliable/ordered transport (WebSocket)
/// where periodic keyframes are *insurance* — there is no datagram loss to actively
/// recover from and `KEYFRAME_REQUEST`s are reliably delivered — so the relief band
/// extends one tier higher than on lossy WebTransport.
///
/// Selection (for the current 8-tier ladder):
/// - lowest tier (`minimal`, idx 7) → [`PERIODIC_KEYFRAME_MAX_INTERVAL_MINIMAL_TIER_MS`] (8s)
/// - second-lowest (`very_low`, idx 6) → [`PERIODIC_KEYFRAME_MAX_INTERVAL_VERY_LOW_TIER_MS`] (7s)
/// - third-lowest (`low`, idx 5) → 7s **only on a lossless transport**, else the base 5s
/// - every higher tier (full_hd … standard) → [`PERIODIC_KEYFRAME_MAX_INTERVAL_MS`] (5s)
///
/// No combination exceeds 8s, bounding the #1662 receiver-side interaction (see
/// [`PERIODIC_KEYFRAME_MAX_INTERVAL_MINIMAL_TIER_MS`]). Screen shares are deliberately
/// NOT relaxed this way — screen keeps the flat 3s
/// [`SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS`] recovery guarantee on every tier
/// (static screen content = keyframes are the only paintable data), and its tier
/// relief lives in the static-share floor fan-out instead (issue #1903 follow-up).
pub fn camera_periodic_keyframe_max_interval_ms(
    tier_index: usize,
    lossless_transport: bool,
) -> f64 {
    let n = VIDEO_QUALITY_TIERS.len();
    // Positions from the bottom of the ladder (most-constrained first).
    let minimal = n.saturating_sub(1);
    let very_low = n.saturating_sub(2);
    let low = n.saturating_sub(3);

    if tier_index >= minimal {
        PERIODIC_KEYFRAME_MAX_INTERVAL_MINIMAL_TIER_MS
    } else if tier_index == very_low {
        PERIODIC_KEYFRAME_MAX_INTERVAL_VERY_LOW_TIER_MS
    } else if lossless_transport && tier_index == low {
        // Lossless (WS) insurance-only relief extends one tier up to `low`.
        PERIODIC_KEYFRAME_MAX_INTERVAL_VERY_LOW_TIER_MS
    } else {
        PERIODIC_KEYFRAME_MAX_INTERVAL_MS
    }
}

/// Wall-clock ceiling for screen-share periodic keyframes (milliseconds).
/// Screen tiers use a ~3s nominal GOP for text readability. The screen-specific
/// cap preserves that 3s design intent under low-fps conditions (issue #1510).
///
/// Deliberately flat across all screen tiers and both transports (issue #1531):
/// screen content is keyframe-critical (a static share = keyframes are the only
/// paintable content; #1899/#1903), so the 3s recovery guarantee is NOT relaxed on
/// low tiers the way the camera cadence is. The tier/congestion relief for screen is
/// applied to the static-share keyframe FLOOR fan-out (how many simulcast layers a
/// floor keyframe re-encodes), not to the cadence — see `screen_encoder.rs`.
pub const SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS: f64 = 3000.0;

// --- Issue #1531 compile-time invariants for the camera keyframe-ceiling relief ---
// The relief must be monotonic toward the scarcest tiers (a lower tier never emits
// keyframes MORE often than a higher one) and the base must remain the #1510 5s
// guarantee, so a future retune that inverts the relaxation or regresses the base
// fails the build rather than silently flattening the policy.
const _: () = assert!(
    PERIODIC_KEYFRAME_MAX_INTERVAL_MS <= PERIODIC_KEYFRAME_MAX_INTERVAL_VERY_LOW_TIER_MS,
    "very_low ceiling must not be tighter than the base (relief only relaxes)"
);
const _: () = assert!(
    PERIODIC_KEYFRAME_MAX_INTERVAL_VERY_LOW_TIER_MS
        <= PERIODIC_KEYFRAME_MAX_INTERVAL_MINIMAL_TIER_MS,
    "minimal ceiling must not be tighter than very_low (relief deepens toward the floor)"
);
// The absolute ceiling stays bounded so the #1662 (MAX_KEYFRAME_LESS_HOLD_MS = 6s)
// receiver interaction cannot be widened past ~2s by a retune.
const _: () = assert!(
    PERIODIC_KEYFRAME_MAX_INTERVAL_MINIMAL_TIER_MS <= 8000.0,
    "the lowest-tier camera keyframe ceiling must stay <= 8s to bound the #1662 receiver interaction"
);

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

/// Select the NETWORK-imposed floor on the starting screen-share quality tier.
///
/// # Signals
/// - `rtt_ms`: Most-recent average server RTT, or `None` if unknown (e.g. first meeting
///   before any RTT probes have completed, or WebSocket-only deployment).
/// - `camera_tier_index`: Current camera AQ tier index (0 = full-HD, higher = degraded).
///   Pass `None` if camera is not started (screen-only share).
///
/// # Returns
/// An index into [`SCREEN_QUALITY_TIERS`]: the WORST (highest-index) tier the
/// share is allowed to start better than. `0` means "the network imposes no
/// constraint" — it does NOT mean "start at the 2160p rung"; the resolution the
/// share actually starts at is chosen by [`resolve_initial_screen_tier`], which
/// composes this answer with the captured source size.
///
/// The constrained answers are resolved by LABEL (`medium` / `low`), so they
/// keep pointing at 720p/8fps and 720p/5fps exactly as they did before issue
/// #2179 inserted the 1440p and 2160p rungs above them. Hard-coding `1` / `2`
/// here would have silently promoted a poor-RTT client from 720p to 1080p and a
/// fair-RTT client from 720p to 1440p.
///
/// # Failure mode — cold start
/// When `rtt_ms` is `None` (first meeting, no prior probes) and the camera has not yet
/// been degraded, this returns `0` — no network floor — so the source-resolution
/// term decides. The PID loop and the five self-congestion axes ramp down within
/// a few seconds if the uplink cannot sustain the chosen tier. In practice this
/// branch is rare: a user reaches the share button after being connected long
/// enough for RTT probes to have reported.
pub fn initial_screen_tier(rtt_ms: Option<f64>, camera_tier_index: Option<usize>) -> usize {
    // Cold start: no signals available → no network-imposed floor.
    if rtt_ms.is_none() && camera_tier_index.is_none() {
        return 0;
    }

    // RTT-based thresholds
    let rtt_poor = rtt_ms.map(|rtt| rtt >= RTT_POOR_MS).unwrap_or(false);
    let rtt_fair = rtt_ms.map(|rtt| rtt >= RTT_FAIR_MS).unwrap_or(false);

    // Camera tier degradation indicators
    // Camera tiers: 0=full_hd, 1=hd_plus, 2=hd, 3=standard, 4=medium, 5=low, 6=very_low, 7=minimal
    // Threshold: ≥3 (sd/low) means camera is already degraded
    let camera_degraded = camera_tier_index.map(|idx| idx >= 3).unwrap_or(false);

    // Decision table:
    // RTT >= POOR (400ms)     → "low"     (720p/5fps)
    // RTT >= FAIR (200ms)     → "medium"  (720p/8fps)
    // RTT < FAIR, camera ≥ sd → "medium"  (camera already degraded)
    // RTT < FAIR, camera < sd → 0         (no network floor; source decides)
    // RTT None, camera ≥ sd   → "medium"  (camera signal only)
    // RTT None, camera < sd   → 0         (camera signal only, optimistic)

    if rtt_poor {
        return screen_tier_index_by_label(SCREEN_TIER_LABEL_FLOOR);
    }

    if rtt_fair || camera_degraded {
        return screen_tier_index_by_label(SCREEN_TIER_LABEL_BASELINE);
    }

    0
}

/// The WORST (cheapest) [`SCREEN_QUALITY_TIERS`] rung that still contains
/// `src_w x src_h` without downscaling it (issue #2179).
///
/// # Semantics
/// Walks the ladder from worst to best and returns the first rung whose box
/// covers the source, comparing LONG edge to long edge and SHORT edge to short
/// edge. Paired with [`crate::orient_box_to_source`] — which the screen encoder
/// uses at every fit site — a covering rung encodes the source at its native
/// size, so this is precisely "the least expensive tier that costs the source
/// zero resolution".
///
/// - A source LARGER than even the top rung returns `0`; the top rung's box
///   still binds and the source is downscaled into it (unavoidable).
/// - A source with an unknown dimension (`0` on either axis — a capture track
///   that has not reported `getSettings()` yet) returns
///   [`DEFAULT_SCREEN_TIER_INDEX`], the pre-#2179 conservative baseline, because
///   guessing a rung from a fabricated size is worse than the old default.
///
/// # Why the match is ORIENTATION-AGNOSTIC (issue #2179 review)
/// Every rung's box is authored landscape. A per-axis comparison therefore
/// matched no rung at all for a rotated 1440x2560 panel (`2160 >= 2560` fails
/// even on the top rung), so it fell through to `0` — handing a portrait share
/// the 2160p rung's 8000 kbps setpoint for a stream that still got downscaled
/// to 1215x2160 by the landscape box. Comparing sorted `(long, short)` pairs
/// selects the `1440p` rung instead, and the oriented box then holds the panel
/// 1:1 at that rung's honest 5000 kbps budget.
///
/// # Known limitation (NOT fixed here)
/// The `getDisplayMedia` capture ceiling is still a per-axis
/// `max 3840 x max 2160`, so a surface TALLER than 2160 px arrives already
/// downscaled by the browser. This function only decides the ENCODE rung; it
/// cannot recover pixels the capture pipeline never delivered.
///
/// # Why "worst that fits" rather than "best available"
/// Requirement: start at the resolution actually being shared. Choosing the
/// BEST rung would hand a 720p window the 2160p rung's 8000 kbps setpoint for a
/// stream that the fit still encodes at 1280x720 — pure waste. Choosing the
/// worst rung that fits pins resolution to the source while keeping the
/// bitrate/fps budget proportionate to it.
pub fn screen_tier_for_source(src_w: u32, src_h: u32) -> usize {
    if src_w == 0 || src_h == 0 {
        return DEFAULT_SCREEN_TIER_INDEX;
    }
    let (src_long, src_short) = (src_w.max(src_h), src_w.min(src_h));
    for (i, tier) in SCREEN_QUALITY_TIERS.iter().enumerate().rev() {
        let (box_long, box_short) = (
            tier.max_width.max(tier.max_height),
            tier.max_width.min(tier.max_height),
        );
        if box_long >= src_long && box_short >= src_short {
            return i;
        }
    }
    0
}

/// Resolve the tier a screen share should START at (issue #2179).
///
/// Composes the two independent inputs:
/// - `network_tier` — the floor from [`initial_screen_tier`] (RTT / camera
///   degradation). Higher index = worse = more conservative.
/// - `(src_w, src_h)` — the capture track's real `getSettings()` size, i.e. the
///   resolution the user actually chose to share.
///
/// # Rule
/// ```text
/// resolved = max( min( screen_tier_for_source(src), index_of("high") ),
///                 network_tier )
/// ```
/// read as two clamps in quality terms:
/// 1. `min(..., index_of("high"))` — never start WORSE than the pre-#2179
///    optimistic answer, which was always the 1080p `high` rung. A 1280x720
///    window is *contained* by the `low` rung (720p/5fps/500kbps), but starting
///    a share there would be a bitrate/fps regression, so it is pulled back up
///    to `high`; `fit_within_preserving_aspect` still encodes it at 1280x720,
///    identical to today, just with `high`'s 2500 kbps / 10 fps budget. This
///    clamp is ALSO what handles an unknown capture size: `screen_tier_for_source`
///    returns [`DEFAULT_SCREEN_TIER_INDEX`] there, which this pulls up to `high`
///    — exactly the pre-#2179 behaviour.
/// 2. `max(..., network_tier)` — never start BETTER than the network signals
///    allow. A poor-RTT client sharing a 4K panel still starts at `low`, exactly
///    as it did before this change.
///
/// Composed, the result is **never worse than the pre-#2179 start in any case,
/// and better only when the source genuinely carries more pixels than the 1080p
/// rung can hold on a link with no network-imposed floor.** The case issue #2179
/// reports — a DPR-2 Retina window at 2496x1440 on a healthy link — resolves to
/// the `1440p` rung and is encoded at 2496x1440, its own pixels, with no
/// resample at all, instead of being fitted into 1920x1080 and then into
/// 1280x720.
pub fn resolve_initial_screen_tier(src_w: u32, src_h: u32, network_tier: usize) -> usize {
    let optimistic_floor = screen_tier_index_by_label(SCREEN_TIER_LABEL_1080P);
    let source_tier = screen_tier_for_source(src_w, src_h).min(optimistic_floor);
    source_tier
        .max(network_tier)
        .min(SCREEN_QUALITY_TIERS.len().saturating_sub(1))
}

// ---------------------------------------------------------------------------
// Screen share PERSISTENT quality ceiling (issue #2179 review round)
// ---------------------------------------------------------------------------
//
// `resolve_initial_screen_tier` only decides where a share STARTS. Once the AQ
// loop is running, nothing stopped it from climbing a small/weak share all the
// way to the `native` rung's 8000 kbps setpoint — a 720p window billed at 4K
// money, multiplied by SFU fan-out. These helpers produce a PERSISTENT index
// floor (quality is the inverse of index) that the controller installs for the
// life of the share, composed with — never replacing — the user's own bounds.

// --- Device-class bars: TWO SEPARATE calibrations (issue #2179 review r2) ---
//
// They currently share their numbers, and they are deliberately NOT expressed in
// terms of each other. One governs how many CONCURRENT encodes a device is
// trusted with; the other how many PIXELS PER SECOND a single screen stream may
// cost it. Collapsing the two is exactly what produced the first cut's defect:
// the `1440p` tier was gated behind the 3-LAYER bar (>= 10 cores), so a 6–9-core
// Retina laptop — the machine class issue #2179 was actually reported from — was
// capped at 1080p and stayed fuzzy, i.e. the fix missed its own bug report on
// most consumer hardware. Either bar may be retuned without touching the other.

/// LAYER bar: logical cores at/above which a sender is trusted with more than
/// ONE concurrent encode.
///
/// Mirrors `dioxus-ui::components::capability_check::MIN_CORES_FOR_MULTILAYER`.
/// Duplicated rather than imported because the dependency runs the other way
/// (`dioxus-ui` → `videocall-aq`) and the UI constant is private; the two are
/// kept in lockstep by naming the same cc7tp post-mortem rule ("2-core /
/// low-core Intel MacBooks stall the main thread under multi-layer encode").
pub const SCREEN_LAYER_BAR_MULTILAYER_CORES: u32 = 6;

/// LAYER bar: logical cores at/above which a sender is trusted with the FULL
/// 3-rung ladder. Mirrors
/// `dioxus-ui::components::capability_check::CORES_FOR_3_LAYERS`.
pub const SCREEN_LAYER_BAR_FULL_LADDER_CORES: u32 = 10;

/// TIER ceiling: logical cores at/above which a share may reach the `1440p`
/// rung.
///
/// # Pixel-rate arithmetic (computed from the shipped table, not asserted)
/// | configuration                       | Mpx/s |
/// |-------------------------------------|-------|
/// | single `high`  1920x1080 @ 10 fps    | 20.74 |
/// | 2-layer ladder `[low, high]`         | 25.34 |
/// | single `1440p` 2560x1440 @ 10 fps    | 36.86 |
/// | 3-layer ladder `[low, high, 1440p]`  | 62.21 |
/// | single `native` 3840x2160 @ 10 fps   | 82.94 |
///
/// A single 1440p stream is **1.45x** the 2-layer ladder this bar already
/// authorises and **0.59x** the 3-layer ladder the upper bar authorises — it
/// falls BETWEEN the two bars rather than neatly under either. It is placed at
/// the LOWER bar deliberately:
/// - it is ONE encode, not two: a single WebCodecs instance, one rate
///   controller, one keyframe cadence, none of the per-stream fixed overhead
///   that makes concurrent encodes cost more than their pixel sum;
/// - the alternative fails the issue on most consumer hardware. Apple-Silicon
///   M-series report 8–12 logical cores, so gating `1440p` at the >= 10 bar
///   leaves a large share of exactly the DPR-2 Retina machines #2179 was
///   reported from capped at 1080p — still fuzzy, which is the entire bug;
/// - it is a CEILING, not an operating point. A device that cannot sustain it
///   is stepped down within seconds by the encoder-queue backpressure axis —
///   the same "erring generous is safe because the shed catches it" argument
///   `capability_check` makes for its own layer bar.
///
/// **Known interaction, not fixed here:** in simulcast the BASE encoder takes
/// its dimensions from the AQ tier while the upper rungs use their own ladder
/// boxes, so a 6-core sender at this ceiling with 2 active rungs costs
/// 36.86 + 20.74 = 57.60 Mpx/s rather than 36.86. That base-layer geometry
/// mismatch is a separate, pre-existing defect (the base layer is also budgeted
/// at rung 0's bitrate); it is being tracked on its own follow-up.
pub const SCREEN_TIER_1440P_MIN_CORES: u32 = 6;

/// TIER ceiling: logical cores at/above which a share may reach the `native`
/// (2160p) rung.
///
/// Stays at the upper bar. A single `native` stream is **82.94 Mpx/s** — see the
/// table on [`SCREEN_TIER_1440P_MIN_CORES`] — which is **1.33x** the entire
/// 3-layer ladder the >= 10-core class is trusted with, so unlike the 1440p case
/// there is no argument for lowering it: it is the most expensive single encode
/// the ladder can ask for, by a wide margin.
pub const SCREEN_TIER_NATIVE_MIN_CORES: u32 = 10;

// Ordering sanity: a better rung may never require FEWER cores than a worse one.
const _: () = assert!(
    SCREEN_TIER_NATIVE_MIN_CORES >= SCREEN_TIER_1440P_MIN_CORES,
    "the `native` tier bar must be at least as demanding as the `1440p` bar"
);

/// Device-class term on a screen share's quality ceiling: the BEST (lowest)
/// [`SCREEN_QUALITY_TIERS`] index a sender with `cores` logical CPUs may reach.
///
/// Three classes, calibrated by pixel rate rather than by concurrent-encode
/// count — see [`SCREEN_TIER_1440P_MIN_CORES`]:
/// - `>= SCREEN_TIER_NATIVE_MIN_CORES` → index `0`, i.e. this term imposes no
///   restriction at all;
/// - `>= SCREEN_TIER_1440P_MIN_CORES`  → no better than `1440p`;
/// - below that                        → no better than `1080p`.
///
/// **The top class does NOT mean a 2160p encode is reachable.** This is one term
/// of a composition, and [`resolve_screen_tier_ceiling`] also applies
/// [`screen_ladder_top_index`], which caps EVERY path at `1440p` (issue #2179
/// review r3). No composed ceiling returns `0`; the `native` rung's only
/// remaining job is to donate the `getDisplayMedia` capture ceiling. Returning
/// `0` here means "the CPU is not what is holding this share back", nothing more.
///
/// `cores == 0` means `navigator.hardwareConcurrency` was unavailable and is
/// treated as the most conservative class, exactly like the UI capability sniff.
pub fn screen_tier_device_floor(cores: u32) -> usize {
    if cores >= SCREEN_TIER_NATIVE_MIN_CORES {
        0
    } else if cores >= SCREEN_TIER_1440P_MIN_CORES {
        screen_tier_index_by_label(SCREEN_TIER_LABEL_1440P)
    } else {
        screen_tier_index_by_label(SCREEN_TIER_LABEL_1080P)
    }
}

/// SINGLE-STREAM term on a screen share's quality ceiling: the BEST (lowest)
/// index a share publishing exactly ONE rung may reach.
///
/// # Why single-stream needs its own, tighter cap
/// On the simulcast path a receiver that cannot afford the top rung simply
/// decodes a lower one. On the single-stream path there is no lower rung: one
/// encode, `TileHint::Uncapped`, no receiver→sender tier feedback, and the
/// receiver's decode budget caps the NUMBER of streams, not their resolution.
/// Every receiver is therefore pinned to whatever the sender chose, so the
/// sender must be the conservative party: never better than `1440p`, and never
/// better than its own device class allows either (composed here rather than
/// left to the caller, so a standalone call is correct on its own).
///
/// # Currently SUBSUMED (issue #2179 review r3)
/// [`screen_ladder_top_index`] now caps every path at the same `1440p` rung, so
/// this term never binds on its own today — a single-stream share and a
/// simulcast share reach the same ceiling. It is kept rather than deleted
/// because the two caps are independent policies that merely coincide: the
/// moment the publish ladder gains a rung above `1440p`, this one starts binding
/// again and the single-stream path stays conservative without anyone having to
/// remember why. `screen_ceiling_cause_reports_the_uniquely_responsible_term`
/// pins the coincidence so a ladder change surfaces it.
pub fn screen_tier_single_stream_floor(cores: u32) -> usize {
    screen_tier_index_by_label(SCREEN_TIER_LABEL_1440P).max(screen_tier_device_floor(cores))
}

// --- Cause vocabulary for a ceiling-constrained share (issue #2179 review r2) -

/// Cause hint for a share held below its source's rung by the DEVICE term.
///
/// Reuses the EXISTING receive-side vocabulary rather than inventing a synonym:
/// `cpu-pressure` is already what `cause_hint_from_trigger` emits for the
/// CPU/fps axis and is already enumerated in the diagnostics panel's Cause
/// legend, so no receiver copy has to change. Its meaning widens slightly —
/// from "the AQ stepped down on CPU pressure" to "this machine's class is what
/// is holding the share back" — but both readings are "your CPU is the limit",
/// which is what the line has to tell the user.
pub const SCREEN_CAUSE_CPU: &str = "cpu-pressure";

/// The BEST (lowest) [`SCREEN_QUALITY_TIERS`] index any SCREEN simulcast rung
/// can carry — derived from the ladder itself, never hard-coded.
///
/// This is the ceiling on EVERY encode path (issue #2179 review r3, security).
/// The simulcast base rung takes its geometry from the AQ tier while its budget
/// comes from ladder rung 0, so an AQ tier better than the ladder's top made the
/// base rung encode 3840x2160 on `low`'s 500 kbps — and the base rung is exactly
/// what a struggling receiver falls back to, which inverts the whole "receivers
/// can choose a lower rung" mitigation. Capping the ceiling here means the base
/// rung can never exceed what some published rung actually carries.
///
/// The base-rung GEOMETRY fix (bounding it by `simulcast_screen_layers(n)[0]`)
/// is the real repair and is tracked separately; this cap is the bound that
/// makes the current geometry safe in the meantime.
pub fn screen_ladder_top_index() -> usize {
    simulcast_screen_layer_labels(SCREEN_SIMULCAST_MAX_LAYERS)
        .iter()
        .map(|&label| screen_tier_index_by_label(label))
        .min()
        .unwrap_or(0)
}

/// Cause hint for a share held below its source's rung by the PUBLISH LADDER's
/// top rung ([`screen_ladder_top_index`]).
///
/// Distinct from the CPU and single-stream causes because it is a property of
/// the product, not of the user's machine or their stream count: no amount of
/// CPU or extra rungs will lift it. Telling a 4K sharer "cpu-pressure" when the
/// ladder would cap them at 1440p regardless would send them optimising the
/// wrong thing.
pub const SCREEN_CAUSE_LADDER: &str = "ladder-limited";

/// Cause hint for a share held below its source's rung by the SINGLE-STREAM cap.
///
/// A genuinely new string: no existing hint expresses "you are publishing one
/// rung, so every receiver is pinned to it and the sender must stay
/// conservative". The receive-side renderer prints the hint verbatim, so this
/// renders correctly today; the diagnostics panel's Cause LEGEND enumerates the
/// vocabulary and must gain this entry.
pub const SCREEN_CAUSE_SINGLE_STREAM: &str = "single-stream-limited";

/// Cause hint for a share held below its source's rung by the network floor or
/// by live AQ congestion — the pre-existing default.
pub const SCREEN_CAUSE_BITRATE: &str = "bitrate-limited";

/// WHICH term of the composed ceiling holds this share below the rung its own
/// captured source needs — `""` when none does (issue #2179 review r2).
///
/// # Why this exists
/// Publishing a single composed ceiling traded a false POSITIVE for a false
/// NEGATIVE: a 4K sharer capped by their CPU class sits exactly at their
/// ceiling, so the "at or better than the ceiling → say nothing" rule stamped
/// no cause at all, while their viewers saw a red "3840x2160 → 2560x1440 ↓44%"
/// downscale badge with nothing to explain it. That is a real, nameable
/// constraint and it must be named.
///
/// The reference point for "is anything being withheld?" is therefore the
/// SOURCE-only rung ([`resolve_initial_screen_tier`] with no network floor), and
/// this function says which of the OTHER terms pushed the ceiling above it.
///
/// Precedence when several terms bind: the single-stream cap only wins when it
/// is strictly tighter than the device term, because on a low-core machine the
/// device class is the ROOT cause and "your CPU" is the more actionable thing to
/// tell the user than "you are publishing one rung" (which is itself a
/// consequence of the core count).
pub fn screen_ceiling_cause(
    src_w: u32,
    src_h: u32,
    cores: u32,
    effective_layers: u32,
) -> &'static str {
    let source = resolve_initial_screen_tier(src_w, src_h, 0);
    let device = screen_tier_device_floor(cores);
    let single = if effective_layers <= 1 {
        screen_tier_single_stream_floor(cores)
    } else {
        0
    };
    // Terms that apply to EVERY share, layered outward. A term is named only
    // when removing it would actually raise the ceiling — otherwise we would
    // blame the user's CPU for a cap the product ladder imposes anyway.
    let always = source.max(screen_ladder_top_index());
    let with_device = always.max(device);
    let ceiling = with_device.max(single);
    if ceiling <= source {
        ""
    } else if single > with_device {
        SCREEN_CAUSE_SINGLE_STREAM
    } else if device > always {
        SCREEN_CAUSE_CPU
    } else {
        SCREEN_CAUSE_LADDER
    }
}

/// The PERSISTENT quality ceiling for a screen share: the BEST (lowest)
/// [`SCREEN_QUALITY_TIERS`] index this share may ever reach, for its whole life.
///
/// Composes three independent terms, most restrictive (highest index) wins:
/// 1. **Source** — [`resolve_initial_screen_tier`] with no network floor, i.e.
///    the cheapest rung that still costs the source zero resolution (pulled up
///    to `high` so a small window is never billed WORSE than the pre-#2179
///    start).
/// 2. **Device** — [`screen_tier_device_floor`].
/// 3. **Stream count** — [`screen_tier_single_stream_floor`], applied only when
///    `effective_layers <= 1`.
/// 4. **Publish ladder** — [`screen_ladder_top_index`], on every path.
///
/// [`screen_ceiling_cause`] reports WHICH of terms 2–4 raised the result above
/// term 1, which is what the publisher stamps as its Cause hint.
///
/// # Consequence: the `native` rung is UNREACHABLE as an ENCODE tier
/// Term 4 caps every path at the ladder's top (`1440p`), so this function can
/// never return `0`. That is deliberate: the `native` rung's remaining job is to
/// donate the CAPTURE ceiling (`SCREEN_QUALITY_TIERS[0]`'s dims are what
/// `getDisplayMedia` requests as `max`), so a 4K surface still arrives at 4K and
/// is downscaled ONCE, by the compositor, into the encode rung — rather than
/// being pre-shrunk at capture and then re-fitted. Nothing encodes at 2160p.
/// Any doc or test claiming a 2160p ENCODE is wrong.
///
/// The result is a FLOOR on the index, so the AQ PID may still step DOWN from
/// it freely under congestion; it can simply never climb past it. It is
/// composed with (never substituted for) the user's own `best`/`worst` bounds.
///
/// The screen encoder publishes this value to the UI as
/// `ScreenQualitySnapshot::best_source_tier_index`, where "live tier index
/// equals it" means "as good as this share can get" and "live index greater
/// than it" means "genuinely constrained below what the source needs".
pub fn resolve_screen_tier_ceiling(
    src_w: u32,
    src_h: u32,
    cores: u32,
    effective_layers: u32,
) -> usize {
    let mut floor = resolve_initial_screen_tier(src_w, src_h, 0)
        .max(screen_tier_device_floor(cores))
        // Issue #2179 review r3 (security): no path may reach a rung better than
        // the publish ladder's own top — see `screen_ladder_top_index`.
        .max(screen_ladder_top_index());
    if effective_layers <= 1 {
        floor = floor.max(screen_tier_single_stream_floor(cores));
    }
    floor.min(SCREEN_QUALITY_TIERS.len().saturating_sub(1))
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
/// to `8 × live_screen_tier_frame_interval_ms` (800ms at 10fps, 1000ms at 8fps,
/// or 1600ms at 5fps), preventing K-amplification false positives on healthy
/// links as the active screen tier degrades or recovers.
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
// Client-Side WebTransport Camera Stale-DELTA Self-Detection (#1737 Phase 1)
// ---------------------------------------------------------------------------

/// Number of sender-side camera DELTA frames age-dropped on WebTransport within
/// [`CAMERA_WT_STALE_DROP_WINDOW_MS`] that triggers a local AQ step-down.
///
/// A stale-delta drop is a soft, potentially frequent event: the transport has
/// deliberately skipped one old delta after `writer.ready()` finally resolved,
/// while keyframes remain exempt. Requiring 12 within a 2s window mirrors the
/// screen freshness axis and means a transient burst drains without cutting a
/// rung, while sustained stale-delta gating converges the camera encoder toward
/// the achievable uplink rate.
pub const CAMERA_WT_STALE_DROP_THRESHOLD: u64 = 12;

/// Tumbling window (ms) for counting #1737 camera WT stale-delta drops.
pub const CAMERA_WT_STALE_DROP_WINDOW_MS: f64 = 2000.0;

const _: () = assert!(
    CAMERA_WT_STALE_DROP_WINDOW_MS >= WS_SELF_CONGESTION_WINDOW_MS,
    "camera WT stale-drop window must be at least as wide as the WS overflow \
     window: stale-delta drops are softer and more frequent than hard send \
     failures, so they must persist longer before shedding."
);
const _: () = assert!(
    CAMERA_WT_STALE_DROP_THRESHOLD > WS_SELF_CONGESTION_DROP_THRESHOLD,
    "camera WT stale-delta drops can be prolific under congestion, so the \
     sustained-cluster threshold must exceed the WS overflow threshold."
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

// ---------------------------------------------------------------------------
// Client-Side Screen WS Freshness-Gate Self-Detection (#1921)
// ---------------------------------------------------------------------------
//
// The #1921 WS send-side freshness gate (videocall-client `screen_encoder`)
// DROPS stale screen DELTAS once the browser's WebSocket `bufferedAmount`
// backlog exceeds ~half a second of screen bitrate, converging the socket queue
// to ~156KB at the top tier. That is BELOW the 1MB `bufferedAmount` memory cap
// that increments `websocket_drop_count()` — the counter the WS overflow axis
// (`WS_SELF_CONGESTION_*`) keys off. So under sustained high-motion congestion
// in the moderate band the gate absorbs the overrun, `websocket_drop_count()`
// never moves, and the screen tier stays pinned high: the encoder keeps
// spending CPU on deltas the gate discards and its keyframes stay large. This
// axis closes that gap by treating a SUSTAINED cluster of freshness-gate drops
// (`screen_ws_stale_delta_drops()`) as its own AQ step-down trigger, so the tier
// converges toward the achievable rate (and the lower tier tightens the gate's
// own threshold — a beneficial coupling that speeds the drain).
//
// Threshold rationale — deliberately NOT the WS 3-in-1000ms shape:
//   A freshness-gate drop is a SOFT, FREQUENT event (it fires whenever the
//   backlog exceeds ~half a second of screen bitrate, potentially many times a
//   second while congested), unlike a WS `bufferedAmount` 1MB overflow, which is
//   a HARD, rare event. Copying the WS threshold of 3 would fire on a sub-second
//   `bufferedAmount` spike — the stateless gate already handles such a blip by
//   resuming deltas the instant the queue drains, so cutting the tier for it
//   would over-react. We require a SUSTAINED cluster instead: a wider 2000ms
//   window (≥ ~2 AQ ticks, matching the WT axis) and a count only a
//   persistently-gated stream reaches (~6 gated deltas/sec sustained). A
//   transient spike cannot shed a layer; a chronically over-capacity share
//   converges one rung per window until it fits. Convergence, not oscillation:
//   each step-down lowers the offered load (and tightens the gate threshold);
//   step-UP is owned by the separate, dwell-gated AQ ramp, so a cleared episode
//   earns back slowly rather than yo-yoing against this fast axis.

/// Number of #1921 screen WS freshness-gate DELTA drops
/// (`videocall_client::encode::screen_ws_stale_delta_drops`) within
/// [`SCREEN_WS_STALE_DROP_WINDOW_MS`] that triggers a local AQ step-down.
///
/// 12 (not the WS overflow axis's 3): a freshness-gate drop is soft and
/// frequent, so a short `bufferedAmount` spike can drop a handful of deltas
/// before the queue drains. Requiring 12 within the 2s window (~6/sec sustained)
/// means only a share that stays OVER CAPACITY across multiple AQ ticks sheds a
/// rung; an isolated blip is left to the stateless gate, which self-clears.
pub const SCREEN_WS_STALE_DROP_THRESHOLD: u64 = 12;

/// Tumbling window (ms) for counting #1921 screen WS freshness-gate drops.
///
/// 2000ms (wider than the WS overflow axis's 1000ms) so the evidence must
/// persist across at least ~2 AQ ticks before shedding — a single sub-second
/// gating burst does not cut the tier.
pub const SCREEN_WS_STALE_DROP_WINDOW_MS: f64 = 2000.0;

// --- Compile-time invariants (#1921) ---
const _: () = assert!(
    SCREEN_WS_STALE_DROP_WINDOW_MS >= WS_SELF_CONGESTION_WINDOW_MS,
    "screen freshness-drop window must be at least as wide as the WS overflow \
     window: freshness-gate drops are softer and more frequent than 1MB \
     bufferedAmount overflows, so they must persist longer before shedding."
);
const _: () = assert!(
    SCREEN_WS_STALE_DROP_THRESHOLD > WS_SELF_CONGESTION_DROP_THRESHOLD,
    "a freshness-gate drop is far more prolific than a 1MB send-buffer overflow, \
     so its sustained-cluster threshold must exceed the WS overflow threshold or \
     a transient bufferedAmount blip would shed a layer."
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
    fn reduced_ladder_exact_values_through_the_resolver() {
        // Issue #1768: pin the REDUCED ladder through the PRODUCTION resolver
        // (`simulcast_layers_for`), so changing a rung — or wiring the variant to
        // the wrong source table — FAILS here. Values are (w, h, fps, ideal_kbps).
        let l = simulcast_layers_for(3, LadderVariant::Reduced);
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
            vec![(320, 180, 7, 120), (640, 360, 15, 350), (960, 540, 30, 900),],
            "reduced camera ladder must be 180p/360p/540p with the 540p band"
        );
        // Keyframe intervals track ~5s wall-clock at each rung's fps (unchanged
        // fps ⇒ unchanged intervals; the top rung stays 30fps at 540p).
        assert_eq!(l[0].keyframe_interval_frames, 35); // 5s × 7fps
        assert_eq!(l[1].keyframe_interval_frames, 75); // 5s × 15fps
        assert_eq!(l[2].keyframe_interval_frames, 150); // 5s × 30fps
                                                        // The min/max band comes from VIDEO_QUALITY_TIERS' 540p rung, not invented.
        assert_eq!((l[2].min_bitrate_kbps, l[2].max_bitrate_kbps), (500, 1500));
    }

    #[test]
    fn default_variant_is_byte_identical_to_the_plain_resolver() {
        // The gate must be INERT when off. NOTE the assertion shape: comparing
        // `simulcast_layers(n)` against `simulcast_layers_for(n, Default)` would be a
        // TAUTOLOGY — the former is *defined* as the latter, so both sides move
        // together under any mutation and the comparison can never fail. (An earlier
        // revision of this test did exactly that, and claimed mutation power it did
        // not have.) So resolve against `SIMULCAST_VIDEO_LAYERS` — the independent
        // source of truth — via `spaced_ladder_positions`, which is what actually
        // fails if the Default arm is pointed at the reduced table.
        for n in 1..=SIMULCAST_MAX_LAYERS {
            let resolved = simulcast_layers(n);
            let expected_positions = spaced_ladder_positions(n, SIMULCAST_VIDEO_LAYERS.len());
            assert_eq!(
                resolved.len(),
                expected_positions.len(),
                "default ladder depth must match the spaced selection at n={n}"
            );
            for (rung, &pos) in resolved.iter().zip(expected_positions.iter()) {
                let src = &SIMULCAST_VIDEO_LAYERS[pos];
                assert_eq!(
                    (
                        rung.label,
                        rung.max_width,
                        rung.max_height,
                        rung.target_fps,
                        rung.ideal_bitrate_kbps
                    ),
                    (
                        src.label,
                        src.max_width,
                        src.max_height,
                        src.target_fps,
                        src.ideal_bitrate_kbps
                    ),
                    "flag-off must resolve rungs from SIMULCAST_VIDEO_LAYERS at n={n}"
                );
            }
        }
        // And the DEFAULT top rung must still be 720p — i.e. the gate did not
        // quietly re-cut the shipped ladder while adding the variant.
        let top = simulcast_layers(3).last().expect("3-rung ladder");
        assert_eq!(
            (top.max_width, top.max_height),
            (1280, 720),
            "flag-off must still publish the 720p top rung"
        );
    }

    #[test]
    fn reduced_ladder_n2_keeps_the_middle_skip_but_shrinks_the_cliff() {
        // With `spaced_ladder_positions` anchoring base+top, n=2 skips the interior
        // on BOTH ladders; what changes is the SIZE of the resulting gap. NOTE the
        // units: the pixel-AREA ratio is 16x -> 9x, while 68.6x -> 38.6x is
        // THROUGHPUT (Mpx/s, folding in 30 vs 7 fps) — the "69x" quoted in the #2143
        // analysis is the latter. Both are asserted below.
        //
        // This narrows what a top-rung receiver PAYS (540p instead of 720p); it does
        // NOT change which rung the #1256 lid selects — `size_cap_layer` never
        // consults the top rung's height (proved exhaustively by
        // `size_cap_layer_is_insensitive_to_the_reduced_ladder_top_rung`).
        let d = simulcast_layers_for(2, LadderVariant::Default);
        let r = simulcast_layers_for(2, LadderVariant::Reduced);
        assert_eq!(d.len(), 2, "n=2 must yield exactly 2 rungs");
        assert_eq!(r.len(), 2, "n=2 must yield exactly 2 rungs");
        // Same base on both.
        assert_eq!((d[0].max_width, d[0].max_height), (320, 180));
        assert_eq!((r[0].max_width, r[0].max_height), (320, 180));
        // Different tops: 720p vs 540p.
        assert_eq!((d[1].max_width, d[1].max_height), (1280, 720));
        assert_eq!((r[1].max_width, r[1].max_height), (960, 540));
        // Cliff-size claims, checked rather than asserted in prose. NOTE the two
        // ratios are different numbers and are routinely confused: the widely
        // quoted "69x" for `[180p, 720p]` is the DECODE/ENCODE THROUGHPUT ratio
        // (Mpx/s, which folds in fps 7 vs 30), while the PIXEL-AREA ratio is 16x.
        // Both are pinned so neither figure can drift unnoticed.
        let px = |t: &VideoQualityTier| (t.max_width as f64) * (t.max_height as f64);
        let mpx = |t: &VideoQualityTier| px(t) * (t.target_fps as f64) / 1e6;

        // Pixel area: 921_600/57_600 = 16.0x  vs  518_400/57_600 = 9.0x
        let d_px_ratio = px(&d[1]) / px(&d[0]);
        let r_px_ratio = px(&r[1]) / px(&r[0]);
        assert!(
            (d_px_ratio - 16.0).abs() < 0.1,
            "default n=2 pixel-area cliff should be 16.0x, got {d_px_ratio:.1}x"
        );
        assert!(
            (r_px_ratio - 9.0).abs() < 0.1,
            "reduced n=2 pixel-area cliff should be 9.0x, got {r_px_ratio:.1}x"
        );

        // Throughput (the "69x" figure): 27.65/0.40 = 68.6x  vs  15.55/0.40 = 38.6x
        let d_mpx_ratio = mpx(&d[1]) / mpx(&d[0]);
        let r_mpx_ratio = mpx(&r[1]) / mpx(&r[0]);
        assert!(
            d_mpx_ratio > 60.0,
            "default n=2 throughput cliff should be ~69x, got {d_mpx_ratio:.1}x"
        );
        assert!(
            r_mpx_ratio < 45.0,
            "reduced n=2 throughput cliff should be ~39x, got {r_mpx_ratio:.1}x"
        );
        assert!(
            r_mpx_ratio < d_mpx_ratio * 0.6,
            "reduced variant must materially shrink the n=2 throughput cliff"
        );
    }

    #[test]
    fn reduced_ladder_cuts_the_dominant_encode_cost() {
        // The justification for touching the TOP rung rather than the floor: the
        // top is ~88% of the 3-layer encode cost. Compute Mpx/s from the
        // production tables so a future retune that moves the cost elsewhere (or
        // that "reduces" the ladder without reducing cost) fails here.
        let mpx = |t: &VideoQualityTier| {
            (t.max_width as f64) * (t.max_height as f64) * (t.target_fps as f64) / 1e6
        };
        let d: f64 = simulcast_layers(3).iter().map(mpx).sum();
        let r: f64 = simulcast_layers_for(3, LadderVariant::Reduced)
            .iter()
            .map(mpx)
            .sum();
        assert!(
            r < d * 0.7,
            "reduced ladder must cut total encode Mpx/s by >30% (default {d:.1}, reduced {r:.1})"
        );
        // And the top rung must still dominate its own ladder (it is the lever).
        let top_share = mpx(simulcast_layers(3).last().unwrap()) / d;
        assert!(
            top_share > 0.8,
            "default top rung should be >80% of encode cost, got {:.1}%",
            top_share * 100.0
        );
    }

    /// The RECEIVER-side saving, pinned to the tables the same way the publisher's
    /// encode saving is (issue #1768).
    ///
    /// A receiver's cost is set by the rung it actually decodes, and the reduced
    /// ladder's whole receiver-side benefit is that a peer forced to the TOP rung
    /// decodes 540p instead of 720p. On the low-power devices this project targets
    /// (no hardware VP8/VP9 decode) software decode is the binding constraint, so
    /// this is the headline receiver result — and it was previously asserted in prose
    /// with no number behind it.
    ///
    /// MUTATION: raise the reduced top rung back toward 720p (or drop its fps) and
    /// the ratio assertion fails.
    #[test]
    fn reduced_ladder_cuts_top_rung_decode_cost() {
        let px = |t: &VideoQualityTier| (t.max_width as f64) * (t.max_height as f64);
        let mpx = |t: &VideoQualityTier| px(t) * (t.target_fps as f64) / 1e6;

        let d_top = simulcast_layers(3).last().expect("default top rung");
        let r_top = simulcast_layers_for(3, LadderVariant::Reduced)
            .last()
            .expect("reduced top rung");

        // Per-frame decode work: 960x540 is 0.5625x the pixels of 1280x720, i.e.
        // ~43.8% less. Allow a small band so a future retune to another sane 540p-ish
        // geometry still passes, while a regression toward 720p fails.
        let ratio = px(r_top) / px(d_top);
        assert!(
            ratio < 0.60,
            "the reduced top rung must cut per-frame decode work by >40% (ratio {ratio:.4})"
        );

        // Sustained decode throughput at each rung's own fps.
        assert!(
            mpx(r_top) < mpx(d_top) * 0.60,
            "reduced top rung decode throughput must be <60% of default (default \
             {:.2} Mpx/s, reduced {:.2} Mpx/s)",
            mpx(d_top),
            mpx(r_top)
        );

        // Downlink for that rung falls too, which is the other half of the
        // receiver-side win on a constrained link.
        assert!(
            r_top.ideal_bitrate_kbps < d_top.ideal_bitrate_kbps,
            "the reduced top rung must also cost less downlink ({} vs {} kbps)",
            r_top.ideal_bitrate_kbps,
            d_top.ideal_bitrate_kbps
        );
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

    /// Pins the SCREEN simulcast rung selection (issue #989, retargeted by
    /// #2179) against LITERAL labels AND LITERAL resolutions, so the ladder
    /// cannot silently repoint when a tier is inserted into
    /// `SCREEN_QUALITY_TIERS`.
    ///
    /// Mutation guards:
    /// - reverting `simulcast_screen_layer_labels(3)` to the pre-#2179
    ///   `[low, medium, high]` makes the `l3` label assert AND the 2560x1440
    ///   resolution assert fail;
    /// - dropping the label indirection back to hard-coded indices `[2,1,0]`
    ///   (which now name `high`/`1440p`/`native`) fails every assert here.
    #[test]
    fn test_simulcast_screen_layers_labels_and_ordering() {
        // n=1 → [low]; n=2 → [low, high]; n=3 → [low, high, 1440p].
        let l1 = simulcast_screen_layers(1);
        assert_eq!(l1.len(), 1);
        assert_eq!(l1[0].label, "low");
        assert_eq!((l1[0].max_width, l1[0].max_height), (1280, 720));

        let l2 = simulcast_screen_layers(2);
        assert_eq!(l2.len(), 2);
        assert_eq!([l2[0].label, l2[1].label], ["low", "high"]);
        // The #1553 cold-start seed rides this ladder: base 720p/500 + top
        // 1080p/2500. #2179 must not have changed it.
        assert_eq!((l2[0].max_width, l2[0].max_height), (1280, 720));
        assert_eq!((l2[1].max_width, l2[1].max_height), (1920, 1080));
        assert_eq!(
            l2[0].ideal_bitrate_kbps + l2[1].ideal_bitrate_kbps,
            3000,
            "the 2-rung seed's cost must stay at the pre-#2179 ≈3000 kbps"
        );

        let l3 = simulcast_screen_layers(3);
        assert_eq!(l3.len(), 3);
        assert_eq!(
            [l3[0].label, l3[1].label, l3[2].label],
            ["low", "high", "1440p"]
        );
        // The top rung must actually be able to carry a DPR-2 Retina window
        // (2496x1440) without downscaling it — the whole point of #2179.
        assert_eq!((l3[2].max_width, l3[2].max_height), (2560, 1440));
        // Prefix property: the 3-rung ladder is the 2-rung ladder plus one on
        // top, so earning the third rung never re-points an already-published
        // one. (The old [low, medium, high] moved layer 1 from high → medium.)
        assert_eq!([l3[0].label, l3[1].label], [l2[0].label, l2[1].label]);
        // Bitrate ideals must be non-decreasing lowest→highest.
        assert!(l3[0].ideal_bitrate_kbps <= l3[1].ideal_bitrate_kbps);
        assert!(l3[1].ideal_bitrate_kbps <= l3[2].ideal_bitrate_kbps);
        // Resolutions must be strictly increasing lowest→highest, which the old
        // ladder violated (low and medium were both 1280x720).
        assert!(
            (l3[0].max_width as u64 * l3[0].max_height as u64)
                < (l3[1].max_width as u64 * l3[1].max_height as u64)
        );
        assert!(
            (l3[1].max_width as u64 * l3[1].max_height as u64)
                < (l3[2].max_width as u64 * l3[2].max_height as u64)
        );
    }

    /// The rungs the simulcast ladder names must all EXIST in
    /// `SCREEN_QUALITY_TIERS`. `screen_tier_index_by_label` falls back to the
    /// worst rung on a miss (deliberately conservative), so a typo'd or removed
    /// label would otherwise degrade silently to a triple-`low` ladder instead
    /// of failing.
    #[test]
    fn simulcast_screen_layer_labels_all_resolve_to_real_tiers() {
        for n in 1..=3 {
            for label in simulcast_screen_layer_labels(n) {
                assert!(
                    SCREEN_QUALITY_TIERS.iter().any(|t| t.label == *label),
                    "simulcast_screen_layer_labels({n}) names '{label}', which is \
                     not a SCREEN_QUALITY_TIERS label"
                );
            }
        }
    }

    #[test]
    #[should_panic(expected = "n must be in")]
    fn test_simulcast_screen_layers_rejects_zero() {
        let _ = simulcast_screen_layers(0);
    }

    #[test]
    fn test_screen_budget_caps_active_sum() {
        // The budget cap is ladder-agnostic; verify it works over the SCREEN
        // ladder. 3-layer ideals (#2179): low 500 + high 2500 + 1440p 5000 = 8000.
        let tiers = simulcast_screen_layers(3);
        let budget = uplink_budget_kbps(tiers, 3);
        assert_eq!(budget, 8000.0);
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

    /// Resolve the expected constrained answers by LABEL, so this test asserts
    /// "poor RTT ⇒ the 720p/5fps rung" rather than "poor RTT ⇒ index 2". If
    /// `initial_screen_tier` regressed to the pre-#2179 hard-coded `1`/`2`, it
    /// would return the `1440p`/`high` rungs and these asserts would fail.
    fn medium_idx() -> usize {
        screen_tier_index_by_label(SCREEN_TIER_LABEL_BASELINE)
    }
    fn low_idx() -> usize {
        screen_tier_index_by_label(SCREEN_TIER_LABEL_FLOOR)
    }

    #[test]
    fn initial_screen_tier_cold_start_imposes_no_floor() {
        // No signals at all → no network-imposed floor; the source term decides.
        assert_eq!(initial_screen_tier(None, None), 0);
    }

    #[test]
    fn initial_screen_tier_good_rtt_good_camera_imposes_no_floor() {
        // RTT well below FAIR threshold, camera not degraded → no floor.
        assert_eq!(initial_screen_tier(Some(50.0), Some(1)), 0);
        assert_eq!(initial_screen_tier(Some(RTT_GOOD_MS), Some(2)), 0);
    }

    #[test]
    fn initial_screen_tier_fair_rtt_returns_medium() {
        // RTT exactly at FAIR threshold → medium tier, regardless of camera.
        assert_eq!(
            initial_screen_tier(Some(RTT_FAIR_MS), Some(0)),
            medium_idx()
        );
        assert_eq!(initial_screen_tier(Some(RTT_FAIR_MS), None), medium_idx());
        // Above FAIR but below POOR → still medium.
        assert_eq!(initial_screen_tier(Some(300.0), Some(1)), medium_idx());
        // …and "medium" must still be the 720p/8fps rung it always was.
        assert_eq!(
            (
                SCREEN_QUALITY_TIERS[medium_idx()].max_width,
                SCREEN_QUALITY_TIERS[medium_idx()].max_height,
                SCREEN_QUALITY_TIERS[medium_idx()].target_fps,
            ),
            (1280, 720, 8),
            "a fair-RTT client must not be promoted above 720p/8fps by #2179"
        );
    }

    #[test]
    fn initial_screen_tier_poor_rtt_returns_low() {
        // RTT at or above POOR threshold → low tier regardless of camera.
        assert_eq!(initial_screen_tier(Some(RTT_POOR_MS), Some(0)), low_idx());
        assert_eq!(initial_screen_tier(Some(RTT_POOR_MS), None), low_idx());
        assert_eq!(initial_screen_tier(Some(1000.0), Some(2)), low_idx());
        // …and "low" must still be the 720p/5fps/500kbps floor.
        assert_eq!(
            (
                SCREEN_QUALITY_TIERS[low_idx()].max_width,
                SCREEN_QUALITY_TIERS[low_idx()].max_height,
                SCREEN_QUALITY_TIERS[low_idx()].target_fps,
                SCREEN_QUALITY_TIERS[low_idx()].ideal_bitrate_kbps,
            ),
            (1280, 720, 5, 500),
            "a poor-RTT client must not be promoted above 720p/5fps by #2179"
        );
        assert_eq!(
            low_idx(),
            SCREEN_QUALITY_TIERS.len() - 1,
            "the poor-RTT answer must be the worst rung on the ladder"
        );
    }

    #[test]
    fn initial_screen_tier_degraded_camera_no_rtt_returns_medium() {
        // Camera already at sd (3) or low (4) tier, RTT unknown → medium.
        assert_eq!(initial_screen_tier(None, Some(3)), medium_idx());
        assert_eq!(initial_screen_tier(None, Some(4)), medium_idx());
    }

    #[test]
    fn initial_screen_tier_good_rtt_degraded_camera_returns_medium() {
        // Good RTT but camera already degraded → conservative medium tier.
        assert_eq!(initial_screen_tier(Some(50.0), Some(3)), medium_idx());
        assert_eq!(
            initial_screen_tier(Some(RTT_GOOD_MS), Some(4)),
            medium_idx()
        );
    }

    #[test]
    fn initial_screen_tier_camera_only_not_degraded_imposes_no_floor() {
        // Camera not degraded (tier ≤ 2), no RTT → no floor.
        assert_eq!(initial_screen_tier(None, Some(0)), 0);
        assert_eq!(initial_screen_tier(None, Some(2)), 0);
    }

    // =====================================================================
    // Issue #2179: source-resolution-driven initial screen tier
    // =====================================================================

    /// `DEFAULT_SCREEN_TIER_INDEX` must keep naming the `medium` rung after the
    /// ladder was extended. The `const _: () = assert!(...)` next to the
    /// constant already fails the BUILD on a drift, so this test additionally
    /// pins the rung's actual numbers — a rename of the label alone (say
    /// `medium` → `720p8`) would keep the const assert honest only if the
    /// constant is updated in lockstep, and this catches a retune of the rung
    /// itself.
    #[test]
    fn default_screen_tier_index_is_the_medium_720p_rung() {
        let t = &SCREEN_QUALITY_TIERS[DEFAULT_SCREEN_TIER_INDEX];
        assert_eq!(t.label, SCREEN_TIER_LABEL_BASELINE);
        assert_eq!((t.max_width, t.max_height, t.target_fps), (1280, 720, 8));
    }

    /// `screen_tier_for_source` must return the WORST rung that still contains
    /// the source, i.e. the cheapest tier that costs the source ZERO
    /// resolution. Every case below is a literal (source → rung label) pair, so
    /// a mutation of the search direction (`.rev()` dropped → returns the best
    /// rung instead of the worst) or of the containment test (`>=` → `>`)
    /// changes at least one answer.
    #[test]
    fn screen_tier_for_source_picks_cheapest_rung_that_fits() {
        let label_for = |w, h| SCREEN_QUALITY_TIERS[screen_tier_for_source(w, h)].label;

        // The issue #2179 case: a DPR-2 Retina window (1248x720 CSS px).
        // MUST NOT land on a 720p rung — that is the reported defect.
        assert_eq!(label_for(2496, 1440), "1440p");
        let idx = screen_tier_for_source(2496, 1440);
        assert!(
            SCREEN_QUALITY_TIERS[idx].max_width >= 2496
                && SCREEN_QUALITY_TIERS[idx].max_height >= 1440,
            "the chosen rung must contain the Retina source without downscaling"
        );
        assert_ne!(
            (
                SCREEN_QUALITY_TIERS[idx].max_width,
                SCREEN_QUALITY_TIERS[idx].max_height
            ),
            (1280, 720),
            "2496x1440 must never resolve to a 720p rung"
        );

        // Exactly equal to a rung cap → that rung (guards `>=` → `>`).
        assert_eq!(label_for(2560, 1440), "1440p");
        assert_eq!(label_for(1920, 1080), "high");
        assert_eq!(label_for(3840, 2160), "native");

        // One pixel over a rung cap → the next rung up (guards an off-by-one).
        assert_eq!(label_for(2561, 1440), "native");
        assert_eq!(label_for(2560, 1441), "native");
        assert_eq!(label_for(1921, 1080), "1440p");

        // 21:9 ultra-wide (the issue #1973 M3 Pro source): height fits every
        // rung, width only fits `native`.
        assert_eq!(label_for(3840, 1600), "native");

        // Beyond the top rung (a 5K Retina panel) → clamp to the best rung; the
        // downscale there is unavoidable.
        assert_eq!(screen_tier_for_source(5120, 2880), 0);

        // A small source picks the CHEAPEST containing rung, which for 720p is
        // the ladder floor. (`resolve_initial_screen_tier` is what stops a
        // share from actually STARTING there — see the test below.)
        assert_eq!(label_for(1280, 720), "low");
        assert_eq!(
            screen_tier_for_source(1280, 720),
            SCREEN_QUALITY_TIERS.len() - 1
        );
    }

    /// Unknown source dimensions must fall back to the pre-#2179 baseline, not
    /// to a fabricated rung.
    /// Issue #2179 review: a PORTRAIT surface must select a rung by its LONG and
    /// SHORT edges, not per-axis.
    ///
    /// Mutation guard: restore the per-axis compare
    /// (`tier.max_width >= src_w && tier.max_height >= src_h`) and a 1440x2560
    /// panel matches NO rung, falling through to index 0 (`native`) — a 720p-class
    /// stream billed at 8000 kbps. Both assertions below fail.
    #[test]
    fn screen_tier_for_source_is_orientation_agnostic() {
        let label_for = |w, h| SCREEN_QUALITY_TIERS[screen_tier_for_source(w, h)].label;

        // A rotated 1440p panel is the `1440p` rung's box with the axes swapped.
        assert_eq!(label_for(1440, 2560), "1440p");
        // …and it must pick the SAME rung as its landscape twin.
        assert_eq!(
            screen_tier_for_source(1440, 2560),
            screen_tier_for_source(2560, 1440)
        );

        // Portrait 1080p / 720p likewise.
        assert_eq!(label_for(1080, 1920), "high");
        assert_eq!(label_for(720, 1280), "low");

        // A portrait surface larger than the top rung on its long edge still
        // returns the top rung (index 0) — that box genuinely binds.
        assert_eq!(screen_tier_for_source(2160, 4320), 0);
    }

    #[test]
    fn screen_tier_for_source_unknown_dims_fall_back_to_default() {
        assert_eq!(screen_tier_for_source(0, 0), DEFAULT_SCREEN_TIER_INDEX);
        assert_eq!(screen_tier_for_source(1920, 0), DEFAULT_SCREEN_TIER_INDEX);
        assert_eq!(screen_tier_for_source(0, 1080), DEFAULT_SCREEN_TIER_INDEX);
    }

    /// `resolve_initial_screen_tier` composes the source term with the network
    /// floor. Each assertion below breaks under a specific mutation:
    /// - dropping `min(..., index_of("high"))` starts a 720p share on the `low`
    ///   rung (a bitrate/fps regression) — caught by the 1280x720 case;
    /// - dropping `max(..., network_tier)` lets a poor-RTT client start at the
    ///   source resolution — caught by the constrained-network cases;
    /// - dropping the source term entirely pins everything at `high` — caught by
    ///   the Retina and 4K cases;
    /// - swapping `min`/`max` breaks all of them.
    #[test]
    fn resolve_initial_screen_tier_composes_source_and_network() {
        let label_at = |idx: usize| SCREEN_QUALITY_TIERS[idx].label;
        let medium = medium_idx();
        let low = low_idx();

        // Healthy network (no floor) + Retina source → the source's own rung.
        assert_eq!(
            label_at(resolve_initial_screen_tier(2496, 1440, 0)),
            "1440p",
            "a healthy link sharing a Retina window must start at its own resolution"
        );
        // Healthy network + 4K panel / 21:9 ultra-wide → the native rung.
        assert_eq!(
            label_at(resolve_initial_screen_tier(3840, 2160, 0)),
            "native"
        );
        assert_eq!(
            label_at(resolve_initial_screen_tier(3840, 1600, 0)),
            "native"
        );
        // Healthy network + 1080p source → the 1080p rung (no downscale).
        assert_eq!(label_at(resolve_initial_screen_tier(1920, 1080, 0)), "high");

        // Healthy network + SMALL source → clamped UP to the 1080p rung, which
        // is exactly the pre-#2179 answer: the encoder still emits 1280x720 (the
        // fit never upscales) but with `high`'s 2500 kbps / 10 fps budget rather
        // than the `low` rung's 500 kbps / 5 fps.
        assert_eq!(label_at(resolve_initial_screen_tier(1280, 720, 0)), "high");
        assert_eq!(label_at(resolve_initial_screen_tier(800, 600, 0)), "high");

        // Constrained network vetoes the source term — a 4K sharer on a poor
        // link still starts at `low`, exactly as before #2179.
        assert_eq!(resolve_initial_screen_tier(3840, 2160, low), low);
        assert_eq!(resolve_initial_screen_tier(2496, 1440, medium), medium);
        assert_eq!(resolve_initial_screen_tier(1280, 720, low), low);

        // Unknown source dims → the pre-#2179 optimistic answer (`high`), NOT a
        // fabricated rung and not the conservative floor.
        assert_eq!(label_at(resolve_initial_screen_tier(0, 0, 0)), "high");
        assert_eq!(label_at(resolve_initial_screen_tier(1920, 0, 0)), "high");
        assert_eq!(resolve_initial_screen_tier(0, 0, low), low);

        // Result is always a valid index, even for a nonsense network floor.
        assert_eq!(
            resolve_initial_screen_tier(2496, 1440, 999),
            SCREEN_QUALITY_TIERS.len() - 1
        );
    }

    /// THE ISSUE-REPORTER CASE (issue #2179, review round 2).
    ///
    /// A DPR-2 Retina laptop shares a 1248x720 CSS window = 2496x1440 real
    /// pixels. Apple-Silicon M-series report 8–12 logical cores, so an 8-core
    /// machine is the modal reporter. It MUST reach the `1440p` rung — anything
    /// less re-introduces the double-downscale (2496x1440 → 1920x1080 → 1280x720)
    /// that is the entire bug.
    ///
    /// Mutation guard: gate `1440p` behind `SCREEN_TIER_NATIVE_MIN_CORES` again
    /// (the first cut's calibration, which tied the tier ceiling to the 3-LAYER
    /// bar) and this resolves to `high` — i.e. the fix silently misses its own
    /// bug report on most consumer hardware.
    #[test]
    fn retina_laptop_reaches_the_1440p_rung_on_consumer_core_counts() {
        let label_at = |i: usize| SCREEN_QUALITY_TIERS[i].label;
        for cores in [SCREEN_TIER_1440P_MIN_CORES, 7, 8, 9] {
            assert_eq!(
                label_at(resolve_screen_tier_ceiling(2496, 1440, cores, 3)),
                "1440p",
                "a {cores}-core Retina sender must reach the 1440p rung"
            );
        }
        // One core below the bar it is still held at 1080p.
        assert_eq!(
            label_at(resolve_screen_tier_ceiling(
                2496,
                1440,
                SCREEN_TIER_1440P_MIN_CORES - 1,
                3
            )),
            "high"
        );
    }

    /// The TIER ceiling bars must not be silently re-derived from the LAYER
    /// bars: they are separate calibrations that happen to share numbers today.
    /// This pins the CURRENT mapping so a retune of either is a deliberate edit.
    #[test]
    fn tier_ceiling_bars_are_calibrated_per_pixel_rate_not_per_layer_count() {
        assert_eq!(screen_tier_device_floor(SCREEN_TIER_NATIVE_MIN_CORES), 0);
        assert_eq!(
            SCREEN_QUALITY_TIERS[screen_tier_device_floor(SCREEN_TIER_1440P_MIN_CORES)].label,
            "1440p"
        );
        assert_eq!(
            SCREEN_QUALITY_TIERS[screen_tier_device_floor(SCREEN_TIER_1440P_MIN_CORES - 1)].label,
            "high"
        );
        assert_eq!(
            SCREEN_QUALITY_TIERS[screen_tier_device_floor(0)].label,
            "high"
        );
        // Monotone in cores: more CPU may never mean a WORSE ceiling.
        let mut prev = usize::MAX;
        for cores in 0..24u32 {
            let f = screen_tier_device_floor(cores);
            assert!(
                f <= prev,
                "device floor must be monotone non-increasing in cores"
            );
            prev = f;
        }
    }

    /// Issue #2179 review r2/r3: the Cause hint must name the term that is
    /// UNIQUELY responsible for holding a share below the rung its own source
    /// needs — silence is only honest when nothing is being withheld, and
    /// naming a term that would not change the outcome is worse than silence.
    ///
    /// Mutation guards:
    /// - return `""` unconditionally and every non-empty assertion fails
    ///   (the false-negative the review round exists to fix);
    /// - drop the `device > always` arm and the 4-core case reports
    ///   `ladder-limited`, hiding a real CPU cap one rung worse;
    /// - compare `device > source` instead of `device > always` and the
    ///   8-core case blames the CPU for a ladder cap it cannot lift.
    #[test]
    fn screen_ceiling_cause_reports_the_uniquely_responsible_term() {
        // A 4K source wants `native`, which NO path publishes — the ladder tops
        // out at `1440p`. That is the binding term on a capable machine…
        assert_eq!(
            screen_ceiling_cause(3840, 2160, SCREEN_TIER_NATIVE_MIN_CORES, 3),
            SCREEN_CAUSE_LADDER
        );
        // …and it stays the binding term on an 8-core machine, because the
        // device class caps at the SAME rung the ladder already does. Blaming
        // the CPU here would send the user optimising something that cannot
        // help them.
        assert_eq!(screen_ceiling_cause(3840, 2160, 8, 3), SCREEN_CAUSE_LADDER);
        // Drop below the 1440p tier bar and the DEVICE really is the binding
        // term: it caps at `high`, one rung worse than the ladder would.
        assert_eq!(screen_ceiling_cause(3840, 2160, 4, 3), SCREEN_CAUSE_CPU);
        assert_eq!(screen_ceiling_cause(3840, 2160, 2, 1), SCREEN_CAUSE_CPU);

        // A source at the rung its own pixels need withholds nothing, whatever
        // the machine or the stream count.
        assert_eq!(screen_ceiling_cause(2496, 1440, 8, 3), "");
        assert_eq!(screen_ceiling_cause(1920, 1080, 2, 1), "");
        assert_eq!(
            screen_ceiling_cause(1920, 1080, SCREEN_TIER_NATIVE_MIN_CORES, 3),
            ""
        );
        assert_eq!(screen_ceiling_cause(1280, 720, 4, 1), "");
        // Unknown source dims never manufacture a cause.
        assert_eq!(screen_ceiling_cause(0, 0, 0, 1), "");

        // ── The SINGLE-STREAM term is currently SUBSUMED ────────────────────
        // Its cap and the ladder cap are both `1440p` today, so it never binds
        // alone and `SCREEN_CAUSE_SINGLE_STREAM` is unreachable. Pinned as an
        // EQUALITY rather than deleted: if the publish ladder ever gains a rung
        // above `1440p`, this assertion flips and whoever raised it learns that
        // the single-stream path starts binding again.
        assert_eq!(
            screen_tier_single_stream_floor(SCREEN_TIER_NATIVE_MIN_CORES),
            screen_ladder_top_index(),
            "single-stream cap and ladder cap coincide today — if this fails, \
             SCREEN_CAUSE_SINGLE_STREAM has become reachable and needs coverage"
        );
        assert_eq!(
            screen_ceiling_cause(3840, 2160, SCREEN_TIER_NATIVE_MIN_CORES, 1),
            SCREEN_CAUSE_LADDER,
            "while the two caps coincide the LADDER is the honest explanation"
        );
    }

    /// Issue #2179 review r3 (security): no path may encode better than the
    /// publish ladder's own top rung, because the simulcast BASE rung takes its
    /// geometry from the AQ tier while its budget comes from ladder rung 0 —
    /// a tier above the ladder made the base rung encode 3840x2160 at 500 kbps,
    /// and the base rung is exactly what a struggling receiver falls back to.
    ///
    /// Mutation guard: drop the `screen_ladder_top_index()` term from
    /// `resolve_screen_tier_ceiling` and the 4K/capable/simulcast case resolves
    /// to index 0 (`native`), failing here.
    #[test]
    fn no_path_reaches_a_rung_better_than_the_publish_ladder_top() {
        let top = screen_ladder_top_index();
        assert_eq!(
            SCREEN_QUALITY_TIERS[top].label, "1440p",
            "the ladder's top rung is what bounds every encode path"
        );

        for &(w, h) in &[(3840u32, 2160u32), (5120, 2880), (2496, 1440), (1920, 1080)] {
            for cores in [0u32, 4, 6, 8, 10, 16, 64] {
                for layers in [1u32, 2, 3] {
                    let ceiling = resolve_screen_tier_ceiling(w, h, cores, layers);
                    assert!(
                        ceiling >= top,
                        "{w}x{h} cores={cores} layers={layers} resolved to {ceiling}, \
                         better than the ladder top {top}"
                    );
                }
            }
        }

        // Stated plainly: the `native` rung is unreachable as an ENCODE tier.
        // It survives only as the CAPTURE-ceiling donor, which is why it is
        // still index 0 of the table.
        assert_eq!(
            SCREEN_QUALITY_TIERS[0].label, "native",
            "rung 0 still donates the getDisplayMedia capture ceiling"
        );
        assert!(
            resolve_screen_tier_ceiling(3840, 2160, 64, 3) > 0,
            "no configuration may reach the native rung as an encode tier"
        );
    }

    /// Whenever [`screen_ceiling_cause`] is silent the ceiling must equal the
    /// source-only rung, and whenever it speaks the ceiling must be strictly
    /// worse. The two functions are read together at every stamp site, so they
    /// must never disagree.
    #[test]
    fn screen_ceiling_cause_agrees_with_resolve_screen_tier_ceiling() {
        for &(w, h) in &[
            (1280u32, 720u32),
            (1920, 1080),
            (2496, 1440),
            (3840, 2160),
            (0, 0),
        ] {
            for cores in [0u32, 2, 4, 6, 8, 10, 16] {
                for layers in [1u32, 2, 3] {
                    let source = resolve_initial_screen_tier(w, h, 0);
                    let ceiling = resolve_screen_tier_ceiling(w, h, cores, layers);
                    let cause = screen_ceiling_cause(w, h, cores, layers);
                    if cause.is_empty() {
                        assert_eq!(
                            ceiling, source,
                            "silent cause but ceiling {ceiling} != source {source} \
                             for {w}x{h} cores={cores} layers={layers}"
                        );
                    } else {
                        assert!(
                            ceiling > source,
                            "cause '{cause}' but ceiling {ceiling} is not worse than \
                             source {source} for {w}x{h} cores={cores} layers={layers}"
                        );
                    }
                }
            }
        }
    }

    /// Issue #2179 review: the PERSISTENT ceiling composes the source term with
    /// the device class and the single-stream cap, most restrictive wins.
    ///
    /// Mutation guards, per assertion:
    /// - drop the `screen_tier_device_floor` term → the sub-bar 4K case reads
    ///   `1440p` instead of `high` on a sub-bar sender;
    /// - drop the `effective_layers <= 1` term → the single-stream 4K case reads
    ///   `native` instead of `1440p`;
    /// - drop the source term → the 720p case reads `native` instead of `high`.
    #[test]
    fn resolve_screen_tier_ceiling_composes_source_device_and_stream_count() {
        let label_at = |i: usize| SCREEN_QUALITY_TIERS[i].label;
        let capable = SCREEN_TIER_NATIVE_MIN_CORES; // whole-ladder-class sender
        let weak = SCREEN_TIER_1440P_MIN_CORES - 1; // below the 1440p tier bar

        // Capable device, 3 rungs, 4K source: the LADDER top, not `native` —
        // no encode path reaches rung 0 (issue #2179 review r3).
        assert_eq!(
            label_at(resolve_screen_tier_ceiling(3840, 2160, capable, 3)),
            "1440p"
        );
        // Same source one core BELOW the native bar: also `1440p` — and NOT
        // `high`, which is what tying the tier ceiling to the 3-layer bar used
        // to produce.
        assert_eq!(
            label_at(resolve_screen_tier_ceiling(3840, 2160, capable - 1, 3)),
            "1440p"
        );
        // Below the 1440p tier bar it drops to `high`.
        assert_eq!(
            label_at(resolve_screen_tier_ceiling(3840, 2160, weak, 3)),
            "high"
        );
        // Capable device but SINGLE-STREAM: also `1440p`. Its own cap and the
        // ladder cap coincide today, so this asserts the RESULT rather than
        // which term produced it — see
        // `screen_ceiling_cause_reports_the_uniquely_responsible_term`.
        assert_eq!(
            label_at(resolve_screen_tier_ceiling(3840, 2160, capable, 1)),
            "1440p"
        );
        // Below the 1440p tier bar AND single-stream: `high`.
        assert_eq!(
            label_at(resolve_screen_tier_ceiling(3840, 2160, weak, 1)),
            "high"
        );
        // A small source is capped by its OWN size even on the best hardware —
        // this is the "720p share may not climb to the 8000 kbps rung" rule.
        assert_eq!(
            label_at(resolve_screen_tier_ceiling(1280, 720, capable, 3)),
            "high"
        );
        // A 1440p Retina window on a capable 3-rung sender reaches `1440p`.
        assert_eq!(
            label_at(resolve_screen_tier_ceiling(2496, 1440, capable, 3)),
            "1440p"
        );
        // Unknown cores (navigator.hardwareConcurrency absent) is the most
        // conservative class, exactly like the UI capability sniff.
        assert_eq!(
            label_at(resolve_screen_tier_ceiling(3840, 2160, 0, 3)),
            "high"
        );
        // Never out of bounds.
        assert!(resolve_screen_tier_ceiling(1, 1, 0, 1) < SCREEN_QUALITY_TIERS.len());
    }

    /// The ceiling is a FLOOR on the index, so it can never be BETTER than what
    /// the source alone would have allowed — i.e. it only ever adds restriction.
    #[test]
    fn resolve_screen_tier_ceiling_never_loosens_the_source_term() {
        for &(w, h) in &[
            (1280u32, 720u32),
            (1920, 1080),
            (2496, 1440),
            (3840, 2160),
            (0, 0),
        ] {
            for cores in [0u32, 4, 6, 8, 10, 16] {
                for layers in [1u32, 2, 3] {
                    let source_only = resolve_initial_screen_tier(w, h, 0);
                    assert!(
                        resolve_screen_tier_ceiling(w, h, cores, layers) >= source_only,
                        "ceiling loosened the source term for {w}x{h} cores={cores} layers={layers}"
                    );
                }
            }
        }
    }

    /// The composed start must NEVER be better than what the pure network
    /// signals allow, and NEVER worse than the pre-#2179 start. Together these
    /// are the "do not regress anyone" contract, checked across the whole ladder
    /// for a range of real sources.
    ///
    /// The pre-#2179 start was `initial_screen_tier`'s answer mapped onto the
    /// old 3-rung table: `0 → high`, `1 → medium`, `2 → low`. Because the
    /// constrained answers are now label-resolved to those same rungs, the
    /// pre-#2179 start is simply `max(network_floor, index_of("high"))`.
    #[test]
    fn resolve_initial_screen_tier_brackets_the_pre_2179_start() {
        let sources = [
            (1280u32, 720u32),
            (1920, 1080),
            (2496, 1440),
            (2560, 1440),
            (3840, 1600),
            (3840, 2160),
            (5120, 2880),
            (0, 0),
        ];
        let high = screen_tier_index_by_label(SCREEN_TIER_LABEL_1080P);
        for floor in 0..SCREEN_QUALITY_TIERS.len() {
            let pre_2179 = floor.max(high);
            for (w, h) in sources {
                let resolved = resolve_initial_screen_tier(w, h, floor);
                assert!(
                    resolved >= floor,
                    "{w}x{h} with network floor {floor} resolved to {resolved}, \
                     which is a BETTER tier than the network allows"
                );
                assert!(
                    resolved <= pre_2179,
                    "{w}x{h} with network floor {floor} resolved to {resolved}, \
                     which is WORSE than the pre-#2179 start ({pre_2179})"
                );
                assert!(resolved < SCREEN_QUALITY_TIERS.len());
            }
        }
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

    // =====================================================================
    // Issue #1531: tier- and transport-aware camera periodic-keyframe ceiling
    // =====================================================================

    /// Pins the per-tier selection over WebTransport (the lossy, primary path):
    /// full_hd … low keep the flat 5s #1510 guarantee, and ONLY the two lowest
    /// tiers relax (very_low → 7s, minimal → 8s). Calls the production
    /// [`camera_periodic_keyframe_max_interval_ms`] so a mutation to the selection
    /// (flattening a relaxed tier back to the base, or relaxing a tier that must
    /// stay at 5s) fails here.
    #[test]
    fn camera_keyframe_ceiling_wt_relaxes_only_two_lowest_tiers() {
        let n = VIDEO_QUALITY_TIERS.len();
        // Every tier from full_hd (0) through `low` (n-3) keeps the base 5s over WT.
        for idx in 0..=(n - 3) {
            assert_eq!(
                camera_periodic_keyframe_max_interval_ms(idx, /* lossless */ false),
                PERIODIC_KEYFRAME_MAX_INTERVAL_MS,
                "tier {idx} over WebTransport must keep the flat #1510 5s ceiling"
            );
        }
        // very_low (n-2) relaxes to 7s.
        assert_eq!(
            camera_periodic_keyframe_max_interval_ms(n - 2, false),
            PERIODIC_KEYFRAME_MAX_INTERVAL_VERY_LOW_TIER_MS,
            "very_low over WebTransport must relax to the 7s ceiling"
        );
        // minimal (n-1) relaxes to 8s.
        assert_eq!(
            camera_periodic_keyframe_max_interval_ms(n - 1, false),
            PERIODIC_KEYFRAME_MAX_INTERVAL_MINIMAL_TIER_MS,
            "minimal over WebTransport must relax to the 8s ceiling"
        );
        // Out-of-range (defensive: a clamp bug upstream) saturates to the lowest
        // tier's relaxed value, never a tighter one.
        assert_eq!(
            camera_periodic_keyframe_max_interval_ms(n + 5, false),
            PERIODIC_KEYFRAME_MAX_INTERVAL_MINIMAL_TIER_MS,
            "an out-of-range tier index must resolve as the lowest (minimal) tier"
        );
    }

    /// Pins the TRANSPORT axis: on a lossless (WS) transport the insurance-only
    /// relief extends one tier higher — the `low` tier (n-3) relaxes to 7s where
    /// over WebTransport it stays at the flat 5s. The very_low/minimal values are
    /// transport-independent. This is the mutation guard for the
    /// `lossless_transport && tier_index == low` arm: deleting it collapses the
    /// `low`-tier WS value back to 5s and fails the first assertion.
    #[test]
    fn camera_keyframe_ceiling_lossless_transport_extends_relief_band() {
        let n = VIDEO_QUALITY_TIERS.len();
        let low = n - 3;

        // `low` differs by transport: 5s over WT, 7s over lossless WS.
        assert_eq!(
            camera_periodic_keyframe_max_interval_ms(low, false),
            PERIODIC_KEYFRAME_MAX_INTERVAL_MS,
            "the `low` tier over WebTransport must stay at the flat 5s ceiling"
        );
        assert_eq!(
            camera_periodic_keyframe_max_interval_ms(low, true),
            PERIODIC_KEYFRAME_MAX_INTERVAL_VERY_LOW_TIER_MS,
            "the `low` tier over a lossless (WS) transport must relax to 7s (insurance-only)"
        );
        // A healthy mid tier does NOT gain relief from lossless transport.
        assert_eq!(
            camera_periodic_keyframe_max_interval_ms(low - 1, true),
            PERIODIC_KEYFRAME_MAX_INTERVAL_MS,
            "a tier above `low` must keep the 5s ceiling even on a lossless transport"
        );
        // very_low / minimal are transport-independent (already relaxed on both).
        assert_eq!(
            camera_periodic_keyframe_max_interval_ms(n - 2, true),
            camera_periodic_keyframe_max_interval_ms(n - 2, false),
            "very_low ceiling must be transport-independent"
        );
        assert_eq!(
            camera_periodic_keyframe_max_interval_ms(n - 1, true),
            camera_periodic_keyframe_max_interval_ms(n - 1, false),
            "minimal ceiling must be transport-independent"
        );
    }

    /// The effective ceiling must never exceed the absolute 8s bound on any
    /// (tier, transport) combination — the #1662 receiver-interaction guard. A
    /// retune that pushed any tier past 8s fails here (belt-and-suspenders with the
    /// compile-time invariant on the constant).
    #[test]
    fn camera_keyframe_ceiling_never_exceeds_absolute_bound() {
        let n = VIDEO_QUALITY_TIERS.len();
        for idx in 0..n {
            for &lossless in &[false, true] {
                let ms = camera_periodic_keyframe_max_interval_ms(idx, lossless);
                assert!(
                    (PERIODIC_KEYFRAME_MAX_INTERVAL_MS..=8000.0).contains(&ms),
                    "tier {idx} (lossless={lossless}) ceiling {ms}ms must stay within [5s, 8s]"
                );
            }
        }
    }
}

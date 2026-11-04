// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure layout helpers extracted from `attendants.rs`.
//!
//! These functions are algorithmically non-trivial but have zero WASM / DOM /
//! Dioxus dependencies, so they can be unit-tested under plain `cargo test`.

use super::decode_budget::{
    build_decoded_bucket, expand_decoded_for_requested, pin_first_window, TileRenderMode,
};
use super::density::{DensityMode, MOBILE_WIDTH_BREAKPOINT_PX};
use super::self_view::{remote_capacity, SelfTileCounts};
use crate::constants::CANVAS_LIMIT;
use crate::context::DockPosition;
use std::collections::{HashMap, HashSet};
use std::ops::RangeInclusive;

/// Tile aspect ratio (width / height) — 3 : 2.
pub(crate) const TILE_AR: f64 = 3.0 / 2.0;

/// Google Meet–style layout: try every column count, compute the maximum
/// 3 : 2 tile size for each, and pick the variant with the largest tile area.
/// Returns `(cols, rows, tile_width)`.
pub(crate) fn compute_layout(n: usize, w: f64, h: f64, gap: f64) -> (usize, usize, f64) {
    if n == 0 {
        return (1, 1, w);
    }
    best_layout(1..=n, |cols| n.div_ceil(cols), w, h, gap)
}

fn best_layout(
    columns: RangeInclusive<usize>,
    rows_for: impl Fn(usize) -> usize,
    w: f64,
    h: f64,
    gap: f64,
) -> (usize, usize, f64) {
    let mut best_cols = 1_usize;
    let mut best_rows = 1_usize;
    let mut best_area = 0.0_f64;
    let mut best_tw = 0.0_f64;
    let ar: f64 = TILE_AR;

    for cols in columns {
        let rows = rows_for(cols);

        let avail_w = (w - (cols as f64 - 1.0) * gap).max(0.0);
        let avail_h = (h - (rows as f64 - 1.0) * gap).max(0.0);

        let mut tw = avail_w / cols as f64;
        let mut th = tw / ar;

        if th * rows as f64 > avail_h {
            th = avail_h / rows as f64;
            tw = th * ar;
        }

        let area = tw * th;
        if area > best_area {
            best_area = area;
            best_cols = cols;
            best_rows = rows;
            best_tw = tw;
        }
    }

    // Floored, not rounded: a flex line re-wraps on a sub-pixel. (issue 2700)
    (best_cols, best_rows, best_tw.floor())
}

/// Rows holding `k` leading 2x2 tiles and `n - k` single cells, densely packed.
fn enlarged_rows(n: usize, k: usize, cols: usize) -> usize {
    let bands = k.div_ceil(cols / 2);
    let free = 2 * bands * cols - 4 * k;
    2 * bands + (n - k).saturating_sub(free).div_ceil(cols)
}

/// What the grid's geometry, the enlarged tiles and the size hints all read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TilePlan {
    pub cols: usize,
    pub rows: usize,
    pub tile_w: f64,
    pub enlarged: usize,
}

impl TilePlan {
    pub fn tile_h(&self) -> f64 {
        self.tile_w / TILE_AR
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TilePlanInput {
    pub tile_count: usize,
    pub split_layout: bool,
    pub full_bleed: bool,
    pub avail_w: f64,
    pub avail_h: f64,
    pub gap: f64,
    pub vw: f64,
}

impl TilePlanInput {
    fn with_enlarged(&self, k: usize) -> Option<TilePlan> {
        if k == 0 || k > self.tile_count || self.tile_count < 2 {
            return None;
        }
        let n = self.tile_count;
        let (cols, rows, tile_w) = best_layout(
            2..=n + k,
            |cols| enlarged_rows(n, k, cols),
            self.avail_w,
            self.avail_h,
            self.gap,
        );
        let floor = DensityMode::Maximum.min_tile_width(self.vw);
        let standard = compute_layout(n, self.avail_w, self.avail_h, self.gap).2;
        (cols >= 2 && tile_w >= floor && 2.0 * tile_w + self.gap > standard).then_some(TilePlan {
            cols,
            rows,
            tile_w,
            enlarged: k,
        })
    }
}

fn plan_tiles(input: TilePlanInput, enlarged_tiles: usize) -> TilePlan {
    let wanted = if input.split_layout || input.full_bleed {
        0
    } else {
        enlarged_tiles
    };
    (1..=wanted)
        .rev()
        .find_map(|k| input.with_enlarged(k))
        .unwrap_or_else(|| {
            let (cols, rows, tile_w) =
                compute_layout(input.tile_count, input.avail_w, input.avail_h, input.gap);
            TilePlan {
                cols,
                rows,
                tile_w,
                enlarged: 0,
            }
        })
}

const ENLARGE_MIN_TILE_W_FINE_POINTER: f64 = 240.0;
const ENLARGE_MIN_TILE_W_COARSE_POINTER: f64 = 330.0;
const ENLARGE_MIN_GAIN: f64 = 1.2;

fn enlarge_offered(input: TilePlanInput, plan: TilePlan, pointer_coarse: bool) -> bool {
    let min_tile_w = if pointer_coarse {
        ENLARGE_MIN_TILE_W_COARSE_POINTER
    } else {
        ENLARGE_MIN_TILE_W_FINE_POINTER
    };
    !input.split_layout
        && !input.full_bleed
        && plan.tile_w >= min_tile_w
        && input
            .with_enlarged(plan.enlarged + 1)
            .is_some_and(|next| 2.0 * next.tile_w + input.gap >= ENLARGE_MIN_GAIN * plan.tile_w)
}

/// The grid's enlarged tiles; empty in the split.
#[derive(Debug, Default, PartialEq)]
pub struct EnlargedTiles {
    /// Painted sessions only.
    pub rank: HashMap<String, usize>,
    pub offered: bool,
}

impl EnlargedTiles {
    pub(crate) fn painted_sessions(&self) -> HashSet<u64> {
        self.rank
            .keys()
            .filter_map(|id| id.parse::<u64>().ok())
            .collect()
    }
}

/// The grid plan for this render list, and which tiles it paints enlarged.
pub fn plan_tile_grid(
    input: TilePlanInput,
    unified_tiles: &[(String, TileRenderMode)],
    enlarged_sessions: &[(String, usize)],
    pointer_coarse: bool,
) -> (TilePlan, EnlargedTiles) {
    let requested: HashMap<&str, usize> = enlarged_sessions
        .iter()
        .map(|(session, rank)| (session.as_str(), *rank))
        .collect();
    let present: Vec<(&String, usize)> = unified_tiles
        .iter()
        .filter_map(|(id, _)| requested.get(id.as_str()).map(|rank| (id, *rank)))
        .collect();
    let plan = plan_tiles(input, present.len());
    let enlarged = EnlargedTiles {
        rank: present
            .into_iter()
            .take(plan.enlarged)
            .map(|(id, rank)| (id.clone(), rank))
            .collect(),
        offered: enlarge_offered(input, plan, pointer_coarse),
    };
    (plan, enlarged)
}

/// How many remote tiles the grid places, and how many participants the +N badge
/// stands in for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GridCapacity {
    pub layout_limit: usize,
    pub displayed: usize,
    pub overflow: usize,
}

impl GridCapacity {
    pub(crate) fn tile_count(&self, counts: &SelfTileCounts) -> usize {
        self.displayed
            + usize::from(self.overflow > 0)
            + usize::from(counts.self_in_grid)
            + counts.share_cells
    }
}

/// `roster_total` is everyone in the meeting, `total_tiles` the capped population
/// that can hold a tile. A roster past the cap asks for one more cell, for its badge.
pub(crate) fn plan_grid_capacity(
    roster_total: usize,
    total_tiles: usize,
    counts: &SelfTileCounts,
    avail_w: f64,
    avail_h: f64,
    gap: f64,
    min_tile_width: f64,
) -> GridCapacity {
    let mut cells = counts.layout_count + usize::from(roster_total > total_tiles);
    while cells > 1 {
        let (_cols, _rows, tile_w) = compute_layout(cells, avail_w, avail_h, gap);
        if tile_w >= min_tile_width {
            break;
        }
        cells -= 1;
    }
    let layout_limit = remote_capacity(cells, counts);
    let (displayed, overflow) = if roster_total > layout_limit.min(total_tiles) {
        let displayed = layout_limit.saturating_sub(1).max(1);
        (displayed, roster_total - displayed)
    } else {
        (total_tiles, 0)
    };
    GridCapacity {
        layout_limit,
        displayed,
        overflow,
    }
}

/// `#grid-container`'s flow longhands: a lone tile stretches across 1fr tracks,
/// 2+ tiles wrap and centre, and enlarged tiles need real tracks to span. Every
/// arm emits the same set. (issue 2700)
pub fn tile_flow_style(tile_count: usize, plan: TilePlan) -> String {
    let (cols, rows) = (plan.cols, plan.rows);
    if plan.enlarged > 0 {
        format!(
            "display: grid; \
             grid-template-columns: repeat({cols}, {tw:.0}px); grid-template-rows: none; \
             grid-auto-rows: {th:.0}px; grid-auto-flow: row dense; \
             justify-content: center; align-content: flex-start; \
             flex-direction: unset; flex-wrap: unset; align-items: unset;",
            tw = plan.tile_w,
            th = plan.tile_h(),
        )
    } else if tile_count == 1 {
        format!(
            "display: grid; \
             grid-template-columns: repeat({cols}, 1fr); \
             grid-template-rows: repeat({rows}, 1fr); \
             grid-auto-rows: auto; grid-auto-flow: row; \
             justify-content: stretch; align-content: stretch; \
             flex-direction: unset; flex-wrap: unset; align-items: unset;"
        )
    } else {
        "display: flex; \
         grid-template-columns: none; grid-template-rows: none; \
         grid-auto-rows: auto; grid-auto-flow: row; \
         justify-content: center; align-content: flex-start; \
         flex-direction: row; flex-wrap: wrap; align-items: flex-start;"
            .to_string()
    }
}

/// [`tile_flow_style`]'s set valued for the screen-share split. (issue 2700)
pub(crate) fn screen_share_flow_style() -> &'static str {
    "display: flex; flex-direction: row; flex-wrap: nowrap; align-items: stretch; \
     justify-content: flex-start; align-content: stretch; \
     grid-template-columns: none; grid-template-rows: none; \
     grid-auto-rows: auto; grid-auto-flow: row;"
}

/// The tile size custom properties, standard and enlarged.
pub fn tile_size_vars(tile_w: f64, tile_h: f64, gap: f64) -> String {
    let (tw, th) = (tile_w.round(), tile_h.round());
    format!(
        "--tile-w: {tw:.0}px; --tile-h: {th:.0}px; \
         --tile-2x-w: {:.0}px; --tile-2x-h: {:.0}px;",
        2.0 * tw + gap,
        2.0 * th + gap,
    )
}

/// `--tile-w` / `--tile-h` for the screen-share split: the largest 3:2 tile in
/// the meeting area, never the participant-count-dependent cell (PR #1946).
pub(crate) fn screen_share_stage_tile_size(avail_w: f64, avail_h: f64) -> (f64, f64) {
    // Width of a 3:2 tile whose height fills `avail_h`, capped so it never
    // exceeds `avail_w` (mirrors the height-vs-width constraint in
    // `compute_layout`'s single-tile case for tall/narrow viewports).
    let tw = (avail_h * TILE_AR).min(avail_w).max(0.0);
    let th = tw / TILE_AR;
    (tw, th)
}

pub(crate) const SPLIT_GAP: f64 = 10.0;
const SPLIT_HANDLE_W: f64 = 6.0;
const SPLIT_PANEL_PAD: f64 = 14.0;
const SPLIT_PANEL_MIN_COL: f64 = 160.0;
const SPLIT_PANEL_SLACK: f64 = 15.0;
const SPLIT_PANEL_SHAVE: f64 = 0.004;

/// The split's peer panel: `#grid-container`'s content-box width, the stage's
/// share of it, the panel's height and its number of tiles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SplitPanel {
    pub(crate) content_w: f64,
    pub(crate) ratio: f64,
    pub(crate) height: f64,
    pub(crate) tiles: usize,
}

/// The panel in a `#grid-container` `grid_w` px wide and `vh` px tall, padded by
/// [`screen_share_padding`]'s `pads`, holding `cameras` and `shares` share tiles.
pub(crate) fn split_panel(
    grid_w: f64,
    vh: f64,
    (top, right, bottom, left): (f64, f64, f64, f64),
    ratio: f64,
    cameras: usize,
    shares: usize,
) -> SplitPanel {
    SplitPanel {
        content_w: grid_w - left - right,
        ratio,
        height: vh - top - bottom,
        tiles: cameras + shares,
    }
}

/// CSS height of a camera tile in `.ss-peer-panel`, rounded up to the widest
/// tile its `auto-fill` grid can paint, with a scrollbar when it overflows.
pub(crate) fn split_peer_tile_height(panel: SplitPanel) -> f64 {
    let flex_w = panel.content_w - SPLIT_HANDLE_W - 2.0 * SPLIT_GAP;
    let panel_w = flex_w * (1.0 - panel.ratio - SPLIT_PANEL_SHAVE) / (1.0 - SPLIT_PANEL_SHAVE);
    let inner = (panel_w - 2.0 * SPLIT_PANEL_PAD).max(0.0);
    let tile = |slack: f64| {
        let cols = ((inner - slack + SPLIT_GAP) / (SPLIT_PANEL_MIN_COL + SPLIT_GAP))
            .floor()
            .max(1.0);
        ((inner - SPLIT_GAP * (cols - 1.0)) / cols / TILE_AR, cols)
    };
    let (h, cols) = tile(0.0);
    let rows = (panel.tiles as f64 / cols).ceil();
    let content_h = rows * (h + SPLIT_GAP) - SPLIT_GAP + 2.0 * SPLIT_PANEL_PAD;
    if content_h > panel.height {
        tile(SPLIT_PANEL_SLACK).0
    } else {
        h
    }
}

/// Device-pixel height of a decoded camera tile, the receive-side video layer lid.
pub(crate) fn decoded_tile_device_px_h(
    split_layout: bool,
    panel: SplitPanel,
    plan: TilePlan,
    tile_count: usize,
    avail_h: f64,
    dpr: f64,
) -> u32 {
    let css_h = if split_layout {
        split_peer_tile_height(panel)
    } else if tile_count == 1 {
        // The lone tile is full-bleed (`.participants-1 .grid-item.full-bleed`).
        avail_h
    } else {
        plan.tile_h()
    };
    (css_h * dpr).round() as u32
}

pub(crate) fn enlarged_tile_device_px_h(plan: TilePlan, gap: f64, dpr: f64) -> u32 {
    ((2.0 * plan.tile_h() + gap) * dpr).round() as u32
}

/// Freshness (ms) an overflow speaker must beat to displace a VISIBLE speaker in
/// [`promote_speakers`] (#2273). Equals the `peer_speech_priority` throttle.
pub(crate) const SPEAKER_PROMOTION_MARGIN_MS: f64 = 5_000.0;

fn selection_tier(camera_on: bool, speaking: bool) -> u8 {
    match (camera_on, speaking) {
        (true, true) => 0,
        (false, true) => 1,
        (true, false) => 2,
        (false, false) => 3,
    }
}

fn recent_speech(
    peer: &str,
    speech_map: &HashMap<String, f64>,
    now_ms: f64,
    active_ms: f64,
) -> Option<f64> {
    speech_map
        .get(peer)
        .copied()
        .filter(|&ts| now_ms - ts < active_ms)
}

/// Rank the roster BEFORE the `CANVAS_LIMIT` cut and return the capped
/// `(session_id, camera_on)` list `attendants.rs` feeds to
/// `partition_camera_tiles` (issue #2273). Enlarged then pinned peers rank first,
/// by `pin_rank` (issues 3367, 2866). Returns the input order untouched when
/// `capped_real` covers the whole roster.
#[allow(clippy::too_many_arguments)]
pub(crate) fn select_display_candidates(
    display_peers: &[String],
    capped_real: usize,
    camera_on: impl Fn(&str) -> bool,
    pin_rank: impl Fn(&str) -> Option<usize>,
    speech_map: &HashMap<String, f64>,
    join_map: &HashMap<String, f64>,
    now_ms: f64,
    active_ms: f64,
) -> Vec<(String, bool)> {
    if capped_real >= display_peers.len() {
        return display_peers
            .iter()
            .map(|peer| (peer.clone(), camera_on(peer)))
            .collect();
    }

    let mut ranked: Vec<_> = display_peers
        .iter()
        .map(|peer| {
            let cam_on = camera_on(peer);
            let speech = recent_speech(peer, speech_map, now_ms, active_ms);
            let key = (
                pin_rank(peer).unwrap_or(usize::MAX),
                selection_tier(cam_on, speech.is_some()),
                speech.map_or(0.0, |ts| -ts), // negated: freshest speaker first
                join_map.get(peer).copied().unwrap_or(0.0),
            );
            (key, peer.clone(), cam_on)
        })
        .collect();

    ranked.sort_by(|a, b| {
        let ((ra, ta, sa, ja), pa, _) = a;
        let ((rb, tb, sb, jb), pb, _) = b;
        ra.cmp(rb)
            .then(ta.cmp(tb))
            .then(sa.partial_cmp(sb).unwrap_or(std::cmp::Ordering::Equal))
            .then(ja.partial_cmp(jb).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| pa.cmp(pb))
    });
    ranked.truncate(capped_real);

    ranked
        .into_iter()
        .map(|(_, peer, cam_on)| (peer, cam_on))
        .collect()
}

/// Order a camera-OFF group so the `off_to_render` remainder in `attendants.rs`
/// sheds silent peers first (#2273). Membership only — `build_unified_render_list`
/// re-sorts by join time, so POSITION is unaffected.
pub(crate) fn sort_camera_off_window(
    peers: &mut [String],
    speech_map: &HashMap<String, f64>,
    join_map: &HashMap<String, f64>,
    now_ms: f64,
    active_ms: f64,
) {
    peers.sort_by(|a, b| {
        let sa = recent_speech(a, speech_map, now_ms, active_ms);
        let sb = recent_speech(b, speech_map, now_ms, active_ms);
        let ja = join_map.get(a).copied().unwrap_or(0.0);
        let jb = join_map.get(b).copied().unwrap_or(0.0);
        sb.is_some()
            .cmp(&sa.is_some())
            .then(
                sb.unwrap_or(0.0)
                    .partial_cmp(&sa.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(ja.partial_cmp(&jb).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.cmp(b))
    });
}

/// Promote overflow speakers into the visible portion of a tile list.
///
/// When there are more tiles than fit on screen, tiles beyond `visible_count`
/// are "overflow".  If an overflow peer spoke within `active_ms` of `now_ms`,
/// swap them with the least-recently-active visible peer that is NOT speaking.
/// The loudest overflow speaker (most recent speech timestamp) gets priority.
///
/// ## Tie-breaking
///
/// * **Overflow speakers** are sorted *descending* by speech timestamp — the
///   most recent speaker is promoted first.
/// * **Swap candidates** (visible non-speakers) are sorted *ascending* by
///   effective timestamp (speech time if any, else join time) — the
///   least-recently-active tile is displaced first.
/// * `f64` ties are broken by `partial_cmp` defaulting to `Equal`, which
///   preserves the original iteration order (stable within the sort).
pub(crate) fn promote_speakers(
    tiles: &mut [String],
    visible_count: usize,
    speech_map: &HashMap<String, f64>,
    join_map: &HashMap<String, f64>,
    now_ms: f64,
    active_ms: f64,
) {
    if visible_count >= tiles.len() {
        return;
    }

    // Effective timestamp: last speech time if exists, else join time.
    let eff_ts = |peer: &str| -> f64 {
        speech_map
            .get(peer)
            .copied()
            .unwrap_or_else(|| join_map.get(peer).copied().unwrap_or(0.0))
    };

    // Overflow tiles that are actively speaking (most recent first).
    let mut overflow_speakers: Vec<(usize, f64)> = Vec::new();
    for (i, peer) in tiles.iter().enumerate().skip(visible_count) {
        if let Some(&ts) = speech_map.get(peer) {
            if now_ms - ts < active_ms {
                overflow_speakers.push((i, ts));
            }
        }
    }
    overflow_speakers.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Visible non-speaking tiles as swap candidates (least recently active first).
    let mut swap_candidates: Vec<(usize, f64)> = (0..visible_count)
        .filter(|&i| {
            speech_map
                .get(&tiles[i])
                .is_none_or(|&ts| now_ms - ts >= active_ms)
        })
        .map(|i| (i, eff_ts(&tiles[i])))
        .collect();
    swap_candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    // Complementary to `swap_candidates`, so the two index sets are disjoint.
    let mut stale_speakers: Vec<(usize, f64)> = (0..visible_count)
        .filter_map(|i| recent_speech(&tiles[i], speech_map, now_ms, active_ms).map(|ts| (i, ts)))
        .collect();
    stale_speakers.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    // Swap pairs — all indices are disjoint so order doesn't matter.
    let num_swaps = overflow_speakers.len().min(swap_candidates.len());
    for i in 0..num_swaps {
        tiles.swap(swap_candidates[i].0, overflow_speakers[i].0);
    }

    // Fallback (#2273): with every visible tile speaking `swap_candidates` is
    // empty and the loop above promotes nobody, so displace the stalest visible
    // speaker — only for an overflow speaker `SPEAKER_PROMOTION_MARGIN_MS`
    // fresher, which makes each swap one-way.
    for (k, &(overflow_idx, overflow_ts)) in overflow_speakers[num_swaps..].iter().enumerate() {
        let Some(&(visible_idx, visible_ts)) = stale_speakers.get(k) else {
            break;
        };
        if overflow_ts < visible_ts + SPEAKER_PROMOTION_MARGIN_MS {
            break;
        }
        tiles.swap(visible_idx, overflow_idx);
    }
}

pub(crate) struct SpeakerInputs<'a> {
    pub speech_map: &'a HashMap<String, f64>,
    pub join_map: &'a HashMap<String, f64>,
    pub now_ms: f64,
    pub active_ms: f64,
}

pub(crate) struct CameraWindowInput<'a> {
    /// Leading session_ids in rank order: enlarged then pinned, or pinned for the split panel.
    pub pins: &'a [String],
    /// Join-sorted.
    pub camera_on: Vec<String>,
    pub camera_off: Vec<String>,
    pub displayed: usize,
    /// Decoded tiles before any PLAY request.
    pub budget: usize,
    pub device_ceiling: Option<usize>,
    pub requested: &'a HashSet<String>,
}

pub(crate) struct CameraWindow {
    pub tiles: Vec<String>,
    pub camera_off: Vec<String>,
    pub decoded: usize,
    pub on_cells: usize,
    pub bucket: HashSet<u64>,
}

/// The camera-on decode pipeline the grid and the split panel share: pins
/// first, the budget window widened for PLAY requests on displayed tiles
/// (#1466), speaker promotion behind the pins, then the PLAY promotion, which
/// keeps off only the pins the budget itself decodes.
pub(crate) fn plan_camera_window(
    input: CameraWindowInput,
    speakers: &SpeakerInputs,
) -> CameraWindow {
    let w = pin_first_window(
        input.pins,
        input.camera_on,
        input.camera_off,
        input.displayed,
    );
    let mut tiles = w.all_tiles;
    let base = w.on_cells.min(input.budget);
    let requested_off_budget = tiles
        .iter()
        .take(w.on_cells)
        .skip(base)
        .filter(|t| input.requested.contains(*t))
        .count();
    let decoded = expand_decoded_for_requested(
        base,
        requested_off_budget,
        input.device_ceiling,
        CANVAS_LIMIT,
    )
    .min(w.on_cells);
    promote_speakers(
        &mut tiles[w.pinned_on..],
        decoded.saturating_sub(w.pinned_on),
        speakers.speech_map,
        speakers.join_map,
        speakers.now_ms,
        speakers.active_ms,
    );
    let bucket = build_decoded_bucket(
        &mut tiles,
        decoded,
        w.on_cells,
        w.pinned_on.min(base),
        input.requested,
    );
    CameraWindow {
        tiles,
        camera_off: w.camera_off,
        decoded,
        on_cells: w.on_cells,
        bucket,
    }
}

/// Determine the effective density mode by auto-escalating from the user's
/// chosen mode until every active speaker fits on-screen.
///
/// Returns the (possibly escalated) `DensityMode`.  If even `Maximum` cannot
/// fit all speakers, `Maximum` is returned (never panics).
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_effective_density(
    user_mode: DensityMode,
    total_tiles: usize,
    avail_w: f64,
    avail_h: f64,
    gap: f64,
    active_speaker_count: usize,
    num_display_peers: usize,
    vw: f64,
) -> DensityMode {
    const MODES_BY_DENSITY: [DensityMode; 4] = [
        DensityMode::Standard,
        DensityMode::Auto,
        DensityMode::Dense,
        DensityMode::Maximum,
    ];

    if active_speaker_count == 0 {
        return user_mode;
    }

    let user_rank = MODES_BY_DENSITY
        .iter()
        .position(|&m| m == user_mode)
        .unwrap_or(1);

    let mut chosen = user_mode;
    for &mode in &MODES_BY_DENSITY[user_rank..] {
        chosen = mode;
        let mtw = mode.min_tile_width(vw);
        let capacity = {
            let mut t = total_tiles;
            while t > 1 {
                let (_c, _r, tw) = compute_layout(t, avail_w, avail_h, gap);
                if tw >= mtw {
                    break;
                }
                t -= 1;
            }
            t
        };
        let vis = if total_tiles > capacity {
            capacity.saturating_sub(1).max(1)
        } else {
            total_tiles
        };
        let vis_real = num_display_peers.min(vis);
        if vis_real >= active_speaker_count {
            break;
        }
    }
    chosen
}

pub(crate) const DRAWER_MIN_WIDTH: f64 = 300.0;

pub(crate) const DRAWER_MAX_ABS: f64 = 720.0;

pub(crate) const CHAT_DRAWER_WIDTH: f64 = 360.0;

/// Alias, not a second literal.
pub(crate) const DRAWER_REFLOW_MIN_VW: f64 = MOBILE_WIDTH_BREAKPOINT_PX;

pub(crate) const CHAT_OVERLAY_MIN_VW: f64 = CHAT_DRAWER_WIDTH + MIN_TILE_BAND;

/// Below its breakpoint chat is a full-width sheet: it reserves nothing, sits at
/// `right: 0`, and is exclusive with every other drawer. Mirrored by the
/// `max-width: 679.98px` block in style.css.
pub(crate) fn chat_is_sheet(vw: f64) -> bool {
    vw < CHAT_OVERLAY_MIN_VW
}

const DRAWER_BUDGET_FRACTION: f64 = 0.60;

const MIN_GRID_BAND: f64 = 400.0;

const MIN_TILE_BAND: f64 = 320.0;

pub(crate) const ACTION_BAR_EDGE_MARGIN: f64 = 40.0;

/// Must equal `--meeting-footer-h` in global.css.
pub(crate) const MEETING_FOOTER_RESERVE: f64 = 32.0;

/// Where row 1 must start, below any status-bar reserve, while the meeting-timer
/// chip is up: its `--top-stack-top`, its expired height and a gap. Pinned
/// against style.css by a test.
pub(crate) const MEETING_TIMER_BAND_CLEARANCE: f64 = 60.0;
pub(crate) const MEETING_TIMER_BAND_CLEARANCE_MOBILE: f64 = 56.0;

const SCREEN_SHARE_PAD_TOP: f64 = 16.0;

/// Extra top padding that clears the meeting-timer chip over a `base_top` pad.
pub(crate) fn meeting_timer_band(shown: bool, vw: f64, base_top: f64) -> f64 {
    if !shown {
        return 0.0;
    }
    let clearance = if vw < MOBILE_WIDTH_BREAKPOINT_PX {
        MEETING_TIMER_BAND_CLEARANCE_MOBILE
    } else {
        MEETING_TIMER_BAND_CLEARANCE
    };
    (clearance - base_top).max(0.0)
}

/// `#grid-container`'s top padding in the grid layout.
pub(crate) fn grid_pad_top(base_top: f64, vw: f64, status_bar_reserve: f64, timer: bool) -> f64 {
    base_top + status_bar_reserve + meeting_timer_band(timer, vw, base_top)
}

/// `(top, right, bottom, left)` for `#grid-container` in the screen-share layout.
pub(crate) fn screen_share_padding(
    status_bar_reserve: f64,
    timer: bool,
    vw: f64,
) -> (f64, f64, f64, f64) {
    (
        SCREEN_SHARE_PAD_TOP
            + status_bar_reserve
            + meeting_timer_band(timer, vw, SCREEN_SHARE_PAD_TOP),
        16.0,
        80.0 + MEETING_FOOTER_RESERVE,
        16.0,
    )
}

/// `(gap, top, right, bottom, left)` for `#grid-container`.
pub(crate) fn grid_padding(dock: DockPosition, vw: f64) -> (f64, f64, f64, f64, f64) {
    let mobile = vw < MOBILE_WIDTH_BREAKPOINT_PX;
    let (gap, top, right, bottom, left) = match (dock, mobile) {
        (DockPosition::Bottom, true) => (8.0, 8.0, 8.0, 80.0, 8.0),
        (DockPosition::Bottom, false) => (16.0, 20.0, 20.0, 120.0, 20.0),
        (DockPosition::Left, true) => (8.0, 8.0, 8.0, 8.0, 80.0),
        (DockPosition::Left, false) => (16.0, 20.0, 20.0, 20.0, 120.0),
        (DockPosition::Right, true) => (8.0, 8.0, 80.0, 8.0, 8.0),
        (DockPosition::Right, false) => (16.0, 20.0, 120.0, 20.0, 20.0),
    };
    (gap, top, right, bottom + MEETING_FOOTER_RESERVE, left)
}

const DRAWER_DRAG_QUANTUM: f64 = 8.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrawerSide {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrawerKind {
    PeerList,
    Diagnostics,
    Chat,
}

impl DrawerKind {
    fn floor(self) -> f64 {
        match self {
            DrawerKind::Chat => CHAT_DRAWER_WIDTH,
            _ => DRAWER_MIN_WIDTH,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DrawersToClose {
    pub(crate) peer_list: bool,
    pub(crate) diagnostics: bool,
    pub(crate) chat: bool,
}

impl DrawersToClose {
    pub(crate) fn any(self) -> bool {
        self.peer_list || self.diagnostics || self.chat
    }
}

pub(crate) fn max_total_reserve(vw: f64) -> f64 {
    (vw * DRAWER_BUDGET_FRACTION)
        .min(vw - MIN_GRID_BAND)
        .max(0.0)
}

pub(crate) fn handle_is_inert(cap: f64) -> bool {
    cap <= DRAWER_MIN_WIDTH
}

pub(crate) fn overflow_budget_width(is_vertical: bool, band_w: f64, vh: f64) -> f64 {
    (if is_vertical {
        vh - MEETING_FOOTER_RESERVE
    } else {
        band_w
    }) - ACTION_BAR_EDGE_MARGIN
}

pub(crate) fn quantise_reserve(px: f64, dragging: bool) -> f64 {
    if dragging {
        (px / DRAWER_DRAG_QUANTUM).ceil() * DRAWER_DRAG_QUANTUM
    } else {
        px
    }
}

const DRAWER_CLOSE_ORDER: [DrawerKind; 3] = [
    DrawerKind::Diagnostics,
    DrawerKind::PeerList,
    DrawerKind::Chat,
];

fn is_open(kind: DrawerKind, peer_list: bool, diagnostics: bool, chat: bool) -> bool {
    match kind {
        DrawerKind::PeerList => peer_list,
        DrawerKind::Diagnostics => diagnostics,
        DrawerKind::Chat => chat,
    }
}

fn close_until_fits(
    vw: f64,
    protected: Option<DrawerKind>,
    mut peer_list: bool,
    mut diagnostics: bool,
    mut chat: bool,
) -> DrawersToClose {
    let mut out = DrawersToClose::default();
    if chat_is_sheet(vw) {
        // An explicit open of another drawer dismisses the sheet; otherwise,
        // including a resize, the sheet stays and the others go.
        if chat {
            match protected {
                Some(DrawerKind::Chat) | None => {
                    out.peer_list = peer_list;
                    out.diagnostics = diagnostics;
                    peer_list = false;
                    diagnostics = false;
                }
                Some(_) => out.chat = true,
            }
        }
        // A sheet reserves nothing either way.
        chat = false;
    }
    if vw < DRAWER_REFLOW_MIN_VW {
        return out;
    }
    let floors = |p: bool, d: bool, c: bool| {
        (if p { DrawerKind::PeerList.floor() } else { 0.0 })
            + (if d {
                DrawerKind::Diagnostics.floor()
            } else {
                0.0
            })
            + (if c { DrawerKind::Chat.floor() } else { 0.0 })
    };
    for kind in DRAWER_CLOSE_ORDER {
        // Never shed the last reserving drawer, or the resize rule would close
        // what the open rule just allowed on a viewport too narrow for one.
        let open_count = usize::from(peer_list) + usize::from(diagnostics) + usize::from(chat);
        if open_count <= 1 || vw - floors(peer_list, diagnostics, chat) >= MIN_TILE_BAND {
            break;
        }
        if protected == Some(kind) || !is_open(kind, peer_list, diagnostics, chat) {
            continue;
        }
        match kind {
            DrawerKind::PeerList => {
                peer_list = false;
                out.peer_list = true;
            }
            DrawerKind::Diagnostics => {
                diagnostics = false;
                out.diagnostics = true;
            }
            DrawerKind::Chat => {
                chat = false;
                out.chat = true;
            }
        }
    }
    out
}

pub(crate) fn drawers_to_close_on_open(
    vw: f64,
    opening: DrawerKind,
    peer_list_open: bool,
    diagnostics_open: bool,
    chat_open: bool,
) -> DrawersToClose {
    close_until_fits(
        vw,
        Some(opening),
        peer_list_open || opening == DrawerKind::PeerList,
        diagnostics_open || opening == DrawerKind::Diagnostics,
        chat_open || opening == DrawerKind::Chat,
    )
}

pub(crate) fn resize_notice(close: DrawersToClose, customize_ended: bool) -> Option<&'static str> {
    if customize_ended {
        return Some("Button editing ended to fit the window; layout saved");
    }
    match (close.peer_list, close.diagnostics, close.chat) {
        (false, false, false) => None,
        (true, false, false) => Some("Participants closed to fit the window"),
        (false, true, false) => Some("Diagnostics closed to fit the window"),
        (false, false, true) => Some("Chat closed to fit the window"),
        _ => Some("Panels closed to fit the window"),
    }
}

pub(crate) fn drawers_to_close_on_resize(
    vw: f64,
    peer_list_open: bool,
    diagnostics_open: bool,
    chat_open: bool,
) -> DrawersToClose {
    close_until_fits(vw, None, peer_list_open, diagnostics_open, chat_open)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DrawerState {
    pub(crate) vw: f64,
    pub(crate) peer_list_open: bool,
    pub(crate) left_w: f64,
    pub(crate) diagnostics_open: bool,
    pub(crate) right_w: f64,
    pub(crate) chat_open: bool,
    pub(crate) chat_w: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DrawerReserves {
    pub(crate) left_render_w: f64,
    pub(crate) chat_render_w: f64,
    pub(crate) diag_render_w: f64,
    pub(crate) chat_right_offset: f64,
    pub(crate) left_reserve: f64,
    pub(crate) right_reserve: f64,
}

/// Zero below [`DRAWER_REFLOW_MIN_VW`]; over [`max_total_reserve`] the drawers
/// shrink diagnostics, then the peer list, then chat, and the floors win over
/// the cap.
pub(crate) fn drawer_reserves(state: DrawerState) -> DrawerReserves {
    // Negated, not `vw < MIN`: a NaN viewport must not reach the grid branch.
    let reflows = state.vw >= DRAWER_REFLOW_MIN_VW;
    if !reflows {
        return DrawerReserves {
            left_render_w: state.left_w,
            chat_render_w: state.chat_w,
            diag_render_w: state.right_w,
            chat_right_offset: 0.0,
            left_reserve: 0.0,
            right_reserve: 0.0,
        };
    }

    let chat_reserves = state.chat_open && !chat_is_sheet(state.vw);
    let open = [state.diagnostics_open, state.peer_list_open, chat_reserves];
    let floors = [
        DRAWER_MIN_WIDTH,
        DRAWER_MIN_WIDTH,
        state.chat_w.min(CHAT_DRAWER_WIDTH),
    ];
    let mut w = [state.right_w, state.left_w, state.chat_w];
    let budget = max_total_reserve(state.vw);
    let occupied = |w: &[f64; 3]| -> f64 {
        w.iter()
            .zip(open.iter())
            .filter(|(_, is_open)| **is_open)
            .map(|(width, _)| *width)
            .sum::<f64>()
    };

    for (i, &floor) in floors.iter().enumerate() {
        let over = occupied(&w) - budget;
        if over <= 0.0 {
            break;
        }
        if open[i] {
            w[i] = (w[i] - over).max(floor);
        }
    }

    let [diag_render_w, left_render_w, chat_render_w] = w;
    let chat_occupies = if chat_reserves { chat_render_w } else { 0.0 };
    let diag_reserve = if state.diagnostics_open {
        diag_render_w
    } else {
        0.0
    };
    DrawerReserves {
        left_render_w,
        chat_render_w,
        diag_render_w,
        // The CSS forces `right: 0 !important` for a sheet; the two must agree.
        chat_right_offset: if chat_is_sheet(state.vw) {
            0.0
        } else {
            diag_reserve
        },
        left_reserve: if state.peer_list_open {
            left_render_w
        } else {
            0.0
        },
        right_reserve: chat_occupies + diag_reserve,
    }
}

/// Largest width a drag on `side` may commit to. Diagnostics shrinks FIRST, so a
/// peer-list drag need only reserve its floor; its own drag reserves live widths.
pub(crate) fn drawer_max_for_side(state: DrawerState, side: DrawerSide) -> f64 {
    let others = if state.vw >= DRAWER_REFLOW_MIN_VW {
        let chat = if state.chat_open && !chat_is_sheet(state.vw) {
            drawer_reserves(state).chat_render_w
        } else {
            0.0
        };
        let opposite = match side {
            DrawerSide::Left if state.diagnostics_open => DRAWER_MIN_WIDTH,
            DrawerSide::Right if state.peer_list_open => state.left_w,
            _ => 0.0,
        };
        chat + opposite
    } else {
        0.0
    };
    let cap = (max_total_reserve(state.vw) - others)
        .min(state.vw * 0.5)
        .min(DRAWER_MAX_ABS);
    // Not `f64::clamp`: at 568px the cap (284) is under the floor, which panics.
    cap.max(DRAWER_MIN_WIDTH)
}

/// The action bar is `position: fixed`, so it does not ride the grid's inset
/// and budgeting it against the whole viewport spills it into the drawers.
pub(crate) fn action_bar_band_width(vw: f64, reserves: DrawerReserves) -> f64 {
    (vw - reserves.left_reserve - reserves.right_reserve).max(0.0)
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- compute_layout ------------------------------------------------

    #[test]
    fn compute_layout_zero_tiles() {
        let (c, r, tw) = compute_layout(0, 1000.0, 600.0, 8.0);
        assert_eq!(c, 1);
        assert_eq!(r, 1);
        assert!((tw - 1000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_layout_single_tile() {
        let (c, r, _tw) = compute_layout(1, 1000.0, 600.0, 8.0);
        assert_eq!(c, 1);
        assert_eq!(r, 1);
    }

    #[test]
    fn compute_layout_respects_aspect_ratio() {
        let (c, _r, tw) = compute_layout(4, 1200.0, 800.0, 0.0);
        // With no gap, 2×2 is optimal for 4 tiles in a 3:2 area.
        assert_eq!(c, 2);
        let th = tw / TILE_AR;
        assert!(th > 0.0);
    }

    #[test]
    fn compute_layout_floors_so_a_full_row_never_overflows_and_wraps() {
        let (cols, rows, tw) = compute_layout(3, 1000.0, 240.0, 10.0);
        assert_eq!((cols, rows), (3, 1));
        assert_eq!(tw, 326.0, "must floor, not round");
        let gaps = (cols as f64 - 1.0) * 10.0;
        assert!(
            cols as f64 * tw + gaps <= 1000.0,
            "row {} exceeds 1000",
            cols as f64 * tw + gaps
        );
        // Premise: one more pixel per tile overflows, so the case discriminates.
        assert!(cols as f64 * (tw + 1.0) + gaps > 1000.0);
    }

    #[test]
    fn wrap_tile_width_matches_the_1280_acceptance_table() {
        // 1280x720, no drawer open: the meeting area is 1240x580.
        let (cols, rows, tw) = compute_layout(4, 1240.0, 580.0, 16.0);
        assert_eq!((cols, rows, tw), (2, 2, 423.0));
        // 3 tiles keep 2x2, so the short row is indented half a pitch: 428.5.
        let (c3, r3, tw3) = compute_layout(3, 1240.0, 580.0, 16.0);
        assert_eq!((c3, r3, tw3), (2, 2, 423.0));
        assert_eq!((tw3 + 16.0) / 2.0, 219.5);
        let (c_open, _r, tw_open) = compute_layout(4, 920.0, 580.0, 16.0);
        assert_eq!((c_open, tw_open), (2, 423.0));
    }

    fn plain(cols: usize, rows: usize) -> TilePlan {
        TilePlan {
            cols,
            rows,
            tile_w: 300.0,
            enlarged: 0,
        }
    }

    #[test]
    fn tile_flow_wraps_and_centres_for_two_or_more_tiles() {
        let s = tile_flow_style(4, plain(2, 2));
        assert!(s.contains("display: flex;"), "{s}");
        assert!(s.contains("flex-wrap: wrap;"), "{s}");
        assert!(s.contains("justify-content: center;"), "{s}");
        // Horizontal only: the tracks this replaces packed top (HCL #6).
        assert!(s.contains("align-content: flex-start;"), "{s}");
        assert!(s.contains("grid-template-columns: none;"), "{s}");
    }

    #[test]
    fn tile_flow_stretches_a_lone_tile_across_one_fr_tracks() {
        let s = tile_flow_style(1, plain(1, 1));
        assert!(s.contains("display: grid;"), "{s}");
        assert!(s.contains("grid-template-columns: repeat(1, 1fr);"), "{s}");
        assert!(s.contains("justify-content: stretch;"), "{s}");
        assert!(!s.contains("flex-wrap: wrap"), "{s}");
        assert!(
            !s.contains("flex-direction: row"),
            "screen-share-layout.spec.ts:285 forbids this, and it is untagged so per-PR CI never runs it: {s}"
        );
    }

    #[test]
    fn all_three_flow_arms_declare_the_same_longhands() {
        fn names(s: &str) -> Vec<&str> {
            let mut v: Vec<&str> = s
                .split(';')
                .filter_map(|d| d.split_once(':'))
                .map(|(k, _)| k.trim())
                .collect();
            v.sort_unstable();
            v
        }
        let lone = tile_flow_style(1, plain(2, 2));
        let wrap = tile_flow_style(4, plain(2, 2));
        let enlarged = tile_flow_style(
            4,
            TilePlan {
                enlarged: 1,
                ..plain(3, 2)
            },
        );
        assert_eq!(names(&lone).len(), 10, "{lone}");
        assert_eq!(names(&lone), names(&wrap));
        assert_eq!(names(&lone), names(&enlarged));
        assert_eq!(names(&lone), names(screen_share_flow_style()));
    }

    #[test]
    fn only_a_painted_enlarged_tile_gives_the_grid_real_tracks() {
        let s = tile_flow_style(
            4,
            TilePlan {
                cols: 4,
                rows: 2,
                tile_w: 298.0,
                enlarged: 1,
            },
        );
        for want in [
            "display: grid;",
            "grid-template-columns: repeat(4, 298px);",
            "grid-auto-rows: 199px;",
            "grid-auto-flow: row dense;",
            "justify-content: center;",
            "align-content: flex-start;",
        ] {
            assert!(s.contains(want), "{want} missing from {s}");
        }
        assert!(tile_flow_style(4, plain(2, 2)).contains("grid-auto-flow: row;"));
    }

    #[test]
    fn the_2x_vars_span_two_painted_cells_and_a_gap() {
        assert_eq!(
            tile_size_vars(298.0, 298.0 / TILE_AR, 16.0),
            "--tile-w: 298px; --tile-h: 199px; --tile-2x-w: 612px; --tile-2x-h: 414px;"
        );
    }

    fn input(tile_count: usize) -> TilePlanInput {
        TilePlanInput {
            tile_count,
            split_layout: false,
            full_bleed: false,
            avail_w: 1240.0,
            avail_h: 548.0,
            gap: 16.0,
            vw: 1280.0,
        }
    }

    /// CSS `grid-auto-flow: row dense`: every item takes the first area it fits.
    fn dense_packed_rows(n: usize, k: usize, cols: usize) -> usize {
        let mut taken: HashSet<(usize, usize)> = HashSet::new();
        let mut rows = 0;
        for span in std::iter::repeat_n(2, k).chain(std::iter::repeat_n(1, n - k)) {
            let (row, col) = (0..)
                .flat_map(|row| (0..=cols - span).map(move |col| (row, col)))
                .find(|&(row, col)| {
                    (row..row + span).all(|r| (col..col + span).all(|c| !taken.contains(&(r, c))))
                })
                .expect("the grid has unbounded rows");
            for r in row..row + span {
                for c in col..col + span {
                    taken.insert((r, c));
                }
            }
            rows = rows.max(row + span);
        }
        rows
    }

    #[test]
    fn enlarged_rows_matches_a_dense_packing_simulation() {
        for n in 1..=24 {
            for k in 1..=n {
                for cols in 2..=n + k {
                    assert_eq!(
                        enlarged_rows(n, k, cols),
                        dense_packed_rows(n, k, cols),
                        "n={n} k={k} cols={cols}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_plan_always_fits_the_meeting_area() {
        for (w, h, gap, vw) in [
            (1240.0, 548.0, 16.0, 1280.0),
            (760.0, 548.0, 16.0, 800.0),
            (1880.0, 908.0, 16.0, 1920.0),
            (359.0, 547.0, 8.0, 375.0),
            (651.0, 263.0, 8.0, 667.0),
        ] {
            for n in 2..=24 {
                for wanted in 1..=n {
                    let p = plan_tiles(
                        TilePlanInput {
                            avail_w: w,
                            avail_h: h,
                            gap,
                            vw,
                            ..input(n)
                        },
                        wanted,
                    );
                    let (cols, rows) = (p.cols as f64, p.rows as f64);
                    assert!(
                        cols * p.tile_w + (cols - 1.0) * gap <= w,
                        "too wide: {p:?} in {w}x{h} n={n}"
                    );
                    assert!(
                        rows * p.tile_h() + (rows - 1.0) * gap <= h + 0.001,
                        "too tall: {p:?} in {w}x{h} n={n}"
                    );
                    if p.enlarged > 0 {
                        assert_eq!(p.rows, dense_packed_rows(n, p.enlarged, p.cols));
                        assert!(p.cols >= 2);
                        assert!(p.tile_w >= DensityMode::Maximum.min_tile_width(vw));
                    }
                }
            }
        }
    }

    #[test]
    fn with_nothing_enlarged_the_plan_is_exactly_compute_layout() {
        for n in 0..=30 {
            for (w, h, gap) in [(1240.0, 548.0, 16.0), (359.0, 547.0, 8.0), (0.0, 0.0, 8.0)] {
                let p = plan_tiles(
                    TilePlanInput {
                        avail_w: w,
                        avail_h: h,
                        gap,
                        ..input(n)
                    },
                    0,
                );
                assert_eq!((p.cols, p.rows, p.tile_w), compute_layout(n, w, h, gap));
                assert_eq!(p.enlarged, 0);
            }
        }
    }

    #[test]
    fn one_enlarged_tile_of_two_takes_three_columns() {
        let p = plan_tiles(input(2), 1);
        assert_eq!((p.cols, p.rows, p.tile_w, p.enlarged), (3, 2, 399.0, 1));
    }

    #[test]
    fn the_width_floor_drops_enlarged_tiles_from_the_back() {
        let area = TilePlanInput {
            avail_w: 420.0,
            avail_h: 700.0,
            ..input(6)
        };
        let floor = DensityMode::Maximum.min_tile_width(area.vw);
        let all_four = best_layout(
            2..=10,
            |cols| enlarged_rows(6, 4, cols),
            area.avail_w,
            area.avail_h,
            area.gap,
        );
        assert!(
            all_four.2 < floor,
            "premise: 4 miss the floor: {all_four:?}"
        );
        let p = plan_tiles(area, 4);
        assert_eq!(p.enlarged, 3, "{p:?}");
        assert!(p.tile_w >= floor, "{p:?}");
    }

    #[test]
    fn no_room_for_two_columns_enlarges_nothing() {
        let p = plan_tiles(
            TilePlanInput {
                avail_w: 200.0,
                avail_h: 900.0,
                ..input(3)
            },
            1,
        );
        assert_eq!(p.enlarged, 0, "{p:?}");
    }

    #[test]
    fn the_split_and_a_full_bleed_tile_enlarge_nothing() {
        let split = TilePlanInput {
            split_layout: true,
            ..input(4)
        };
        let bleed = TilePlanInput {
            full_bleed: true,
            ..input(2)
        };
        assert_eq!(plan_tiles(input(4), 1).enlarged, 1, "premise");
        assert_eq!(plan_tiles(split, 1).enlarged, 0);
        assert_eq!(plan_tiles(bleed, 1).enlarged, 0);
        assert_eq!(
            plan_tiles(input(1), 1).enlarged,
            0,
            "a lone tile has no second column to span"
        );
    }

    #[test]
    fn the_split_and_full_bleed_withdraw_the_offer() {
        let two = input(2);
        assert!(enlarge_offered(two, plan_tiles(two, 0), false));
        assert!(enlarge_offered(two, plan_tiles(two, 0), true));
        for off in [
            TilePlanInput {
                split_layout: true,
                ..two
            },
            TilePlanInput {
                full_bleed: true,
                ..two
            },
        ] {
            assert!(!enlarge_offered(off, plan_tiles(off, 0), false), "{off:?}");
        }
    }

    fn portrait_phone() -> TilePlanInput {
        TilePlanInput {
            avail_w: 359.0,
            avail_h: 547.0,
            gap: 8.0,
            vw: 375.0,
            ..input(2)
        }
    }

    #[test]
    fn a_portrait_phone_is_not_offered_enlarge() {
        let phone = portrait_phone();
        let plan = plan_tiles(phone, 0);
        assert!(plan.tile_w >= ENLARGE_MIN_TILE_W_COARSE_POINTER, "{plan:?}");
        assert!(
            !enlarge_offered(phone, plan, true),
            "2x a 175px tile does not beat the {} px tile it replaces",
            plan.tile_w
        );
    }

    #[test]
    fn a_painted_tile_must_be_bigger_than_a_standard_tile_with_nothing_enlarged() {
        let phone = portrait_phone();
        assert_eq!(plan_tiles(phone, 0).tile_w, 359.0, "premise");
        assert_eq!(
            best_layout(2..=3, |c| enlarged_rows(2, 1, c), 359.0, 547.0, 8.0).2,
            175.0
        );
        assert_eq!(
            plan_tiles(phone, 1).enlarged,
            0,
            "358 px at 2x is not an enlargement of 359 px"
        );
    }

    #[test]
    fn offering_needs_a_1_2x_gain_but_keeping_needs_only_a_bigger_tile() {
        for (avail_w, avail_h, vw, standard_w, enlarged_w, offered) in [
            (1326.0, 478.0, 1366.0, 655.0, 708.0, false),
            (1496.0, 558.0, 1536.0, 740.0, 828.0, false),
            (1240.0, 548.0, 1280.0, 612.0, 814.0, true),
            (1880.0, 768.0, 1920.0, 932.0, 1144.0, true),
            (1880.0, 797.0, 1920.0, 932.0, 1186.0, true),
        ] {
            let area = TilePlanInput {
                avail_w,
                avail_h,
                vw,
                ..input(2)
            };
            let standard = plan_tiles(area, 0);
            let kept = plan_tiles(area, 1);
            assert_eq!(standard.tile_w, standard_w, "{area:?}");
            assert_eq!(2.0 * kept.tile_w + area.gap, enlarged_w, "{area:?}");
            assert_eq!(kept.enlarged, 1, "{area:?}: any bigger tile stays painted");
            assert_eq!(enlarge_offered(area, standard, false), offered, "{area:?}");
        }
    }

    #[test]
    fn with_two_remotes_the_second_tile_is_not_offered() {
        let two = input(2);
        let painted = plan_tiles(two, 1);
        assert_eq!(painted.enlarged, 1, "premise");
        assert!(!enlarge_offered(two, painted, false));
        assert!(!enlarge_offered(two, painted, true));
    }

    #[test]
    fn an_offer_always_paints_one_more_tile_at_a_worthwhile_size() {
        let mut offered = 0;
        for (w, h, gap, vw) in [
            (1240.0, 548.0, 16.0, 1280.0),
            (1326.0, 478.0, 16.0, 1366.0),
            (760.0, 548.0, 16.0, 800.0),
            (1880.0, 908.0, 16.0, 1920.0),
            (396.0, 719.0, 8.0, 412.0),
            (359.0, 547.0, 8.0, 375.0),
            (651.0, 263.0, 8.0, 667.0),
            (700.0, 330.0, 16.0, 740.0),
        ] {
            for n in 2..=24 {
                for k in 0..n {
                    let area = TilePlanInput {
                        avail_w: w,
                        avail_h: h,
                        gap,
                        vw,
                        ..input(n)
                    };
                    let plan = plan_tiles(area, k);
                    if !enlarge_offered(area, plan, false) {
                        continue;
                    }
                    offered += 1;
                    let next = plan_tiles(area, plan.enlarged + 1);
                    assert_eq!(next.enlarged, plan.enlarged + 1, "{w}x{h} n={n} k={k}");
                    assert!(
                        2.0 * next.tile_w + gap >= 1.2 * plan.tile_w,
                        "{w}x{h} n={n} k={k}: {next:?} over {plan:?}"
                    );
                }
            }
        }
        assert!(offered > 0, "premise: something must be offered");
    }

    #[test]
    fn the_offer_needs_a_240_px_tile_on_a_fine_pointer_and_330_px_on_a_coarse_one() {
        let area = TilePlanInput {
            avail_w: 1000.0,
            avail_h: 540.0,
            ..input(4)
        };
        let at = |tile_w: f64| TilePlan {
            tile_w,
            ..plan_tiles(area, 0)
        };
        assert!(enlarge_offered(area, at(240.0), false));
        assert!(!enlarge_offered(area, at(239.0), false));
        assert!(enlarge_offered(area, at(330.0), true));
        assert!(!enlarge_offered(area, at(329.0), true));
    }

    #[test]
    fn the_offer_needs_a_gain_of_at_least_1_2x() {
        let area = TilePlanInput {
            avail_w: 1096.0,
            avail_h: 400.0,
            ..input(4)
        };
        let next = plan_tiles(area, 1);
        assert_eq!(2.0 * next.tile_w + area.gap, 540.0, "premise");
        let at = |tile_w: f64| TilePlan {
            tile_w,
            ..plan_tiles(area, 0)
        };
        assert!(enlarge_offered(area, at(450.0), false), "exactly 1.2x");
        assert!(!enlarge_offered(area, at(451.0), false), "just under 1.2x");
    }

    #[test]
    fn only_numeric_painted_tiles_have_a_session_to_lid() {
        let enlarged = EnlargedTiles {
            rank: HashMap::from([
                ("7".to_string(), 0),
                ("mock-1".to_string(), 1),
                ("12".to_string(), 2),
            ]),
            offered: false,
        };
        assert_eq!(enlarged.painted_sessions(), HashSet::from([7, 12]));
        assert!(EnlargedTiles::default().painted_sessions().is_empty());
    }

    #[test]
    fn a_tile_the_plan_could_not_paint_is_never_offered() {
        let mut unpainted = 0;
        for n in 2..=24 {
            for wanted in 1..=n {
                let area = TilePlanInput {
                    avail_w: 620.0,
                    avail_h: 400.0,
                    vw: 660.0,
                    ..input(n)
                };
                let plan = plan_tiles(area, wanted);
                if plan.enlarged < wanted {
                    unpainted += 1;
                    assert!(!enlarge_offered(area, plan, false), "n={n} {plan:?}");
                }
            }
        }
        assert!(unpainted > 0, "premise: the floor must bind somewhere");
    }

    #[test]
    fn the_render_order_of_enlarged_tiles_picks_who_is_painted() {
        let tiles: Vec<(String, TileRenderMode)> = ["8", "7", "9", "6", "4", "3"]
            .map(|id| (id.to_string(), TileRenderMode::Decoded))
            .to_vec();
        let requested = |sessions: &[(&str, usize)]| -> Vec<(String, usize)> {
            sessions.iter().map(|(s, r)| (s.to_string(), *r)).collect()
        };
        let floor_at_two = TilePlanInput {
            avail_w: 700.0,
            avail_h: 330.0,
            vw: 740.0,
            ..input(6)
        };
        assert_eq!(plan_tiles(floor_at_two, 3).enlarged, 2, "premise");

        let (plan, enlarged) = plan_tile_grid(
            floor_at_two,
            &tiles,
            &requested(&[("9", 2), ("8", 0), ("7", 1), ("5", 3)]),
            false,
        );
        assert_eq!(
            plan.enlarged, 2,
            "a peer who left the render list is not counted"
        );
        assert_eq!(
            enlarged.rank,
            HashMap::from([("8".to_string(), 0), ("7".to_string(), 1)]),
            "only the painted tiles get an enlarged `order`"
        );
        assert!(
            !enlarged.offered,
            "the tile it could not paint gets no control"
        );

        let (plan, enlarged) = plan_tile_grid(input(6), &tiles, &requested(&[("7", 0)]), false);
        assert_eq!(plan.enlarged, 1);
        assert_eq!(enlarged.rank, HashMap::from([("7".to_string(), 0)]));
        assert!(enlarged.offered, "a second tile still fits");
    }

    #[test]
    fn the_grid_plan_enlarges_nothing_in_the_split_or_at_full_bleed() {
        let tiles = vec![
            ("7".to_string(), TileRenderMode::Decoded),
            ("8".to_string(), TileRenderMode::Decoded),
        ];
        let requested = [("7".to_string(), 0)];
        let (plan, enlarged) = plan_tile_grid(input(2), &tiles, &requested, false);
        assert_eq!(plan.enlarged, 1, "premise");
        assert!(!enlarged.rank.is_empty(), "premise");
        for off in [
            TilePlanInput {
                split_layout: true,
                ..input(2)
            },
            TilePlanInput {
                full_bleed: true,
                ..input(2)
            },
        ] {
            let (plan, enlarged) = plan_tile_grid(off, &tiles, &requested, false);
            assert_eq!(plan.enlarged, 0, "{off:?}");
            assert_eq!(enlarged, EnlargedTiles::default(), "{off:?}");
        }
    }

    #[test]
    fn the_enlarged_lid_is_two_painted_cells_and_a_gap() {
        let plan = TilePlan {
            cols: 4,
            rows: 2,
            tile_w: 300.0,
            enlarged: 1,
        };
        assert_eq!(enlarged_tile_device_px_h(plan, 16.0, 1.0), 416);
        assert_eq!(enlarged_tile_device_px_h(plan, 16.0, 2.0), 832);
    }

    // -- screen_share_stage_tile_size ---------------------------------

    #[test]
    fn ss_stage_tile_matches_the_single_full_area_tile() {
        // Landscape meeting area (1280x720 viewport minus grid padding:
        // avail_w = 1280-40 = 1240, avail_h = 720-140 = 580 — the exact
        // dimensions the screen-share E2E harness runs at).
        let (tw, th) = screen_share_stage_tile_size(1240.0, 580.0);
        // A 3:2 tile filling the 580px height is 870px wide, which fits in
        // 1240px, so height is the binding constraint.
        assert!((th - 580.0).abs() < 0.5, "th was {th}");
        assert!((tw - 870.0).abs() < 0.5, "tw was {tw}");
        let (_c, _r, grid_tw) = compute_layout(1, 1240.0, 580.0, 16.0);
        let grid_th = grid_tw / TILE_AR;
        assert!((tw - grid_tw).abs() < 0.5, "tw {tw} != grid_tw {grid_tw}");
        assert!((th - grid_th).abs() < 0.5, "th {th} != grid_th {grid_th}");
    }

    #[test]
    fn ss_stage_tile_is_independent_of_participant_count() {
        let (_tw, th_stage) = screen_share_stage_tile_size(1240.0, 580.0);
        let (_c, _r, grid_tw_9) = compute_layout(9, 1240.0, 580.0, 16.0);
        let grid_th_9 = grid_tw_9 / TILE_AR;
        assert!(
            grid_th_9 < 250.0,
            "9-tile grid th unexpectedly large: {grid_th_9}"
        );
        assert!(
            th_stage > grid_th_9 + 100.0,
            "stage th {th_stage} not clearly larger than 9-tile grid th {grid_th_9}"
        );
        assert!(th_stage >= 293.0, "stage th {th_stage}");
    }

    #[test]
    fn ss_stage_tile_caps_width_in_tall_narrow_viewport() {
        // Portrait/narrow area: a 3:2 tile of full height would overflow the
        // width, so width binds and height derives from it.
        let (tw, th) = screen_share_stage_tile_size(300.0, 1000.0);
        assert!((tw - 300.0).abs() < 0.5, "tw was {tw}");
        assert!((th - 200.0).abs() < 0.5, "th was {th}");
    }

    #[test]
    fn ss_stage_tile_never_negative() {
        // Degenerate collapsed viewport must not produce negative sizes.
        let (tw, th) = screen_share_stage_tile_size(0.0, 0.0);
        assert!(tw >= 0.0 && th >= 0.0, "got ({tw}, {th})");
    }

    // -- promote_speakers ---------------------------------------------

    fn make_tiles(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("peer_{i}")).collect()
    }

    #[test]
    fn promote_no_overflow() {
        let mut tiles = make_tiles(4);
        let original = tiles.clone();
        promote_speakers(
            &mut tiles,
            4, // visible_count == len → no overflow
            &HashMap::new(),
            &HashMap::new(),
            1000.0,
            500.0,
        );
        assert_eq!(tiles, original);
    }

    #[test]
    fn promote_overflow_no_active_speakers() {
        let mut tiles = make_tiles(6);
        let original = tiles.clone();
        // No one in speech_map → no active overflow speakers → no swaps.
        promote_speakers(
            &mut tiles,
            3,
            &HashMap::new(),
            &HashMap::new(),
            1000.0,
            500.0,
        );
        assert_eq!(tiles, original);
    }

    #[test]
    fn promote_single_overflow_speaker() {
        // 5 tiles, 3 visible. peer_4 (index 4) is speaking.
        let mut tiles = make_tiles(5);
        let mut speech = HashMap::new();
        speech.insert("peer_4".into(), 900.0); // spoke at 900, now=1000, active_ms=500 → active

        let join = HashMap::new();
        promote_speakers(&mut tiles, 3, &speech, &join, 1000.0, 500.0);

        // peer_4 should now be in the visible portion (index 0..3)
        let visible = &tiles[..3];
        assert!(
            visible.contains(&"peer_4".to_string()),
            "Active overflow speaker should be promoted into visible set. tiles: {tiles:?}"
        );
    }

    #[test]
    fn promote_displaces_least_recently_active() {
        // 4 tiles, 2 visible. peer_0 joined at 100, peer_1 joined at 200.
        // peer_3 (overflow) is speaking.
        // peer_0 has the lower effective timestamp → should be displaced.
        let mut tiles = make_tiles(4);
        let mut speech = HashMap::new();
        speech.insert("peer_3".into(), 950.0);

        let mut join = HashMap::new();
        join.insert("peer_0".into(), 100.0);
        join.insert("peer_1".into(), 200.0);

        promote_speakers(&mut tiles, 2, &speech, &join, 1000.0, 500.0);

        let visible = &tiles[..2];
        assert!(
            visible.contains(&"peer_3".to_string()),
            "Overflow speaker should be promoted. tiles: {tiles:?}"
        );
        assert!(
            !visible.contains(&"peer_0".to_string()),
            "Least-recently-active visible peer should be displaced. tiles: {tiles:?}"
        );
        assert!(
            visible.contains(&"peer_1".to_string()),
            "More-recently-active visible peer should stay. tiles: {tiles:?}"
        );
    }

    #[test]
    fn promote_multiple_overflow_speakers_limited_by_candidates() {
        // 6 tiles, 2 visible. peer_0 and peer_1 are both visible non-speakers.
        // peer_3, peer_4, peer_5 are all overflow speakers.
        // Only 2 candidates → only 2 swaps (most recent overflow speakers win).
        let mut tiles = make_tiles(6);
        let mut speech = HashMap::new();
        speech.insert("peer_3".into(), 800.0);
        speech.insert("peer_4".into(), 900.0);
        speech.insert("peer_5".into(), 950.0);

        promote_speakers(&mut tiles, 2, &speech, &HashMap::new(), 1000.0, 500.0);

        let visible = &tiles[..2];
        // peer_5 (most recent) and peer_4 should be promoted.
        assert!(
            visible.contains(&"peer_5".to_string()),
            "Most recent overflow speaker should be promoted. tiles: {tiles:?}"
        );
        assert!(
            visible.contains(&"peer_4".to_string()),
            "Second most recent overflow speaker should be promoted. tiles: {tiles:?}"
        );
        // peer_3 (least recent) stays in overflow.
        assert!(
            !visible.contains(&"peer_3".to_string()),
            "Least recent overflow speaker should remain in overflow. tiles: {tiles:?}"
        );
    }

    #[test]
    fn promote_all_visible_are_active_speakers() {
        let mut tiles = make_tiles(4);
        let mut speech = HashMap::new();
        speech.insert("peer_0".into(), 950.0);
        speech.insert("peer_1".into(), 960.0);
        speech.insert("peer_3".into(), 970.0);

        let original = tiles.clone();
        promote_speakers(&mut tiles, 2, &speech, &HashMap::new(), 1000.0, 500.0);
        assert_eq!(
            tiles, original,
            "a 20 ms-fresher overflow speaker is under the margin, so no swap"
        );
    }

    #[test]
    fn promote_fallback_margin_brackets_five_seconds() {
        let now = 1_000_000.0;
        let visible_ts = now - 20_000.0;
        let run = |overflow_ts: f64| {
            let mut tiles = make_tiles(4);
            let mut speech = HashMap::new();
            speech.insert("peer_0".into(), visible_ts);
            speech.insert("peer_1".into(), now - 1_000.0);
            speech.insert("peer_3".into(), overflow_ts);
            promote_speakers(&mut tiles, 2, &speech, &HashMap::new(), now, 30_000.0);
            tiles
        };

        let declined = run(visible_ts + 4_999.0);
        assert!(
            !declined[..2].contains(&"peer_3".to_string()),
            "4 999 ms fresher is under the 5 000 ms margin: no swap. tiles: {declined:?}"
        );

        let promoted = run(visible_ts + 5_001.0);
        assert!(
            promoted[..2].contains(&"peer_3".to_string()),
            "5 001 ms fresher clears the 5 000 ms margin: swap. tiles: {promoted:?}"
        );
    }

    #[test]
    fn promote_fallback_pairs_freshest_overflow_with_stalest_visible() {
        // Asserted on the whole vector so the PAIRING is pinned by position, not
        // membership: freshest overflow takes the stalest visible speaker's slot.
        let now = 1_000_000.0;
        let mut tiles = make_tiles(6);
        let mut speech = HashMap::new();
        speech.insert("peer_0".into(), now - 20_000.0); // visible, 2nd stalest
        speech.insert("peer_1".into(), now - 2_000.0); // visible, freshest
        speech.insert("peer_2".into(), now - 28_000.0); // visible, stalest
        speech.insert("peer_4".into(), now - 100.0); // overflow, freshest
        speech.insert("peer_5".into(), now - 10_000.0); // overflow, 2nd

        promote_speakers(&mut tiles, 3, &speech, &HashMap::new(), now, 30_000.0);

        // peer_1, the freshest visible speaker, is never displaced.
        assert_eq!(
            tiles,
            vec!["peer_5", "peer_1", "peer_4", "peer_3", "peer_2", "peer_0"],
            "freshest overflow must displace the stalest visible speaker"
        );
    }

    #[test]
    fn promote_falls_back_to_stalest_visible_speaker() {
        // Open mics: every VISIBLE tile is inside the 30 s window, so pre-#2273
        // `swap_candidates` was empty and nobody was promoted.
        let now = 1_000_000.0;
        let mut tiles = make_tiles(4);
        let mut speech = HashMap::new();
        speech.insert("peer_0".into(), now - 25_000.0); // visible, long silent
        speech.insert("peer_1".into(), now - 1_000.0); // visible, still talking
        speech.insert("peer_3".into(), now - 200.0); // overflow, talking NOW

        promote_speakers(&mut tiles, 2, &speech, &HashMap::new(), now, 30_000.0);

        let visible = &tiles[..2];
        assert!(
            visible.contains(&"peer_3".to_string()),
            "the peer talking now must be promoted. tiles: {tiles:?}"
        );
        assert!(
            !visible.contains(&"peer_0".to_string()),
            "the stalest visible speaker is the one displaced. tiles: {tiles:?}"
        );
        assert!(
            visible.contains(&"peer_1".to_string()),
            "a currently-talking visible peer must not be displaced. tiles: {tiles:?}"
        );
    }

    #[test]
    fn promote_fallback_declines_inside_the_margin() {
        // Anti-flap (#1923): three talkers inside one margin window; nothing moves.
        let now = 1_000_000.0;
        let mut tiles = make_tiles(4);
        let mut speech = HashMap::new();
        speech.insert("peer_0".into(), now - 2_000.0);
        speech.insert("peer_1".into(), now - 1_500.0);
        speech.insert("peer_3".into(), now - 100.0);

        let original = tiles.clone();
        promote_speakers(&mut tiles, 2, &speech, &HashMap::new(), now, 30_000.0);
        assert_eq!(
            tiles, original,
            "concurrent talkers inside the margin must not rotate the grid"
        );
    }

    #[test]
    fn promote_fallback_does_not_ping_pong() {
        // One-way: re-running on its own output must be a fixed point.
        let now = 1_000_000.0;
        let mut tiles = make_tiles(4);
        let mut speech = HashMap::new();
        speech.insert("peer_0".into(), now - 25_000.0);
        speech.insert("peer_1".into(), now - 1_000.0);
        speech.insert("peer_3".into(), now - 200.0);

        promote_speakers(&mut tiles, 2, &speech, &HashMap::new(), now, 30_000.0);
        let after_first = tiles.clone();
        promote_speakers(&mut tiles, 2, &speech, &HashMap::new(), now, 30_000.0);
        assert_eq!(tiles, after_first, "promotion must be idempotent");
    }

    #[test]
    fn promote_fallback_is_inert_while_a_silent_visible_tile_exists() {
        let now = 1_000_000.0;
        let mut tiles = make_tiles(4);
        let mut speech = HashMap::new();
        speech.insert("peer_0".into(), now - 25_000.0);
        speech.insert("peer_3".into(), now - 200.0);

        promote_speakers(&mut tiles, 2, &speech, &HashMap::new(), now, 30_000.0);

        let visible = &tiles[..2];
        assert!(visible.contains(&"peer_3".to_string()), "tiles: {tiles:?}");
        assert!(
            visible.contains(&"peer_0".to_string()),
            "a silent visible tile must be displaced before a speaking one. tiles: {tiles:?}"
        );
    }

    #[test]
    fn promote_ties_are_deterministic() {
        // Two overflow speakers with identical timestamps.
        // Result should be deterministic (iteration order preserved).
        let mut tiles = make_tiles(5);
        let mut speech = HashMap::new();
        speech.insert("peer_3".into(), 900.0);
        speech.insert("peer_4".into(), 900.0); // same timestamp

        let mut tiles2 = tiles.clone();
        promote_speakers(&mut tiles, 3, &speech, &HashMap::new(), 1000.0, 500.0);
        promote_speakers(&mut tiles2, 3, &speech, &HashMap::new(), 1000.0, 500.0);
        assert_eq!(
            tiles, tiles2,
            "Identical inputs must produce identical outputs"
        );
    }

    // -- compute_effective_density ------------------------------------

    // Desktop viewport for tests.
    const VW: f64 = 1366.0;
    const AVAIL_W: f64 = 1300.0;
    const AVAIL_H: f64 = 700.0;
    const GAP: f64 = 8.0;

    #[test]
    fn density_no_active_speakers_returns_user_mode() {
        let result = compute_effective_density(
            DensityMode::Standard,
            20,
            AVAIL_W,
            AVAIL_H,
            GAP,
            0, // no active speakers
            20,
            VW,
        );
        assert_eq!(result, DensityMode::Standard);
    }

    #[test]
    fn density_user_mode_fits_all_speakers() {
        // Standard mode can fit ~9 tiles on desktop. 3 active speakers → no escalation.
        let result =
            compute_effective_density(DensityMode::Standard, 9, AVAIL_W, AVAIL_H, GAP, 3, 9, VW);
        assert_eq!(result, DensityMode::Standard);
    }

    #[test]
    fn density_escalates_when_user_mode_too_sparse() {
        // Standard mode fits ~9 on desktop. If we have 20 tiles with 15
        // active speakers, Standard can't show them all → must escalate.
        let result =
            compute_effective_density(DensityMode::Standard, 20, AVAIL_W, AVAIL_H, GAP, 15, 20, VW);
        assert_ne!(
            result,
            DensityMode::Standard,
            "Should escalate past Standard when 15 speakers can't fit"
        );
        // The result should be denser than Standard.
        let rank = |m: DensityMode| -> usize {
            [
                DensityMode::Standard,
                DensityMode::Auto,
                DensityMode::Dense,
                DensityMode::Maximum,
            ]
            .iter()
            .position(|&x| x == m)
            .unwrap()
        };
        assert!(rank(result) > rank(DensityMode::Standard));
    }

    #[test]
    fn density_maximum_when_nothing_else_fits() {
        // Even Dense can't fit 50 speakers → should return Maximum.
        let result =
            compute_effective_density(DensityMode::Standard, 50, AVAIL_W, AVAIL_H, GAP, 50, 50, VW);
        assert_eq!(result, DensityMode::Maximum);
    }

    #[test]
    fn density_already_at_maximum_stays() {
        let result =
            compute_effective_density(DensityMode::Maximum, 20, AVAIL_W, AVAIL_H, GAP, 15, 20, VW);
        assert_eq!(result, DensityMode::Maximum);
    }

    // -- presenter-aware shedding: active-speaker exemption (issue #1559) -----
    //
    // Presenter-aware shedding LOWERS the decode-budget cap (and hence
    // `visible_count`) while screen-sharing under pressure. The active-speaker
    // exemption is delivered by `promote_speakers` running against that LOWER
    // `visible_count`: an active speaker ranked beyond the shrunken decoded
    // window is swapped INWARD, displacing a NON-speaking visible tile — so the
    // presenter still sees who is talking while non-speaker thumbnails are shed
    // first. This pins that the exemption holds at the smaller cap the presenter
    // bias produces.

    #[test]
    fn presenter_shrunk_window_still_retains_active_speaker() {
        // 6 peers. Without sharing the budget would decode (say) 4; under a
        // presenter shed the visible window shrinks to 2. peer_5 (overflow) is an
        // ACTIVE speaker; peer_0 / peer_1 (visible) are NOT speaking.
        let mut tiles = make_tiles(6);
        let mut speech = HashMap::new();
        speech.insert("peer_5".into(), 950.0); // now=1000, active_ms=500 → active
        let join = HashMap::new();

        // Lowered (presenter) visible window == 2.
        promote_speakers(&mut tiles, 2, &speech, &join, 1000.0, 500.0);

        // The active speaker is retained INSIDE the shrunken decoded window even
        // though it ranked at index 5 (beyond the cap). This is the exemption: a
        // presenter still decodes whoever is talking.
        assert!(
            tiles[..2].contains(&"peer_5".to_string()),
            "an active speaker must stay decoded even at the shrunken presenter cap. tiles: {tiles:?}"
        );
        // A NON-speaking tile is the one shed out of the decoded window — the
        // off-screen thumbnail is dropped first, not the speaker.
        let shed_first = tiles[2..]
            .iter()
            .any(|t| speech.get(t).is_none_or(|&ts| 1000.0 - ts >= 500.0));
        assert!(
            shed_first,
            "a non-speaking tile is shed out of the decoded window before the active speaker"
        );
    }

    #[test]
    fn presenter_shed_keeps_multiple_speakers_drops_silent_thumbnails() {
        // 6 peers, presenter window shrunk to 2. TWO overflow speakers
        // (peer_4, peer_5); peer_0/peer_1 visible and silent. Both speakers
        // should be promoted, displacing both silent visible tiles.
        let mut tiles = make_tiles(6);
        let mut speech = HashMap::new();
        speech.insert("peer_4".into(), 900.0);
        speech.insert("peer_5".into(), 950.0);
        let join = HashMap::new();

        promote_speakers(&mut tiles, 2, &speech, &join, 1000.0, 500.0);

        let visible = &tiles[..2];
        assert!(
            visible.contains(&"peer_4".to_string()) && visible.contains(&"peer_5".to_string()),
            "both active speakers retained at the shrunken presenter cap. tiles: {tiles:?}"
        );
        // The displaced silent peers fall OUT of the decoded window.
        assert!(
            tiles[2..].contains(&"peer_0".to_string())
                && tiles[2..].contains(&"peer_1".to_string()),
            "silent visible thumbnails are shed first. tiles: {tiles:?}"
        );
    }

    use crate::components::decode_budget::partition_camera_tiles;

    /// `CANVAS_LIMIT` from `constants.rs`, pinned so the fixture stays 46-vs-30.
    const CUT: usize = 30;
    const ACTIVE_MS: f64 = 30_000.0;

    fn bloated_roster() -> (Vec<String>, HashMap<String, f64>) {
        let peers: Vec<String> = (1..=46).map(|i| i.to_string()).collect();
        let join = peers
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), 1_000.0 + i as f64))
            .collect();
        (peers, join)
    }

    #[test]
    fn selection_keeps_late_session_id_camera_and_speaking_peers() {
        // Camera-on peers and the active speaker all sort LATE by session_id —
        // exactly the peers a plain `take(30)` drops before #1465 can see them.
        let (peers, join) = bloated_roster();
        let now = 1_000_000.0;
        let cameras_on = ["41", "43", "45"];
        let speaker = "46";
        let mut speech = HashMap::new();
        speech.insert(speaker.to_string(), now - 500.0);

        let selected = select_display_candidates(
            &peers,
            CUT,
            |p| cameras_on.contains(&p),
            |_| None,
            &speech,
            &join,
            now,
            ACTIVE_MS,
        );

        assert_eq!(selected.len(), CUT, "the cut still caps at CANVAS_LIMIT");
        let ids: Vec<&str> = selected.iter().map(|(p, _)| p.as_str()).collect();
        for late in cameras_on {
            assert!(
                ids.contains(&late),
                "camera-on peer {late} was shed by the session_id cut. selected: {ids:?}"
            );
        }
        assert!(
            ids.contains(&speaker),
            "the active speaker was shed by the session_id cut. selected: {ids:?}"
        );

        let (camera_on_real, camera_off_real) = partition_camera_tiles(&selected);
        for late in cameras_on {
            assert!(
                camera_on_real.contains(&late.to_string()),
                "camera-on peer {late} missing from camera_on_real: {camera_on_real:?}"
            );
        }
        assert!(
            camera_off_real.contains(&speaker.to_string()),
            "speaker missing from camera_off_real: {camera_off_real:?}"
        );
    }

    #[test]
    fn selection_is_inert_when_the_cut_does_not_bind() {
        // Anti-flap (#1923): camera/speech must not reshuffle a grid that fits.
        let peers: Vec<String> = (1..=12).map(|i| i.to_string()).collect();
        let join: HashMap<String, f64> = peers
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), 1_000.0 + i as f64))
            .collect();
        let now = 1_000_000.0;
        let mut speech = HashMap::new();
        speech.insert("12".to_string(), now - 100.0);

        let selected = select_display_candidates(
            &peers,
            peers.len(),
            |p| p == "11" || p == "12",
            |_| None,
            &speech,
            &join,
            now,
            ACTIVE_MS,
        );

        let ids: Vec<String> = selected.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(ids, peers, "roster order must survive a non-binding cut");
    }

    #[test]
    fn selection_ranks_live_speaker_above_stale_ghost_session() {
        // #2267 ghosts keep the timestamp they had when they went away, and sort
        // FIRST by both session_id and join time.
        let now = 1_000_000.0;
        let peers = vec!["7".to_string(), "88".to_string()];
        let mut join = HashMap::new();
        join.insert("7".to_string(), 1_000.0); // ghost joined first
        join.insert("88".to_string(), 9_000.0);
        let mut speech = HashMap::new();
        speech.insert("7".to_string(), now - 25_000.0); // frozen, still inside 30 s
        speech.insert("88".to_string(), now - 300.0); // live speaker

        let selected = select_display_candidates(
            &peers,
            1,
            |_| false,
            |_| None,
            &speech,
            &join,
            now,
            ACTIVE_MS,
        );

        assert_eq!(
            selected.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            vec!["88"],
            "a live speaker must outrank a stale duplicate session"
        );
    }

    #[test]
    fn selection_prefers_camera_on_over_a_silent_camera_off_peer() {
        let (peers, join) = bloated_roster();
        let now = 1_000_000.0;
        let selected = select_display_candidates(
            &peers,
            CUT,
            |p| p == "46",
            |_| None,
            &HashMap::new(),
            &join,
            now,
            ACTIVE_MS,
        );
        let ids: Vec<&str> = selected.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(ids[0], "46", "the only camera-on peer must lead. {ids:?}");
        assert!(
            !ids.contains(&"30"),
            "the silent camera-off tail is what gets shed. {ids:?}"
        );
    }

    #[test]
    fn selection_keeps_every_pin_ahead_of_speakers_and_cameras() {
        let (peers, join) = bloated_roster();
        let now = 1_000_000.0;
        let mut speech = HashMap::new();
        speech.insert("45".to_string(), now - 100.0);
        let rank = |p: &str| match p {
            "46" => Some(0),
            "44" => Some(1),
            _ => None,
        };
        let selected = select_display_candidates(
            &peers,
            2,
            |p| p == "45",
            rank,
            &speech,
            &join,
            now,
            ACTIVE_MS,
        );
        let ids: Vec<&str> = selected.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            ids,
            ["46", "44"],
            "a silent camera-off pin outranks a speaking camera-on peer at the cut"
        );
    }

    fn window(
        pins: &[&str],
        on: &[&str],
        budget: usize,
        requested: &[&str],
        ceiling: Option<usize>,
        speech: &HashMap<String, f64>,
    ) -> CameraWindow {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();
        let join: HashMap<String, f64> = on
            .iter()
            .enumerate()
            .map(|(i, p)| (p.to_string(), 1.0 + i as f64))
            .collect();
        let requested: HashSet<String> = requested.iter().map(|s| s.to_string()).collect();
        plan_camera_window(
            CameraWindowInput {
                pins: &ids(pins),
                camera_on: ids(on),
                camera_off: Vec::new(),
                displayed: on.len(),
                budget,
                device_ceiling: ceiling,
                requested: &requested,
            },
            &SpeakerInputs {
                speech_map: speech,
                join_map: &join,
                now_ms: 1_000_000.0,
                active_ms: ACTIVE_MS,
            },
        )
    }

    fn decoded(ids: &[u64]) -> HashSet<u64> {
        ids.iter().copied().collect()
    }

    #[test]
    fn play_decodes_the_requested_tile_when_pins_fill_the_budget() {
        let quiet = HashMap::new();
        let w = window(
            &["11", "12", "13"],
            &["11", "12", "13", "15"],
            1,
            &["15"],
            None,
            &quiet,
        );
        assert_eq!(w.bucket, decoded(&[11, 15]));
        let w = window(
            &["11", "12", "13", "14"],
            &["11", "12", "13", "14", "15"],
            2,
            &["14"],
            None,
            &quiet,
        );
        assert_eq!(
            w.bucket,
            decoded(&[11, 12, 14]),
            "PLAY on a paused pin decodes that pin"
        );
        assert_eq!(w.decoded, 3);
    }

    #[test]
    fn a_play_request_never_takes_a_budget_decoded_pin() {
        let w = window(
            &["11", "12"],
            &["11", "12", "13", "14", "15"],
            2,
            &["14", "15"],
            Some(3),
            &HashMap::new(),
        );
        assert_eq!(
            w.bucket,
            decoded(&[11, 12, 14]),
            "the device ceiling admits one request, and it is not a pin's slot"
        );
    }

    #[test]
    fn an_overflow_speaker_never_takes_a_pin_decode() {
        let speech: HashMap<String, f64> = [("13".to_string(), 1_000_000.0 - 100.0)].into();
        let w = window(&["11"], &["11", "12", "13"], 2, &[], None, &speech);
        assert_eq!(w.tiles[0], "11");
        assert_eq!(
            w.bucket,
            decoded(&[11, 13]),
            "the speaker displaces the silent unpinned tile"
        );
    }

    #[test]
    fn a_play_request_in_the_overflow_does_not_widen_the_window() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();
        let plan = |requested: &[&str]| {
            plan_camera_window(
                CameraWindowInput {
                    pins: &[],
                    camera_on: ids(&["1", "2", "3", "4", "5"]),
                    camera_off: Vec::new(),
                    displayed: 3,
                    budget: 2,
                    device_ceiling: None,
                    requested: &requested.iter().map(|s| s.to_string()).collect(),
                },
                &SpeakerInputs {
                    speech_map: &HashMap::new(),
                    join_map: &HashMap::new(),
                    now_ms: 0.0,
                    active_ms: ACTIVE_MS,
                },
            )
        };
        let folded = plan(&["5"]);
        assert_eq!(
            folded.decoded, 2,
            "tile 5 is in the +N overflow (issue 1466)"
        );
        assert_eq!(folded.bucket, decoded(&[1, 2]));
        assert_eq!(
            plan(&["3"]).decoded,
            3,
            "premise: a displayed avatar does widen it"
        );
    }

    #[test]
    fn a_camera_off_pin_takes_a_cell_from_the_camera_on_window() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();
        let w = plan_camera_window(
            CameraWindowInput {
                pins: &ids(&["8"]),
                camera_on: ids(&["1", "2", "3"]),
                camera_off: ids(&["9", "8"]),
                displayed: 3,
                budget: 3,
                device_ceiling: None,
                requested: &HashSet::new(),
            },
            &SpeakerInputs {
                speech_map: &HashMap::new(),
                join_map: &HashMap::new(),
                now_ms: 0.0,
                active_ms: ACTIVE_MS,
            },
        );
        assert_eq!(w.camera_off, ids(&["8", "9"]));
        assert_eq!((w.on_cells, w.decoded), (2, 2));
        assert_eq!(w.bucket, decoded(&[1, 2]));
    }

    #[test]
    fn selection_tier_ranks_speech_above_camera_state() {
        assert!(
            selection_tier(true, true) < selection_tier(false, true),
            "among peers inside the speech window, camera-on leads"
        );
        assert!(
            selection_tier(false, true) < selection_tier(true, false),
            "a peer inside the speech window outranks any silent peer (issue 2273)"
        );
        assert!(
            selection_tier(true, false) < selection_tier(false, false),
            "among silent peers, camera-on leads"
        );
    }

    #[test]
    fn selection_keeps_a_live_camera_off_speaker_when_cameras_fill_the_cut() {
        let (peers, join) = bloated_roster();
        let now = 1_000_000.0;
        let cams: Vec<String> = (1..=CUT).map(|i| i.to_string()).collect();
        let mut speech = HashMap::new();
        speech.insert("46".to_string(), now - 100.0);

        let selected = select_display_candidates(
            &peers,
            CUT,
            |p| cams.iter().any(|c| c == p),
            |_| None,
            &speech,
            &join,
            now,
            ACTIVE_MS,
        );

        let ids: Vec<&str> = selected.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(ids.len(), CUT, "the cut still caps at CANVAS_LIMIT");
        assert!(
            ids.contains(&"46"),
            "a peer speaking NOW was shed by {CUT} silent camera-on peers. selected: {ids:?}"
        );
        assert!(
            !ids.contains(&"30"),
            "the latest-joining silent camera-on peer is the shed victim. selected: {ids:?}"
        );
    }

    #[test]
    fn camera_off_window_leads_with_recent_speakers() {
        let now = 1_000_000.0;
        let mut off: Vec<String> = (1..=5).map(|i| i.to_string()).collect();
        let join: HashMap<String, f64> = off
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), 1_000.0 + i as f64))
            .collect();
        let mut speech = HashMap::new();
        speech.insert("5".to_string(), now - 200.0);
        speech.insert("4".to_string(), now - 9_000.0);
        speech.insert("2".to_string(), now - 60_000.0); // outside the window

        sort_camera_off_window(&mut off, &speech, &join, now, ACTIVE_MS);

        assert_eq!(off, vec!["5", "4", "1", "2", "3"], "got {off:?}");
    }

    #[test]
    fn camera_off_window_keeps_join_order_with_no_speakers() {
        let now = 1_000_000.0;
        let mut off: Vec<String> = vec!["30".into(), "4".into(), "17".into()];
        let mut join = HashMap::new();
        join.insert("30".to_string(), 1_000.0);
        join.insert("4".to_string(), 2_000.0);
        join.insert("17".to_string(), 3_000.0);

        sort_camera_off_window(&mut off, &HashMap::new(), &join, now, ACTIVE_MS);

        assert_eq!(off, vec!["30", "4", "17"], "join order must be preserved");
    }

    fn desktop_state(peer_list_open: bool, diagnostics_open: bool, chat_open: bool) -> DrawerState {
        DrawerState {
            vw: 1280.0,
            peer_list_open,
            left_w: 320.0,
            diagnostics_open,
            right_w: 560.0,
            chat_open,
            chat_w: CHAT_DRAWER_WIDTH,
        }
    }

    fn close_to(actual: f64, expected: f64, what: &str) {
        assert!(
            (actual - expected).abs() < 0.5,
            "{what}: got {actual}, want {expected}"
        );
    }

    #[test]
    fn drawer_reserves_chat_and_diagnostics_shrinks_diagnostics_to_budget() {
        let r = drawer_reserves(desktop_state(false, true, true));
        close_to(r.diag_render_w, 408.0, "diagnostics render width");
        close_to(r.chat_render_w, 360.0, "chat render width");
        close_to(r.right_reserve, 768.0, "right reserve");
        close_to(r.left_reserve, 0.0, "left reserve");
        close_to(r.chat_right_offset, 408.0, "chat right offset");
    }

    #[test]
    fn drawer_reserves_all_three_open_lands_on_the_floors() {
        let r = drawer_reserves(desktop_state(true, true, true));
        close_to(r.left_render_w, 300.0, "peer list render width");
        close_to(r.chat_render_w, 360.0, "chat render width");
        close_to(r.diag_render_w, 300.0, "diagnostics render width");
        close_to(r.left_reserve, 300.0, "left reserve");
        close_to(r.right_reserve, 660.0, "right reserve");
        assert!(r.left_reserve + r.right_reserve > 1280.0 * 0.6);
    }

    #[test]
    fn drawer_reserves_single_drawer_reserves_only_its_own_side() {
        let left_only = drawer_reserves(desktop_state(true, false, false));
        close_to(left_only.left_reserve, 320.0, "peer-list-only left reserve");
        close_to(left_only.right_reserve, 0.0, "peer-list-only right reserve");

        let right_only = drawer_reserves(desktop_state(false, true, false));
        close_to(
            right_only.left_reserve,
            0.0,
            "diagnostics-only left reserve",
        );
        close_to(
            right_only.right_reserve,
            560.0,
            "diagnostics-only right reserve",
        );
        close_to(right_only.chat_right_offset, 560.0, "chat right offset");
    }

    #[test]
    fn drawer_reserves_are_zero_below_the_reflow_breakpoint() {
        let mobile = DrawerState {
            vw: 375.0,
            peer_list_open: true,
            left_w: 320.0,
            diagnostics_open: true,
            right_w: 560.0,
            chat_open: true,
            chat_w: CHAT_DRAWER_WIDTH,
        };
        let r = drawer_reserves(mobile);
        close_to(r.left_reserve, 0.0, "mobile left reserve");
        close_to(r.right_reserve, 0.0, "mobile right reserve");
        close_to(r.chat_right_offset, 0.0, "mobile chat right offset");
    }

    #[test]
    fn drawer_reserves_shrink_diagnostics_before_the_peer_list() {
        let state = DrawerState {
            vw: 1400.0,
            peer_list_open: true,
            left_w: 320.0,
            diagnostics_open: true,
            right_w: 560.0,
            chat_open: false,
            chat_w: CHAT_DRAWER_WIDTH,
        };
        let r = drawer_reserves(state);
        close_to(r.diag_render_w, 520.0, "diagnostics absorbs the overflow");
        close_to(r.left_render_w, 320.0, "peer list is untouched");
    }

    #[test]
    fn drawer_max_for_side_is_budget_aware() {
        close_to(
            drawer_max_for_side(desktop_state(false, true, true), DrawerSide::Right),
            408.0,
            "diagnostics cap with chat open",
        );
        close_to(
            drawer_max_for_side(desktop_state(true, true, true), DrawerSide::Right),
            300.0,
            "diagnostics cap with chat + peer list open",
        );
        close_to(
            drawer_max_for_side(desktop_state(false, true, false), DrawerSide::Right),
            640.0,
            "diagnostics cap alone",
        );
    }

    #[test]
    fn drawer_max_for_left_only_reserves_the_diagnostics_floor() {
        // 768 - 300, not 768 - 560: the LIVE width would pin it at its floor.
        close_to(
            drawer_max_for_side(desktop_state(true, true, false), DrawerSide::Left),
            468.0,
            "peer-list cap with diagnostics open",
        );
        close_to(
            drawer_max_for_side(desktop_state(true, true, true), DrawerSide::Left),
            300.0,
            "peer-list cap with diagnostics + chat open",
        );
    }

    fn with_chat_w(state: DrawerState, chat_w: f64) -> DrawerState {
        DrawerState { chat_w, ..state }
    }

    #[test]
    fn a_wide_chat_shrinks_last_and_never_below_its_default() {
        let alone = drawer_reserves(with_chat_w(desktop_state(false, false, true), 720.0));
        close_to(alone.chat_render_w, 720.0, "chat alone keeps its width");
        close_to(alone.right_reserve, 720.0, "chat alone reserves its width");

        let beside_diag = drawer_reserves(with_chat_w(desktop_state(false, true, true), 720.0));
        close_to(
            beside_diag.diag_render_w,
            300.0,
            "diagnostics shrinks first",
        );
        close_to(
            beside_diag.chat_render_w,
            468.0,
            "then chat absorbs the rest",
        );
        close_to(beside_diag.chat_right_offset, 300.0, "chat sits inboard");
        close_to(
            beside_diag.right_reserve,
            768.0,
            "the stack fills the budget",
        );

        let beside_both = drawer_reserves(with_chat_w(desktop_state(true, true, true), 720.0));
        close_to(beside_both.left_render_w, 300.0, "peer list at its floor");
        close_to(beside_both.diag_render_w, 300.0, "diagnostics at its floor");
        close_to(
            beside_both.chat_render_w,
            360.0,
            "chat stops at its default",
        );

        let narrow = drawer_reserves(with_chat_w(desktop_state(true, true, true), 300.0));
        close_to(
            narrow.chat_render_w,
            300.0,
            "a narrowed chat is never widened",
        );
        close_to(
            narrow.right_reserve,
            600.0,
            "and reserves only what it paints",
        );
    }

    #[test]
    fn chat_render_width_ignores_the_other_drawers_preferred_widths() {
        let mut vw = CHAT_OVERLAY_MIN_VW;
        while vw <= 1920.0 {
            for chat_w in [DRAWER_MIN_WIDTH, CHAT_DRAWER_WIDTH, 500.0, DRAWER_MAX_ABS] {
                for (p, d) in [(false, false), (true, false), (false, true), (true, true)] {
                    let others = f64::from(u8::from(p) + u8::from(d)) * DRAWER_MIN_WIDTH;
                    let want = chat_w.min((max_total_reserve(vw) - others).max(CHAT_DRAWER_WIDTH));
                    for side_w in [DRAWER_MIN_WIDTH, 560.0, DRAWER_MAX_ABS] {
                        let state = DrawerState {
                            vw,
                            peer_list_open: p,
                            left_w: side_w,
                            diagnostics_open: d,
                            right_w: side_w,
                            chat_open: true,
                            chat_w,
                        };
                        close_to(
                            drawer_reserves(state).chat_render_w,
                            want,
                            &format!("{state:?}"),
                        );
                    }
                }
            }
            vw += 7.0;
        }
    }

    #[test]
    fn the_left_and_right_caps_subtract_the_live_chat_width() {
        let wide = |p: bool, d: bool| DrawerState {
            vw: 1600.0,
            ..with_chat_w(desktop_state(p, d, true), 400.0)
        };
        close_to(
            drawer_max_for_side(wide(false, true), DrawerSide::Right),
            560.0,
            "960 budget less a 400 chat",
        );
        close_to(
            drawer_max_for_side(wide(true, false), DrawerSide::Left),
            560.0,
            "960 budget less a 400 chat",
        );
        close_to(
            drawer_max_for_side(
                with_chat_w(desktop_state(false, true, true), 300.0),
                DrawerSide::Right,
            ),
            468.0,
            "768 budget less a 300 chat",
        );
    }

    /// ADVERSARIAL (mutation): revert one rule to `left: 50%` → named.
    #[test]
    fn every_fixed_centred_surface_tracks_the_dock() {
        let css = include_str!("../../static/style.css");
        let global = include_str!("../../static/global.css");

        /// The `left:` declaration of the rule whose selector line is exactly
        /// `selector {`, with whitespace collapsed so a line break cannot redden
        /// this.
        fn left_decl(css: &str, selector: &str) -> String {
            let head = format!("\n{selector} {{");
            let start = css
                .find(&head)
                .unwrap_or_else(|| panic!("no rule `{selector} {{` in the stylesheet"))
                + head.len();
            let body = &css[start..start + css[start..].find('}').unwrap_or(0)];
            let at = body
                .find("left:")
                .unwrap_or_else(|| panic!("`{selector}` has no `left:`"));
            let decl = &body[at..at + body[at..].find(';').unwrap_or(0)];
            decl.split_whitespace().collect::<Vec<_>>().join(" ")
        }

        let reference = left_decl(global, ".video-controls-container.dock-bottom");
        assert!(
            reference.contains("--drawer-left-reserve"),
            "the dock itself stopped re-centring: {reference}"
        );
        for selector in [
            ".mock-peers-popover",
            ".density-popover",
            ".reactions-palette",
            ".reactions-overlay",
            ".meeting-timer-popover",
            ".host-controls-container",
        ] {
            assert_eq!(
                left_decl(css, selector),
                reference,
                "`{selector}` does not track the dock's centring"
            );
        }

        for (selector, prop) in [
            (".reactions-overlay", "width:"),
            (".host-controls-container", "min-width:"),
            (".host-controls-container", "max-width:"),
        ] {
            let head = format!("\n{selector} {{");
            let start = css.find(&head).unwrap() + head.len();
            let body = &css[start..start + css[start..].find('}').unwrap()];
            let at = body.find(prop).unwrap_or_else(|| {
                panic!("`{selector}` has no `{prop}`");
            });
            let decl: String = body[at..at + body[at..].find(';').unwrap()]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                decl.contains("--drawer-left-reserve") && decl.contains("--drawer-right-reserve"),
                "`{selector}`'s `{prop}` is viewport-sized, so it overhangs the \
                 drawers: {decl}"
            );
        }
    }

    #[test]
    fn the_wrap_rule_never_re_inflates_a_hidden_self_tile() {
        let css = include_str!("../../static/style.css");
        let flat: String = css.split_whitespace().collect::<Vec<_>>().join(" ");

        let collapse = flat
            .split(".host[data-self-hidden=\"true\"] {")
            .nth(1)
            .expect("style.css no longer collapses the hidden self tile");
        let body = collapse.split('}').next().unwrap_or_default();
        assert!(
            body.contains("width: 0;") && body.contains("height: 0;"),
            "premise: hiding the self view is a width/height collapse, so a \
             higher-specificity width/height wins over it: {body}"
        );

        assert!(
            flat.contains(
                "#grid-container[data-tile-flow=\"wrap\"] > \
                 .host[data-self-placement=\"grid\"]:not([data-self-hidden=\"true\"])"
            ),
            "the wrap arm is (1,3,0) and the collapse above is (0,2,0), so \
             without :not([data-self-hidden=\"true\"]) a hidden self view \
             inflates to a full tile over tile one"
        );
        assert!(
            !flat.contains(
                "#grid-container[data-tile-flow=\"wrap\"] > \
                 .host[data-self-placement=\"grid\"],"
            ),
            "an unguarded host arm remains in the wrap rule"
        );
    }

    #[test]
    fn the_drawer_media_queries_track_the_reflow_breakpoint() {
        let css = include_str!("../../static/style.css");

        /// The block body, by brace depth, so a selector is checked INSIDE it.
        fn media_block(css: &str, px: f64) -> String {
            let query = format!("@media (max-width: {px:.2}px)");
            let start = css
                .find(&query)
                .unwrap_or_else(|| panic!("style.css has no `{query}`"));
            let mut depth = 0usize;
            let mut end = start;
            for (i, ch) in css[start..].char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            end = start + i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            css[start..end].to_string()
        }

        let reflow = media_block(css, DRAWER_REFLOW_MIN_VW - 0.02);
        for selector in ["#peer-list-container", "#diagnostics-sidebar"] {
            assert!(
                reflow.contains(selector),
                "{selector} is not in the {:.2}px block, so it no longer becomes \
                 a full-screen overlay at DRAWER_REFLOW_MIN_VW",
                DRAWER_REFLOW_MIN_VW - 0.02
            );
        }
        assert!(
            !reflow.contains("#chat-sidebar"),
            "chat must key off CHAT_OVERLAY_MIN_VW, not the reflow breakpoint"
        );

        let sheet = media_block(css, CHAT_OVERLAY_MIN_VW - 0.02);
        assert!(
            sheet.contains("#chat-sidebar") && sheet.contains("width: 100% !important"),
            "the {:.2}px block no longer makes chat a full-width sheet",
            CHAT_OVERLAY_MIN_VW - 0.02
        );
        let sheet_flat = sheet.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            sheet_flat
                .contains("#chat-sidebar .drawer-resize-handle { display: none !important; }"),
            "the chat resize handle stays grabbable over the full-width sheet"
        );
    }

    #[test]
    fn max_total_reserve_is_capped_by_the_tile_band_on_narrow_viewports() {
        close_to(max_total_reserve(1280.0), 768.0, "1280");
        close_to(max_total_reserve(1400.0), 840.0, "1400");
        close_to(max_total_reserve(900.0), 500.0, "900");
        close_to(max_total_reserve(700.0), 300.0, "700");
        assert!(max_total_reserve(300.0) >= 0.0, "never negative");
    }

    fn opens(vw: f64, kind: DrawerKind, p: bool, d: bool, c: bool) -> DrawersToClose {
        drawers_to_close_on_open(vw, kind, p, d, c)
    }

    const GRID_SIDE_PADDING: f64 = 40.0;

    /// Mirrors the CSS: peer list at `left: 0`, diagnostics at `right: 0`,
    /// chat at `right: chat_right_offset`, or the full-width sheet.
    fn open_drawer_rects(state: DrawerState, r: DrawerReserves) -> Vec<(&'static str, f64, f64)> {
        let mut out = Vec::new();
        if state.peer_list_open {
            out.push(("peer list", 0.0, r.left_render_w));
        }
        if state.diagnostics_open {
            out.push(("diagnostics", state.vw - r.diag_render_w, state.vw));
        }
        if state.chat_open {
            if chat_is_sheet(state.vw) {
                out.push(("chat sheet", 0.0, state.vw));
            } else {
                let right = state.vw - r.chat_right_offset;
                out.push(("chat", right - r.chat_render_w, right));
            }
        }
        out
    }
    const MIN_TILE_AREA: f64 = MIN_TILE_BAND - GRID_SIDE_PADDING;

    /// ADVERSARIAL (mutation): drop the `vw - floors >= MIN_TILE_BAND` test from
    /// `close_until_fits` and every narrow case below goes red.
    #[test]
    fn opening_a_drawer_closes_others_only_when_the_band_would_collapse() {
        assert_eq!(
            opens(1280.0, DrawerKind::Chat, true, true, false),
            DrawersToClose::default(),
            "the 1280 all-three case must stay allowed"
        );
        assert_eq!(
            opens(1024.0, DrawerKind::Chat, true, true, false),
            DrawersToClose {
                diagnostics: true,
                ..Default::default()
            }
        );
        assert_eq!(
            opens(900.0, DrawerKind::Diagnostics, false, false, true),
            DrawersToClose {
                chat: true,
                ..Default::default()
            }
        );
        assert_eq!(
            opens(900.0, DrawerKind::Diagnostics, true, false, false),
            DrawersToClose {
                peer_list: true,
                ..Default::default()
            }
        );
        let at_768 = opens(768.0, DrawerKind::PeerList, false, true, true);
        assert!(!at_768.peer_list, "the opener must win");
        assert!(at_768.diagnostics && at_768.chat);
    }

    /// Sweeps 568..1600 and every set reachable through both entry points.
    ///
    /// ADVERSARIAL (mutation): dropping the gate in `close_until_fits` fails
    /// this; dropping `max_total_reserve`'s band term does NOT.
    #[test]
    fn no_reachable_drawer_set_can_collapse_the_tile_band() {
        let flags = |i: usize| (i & 1 != 0, i & 2 != 0, i & 4 != 0);
        let kinds = [
            DrawerKind::PeerList,
            DrawerKind::Diagnostics,
            DrawerKind::Chat,
        ];
        for chat_w in [DRAWER_MIN_WIDTH, CHAT_DRAWER_WIDTH, DRAWER_MAX_ABS] {
            let mut vw = DRAWER_REFLOW_MIN_VW;
            while vw <= 1600.0 {
                for i in 0..8usize {
                    let (p0, d0, c0) = flags(i);
                    let mut cases = vec![(drawers_to_close_on_resize(vw, p0, d0, c0), p0, d0, c0)];
                    for kind in kinds {
                        let (p, d, c) = (
                            p0 || kind == DrawerKind::PeerList,
                            d0 || kind == DrawerKind::Diagnostics,
                            c0 || kind == DrawerKind::Chat,
                        );
                        cases.push((drawers_to_close_on_open(vw, kind, p0, d0, c0), p, d, c));
                    }
                    for (close, p, d, c) in cases {
                        let state = DrawerState {
                            vw,
                            peer_list_open: p && !close.peer_list,
                            left_w: DRAWER_MAX_ABS,
                            diagnostics_open: d && !close.diagnostics,
                            right_w: DRAWER_MAX_ABS,
                            chat_open: c && !close.chat,
                            chat_w,
                        };
                        let r = drawer_reserves(state);
                        // Tile AREA: a 40px band is all padding and holds no tile.
                        let area = vw - r.left_reserve - r.right_reserve - GRID_SIDE_PADDING;
                        let reserving = usize::from(state.peer_list_open)
                            + usize::from(state.diagnostics_open)
                            + usize::from(state.chat_open && !chat_is_sheet(vw));
                        let floor = if reserving >= 2 { MIN_TILE_AREA } else { 1.0 };
                        assert!(
                            area >= floor,
                            "vw {vw}: {state:?} left {area}px of tile area against a \
                         {floor}px floor (reserves {} + {})",
                            r.left_reserve,
                            r.right_reserve
                        );

                        let rects = open_drawer_rects(state, r);
                        for &(name, l, right) in &rects {
                            assert!(
                                l >= 0.0 && right <= vw,
                                "vw {vw}: {name} paints at [{l}, {right}], outside the \
                             viewport ({state:?})"
                            );
                        }
                        for (a, b) in rects
                            .iter()
                            .enumerate()
                            .flat_map(|(i, a)| rects[i + 1..].iter().map(move |b| (a, b)))
                        {
                            assert!(
                                a.2 <= b.1 || b.2 <= a.1,
                                "vw {vw}: {} [{}, {}] overlaps {} [{}, {}] ({state:?})",
                                a.0,
                                a.1,
                                a.2,
                                b.0,
                                b.1,
                                b.2
                            );
                        }
                    }
                }
                vw += 1.0;
            }
        }
    }

    /// ADVERSARIAL (mutation): drop the `chat_is_sheet` gate in
    /// `drawer_reserves` and the 600px case reserves 360 instead of 0.
    #[test]
    fn chat_overlays_instead_of_reserving_below_its_breakpoint() {
        let chat_at = |vw: f64| {
            drawer_reserves(DrawerState {
                vw,
                peer_list_open: false,
                left_w: 320.0,
                diagnostics_open: false,
                right_w: 560.0,
                chat_open: true,
                chat_w: CHAT_DRAWER_WIDTH,
            })
        };
        close_to(chat_at(600.0).right_reserve, 0.0, "600 chat overlays");
        close_to(
            chat_at(679.0).right_reserve,
            0.0,
            "just under the breakpoint",
        );
        close_to(chat_at(680.0).right_reserve, 360.0, "680 chat reserves");
        close_to(chat_at(1280.0).right_reserve, 360.0, "1280 unchanged");
        close_to(chat_at(600.0).chat_render_w, 360.0, "600 render width");

        // The exclusivity rule makes this pair unreachable, so nothing else
        // guards the offset, and ungated it pushed the sheet off the LEFT edge.
        //
        // ADVERSARIAL (mutation): drop the gate and this reads 300, not 0.
        let sheet_over_diagnostics = DrawerState {
            vw: 600.0,
            peer_list_open: false,
            left_w: 320.0,
            diagnostics_open: true,
            right_w: 560.0,
            chat_open: true,
            chat_w: CHAT_DRAWER_WIDTH,
        };
        let r = drawer_reserves(sheet_over_diagnostics);
        assert!(
            r.diag_render_w > 0.0,
            "premise: diagnostics does reserve here"
        );
        close_to(r.chat_right_offset, 0.0, "600 sheet offset");
    }

    /// A sheet spans the viewport, so chat is exclusive with EVERY drawer:
    /// the peer list reflows at 568..680 and would paint over it at z 9300.
    ///
    /// ADVERSARIAL (mutation): drop `out.peer_list` and the sweep fails.
    #[test]
    fn a_chat_sheet_is_exclusive_with_every_other_drawer() {
        for vw in [375.0, 568.0, 600.0, 679.0] {
            assert_eq!(
                opens(vw, DrawerKind::Chat, true, true, false),
                DrawersToClose {
                    peer_list: true,
                    diagnostics: true,
                    ..Default::default()
                },
                "opening the sheet at {vw} must clear both"
            );
            for kind in [DrawerKind::PeerList, DrawerKind::Diagnostics] {
                assert_eq!(
                    opens(vw, kind, false, false, true),
                    DrawersToClose {
                        chat: true,
                        ..Default::default()
                    },
                    "opening {kind:?} at {vw} must dismiss the sheet"
                );
            }
            assert_eq!(
                drawers_to_close_on_resize(vw, true, true, true),
                DrawersToClose {
                    peer_list: true,
                    diagnostics: true,
                    ..Default::default()
                },
                "resizing into {vw} must leave only the sheet"
            );
        }
        assert_eq!(
            opens(680.0, DrawerKind::Chat, false, false, false),
            DrawersToClose::default()
        );
    }

    /// ADVERSARIAL (mutation): widen `handle_is_inert` to `<` → red.
    #[test]
    fn a_handle_is_inert_exactly_when_its_cap_is_at_the_floor() {
        assert!(handle_is_inert(DRAWER_MIN_WIDTH));
        assert!(handle_is_inert(DRAWER_MIN_WIDTH - 1.0));
        assert!(!handle_is_inert(DRAWER_MIN_WIDTH + 1.0));
        assert!(handle_is_inert(drawer_max_for_side(
            desktop_state(true, true, true),
            DrawerSide::Right
        )));
        assert!(!handle_is_inert(drawer_max_for_side(
            desktop_state(false, true, false),
            DrawerSide::Right
        )));
    }

    #[test]
    fn overflow_budget_is_the_band_less_the_edge_gutters() {
        let reserves = drawer_reserves(DrawerState {
            vw: 1280.0,
            peer_list_open: false,
            left_w: 320.0,
            diagnostics_open: true,
            right_w: 560.0,
            chat_open: true,
            chat_w: CHAT_DRAWER_WIDTH,
        });
        let band = action_bar_band_width(1280.0, reserves);
        close_to(band, 512.0, "band");
        close_to(
            overflow_budget_width(false, band, 720.0),
            472.0,
            "bottom dock budget",
        );
        close_to(
            overflow_budget_width(true, band, 720.0),
            648.0,
            "a vertical dock budgets against vh above the meeting footer, untouched by the band",
        );
    }

    #[test]
    fn grid_padding_reserves_the_meeting_footer_under_every_dock() {
        let cases = [
            (
                DockPosition::Bottom,
                1280.0,
                (16.0, 20.0, 20.0, 152.0, 20.0),
            ),
            (DockPosition::Bottom, 568.0, (16.0, 20.0, 20.0, 152.0, 20.0)),
            (DockPosition::Bottom, 567.0, (8.0, 8.0, 8.0, 112.0, 8.0)),
            (DockPosition::Left, 1280.0, (16.0, 20.0, 20.0, 52.0, 120.0)),
            (DockPosition::Left, 375.0, (8.0, 8.0, 8.0, 40.0, 80.0)),
            (DockPosition::Right, 1280.0, (16.0, 20.0, 120.0, 52.0, 20.0)),
            (DockPosition::Right, 375.0, (8.0, 8.0, 80.0, 40.0, 8.0)),
        ];
        for (dock, vw, want) in cases {
            assert_eq!(grid_padding(dock, vw), want, "{dock:?} at {vw}px");
        }
    }

    #[test]
    fn screen_share_padding_reserves_the_meeting_footer() {
        assert_eq!(
            screen_share_padding(0.0, false, 1280.0),
            (16.0, 16.0, 112.0, 16.0)
        );
        assert_eq!(
            screen_share_padding(36.0, false, 1280.0),
            (52.0, 16.0, 112.0, 16.0),
            "the recording bar adds to the top only"
        );
    }

    #[test]
    fn row_one_starts_below_the_meeting_timer_chip_only_while_it_is_up() {
        for (dock, vw, base, band_top) in [
            (DockPosition::Bottom, 1280.0, 20.0, 60.0),
            (DockPosition::Left, 1280.0, 20.0, 60.0),
            (DockPosition::Right, 568.0, 20.0, 60.0),
            (DockPosition::Bottom, 567.0, 8.0, 56.0),
            (DockPosition::Left, 375.0, 8.0, 56.0),
        ] {
            let (_, top, ..) = grid_padding(dock, vw);
            assert_eq!(top, base, "premise: {dock:?} at {vw}px");
            assert_eq!(grid_pad_top(top, vw, 0.0, false), base, "{dock:?} idle");
            assert_eq!(grid_pad_top(top, vw, 0.0, true), band_top, "{dock:?} timer");
            assert_eq!(
                grid_pad_top(top, vw, 40.0, true),
                band_top + 40.0,
                "the band stacks under the recording bar"
            );
            assert_eq!(screen_share_padding(0.0, false, vw).0, 16.0);
            assert_eq!(screen_share_padding(0.0, true, vw).0, band_top);
            assert_eq!(screen_share_padding(32.0, true, vw).0, band_top + 32.0);
        }
    }

    #[test]
    fn unbanded_tile_names_ellipsize_before_the_meeting_timer_chip() {
        let flat = include_str!("../../static/style.css")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let rule = "#grid-container.participants-1:has(> .meeting-timer-chip) \
                    .grid-item.full-bleed .floating-name \
                    { max-width: calc(50% - 100px); }";
        assert!(
            flat.contains(rule),
            "a full-bleed tile has no band, so this cap is all that keeps a long name off \
             the chip"
        );
    }

    #[test]
    fn the_meeting_timer_band_clears_the_expired_chip_at_both_widths() {
        let style = include_str!("../../static/style.css");
        let global = include_str!("../../static/global.css");
        let px = |css: &str, name: &str| -> f64 {
            let at = css
                .find(&format!("{name}:"))
                .unwrap_or_else(|| panic!("no `{name}`"))
                + name.len()
                + 1;
            let v = css[at..at + css[at..].find(';').unwrap()].trim();
            match v.strip_suffix("rem") {
                Some(rem) => rem.parse::<f64>().unwrap() * 16.0,
                None => v.trim_end_matches("px").parse().unwrap(),
            }
        };
        let flat = style.split_whitespace().collect::<Vec<_>>().join(" ");
        for formula in [
            "--meeting-timer-chip-h: calc( var(--space-2) * 2 + 4px + max(18px, calc(var(--fs-6) * 1.3)) );",
            "@media (max-width: 567px) { #grid-container { --meeting-timer-chip-h: \
             calc(6px * 2 + 4px + max(18px, calc(var(--fs-5) * 1.3)));",
        ] {
            assert!(flat.contains(formula), "re-derive the band: `{formula}` changed");
        }

        let top = px(style, "--top-stack-top");
        let desktop = px(global, "--space-2") * 2.0 + 4.0 + (px(global, "--fs-6") * 1.3).max(18.0);
        let mobile = 6.0 * 2.0 + 4.0 + (px(global, "--fs-5") * 1.3).max(18.0);
        for (clearance, chip) in [
            (MEETING_TIMER_BAND_CLEARANCE, desktop),
            (MEETING_TIMER_BAND_CLEARANCE_MOBILE, mobile),
        ] {
            let gap = clearance - (top + chip);
            assert!(
                (8.0..=12.0).contains(&gap),
                "row 1 must start 8-12px below the expired chip, not {gap}px"
            );
        }
    }

    #[test]
    fn meeting_footer_reserve_matches_the_css_token() {
        let css = include_str!("../../static/global.css");
        let token = format!("--meeting-footer-h: {}px;", MEETING_FOOTER_RESERVE as u32);
        assert!(
            css.contains(&token),
            "global.css must declare `{token}` to match MEETING_FOOTER_RESERVE"
        );
    }

    #[test]
    fn a_lone_drawer_is_never_closed_even_on_a_viewport_too_narrow_for_it() {
        assert_eq!(
            opens(600.0, DrawerKind::Chat, false, false, false),
            DrawersToClose::default()
        );
        assert_eq!(
            drawers_to_close_on_resize(600.0, false, false, true),
            DrawersToClose::default()
        );
    }

    #[test]
    fn below_the_reflow_breakpoint_peer_list_and_diagnostics_stay_independent() {
        assert_eq!(
            opens(375.0, DrawerKind::PeerList, false, true, false),
            DrawersToClose::default()
        );
        assert_eq!(
            opens(375.0, DrawerKind::Diagnostics, true, false, false),
            DrawersToClose::default()
        );
        assert_eq!(
            drawers_to_close_on_resize(375.0, true, true, false),
            DrawersToClose::default()
        );
    }

    #[test]
    fn a_resize_that_sheds_a_drawer_says_so_once() {
        let shed = drawers_to_close_on_resize(900.0, true, true, false);
        assert!(shed.any(), "premise: 900 with two drawers does shed one");
        assert_eq!(
            resize_notice(shed, false),
            Some("Diagnostics closed to fit the window")
        );
        assert_eq!(
            resize_notice(
                DrawersToClose {
                    peer_list: true,
                    ..Default::default()
                },
                false
            ),
            Some("Participants closed to fit the window")
        );
        assert_eq!(
            resize_notice(
                DrawersToClose {
                    chat: true,
                    ..Default::default()
                },
                false
            ),
            Some("Chat closed to fit the window")
        );
        assert_eq!(
            resize_notice(
                DrawersToClose {
                    peer_list: true,
                    diagnostics: true,
                    ..Default::default()
                },
                false
            ),
            Some("Panels closed to fit the window")
        );
        assert_eq!(
            resize_notice(DrawersToClose::default(), true),
            Some("Button editing ended to fit the window; layout saved")
        );
        assert_eq!(resize_notice(DrawersToClose::default(), false), None);
        assert_eq!(
            resize_notice(drawers_to_close_on_resize(1280.0, true, true, true), false),
            None,
            "1280 sheds nothing, so a resize there is silent"
        );
    }

    #[test]
    fn a_resize_closes_drawers_with_nothing_protected() {
        assert_eq!(
            drawers_to_close_on_resize(1280.0, true, true, true),
            DrawersToClose::default(),
            "1280 all three still fits"
        );
        assert_eq!(
            drawers_to_close_on_resize(900.0, true, true, true),
            DrawersToClose {
                peer_list: true,
                diagnostics: true,
                ..Default::default()
            }
        );
        let after = drawers_to_close_on_resize(700.0, true, true, true);
        let kept = if after.peer_list { 0.0 } else { 300.0 }
            + if after.diagnostics { 0.0 } else { 300.0 }
            + if after.chat { 0.0 } else { 360.0 };
        assert!(
            700.0 - kept >= MIN_TILE_BAND,
            "700px left a {}px band",
            700.0 - kept
        );
    }

    /// ADVERSARIAL (mutation): delete the `dragging` branch and 336 becomes 333.
    #[test]
    fn reserves_quantise_only_while_dragging() {
        close_to(quantise_reserve(333.0, true), 336.0, "mid-drag rounds up");
        close_to(quantise_reserve(333.0, false), 333.0, "at rest is exact");
        close_to(quantise_reserve(320.0, true), 320.0, "320 is on the step");
        close_to(quantise_reserve(360.0, true), 360.0, "360 is on the step");
    }

    #[test]
    fn split_panel_camera_tiles_are_lidded_to_their_painted_size() {
        use crate::components::decode_budget::screen_decode_sets;
        use videocall_client::{size_cap_layer, PrefMediaKind, TileHint};
        let split = SplitPanel {
            content_w: 1248.0,
            ratio: 0.667,
            height: 560.0,
            tiles: 2,
        };
        let plan = plan_tiles(
            TilePlanInput {
                avail_h: 560.0,
                ..input(9)
            },
            0,
        );
        let lid = |dpr| decoded_tile_device_px_h(true, split, plan, 9, 560.0, dpr);
        assert_eq!((lid(1.0), lid(2.0)), (122, 244), "a ~183x122 thumbnail");
        assert_eq!(
            decoded_tile_device_px_h(false, split, plan, 9, 560.0, 1.0),
            176,
            "the grid ignores the panel"
        );
        let (_, hinted) = screen_decode_sets(&[7, 8].into_iter().collect(), None, &[]);
        let hints = hinted.tile_hints(&[7, 8], lid(2.0), &HashSet::new(), 0);
        assert_eq!(
            hints[&7],
            TileHint::Capped { device_px_h: 244 },
            "the sharer too"
        );
        let layer = |h| size_cap_layer(h, 2, 3, PrefMediaKind::Video);
        assert_eq!((layer(lid(1.0)), layer(lid(2.0))), (0, 1));
        assert_eq!(
            size_cap_layer(244, 2, 3, PrefMediaKind::Screen),
            2,
            "never lids a screen"
        );
    }

    #[test]
    fn the_panel_lid_allows_for_a_scrollbar_only_when_its_tiles_overflow() {
        use crate::components::share_view::panel_share_tiles;
        let pads = screen_share_padding(0.0, false, 1180.0);
        let lid = |cameras, shares| {
            split_peer_tile_height(split_panel(1180.0, 700.0, pads, 0.667, cameras, shares))
        };
        let stage_only = panel_share_tiles((false, false), &[]);
        close_to(
            lid(8, stage_only),
            110.9,
            "8 cameras fit two columns: no scrollbar",
        );
        close_to(
            lid(8, panel_share_tiles((false, true), &[])),
            228.4,
            "the own share's tile overflows the panel",
        );
        close_to(
            lid(8, panel_share_tiles((false, false), &[false, true])),
            228.4,
            "so does a displaced share's tile",
        );
        assert_eq!(panel_share_tiles((true, true), &[true, false, true]), 4);
        close_to(
            lid(10, stage_only),
            228.4,
            "10 overflow once the padding is subtracted",
        );
        close_to(lid(24, stage_only), 228.4, "24 overflow");
    }
}

#[cfg(test)]
mod grid_capacity_tests {
    use super::*;
    use crate::components::density::DENSITY_MODES;
    use crate::components::self_view::self_tile_counts;
    use crate::context::SelfViewPlacement;

    const GAP: f64 = 8.0;
    const DESKTOP: (f64, f64) = (1920.0, 1000.0);
    const LAPTOP: (f64, f64) = (1366.0, 640.0);
    const FITS_30_CELLS: (f64, f64) = (800.0, 450.0);
    const FITS_31_CELLS: (f64, f64) = (3960.0, 100.0);
    const PHONE: (f64, f64) = (375.0, 600.0);
    const ONE_CELL: (f64, f64) = (100.0, 100.0);
    const VIEWPORTS: [(f64, f64); 6] = [
        DESKTOP,
        LAPTOP,
        FITS_30_CELLS,
        FITS_31_CELLS,
        PHONE,
        ONE_CELL,
    ];

    #[derive(Clone, Copy, Debug)]
    struct Call {
        real: usize,
        mocks: usize,
        self_in_grid: bool,
        share_cells: usize,
        viewport: (f64, f64),
    }

    impl Call {
        fn real_only(real: usize, self_in_grid: bool, viewport: (f64, f64)) -> Self {
            Self {
                real,
                mocks: 0,
                self_in_grid,
                share_cells: 0,
                viewport,
            }
        }

        fn total_tiles(self) -> usize {
            self.real.min(CANVAS_LIMIT) + self.mocks
        }

        fn plan_in(self, mode: DensityMode) -> GridCapacity {
            let (w, h) = self.viewport;
            plan_grid_capacity(
                self.real + self.mocks,
                self.total_tiles(),
                &self_tile_counts(
                    self.real.min(CANVAS_LIMIT),
                    self.mocks,
                    SelfViewPlacement::Grid,
                    self.self_in_grid,
                    false,
                    self.share_cells,
                ),
                w,
                h,
                GAP,
                mode.min_tile_width(w),
            )
        }

        fn plan(self) -> GridCapacity {
            self.plan_in(DensityMode::Maximum)
        }
    }

    fn fits(cells: usize, (w, h): (f64, f64)) -> bool {
        compute_layout(cells, w, h, GAP).2 >= DensityMode::Maximum.min_tile_width(w)
    }

    fn capacity(layout_limit: usize, displayed: usize, overflow: usize) -> GridCapacity {
        GridCapacity {
            layout_limit,
            displayed,
            overflow,
        }
    }

    #[test]
    fn past_the_cap_thirty_tiles_show_beside_the_badge() {
        for real in [31, 40, 100] {
            for self_in_grid in [true, false] {
                for viewport in [DESKTOP, LAPTOP] {
                    assert_eq!(
                        Call::real_only(real, self_in_grid, viewport).plan(),
                        capacity(31, 30, real - 30),
                        "real={real} self_in_grid={self_in_grid} viewport={viewport:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_share_cell_keeps_thirty_tiles_beside_the_badge() {
        let call = Call {
            share_cells: 1,
            ..Call::real_only(40, true, DESKTOP)
        };
        assert_eq!(call.plan(), capacity(31, 30, 10));
    }

    #[test]
    fn the_badge_takes_a_tiles_cell_when_no_extra_cell_fits() {
        assert!(fits(30, FITS_30_CELLS) && !fits(31, FITS_30_CELLS));
        assert!(fits(31, FITS_31_CELLS) && !fits(32, FITS_31_CELLS));
        for real in [31, 40, 100] {
            let expected = capacity(30, 29, real - 29);
            assert_eq!(Call::real_only(real, false, FITS_30_CELLS).plan(), expected);
            assert_eq!(Call::real_only(real, true, FITS_31_CELLS).plan(), expected);
        }
    }

    #[test]
    fn a_small_viewport_counts_the_whole_roster_in_the_badge() {
        assert!(fits(24, PHONE) && !fits(25, PHONE));
        for real in [30, 31, 40, 100] {
            assert_eq!(
                Call::real_only(real, true, PHONE).plan(),
                capacity(23, 22, real - 22)
            );
        }
    }

    #[test]
    fn at_the_cap_with_room_every_peer_has_a_tile_and_no_badge_shows() {
        for self_in_grid in [true, false] {
            assert_eq!(
                Call::real_only(30, self_in_grid, DESKTOP).plan(),
                capacity(30, 30, 0)
            );
        }
    }

    #[test]
    fn at_or_below_the_cap_the_badge_shows_only_when_the_tiles_do_not_fit() {
        assert_eq!(
            Call::real_only(20, true, LAPTOP).plan_in(DensityMode::Dense),
            capacity(14, 13, 7)
        );
        assert_eq!(
            Call::real_only(30, true, DESKTOP).plan_in(DensityMode::Auto),
            capacity(29, 28, 2)
        );
        assert_eq!(
            Call::real_only(30, false, DESKTOP).plan_in(DensityMode::Auto),
            capacity(30, 30, 0)
        );
    }

    #[test]
    fn a_viewport_that_fits_one_cell_still_shows_one_tile() {
        assert!(!fits(2, ONE_CELL));
        for self_in_grid in [true, false] {
            assert_eq!(
                Call::real_only(3, self_in_grid, ONE_CELL).plan(),
                capacity(1, 1, 2)
            );
        }
    }

    #[test]
    fn displayed_and_overflow_sum_to_the_roster() {
        for real in (0..=35).chain([40, 100]) {
            for mocks in [0, 3] {
                for self_in_grid in [true, false] {
                    for share_cells in [0, 1] {
                        for viewport in VIEWPORTS {
                            for mode in DENSITY_MODES {
                                let call = Call {
                                    real,
                                    mocks,
                                    self_in_grid,
                                    share_cells,
                                    viewport,
                                };
                                let plan = call.plan_in(mode);
                                let roster_total = real + mocks;
                                let context = format!("{call:?} {mode:?} {plan:?}");
                                assert_eq!(
                                    plan.displayed + plan.overflow,
                                    roster_total,
                                    "{context}"
                                );
                                assert!(plan.displayed <= call.total_tiles(), "{context}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_render_body_takes_its_counts_from_the_plan() {
        let flat: String = include_str!("attendants.rs").split_whitespace().collect();
        for wired in [
            "lettotal_tiles=capped_real+mock_count;",
            "letroster_total=num_display_peers+mock_count;",
            "plan_grid_capacity(roster_total,total_tiles,&self_counts,avail_w,avail_h,gap,\
             effective_mode.min_tile_width(vw),)",
            "letdecoded_limit=grid_capacity.layout_limit.min(budget_cap.max(MIN_CAP));",
            "letdisplayed_tile_count=grid_capacity.displayed;",
            "letoverflow_count=grid_capacity.overflow;",
            "displayed:displayed_tile_count,",
            "lettile_count=grid_capacity.tile_count(&self_counts);",
        ] {
            assert_eq!(flat.matches(wired).count(), 1, "{wired}");
        }
    }

    #[test]
    fn the_tile_count_is_the_tiles_the_badge_the_self_and_the_share_cells() {
        let corner = self_tile_counts(3, 0, SelfViewPlacement::Corner, true, false, 0);
        let grid_and_shares = self_tile_counts(30, 0, SelfViewPlacement::Grid, true, false, 2);
        assert_eq!(capacity(3, 3, 0).tile_count(&corner), 3);
        assert_eq!(capacity(31, 30, 10).tile_count(&corner), 31);
        assert_eq!(capacity(31, 30, 10).tile_count(&grid_and_shares), 34);
    }

    fn sessions(count: usize) -> Vec<String> {
        (1..=count).map(|id| id.to_string()).collect()
    }

    fn camera_window(pins: &[String], tiles: &[String], capacity: GridCapacity) -> CameraWindow {
        plan_camera_window(
            CameraWindowInput {
                pins,
                camera_on: tiles.to_vec(),
                camera_off: Vec::new(),
                displayed: capacity.displayed,
                budget: capacity.layout_limit,
                device_ceiling: None,
                requested: &HashSet::new(),
            },
            &SpeakerInputs {
                speech_map: &HashMap::new(),
                join_map: &HashMap::new(),
                now_ms: 0.0,
                active_ms: 0.0,
            },
        )
    }

    /// The render's path from the roster to the grid plan, with the last capped
    /// session enlarged.
    fn enlarged_grid(
        roster_total: usize,
        placement: SelfViewPlacement,
        (w, h): (f64, f64),
        gap: f64,
    ) -> (GridCapacity, usize, TilePlan, EnlargedTiles) {
        let total_tiles = roster_total.min(CANVAS_LIMIT);
        let counts = self_tile_counts(total_tiles, 0, placement, true, false, 0);
        let capacity = plan_grid_capacity(
            roster_total,
            total_tiles,
            &counts,
            w,
            h,
            gap,
            DensityMode::Maximum.min_tile_width(w),
        );
        let tiles = sessions(total_tiles);
        let enlarged = tiles[total_tiles - 1].clone();
        let lead = [enlarged.clone()];
        let plain = camera_window(&[], &tiles, capacity);
        let window = camera_window(&lead, &tiles, capacity);
        assert_eq!(window.on_cells, plain.on_cells, "the window keeps its size");
        assert_eq!(window.decoded, plain.decoded);
        assert_eq!(
            window.tiles[0], enlarged,
            "the enlarged tile is in the window"
        );
        let unified: Vec<(String, TileRenderMode)> = window
            .tiles
            .iter()
            .take(window.on_cells)
            .map(|id| (id.clone(), TileRenderMode::Decoded))
            .collect();
        let tile_count = capacity.tile_count(&counts);
        let (plan, enlarged_tiles) = plan_tile_grid(
            TilePlanInput {
                tile_count,
                split_layout: false,
                full_bleed: false,
                avail_w: w,
                avail_h: h,
                gap,
                vw: w,
            },
            &unified,
            &[(enlarged, 0)],
            false,
        );
        (capacity, tile_count, plan, enlarged_tiles)
    }

    #[test]
    fn a_past_cap_roster_keeps_its_window_and_badge_beside_an_enlarged_tile() {
        use SelfViewPlacement::{Corner, Grid};
        for (placement, viewport, expected, cells, layout) in [
            (Corner, DESKTOP, capacity(31, 30, 10), 31, (7, 5, 267.0, 1)),
            (Grid, DESKTOP, capacity(31, 30, 10), 32, (7, 5, 267.0, 1)),
            (Corner, LAPTOP, capacity(31, 30, 10), 31, (7, 5, 182.0, 1)),
            (Grid, LAPTOP, capacity(31, 30, 10), 32, (7, 5, 182.0, 1)),
            (
                Corner,
                FITS_30_CELLS,
                capacity(30, 29, 11),
                30,
                (6, 5, 125.0, 0),
            ),
            (
                Grid,
                FITS_30_CELLS,
                capacity(29, 28, 12),
                30,
                (6, 5, 125.0, 0),
            ),
        ] {
            let context = format!("{placement:?} {viewport:?}");
            let (planned, tile_count, plan, enlarged) = enlarged_grid(40, placement, viewport, GAP);
            assert_eq!(planned, expected, "{context}");
            assert_eq!(tile_count, cells, "{context}");
            assert_eq!(
                (plan.cols, plan.rows, plan.tile_w, plan.enlarged),
                layout,
                "{context}"
            );
            assert_eq!(enlarged.rank.len(), plan.enlarged, "{context}");
            let (w, h) = viewport;
            let rows = plan.rows as f64;
            assert!(
                rows * plan.tile_h() + (rows - 1.0) * GAP <= h,
                "{context}: too tall"
            );
            if plan.enlarged > 0 {
                assert!(2.0 * plan.tile_w + GAP <= w, "{context}: too wide");
                assert_eq!(
                    plan.rows,
                    enlarged_rows(tile_count, 1, plan.cols),
                    "{context}: the badge has its cell"
                );
            }
        }
    }

    #[test]
    fn the_past_cap_badge_cell_can_push_an_enlarged_plan_under_the_floor() {
        const DESKTOP_GAP: f64 = 16.0;
        const WIDE_AND_SHORT: (f64, f64) = (1600.0, 320.0);

        let (at_cap, cells, plan, enlarged) =
            enlarged_grid(30, SelfViewPlacement::Corner, WIDE_AND_SHORT, DESKTOP_GAP);
        assert_eq!(at_cap, capacity(30, 30, 0), "premise: no badge");
        assert_eq!(cells, 30);
        assert_eq!((plan.cols, plan.tile_w, plan.enlarged), (11, 130.0, 1));
        assert_eq!(enlarged.rank.len(), 1);

        let (past_cap, cells, plan, enlarged) =
            enlarged_grid(40, SelfViewPlacement::Corner, WIDE_AND_SHORT, DESKTOP_GAP);
        assert_eq!(past_cap, capacity(31, 30, 10), "premise: the badge's cell");
        assert_eq!(cells, 31);
        assert_eq!(
            (plan.cols, plan.tile_w, plan.enlarged),
            (11, 130.0, 0),
            "one more cell makes the enlarged plan 118 px wide, under the 120 px floor"
        );
        assert_eq!(enlarged, EnlargedTiles::default());
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod split_panel_dom_tests {
    use super::*;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::wasm_bindgen_test;

    fn painted_tile_h(width: f64, ratio: f64, tiles: usize) -> f64 {
        let doc = gloo_utils::document();
        let frame = doc.create_element("iframe").unwrap();
        let size = format!("width: {width}px; height: 600px; border: 0;");
        frame.set_attribute("style", &size).unwrap();
        doc.body().unwrap().append_child(&frame).unwrap();
        let page: web_sys::Document = js_sys::Reflect::get(&frame, &"contentDocument".into())
            .unwrap()
            .unchecked_into();
        let style = page.create_element("style").unwrap();
        style.set_text_content(Some(&format!(
            "*, ::before, ::after {{ box-sizing: border-box; }}{}",
            include_str!("../../static/style.css")
        )));
        page.head().unwrap().append_child(&style).unwrap();
        let pct = ratio * 100.0;
        page.body().unwrap().set_inner_html(&format!(
            "<div id='grid-container' class='has-screen-share' style='position: absolute; \
             inset: 0; gap: {SPLIT_GAP}px; padding: 16px; box-sizing: border-box; {}'>\
             <div class='ss-left-pane' style='--ss-ratio: {pct:.2}%;'></div>\
             <div class='screen-share-resize-handle'></div>\
             <div class='ss-peer-panel' style='--ss-ratio: {pct:.2}%;'>{}</div></div>",
            screen_share_flow_style(),
            "<div class='tile-slot'><div class='split-peer-tile'></div></div>".repeat(tiles),
        ));
        let tile = page.query_selector(".split-peer-tile").unwrap().unwrap();
        let h = tile.get_bounding_client_rect().height();
        frame.remove();
        h
    }

    #[wasm_bindgen_test]
    fn the_split_lid_covers_the_painted_panel_tile() {
        for width in [800.0, 1024.0, 1160.0, 1180.0, 1200.0, 1280.0, 1920.0] {
            for ratio in [0.3, 0.667, 0.85] {
                for tiles in [2, 24] {
                    let painted = painted_tile_h(width, ratio, tiles);
                    let lid = split_peer_tile_height(SplitPanel {
                        content_w: width - 32.0,
                        ratio,
                        height: 600.0 - 32.0,
                        tiles,
                    });
                    let at = format!("{width}px at {ratio} with {tiles} tiles");
                    assert!(painted > 0.0, "premise: {at}");
                    assert!(lid >= painted - 0.5, "{at}: lid {lid:.1} < {painted:.1}");
                    if tiles == 2 {
                        assert!(lid <= painted * 1.04, "{at}: lid {lid:.1} >> {painted:.1}");
                    }
                }
            }
        }
    }
}

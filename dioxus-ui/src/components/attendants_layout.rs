// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure layout helpers extracted from `attendants.rs`.
//!
//! These functions are algorithmically non-trivial but have zero WASM / DOM /
//! Dioxus dependencies, so they can be unit-tested under plain `cargo test`.

use super::density::{DensityMode, MOBILE_WIDTH_BREAKPOINT_PX};
use std::collections::HashMap;

/// Tile aspect ratio (width / height) — 3 : 2.
pub(crate) const TILE_AR: f64 = 3.0 / 2.0;

/// Google Meet–style layout: try every column count, compute the maximum
/// 3 : 2 tile size for each, and pick the variant with the largest tile area.
/// Returns `(cols, rows, tile_width)`.
pub(crate) fn compute_layout(n: usize, w: f64, h: f64, gap: f64) -> (usize, usize, f64) {
    if n == 0 {
        return (1, 1, w);
    }
    let mut best_cols = 1_usize;
    let mut best_rows = 1_usize;
    let mut best_area = 0.0_f64;
    let mut best_tw = 0.0_f64;
    let ar: f64 = TILE_AR;

    for cols in 1..=n {
        let rows = n.div_ceil(cols);

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

/// `#grid-container`'s flow longhands: a lone tile stretches across 1fr tracks,
/// 2+ tiles wrap and centre. Every arm emits the same set. (issue 2700)
pub(crate) fn tile_flow_style(tile_count: usize, cols: usize, rows: usize) -> String {
    if tile_count == 1 {
        format!(
            "display: grid; \
             grid-template-columns: repeat({cols}, 1fr); \
             grid-template-rows: repeat({rows}, 1fr); \
             justify-content: stretch; align-content: stretch; \
             flex-direction: unset; flex-wrap: unset; align-items: unset;"
        )
    } else {
        "display: flex; \
         grid-template-columns: none; grid-template-rows: none; \
         justify-content: center; align-content: flex-start; \
         flex-direction: row; flex-wrap: wrap; align-items: flex-start;"
            .to_string()
    }
}

/// [`tile_flow_style`]'s set valued for the screen-share split. (issue 2700)
pub(crate) fn screen_share_flow_style() -> &'static str {
    "display: flex; flex-direction: row; flex-wrap: nowrap; align-items: stretch; \
     justify-content: flex-start; align-content: stretch; \
     grid-template-columns: none; grid-template-rows: none;"
}

/// Nominal camera-tile geometry (`--tile-w`, `--tile-h`) for the screen-share
/// split layout's *maximized* (pinned) tile.
///
/// During screen share the participant panel renders small side-panel
/// thumbnails, but a PINNED side-panel tile is `position: fixed` with its
/// insets from the drawer reserves (style.css
/// `.split-peer-tile.grid-item-pinned`) — it maximizes over the shared screen,
/// exactly like `.grid-item-pinned` in the normal grid. That pinned tile's
/// chrome (name badge, top-icon cluster,
/// camera-off placeholder) is sized from the `--tile-w`/`--tile-h` custom
/// properties on `#grid-container`, so those vars must describe the MAXIMIZED
/// tile — NOT the compact side-panel thumbnail and NOT an N-tile grid cell
/// (whose height shrinks as the participant count grows).
///
/// Returns the largest 3:2 tile that fits the available meeting area
/// (`avail_w` × `avail_h`). This is intentionally the single-full-area tile —
/// numerically identical to `compute_layout(1, avail_w, avail_h, _)` — and is
/// a distinct, self-documenting function so the call site cannot be mistaken
/// for the participant-count-dependent grid packing math that this value must
/// NEVER reuse (PR #1946: reusing the grid cell size froze the pinned chrome
/// at a stale, count-dependent size). Depends only on the viewport-derived
/// available area, so it is deterministic across clients for a given viewport.
pub(crate) fn screen_share_pinned_tile_size(avail_w: f64, avail_h: f64) -> (f64, f64) {
    // Width of a 3:2 tile whose height fills `avail_h`, capped so it never
    // exceeds `avail_w` (mirrors the height-vs-width constraint in
    // `compute_layout`'s single-tile case for tall/narrow viewports).
    let tw = (avail_h * TILE_AR).min(avail_w).max(0.0);
    let th = tw / TILE_AR;
    (tw, th)
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
/// `partition_camera_tiles` (issue #2273). Returns the input order untouched
/// when `capped_real` covers the whole roster.
pub(crate) fn select_display_candidates(
    display_peers: &[String],
    capped_real: usize,
    camera_on: impl Fn(&str) -> bool,
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
                selection_tier(cam_on, speech.is_some()),
                speech.map_or(0.0, |ts| -ts), // negated: freshest speaker first
                join_map.get(peer).copied().unwrap_or(0.0),
            );
            (key, peer.clone(), cam_on)
        })
        .collect();

    ranked.sort_by(|a, b| {
        let ((ta, sa, ja), pa, _) = a;
        let ((tb, sb, jb), pb, _) = b;
        ta.cmp(tb)
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

const DRAWER_SHRINK_FLOORS: [f64; 2] = [DRAWER_MIN_WIDTH, DRAWER_MIN_WIDTH];

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
    (if is_vertical { vh } else { band_w }) - ACTION_BAR_EDGE_MARGIN
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
        return Some("Customize ended to fit the window; layout saved");
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
/// shrink diagnostics then the peer list, and the floors win over the cap.
pub(crate) fn drawer_reserves(state: DrawerState) -> DrawerReserves {
    // Negated, not `vw < MIN`: a NaN viewport must not reach the grid branch.
    let reflows = state.vw >= DRAWER_REFLOW_MIN_VW;
    if !reflows {
        return DrawerReserves {
            left_render_w: state.left_w,
            chat_render_w: CHAT_DRAWER_WIDTH,
            diag_render_w: state.right_w,
            chat_right_offset: 0.0,
            left_reserve: 0.0,
            right_reserve: 0.0,
        };
    }

    let open = [state.diagnostics_open, state.peer_list_open];
    let mut w = [state.right_w, state.left_w];
    let chat_render_w = CHAT_DRAWER_WIDTH;
    // Below its breakpoint chat paints 360 wide but reserves nothing.
    let chat_occupies = if state.chat_open && !chat_is_sheet(state.vw) {
        chat_render_w
    } else {
        0.0
    };
    let budget = max_total_reserve(state.vw);
    let occupied = |w: &[f64; 2]| -> f64 {
        chat_occupies
            + w.iter()
                .zip(open.iter())
                .filter(|(_, is_open)| **is_open)
                .map(|(width, _)| *width)
                .sum::<f64>()
    };

    for (i, &floor) in DRAWER_SHRINK_FLOORS.iter().enumerate() {
        let over = occupied(&w) - budget;
        if over <= 0.0 {
            break;
        }
        if open[i] {
            w[i] = (w[i] - over).max(floor);
        }
    }

    let [diag_render_w, left_render_w] = w;
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
            CHAT_DRAWER_WIDTH
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

    #[test]
    fn tile_flow_wraps_and_centres_for_two_or_more_tiles() {
        let s = tile_flow_style(4, 2, 2);
        assert!(s.contains("display: flex;"), "{s}");
        assert!(s.contains("flex-wrap: wrap;"), "{s}");
        assert!(s.contains("justify-content: center;"), "{s}");
        // Horizontal only: the tracks this replaces packed top (HCL #6).
        assert!(s.contains("align-content: flex-start;"), "{s}");
        assert!(s.contains("grid-template-columns: none;"), "{s}");
    }

    #[test]
    fn tile_flow_stretches_a_lone_tile_across_one_fr_tracks() {
        let s = tile_flow_style(1, 1, 1);
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
        let lone = tile_flow_style(1, 2, 2);
        let wrap = tile_flow_style(4, 2, 2);
        assert_eq!(names(&lone).len(), 8, "{lone}");
        assert_eq!(names(&lone), names(&wrap));
        assert_eq!(names(&lone), names(screen_share_flow_style()));
    }

    // -- screen_share_pinned_tile_size --------------------------------

    #[test]
    fn ss_pinned_tile_matches_single_maximized_tile() {
        // Landscape meeting area (1280x720 viewport minus grid padding:
        // avail_w = 1280-40 = 1240, avail_h = 720-140 = 580 — the exact
        // dimensions the screen-share E2E harness runs at).
        let (tw, th) = screen_share_pinned_tile_size(1240.0, 580.0);
        // A 3:2 tile filling the 580px height is 870px wide, which fits in
        // 1240px, so height is the binding constraint.
        assert!((th - 580.0).abs() < 0.5, "th was {th}");
        assert!((tw - 870.0).abs() < 0.5, "tw was {tw}");
        // Must equal the single full-area grid tile (the `tile_count == 1`
        // pin), the value the normal-grid pin uses — this is the parity the
        // pinned split-tile chrome depends on.
        let (_c, _r, grid_tw) = compute_layout(1, 1240.0, 580.0, 16.0);
        let grid_th = grid_tw / TILE_AR;
        assert!((tw - grid_tw).abs() < 0.5, "tw {tw} != grid_tw {grid_tw}");
        assert!((th - grid_th).abs() < 0.5, "th {th} != grid_th {grid_th}");
    }

    #[test]
    fn ss_pinned_tile_is_independent_of_participant_count() {
        // The whole point of the fix: the pinned split-tile size must NOT
        // track the grid cell size, which shrinks as tiles are added. At 9
        // tiles the grid cell height collapses well below the maximized
        // height, so if this value ever tracked the grid it would regress.
        let (_tw, th_pin) = screen_share_pinned_tile_size(1240.0, 580.0);
        let (_c, _r, grid_tw_9) = compute_layout(9, 1240.0, 580.0, 16.0);
        let grid_th_9 = grid_tw_9 / TILE_AR;
        // Sanity: 9-tile grid cell is far smaller than the maximized pin, and
        // below the 293px chrome-saturation threshold the pin must stay above.
        assert!(
            grid_th_9 < 250.0,
            "9-tile grid th unexpectedly large: {grid_th_9}"
        );
        assert!(
            th_pin > grid_th_9 + 100.0,
            "pinned th {th_pin} not clearly larger than 9-tile grid th {grid_th_9}"
        );
        assert!(
            th_pin >= 293.0,
            "pinned th {th_pin} below chrome-saturation threshold"
        );
    }

    #[test]
    fn ss_pinned_tile_caps_width_in_tall_narrow_viewport() {
        // Portrait/narrow area: a 3:2 tile of full height would overflow the
        // width, so width binds and height derives from it.
        let (tw, th) = screen_share_pinned_tile_size(300.0, 1000.0);
        assert!((tw - 300.0).abs() < 0.5, "tw was {tw}");
        assert!((th - 200.0).abs() < 0.5, "th was {th}");
    }

    #[test]
    fn ss_pinned_tile_never_negative() {
        // Degenerate collapsed viewport must not produce negative sizes.
        let (tw, th) = screen_share_pinned_tile_size(0.0, 0.0);
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

        let selected =
            select_display_candidates(&peers, 1, |_| false, &speech, &join, now, ACTIVE_MS);

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
            680.0,
            "a vertical dock budgets against vh, untouched by the band",
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
            Some("Customize ended to fit the window; layout saved")
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
}

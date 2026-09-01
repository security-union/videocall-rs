// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure layout helpers extracted from `attendants.rs`.
//!
//! These functions are algorithmically non-trivial but have zero WASM / DOM /
//! Dioxus dependencies, so they can be unit-tested under plain `cargo test`.

use super::density::DensityMode;
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

    (best_cols, best_rows, best_tw)
}

/// Nominal camera-tile geometry (`--tile-w`, `--tile-h`) for the screen-share
/// split layout's *maximized* (pinned) tile.
///
/// During screen share the participant panel renders small side-panel
/// thumbnails, but a PINNED side-panel tile is `position: fixed;
/// width/height: 100%` (style.css `.split-peer-tile.grid-item-pinned`) — it
/// maximizes over the shared screen, exactly like `.grid-item-pinned` in the
/// normal grid. That pinned tile's chrome (name badge, top-icon cluster,
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
}

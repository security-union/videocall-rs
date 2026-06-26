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

    // Swap pairs — all indices are disjoint so order doesn't matter.
    let num_swaps = overflow_speakers.len().min(swap_candidates.len());
    for i in 0..num_swaps {
        tiles.swap(swap_candidates[i].0, overflow_speakers[i].0);
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
        // 4 tiles, 2 visible. Both visible peers are active speakers.
        // peer_3 is also an active overflow speaker.
        // No candidates → no swaps.
        let mut tiles = make_tiles(4);
        let mut speech = HashMap::new();
        speech.insert("peer_0".into(), 950.0);
        speech.insert("peer_1".into(), 960.0);
        speech.insert("peer_3".into(), 970.0);

        let original = tiles.clone();
        promote_speakers(&mut tiles, 2, &speech, &HashMap::new(), 1000.0, 500.0);
        assert_eq!(
            tiles, original,
            "No swaps when all visible tiles are active speakers"
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
}

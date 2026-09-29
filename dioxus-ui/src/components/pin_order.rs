// SPDX-License-Identifier: MIT OR Apache-2.0

//! Issue 2866: the ordered pin list. Index 0 is the most recently pinned tile,
//! and a pinned tile is laid out at the front of the grid at its normal size.

use crate::components::canvas_generator::{PinnedTile, PinnedTileKind};

/// CSS `order` of the rank-0 pin; rank `r` sits at `PIN_ORDER_BASE + r`.
pub const PIN_ORDER_BASE: i32 = -1000;
/// A pin click this soon after a pin change on ANOTHER tile is dropped.
pub const PIN_DOUBLE_ACTIVATION_MS: f64 = 350.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinChange {
    Pinned,
    Unpinned,
}

pub fn toggle_pin(list: &mut Vec<PinnedTile>, tile: PinnedTile) -> PinChange {
    if unpin(list, &tile) {
        PinChange::Unpinned
    } else {
        list.insert(0, tile);
        PinChange::Pinned
    }
}

/// Pins `tile` at the front, or moves it there. Returns whether the list changed.
pub fn pin_front(list: &mut Vec<PinnedTile>, tile: PinnedTile) -> bool {
    if list.first() == Some(&tile) {
        return false;
    }
    list.retain(|p| *p != tile);
    list.insert(0, tile);
    true
}

/// Returns whether `tile` was pinned.
pub fn unpin(list: &mut Vec<PinnedTile>, tile: &PinnedTile) -> bool {
    let before = list.len();
    list.retain(|p| p != tile);
    list.len() != before
}

pub fn pin_rank(list: &[PinnedTile], user_id: &str, kind: PinnedTileKind) -> Option<usize> {
    list.iter()
        .position(|p| p.kind == kind && p.user_id == user_id)
}

pub fn tile_order(rank: Option<usize>, unpinned: i32) -> i32 {
    rank.map_or(unpinned, |r| PIN_ORDER_BASE + r as i32)
}

/// One pin-button click: the double-activation guard, then the toggle. `None`
/// when the guard drops the click.
pub fn activate_pin(
    list: &mut Vec<PinnedTile>,
    last: &mut Option<(f64, PinnedTile)>,
    now_ms: f64,
    clicked: PinnedTile,
) -> Option<PinChange> {
    let recent = last.as_ref().map(|(at, tile)| (*at, tile));
    if !accept_pin_activation(now_ms, recent, &clicked) {
        return None;
    }
    let change = toggle_pin(list, clicked.clone());
    *last = Some((now_ms, clicked));
    Some(change)
}

/// Never falls back to the user id, which is often an email.
pub fn pin_announcement(name: Option<&str>, change: PinChange) -> String {
    let who = name.filter(|n| !n.is_empty()).unwrap_or("Participant");
    match change {
        PinChange::Pinned => format!("{who} pinned"),
        PinChange::Unpinned => format!("{who} unpinned"),
    }
}

/// The pins without the stale share pins, or `None` when none is stale. Camera
/// pins are never tested, so they outlive their peer.
pub fn without_stale_share_pins(
    pins: &[PinnedTile],
    is_stale: impl Fn(&PinnedTile) -> bool,
) -> Option<Vec<PinnedTile>> {
    let stale = |p: &PinnedTile| p.kind != PinnedTileKind::Camera && is_stale(p);
    pins.iter()
        .any(stale)
        .then(|| pins.iter().filter(|p| !stale(p)).cloned().collect())
}

pub fn accept_pin_activation(
    now_ms: f64,
    last: Option<(f64, &PinnedTile)>,
    clicked: &PinnedTile,
) -> bool {
    !last.is_some_and(|(at, tile)| {
        (0.0..PIN_DOUBLE_ACTIVATION_MS).contains(&(now_ms - at)) && tile != clicked
    })
}

/// `(session_id, rank)` for every session of a camera-pinned user, in rank
/// order. Share pins and users with no session in `sessions` are skipped.
pub fn camera_pin_sessions(
    pins: &[PinnedTile],
    sessions: &[String],
    user_of: impl Fn(&str) -> String,
) -> Vec<(String, usize)> {
    if !pins.iter().any(|p| p.kind == PinnedTileKind::Camera) {
        return Vec::new();
    }
    let users: Vec<String> = sessions.iter().map(|s| user_of(s)).collect();
    pins.iter()
        .enumerate()
        .filter(|(_, p)| p.kind == PinnedTileKind::Camera)
        .flat_map(|(rank, p)| {
            sessions
                .iter()
                .zip(&users)
                .filter(move |(_, user)| **user == p.user_id)
                .map(move |(session, _)| (session.clone(), rank))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(user: &str) -> PinnedTile {
        PinnedTile::camera(user)
    }

    #[test]
    fn the_most_recent_pin_leads_and_a_re_pin_moves_back_to_the_front() {
        let mut list = Vec::new();
        assert_eq!(toggle_pin(&mut list, cam("a")), PinChange::Pinned);
        assert_eq!(toggle_pin(&mut list, cam("b")), PinChange::Pinned);
        assert_eq!(list, vec![cam("b"), cam("a")]);
        assert_eq!(toggle_pin(&mut list, cam("a")), PinChange::Unpinned);
        assert_eq!(list, vec![cam("b")]);
        assert_eq!(toggle_pin(&mut list, cam("a")), PinChange::Pinned);
        assert_eq!(list, vec![cam("a"), cam("b")]);
    }

    #[test]
    fn a_camera_pin_and_a_share_pin_of_one_user_are_separate_entries() {
        let mut list = Vec::new();
        toggle_pin(&mut list, PinnedTile::screen("a"));
        toggle_pin(&mut list, cam("a"));
        assert_eq!(pin_rank(&list, "a", PinnedTileKind::Camera), Some(0));
        assert_eq!(pin_rank(&list, "a", PinnedTileKind::Screen), Some(1));
        assert_eq!(pin_rank(&list, "a", PinnedTileKind::OwnScreen), None);
        assert_eq!(pin_rank(&list, "b", PinnedTileKind::Camera), None);
    }

    #[test]
    fn pin_front_moves_an_entry_and_reports_a_no_op() {
        let mut list = vec![cam("a"), cam("b")];
        assert!(!pin_front(&mut list, cam("a")));
        assert!(pin_front(&mut list, cam("b")));
        assert_eq!(list, vec![cam("b"), cam("a")]);
        assert!(pin_front(&mut list, cam("c")));
        assert_eq!(list, vec![cam("c"), cam("b"), cam("a")]);
    }

    #[test]
    fn unpin_reports_whether_anything_changed() {
        let mut list = vec![cam("a")];
        assert!(!unpin(&mut list, &cam("b")));
        assert!(unpin(&mut list, &cam("a")));
        assert!(list.is_empty());
    }

    #[test]
    fn a_pinned_tile_orders_ahead_of_every_unpinned_slot() {
        assert_eq!(tile_order(Some(0), 0), -1000);
        assert_eq!(tile_order(Some(2), -3), -998);
        assert_eq!(tile_order(None, -3), -3);
        assert_eq!(tile_order(None, 0), 0);
        let deepest = tile_order(Some(crate::constants::CANVAS_LIMIT + 2), 0);
        assert!(
            deepest < -3,
            "a share tile's unpinned -3 must sort after every pin"
        );
    }

    #[test]
    fn only_a_fast_click_on_a_different_tile_is_dropped() {
        let (a, b) = (cam("a"), cam("b"));
        assert!(accept_pin_activation(1_000.0, None, &a));
        assert!(!accept_pin_activation(1_200.0, Some((1_000.0, &b)), &a));
        assert!(
            accept_pin_activation(1_200.0, Some((1_000.0, &a)), &a),
            "a re-click on the same tile can always undo"
        );
        assert!(accept_pin_activation(1_350.0, Some((1_000.0, &b)), &a));
    }

    #[test]
    fn a_clock_that_steps_back_never_drops_a_click() {
        let (a, b) = (cam("a"), cam("b"));
        assert!(accept_pin_activation(1_000.0, Some((5_000.0, &b)), &a));
    }

    #[test]
    fn a_fast_second_click_on_another_tile_changes_nothing() {
        let (mut list, mut last) = (Vec::new(), None);
        assert_eq!(
            activate_pin(&mut list, &mut last, 0.0, cam("a")),
            Some(PinChange::Pinned)
        );
        assert_eq!(activate_pin(&mut list, &mut last, 100.0, cam("b")), None);
        assert_eq!(list, vec![cam("a")]);
        assert_eq!(
            activate_pin(&mut list, &mut last, 200.0, cam("a")),
            Some(PinChange::Unpinned),
            "the same tile can always undo"
        );
        assert_eq!(
            activate_pin(&mut list, &mut last, 600.0, cam("b")),
            Some(PinChange::Pinned)
        );
        assert_eq!(list, vec![cam("b")]);
    }

    #[test]
    fn the_announcement_never_voices_the_user_id() {
        assert_eq!(
            pin_announcement(Some("Ann"), PinChange::Unpinned),
            "Ann unpinned"
        );
        assert_eq!(
            pin_announcement(None, PinChange::Pinned),
            "Participant pinned"
        );
        assert_eq!(
            pin_announcement(Some(""), PinChange::Pinned),
            "Participant pinned"
        );
    }

    #[test]
    fn a_share_ending_prunes_only_share_pins() {
        let pins = vec![
            cam("bob"),
            PinnedTile::screen("carol"),
            PinnedTile::own_screen("me"),
        ];
        assert_eq!(
            without_stale_share_pins(&pins, |p| p.kind == PinnedTileKind::Screen),
            Some(vec![cam("bob"), PinnedTile::own_screen("me")])
        );
        assert_eq!(
            without_stale_share_pins(&pins, |_| true),
            Some(vec![cam("bob")]),
            "a camera pin outlives its peer"
        );
        assert_eq!(
            without_stale_share_pins(&pins, |_| false),
            None,
            "nothing stale, so no write"
        );
    }

    #[test]
    fn camera_pins_resolve_every_session_of_the_user_at_the_pin_rank() {
        let pins = vec![
            PinnedTile::own_screen("me"),
            cam("bob"),
            PinnedTile::screen("carol"),
            cam("gone"),
            cam("carol"),
        ];
        let sessions: Vec<String> = ["1", "2", "3", "4"].map(String::from).to_vec();
        let user = |s: &str| {
            match s {
                "1" => "carol",
                "2" => "bob",
                "3" => "bob",
                _ => "me",
            }
            .to_string()
        };
        assert_eq!(
            camera_pin_sessions(&pins, &sessions, user),
            vec![("2".into(), 1), ("3".into(), 1), ("1".into(), 4)],
            "share pins never resolve to a camera session (HCL 828)"
        );
        assert!(camera_pin_sessions(&[PinnedTile::screen("bob")], &sessions, user).is_empty());
    }
}

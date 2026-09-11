/*
 * Copyright 2026 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Issue 66: the self view has no decoder but does occupy a grid cell, so both
//! populations are derived here together.

use crate::context::{DockPosition, SelfViewPlacement};

/// Falls back to `Corner` while a share is on screen. Presentation-time only:
/// the stored preference is untouched, so the tile returns by itself.
pub fn effective_self_placement(
    preference: SelfViewPlacement,
    has_screen_share: bool,
) -> SelfViewPlacement {
    if has_screen_share {
        SelfViewPlacement::Corner
    } else {
        preference
    }
}

/// The two tile populations, derived from one set of inputs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SelfTileCounts {
    /// Whether the self tile occupies a grid cell.
    pub self_in_grid: bool,
    /// Tiles that own a decoder. Remote peers and mocks; never the self tile.
    pub decode_count: usize,
    /// Grid cells to lay out: `decode_count` plus the self cell.
    pub layout_count: usize,
}

/// Split the population into decode-bearing and laid-out counts.
pub fn self_tile_counts(
    remote_count: usize,
    mock_count: usize,
    placement: SelfViewPlacement,
    visible: bool,
    has_screen_share: bool,
) -> SelfTileCounts {
    let effective = effective_self_placement(placement, has_screen_share);
    let self_in_grid = visible && effective == SelfViewPlacement::Grid;
    let decode_count = remote_count + mock_count;
    SelfTileCounts {
        self_in_grid,
        decode_count,
        layout_count: decode_count + usize::from(self_in_grid),
    }
}

/// CELL capacity from the min-tile-width loop -> REMOTE capacity. The floor
/// matters: that loop stops at one cell, and a zero here avatars every peer.
pub fn remote_capacity(cells_that_fit: usize, counts: &SelfTileCounts) -> usize {
    if counts.layout_count == 0 {
        return 0;
    }
    cells_that_fit
        .saturating_sub(usize::from(counts.self_in_grid))
        .max(1)
}

/// A lone remote fills the grid only while self is not taking a cell.
pub fn remote_full_bleed(sole_real_tile: bool, counts: &SelfTileCounts) -> bool {
    sole_real_tile && !counts.self_in_grid
}

/// The self tile fills the grid when it is the only cell in it.
pub fn self_full_bleed(counts: &SelfTileCounts) -> bool {
    counts.self_in_grid && counts.layout_count == 1
}

/// The `data-self-placement` attribute value for the self-view nav.
pub fn self_placement_attr(effective: SelfViewPlacement) -> &'static str {
    effective.as_str()
}

/// Announced when a hidden self view is brought back.
pub const SELF_VIEW_SHOWN_ANNOUNCEMENT: &str = "Self view shown.";

/// The persistent stand-in for a hidden self tile is shown only to a
/// participant whose self tile could exist at all — the same `can_stream` gate
/// the self-view nav itself carries.
pub fn self_view_hidden_icon_visible(self_view_visible: bool, can_stream: bool) -> bool {
    !self_view_visible && can_stream
}

/// Only a RIGHT dock occupies the bottom-right lane the icon claims, so the
/// other two positions leave it where the vacated tile was.
pub fn self_view_hidden_icon_dock_modifier(dock: DockPosition) -> &'static str {
    match dock {
        DockPosition::Right => "self-view-hidden-icon--dock-right",
        DockPosition::Bottom | DockPosition::Left => "",
    }
}

pub const SELF_VIEW_SHOW_BUTTON_SELECTOR: &str = "[data-testid='self-view-show-button']";

pub const SELF_VIEW_HIDDEN_TOAST_SELECTOR: &str = ".self-view-hidden-toast";

/// Focus destination when the hide toast is dismissed. `None` leaves focus
/// alone: an Escape pressed from outside the toast destroys nothing the user
/// was on. Otherwise the corner Show icon, or the grid when no icon is mounted
/// — a participant who cannot stream has none.
pub fn self_view_toast_dismiss_focus_target(
    focus_was_inside_toast: bool,
    show_icon_present: bool,
) -> Option<&'static str> {
    if !focus_was_inside_toast {
        return None;
    }
    Some(if show_icon_present {
        SELF_VIEW_SHOW_BUTTON_SELECTOR
    } else {
        "#grid-container"
    })
}

/// How long the corner icon explains itself on arrival. Phones get no hover and
/// the action bar hides its tooltips there, so a returning participant would
/// otherwise meet an unlabelled circle.
pub const SELF_VIEW_HIDDEN_TOOLTIP_REVEAL_MS: u32 = 4_000;

/// What the corner icon's arrival reveal should do on one wake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooltipReveal {
    /// Show the tooltip now and start its timer.
    pub open: bool,
    /// A reveal is owed but the settings overlay is covering the corner.
    pub pending: bool,
}

/// Whether the corner icon should explain itself. A hide toast CONSUMES the
/// reveal, having already said the same thing. The settings overlay (fixed,
/// z 9500) DEFERS it, because a reveal behind it would burn its four seconds
/// unseen; it fires on the first wake the overlay is not covering the corner.
pub fn self_view_tooltip_reveal(
    shown_now: bool,
    shown_before: bool,
    toast_present: bool,
    settings_open: bool,
    pending: bool,
) -> TooltipReveal {
    if !shown_now {
        return TooltipReveal {
            open: false,
            pending: false,
        };
    }
    // Off the arrival edge, only an already-owed reveal is still live: that is
    // what holds this to once per appearance.
    let owed = if shown_before {
        pending
    } else {
        !toast_present
    };
    if owed && !settings_open {
        TooltipReveal {
            open: true,
            pending: false,
        }
    } else {
        TooltipReveal {
            open: false,
            pending: owed,
        }
    }
}

/// Placement-change text only: the toast announces a hide.
pub fn self_view_announcement(effective: SelfViewPlacement) -> &'static str {
    match effective {
        SelfViewPlacement::Grid => "Self view moved to the grid",
        SelfViewPlacement::Corner => "Self view moved to the corner",
    }
}

/// Accessible name for the placement toggle, which reads as the destination.
pub fn placement_toggle_label(effective: SelfViewPlacement) -> &'static str {
    match effective {
        SelfViewPlacement::Grid => "Move self view to corner",
        SelfViewPlacement::Corner => "Move self view to grid",
    }
}

/// The placement a press of the toggle moves to.
pub fn toggled_placement(current: SelfViewPlacement) -> SelfViewPlacement {
    match current {
        SelfViewPlacement::Grid => SelfViewPlacement::Corner,
        SelfViewPlacement::Corner => SelfViewPlacement::Grid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_share_overrides_a_grid_preference_without_changing_it() {
        assert_eq!(
            effective_self_placement(SelfViewPlacement::Grid, true),
            SelfViewPlacement::Corner
        );
        assert_eq!(
            effective_self_placement(SelfViewPlacement::Grid, false),
            SelfViewPlacement::Grid
        );
        assert_eq!(
            effective_self_placement(SelfViewPlacement::Corner, true),
            SelfViewPlacement::Corner
        );
        assert_eq!(
            effective_self_placement(SelfViewPlacement::Corner, false),
            SelfViewPlacement::Corner
        );
    }

    #[test]
    fn corner_placement_leaves_both_counts_remote_only() {
        let c = self_tile_counts(3, 0, SelfViewPlacement::Corner, true, false);
        assert_eq!(c.decode_count, 3);
        assert_eq!(c.layout_count, 3);
        assert!(!c.self_in_grid);
    }

    #[test]
    fn grid_placement_adds_a_cell_but_never_a_decoder() {
        let c = self_tile_counts(3, 0, SelfViewPlacement::Grid, true, false);
        assert_eq!(c.decode_count, 3, "self has no decoder");
        assert_eq!(c.layout_count, 4, "self takes a grid cell");
        assert!(c.self_in_grid);
    }

    #[test]
    fn mock_peers_count_toward_both_populations() {
        let c = self_tile_counts(2, 3, SelfViewPlacement::Grid, true, false);
        assert_eq!(c.decode_count, 5);
        assert_eq!(c.layout_count, 6);
    }

    #[test]
    fn hidden_self_takes_no_cell_in_either_placement() {
        for placement in [SelfViewPlacement::Corner, SelfViewPlacement::Grid] {
            let c = self_tile_counts(2, 0, placement, false, false);
            assert!(!c.self_in_grid, "hidden self never occupies a cell");
            assert_eq!(c.layout_count, 2);
            assert_eq!(c.decode_count, 2);
        }
    }

    #[test]
    fn screen_share_returns_a_grid_self_to_the_corner_counts() {
        let c = self_tile_counts(2, 0, SelfViewPlacement::Grid, true, true);
        assert!(!c.self_in_grid);
        assert_eq!(c.layout_count, 2);
        assert_eq!(c.decode_count, 2);
    }

    #[test]
    fn one_remote_is_full_bleed_only_while_self_is_in_the_corner() {
        let corner = self_tile_counts(1, 0, SelfViewPlacement::Corner, true, false);
        assert!(
            remote_full_bleed(true, &corner),
            "1 remote + corner self = a lone tile"
        );
        assert!(!self_full_bleed(&corner));

        let grid = self_tile_counts(1, 0, SelfViewPlacement::Grid, true, false);
        assert!(
            !remote_full_bleed(true, &grid),
            "1 remote + self in grid = two tiles, neither full-bleed"
        );
        assert!(!self_full_bleed(&grid));
    }

    #[test]
    fn solo_in_grid_makes_the_self_tile_the_full_bleed_one() {
        let grid = self_tile_counts(0, 0, SelfViewPlacement::Grid, true, false);
        assert_eq!(grid.layout_count, 1);
        assert!(self_full_bleed(&grid), "self alone fills the grid");
        assert!(
            !remote_full_bleed(false, &grid),
            "there is no remote tile to fill it"
        );

        let corner = self_tile_counts(0, 0, SelfViewPlacement::Corner, true, false);
        assert_eq!(corner.layout_count, 0);
        assert!(!self_full_bleed(&corner));
        assert!(!remote_full_bleed(false, &corner));
    }

    #[test]
    fn a_grid_self_beside_remotes_is_not_the_lone_cell() {
        for remotes in [1usize, 2, 5] {
            let c = self_tile_counts(remotes, 0, SelfViewPlacement::Grid, true, false);
            assert!(
                !self_full_bleed(&c),
                "{remotes} remotes share the grid with self"
            );
            assert!(!remote_full_bleed(true, &c));
        }
    }

    #[test]
    fn a_hidden_or_share_suppressed_self_leaves_the_lone_remote_full_bleed() {
        let hidden = self_tile_counts(1, 0, SelfViewPlacement::Grid, false, false);
        assert!(remote_full_bleed(true, &hidden));
        let shared = self_tile_counts(1, 0, SelfViewPlacement::Grid, true, true);
        assert!(remote_full_bleed(true, &shared));
    }

    #[test]
    fn placement_attribute_tracks_the_effective_placement() {
        assert_eq!(self_placement_attr(SelfViewPlacement::Corner), "corner");
        assert_eq!(self_placement_attr(SelfViewPlacement::Grid), "grid");
    }

    #[test]
    fn a_one_cell_viewport_still_leaves_room_for_one_remote() {
        let grid = self_tile_counts(1, 0, SelfViewPlacement::Grid, true, false);
        assert_eq!(
            remote_capacity(1, &grid),
            1,
            "a zero here starves the decode limit and avatars every peer"
        );

        let corner = self_tile_counts(1, 0, SelfViewPlacement::Corner, true, false);
        assert_eq!(remote_capacity(1, &corner), 1);
    }

    #[test]
    fn remote_capacity_subtracts_the_self_cell_when_there_is_room() {
        let grid = self_tile_counts(5, 0, SelfViewPlacement::Grid, true, false);
        assert_eq!(remote_capacity(6, &grid), 5);
        assert_eq!(
            remote_capacity(4, &grid),
            3,
            "a shrunken grid sheds remotes"
        );

        let corner = self_tile_counts(5, 0, SelfViewPlacement::Corner, true, false);
        assert_eq!(
            remote_capacity(4, &corner),
            4,
            "the corner tile is not a cell, so it costs no remote capacity"
        );
    }

    #[test]
    fn an_empty_grid_has_no_remote_capacity_to_floor() {
        let empty = self_tile_counts(0, 0, SelfViewPlacement::Corner, true, false);
        assert_eq!(empty.layout_count, 0);
        assert_eq!(
            remote_capacity(0, &empty),
            0,
            "no tiles at all must not be floored up to one"
        );
    }

    #[test]
    fn toggle_label_names_the_destination_not_the_current_placement() {
        assert_eq!(
            placement_toggle_label(SelfViewPlacement::Corner),
            "Move self view to grid"
        );
        assert_eq!(
            placement_toggle_label(SelfViewPlacement::Grid),
            "Move self view to corner"
        );
        assert_eq!(
            toggled_placement(SelfViewPlacement::Corner),
            SelfViewPlacement::Grid
        );
        assert_eq!(
            toggled_placement(SelfViewPlacement::Grid),
            SelfViewPlacement::Corner
        );
    }

    #[test]
    fn announcement_names_the_destination_and_never_a_hide() {
        assert_eq!(
            self_view_announcement(SelfViewPlacement::Grid),
            "Self view moved to the grid"
        );
        assert_eq!(
            self_view_announcement(SelfViewPlacement::Corner),
            "Self view moved to the corner"
        );
        assert_eq!(SELF_VIEW_SHOWN_ANNOUNCEMENT, "Self view shown.");
    }

    #[test]
    fn the_icon_stands_in_only_where_the_self_tile_could_exist() {
        assert!(
            self_view_hidden_icon_visible(false, true),
            "a hidden tile is what the icon exists to restore"
        );
        assert!(
            !self_view_hidden_icon_visible(true, true),
            "a visible tile needs no stand-in"
        );
        assert!(
            !self_view_hidden_icon_visible(false, false),
            "a participant who cannot stream has no self tile to restore"
        );
        assert!(!self_view_hidden_icon_visible(true, false));
    }

    #[test]
    fn the_dock_right_modifier_has_a_matching_stylesheet_rule() {
        let css = include_str!("../../static/style.css");
        // The trailing brace is load-bearing: a bare `.{class}` needle also
        // matches a RENAMED `.{class}ward`, which makes the pin vacuous.
        let needle = format!(
            ".{} {{",
            self_view_hidden_icon_dock_modifier(DockPosition::Right)
        );
        assert!(
            css.contains(&needle),
            "{needle} must have a rule in style.css: the class name is duplicated \
             between Rust and CSS, so a rename on either side silently strips the \
             icon's clearance from a right dock"
        );
    }

    /// Collapse CSS whitespace WITHOUT erasing the descendant combinator: runs
    /// become one space, then spaces touching punctuation are dropped. Stripping
    /// every space would make `.a .b` and `.a.b` the same string, and those are
    /// the two selectors this whole feature turns on.
    fn normalized(css: &str) -> String {
        const AFTER: &str = "{;,:>+~(";
        const BEFORE: &str = "{};,>+~)";
        let mut collapsed = String::with_capacity(css.len());
        let mut pending_space = false;
        for c in css.chars() {
            if c.is_whitespace() {
                pending_space = !collapsed.is_empty();
                continue;
            }
            if pending_space {
                collapsed.push(' ');
                pending_space = false;
            }
            collapsed.push(c);
        }
        let chars: Vec<char> = collapsed.chars().collect();
        let mut out = String::with_capacity(chars.len());
        for (i, &c) in chars.iter().enumerate() {
            if c == ' ' {
                let prev = if i > 0 { chars[i - 1] } else { ' ' };
                let next = *chars.get(i + 1).unwrap_or(&' ');
                if AFTER.contains(prev) || BEFORE.contains(next) {
                    continue;
                }
            }
            out.push(c);
        }
        out
    }

    /// Every declaration block in `css` whose selector list is exactly
    /// `selector`. Plural and unordered on purpose: a rule must be found by what
    /// it declares, not by where it sits in the file.
    fn rule_bodies(css: &str, selector: &str) -> Vec<String> {
        let normalized_css = normalized(css);
        let opener = format!("{selector}{{");
        let mut bodies = Vec::new();
        let mut rest = normalized_css.as_str();
        while let Some(at) = rest.find(&opener) {
            // A preceding selector char would make this a longer selector that
            // merely ENDS with ours, so the match is not the rule we asked for.
            let boundary = rest[..at].chars().next_back();
            let after = &rest[at + opener.len()..];
            if !matches!(boundary, Some(c) if !matches!(c, '{' | '}' | ' ' | ',')) {
                if let Some(end) = after.find('}') {
                    bodies.push(after[..end].to_string());
                }
            }
            rest = after;
        }
        bodies
    }

    #[test]
    fn the_css_normalizer_keeps_a_descendant_apart_from_a_compound() {
        assert_eq!(normalized(".a .b { c: d; }"), ".a .b{c:d;}");
        assert_ne!(
            normalized(".a .b { c: d; }"),
            normalized(".a.b { c: d; }"),
            "collapsing these two would make every selector pin in this file \
             vacuous, since the whole feature turns on the compound form"
        );
    }

    /// The corner control borrows the action bar's button. `global.css` loads
    /// AFTER `style.css`, so at equal specificity the base rule wins and the
    /// corner anchor has to be doubled up to place the button at all.
    #[test]
    fn the_corner_icon_out_specifies_the_action_bar_button_it_borrows() {
        let base = rule_bodies(
            include_str!("../../static/global.css"),
            ".video-control-button",
        );
        assert!(
            base.iter().any(|b| b.contains("position:relative")),
            "this is the declaration the corner anchor exists to beat; if the \
             action-bar button no longer sets `position`, re-derive the override \
             rather than leaving it doubled on faith"
        );
        let corner = rule_bodies(
            include_str!("../../static/style.css"),
            ".video-control-button.self-view-hidden-icon",
        );
        assert!(
            corner.iter().any(|b| b.contains("position:absolute")),
            "a single-class corner anchor loses to `.video-control-button` in \
             global.css, and the button lands in the action bar's flow instead \
             of the grid's corner"
        );
        assert!(
            corner
                .iter()
                .any(|b| b.contains("transition-property:background-color")),
            "the base rule transitions `all`, which animates the runtime \
             bottom/right re-anchors on the main thread"
        );
    }

    #[test]
    fn dismissing_the_toast_prefers_the_corner_icon_over_the_grid() {
        assert_eq!(
            self_view_toast_dismiss_focus_target(true, true),
            Some(SELF_VIEW_SHOW_BUTTON_SELECTOR),
            "the icon is the control that replaces the dismissed toast"
        );
        assert_eq!(
            self_view_toast_dismiss_focus_target(true, false),
            Some("#grid-container"),
            "no icon is mounted for a participant who cannot stream, and focus \
             must not be left on a destroyed node"
        );
        assert_eq!(
            self_view_toast_dismiss_focus_target(false, true),
            None,
            "Escape from elsewhere destroys nothing the user was on, so yanking \
             focus to the corner would be the rudest possible response"
        );
        assert_eq!(self_view_toast_dismiss_focus_target(false, false), None);
    }

    /// `(open, pending)` for brevity in the reveal cases below.
    fn reveal(
        shown_now: bool,
        shown_before: bool,
        toast: bool,
        settings: bool,
        pending: bool,
    ) -> (bool, bool) {
        let r = self_view_tooltip_reveal(shown_now, shown_before, toast, settings, pending);
        (r.open, r.pending)
    }

    #[test]
    fn the_icon_explains_itself_on_an_unobstructed_arrival() {
        assert_eq!(
            reveal(true, false, false, false, false),
            (true, false),
            "a returning participant meets the icon with no toast and no hover"
        );
    }

    #[test]
    fn a_hide_toast_consumes_the_reveal_rather_than_deferring_it() {
        assert_eq!(
            reveal(true, false, true, false, false),
            (false, false),
            "the toast already says what the icon would say, and it must not \
             come back later either"
        );
        assert_eq!(
            reveal(true, true, true, false, false),
            (false, false),
            "and nothing is owed once the toast has been and gone"
        );
    }

    #[test]
    fn the_settings_overlay_defers_the_reveal_until_it_closes() {
        assert_eq!(
            reveal(true, false, false, true, false),
            (false, true),
            "a Preferences hide would otherwise burn all four seconds behind a \
             fixed, full-screen overlay"
        );
        assert_eq!(
            reveal(true, true, false, true, true),
            (false, true),
            "still covered, still owed"
        );
        assert_eq!(
            reveal(true, true, false, false, true),
            (true, false),
            "closing settings is the first wake the corner is visible on"
        );
    }

    #[test]
    fn the_reveal_fires_once_per_appearance_and_resets_when_hidden() {
        assert_eq!(
            reveal(true, true, false, false, false),
            (false, false),
            "already shown with nothing owed: an unrelated re-render must not \
             re-reveal"
        );
        assert_eq!(
            reveal(false, true, false, false, true),
            (false, false),
            "leaving the screen drops the debt, so the next arrival is judged \
             on its own"
        );
        assert_eq!(reveal(false, false, false, false, false), (false, false));
        assert_eq!(
            reveal(false, true, false, true, true),
            (false, false),
            "not shown wins over every other input"
        );
    }

    /// The reveal is worthless without the rule that lets it through on phones,
    /// where `global.css` hides action-bar tooltips outright.
    #[test]
    fn the_reveal_attribute_and_the_phone_opt_out_both_have_rules() {
        let style_src = include_str!("../../static/style.css");
        let reveal = rule_bodies(
            style_src,
            ".video-control-button.self-view-hidden-icon[data-tooltip-open=\"true\"] .tooltip",
        );
        assert!(
            reveal
                .iter()
                .any(|b| b.contains("visibility:visible") && b.contains("opacity:1")),
            "nothing reveals the tooltip without this rule, and the attribute \
             becomes decoration"
        );
        assert!(
            rule_bodies(
                include_str!("../../static/global.css"),
                ".video-control-button .tooltip"
            )
            .iter()
            .any(|b| b.contains("display:none")),
            "this is the phone rule the opt-out exists to beat; if it is gone, \
             delete the opt-out rather than leaving it unexplained"
        );
        assert!(
            rule_bodies(
                style_src,
                ".video-control-button.self-view-hidden-icon .tooltip"
            )
            .iter()
            .any(|b| b.contains("display:flex")),
            "the opt-out must restore the base rule's `display: flex` at 0,3,0 — \
             opacity cannot bring back a `display: none` element"
        );
        assert!(
            normalized(style_src).contains(
                "@media (max-width:640px){.video-control-button.self-view-hidden-icon .tooltip{"
            ),
            "and it must sit inside the phone media query it exists to answer"
        );
    }

    #[test]
    fn only_a_right_dock_shifts_the_icon_off_the_vacated_lane() {
        assert_eq!(
            self_view_hidden_icon_dock_modifier(DockPosition::Right),
            "self-view-hidden-icon--dock-right"
        );
        assert_eq!(
            self_view_hidden_icon_dock_modifier(DockPosition::Bottom),
            "",
            "a bottom dock leaves bottom-right alone"
        );
        assert_eq!(
            self_view_hidden_icon_dock_modifier(DockPosition::Left),
            "",
            "a left dock leaves bottom-right alone"
        );
    }
}

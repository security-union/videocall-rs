// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure sizing math for the detached content window (issue #1842).
//!
//! Split out of the wasm-only `screen_share_detach` module so the aspect-fit /
//! clamp logic is host-testable (plain `#[test]`, no browser). `screen_share_detach`
//! reads the decoded source-canvas dims + the available screen and calls
//! [`detached_window_inner_dims`] to size the Document PiP / `window.open` window
//! to the shared content's aspect.

/// Margin (px) reserved on each axis for OS window chrome + breathing room, so a
/// content-sized window never spills off the available screen.
pub(crate) const DETACHED_MARGIN_PX: i32 = 48;
/// Minimum detached video-area width/height (px); a window never shrinks below
/// this even for an extreme aspect (see [`detached_window_inner_dims`]).
pub(crate) const DETACHED_MIN_W: i32 = 320;
pub(crate) const DETACHED_MIN_H: i32 = 240;
/// Title-bar height (px) the detached DOM renders. MUST match the pinned
/// `.ss-detached-bar { height: 40px }` in `screen_share_detach::DETACHED_CSS`: the
/// sizing math adds it to the video area to get the window's viewport height.
pub(crate) const DETACHED_BAR_H_PX: i32 = 40;

/// Compute the detached window's INNER (viewport) dimensions so the video area
/// matches the shared content's aspect (issue #1842).
///
/// `content_w`/`content_h` are the decoded source-canvas dims (the decoder sets
/// `canvas.width/height` per frame; `peer_decoder.rs`). The video area is fit
/// aspect-preserving into the available screen box minus [`DETACHED_MARGIN_PX`] on
/// each axis and the `bar_h` title bar; the returned height ALREADY includes
/// `bar_h` (window viewport = video area + title bar). Callers must pass positive
/// content dims (the undecoded 300x150 default / `<= 0` is handled upstream with a
/// generic fallback).
///
/// EXTREME ASPECTS — per-axis MIN floor (deliberate, tested): a very wide or very
/// tall share whose aspect-fit falls below the min on one axis is floored on THAT
/// axis independently. This never distorts the content — the `<video>`'s
/// `object-fit: contain` still shows it at its true aspect, so a per-axis floor
/// only adds a little letterbox/pillarbox — and it always yields a sane window (an
/// aspect-preserving floor can be unsatisfiable inside the box for an extreme
/// ratio). The cross-multiply uses `i64` so a 4K-scale product cannot overflow.
pub(crate) fn detached_window_inner_dims(
    content_w: i32,
    content_h: i32,
    avail_w: i32,
    avail_h: i32,
    bar_h: i32,
) -> (i32, i32) {
    // Available box for the VIDEO area (viewport minus chrome margin and the bar),
    // itself floored to the min so a tiny screen still yields the min window.
    let max_vw = (avail_w - DETACHED_MARGIN_PX).max(DETACHED_MIN_W);
    let max_vh = (avail_h - DETACHED_MARGIN_PX - bar_h).max(DETACHED_MIN_H);

    // Fit content aspect into the box, binding on whichever axis is tighter.
    // Content is wider-or-equal than the box  <=>  content_w/content_h >= max_vw/max_vh.
    let (mut vw, mut vh) =
        if (content_w as i64) * (max_vh as i64) >= (content_h as i64) * (max_vw as i64) {
            let vh = ((max_vw as i64 * content_h as i64) / content_w as i64) as i32;
            (max_vw, vh)
        } else {
            let vw = ((max_vh as i64 * content_w as i64) / content_h as i64) as i32;
            (vw, max_vh)
        };

    // Per-axis min floor (see doc): always sane; object-fit preserves true aspect.
    vw = vw.max(DETACHED_MIN_W);
    vh = vh.max(DETACHED_MIN_H);

    (vw, vh + bar_h)
}

// ---------------------------------------------------------------------------
// Issue #1821: Maximize behaviour + button presentation (host-testable pure
// helpers; the wasm `screen_share_detach` module drives the real DOM off them).
// ---------------------------------------------------------------------------

/// How the Maximize control behaves, chosen by how the detached window was opened.
///
/// Document PiP windows are UA-clamped and spec-forbidden from
/// `requestFullscreen`, so they can NEVER fill the display in place (the user's
/// exact complaint on issue #1821) — the only route to a maximized-window geometry
/// is to migrate the mirror into a script-opened popup. A popup can toggle real
/// fullscreen directly.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum MaximizeAction {
    /// Document PiP: open a maximized popup and migrate the mirror into it.
    MigrateToMaximizedPopup,
    /// Popup: toggle real fullscreen on the viewport.
    ToggleFullscreen,
}

/// Select the Maximize behaviour from how the detached window was opened
/// (`via_pip`). The wasm handler matches on this so the choice is guarded by
/// [`maximize_action_selects_by_open_mode`].
pub(crate) fn maximize_action_for(via_pip: bool) -> MaximizeAction {
    if via_pip {
        MaximizeAction::MigrateToMaximizedPopup
    } else {
        MaximizeAction::ToggleFullscreen
    }
}

/// The `window.open` feature string that requests a maximized-window geometry: the
/// FULL available display box (`screen.availLeft/availTop/availWidth/availHeight`),
/// which mirrors the OS taskbar/dock-respecting bounds of an OS-maximized window.
///
/// Script-opened popups are EXPECTED to honour these size/position features (unlike
/// UA-clamped Document PiP windows — the reason Maximize on a PiP window migrates to
/// a popup rather than resizing in place). That the UA actually applies the full box
/// is spec-reasoned but PENDING a live headed receipt on real Chrome Document PiP;
/// the e2e stage will attempt it. Whatever the UA grants, the geometry is a request,
/// not a guarantee, and the mirror renders correctly at any size.
pub(crate) fn maximized_popup_features(left: i32, top: i32, width: i32, height: i32) -> String {
    format!("popup=yes,left={left},top={top},width={width},height={height}")
}

/// Issue #1821: the Maximize button's initial presentation, by open mode. A popup
/// toggles fullscreen (a two-state control → gets `aria-pressed`); a Document PiP
/// performs a one-shot migrate-to-maximized-popup (no pressed state). `label` is
/// the initial `aria-label`/`title`; `glyph` the button text.
pub(crate) struct MaximizeButtonSpec {
    pub label: &'static str,
    pub glyph: &'static str,
    /// Whether the control is a two-state toggle (renders `aria-pressed`).
    pub toggle: bool,
}

pub(crate) fn maximize_button_spec(via_pip: bool) -> MaximizeButtonSpec {
    if via_pip {
        MaximizeButtonSpec {
            // One-shot: first click migrates the mirror to a maximized popup, where
            // the (then-fullscreen-toggle) Maximize button takes over.
            label: "Maximize to full display",
            glyph: "\u{2922}", // U+2922 ⤢
            toggle: false,
        }
    } else {
        MaximizeButtonSpec {
            label: "Enter full screen (Escape to exit)",
            glyph: "\u{26F6}", // U+26F6 ⛶
            toggle: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A common 1080p available screen.
    const AV_W: i32 = 1920;
    const AV_H: i32 = 1080;
    const BAR: i32 = DETACHED_BAR_H_PX;

    /// Video area (returned height minus the bar) preserves the content aspect,
    /// within a small tolerance from integer rounding.
    fn assert_video_aspect(w: i32, h: i32, content_w: i32, content_h: i32) {
        let video_h = h - BAR;
        let got = w as f64 / video_h as f64;
        let want = content_w as f64 / content_h as f64;
        assert!((got - want).abs() < 0.05, "aspect got {got} want {want}");
    }

    #[test]
    fn sixteen_nine_fits_within_screen_and_keeps_aspect() {
        let (w, h) = detached_window_inner_dims(1920, 1080, AV_W, AV_H, BAR);
        assert!(w >= DETACHED_MIN_W && h >= DETACHED_MIN_H);
        assert!(w <= AV_W - DETACHED_MARGIN_PX);
        assert!(h <= AV_H - DETACHED_MARGIN_PX);
        assert_video_aspect(w, h, 1920, 1080);
    }

    #[test]
    fn width_binds_for_wide_content() {
        // 2560x1080 is wider than the box → width-bound (video width == max_vw).
        let (w, h) = detached_window_inner_dims(2560, 1080, AV_W, AV_H, BAR);
        assert_eq!(w, AV_W - DETACHED_MARGIN_PX);
        assert_video_aspect(w, h, 2560, 1080);
    }

    #[test]
    fn height_binds_for_tall_content() {
        // 1080x1920 portrait → height-bound (video height == max_vh).
        let (w, h) = detached_window_inner_dims(1080, 1920, AV_W, AV_H, BAR);
        assert_eq!(h - BAR, AV_H - DETACHED_MARGIN_PX - BAR);
        assert!(w >= DETACHED_MIN_W);
        assert_video_aspect(w, h, 1080, 1920);
    }

    #[test]
    fn bar_height_is_added_to_the_viewport() {
        // For width-bound content the video height is independent of the bar, so
        // the returned height differs by exactly bar_h.
        let (_, h40) = detached_window_inner_dims(2560, 1080, AV_W, AV_H, 40);
        let (_, h0) = detached_window_inner_dims(2560, 1080, AV_W, AV_H, 0);
        assert_eq!(h40 - h0, 40);
    }

    // ── Extreme aspects: per-axis MIN floor keeps the window sane (issue #1842) ──

    #[test]
    fn ultrawide_3440x600_stays_within_box() {
        // a ≈ 5.73, width-bound; the fitted height (~326) is above MIN_H, so no
        // floor — just a short, wide, sane window.
        let (w, h) = detached_window_inner_dims(3440, 600, AV_W, AV_H, BAR);
        assert_eq!(w, AV_W - DETACHED_MARGIN_PX);
        assert!(h - BAR >= DETACHED_MIN_H);
        assert!(h <= AV_H - DETACHED_MARGIN_PX);
        assert_video_aspect(w, h, 3440, 600);
    }

    #[test]
    fn extreme_ultrawide_floors_height_to_min() {
        // 4000x300 (a ≈ 13.3) on a wide-but-short 1600x400 screen: width-bound
        // fit gives height ~116 < MIN_H → floored to MIN_H. Content letterboxes.
        let (w, h) = detached_window_inner_dims(4000, 300, 1600, 400, BAR);
        assert_eq!(h - BAR, DETACHED_MIN_H);
        assert!(w >= DETACHED_MIN_W);
    }

    #[test]
    fn tall_600x2000_floors_width_to_min() {
        // a = 0.3, height-bound; video width ~297 < MIN_W → floored to MIN_W.
        let (w, h) = detached_window_inner_dims(600, 2000, AV_W, AV_H, BAR);
        assert_eq!(w, DETACHED_MIN_W);
        assert!(h - BAR >= DETACHED_MIN_H);
    }

    #[test]
    fn tiny_screen_floors_to_min_window() {
        // Available smaller than min + margin: the box floors to min, so the
        // window is the min video area plus the bar.
        let (w, h) = detached_window_inner_dims(1920, 1080, 100, 100, BAR);
        assert_eq!(w, DETACHED_MIN_W);
        assert_eq!(h, DETACHED_MIN_H + BAR);
    }

    // ── Issue #1821: Maximize behaviour + button presentation ────────────────

    #[test]
    fn maximize_action_selects_by_open_mode() {
        // Document PiP cannot fill the display in place → migrate to a popup.
        assert_eq!(
            maximize_action_for(true),
            MaximizeAction::MigrateToMaximizedPopup
        );
        // A popup can go fullscreen directly.
        assert_eq!(maximize_action_for(false), MaximizeAction::ToggleFullscreen);
    }

    #[test]
    fn maximized_popup_features_uses_full_avail_box() {
        // Distinct values on every axis so a dropped/swapped field is caught.
        assert_eq!(
            maximized_popup_features(11, 22, 1920, 1200),
            "popup=yes,left=11,top=22,width=1920,height=1200"
        );
    }

    #[test]
    fn maximize_button_spec_differs_by_open_mode() {
        let pip = maximize_button_spec(true);
        assert!(
            !pip.toggle,
            "PiP Maximize is a one-shot migrate, not a two-state toggle"
        );
        assert_eq!(pip.label, "Maximize to full display");

        let popup = maximize_button_spec(false);
        assert!(
            popup.toggle,
            "popup Maximize toggles fullscreen (aria-pressed)"
        );
        assert_ne!(
            popup.glyph, pip.glyph,
            "the two modes must show distinct glyphs"
        );
        assert_ne!(popup.label, pip.label);
    }
}

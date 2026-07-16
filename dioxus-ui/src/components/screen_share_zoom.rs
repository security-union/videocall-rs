// SPDX-License-Identifier: MIT OR Apache-2.0

//! Issue 1175: pure zoom / pan math for RECEIVED shared content (a peer's
//! screen share) on the receiving side.
//!
//! This module holds ONLY pure, DOM-free logic — zoom clamping, the step
//! calculators, the reset/fit value, the percentage label, and the pan-offset
//! math — so it can be unit-tested on the host (`cargo test`) without a browser.
//! The Dioxus rendering (a declarative CSS `transform` built from these values)
//! lives in [`super::canvas_generator`]; the imperative detached-window glue
//! lives in [`super::screen_share_detach`]. Neither the render path nor the
//! detach path recompute any of this arithmetic inline — they call the helpers
//! here, which is what the host `#[test]`s below actually guard.

use crate::context::ScreenZoomState;

/// SINGLE SOURCE OF TRUTH for the screen-share `<canvas>` DOM id, given a peer
/// session id. The decode wiring (`ScreenCanvas`), the crop toggle, the
/// `get_peer_screen_canvas_id` client callback, and the detach path
/// (`screen_share_detach::open`) all resolve the canvas through this one
/// function, so the id can never drift between the element and its lookups — a
/// drift would make detach silently no-op. Pinned by `screen_canvas_id_is_stable`.
pub fn screen_canvas_id(peer: &str) -> String {
    format!("screen-share-{peer}")
}

/// Minimum zoom factor. `1.0` == fit-to-tile (the natural resting state). We
/// never shrink below the tile: the screen-share canvas already letterboxes to
/// the tile via CSS `object-fit: contain`, so a sub-1.0 scale would only add
/// empty margins with nothing to pan to.
pub const MIN_ZOOM: f64 = 1.0;

/// Maximum zoom factor. 4x is enough to read small UI text in a shared window
/// without letting the content disappear entirely off-view. Kept modest because
/// zoom is a CSS upscale of an already-rasterized frame — beyond ~4x the
/// upscaled pixels stop carrying useful detail.
pub const MAX_ZOOM: f64 = 4.0;

/// Multiplicative step applied per zoom-in / zoom-out click. 1.25 gives a
/// pleasant ~7 clicks across the full `[1.0, 4.0]` range.
pub const ZOOM_STEP: f64 = 1.25;

/// The reset / "100%" zoom level. Resetting returns the content to exactly
/// filling the tile (fit-to-tile, no pan offset). Issue 1175 ships reset = Fit
/// labeled "100%"; a true 1:1 source-pixel mode is deferred to a follow-up.
pub const RESET_ZOOM: f64 = MIN_ZOOM;

/// Keyboard-pan step in CSS pixels for a single arrow-key press. Small, fixed
/// nudge so a keyboard / switch user can pan the zoomed content without
/// overshooting.
pub const PAN_STEP_PX: f64 = 40.0;

/// Keyboard-pan step in CSS pixels for a single Page Up / Page Down press.
/// Deliberately a large multiple of [`PAN_STEP_PX`] so paging moves a
/// screenful-ish chunk and is clearly coarser than an arrow nudge.
pub const PAN_PAGE_STEP_PX: f64 = PAN_STEP_PX * 8.0;

/// Clamp an arbitrary zoom factor into the supported `[MIN_ZOOM, MAX_ZOOM]`
/// range. Used everywhere a zoom value is produced (button steps, future
/// wheel/pinch input) so an out-of-range value can never reach the rendered
/// transform.
///
/// `NaN` maps to the reset level rather than propagating: a `NaN` scale would
/// collapse the wrapper (`transform: scale(NaN)`), which reads as a frozen /
/// blank tile. Mapping it back to [`RESET_ZOOM`] keeps the content visible.
pub fn clamp_zoom(z: f64) -> f64 {
    if z.is_nan() {
        return RESET_ZOOM;
    }
    z.clamp(MIN_ZOOM, MAX_ZOOM)
}

/// Next zoom level after a single zoom-IN click, clamped to range.
pub fn zoom_in(current: f64) -> f64 {
    clamp_zoom(clamp_zoom(current) * ZOOM_STEP)
}

/// Next zoom level after a single zoom-OUT click, clamped to range.
pub fn zoom_out(current: f64) -> f64 {
    clamp_zoom(clamp_zoom(current) / ZOOM_STEP)
}

/// Whether the content is currently zoomed past the fit level. When `true` the
/// tile is pannable (drag / keyboard) and the reset affordance is meaningful;
/// when `false` there is nothing to pan.
pub fn is_zoomed(z: f64) -> bool {
    clamp_zoom(z) > MIN_ZOOM
}

/// Format a zoom factor as an integer percentage label for the controls (e.g.
/// `1.0 -> "100%"`, `2.5 -> "250%"`). Rounds to the nearest percent so the
/// label is stable across the multiplicative steps.
pub fn zoom_percent_label(z: f64) -> String {
    let pct = (clamp_zoom(z) * 100.0).round() as i64;
    format!("{pct}%")
}

/// Whether the (clamped) zoom is at or beyond the maximum, so the zoom-IN button
/// must be disabled. Clamps first so an out-of-range value still reports the
/// correct limit state.
pub fn at_max_zoom(z: f64) -> bool {
    clamp_zoom(z) >= MAX_ZOOM
}

/// Whether the (clamped) zoom is at or below the minimum (fit), so the zoom-OUT
/// button must be disabled.
pub fn at_min_zoom(z: f64) -> bool {
    clamp_zoom(z) <= MIN_ZOOM
}

/// Maximum absolute pan offset (CSS px) on one axis for a given `scale` and
/// viewport `half_extent` (half the viewport's width or height).
///
/// The content wrapper is rendered as `transform: translate(off) scale(scale)`
/// with `transform-origin: center`, so a wrapper of viewport size `2*half`
/// grows to `2*half*scale` and overflows the viewport by `(scale-1)*half` on
/// each side. Panning further than that would reveal empty space past the
/// content edge, so the offset is bounded to `±(scale-1)*half`. At fit
/// (`scale <= 1`) the bound is 0 — nothing to pan.
pub fn max_pan_offset(scale: f64, half_extent: f64) -> f64 {
    let over = (clamp_zoom(scale) - 1.0).max(0.0);
    (over * half_extent).max(0.0)
}

/// Clamp a pan offset to the pannable range for the current `scale` and
/// viewport `half_extent`. Keeps the zoomed content from being dragged/keyed
/// past its edge into empty space, and (re-)applied whenever `scale` changes so
/// zooming back out cannot leave a stale offset stranded off-view.
pub fn clamp_pan(off: f64, scale: f64, half_extent: f64) -> f64 {
    if off.is_nan() {
        return 0.0;
    }
    let max = max_pan_offset(scale, half_extent);
    off.clamp(-max, max)
}

/// Pure delta calculator for keyboard panning.
///
/// Maps an arrow / page key to the `(dx, dy)` delta (CSS px) ADDED to the pan
/// offset. Sign convention matches the pointer-drag ("grab") path so the whole
/// feature has one mental model: a positive `dx` moves the content right — as if
/// grabbing and dragging it right — which brings its left portion into view.
/// `ArrowRight` therefore nudges the content right exactly like a rightward
/// drag. `Home`/`End` are handled by the caller against the current clamp
/// extents (they need the max offset the pure layer can't know), so they return
/// `None` here alongside any non-pan key — the caller then must NOT
/// `preventDefault`, leaving normal focus navigation untrapped.
pub fn pan_key_delta(key: &str) -> Option<(f64, f64)> {
    match key {
        "ArrowLeft" => Some((-PAN_STEP_PX, 0.0)),
        "ArrowRight" => Some((PAN_STEP_PX, 0.0)),
        "ArrowUp" => Some((0.0, -PAN_STEP_PX)),
        "ArrowDown" => Some((0.0, PAN_STEP_PX)),
        "PageUp" => Some((0.0, -PAN_PAGE_STEP_PX)),
        "PageDown" => Some((0.0, PAN_PAGE_STEP_PX)),
        _ => None,
    }
}

/// Build the next [`ScreenZoomState`] after a zoom change to `next_scale`,
/// re-clamping the existing pan offsets to the new scale's pannable range
/// (`half_w` / `half_h` are half the viewport width / height in CSS px). Zoom
/// anchors on the viewport center, so the offsets are preserved-then-clamped
/// rather than recomputed. Returning to fit (`scale == 1.0`) forces the offsets
/// to 0 because `max_pan_offset` is 0 there.
pub fn zoom_to(
    prev: ScreenZoomState,
    next_scale: f64,
    half_w: f64,
    half_h: f64,
) -> ScreenZoomState {
    let scale = clamp_zoom(next_scale);
    ScreenZoomState {
        scale,
        off_x: clamp_pan(prev.off_x, scale, half_w),
        off_y: clamp_pan(prev.off_y, scale, half_h),
    }
}

/// Build the next [`ScreenZoomState`] after a pan by `(dx, dy)` CSS px, clamped
/// to the current scale's pannable range. A pan at fit is a no-op because the
/// clamp range is 0.
pub fn pan_by(
    prev: ScreenZoomState,
    dx: f64,
    dy: f64,
    half_w: f64,
    half_h: f64,
) -> ScreenZoomState {
    ScreenZoomState {
        scale: prev.scale,
        off_x: clamp_pan(prev.off_x + dx, prev.scale, half_w),
        off_y: clamp_pan(prev.off_y + dy, prev.scale, half_h),
    }
}

/// The CSS `transform` value for a zoom state. Declarative: this string is set
/// as the `transform` style of the content wrapper, so a zoom/pan change only
/// patches an attribute and never recreates the `<canvas>` inside the wrapper.
/// At fit (`scale == 1.0`, no offset) it returns `"none"` so the wrapper carries
/// no transform in the common case.
pub fn transform_css(state: &ScreenZoomState) -> String {
    if !is_zoomed(state.scale) && state.off_x == 0.0 && state.off_y == 0.0 {
        return "none".to_string();
    }
    let scale = clamp_zoom(state.scale);
    format!(
        "translate({}px, {}px) scale({})",
        round2(state.off_x),
        round2(state.off_y),
        round4(scale)
    )
}

/// Round to 2 decimals for compact, stable pixel offsets in the CSS string.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Round to 4 decimals for the scale factor (the multiplicative steps land on
/// values like 1.5625; 4 places keeps them exact enough without a long string).
fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- screen_canvas_id (single source of truth) ---------------------------

    #[test]
    fn screen_canvas_id_is_stable() {
        // Pins the ONE canonical format every call site delegates to. If this
        // format changes, the decode wiring, crop, client callback, and detach
        // path all move together (they call this fn), and this pin is the guard.
        assert_eq!(screen_canvas_id("42"), "screen-share-42");
        assert_eq!(screen_canvas_id("abc"), "screen-share-abc");
    }

    // --- clamp_zoom ----------------------------------------------------------

    #[test]
    fn clamp_below_min_pins_to_min() {
        assert_eq!(clamp_zoom(0.2), MIN_ZOOM);
        assert_eq!(clamp_zoom(-5.0), MIN_ZOOM);
    }

    #[test]
    fn clamp_above_max_pins_to_max() {
        assert_eq!(clamp_zoom(10.0), MAX_ZOOM);
    }

    #[test]
    fn clamp_in_range_is_identity() {
        assert_eq!(clamp_zoom(1.0), 1.0);
        assert_eq!(clamp_zoom(2.5), 2.5);
        assert_eq!(clamp_zoom(4.0), 4.0);
    }

    #[test]
    fn clamp_nan_returns_reset() {
        // A NaN scale would collapse the wrapper (blank tile); map to reset.
        assert_eq!(clamp_zoom(f64::NAN), RESET_ZOOM);
    }

    // --- zoom_in / zoom_out --------------------------------------------------

    #[test]
    fn zoom_in_steps_up_by_factor() {
        assert_eq!(zoom_in(1.0), ZOOM_STEP);
    }

    #[test]
    fn zoom_in_saturates_at_max() {
        assert_eq!(zoom_in(MAX_ZOOM), MAX_ZOOM);
        assert_eq!(zoom_in(3.5), MAX_ZOOM);
    }

    #[test]
    fn zoom_out_steps_down_by_factor() {
        assert!((zoom_out(2.0) - 1.6).abs() < 1e-9);
    }

    #[test]
    fn zoom_out_saturates_at_min() {
        assert_eq!(zoom_out(MIN_ZOOM), MIN_ZOOM);
        assert_eq!(zoom_out(1.1), MIN_ZOOM);
    }

    #[test]
    fn zoom_in_then_out_is_stable_at_fit() {
        let up = zoom_in(1.0);
        assert_eq!(zoom_out(up), MIN_ZOOM);
    }

    // --- is_zoomed / at_min / at_max -----------------------------------------

    #[test]
    fn is_zoomed_false_at_fit_true_when_scaled() {
        assert!(!is_zoomed(MIN_ZOOM));
        assert!(!is_zoomed(0.3));
        assert!(is_zoomed(1.5));
        assert!(is_zoomed(MAX_ZOOM));
    }

    #[test]
    fn at_max_zoom_true_at_and_beyond_limit() {
        assert!(at_max_zoom(MAX_ZOOM));
        assert!(at_max_zoom(10.0));
        assert!(!at_max_zoom(2.0));
        assert!(!at_max_zoom(MIN_ZOOM));
    }

    #[test]
    fn at_min_zoom_true_at_and_below_limit() {
        assert!(at_min_zoom(MIN_ZOOM));
        assert!(at_min_zoom(0.2));
        assert!(!at_min_zoom(2.0));
        assert!(!at_min_zoom(MAX_ZOOM));
    }

    // --- zoom_percent_label --------------------------------------------------

    #[test]
    fn percent_label_formats_and_rounds() {
        assert_eq!(zoom_percent_label(1.0), "100%");
        assert_eq!(zoom_percent_label(2.0), "200%");
        assert_eq!(zoom_percent_label(4.0), "400%");
        assert_eq!(zoom_percent_label(ZOOM_STEP), "125%");
        assert_eq!(zoom_percent_label(1.5625), "156%");
    }

    #[test]
    fn percent_label_clamps_out_of_range() {
        assert_eq!(zoom_percent_label(0.1), "100%");
        assert_eq!(zoom_percent_label(99.0), "400%");
    }

    // --- max_pan_offset / clamp_pan ------------------------------------------

    #[test]
    fn max_pan_offset_zero_at_fit() {
        // At fit there is no overflow, so nothing is pannable on either axis.
        assert_eq!(max_pan_offset(1.0, 400.0), 0.0);
        assert_eq!(max_pan_offset(0.5, 400.0), 0.0);
    }

    #[test]
    fn max_pan_offset_scales_with_overflow() {
        // 2x over a 400px half-extent overflows by (2-1)*400 = 400px each side.
        assert_eq!(max_pan_offset(2.0, 400.0), 400.0);
        // 1.5x over 300px half → 150px.
        assert_eq!(max_pan_offset(1.5, 300.0), 150.0);
    }

    #[test]
    fn clamp_pan_bounds_to_extent() {
        // Within range is identity.
        assert_eq!(clamp_pan(100.0, 2.0, 400.0), 100.0);
        // Beyond the +400 extent pins to +400; beyond -400 pins to -400.
        assert_eq!(clamp_pan(999.0, 2.0, 400.0), 400.0);
        assert_eq!(clamp_pan(-999.0, 2.0, 400.0), -400.0);
    }

    #[test]
    fn clamp_pan_forces_zero_at_fit() {
        // Any offset at fit clamps to 0 (nothing to pan).
        assert_eq!(clamp_pan(250.0, 1.0, 400.0), 0.0);
        assert_eq!(clamp_pan(-250.0, 1.0, 400.0), 0.0);
    }

    #[test]
    fn clamp_pan_nan_is_zero() {
        assert_eq!(clamp_pan(f64::NAN, 2.0, 400.0), 0.0);
    }

    // --- pan_key_delta -------------------------------------------------------

    #[test]
    fn pan_arrow_keys_step_one_axis_by_named_const() {
        assert_eq!(pan_key_delta("ArrowLeft"), Some((-PAN_STEP_PX, 0.0)));
        assert_eq!(pan_key_delta("ArrowRight"), Some((PAN_STEP_PX, 0.0)));
        assert_eq!(pan_key_delta("ArrowUp"), Some((0.0, -PAN_STEP_PX)));
        assert_eq!(pan_key_delta("ArrowDown"), Some((0.0, PAN_STEP_PX)));
    }

    #[test]
    fn pan_page_keys_use_larger_page_step() {
        assert_eq!(pan_key_delta("PageUp"), Some((0.0, -PAN_PAGE_STEP_PX)));
        assert_eq!(pan_key_delta("PageDown"), Some((0.0, PAN_PAGE_STEP_PX)));
        // Page step must be strictly coarser than the arrow step (compile-time).
        const { assert!(PAN_PAGE_STEP_PX > PAN_STEP_PX) };
    }

    #[test]
    fn pan_non_pan_keys_return_none() {
        assert_eq!(pan_key_delta("Enter"), None);
        assert_eq!(pan_key_delta("a"), None);
        assert_eq!(pan_key_delta("Home"), None);
        assert_eq!(pan_key_delta("End"), None);
    }

    // --- zoom_to / pan_by (state transitions) --------------------------------

    #[test]
    fn zoom_to_reclamps_offsets_to_new_scale() {
        // Panned to the edge at 2x (max (2-1)*400 = 400 over half 400), then zoom
        // out to 1.5x (max (1.5-1)*400 = 200): the offset must re-clamp down to
        // 200, not stay at the old 400.
        let prev = ScreenZoomState {
            scale: 2.0,
            off_x: 400.0,
            off_y: -400.0,
        };
        let next = zoom_to(prev, 1.5, 400.0, 400.0);
        assert_eq!(next.scale, 1.5);
        assert_eq!(next.off_x, 200.0);
        assert_eq!(next.off_y, -200.0);
    }

    #[test]
    fn zoom_to_fit_zeroes_offsets() {
        let prev = ScreenZoomState {
            scale: 3.0,
            off_x: 200.0,
            off_y: 150.0,
        };
        let next = zoom_to(prev, 1.0, 400.0, 400.0);
        assert_eq!(next.scale, 1.0);
        assert_eq!(next.off_x, 0.0);
        assert_eq!(next.off_y, 0.0);
    }

    #[test]
    fn pan_by_accumulates_and_clamps() {
        let prev = ScreenZoomState {
            scale: 2.0,
            off_x: 0.0,
            off_y: 0.0,
        };
        // A within-range nudge accumulates.
        let a = pan_by(prev, 40.0, -40.0, 400.0, 400.0);
        assert_eq!(a.off_x, 40.0);
        assert_eq!(a.off_y, -40.0);
        // A huge nudge clamps to the extent, not beyond.
        let b = pan_by(a, 10_000.0, -10_000.0, 400.0, 400.0);
        assert_eq!(b.off_x, 400.0);
        assert_eq!(b.off_y, -400.0);
    }

    #[test]
    fn pan_by_is_noop_at_fit() {
        let prev = ScreenZoomState::default();
        let next = pan_by(prev, 100.0, 100.0, 400.0, 400.0);
        assert_eq!(next, ScreenZoomState::default());
    }

    // --- transform_css -------------------------------------------------------

    #[test]
    fn transform_css_is_none_at_fit() {
        assert_eq!(transform_css(&ScreenZoomState::default()), "none");
    }

    #[test]
    fn transform_css_composes_translate_then_scale() {
        let s = ScreenZoomState {
            scale: 2.0,
            off_x: 12.5,
            off_y: -7.25,
        };
        assert_eq!(transform_css(&s), "translate(12.5px, -7.25px) scale(2)");
    }
}

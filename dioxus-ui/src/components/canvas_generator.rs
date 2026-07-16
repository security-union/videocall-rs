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

use crate::components::icons::crop::CropIcon;
use crate::components::icons::crown::CrownIcon;
use crate::components::icons::mic::MicIcon;
use crate::components::icons::peer::PeerIcon;
use crate::components::icons::push_pin::PushPinIcon;
use crate::components::icons::signal_bars::SignalBarsIcon;
use crate::components::icons::zoom::{DetachIcon, ZoomInIcon, ZoomOutIcon, ZoomResetIcon};
use crate::components::media_metrics_overlay::media_metrics_overlay;
use crate::components::screen_share_zoom;
use crate::components::signal_quality::{SignalInfo, SignalQualityPopup};
// SignalMeterMode is referenced via SignalInfo internally — no direct import
// needed in this file (yet); attendants/peer_tile own the call-site values.
use crate::constants::users_allowed_to_stream;
use crate::context::{
    AppearanceSettings, CroppedTilesCtx, DetachedShareCtx, HostSetCtx, ScreenZoomCtx,
    ScreenZoomState, VideoCallClientCtx,
};
use dioxus::prelude::*;
use dioxus::web::WebEventExt;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use videocall_client::VideoCallClient;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::{window, HtmlCanvasElement, HtmlElement};

// ─── Glow formula constants ───────────────────────────────────────────────────

/// Base outer blur radius in pixels at zero audio level.
const OUTER_BLUR_BASE: f32 = 14.0;
/// Outer blur radius contribution per unit of audio intensity.
const OUTER_BLUR_INTENSITY: f32 = 14.0;

/// Base outer spread in pixels at zero audio level.
const OUTER_SPREAD_BASE: f32 = 1.0;
/// Outer spread contribution per unit of audio intensity.
const OUTER_SPREAD_INTENSITY: f32 = 2.0;
/// Scale for glow bleed at 0% slider: no glow shadow.
const GLOW_BLEED_MIN: f32 = 0.0;
/// Additional glow bleed scale from slider 0% -> 100%.
const GLOW_BLEED_RANGE: f32 = 3.80;
/// Scale for color intensity at 0% brightness: keep a faint hint visible.
const BRIGHTNESS_INTENSITY_MIN: f32 = 0.05;
/// Additional color intensity from brightness 0% -> 100%.
const BRIGHTNESS_INTENSITY_RANGE: f32 = 1.95;

/// Base outer shadow alpha at zero audio level.
const OUTER_ALPHA_BASE: f32 = 0.18;
/// Outer shadow alpha increase per unit of audio intensity.
const OUTER_ALPHA_INTENSITY: f32 = 0.32;

/// Base inner blur radius in pixels at zero audio level.
const INNER_BLUR_BASE: f32 = 10.0;
/// Inner blur radius contribution per unit of audio intensity.
const INNER_BLUR_INTENSITY: f32 = 10.0;
/// Additional inner blur contributed by inner-glow strength² per unit of intensity.
const INNER_BLUR_STRENGTH: f32 = 12.0;

/// Base inner shadow alpha at zero audio level.
const INNER_ALPHA_BASE: f32 = 0.10;
/// Inner shadow alpha increase per unit of audio intensity.
const INNER_ALPHA_INTENSITY: f32 = 0.22;
/// Minimum inner-strength multiplier (prevents inner glow from vanishing when strength = 0).
const INNER_ALPHA_STRENGTH_MIN: f32 = 0.25;
/// Range of the inner-strength multiplier.
const INNER_ALPHA_STRENGTH_RANGE: f32 = 0.75;

/// Base border alpha at zero audio level.
const BORDER_ALPHA_BASE: f32 = 0.50;
/// Border alpha increase per unit of audio intensity.
const BORDER_ALPHA_INTENSITY: f32 = 0.42;
pub(crate) const DEFAULT_TILE_BORDER_COLOR: &str = "rgba(100, 100, 100, 0.30)";
const SILENT_BORDER_RESET_SECONDS: f32 = 0.30;
const GLOW_FADE_IN_SECONDS_DEFAULT: f32 = 0.15;
const GLOW_FADE_OUT_SECONDS_DEFAULT: f32 = 1.50;
const GLOW_FADE_OUT_SECONDS_MAX: f32 = 3.00;

// ─── Shared glow parameter struct ────────────────────────────────────────────

/// Pre-computed glow parameters produced by [`calculate_glow_params`].
pub(crate) struct GlowParams {
    pub outer_blur: f32,
    pub outer_spread: f32,
    pub outer_alpha: f32,
    pub inner_blur: f32,
    pub inner_spread: f32,
    pub inner_alpha: f32,
    /// Border alpha follows brightness intensity so very low brightness can
    /// render a subtle border while higher brightness remains clearly visible.
    pub border_alpha: f32,
}

/// Compute glow shadow parameters from the three driving variables.
///
/// * `intensity`      — current audio level (0.0–1.0), or a fixed preview value
/// * `brightness`     — viewer's glow-brightness setting (0.0–1.0)
/// * `inner_strength` — viewer's inner-glow-strength setting (0.0–1.0)
pub(crate) fn calculate_glow_params(
    intensity: f32,
    brightness: f32,
    inner_strength: f32,
) -> GlowParams {
    let i = intensity.clamp(0.0, 1.0);
    let b = brightness.clamp(0.0, 1.0);
    let s = inner_strength.clamp(0.0, 1.0);
    // Brightness changes ONLY color intensity (alpha), not glow geometry.
    // 50% maps to roughly the previous full-brightness intensity.
    let brightness_intensity = BRIGHTNESS_INTENSITY_MIN + b * BRIGHTNESS_INTENSITY_RANGE;
    // The "Glow" slider controls shadow/bleed geometry. 0% must produce no
    // glow shadow, while 50% is the practical default and 100% is exaggerated.
    let glow_bleed = GLOW_BLEED_MIN + s * GLOW_BLEED_RANGE;
    GlowParams {
        outer_blur: OUTER_BLUR_BASE + i * OUTER_BLUR_INTENSITY * glow_bleed,
        outer_spread: OUTER_SPREAD_BASE + i * OUTER_SPREAD_INTENSITY * glow_bleed,
        outer_alpha: ((OUTER_ALPHA_BASE + i * OUTER_ALPHA_INTENSITY) * brightness_intensity * s)
            .clamp(0.0, 1.0),
        inner_blur: INNER_BLUR_BASE + i * (INNER_BLUR_INTENSITY + INNER_BLUR_STRENGTH * glow_bleed),
        inner_spread: 0.0,
        inner_alpha: ((INNER_ALPHA_BASE + i * INNER_ALPHA_INTENSITY)
            * brightness_intensity
            * (INNER_ALPHA_STRENGTH_MIN
                + INNER_ALPHA_STRENGTH_RANGE * glow_bleed / (GLOW_BLEED_MIN + GLOW_BLEED_RANGE))
            * s)
            .clamp(0.0, 1.0),
        border_alpha: ((BORDER_ALPHA_BASE + i * BORDER_ALPHA_INTENSITY) * brightness_intensity)
            .clamp(0.05, 1.0),
    }
}

/// Compute the inline CSS for the speaking glow on the outer tile container.
/// Emits `box-shadow`, `border-color`, and `transition` values driven by the
/// viewer's local [`AppearanceSettings`].
pub(crate) fn speak_style(
    audio_level: f32,
    speaking_active: bool,
    settings: &AppearanceSettings,
) -> String {
    if !settings.glow_enabled {
        return format!(
            "box-shadow: none; border-color: {DEFAULT_TILE_BORDER_COLOR}; transition: border-color {SILENT_BORDER_RESET_SECONDS:.1}s ease-out, box-shadow {GLOW_FADE_OUT_SECONDS_DEFAULT:.2}s ease-out;"
        );
    }

    let (fade_in_seconds, fade_out_seconds) = glow_transition_seconds(settings.glow_decay);
    if !speaking_active || audio_level <= 0.0 {
        return format!(
            "box-shadow: none; border-color: {DEFAULT_TILE_BORDER_COLOR}; transition: border-color {SILENT_BORDER_RESET_SECONDS:.1}s ease-out, box-shadow {fade_out_seconds:.2}s ease-out;"
        );
    }

    let (r, g, b) = settings.glow_color.to_rgb();
    let p = calculate_glow_params(
        audio_level,
        settings.glow_brightness,
        settings.inner_glow_strength,
    );
    if settings.inner_glow_strength <= f32::EPSILON {
        // @token-exempt: dynamic rgba from settings.glow_color.to_rgb(), not a hardcoded color
        return format!(
            "box-shadow: none; border-color: rgba({r}, {g}, {b}, {:.2}); transition: border-color {fade_in_seconds:.2}s ease-in, box-shadow {fade_in_seconds:.2}s ease-in;", // @token-exempt: dynamic rgba from settings.glow_color.to_rgb(), not a hardcoded color
            p.border_alpha,
        );
    }
    format!(
        "box-shadow: 0 0 {:.0}px {:.0}px rgba({r}, {g}, {b}, {:.2}), \
             inset 0 0 {:.0}px {:.0}px rgba({r}, {g}, {b}, {:.2}); \
             border-color: rgba({r}, {g}, {b}, {:.2}); \
             transition: border-color {fade_in_seconds:.2}s ease-in, box-shadow {fade_in_seconds:.2}s ease-in;",
        p.outer_blur,
        p.outer_spread,
        p.outer_alpha,
        p.inner_blur,
        p.inner_spread,
        p.inner_alpha,
        p.border_alpha,
    )
}

/// Returns `true` when the peer's speaking glow should be suppressed because
/// a different peer is currently pinned.
pub(crate) fn is_speaking_suppressed(is_pinned: bool, pinned_peer_id: Option<&str>) -> bool {
    pinned_peer_id.is_some() && !is_pinned
}

/// Compute the inline CSS for the mic icon glow.
/// Always returns explicit values — no reliance on CSS class for glow reset.
///
/// Two separate signals control different visual properties:
/// - `mic_audio_level` (held 1s after silence) controls the icon COLOR
/// - `glow_audio_level` (raw, same as border) controls the drop-shadow GLOW
///
/// Color and glow intensity are driven by the viewer's local
/// [`AppearanceSettings`].
fn mic_style(mic_audio_level: f32, glow_audio_level: f32, settings: &AppearanceSettings) -> String {
    if !settings.glow_enabled {
        // Respect the global glow toggle for mic visuals too.
        return format!(
            "color: inherit; filter: none; transition: color 5.0s ease-out, filter {GLOW_FADE_OUT_SECONDS_DEFAULT:.2}s ease-out;"
        );
    }

    let (fade_in_seconds, fade_out_seconds) = glow_transition_seconds(settings.glow_decay);

    if mic_audio_level <= 0.0 && glow_audio_level <= 0.0 {
        // Fully silent: fade out both color and filter
        return format!(
            "color: inherit; filter: none; transition: color 5.0s ease-out, filter {fade_out_seconds:.2}s ease-out;"
        );
    }

    let (r, g, b) = settings.glow_color.to_rgb();
    let brightness = settings.glow_brightness.clamp(0.0, 1.0);
    let brightness_curve = brightness * brightness;
    let icon_alpha = (0.4 + brightness_curve * 0.6).clamp(0.24, 1.0);
    let icon_color = format!("rgba({r}, {g}, {b}, {icon_alpha:.2})");

    // Unreachable in practice: the mic hold timer guarantees mic_audio_level
    // stays positive at least as long as glow_audio_level. Handle defensively
    // by showing only the glow without the icon color.
    if mic_audio_level <= 0.0 && glow_audio_level > 0.0 {
        let clamped = glow_audio_level.clamp(0.0, 1.0);
        let glow_i = clamped.sqrt();
        return format!(
            "color: inherit; \
             filter: drop-shadow(0 0 {:.0}px rgba({r}, {g}, {b}, {:.2})) \
                     drop-shadow(0 0 {:.0}px rgba({r}, {g}, {b}, {:.2})); \
             transition: color 5.0s ease-out, filter {fade_in_seconds:.2}s ease-in;",
            8.0 + glow_i * 16.0,
            (0.55 + glow_i * 0.45) * brightness_curve,
            3.0 + glow_i * 8.0,
            (0.60 + glow_i * 0.40) * brightness_curve,
        );
    }
    if mic_audio_level > 0.0 && glow_audio_level <= 0.0 {
        // Held color but raw glow has faded — no drop-shadow
        return format!(
            "color: {icon_color}; filter: none; transition: color 0.05s ease-in, filter {fade_out_seconds:.2}s ease-out;"
        );
    }
    // Both positive: colored icon + scaled drop-shadow glow
    let clamped = glow_audio_level.clamp(0.0, 1.0);
    let glow_i = clamped.sqrt();
    format!(
        "color: {icon_color}; \
         filter: drop-shadow(0 0 {:.0}px rgba({r}, {g}, {b}, {:.2})) \
                 drop-shadow(0 0 {:.0}px rgba({r}, {g}, {b}, {:.2})); \
         transition: color 0.05s ease-in, filter {fade_in_seconds:.2}s ease-in;",
        8.0 + glow_i * 16.0,
        (0.55 + glow_i * 0.45) * brightness_curve,
        3.0 + glow_i * 8.0,
        (0.60 + glow_i * 0.40) * brightness_curve,
    )
}

/// Map the 0.0..1.0 decay slider to glow fade-in/fade-out timings.
///
/// Contracts:
/// - 0% decay -> instant glow on/off; border reset still uses 0.3s
/// - non-zero decay -> fixed 0.15s fade-in (current behavior)
/// - 50% decay -> current 1.50s glow fade-out
/// - 100% decay -> longer 3.00s glow fade-out tail
fn glow_transition_seconds(decay: f32) -> (f32, f32) {
    let d = decay.clamp(0.0, 1.0);
    if d <= f32::EPSILON {
        return (0.0, 0.0);
    }

    let fade_out_seconds = if d <= 0.5 {
        GLOW_FADE_OUT_SECONDS_DEFAULT * (d / 0.5)
    } else {
        GLOW_FADE_OUT_SECONDS_DEFAULT
            + (d - 0.5) / 0.5 * (GLOW_FADE_OUT_SECONDS_MAX - GLOW_FADE_OUT_SECONDS_DEFAULT)
    };

    (GLOW_FADE_IN_SECONDS_DEFAULT, fade_out_seconds)
}

/// Issue #1483: which transport a peer's media is flowing over, for the
/// per-tile "WT"/"WS" badge. `Unknown` covers the raw `"unknown"` string, an
/// empty string, `None`, and any unrecognised value — the badge is NEVER
/// rendered for `Unknown` (see `transport_badge` below), so an unclassified
/// transport produces no badge rather than a misleading one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransportBadge {
    /// WebTransport (the primary production transport).
    Wt,
    /// WebSocket (fallback transport).
    Ws,
    /// Unknown / unreported — no badge rendered.
    Unknown,
}

/// Pure map from the raw per-peer transport string (as carried on the
/// `peer_status` diagnostics `peer_transport` metric) to a [`TransportBadge`].
///
/// `"webtransport"` → `Wt`, `"websocket"` → `Ws`; everything else — including
/// `"unknown"`, the empty string, and any junk value — maps to `Unknown`. Kept
/// pure (no `app_config()` / DOM / signal access) so it is host-unit-testable.
pub fn transport_badge_from_str(raw: &str) -> TransportBadge {
    match raw {
        "webtransport" => TransportBadge::Wt,
        "websocket" => TransportBadge::Ws,
        _ => TransportBadge::Unknown,
    }
}

/// Render the per-tile transport badge (issue #1483) next to the
/// `.signal-indicator` button. Factored out so the markup is shared by all
/// three `.tile-top-icons` arms (split screen-share, split peer-video, and the
/// normal grid tile) instead of being triplicated.
///
/// The caller passes `Some(TransportBadge::Wt | Ws)` ONLY when BOTH the
/// server-side `transportBadgeEnabled` flag is on AND the transport is known —
/// that gating happens once per tile render in `peer_tile.rs` (so the JSON
/// re-parse in `transport_badge_enabled()` is paid once, not per render arm).
/// This helper therefore renders nothing for `None` or `Some(Unknown)`, which
/// keeps the "flag OFF → nothing" and "Unknown → nothing" contract in one place.
fn transport_badge(badge: Option<TransportBadge>) -> Element {
    match badge {
        Some(TransportBadge::Wt) => rsx! {
            span {
                class: "transport-badge transport-badge--wt",
                "aria-label": "Transport reported by peer: WebTransport",
                title: "Transport reported by peer: WebTransport",
                "WT"
            }
        },
        Some(TransportBadge::Ws) => rsx! {
            span {
                class: "transport-badge transport-badge--ws",
                "aria-label": "Transport reported by peer: WebSocket",
                title: "Transport reported by peer: WebSocket",
                "WS"
            }
        },
        // `None` (flag off / no transport yet) or `Some(Unknown)`: render nothing.
        _ => rsx! {},
    }
}

/// Controls what a `PeerTile` renders in the split screen-share layout.
#[derive(Debug, PartialEq, Clone, Default)]
pub enum TileMode {
    /// Normal grid tile — renders screen-share canvas (if active) AND peer video side-by-side.
    #[default]
    Full,
    /// Split-layout left panel — renders only the screen-share canvas for this peer.
    /// Returns empty when the peer is not screen-sharing.
    ScreenOnly,
    /// Split-layout right panel — renders only the peer video tile (no screen-share canvas).
    VideoOnly,
}

/// Outcome of the split-layout eligibility check.
#[derive(Debug, PartialEq)]
pub(crate) enum TileDecision {
    /// Render nothing — the peer should not appear in this panel.
    Empty,
    /// Render the screen-share canvas for this peer.
    RenderScreenShare,
    /// Render the peer video tile (no screen-share canvas).
    RenderVideo,
    /// Not a split-layout mode — fall through to full/grid logic.
    FallThrough,
}

/// Pure decision function: given the tile mode, whether the peer is
/// screen-sharing, and whether the peer is the local user, returns
/// which rendering path to take.
///
/// Extracted so that the branching logic can be tested without requiring
/// a `VideoCallClient`, DOM, or any WASM environment.
pub(crate) fn split_layout_decision(
    mode: &TileMode,
    is_screen_share_enabled: bool,
    is_self_peer: bool,
) -> TileDecision {
    match mode {
        TileMode::ScreenOnly => {
            if !is_screen_share_enabled || is_self_peer {
                TileDecision::Empty
            } else {
                TileDecision::RenderScreenShare
            }
        }
        TileMode::VideoOnly => TileDecision::RenderVideo,
        TileMode::Full => TileDecision::FallThrough,
    }
}

/// Render the "Mute" menu item for a video tile's host-actions menu. Like the
/// other `*_menu_item` helpers, factored out so the markup is shared by all
/// three tile render paths (grid / split / full-bleed) instead of being
/// triplicated inline. The handler is `Some` only when the action is permitted
/// for this peer (gating lives in `peer_tile.rs`).
fn mute_menu_item(on_mute: Option<EventHandler<()>>, mut show_tile_menu: Signal<bool>) -> Element {
    rsx! {
        if let Some(cb) = on_mute {
            button {
                class: "tile-context-menu-item",
                onclick: move |_| {
                    show_tile_menu.set(false);
                    cb.call(());
                },
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    width: "14",
                    height: "14",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    line { x1: "1", y1: "1", x2: "23", y2: "23" }
                    path { d: "M9 9v3a3 3 0 0 0 5.12 2.12M15 9.34V4a3 3 0 0 0-5.94-.6" }
                    path { d: "M17 16.95A7 7 0 0 1 5 12v-2m14 0v2a7 7 0 0 1-.11 1.23" }
                    line { x1: "12", y1: "19", x2: "12", y2: "23" }
                    line { x1: "8", y1: "23", x2: "16", y2: "23" }
                }
                "Mute"
            }
        }
    }
}

/// Render the "Disable video" menu item for a video tile's host-actions menu.
/// Factored out and shared by all three tile render paths; `Some` only when the
/// action is permitted for this peer.
fn disable_video_menu_item(
    on_disable_video: Option<EventHandler<()>>,
    mut show_tile_menu: Signal<bool>,
) -> Element {
    rsx! {
        if let Some(cb) = on_disable_video {
            button {
                class: "tile-context-menu-item",
                onclick: move |_| {
                    show_tile_menu.set(false);
                    cb.call(());
                },
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    width: "14",
                    height: "14",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    path { d: "M16 16v1a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2m5.66 0H14a2 2 0 0 1 2 2v3.34l1 1L23 7v10" }
                    line { x1: "1", y1: "1", x2: "23", y2: "23" }
                }
                "Disable video"
            }
        }
    }
}

/// Render the "Remove from meeting" (kick) menu item for a video tile's
/// host-actions menu. Factored out and shared by all three tile render paths;
/// `Some` only when the action is permitted for this peer.
fn kick_menu_item(on_kick: Option<EventHandler<()>>, mut show_tile_menu: Signal<bool>) -> Element {
    rsx! {
        if let Some(cb) = on_kick {
            button {
                class: "tile-context-menu-item",
                onclick: move |_| {
                    show_tile_menu.set(false);
                    cb.call(());
                },
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    width: "14",
                    height: "14",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    path { d: "M13 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h7" }
                    polyline { points: "17 8 21 12 17 16" }
                    line { x1: "21", y1: "12", x2: "9", y2: "12" }
                }
                "Remove from meeting"
            }
        }
    }
}

/// Render the transfer-host menu item for a video tile's host-actions menu.
/// Factored out so the same markup is shared by all three tile render paths
/// (grid / split / full-bleed) instead of being triplicated inline. The handler
/// is `Some` only when the action is permitted for this peer (gating lives in
/// `peer_tile.rs`).
fn host_promotion_menu_items(
    on_transfer_host: Option<EventHandler<()>>,
    mut show_tile_menu: Signal<bool>,
) -> Element {
    rsx! {
        if let Some(cb) = on_transfer_host {
            button {
                class: "tile-context-menu-item",
                onclick: move |_| {
                    show_tile_menu.set(false);
                    cb.call(());
                },
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    width: "14",
                    height: "14",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    polyline { points: "17 1 21 5 17 9" }
                    path { d: "M3 11V9a4 4 0 0 1 4-4h14" }
                    polyline { points: "7 23 3 19 7 15" }
                    path { d: "M21 13v2a4 4 0 0 1-4 4H3" }
                }
                "Transfer host"
            }
        }
    }
}

/// Audio level pair passed to [`generate_for_peer`] so the two related
/// values travel as one argument (keeps the arg count at 7).
pub struct AudioLevels {
    /// Raw audio level (0.0–1.0) driving the border/glow intensity.
    pub raw: f32,
    /// Mic-held audio level (held 1 s after silence) driving the icon color.
    pub mic: f32,
}

/// HCL bugs #8 + #9: bundled per-tile signal-popup state + callbacks
/// passed into [`generate_for_peer`]. Replaces the previous
/// `Signal<bool>` so the popup state can be owned by a context-wide map
/// (surviving peer-leave-induced remounts and layout switches) and so
/// drag/reanchor wiring lives alongside the toggle/close events.
pub struct SignalPopupHandlers {
    /// Whether the popup is currently open for this tile.
    pub show: bool,
    /// HCL bug #9: `Some(left, top)` when the user has dragged the popup
    /// to a fixed viewport position; `None` re-engages the anchored
    /// follow-the-tile behaviour.
    pub free_position: Option<(f64, f64)>,
    /// Fired when the user clicks the signal-meter icon to toggle the
    /// popup open/closed.
    pub on_toggle: EventHandler<()>,
    /// Fired when the user explicitly dismisses the popup via the "X".
    pub on_close: EventHandler<()>,
    /// HCL bug #9: fired when the user drops the popup at a new
    /// viewport position. The host commits the position to the
    /// popup-state map so the popup re-mounts at the same place on
    /// later renders.
    pub on_drag_commit: EventHandler<(f64, f64)>,
    /// HCL bug #9: fired when the user clicks the 📌 reanchor button so
    /// the popup snaps back to the tile.
    pub on_reanchor: EventHandler<()>,
}

/// Render a single peer tile. If `full_bleed` is true and the peer is not screen sharing,
/// the video tile will occupy the full grid area. The `audio_levels.raw` parameter (0.0–1.0) drives
/// a glow whose intensity scales with voice volume.
/// If `host_user_id` matches the peer's authenticated user_id, a crown icon is displayed next to the name.
///
/// `my_session_id` is the LOCAL session_id (from `VideoCallClient::get_own_session_id`). It is
/// compared against `key` (the peer's session_id) to detect the local user's own tile. Prior
/// versions of this function used the local user_id, which caused sibling same-user sessions to
/// be misidentified as "self" (HCL issue 828): each tab of the same authenticated user has its own
/// distinct session_id but a shared user_id, so a user-id compare collapses sibling tabs into a
/// single "self" tile in split layouts and screen-share paths.
#[allow(clippy::too_many_arguments)]
pub fn generate_for_peer(
    client: &VideoCallClient,
    key: &String,
    full_bleed: bool,
    audio_levels: AudioLevels,
    host_user_id: Option<&str>,
    mode: TileMode,
    my_session_id: Option<&str>,
    signal_info: SignalInfo,
    signal_popup: SignalPopupHandlers,
    mut show_tile_menu: Signal<bool>,
    on_mute: Option<EventHandler<()>>,
    on_disable_video: Option<EventHandler<()>>,
    on_kick: Option<EventHandler<()>>,
    on_transfer_host: Option<EventHandler<()>>,
    pinned_peer_id: Option<&str>,
    on_toggle_pin: EventHandler<String>,
    appearance: &AppearanceSettings,
    // Issue #1466: fired when the user clicks the per-tile PLAY button on a
    // decode-budget-PAUSED tile (only rendered when `paused_by_device`). It
    // carries the tile's `session_id` (`key`) up to `attendants.rs`, which
    // toggles it into `UserRequestedDecodeCtx` so the peer is force-decoded.
    // `PeerTile` supplies a no-op default for call sites that never reach the
    // paused arm, so threading it everywhere is unnecessary.
    on_request_decode: EventHandler<String>,
    // Issue #987, task 1a.4: when `true`, this tile is "off-budget" — the
    // adaptive decode-budget controller has excluded the peer from video decode
    // to save CPU. The tile renders the avatar/initials placeholder instead of a
    // live `<canvas>` (so no decode pipeline is bound) and tags the grid item
    // with `off-budget-tile` for styling / E2E. Audio is unaffected: the peer is
    // simply not in `active_decode_set`. Always `false` on the full-bleed
    // screen-share path. In the split-layout right panel, off-budget SS tiles
    // pass `true` just as the normal grid does.
    force_avatar: bool,
) -> Element {
    let cropped_tiles: Option<Signal<HashMap<String, bool>>> =
        try_use_context::<CroppedTilesCtx>().map(|c| c.0);
    let audio_level = audio_levels.raw;
    let mic_audio_level = audio_levels.mic;
    let signal_level = signal_info.level;
    let signal_history = signal_info.history;
    let meeting_start_ms = signal_info.meeting_start_ms;
    // Pulled out once before rsx so the SignalQualityPopup call sites
    // below can each pass an `Option<String>` clone without hunting
    // through the bundle.
    let signal_transport = signal_info.transport;
    let signal_meter_mode = signal_info.meter_mode;
    // Per-peer RECEIVE layer diag for this peer (resolved upstream in
    // `peer_tile` by `session_id == peer_id`). Cloned per popup call site
    // below so the popup's Layers section matches the perf dialog.
    let signal_receive_diag = signal_info.receive_diag;
    // #1482: this peer's device/hardware info for the popup's compact "Device"
    // line. Resolved upstream in `peer_tile` (same `session_id == peer_id`
    // lookup), cloned per popup call site below.
    let signal_device_info = signal_info.device_info;
    // Issue #1483: per-tile "WT"/"WS" transport badge. `Copy`, so it can be
    // passed to `transport_badge(...)` in each `.tile-top-icons` arm without
    // cloning. Already gated upstream: `Some(Wt | Ws)` only when the
    // `transportBadgeEnabled` flag is on AND the transport is known; `None`
    // otherwise. `transport_badge` renders nothing for `None`/`Unknown`.
    let badge_transport = signal_info.badge_transport;
    // Issue 1768: per-tile media-metrics overlay payload (received/sending
    // res·fps·audio), or `None` when the diagnostics checkbox is off — then
    // `media_metrics_overlay` renders nothing. Only the two VIDEO tile arms
    // (grid + split peer-video) inject it; screen-share panels do not.
    let metrics_overlay = signal_info.metrics_overlay;
    // Bundled popup handlers (lifted out of per-tile state for bugs #8 + #9).
    let SignalPopupHandlers {
        show: show_signal_popup,
        free_position: signal_free_position,
        on_toggle: on_toggle_signal_popup,
        on_close: on_close_signal_popup,
        on_drag_commit: on_drag_commit_signal_popup,
        on_reanchor: on_reanchor_signal_popup,
    } = signal_popup;
    let peer_user_id = client.get_peer_user_id(key).unwrap_or_else(|| key.clone());
    let peer_display_name = client
        .get_peer_display_name(key)
        .unwrap_or_else(|| peer_user_id.clone());

    // Compare authenticated user_id (from JWT/DB) instead of user-chosen display name
    // to prevent spoofing the host crown icon. The current host can change via
    // transfer-host, so prefer the reactive `HostSetCtx` (updated live on
    // HOST_GRANTED/HOST_REVOKED) and fall back to the `host_user_id` prop only
    // when no provider is present (e.g. isolated tests).
    let host_set = try_use_context::<HostSetCtx>();
    let is_host = match host_set.as_ref() {
        Some(hs) => hs.is_host(&peer_user_id),
        None => host_user_id.map(|h| h == peer_user_id).unwrap_or(false),
    };
    let is_guest = client.get_peer_is_guest(key).unwrap_or(false);
    let allowed = users_allowed_to_stream().unwrap_or_default();
    if !allowed.is_empty() && !allowed.contains(&peer_user_id) {
        return rsx! {};
    }

    let is_video_enabled_for_peer = client.is_video_enabled_for_peer(key);
    let is_audio_enabled_for_peer = client.is_audio_enabled_for_peer(key);
    let is_screen_share_enabled_for_peer = client.is_screen_share_enabled_for_peer(key);

    // Issue #987, task 1a.4: an off-budget tile renders the avatar placeholder
    // even when the peer's camera is on, because the decode-budget controller
    // has excluded it from `active_decode_set` (no frames are being decoded for
    // it). `show_canvas` therefore requires BOTH the peer's camera to be on AND
    // the tile not to be forced into avatar mode. When `force_avatar` is false
    // (the no-cap default) this is exactly `is_video_enabled_for_peer`, so
    // behaviour is unchanged.
    let show_canvas = is_video_enabled_for_peer && !force_avatar;

    let is_pinned = pinned_peer_id
        .map(|p| p == peer_user_id.as_str())
        .unwrap_or(false);

    let is_suppressed = is_speaking_suppressed(is_pinned, pinned_peer_id);

    let visible_audio_level = if is_suppressed { 0.0 } else { audio_level };
    let visible_mic_level = if is_suppressed { 0.0 } else { mic_audio_level };

    let is_speaking = visible_mic_level > 0.0;
    let speaking_class = if is_speaking { " speaking-tile" } else { "" };

    let audio_speaking_class = if is_speaking {
        "audio-indicator speaking"
    } else {
        "audio-indicator"
    };

    let tile_style = speak_style(visible_audio_level, is_speaking, appearance);
    let mic_inline_style = mic_style(visible_mic_level, visible_audio_level, appearance);

    // ---- Split-layout: screen-share left panel --------------------------------
    // Self-identification keys on session_id, not user_id: two tabs/devices of
    // the same authenticated user share a user_id but have distinct session_ids,
    // and a sibling session must not be treated as "self" (HCL issue 828).
    if matches!(mode, TileMode::ScreenOnly) {
        // Don't render the local user's own screen share
        if !is_screen_share_enabled_for_peer || my_session_id == Some(key.as_str()) {
            return rsx! {};
        }
    }

    // ---- Split-layout: early return for ScreenOnly / VideoOnly ----------------
    let is_self_peer = my_session_id == Some(key.as_str());
    let decision = split_layout_decision(&mode, is_screen_share_enabled_for_peer, is_self_peer);

    if decision == TileDecision::Empty {
        return rsx! {};
    }

    // ---- Split-layout: screen-share left panel --------------------------------
    if decision == TileDecision::RenderScreenShare {
        let ss_canvas_crop = screen_share_zoom::screen_canvas_id(key);
        let ss_div_id = Rc::new(format!("screen-share-{}-div", &key));
        let ss_div_pin = (*ss_div_id).clone();
        let peer_user_id_for_pin_ss = peer_user_id.clone();
        let ss_name = format!("{}-screen", peer_display_name);
        let ss_name_title = ss_name.clone();
        // HCL bug #2: the shared-content tile gets its own signal-meter
        // icon + popup. The popup-state map keys on `(peer_id, meter_mode)`,
        // so this popup and the matching peer-tile popup coexist without
        // collision. Anchor on the screen-share div so the portal positioner
        // tracks it through layout reflows.
        // HCL follow-up 957 (@token-exempt): anchor the signal-meter
        // popup directly on the signal-quality button so the popup reads
        // as "growing out of" the button on first open. The button id is
        // stable (`<tile-div-id>-signal-btn`), unique per tile, and ASCII
        // safe — mirrors the existing `<tile-div-id>-name` pattern from
        // PR 952.
        let ss_name_id = format!("{}-name", &*ss_div_id);
        let ss_signal_btn_id = format!("{}-signal-btn", &*ss_div_id);
        let ss_anchor_id = ss_signal_btn_id.clone();
        // issue 932 (follow-up to PR 931): the popup now floats via a
        // `position: fixed` portal that escapes the tile's `overflow: hidden`,
        // so the legacy `signal-popup-open` overflow-visible toggle is dead and
        // its class is no longer emitted.
        // Issue 1175 (user-test round): while detached the WHOLE share pane is
        // hidden off-screen at the layout level (`.share-detached` on the grid
        // container), so this tile needs no detached-state markup — no overlay,
        // no inert wrapper. The canvas stays mounted + painting (feeding the
        // detached-window mirror); the pane is just moved off-screen. Detach /
        // zoom / reattach affordances all live in the detached window.
        let ss_split_class = "split-screen-tile";
        return rsx! {
            div {
                id: "{ss_div_id}",
                class: "{ss_split_class}",
                "data-tile-root": "true",
                div {
                    class: "canvas-container video-on",
                    // Issue 1175: zoom/pan viewport wrapping the SAME decoder
                    // canvas. The canvas is never recreated — zoom/pan are a CSS
                    // transform driven declaratively from per-tile signal state.
                    ScreenShareZoomable { peer_id: key.clone() }
                    h4 {
                        id: "{ss_name_id}",
                        class: "floating-name",
                        title: "{ss_name_title}",
                        dir: "auto",
                        span { class: "floating-name-text", "{ss_name}" }
                        if is_guest {
                            span { class: "guest-badge", "Guest" }
                        }
                    }
                    // Issue 1175: zoom / reset / detach controls for the ATTACHED
                    // state (in-window). All handlers are ordinary main-document
                    // Dioxus handlers, so they are always live.
                    ScreenShareZoomControls { peer_id: key.clone(), name: peer_display_name.clone() }
                    div {
                        class: "tile-top-icons",
                        // HCL bug #2: signal-meter icon button on the
                        // shared-content tile. Visually identical to peer
                        // tiles (same `.signal-indicator` class + bars
                        // icon). Toggles the SCREEN-ONLY popup for this
                        // publisher.
                        button {
                            id: "{ss_signal_btn_id}",
                            class: "signal-indicator",
                            "aria-label": "Show screen-share signal quality",
                            // stop_propagation: this is a tile-overlay control, not a
                            // background/grid click, so it must not light-dismiss an
                            // open side panel (issue #1790).
                            onclick: move |e: MouseEvent| {
                                e.stop_propagation();
                                on_toggle_signal_popup.call(());
                            },
                            SignalBarsIcon { level: signal_level.bars(), lost: signal_level.is_lost() }
                        }
                        // Issue #1483: transport badge adjacent to the signal
                        // meter. Renders nothing unless the flag is on AND the
                        // transport is known (gated upstream → `badge_transport`).
                        {transport_badge(badge_transport)}
                        button {
                            onclick: move |e: MouseEvent| {
                                // stop_propagation: tile-overlay control, not a grid
                                // click — must not light-dismiss a side panel (#1790).
                                e.stop_propagation();
                                toggle_pinned_div(&ss_div_pin);
                                on_toggle_pin.call(peer_user_id_for_pin_ss.clone());
                            },
                            class: "pin-icon",
                            PushPinIcon {}
                        }
                        {
                            let ss_crop_class = ss_canvas_crop.clone();
                            rsx! {
                                button {
                                    onclick: move |e: MouseEvent| {
                                        // stop_propagation: tile-overlay control, not a
                                        // grid click — must not light-dismiss a panel (#1790).
                                        e.stop_propagation();
                                        toggle_canvas_crop(&ss_canvas_crop, cropped_tiles);
                                    },
                                    class: if is_canvas_letterboxed(&ss_crop_class, &cropped_tiles) { "crop-icon" } else { "crop-icon active" },
                                    CropIcon {}
                                }
                            }
                        }
                    }
                }
                if show_signal_popup {
                    {
                        let h = signal_history.clone();
                        let popup_peer_id = key.clone();
                        let popup_peer_name = peer_display_name.clone();
                        let popup_transport = signal_transport.clone();
                        let popup_receive_diag = signal_receive_diag.clone();
                        let popup_device_info = signal_device_info.clone();
                        let popup_anchor = ss_anchor_id.clone();
                        rsx! {
                            SignalQualityPopup {
                                peer_id: popup_peer_id,
                                peer_name: popup_peer_name,
                                history: h,
                                meeting_start_ms,
                                transport: popup_transport,
                                anchor_id: popup_anchor,
                                meter_mode: signal_meter_mode,
                                receive_diag: popup_receive_diag,
                                device_info: popup_device_info,
                                free_position: signal_free_position,
                                on_drag_commit: move |p| on_drag_commit_signal_popup.call(p),
                                on_reanchor: move |_| on_reanchor_signal_popup.call(()),
                                on_close: move |_| on_close_signal_popup.call(()),
                            }
                        }
                    }
                }
            }
        };
    }

    // ---- Split-layout: peer video right panel ---------------------------------
    if decision == TileDecision::RenderVideo {
        let peer_video_div_id = Rc::new(format!("peer-video-{}-div", &key));
        let div_id_mobile = (*peer_video_div_id).clone();
        let div_id_pin = (*peer_video_div_id).clone();
        let peer_user_id_for_pin_vo = peer_user_id.clone();
        let pv_canvas_crop = key.clone();
        let key_clone = key.clone();
        let peer_display_name_vo = peer_display_name.clone();
        let title_vo = if is_host {
            format!("Host: {peer_user_id}")
        } else {
            peer_user_id.clone()
        };
        let vo_tile_style = tile_style.clone();
        let vo_mic_style = mic_inline_style.clone();
        let vo_audio_class = audio_speaking_class;
        let vo_speaking = speaking_class;
        let grid_class = if is_video_enabled_for_peer {
            "canvas-container video-on"
        } else {
            "canvas-container"
        };
        // issue 932 (follow-up to PR 931): popup floats via a fixed-position
        // portal, so the dead `signal-popup-open` overflow toggle is gone.
        let split_peer_class = "split-peer-tile";
        // HCL follow-up 957 (@token-exempt): the signal-meter popup
        // anchors directly on the signal-quality button (id below) so
        // the popup overlays the button's top-left corner on first open.
        // The portal positioner reads the button's bounding rect through
        // ResizeObserver / window listeners so the popup stays glued to
        // the button through grid reflows. `split_name_id` is still
        // emitted on the `<h4>` so the fallback walker has a stable
        // tile-relative anchor if the button id lookup ever misses.
        let split_name_id = format!("{}-name", &*peer_video_div_id);
        let split_signal_btn_id = format!("{}-signal-btn", &*peer_video_div_id);
        let split_anchor_id = split_signal_btn_id.clone();
        return rsx! {
            div {
                class: "{split_peer_class}{vo_speaking}",
                id: "{peer_video_div_id}",
                "data-tile-root": "true",
                style: "{vo_tile_style}",
                div {
                    class: "{grid_class}",
                    onclick: move |_| {
                        if is_mobile_viewport() {
                            toggle_pinned_div(&div_id_mobile);
                        }
                    },
                    if show_canvas {
                        UserVideo { id: key_clone.clone(), hidden: false }
                    } else if force_avatar && is_video_enabled_for_peer {
                        // Device-paused avatar: peer's camera is on but our
                        // decode budget excluded this tile. Mirror the grid
                        // path's paused placeholder, with a real PLAY button
                        // (issue #1466) so the user can opt this one peer back
                        // into decode. Camera-OFF tiles never reach this arm
                        // (`is_video_enabled_for_peer` is false for them) — they
                        // fall into the plain `else` below — so the PLAY button
                        // only ever appears on a recoverable "paused" tile.
                        div {
                            // Issue #1466 (B1/B2): the paused placeholder no longer
                            // carries `role="img"` + `aria-label`. A `role="img"`
                            // wrapper collapses its whole subtree into one graphic and
                            // can drop the descendant PLAY <button> from the
                            // accessibility tree. The "paused by your device" reason
                            // now lives on the BUTTON itself (`title` + per-button
                            // `aria-label`), keeping the interactive control fully
                            // exposed to AT while still explaining WHY the tile paused.
                            class: "placeholder-content placeholder-content--paused",
                            // Issue #1466 (B1): PLAY control is now a CENTERED overlay
                            // over the PeerIcon, not a corner badge. The old corner
                            // badge (top/right -6px, 44px via negative margin) grew UP
                            // and RIGHT into the tile corner where
                            // `.canvas-container { overflow: hidden }` CLIPPED it, and
                            // its right edge ran under `.tile-top-icons` (z-index:3,
                            // holds the interactive signal button) — so the real tap
                            // area was well under 44px and ambiguous taps hit the
                            // signal button. A button centered on the placeholder
                            // (which is itself centered in the tile via the flex
                            // `.canvas-container`) gives a full, unclipped ≥44px target
                            // that is far from the corner-pinned `.tile-top-icons`.
                            // `stop_propagation()` runs FIRST so a tap does NOT also
                            // hit the parent `.canvas-container` mobile-pin handler
                            // (mirrors the host-menu button pattern), then request
                            // force-decode for THIS peer's session_id (`key`).
                            {
                                // Owned session_id clone for the `move` onclick:
                                // event handlers must be `'static`, so we cannot
                                // capture the borrowed `key: &String` directly.
                                let request_decode_key = key.clone();
                                rsx! {
                                    button {
                                        r#type: "button",
                                        class: "decode-play-overlay",
                                        // #1466: stable E2E hook for the per-tile
                                        // un-pause (PLAY) control on a
                                        // decode-budget-paused tile.
                                        "data-testid": "decode-play-btn",
                                        "aria-label": format!("Play {peer_display_name}'s video"),
                                        // #1466 (B2): explanatory reason moved off the
                                        // role=img wrapper onto the interactive control
                                        // so it stays accessible without hiding the
                                        // button from AT.
                                        title: "Paused by your device to keep the call smooth. Audio is still on.",
                                        onclick: move |e: MouseEvent| {
                                            e.stop_propagation();
                                            on_request_decode.call(request_decode_key.clone());
                                        },
                                        svg {
                                            width: "20",
                                            height: "20",
                                            view_box: "0 0 24 24",
                                            fill: "currentColor",
                                            stroke: "none",
                                            polygon { points: "8 5 19 12 8 19 8 5" }
                                        }
                                    }
                                }
                            }
                            PeerIcon {}
                            span { class: "placeholder-text", "Video paused" }
                        }
                    } else {
                        div {
                            class: "placeholder-content",
                            PeerIcon {}
                            span { class: "placeholder-text", "Video Disabled" }
                        }
                    }
                    // Issue 1768: media-metrics overlay (bottom-anchored, passive,
                    // pointer-events:none). Empty node when the checkbox is off.
                    {media_metrics_overlay(metrics_overlay.as_ref())}
                    h4 {
                        id: "{split_name_id}",
                        class: "floating-name",
                        title: "{title_vo}",
                        dir: "auto",
                        span { class: "floating-name-text", "{peer_display_name_vo}" }
                        if is_host {
                            CrownIcon {}
                        }
                        if is_guest {
                            span { class: "guest-badge", "Guest" }
                        }
                    }
                    div {
                        class: "tile-top-icons",
                        // Mic icon (rightmost via row-reverse, always visible)
                        div {
                            class: "{vo_audio_class}",
                            style: "{vo_mic_style}",
                            MicIcon { muted: !is_audio_enabled_for_peer }
                        }
                        // Signal icon (always visible, clickable)
                        button {
                            id: "{split_signal_btn_id}",
                            class: "signal-indicator",
                            "aria-label": "Show signal quality",
                            // stop_propagation: tile-overlay control, not a grid
                            // click — must not light-dismiss a side panel (#1790).
                            onclick: move |e: MouseEvent| {
                                e.stop_propagation();
                                on_toggle_signal_popup.call(());
                            },
                            SignalBarsIcon { level: signal_level.bars(), lost: signal_level.is_lost() }
                        }
                        // Issue #1483: transport badge adjacent to the signal
                        // meter (renders nothing unless flag on + transport known).
                        {transport_badge(badge_transport)}
                        // Crop (visible on hover only, hidden when video disabled)
                        if is_video_enabled_for_peer {
                            {
                                let pv_crop_class = pv_canvas_crop.clone();
                                rsx! {
                                    button {
                                        onclick: move |e: MouseEvent| {
                                            // stop_propagation: tile-overlay control, not a
                                            // grid click — must not light-dismiss a panel (#1790).
                                            e.stop_propagation();
                                            toggle_canvas_crop(&pv_canvas_crop, cropped_tiles);
                                        },
                                        class: if is_canvas_letterboxed(&pv_crop_class, &cropped_tiles) { "crop-icon" } else { "crop-icon active" },
                                        CropIcon {}
                                    }
                                }
                            }
                        }
                        // Three-dot host control menu (visible on hover, only for host)
                        if on_mute.is_some()
                            || on_disable_video.is_some()
                            || on_kick.is_some()
                            || on_transfer_host.is_some()
                        {
                            {
                                rsx! {
                                    div { class: "tile-mute-menu-wrapper",
                                        button {
                                            class: "tile-mute-btn",
                                            title: "Host actions",
                                            "aria-label": "Host actions",
                                            onclick: move |e: MouseEvent| {
                                                e.stop_propagation();
                                                show_tile_menu.set(!show_tile_menu());
                                            },
                                            svg {
                                                xmlns: "http://www.w3.org/2000/svg",
                                                width: "16",
                                                height: "16",
                                                view_box: "0 0 24 24",
                                                fill: "none",
                                                stroke: "currentColor",
                                                stroke_width: "2",
                                                stroke_linecap: "round",
                                                stroke_linejoin: "round",
                                                circle { cx: "12", cy: "12", r: "1" }
                                                circle { cx: "12", cy: "5", r: "1" }
                                                circle { cx: "12", cy: "19", r: "1" }
                                            }
                                        }
                                        if show_tile_menu() {
                                            div {
                                                style: "position: fixed; inset: 0; z-index: 99;",
                                                onclick: move |_| show_tile_menu.set(false),
                                            }
                                            div { class: "tile-context-menu",
                                                {mute_menu_item(on_mute, show_tile_menu)}
                                                {disable_video_menu_item(on_disable_video, show_tile_menu)}
                                                {kick_menu_item(on_kick, show_tile_menu)}
                                                {host_promotion_menu_items(on_transfer_host, show_tile_menu)}
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // Pin (visible on hover / when speaking)
                        button {
                            onclick: move |e: MouseEvent| {
                                // stop_propagation: tile-overlay control, not a grid
                                // click — must not light-dismiss a side panel (#1790).
                                e.stop_propagation();
                                toggle_pinned_div(&div_id_pin);
                                on_toggle_pin.call(peer_user_id_for_pin_vo.clone());
                            },
                            class: "pin-icon",
                            PushPinIcon {}
                        }
                    }
                }
                // Signal-quality popup rendered as a sibling of
                // `.canvas-container` (rather than a child) so the
                // tile's `overflow: hidden` border-radius clip from
                // PR #923 cannot cut it off. The popup itself is // @token-exempt: PR ref, not a color
                // `position: fixed` (see `.signal-quality-popup-portal`
                // in style.css) and anchors to this tile by id.
                if show_signal_popup {
                    {
                        let h = signal_history.clone();
                        let popup_peer_id = key.clone();
                        let popup_peer_name = peer_display_name.clone();
                        let popup_transport = signal_transport.clone();
                        let popup_receive_diag = signal_receive_diag.clone();
                        let popup_device_info = signal_device_info.clone();
                        let popup_anchor = split_anchor_id.clone();
                        rsx! {
                            SignalQualityPopup {
                                peer_id: popup_peer_id,
                                peer_name: popup_peer_name,
                                history: h,
                                meeting_start_ms,
                                transport: popup_transport,
                                anchor_id: popup_anchor,
                                meter_mode: signal_meter_mode,
                                receive_diag: popup_receive_diag,
                                device_info: popup_device_info,
                                free_position: signal_free_position,
                                on_drag_commit: move |p| on_drag_commit_signal_popup.call(p),
                                on_reanchor: move |_| on_reanchor_signal_popup.call(()),
                                on_close: move |_| on_close_signal_popup.call(()),
                            }
                        }
                    }
                }
            }
        };
    }

    // Regular grid tile, optionally with screen share tile
    let screen_share_css = if client.is_awaiting_peer_screen_frame(key) {
        "grid-item hidden"
    } else {
        "grid-item"
    };
    let screen_share_div_id = Rc::new(format!("screen-share-{}-div", &key));
    let peer_video_div_id = Rc::new(format!("peer-video-{}-div", &key));

    let ss_div_mobile = (*screen_share_div_id).clone();
    let ss_div_pin = (*screen_share_div_id).clone();
    let ss_canvas_crop = screen_share_zoom::screen_canvas_id(key);
    let ss_name = format!("{}-screen", peer_display_name);

    let pv_div_mobile = (*peer_video_div_id).clone();
    let pv_div_pin = (*peer_video_div_id).clone();
    let pv_canvas_crop = key.clone();
    let key_clone = key.clone();
    let peer_display_name_grid = peer_display_name.clone();
    let peer_user_id_for_pin = peer_user_id.clone();
    let peer_user_id_for_pin_ss = peer_user_id.clone();
    let title_grid = if is_host {
        format!("Host: {peer_user_id}")
    } else {
        peer_user_id.clone()
    };

    // Derive flat &str values so the rsx! condition is a simple != comparison.
    // Self-identification keys on session_id (`key`), not user_id, so sibling
    // same-user sessions get their own screen-share canvas (HCL issue 828).
    let peer_session_id = key.as_str();
    let my_session_id_str = my_session_id.unwrap_or("");

    rsx! {
        // Canvas for Screen share.
        //
        // Issue 1175: this grid-arm (`TileMode::Full`) screen-share render is
        // UNREACHABLE for a RECEIVED (non-self) share, so it deliberately carries
        // no zoom/detach — that's not an asymmetry with the split-layout tile.
        // Any displayed non-self sharer forces `has_screen_share = true` in
        // `AttendantsComponent` (the `active_screen_sharer` stack and this arm's
        // `is_screen_share_enabled_for_peer` prop derive from the SAME
        // `client.is_screen_share_enabled_for_peer`), which routes the sharer to
        // the split layout (`TileMode::ScreenOnly` → `RenderScreenShare`, the
        // zoom/detach-enhanced arm above). This arm is only reached when
        // `has_screen_share = false`, i.e. no displayed non-self peer is sharing.
        if peer_session_id != my_session_id_str && is_screen_share_enabled_for_peer {
            div {
                class: "{screen_share_css}",
                id: "{screen_share_div_id}",
                div {
                    class: "canvas-container video-on",
                    onclick: move |_| {
                        if is_mobile_viewport() {
                            toggle_pinned_div(&ss_div_mobile);
                        }
                    },
                    ScreenCanvas { peer_id: key.clone() }
                    h4 {
                        class: "floating-name",
                        title: "{ss_name}",
                        dir: "auto",
                        span { class: "floating-name-text", "{ss_name}" }
                        if is_guest {
                            span { class: "guest-badge", "Guest" }
                        }
                    }
                    {
                        let ss_crop_class = ss_canvas_crop.clone();
                        rsx! {
                            button {
                                onclick: move |e: MouseEvent| {
                                    // stop_propagation: tile-overlay control, not a grid
                                    // click — must not light-dismiss a side panel (#1790).
                                    e.stop_propagation();
                                    toggle_canvas_crop(&ss_canvas_crop, cropped_tiles);
                                },
                                class: if is_canvas_letterboxed(&ss_crop_class, &cropped_tiles) { "crop-icon" } else { "crop-icon active" },
                                CropIcon {}
                            }
                        }
                    }
                    button {
                        onclick: move |e: MouseEvent| {
                            // stop_propagation: tile-overlay control, not a grid
                            // click — must not light-dismiss a side panel (#1790).
                            e.stop_propagation();
                            toggle_pinned_div(&ss_div_pin);
                            on_toggle_pin.call(peer_user_id_for_pin_ss.clone());
                        },
                        class: "pin-icon",
                        PushPinIcon {}
                    }
                }
            }
        }
        {
            let grid_class = if show_canvas {
                "canvas-container video-on"
            } else {
                "canvas-container"
            };
            let grid_tile_style = tile_style.clone();
            let grid_mic_style = mic_inline_style.clone();
            let grid_speaking = speaking_class;
            // issue 508: the surviving single peer (full_bleed) is now rendered
            // from THIS one grid template with `full-bleed` as a plain CLASS
            // toggle, instead of a separate full-bleed rsx! branch. Dioxus 0.7
            // diffs by template-pointer identity, so the old branch swap tore
            // down the `<canvas>` and rebuilt the renderer (last_width:0 → resize
            // → FPS collapse) on every 2<->1 transition. Keeping one template
            // lets Dioxus diff the tile in place and REUSE the same `<canvas>`
            // node. The className is built to be byte-identical to the previous
            // behaviour: "grid-item" in the normal grid, "grid-item full-bleed"
            // for the single surviving peer.
            //
            // issue 932 (follow-up to PR 931): the former " signal-popup-open"
            // suffix is dropped — the popup now floats via a `position: fixed`
            // portal that escapes the tile's `overflow: hidden`, so the
            // overflow-visible toggle that class drove is dead.
            let mut grid_item_class = String::from("grid-item");
            if full_bleed {
                grid_item_class.push_str(" full-bleed");
            }
            // HCL follow-up 957 (@token-exempt): anchor the popup on
            // the tile's signal-quality button (id below) so the popup
            // overlays the button's top-left corner on first open.
            // `grid_name_id` is still emitted on the `<h4>` for the
            // fallback walker.
            let grid_name_id = format!("{}-name", &*peer_video_div_id);
            let grid_signal_btn_id = format!("{}-signal-btn", &*peer_video_div_id);
            let grid_anchor_id = grid_signal_btn_id.clone();
            // Placeholder wording reflects WHY there is no video:
            //   - camera genuinely off               → "Video Disabled" (unchanged)
            //   - camera on but decode budget-paused  → "Video paused" (task 1a.4)
            // An off-budget tile whose camera is also off keeps the camera-off
            // wording, since that is the more fundamental reason.
            // Distinguish "paused by MY device" (decode budget) from "camera off
            // by THEIR choice" (#1142 Phase 1, Part C). `paused_by_device` is true
            // only when this tile was forced to an avatar by the local decode
            // budget while the peer's camera is actually ON — i.e. we are choosing
            // not to decode their live video. That case gets a distinct pause
            // glyph + tooltip; a genuine camera-off tile keeps the plain wording.
            let paused_by_device = force_avatar && is_video_enabled_for_peer;
            // Issue #1465: only DASH a tile (`.off-budget-tile`) when it is a
            // budget-SHED tile that actually has video to decode — i.e. the
            // local decode budget chose not to decode a live stream. A genuine
            // camera-off real peer (force_avatar but camera off) must render a
            // PLAIN avatar with no dash: there is nothing being shed, so the
            // "paused/sheddable" outline is misleading (the field complaint).
            //   real camera-OFF  → is_video_enabled_for_peer false, is_mock false
            //                      → no dash (the #1465 fix)
            //   real camera-ON, budget-shed → is_video_enabled_for_peer true → dash
            //   mock, budget-shed → is_video_enabled_for_peer is FALSE for mocks
            //                      (non-numeric key), so the `is_mock` OR is what
            //                      keeps the mock's dash for local layout testing.
            // Tag with `off-budget-tile` so CSS can style it and E2E can query
            // `.grid-item.off-budget-tile`. Empty string in the no-cap path
            // (force_avatar false), so the class list is unchanged with no budget.
            let is_mock = key.starts_with("mock-");
            let off_budget_class = if force_avatar && (is_mock || is_video_enabled_for_peer) {
                " off-budget-tile"
            } else {
                ""
            };
            let placeholder_label = if paused_by_device {
                "Video paused"
            } else {
                "Video Disabled"
            };
            // issue 508: the full-bleed single peer used to read "Camera Off"
            // in its now-deleted separate template. Preserve that exact visible
            // text here, WITHIN this one template, for the plain camera-off arm
            // only. A full-bleed tile has force_avatar == false, so
            // paused_by_device is always false for it and it always lands in the
            // plain `else` arm below — the paused arm keeps using
            // `placeholder_label` and is unreachable for full-bleed tiles.
            let camera_off_label = if full_bleed {
                "Camera Off"
            } else {
                placeholder_label
            };
            // Tooltip / screen-reader text for the device-paused case. Empty for a
            // normal camera-off tile so nothing extra is announced there.
            let paused_help = if paused_by_device {
                "Paused by your device to keep the call smooth. Audio is still on."
            } else {
                ""
            };
            rsx! {
                div {
                    class: "{grid_item_class}{grid_speaking}{off_budget_class}",
                    id: "{peer_video_div_id}",
                    "data-tile-root": "true",
                    "data-off-budget": if force_avatar { "true" } else { "false" },
                    style: "{grid_tile_style}",
                    // One canvas for the User Video
                    div {
                        class: "{grid_class}",
                        onclick: move |_| {
                            if is_mobile_viewport() {
                                toggle_pinned_div(&pv_div_mobile);
                            }
                        },
                        if show_canvas {
                            UserVideo { id: key_clone.clone(), hidden: false }
                        } else if paused_by_device {
                            // Device-paused avatar: PeerIcon + a PLAY button so it
                            // reads as "paused by us, click to resume", not "camera
                            // off". `title` + `aria-label` on the placeholder
                            // explain WHY and reassure that audio is unaffected (the
                            // mic indicator below stays live regardless).
                            // `paused_by_device` is ONLY true when the peer's camera
                            // is on but our budget excluded the tile — a genuine
                            // camera-off tile lands in the plain `else` arm below and
                            // never gets this PLAY affordance (issue #1466).
                            div {
                                // Issue #1466 (B2): dropped `role="img"` +
                                // `aria-label` from this paused placeholder. The
                                // role=img wrapper collapses the subtree into a single
                                // graphic and can hide the descendant PLAY <button>
                                // from AT. The `{paused_help}` reason now rides on the
                                // BUTTON (`title` + per-button `aria-label`), so it
                                // stays accessible without masking the control.
                                class: "placeholder-content placeholder-content--paused",
                                // Issue #1466 (B1): PLAY control is a CENTERED overlay
                                // over the PeerIcon, not a corner badge. The old corner
                                // badge was clipped by `.canvas-container { overflow:
                                // hidden }` and overlapped the corner-pinned
                                // `.tile-top-icons` (interactive signal button), so its
                                // real tap area was sub-44px and ambiguous. Centering
                                // over the placeholder (itself centered in the tile)
                                // yields a full, unclipped ≥44px target clear of the
                                // corner icons. `stop_propagation` runs FIRST so a
                                // mobile tap does not also fire the parent
                                // `.canvas-container` pin handler, then force-decode
                                // THIS peer via its session_id (`key`).
                                {
                                    // Owned session_id clone for the `move`
                                    // onclick (handlers must be `'static`; the
                                    // borrowed `key: &String` cannot be captured).
                                    let request_decode_key = key.clone();
                                    rsx! {
                                        button {
                                            r#type: "button",
                                            class: "decode-play-overlay",
                                            // #1466: stable E2E hook for the
                                            // per-tile un-pause (PLAY) control on a
                                            // decode-budget-paused tile.
                                            "data-testid": "decode-play-btn",
                                            "aria-label": format!("Play {peer_display_name}'s video"),
                                            // #1466 (B2): explanatory reason moved off
                                            // the role=img wrapper onto the interactive
                                            // control so it stays accessible.
                                            title: "{paused_help}",
                                            onclick: move |e: MouseEvent| {
                                                e.stop_propagation();
                                                on_request_decode.call(request_decode_key.clone());
                                            },
                                            svg {
                                                width: "20",
                                                height: "20",
                                                view_box: "0 0 24 24",
                                                fill: "currentColor",
                                                stroke: "none",
                                                polygon { points: "8 5 19 12 8 19 8 5" }
                                            }
                                        }
                                    }
                                }
                                PeerIcon {}
                                span { class: "placeholder-text", "{placeholder_label}" }
                            }
                        } else {
                            div { class: "placeholder-content",
                                PeerIcon {}
                                span { class: "placeholder-text", "{camera_off_label}" }
                            }
                        }
                        // Issue 1768: media-metrics overlay (bottom-anchored, passive,
                        // pointer-events:none). Empty node when the checkbox is off.
                        {media_metrics_overlay(metrics_overlay.as_ref())}
                        h4 {
                            id: "{grid_name_id}",
                            class: "floating-name",
                            title: "{title_grid}",
                            dir: "auto",
                            span { class: "floating-name-text", "{peer_display_name_grid}" }
                            if is_host {
                                CrownIcon {}
                            }
                            if is_guest {
                                span { class: "guest-badge", "Guest" }
                            }
                        }
                        div {
                            class: "tile-top-icons",
                            // Mic icon (rightmost via row-reverse, always visible)
                            div {
                                class: "{audio_speaking_class}",
                                style: "{grid_mic_style}",
                                MicIcon { muted: !is_audio_enabled_for_peer }
                            }
                            // Signal icon (always visible, clickable)
                            button {
                                id: "{grid_signal_btn_id}",
                                class: "signal-indicator",
                                "aria-label": "Show signal quality",
                                // stop_propagation: tile-overlay control, not a grid
                                // click — must not light-dismiss a side panel (#1790).
                                onclick: move |e: MouseEvent| {
                                    e.stop_propagation();
                                    on_toggle_signal_popup.call(());
                                },
                                SignalBarsIcon { level: signal_level.bars(), lost: signal_level.is_lost() }
                            }
                            // Issue #1483: transport badge adjacent to the signal
                            // meter (renders nothing unless flag on + transport known).
                            {transport_badge(badge_transport)}
                            // Crop (visible on hover only). Gated on `show_canvas`
                            // so off-budget avatar tiles — which have no canvas —
                            // don't show a no-op crop button (task 1a.4).
                            if show_canvas {
                                {
                                    let pv_crop_class = pv_canvas_crop.clone();
                                    rsx! {
                                        button {
                                            onclick: move |e: MouseEvent| {
                                                // stop_propagation: tile-overlay control, not a
                                                // grid click — must not light-dismiss a panel (#1790).
                                                e.stop_propagation();
                                                toggle_canvas_crop(&pv_canvas_crop, cropped_tiles);
                                            },
                                            class: if is_canvas_letterboxed(&pv_crop_class, &cropped_tiles) { "crop-icon" } else { "crop-icon active" },
                                            CropIcon {}
                                        }
                                    }
                                }
                            }
                            // Three-dot host control menu (visible on hover, only for host)
                            if on_mute.is_some()
                                || on_disable_video.is_some()
                                || on_kick.is_some()
                                || on_transfer_host.is_some()
                            {
                                {
                                    rsx! {
                                        div { class: "tile-mute-menu-wrapper",
                                            button {
                                                class: "tile-mute-btn",
                                                title: "Host actions",
                                                "aria-label": "Host actions",
                                                onclick: move |e: MouseEvent| {
                                                    e.stop_propagation();
                                                    show_tile_menu.set(!show_tile_menu());
                                                },
                                                svg {
                                                    xmlns: "http://www.w3.org/2000/svg",
                                                    width: "16",
                                                    height: "16",
                                                    view_box: "0 0 24 24",
                                                    fill: "none",
                                                    stroke: "currentColor",
                                                    stroke_width: "2",
                                                    stroke_linecap: "round",
                                                    stroke_linejoin: "round",
                                                    circle { cx: "12", cy: "12", r: "1" }
                                                    circle { cx: "12", cy: "5", r: "1" }
                                                    circle { cx: "12", cy: "19", r: "1" }
                                                }
                                            }
                                            if show_tile_menu() {
                                                div {
                                                    style: "position: fixed; inset: 0; z-index: 99;",
                                                    onclick: move |_| show_tile_menu.set(false),
                                                }
                                                div { class: "tile-context-menu",
                                                    {mute_menu_item(on_mute, show_tile_menu)}
                                                    {disable_video_menu_item(on_disable_video, show_tile_menu)}
                                                    {kick_menu_item(on_kick, show_tile_menu)}
                                                    {host_promotion_menu_items(on_transfer_host, show_tile_menu)}
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            // Pin (visible on hover / when speaking)
                            button {
                                onclick: move |e: MouseEvent| {
                                    // stop_propagation: tile-overlay control, not a grid
                                    // click — must not light-dismiss a side panel (#1790).
                                    e.stop_propagation();
                                    toggle_pinned_div(&pv_div_pin);
                                    on_toggle_pin.call(peer_user_id_for_pin.clone());
                                },
                                class: "pin-icon",
                                PushPinIcon {}
                            }
                        }
                    }
                    // Popup hoisted out of `.canvas-container` so PR #923's // @token-exempt: PR ref, not a color
                    // border-radius `overflow: hidden` clip cannot crop it.
                    if show_signal_popup {
                        {
                            let h = signal_history.clone();
                            let popup_peer_id = key.clone();
                            let popup_peer_name = peer_display_name.clone();
                            let popup_transport = signal_transport.clone();
                            let popup_receive_diag = signal_receive_diag.clone();
                            let popup_device_info = signal_device_info.clone();
                            let popup_anchor = grid_anchor_id.clone();
                            rsx! {
                                SignalQualityPopup {
                                    peer_id: popup_peer_id,
                                    peer_name: popup_peer_name,
                                    history: h,
                                    meeting_start_ms,
                                    transport: popup_transport,
                                    anchor_id: popup_anchor,
                                    meter_mode: signal_meter_mode,
                                    receive_diag: popup_receive_diag,
                                    device_info: popup_device_info,
                                    free_position: signal_free_position,
                                    on_drag_commit: move |p| on_drag_commit_signal_popup.call(p),
                                    on_reanchor: move |_| on_reanchor_signal_popup.call(()),
                                    on_close: move |_| on_close_signal_popup.call(()),
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn UserVideo(id: String, hidden: bool) -> Element {
    let client = use_context::<VideoCallClientCtx>();
    let cropped_tiles = try_use_context::<CroppedTilesCtx>().map(|c| c.0);
    let id_for_effect = id.clone();
    let id_for_class = id.clone();

    use_effect(move || {
        if let Some(elem) = gloo_utils::document().get_element_by_id(&id_for_effect) {
            if let Ok(canvas) = elem.dyn_into::<HtmlCanvasElement>() {
                let client_ref = client.clone();
                let id_ref = id_for_effect.clone();
                if let Err(e) = client_ref.set_peer_video_canvas(&id_ref, canvas.clone()) {
                    log::debug!("Canvas not yet ready for peer {id_ref}: {e:?}");
                }
            }
        }
    });

    let crop_class = if is_canvas_letterboxed(&id_for_class, &cropped_tiles) {
        "uncropped"
    } else {
        "cropped"
    };

    rsx! {
        canvas {
            id: "{id}",
            hidden: hidden,
            class: crop_class,
        }
    }
}

#[component]
fn ScreenCanvas(peer_id: String) -> Element {
    let client = use_context::<VideoCallClientCtx>();
    let cropped_tiles = try_use_context::<CroppedTilesCtx>().map(|c| c.0);
    // Single source of truth (shared with the detach path + client callback).
    let canvas_id = screen_share_zoom::screen_canvas_id(&peer_id);
    let canvas_id_for_effect = canvas_id.clone();
    let canvas_id_for_class = canvas_id.clone();
    let peer_id_for_effect = peer_id.clone();

    use_effect(move || {
        if let Some(elem) = gloo_utils::document().get_element_by_id(&canvas_id_for_effect) {
            if let Ok(canvas) = elem.dyn_into::<HtmlCanvasElement>() {
                let client_ref = client.clone();
                let peer_id_ref = peer_id_for_effect.clone();
                if let Err(e) = client_ref.set_peer_screen_canvas(&peer_id_ref, canvas.clone()) {
                    log::debug!("Screen canvas not yet ready for peer {peer_id_ref}: {e:?}");
                }
            }
        }
    });

    let crop_class = if is_canvas_letterboxed(&canvas_id_for_class, &cropped_tiles) {
        "uncropped"
    } else {
        "cropped"
    };

    rsx! {
        canvas {
            id: "{canvas_id}",
            class: crop_class,
        }
    }
}

// ─── Issue 1175: received-shared-content zoom / pan / detach ──────────────────

/// Read the current zoom state for `peer` from the shared per-tile map.
fn read_zoom_state(ctx: &Signal<HashMap<String, ScreenZoomState>>, peer: &str) -> ScreenZoomState {
    ctx.read().get(peer).copied().unwrap_or_default()
}

/// Write the zoom state for `peer`, pruning the entry back out when it returns
/// to the default fit state so an un-zoomed tile stores nothing.
fn write_zoom_state(
    ctx: &mut Signal<HashMap<String, ScreenZoomState>>,
    peer: &str,
    state: ScreenZoomState,
) {
    ctx.with_mut(|map| {
        if state == ScreenZoomState::default() {
            map.remove(peer);
        } else {
            map.insert(peer.to_string(), state);
        }
    });
}

/// Half the zoom viewport's client width/height (CSS px), for pan clamping.
/// `None` when the element isn't in the DOM yet or has zero size.
fn viewport_half_dims(viewport_id: &str) -> Option<(f64, f64)> {
    let el = window()?.document()?.get_element_by_id(viewport_id)?;
    let w = el.client_width() as f64;
    let h = el.client_height() as f64;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    Some((w / 2.0, h / 2.0))
}

/// Move keyboard focus to the element with `id`, if present and focusable.
/// Used to keep focus with the detach/reattach mode change so it never drops to
/// `<body>` (the a11y blocker class). No-op if the element is gone.
fn focus_element_by_id(id: &str) {
    if let Some(el) = window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
    {
        if let Ok(html) = el.dyn_into::<HtmlElement>() {
            let _ = html.focus();
        }
    }
}

/// Map a Dioxus [`Key`] to the canonical key name the pure `pan_key_delta`
/// helper matches on. `None` for keys that aren't pan keys.
fn pan_key_name(key: &Key) -> Option<&'static str> {
    match key {
        Key::ArrowLeft => Some("ArrowLeft"),
        Key::ArrowRight => Some("ArrowRight"),
        Key::ArrowUp => Some("ArrowUp"),
        Key::ArrowDown => Some("ArrowDown"),
        Key::PageUp => Some("PageUp"),
        Key::PageDown => Some("PageDown"),
        _ => None,
    }
}

/// Per-tile drag accumulator for pointer panning. Deltas accumulate here and are
/// flushed to the zoom signal at most once per animation frame.
#[derive(Default)]
struct ScreenPanDrag {
    active: bool,
    last: Option<(f64, f64)>,
    pending_dx: f64,
    pending_dy: f64,
    raf_scheduled: bool,
}

/// End an in-progress pointer pan (pointerup / leave / cancel): clear the drag
/// and release pointer capture on the viewport.
fn end_screen_pan(drag: &Rc<RefCell<ScreenPanDrag>>, viewport_id: &str, evt: &PointerEvent) {
    let was_active = {
        let mut d = drag.borrow_mut();
        let a = d.active;
        d.active = false;
        d.last = None;
        a
    };
    if was_active {
        if let Some(web_evt) = evt.try_as_web_event() {
            if let Some(el) = window()
                .and_then(|w| w.document())
                .and_then(|doc| doc.get_element_by_id(viewport_id))
            {
                let _ = el.release_pointer_capture(web_evt.pointer_id());
            }
        }
    }
}

/// Issue 1175: the zoom/pan viewport for a RECEIVED shared-content tile. Wraps
/// the SAME decoder `<canvas>` (via [`ScreenCanvas`]) in a `.ss-zoom-wrapper`
/// whose CSS `transform` is driven declaratively from [`ScreenZoomCtx`], so a
/// zoom/pan change only patches an attribute and never recreates the canvas the
/// decoder paints into. The viewport is a focusable group; arrow / page / Home /
/// End keys and drag pan it when zoomed (no-op at fit, so keys aren't trapped).
#[component]
fn ScreenShareZoomable(peer_id: String) -> Element {
    let zoom_ctx = use_context::<ScreenZoomCtx>().0;
    let viewport_id = format!("screen-share-{}-viewport", peer_id);

    // Declarative transform from current state (subscribes this tile to zoom).
    let zoom_state = read_zoom_state(&zoom_ctx, &peer_id);
    let transform = screen_share_zoom::transform_css(&zoom_state);
    // Issue 1175 (item 6): only promote to a GPU layer (`will-change`) and show
    // the grab cursor while actually zoomed; both are released at fit via this
    // class so an idle tile carries no blanket compositor promotion.
    let viewport_class = if screen_share_zoom::is_zoomed(zoom_state.scale) {
        "ss-zoom-viewport is-zoomed"
    } else {
        "ss-zoom-viewport"
    };

    // Persistent drag accumulator + one reusable rAF closure. Panning writes the
    // signal at most once per animation frame (not at raw input rate), so a fast
    // drag re-renders this single tile ~once/frame — the canvas node is retained,
    // so each re-render only patches the wrapper's `transform`.
    let drag = use_hook(|| Rc::new(RefCell::new(ScreenPanDrag::default())));
    let raf: Rc<Closure<dyn FnMut()>> = use_hook({
        let drag = drag.clone();
        let peer = peer_id.clone();
        let vp = viewport_id.clone();
        let mut ctx = zoom_ctx;
        move || {
            Rc::new(Closure::<dyn FnMut()>::new(move || {
                let (dx, dy) = {
                    let mut d = drag.borrow_mut();
                    d.raf_scheduled = false;
                    let v = (d.pending_dx, d.pending_dy);
                    d.pending_dx = 0.0;
                    d.pending_dy = 0.0;
                    v
                };
                if dx == 0.0 && dy == 0.0 {
                    return;
                }
                if let Some((hw, hh)) = viewport_half_dims(&vp) {
                    let next =
                        screen_share_zoom::pan_by(read_zoom_state(&ctx, &peer), dx, dy, hw, hh);
                    write_zoom_state(&mut ctx, &peer, next);
                }
            }))
        }
    });

    // Clear the drag accumulator on unmount so a late rAF flush is a no-op.
    {
        let drag = drag.clone();
        use_drop(move || {
            let mut d = drag.borrow_mut();
            d.active = false;
            d.pending_dx = 0.0;
            d.pending_dy = 0.0;
        });
    }

    let on_down = {
        let drag = drag.clone();
        let ctx = zoom_ctx;
        let peer = peer_id.clone();
        let vp = viewport_id.clone();
        move |evt: PointerEvent| {
            // Nothing to pan at fit — leave the event alone.
            if !screen_share_zoom::is_zoomed(read_zoom_state(&ctx, &peer).scale) {
                return;
            }
            if let Some(web_evt) = evt.try_as_web_event() {
                if let Some(el) = window()
                    .and_then(|w| w.document())
                    .and_then(|doc| doc.get_element_by_id(&vp))
                {
                    let _ = el.set_pointer_capture(web_evt.pointer_id());
                }
            }
            let c = evt.element_coordinates();
            let mut d = drag.borrow_mut();
            d.active = true;
            d.last = Some((c.x, c.y));
        }
    };

    let on_move = {
        let drag = drag.clone();
        let raf = raf.clone();
        move |evt: PointerEvent| {
            let c = evt.element_coordinates();
            let schedule = {
                let mut d = drag.borrow_mut();
                if !d.active {
                    return;
                }
                let (lx, ly) = d.last.unwrap_or((c.x, c.y));
                d.pending_dx += c.x - lx;
                d.pending_dy += c.y - ly;
                d.last = Some((c.x, c.y));
                if d.raf_scheduled {
                    false
                } else {
                    d.raf_scheduled = true;
                    true
                }
            };
            if schedule {
                if let Some(win) = window() {
                    let cb: &js_sys::Function = (*raf).as_ref().unchecked_ref();
                    let _ = win.request_animation_frame(cb);
                }
            }
        }
    };

    let on_key = {
        let mut ctx = zoom_ctx;
        let peer = peer_id.clone();
        let vp = viewport_id.clone();
        move |evt: KeyboardEvent| {
            let cur = read_zoom_state(&ctx, &peer);
            // Don't trap keys when there's nothing to pan.
            if !screen_share_zoom::is_zoomed(cur.scale) {
                return;
            }
            let (hw, hh) = match viewport_half_dims(&vp) {
                Some(v) => v,
                None => return,
            };
            let next = match evt.key() {
                // Home / End jump to the top-left / bottom-right extents (they
                // need the max offset the pure delta helper can't know).
                Key::Home => Some(ScreenZoomState {
                    scale: cur.scale,
                    off_x: screen_share_zoom::max_pan_offset(cur.scale, hw),
                    off_y: screen_share_zoom::max_pan_offset(cur.scale, hh),
                }),
                Key::End => Some(ScreenZoomState {
                    scale: cur.scale,
                    off_x: -screen_share_zoom::max_pan_offset(cur.scale, hw),
                    off_y: -screen_share_zoom::max_pan_offset(cur.scale, hh),
                }),
                other => pan_key_name(&other)
                    .and_then(screen_share_zoom::pan_key_delta)
                    .map(|(dx, dy)| screen_share_zoom::pan_by(cur, dx, dy, hw, hh)),
            };
            if let Some(next) = next {
                evt.prevent_default();
                write_zoom_state(&mut ctx, &peer, next);
            }
        }
    };

    let on_up = {
        let drag = drag.clone();
        let vp = viewport_id.clone();
        move |e: PointerEvent| end_screen_pan(&drag, &vp, &e)
    };
    let on_leave = {
        let drag = drag.clone();
        let vp = viewport_id.clone();
        move |e: PointerEvent| end_screen_pan(&drag, &vp, &e)
    };
    let on_cancel = {
        let drag = drag.clone();
        let vp = viewport_id.clone();
        move |e: PointerEvent| end_screen_pan(&drag, &vp, &e)
    };

    rsx! {
        div {
            id: "{viewport_id}",
            class: "{viewport_class}",
            "data-testid": "ss-zoom-viewport",
            tabindex: "0",
            role: "group",
            "aria-label": "Shared content. Zoom with the controls, then drag or use the arrow keys to pan.",
            onpointerdown: on_down,
            onpointermove: on_move,
            onpointerup: on_up,
            onpointerleave: on_leave,
            onpointercancel: on_cancel,
            onkeydown: on_key,
            div {
                class: "ss-zoom-wrapper",
                style: "transform: {transform};",
                ScreenCanvas { peer_id: peer_id.clone() }
            }
        }
    }
}

/// Issue 1175: zoom / reset / detach controls for a RECEIVED shared-content
/// tile. Always-present markup (its shape never changes with zoom/detach state)
/// so re-renders never tear down the canvas. Every handler is an ordinary
/// main-document Dioxus handler, so they are always live — unlike v1's dead
/// in-PiP delegated handlers. The detach button is omitted where no separate
/// window is available (see `screen_share_detach::detach_supported`).
#[component]
fn ScreenShareZoomControls(peer_id: String, name: String) -> Element {
    let zoom_ctx = use_context::<ScreenZoomCtx>().0;
    let detached_ctx = use_context::<DetachedShareCtx>().0;
    let viewport_id = format!("screen-share-{}-viewport", peer_id);

    // Issue 1175 (item 4): read ONLY the scale, through a memo, so an offset-only
    // pan write (scale + offsets share the one `ScreenZoomCtx` map) does NOT
    // re-render this controls bar every animation frame. The memo re-runs on any
    // map change but its `f64` output is unchanged on a pan, so it doesn't notify
    // subscribers; zoom-in/out/reset (which change scale) still update the bar.
    let scale_memo = use_memo({
        let peer = peer_id.clone();
        move || read_zoom_state(&zoom_ctx, &peer).scale
    });
    let scale = scale_memo();
    let label = screen_share_zoom::zoom_percent_label(scale);
    let at_min = screen_share_zoom::at_min_zoom(scale);
    let at_max = screen_share_zoom::at_max_zoom(scale);
    let is_detached = detached_ctx.read().as_deref() == Some(peer_id.as_str());

    // Stable, peer-scoped ids so focus management can find the detach toggle and
    // the overlay's "Bring it back" button (which lives in `generate_for_peer`,
    // a sibling subtree of this component) across the mode change.
    let detach_btn_dom_id = format!("screen-share-{}-detach-btn", peer_id);

    #[cfg(target_arch = "wasm32")]
    let can_detach = crate::components::screen_share_detach::detach_supported();
    #[cfg(not(target_arch = "wasm32"))]
    let can_detach = false;

    // A11y (blocker class from PR #1756): keep focus WITH the detach/reattach
    // mode change instead of letting it drop to <body>. Traces BOTH halves:
    //   * ENTER (not-detached → detached): the whole share pane (incl. the detach
    //     toggle) is hidden off-screen, so there is no in-pane control to focus.
    //     Focus the meeting grid landmark (`#grid-container`, tabindex=-1) and let
    //     the OS focus the newly-opened window; reattach affordances live there.
    //   * EXIT  (detached → not-detached), covering the detached window's Reattach
    //     button, Escape, and closing the window: the pane reappears, so focus
    //     returns to the detach toggle.
    // A prev-state cell ensures mount doesn't steal focus and only real
    // transitions act. Presenter-stops-while-detached unmounts this tile (no
    // toggle to focus) and is handled in `use_drop` below.
    {
        let detach_target = detach_btn_dom_id.clone();
        let peer_fx = peer_id.clone();
        let detached_fx = detached_ctx;
        let prev = use_hook(|| Rc::new(std::cell::Cell::new(false)));
        use_effect(move || {
            let now = detached_fx.read().as_deref() == Some(peer_fx.as_str());
            if now != prev.get() {
                prev.set(now);
                if now {
                    focus_element_by_id("grid-container");
                } else {
                    focus_element_by_id(&detach_target);
                }
            }
        });
    }

    // If this shared-content tile unmounts while detached (presenter stops
    // sharing, receiver reconnects, meeting ends), close the detached window so
    // it can't linger showing a now-frozen mirror, and move focus to the meeting
    // grid — the detach toggle that would otherwise receive it is gone with the
    // tile, so without this focus would drop to <body>. `teardown` is a no-op
    // when this peer isn't the detached one.
    #[cfg(target_arch = "wasm32")]
    {
        let peer_drop = peer_id.clone();
        let detached_drop = detached_ctx;
        use_drop(move || {
            let was_detached = detached_drop.peek().as_deref() == Some(peer_drop.as_str());
            crate::components::screen_share_detach::teardown(&peer_drop);
            if was_detached {
                focus_element_by_id("grid-container");
            }
        });
    }

    let on_zoom_out = {
        let vp = viewport_id.clone();
        let peer = peer_id.clone();
        let mut ctx = zoom_ctx;
        move |e: MouseEvent| {
            e.stop_propagation();
            let (hw, hh) = viewport_half_dims(&vp).unwrap_or((0.0, 0.0));
            let cur = read_zoom_state(&ctx, &peer);
            let next =
                screen_share_zoom::zoom_to(cur, screen_share_zoom::zoom_out(cur.scale), hw, hh);
            write_zoom_state(&mut ctx, &peer, next);
        }
    };
    let on_zoom_in = {
        let vp = viewport_id.clone();
        let peer = peer_id.clone();
        let mut ctx = zoom_ctx;
        move |e: MouseEvent| {
            e.stop_propagation();
            let (hw, hh) = viewport_half_dims(&vp).unwrap_or((0.0, 0.0));
            let cur = read_zoom_state(&ctx, &peer);
            let next =
                screen_share_zoom::zoom_to(cur, screen_share_zoom::zoom_in(cur.scale), hw, hh);
            write_zoom_state(&mut ctx, &peer, next);
        }
    };
    let on_reset = {
        let vp = viewport_id.clone();
        let peer = peer_id.clone();
        let mut ctx = zoom_ctx;
        move |e: MouseEvent| {
            e.stop_propagation();
            let (hw, hh) = viewport_half_dims(&vp).unwrap_or((0.0, 0.0));
            let cur = read_zoom_state(&ctx, &peer);
            let next = screen_share_zoom::zoom_to(cur, screen_share_zoom::RESET_ZOOM, hw, hh);
            write_zoom_state(&mut ctx, &peer, next);
        }
    };
    let on_detach = {
        let peer = peer_id.clone();
        let name = name.clone();
        move |e: MouseEvent| {
            e.stop_propagation();
            #[cfg(target_arch = "wasm32")]
            {
                use crate::components::screen_share_detach as ssd;
                let mut dctx = detached_ctx;
                if dctx.read().as_deref() == Some(peer.as_str()) {
                    // Already detached → reattach (teardown flips the signal).
                    ssd::reattach(&peer);
                } else {
                    // Optimistically mark detached, then open. Every failure /
                    // close path calls the callback below to reset the signal.
                    dctx.set(Some(peer.clone()));
                    let dctx_cb = detached_ctx;
                    ssd::open(
                        &peer,
                        &name,
                        Box::new(move || {
                            // `Signal` is `Copy`, so copy into a local to satisfy
                            // the `Fn` callback (`set` needs `&mut self`).
                            let mut d = dctx_cb;
                            d.set(None);
                        }),
                    );
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                let _ = (&peer, &name, &detached_ctx);
            }
        }
    };

    let out_class = if at_min {
        "ss-zoom-btn ss-zoom-btn--disabled"
    } else {
        "ss-zoom-btn"
    };
    let in_class = if at_max {
        "ss-zoom-btn ss-zoom-btn--disabled"
    } else {
        "ss-zoom-btn"
    };

    rsx! {
        div { class: "ss-zoom-controls", "data-testid": "ss-zoom-controls",
            // aria-disabled (not the native `disabled`) at the clamps: the pure
            // step helpers already saturate, so a click at the limit is a
            // harmless no-op — and keeping the button focusable means a keyboard
            // user isn't dumped to <body> when they reach min/max.
            button {
                r#type: "button",
                class: "{out_class}",
                "data-testid": "ss-zoom-out",
                title: "Zoom out",
                "aria-label": "Zoom out shared content",
                "aria-disabled": if at_min { "true" } else { "false" },
                onclick: on_zoom_out,
                ZoomOutIcon {}
            }
            span {
                class: "ss-zoom-label",
                "data-testid": "ss-zoom-label",
                role: "status",
                "aria-live": "polite",
                "{label}"
            }
            button {
                r#type: "button",
                class: "{in_class}",
                "data-testid": "ss-zoom-in",
                title: "Zoom in",
                "aria-label": "Zoom in shared content",
                "aria-disabled": if at_max { "true" } else { "false" },
                onclick: on_zoom_in,
                ZoomInIcon {}
            }
            span { class: "ss-zoom-sep", "aria-hidden": "true" }
            button {
                r#type: "button",
                class: "ss-zoom-btn",
                "data-testid": "ss-zoom-reset",
                title: "Reset zoom to 100%",
                "aria-label": "Reset shared content zoom to 100 percent",
                onclick: on_reset,
                ZoomResetIcon {}
            }
            if can_detach {
                button {
                    r#type: "button",
                    id: "{detach_btn_dom_id}",
                    class: "ss-zoom-btn ss-detach-btn",
                    "data-testid": "ss-detach",
                    title: if is_detached { "Return shared content to the meeting window" } else { "Open shared content in a separate window" },
                    "aria-label": if is_detached { "Return shared content to the meeting window" } else { "Open shared content in a separate window" },
                    "aria-pressed": if is_detached { "true" } else { "false" },
                    onclick: on_detach,
                    DetachIcon {}
                }
            }
        }
    }
}

/// Issue 1175: a visually-hidden polite live region that announces detach /
/// reattach to screen-reader users. Rendered ONCE at the meeting level (by
/// `AttendantsComponent`), OUTSIDE the share pane that gets hidden off-screen
/// while detached, so it stays in the a11y tree and is read. Announces on REAL
/// transitions only (a prev-state cell): focus-land alone under-announces (on
/// ENTER the OS focus moves to the new window; on EXIT the detach toggle's label
/// describes its function, not the outcome). One detached share at a time, so it
/// keys off `DetachedShareCtx` being Some vs None, not a specific peer.
#[component]
pub fn ScreenDetachAnnouncer() -> Element {
    let detached_ctx = use_context::<DetachedShareCtx>().0;
    let mut message = use_signal(String::new);
    let prev = use_hook(|| Rc::new(std::cell::Cell::new(false)));
    use_effect(move || {
        let now = detached_ctx.read().is_some();
        if now != prev.get() {
            prev.set(now);
            message.set(
                if now {
                    "Shared content opened in a separate window"
                } else {
                    "Shared content returned to the meeting"
                }
                .to_string(),
            );
        }
    });

    rsx! {
        div {
            class: "visually-hidden",
            "data-testid": "ss-detach-announce",
            role: "status",
            "aria-live": "polite",
            "{message}"
        }
    }
}

fn toggle_pinned_div(div_id: &str) {
    if let Some(div) = window()
        .and_then(|w| w.document())
        .and_then(|doc| doc.get_element_by_id(div_id))
    {
        if !div.class_list().contains("grid-item-pinned") {
            div.class_list().add_1("grid-item-pinned").unwrap();
        } else {
            div.class_list().remove_1("grid-item-pinned").unwrap();
        }
    }
}

fn is_mobile_viewport() -> bool {
    if let Some(win) = window() {
        if let Ok(width) = win.inner_width() {
            let px = width.as_f64().unwrap_or(1024.0);
            return px < 768.0;
        }
    }
    false
}

fn toggle_canvas_crop(canvas_id: &str, cropped_tiles: Option<Signal<HashMap<String, bool>>>) {
    if let Some(mut ct) = cropped_tiles {
        ct.with_mut(|map| {
            let entry = map.entry(canvas_id.to_string()).or_insert(false);
            *entry = !*entry;
        });
    }
}

/// Returns whether the given canvas is currently in letterboxed (uncropped) mode.
/// When `true`, the canvas preserves its native aspect ratio with black bars;
/// when `false`, the canvas is filled/cropped to cover the tile.
fn is_canvas_letterboxed(
    canvas_id: &str,
    cropped_tiles: &Option<Signal<HashMap<String, bool>>>,
) -> bool {
    cropped_tiles
        .as_ref()
        .and_then(|ct| ct.read().get(canvas_id).copied())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Unit tests — split-layout decision logic
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    // -- ScreenOnly: remote peer IS screen-sharing → render screen share ------
    #[test]
    fn screen_only_remote_sharing_renders_screen() {
        assert_eq!(
            split_layout_decision(&TileMode::ScreenOnly, true, false),
            TileDecision::RenderScreenShare,
        );
    }

    // -- ScreenOnly: remote peer is NOT screen-sharing → empty ----------------
    #[test]
    fn screen_only_remote_not_sharing_returns_empty() {
        assert_eq!(
            split_layout_decision(&TileMode::ScreenOnly, false, false),
            TileDecision::Empty,
        );
    }

    // -- ScreenOnly: local (self) peer IS screen-sharing → empty (never show
    //    own screen share in the split panel) ---------------------------------
    #[test]
    fn screen_only_self_peer_sharing_returns_empty() {
        assert_eq!(
            split_layout_decision(&TileMode::ScreenOnly, true, true),
            TileDecision::Empty,
        );
    }

    // -- ScreenOnly: local peer, not sharing → empty --------------------------
    #[test]
    fn screen_only_self_peer_not_sharing_returns_empty() {
        assert_eq!(
            split_layout_decision(&TileMode::ScreenOnly, false, true),
            TileDecision::Empty,
        );
    }

    // -- VideoOnly: always renders the peer video tile ------------------------
    #[test]
    fn video_only_renders_video() {
        assert_eq!(
            split_layout_decision(&TileMode::VideoOnly, false, false),
            TileDecision::RenderVideo,
        );
    }

    #[test]
    fn video_only_self_peer_renders_video() {
        assert_eq!(
            split_layout_decision(&TileMode::VideoOnly, false, true),
            TileDecision::RenderVideo,
        );
    }

    #[test]
    fn video_only_with_screen_share_renders_video() {
        assert_eq!(
            split_layout_decision(&TileMode::VideoOnly, true, false),
            TileDecision::RenderVideo,
        );
    }

    #[test]
    fn video_only_self_peer_with_screen_share_renders_video() {
        assert_eq!(
            split_layout_decision(&TileMode::VideoOnly, true, true),
            TileDecision::RenderVideo,
        );
    }

    // -- Full: always falls through to the grid / full-bleed paths ------------
    #[test]
    fn full_mode_falls_through() {
        assert_eq!(
            split_layout_decision(&TileMode::Full, false, false),
            TileDecision::FallThrough,
        );
    }

    #[test]
    fn full_mode_with_screen_share_falls_through() {
        assert_eq!(
            split_layout_decision(&TileMode::Full, true, false),
            TileDecision::FallThrough,
        );
    }

    #[test]
    fn full_mode_self_peer_falls_through() {
        assert_eq!(
            split_layout_decision(&TileMode::Full, false, true),
            TileDecision::FallThrough,
        );
    }

    // -- TileMode Default trait -----------------------------------------------
    #[test]
    fn tile_mode_default_is_full() {
        assert_eq!(TileMode::default(), TileMode::Full);
    }

    // -- is_speaking_suppressed -----------------------------------------------

    /// No peer is pinned → glow is never suppressed.
    #[test]
    fn suppressed_no_pin_returns_false() {
        assert!(!is_speaking_suppressed(false, None));
    }

    /// The pinned peer itself → glow is NOT suppressed.
    #[test]
    fn suppressed_pinned_peer_returns_false() {
        assert!(!is_speaking_suppressed(true, Some("alice")));
    }

    /// A non-pinned peer while another peer is pinned → glow IS suppressed.
    #[test]
    fn suppressed_non_pinned_while_pin_active_returns_true() {
        assert!(is_speaking_suppressed(false, Some("alice")));
    }

    #[test]
    fn speak_style_reset_restores_default_border_color() {
        let style = speak_style(0.0, false, &AppearanceSettings::default());

        assert!(style.contains("box-shadow: none;"));
        assert!(style.contains(DEFAULT_TILE_BORDER_COLOR));
    }

    #[test]
    fn glow_decay_zero_is_instant_on_and_off() {
        let settings = AppearanceSettings {
            glow_decay: 0.0,
            ..AppearanceSettings::default()
        };

        let on = speak_style(0.8, true, &settings);
        let off = speak_style(0.0, false, &settings);

        assert!(on.contains("border-color 0.00s ease-in"));
        assert!(on.contains("box-shadow 0.00s ease-in"));
        assert!(off.contains("border-color 0.3s ease-out"));
        assert!(off.contains("box-shadow 0.00s ease-out"));
    }

    #[test]
    fn glow_decay_midpoint_preserves_border_reset_timing() {
        let settings = AppearanceSettings {
            glow_decay: 0.5,
            ..AppearanceSettings::default()
        };
        let on = speak_style(0.8, true, &settings);
        let off = speak_style(0.0, false, &settings);

        assert!(on.contains("border-color 0.15s ease-in"));
        assert!(on.contains("box-shadow 0.15s ease-in"));
        assert!(off.contains("border-color 0.3s ease-out"));
        assert!(off.contains("box-shadow 1.50s ease-out"));
    }

    #[test]
    fn glow_decay_full_extends_fade_out_tail() {
        let settings = AppearanceSettings {
            glow_decay: 1.0,
            ..AppearanceSettings::default()
        };
        let off = speak_style(0.0, false, &settings);

        assert!(off.contains("border-color 0.3s ease-out"));
        assert!(off.contains("box-shadow 3.00s ease-out"));
    }

    #[test]
    fn glow_brightness_changes_intensity_not_geometry() {
        let low = calculate_glow_params(0.65, 0.0, 0.5);
        let high = calculate_glow_params(0.65, 1.0, 0.5);

        assert_eq!(low.outer_blur, high.outer_blur);
        assert_eq!(low.outer_spread, high.outer_spread);
        assert_eq!(low.inner_blur, high.inner_blur);
        assert!(high.outer_alpha > low.outer_alpha);
        assert!(high.inner_alpha > low.inner_alpha);
        assert!(high.border_alpha > low.border_alpha);
    }

    #[test]
    fn glow_slider_scales_bleed_from_subtle_to_exaggerated() {
        let subtle = calculate_glow_params(0.65, 0.5, 0.0);
        let balanced = calculate_glow_params(0.65, 0.5, 0.5);
        let strong = calculate_glow_params(0.65, 0.5, 1.0);

        assert!(subtle.outer_blur < balanced.outer_blur);
        assert!(balanced.outer_blur < strong.outer_blur);
        assert!(subtle.outer_spread < balanced.outer_spread);
        assert!(balanced.outer_spread < strong.outer_spread);
        assert!(subtle.inner_blur < balanced.inner_blur);
        assert!(balanced.inner_blur < strong.inner_blur);
    }

    #[test]
    fn glow_midpoint_is_much_stronger_than_zero_point_bleed() {
        let subtle = calculate_glow_params(1.0, 0.5, 0.0);
        let balanced = calculate_glow_params(1.0, 0.5, 0.5);

        let subtle_delta = subtle.outer_blur - OUTER_BLUR_BASE;
        let balanced_delta = balanced.outer_blur - OUTER_BLUR_BASE;
        assert!(balanced_delta > subtle_delta * 1.8);
    }

    #[test]
    fn glow_zero_disables_shadow_but_keeps_colored_border() {
        let settings = AppearanceSettings {
            glow_enabled: true,
            glow_brightness: 1.0,
            inner_glow_strength: 0.0,
            ..AppearanceSettings::default()
        };
        let style = speak_style(0.7, true, &settings);

        assert!(style.contains("box-shadow: none;"));
        // @token-exempt: tests the presence of a dynamic rgba() token, not a color literal
        assert!(style.contains("border-color: rgba("));
    }

    // -- Crop state: HashMap toggle/lookup logic ---------------------------------

    #[test]
    fn crop_toggle_roundtrip() {
        let mut map = HashMap::<String, bool>::new();
        let id = "peer-abc";

        // Initially not letterboxed (fill/cropped is the default)
        assert!(!map.get(id).copied().unwrap_or(false));

        // First toggle → letterboxed (uncropped, preserves aspect ratio)
        let entry = map.entry(id.to_string()).or_insert(false);
        *entry = !*entry;
        assert!(map.get(id).copied().unwrap_or(false));

        // Second toggle → back to fill/cropped
        let entry = map.entry(id.to_string()).or_insert(false);
        *entry = !*entry;
        assert!(!map.get(id).copied().unwrap_or(false));
    }

    #[test]
    fn crop_cleanup_on_peer_removal() {
        let mut map = HashMap::<String, bool>::new();
        let peer_id = "session-123";

        // Set crop state for both video and screen-share canvases. The
        // screen-share key is built via the production single-source-of-truth
        // getter (issue 1175), so this test tracks the real id format instead of
        // re-hardcoding the literal.
        map.insert(peer_id.to_string(), true);
        map.insert(screen_share_zoom::screen_canvas_id(peer_id), true);
        assert_eq!(map.len(), 2);

        // Simulate on_peer_removed cleanup (same getter the production path uses).
        map.remove(peer_id);
        map.remove(&screen_share_zoom::screen_canvas_id(peer_id));
        assert!(map.is_empty());
    }

    #[test]
    fn crop_missing_id_returns_false() {
        let map = HashMap::<String, bool>::new();
        assert!(!map.get("nonexistent").copied().unwrap_or(false));
    }

    #[test]
    fn crop_none_context_returns_false() {
        let ct: Option<&HashMap<String, bool>> = None;
        let result = ct.and_then(|m| m.get("any-id").copied()).unwrap_or(false);
        assert!(!result);
    }

    // -- Issue #1483: transport badge string → enum mapping -------------------
    //
    // `transport_badge_from_str` is a pure host-testable map (no app_config /
    // DOM / signals), so these run on the host like the split-layout tests
    // above. The assertions are mutation-sensitive: each known input is pinned
    // to its enum AND asserted NOT to equal the other transport, so swapping
    // the `"webtransport" => Wt` / `"websocket" => Ws` arms would fail here.

    #[test]
    fn transport_badge_webtransport_maps_to_wt() {
        assert_eq!(transport_badge_from_str("webtransport"), TransportBadge::Wt);
        // Mutation guard: if the WT arm were swapped to Ws this fails.
        assert_ne!(transport_badge_from_str("webtransport"), TransportBadge::Ws);
    }

    #[test]
    fn transport_badge_websocket_maps_to_ws() {
        assert_eq!(transport_badge_from_str("websocket"), TransportBadge::Ws);
        // Mutation guard: if the WS arm were swapped to Wt this fails.
        assert_ne!(transport_badge_from_str("websocket"), TransportBadge::Wt);
    }

    #[test]
    fn transport_badge_unknown_literal_maps_to_unknown() {
        assert_eq!(transport_badge_from_str("unknown"), TransportBadge::Unknown);
    }

    #[test]
    fn transport_badge_empty_maps_to_unknown() {
        assert_eq!(transport_badge_from_str(""), TransportBadge::Unknown);
    }

    #[test]
    fn transport_badge_junk_maps_to_unknown() {
        assert_eq!(
            transport_badge_from_str("quic-but-not-really"),
            TransportBadge::Unknown
        );
        // Case sensitivity: the diagnostics metric emits lowercase, so a
        // mixed-case value is NOT a known transport.
        assert_eq!(
            transport_badge_from_str("WebTransport"),
            TransportBadge::Unknown
        );
    }

    #[test]
    fn transport_badge_wt_and_ws_are_distinct() {
        // The two transports must render distinctly; a single-variant collapse
        // (both → same) would defeat the whole feature.
        assert_ne!(TransportBadge::Wt, TransportBadge::Ws);
    }
}

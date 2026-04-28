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
use crate::components::signal_quality::{SignalInfo, SignalQualityPopup};
use crate::constants::users_allowed_to_stream;
use crate::context::{AppearanceSettings, VideoCallClientCtx};
use dioxus::prelude::*;
use std::rc::Rc;
use videocall_client::VideoCallClient;
use wasm_bindgen::JsCast;
use web_sys::{window, HtmlCanvasElement};

// ─── Glow formula constants ───────────────────────────────────────────────────

/// Base outer blur radius in pixels at zero audio level.
const OUTER_BLUR_BASE: f32 = 14.0;
/// Outer blur radius contribution per unit of audio intensity.
const OUTER_BLUR_INTENSITY: f32 = 14.0;
/// Additional outer blur contributed by brightness² per unit of intensity.
const OUTER_BLUR_BRIGHTNESS: f32 = 10.0;

/// Base outer spread in pixels at zero audio level.
const OUTER_SPREAD_BASE: f32 = 1.0;
/// Outer spread contribution per unit of audio intensity.
const OUTER_SPREAD_INTENSITY: f32 = 2.0;
/// Additional outer spread contributed by brightness² per unit of intensity.
const OUTER_SPREAD_BRIGHTNESS: f32 = 4.0;

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

// ─── Shared glow parameter struct ────────────────────────────────────────────

/// Pre-computed glow parameters produced by [`calculate_glow_params`].
pub(crate) struct GlowParams {
    pub outer_blur: f32,
    pub outer_spread: f32,
    pub outer_alpha: f32,
    pub inner_blur: f32,
    pub inner_spread: f32,
    pub inner_alpha: f32,
    /// Border alpha is independent of brightness so the colored border stays
    /// clearly visible even when glow brightness is turned down to zero.
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
    let brightness_curve = b * b;
    let inner_curve = s * s;
    GlowParams {
        outer_blur: OUTER_BLUR_BASE
            + i * (OUTER_BLUR_INTENSITY + brightness_curve * OUTER_BLUR_BRIGHTNESS),
        outer_spread: OUTER_SPREAD_BASE
            + i * (OUTER_SPREAD_INTENSITY + brightness_curve * OUTER_SPREAD_BRIGHTNESS),
        outer_alpha: (OUTER_ALPHA_BASE + i * OUTER_ALPHA_INTENSITY) * brightness_curve,
        inner_blur: INNER_BLUR_BASE
            + i * (INNER_BLUR_INTENSITY + inner_curve * INNER_BLUR_STRENGTH),
        inner_spread: 0.0,
        inner_alpha: (INNER_ALPHA_BASE + i * INNER_ALPHA_INTENSITY)
            * brightness_curve
            * (INNER_ALPHA_STRENGTH_MIN + inner_curve * INNER_ALPHA_STRENGTH_RANGE),
        border_alpha: (BORDER_ALPHA_BASE + i * BORDER_ALPHA_INTENSITY).clamp(0.45, 0.92),
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
    if !settings.glow_enabled || !speaking_active || audio_level <= 0.0 {
        return format!(
            "box-shadow: none; border-color: {DEFAULT_TILE_BORDER_COLOR}; transition: border-color 0.3s ease-out, box-shadow 1.5s ease-out;"
        );
    }

    let (r, g, b) = settings.glow_color.to_rgb();
    let p = calculate_glow_params(
        audio_level,
        settings.glow_brightness,
        settings.inner_glow_strength,
    );
    format!(
        "box-shadow: 0 0 {:.0}px {:.0}px rgba({r}, {g}, {b}, {:.2}), \
             inset 0 0 {:.0}px {:.0}px rgba({r}, {g}, {b}, {:.2}); \
             border-color: rgba({r}, {g}, {b}, {:.2}); \
             transition: border-color 0.15s ease-in, box-shadow 0.15s ease-in;",
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
    if mic_audio_level <= 0.0 && glow_audio_level <= 0.0 {
        // Fully silent: fade out both color and filter
        return "color: inherit; filter: none; transition: color 5.0s ease-out, filter 1.5s ease-out;".to_string();
    }

    if !settings.glow_enabled {
        // Respect the global glow toggle for mic visuals too.
        return "color: inherit; filter: none; transition: color 5.0s ease-out, filter 1.5s ease-out;".to_string();
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
             transition: color 5.0s ease-out, filter 0.15s ease-in;",
            8.0 + glow_i * 16.0,
            (0.55 + glow_i * 0.45) * brightness_curve,
            3.0 + glow_i * 8.0,
            (0.60 + glow_i * 0.40) * brightness_curve,
        );
    }
    if mic_audio_level > 0.0 && glow_audio_level <= 0.0 {
        // Held color but raw glow has faded — no drop-shadow
        return format!("color: {icon_color}; filter: none; transition: color 0.05s ease-in, filter 1.5s ease-out;");
    }
    // Both positive: colored icon + scaled drop-shadow glow
    let clamped = glow_audio_level.clamp(0.0, 1.0);
    let glow_i = clamped.sqrt();
    format!(
        "color: {icon_color}; \
         filter: drop-shadow(0 0 {:.0}px rgba({r}, {g}, {b}, {:.2})) \
                 drop-shadow(0 0 {:.0}px rgba({r}, {g}, {b}, {:.2})); \
         transition: color 0.05s ease-in, filter 0.15s ease-in;",
        8.0 + glow_i * 16.0,
        (0.55 + glow_i * 0.45) * brightness_curve,
        3.0 + glow_i * 8.0,
        (0.60 + glow_i * 0.40) * brightness_curve,
    )
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

/// Audio level pair passed to [`generate_for_peer`] so the two related
/// values travel as one argument (keeps the arg count at 7).
pub struct AudioLevels {
    /// Raw audio level (0.0–1.0) driving the border/glow intensity.
    pub raw: f32,
    /// Mic-held audio level (held 1 s after silence) driving the icon color.
    pub mic: f32,
}

/// Render a single peer tile. If `full_bleed` is true and the peer is not screen sharing,
/// the video tile will occupy the full grid area. The `audio_levels.raw` parameter (0.0–1.0) drives
/// a glow whose intensity scales with voice volume.
/// If `host_user_id` matches the peer's authenticated user_id, a crown icon is displayed next to the name.
#[allow(clippy::too_many_arguments)]
pub fn generate_for_peer(
    client: &VideoCallClient,
    key: &String,
    full_bleed: bool,
    audio_levels: AudioLevels,
    host_user_id: Option<&str>,
    mode: TileMode,
    my_peer_id: Option<&str>,
    signal_info: SignalInfo,
    mut show_signal_popup: Signal<bool>,
    pinned_peer_id: Option<&str>,
    on_toggle_pin: EventHandler<String>,
    appearance: &AppearanceSettings,
) -> Element {
    let audio_level = audio_levels.raw;
    let mic_audio_level = audio_levels.mic;
    let signal_level = signal_info.level;
    let signal_history = signal_info.history;
    let meeting_start_ms = signal_info.meeting_start_ms;
    let peer_user_id = client.get_peer_user_id(key).unwrap_or_else(|| key.clone());
    let peer_display_name = client
        .get_peer_display_name(key)
        .unwrap_or_else(|| peer_user_id.clone());

    // Compare authenticated user_id (from JWT/DB) instead of user-chosen display name
    // to prevent spoofing the host crown icon.
    let is_host = host_user_id.map(|h| h == peer_user_id).unwrap_or(false);
    let is_guest = client.get_peer_is_guest(key).unwrap_or(false);
    let allowed = users_allowed_to_stream().unwrap_or_default();
    if !allowed.is_empty() && !allowed.contains(&peer_user_id) {
        return rsx! {};
    }

    let is_video_enabled_for_peer = client.is_video_enabled_for_peer(key);
    let is_audio_enabled_for_peer = client.is_audio_enabled_for_peer(key);
    let is_screen_share_enabled_for_peer = client.is_screen_share_enabled_for_peer(key);

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
    if matches!(mode, TileMode::ScreenOnly) {
        // Don't render the local user's own screen share
        if !is_screen_share_enabled_for_peer || my_peer_id == Some(peer_user_id.as_str()) {
            return rsx! {};
        }
    }

    // ---- Split-layout: early return for ScreenOnly / VideoOnly ----------------
    let is_self_peer = my_peer_id == Some(peer_user_id.as_str());
    let decision = split_layout_decision(&mode, is_screen_share_enabled_for_peer, is_self_peer);

    if decision == TileDecision::Empty {
        return rsx! {};
    }

    // ---- Split-layout: screen-share left panel --------------------------------
    if decision == TileDecision::RenderScreenShare {
        let ss_canvas_crop = format!("screen-share-{}", key);
        let ss_div_id = Rc::new(format!("screen-share-{}-div", &key));
        let ss_div_pin = (*ss_div_id).clone();
        let peer_user_id_for_pin_ss = peer_user_id.clone();
        let ss_name = format!("{}-screen", peer_display_name);
        let ss_name_title = ss_name.clone();
        return rsx! {
            div {
                id: "{ss_div_id}",
                class: "split-screen-tile",
                div {
                    class: "canvas-container video-on",
                    ScreenCanvas { peer_id: key.clone() }
                    h4 {
                        class: "floating-name",
                        title: "{ss_name_title}",
                        dir: "auto",
                        "{ss_name}"
                        if is_guest {
                            span { class: "guest-badge", "Guest" }
                        }
                    }
                    div {
                        class: "tile-top-icons",
                        button {
                            onclick: move |_| {
                                toggle_pinned_div(&ss_div_pin);
                                on_toggle_pin.call(peer_user_id_for_pin_ss.clone());
                            },
                            class: "pin-icon",
                            PushPinIcon {}
                        }
                        button {
                            onclick: move |_| toggle_canvas_crop(&ss_canvas_crop),
                            class: "crop-icon",
                            CropIcon {}
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
        let split_peer_class = if show_signal_popup() {
            "split-peer-tile signal-popup-open"
        } else {
            "split-peer-tile"
        };
        return rsx! {
            div {
                class: "{split_peer_class}{vo_speaking}",
                id: "{peer_video_div_id}",
                style: "{vo_tile_style}",
                div {
                    class: "{grid_class}",
                    onclick: move |_| {
                        if is_mobile_viewport() {
                            toggle_pinned_div(&div_id_mobile);
                        }
                    },
                    if is_video_enabled_for_peer {
                        UserVideo { id: key_clone.clone(), hidden: false }
                    } else {
                        div {
                            class: "placeholder-content",
                            PeerIcon {}
                            span { class: "placeholder-text", "Video Disabled" }
                        }
                    }
                    h4 {
                        class: "floating-name",
                        title: "{title_vo}",
                        dir: "auto",
                        "{peer_display_name_vo}"
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
                            class: "signal-indicator",
                            "aria-label": "Show signal quality",
                            onclick: move |_| show_signal_popup.toggle(),
                            SignalBarsIcon { level: signal_level.bars(), lost: signal_level.is_lost() }
                        }
                        // Pin (visible on hover only)
                        button {
                            onclick: move |_| {
                                toggle_pinned_div(&div_id_pin);
                                on_toggle_pin.call(peer_user_id_for_pin_vo.clone());
                            },
                            class: "pin-icon",
                            PushPinIcon {}
                        }
                        // Crop (visible on hover only)
                        button {
                            onclick: move |_| toggle_canvas_crop(&pv_canvas_crop),
                            class: "crop-icon",
                            CropIcon {}
                        }
                    }
                    if show_signal_popup() {
                        {
                            let h = signal_history.clone();
                            let popup_peer_id = key.clone();
                            let popup_peer_name = peer_display_name.clone();
                            rsx! {
                                SignalQualityPopup {
                                    peer_id: popup_peer_id,
                                    peer_name: popup_peer_name,
                                    history: h,
                                    meeting_start_ms,
                                    on_close: move |_| show_signal_popup.set(false),
                                }
                            }
                        }
                    }
                }
            }
        };
    }

    // Full-bleed single peer (no screen share)
    if full_bleed && !is_screen_share_enabled_for_peer {
        let peer_video_div_id = Rc::new(format!("peer-video-{}-div", &key));
        let div_id_mobile = (*peer_video_div_id).clone();
        let div_id_pin = (*peer_video_div_id).clone();
        let canvas_id_crop = key.clone();
        let key_clone = key.clone();
        let peer_display_name_fb = peer_display_name.clone();
        let peer_user_id_for_pin = peer_user_id.clone();
        let title = if is_host {
            format!("Host: {peer_user_id}")
        } else {
            peer_user_id.clone()
        };
        let full_bleed_class = if is_video_enabled_for_peer {
            "canvas-container video-on"
        } else {
            "canvas-container"
        };
        let full_bleed_grid_class = if show_signal_popup() {
            "grid-item full-bleed signal-popup-open"
        } else {
            "grid-item full-bleed"
        };
        return rsx! {
            div {
                class: "{full_bleed_grid_class}{speaking_class}",
                id: "{peer_video_div_id}",
                style: "{tile_style}",
                div {
                    class: "{full_bleed_class}",
                    onclick: move |_| {
                        if is_mobile_viewport() {
                            toggle_pinned_div(&div_id_mobile);
                        }
                    },
                    if is_video_enabled_for_peer {
                        UserVideo { id: key_clone.clone(), hidden: false }
                    } else {
                        div {
                            class: "",
                            div {
                                class: "placeholder-content",
                                PeerIcon {}
                                span { class: "placeholder-text", "Camera Off" }
                            }
                        }
                    }
                    h4 {
                        class: "floating-name",
                        title: "{title}",
                        dir: "auto",
                        "{peer_display_name_fb}"
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
                            style: "{mic_inline_style}",
                            MicIcon { muted: !is_audio_enabled_for_peer }
                        }
                        // Signal icon (always visible, clickable)
                        button {
                            class: "signal-indicator",
                            "aria-label": "Show signal quality",
                            onclick: move |_| show_signal_popup.toggle(),
                            SignalBarsIcon { level: signal_level.bars(), lost: signal_level.is_lost() }
                        }
                        // Pin and Crop (visible on hover only)
                        button {
                            onclick: move |_| {
                                toggle_pinned_div(&div_id_pin);
                                on_toggle_pin.call(peer_user_id_for_pin.clone());
                            },
                            class: "pin-icon",
                            PushPinIcon {}
                        }
                        button {
                            onclick: move |_| toggle_canvas_crop(&canvas_id_crop),
                            class: "crop-icon",
                            CropIcon {}
                        }
                    }
                    if show_signal_popup() {
                        {
                            let h = signal_history.clone();
                            let popup_peer_id = key.clone();
                            let popup_peer_name = peer_display_name.clone();
                            rsx! {
                                SignalQualityPopup {
                                    peer_id: popup_peer_id,
                                    peer_name: popup_peer_name,
                                    history: h,
                                    meeting_start_ms,
                                    on_close: move |_| show_signal_popup.set(false),
                                }
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
    let ss_canvas_crop = format!("screen-share-{}", key);
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
    let peer_id = peer_user_id.as_str();
    let my_peer_id = my_peer_id.unwrap_or("");

    rsx! {
        // Canvas for Screen share.
        if peer_id != my_peer_id && is_screen_share_enabled_for_peer {
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
                        "{ss_name}"
                        if is_guest {
                            span { class: "guest-badge", "Guest" }
                        }
                    }
                    button {
                        onclick: move |_| toggle_canvas_crop(&ss_canvas_crop),
                        class: "crop-icon",
                        CropIcon {}
                    }
                    button {
                        onclick: move |_| {
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
            let grid_class = if is_video_enabled_for_peer {
                "canvas-container video-on"
            } else {
                "canvas-container"
            };
            let grid_tile_style = tile_style.clone();
            let grid_mic_style = mic_inline_style.clone();
            let grid_speaking = speaking_class;
            let grid_item_class = if show_signal_popup() {
                "grid-item signal-popup-open"
            } else {
                "grid-item"
            };
            rsx! {
                div {
                    class: "{grid_item_class}{grid_speaking}",
                    id: "{peer_video_div_id}",
                    style: "{grid_tile_style}",
                    // One canvas for the User Video
                    div {
                        class: "{grid_class}",
                        onclick: move |_| {
                            if is_mobile_viewport() {
                                toggle_pinned_div(&pv_div_mobile);
                            }
                        },
                        if is_video_enabled_for_peer {
                            UserVideo { id: key_clone.clone(), hidden: false }
                        } else {
                            div { class: "placeholder-content",
                                PeerIcon {}
                                span { class: "placeholder-text", "Video Disabled" }
                            }
                        }
                        h4 {
                            class: "floating-name",
                            title: "{title_grid}",
                            dir: "auto",
                            "{peer_display_name_grid}"
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
                                class: "signal-indicator",
                                "aria-label": "Show signal quality",
                                onclick: move |_| show_signal_popup.toggle(),
                                SignalBarsIcon { level: signal_level.bars(), lost: signal_level.is_lost() }
                            }
                            // Pin and Crop (visible on hover only)
                            button {
                                onclick: move |_| {
                                    toggle_pinned_div(&pv_div_pin);
                                    on_toggle_pin.call(peer_user_id_for_pin.clone());
                                },
                                class: "pin-icon",
                                PushPinIcon {}
                            }
                            button {
                                onclick: move |_| toggle_canvas_crop(&pv_canvas_crop),
                                class: "crop-icon",
                                CropIcon {}
                            }
                        }
                        if show_signal_popup() {
                            {
                                let h = signal_history.clone();
                                let popup_peer_id = key.clone();
                                let popup_peer_name = peer_display_name.clone();
                                rsx! {
                                    SignalQualityPopup {
                                        peer_id: popup_peer_id,
                                        peer_name: popup_peer_name,
                                        history: h,
                                        meeting_start_ms,
                                        on_close: move |_| show_signal_popup.set(false),
                                    }
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
    let id_for_effect = id.clone();

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

    rsx! {
        canvas {
            id: "{id}",
            hidden: hidden,
            class: "uncropped",
        }
    }
}

#[component]
fn ScreenCanvas(peer_id: String) -> Element {
    let client = use_context::<VideoCallClientCtx>();
    let canvas_id = format!("screen-share-{}", peer_id);
    let canvas_id_for_effect = canvas_id.clone();
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

    rsx! {
        canvas {
            id: "{canvas_id}",
            class: "uncropped",
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

fn toggle_canvas_crop(canvas_id: &str) {
    if let Some(canvas) = window()
        .and_then(|w| w.document())
        .and_then(|doc| doc.get_element_by_id(canvas_id))
    {
        let class_list = canvas.class_list();
        let is_cropped = class_list.contains("cropped");
        if is_cropped {
            let _ = class_list.remove_1("cropped");
            let _ = class_list.add_1("uncropped");
        } else {
            let _ = class_list.remove_1("uncropped");
            let _ = class_list.add_1("cropped");
        }
    }
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
}

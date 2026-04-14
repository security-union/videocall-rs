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
use crate::context::VideoCallClientCtx;
use dioxus::prelude::*;
use std::rc::Rc;
use videocall_client::VideoCallClient;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{window, HtmlCanvasElement, IntersectionObserver, IntersectionObserverEntry};

/// Compute the inline CSS for the speaking glow on the outer tile container.
/// Border color is controlled via the `.speaking-tile` CSS class; this
/// function only emits `box-shadow` and `transition` values.
pub(crate) fn speak_style(audio_level: f32, speaking_active: bool) -> String {
    if !speaking_active || audio_level <= 0.0 {
        return "box-shadow: none; transition: border-color 0.3s ease-out, box-shadow 1.5s ease-out;".to_string();
    }

    let i = audio_level.clamp(0.0, 1.0);
    format!(
        "box-shadow: 0 0 {:.0}px {:.0}px rgba(91, 207, 159, {:.2}), \
         inset 0 0 {:.0}px 0 rgba(91, 207, 159, {:.2}); \
         transition: border-color 0.15s ease-in, box-shadow 0.15s ease-in;",
        16.0 + i * 15.0,
        1.5 + i * 3.0,
        0.24 + i * 0.20,
        13.0 + i * 13.0,
        0.14 + i * 0.12,
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
/// - `mic_audio_level` (held 1s after silence) controls the icon COLOR (mint)
/// - `glow_audio_level` (raw, same as border) controls the drop-shadow GLOW
///
/// This way the icon stays mint briefly after speech stops (via the held signal)
/// while the drop-shadow glow tracks the border glow exactly.
fn mic_style(mic_audio_level: f32, glow_audio_level: f32) -> String {
    if mic_audio_level <= 0.0 && glow_audio_level <= 0.0 {
        // Fully silent: fade out both color and filter
        return "color: inherit; filter: none; transition: color 5.0s ease-out, filter 1.5s ease-out;".to_string();
    }
    // Unreachable in practice: the mic hold timer guarantees mic_audio_level
    // stays positive at least as long as glow_audio_level. Handle defensively
    // by showing only the glow without the mint icon color.
    if mic_audio_level <= 0.0 && glow_audio_level > 0.0 {
        let clamped = glow_audio_level.clamp(0.0, 1.0);
        let glow_i = clamped.sqrt();
        return format!(
            "color: inherit; \
             filter: drop-shadow(0 0 {:.0}px rgba(91, 207, 159, {:.2})) \
                     drop-shadow(0 0 {:.0}px rgba(91, 207, 159, {:.2})); \
             transition: color 5.0s ease-out, filter 0.15s ease-in;",
            8.0 + glow_i * 16.0,
            0.7 + glow_i * 0.3,
            3.0 + glow_i * 8.0,
            0.8 + glow_i * 0.2,
        );
    }
    if mic_audio_level > 0.0 && glow_audio_level <= 0.0 {
        // Held color (still mint) but raw glow has faded — no drop-shadow
        return "color: #5bcf9f; filter: none; transition: color 0.05s ease-in, filter 1.5s ease-out;".to_string();
    }
    // Both positive: mint color + scaled drop-shadow glow
    let clamped = glow_audio_level.clamp(0.0, 1.0);
    let glow_i = clamped.sqrt();
    format!(
        "color: #5bcf9f; \
         filter: drop-shadow(0 0 {:.0}px rgba(91, 207, 159, {:.2})) \
                 drop-shadow(0 0 {:.0}px rgba(91, 207, 159, {:.2})); \
         transition: color 0.05s ease-in, filter 0.15s ease-in;",
        8.0 + glow_i * 16.0, // primary drop-shadow blur: 8–24px
        0.7 + glow_i * 0.3,  // primary drop-shadow alpha: 0.7–1.0
        3.0 + glow_i * 8.0,  // secondary (tight) glow blur: 3–11px
        0.8 + glow_i * 0.2,  // secondary glow alpha: 0.8–1.0
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

    let is_suppressed =
        is_speaking_suppressed(is_pinned, pinned_peer_id) || is_screen_share_enabled_for_peer;

    let visible_audio_level = if is_suppressed { 0.0 } else { audio_level };
    let visible_mic_level = if is_suppressed { 0.0 } else { mic_audio_level };

    let is_speaking = visible_mic_level > 0.0;
    let speaking_class = if is_speaking { " speaking-tile" } else { "" };

    let audio_speaking_class = if is_speaking {
        "audio-indicator speaking"
    } else {
        "audio-indicator"
    };

    let tile_style = speak_style(visible_audio_level, is_speaking);
    let mic_inline_style = mic_style(visible_mic_level, visible_audio_level);

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
        let ss_name = format!("{}-screen", peer_display_name);
        let ss_name_title = ss_name.clone();
        return rsx! {
            div {
                class: "split-screen-tile",
                div {
                    class: "canvas-container video-on",
                    ScreenCanvas { peer_id: key.clone() }
                    h4 {
                        class: "floating-name",
                        title: "{ss_name_title}",
                        dir: "auto",
                        "{ss_name}"
                    }
                    button {
                        onclick: move |_| toggle_canvas_crop(&ss_canvas_crop),
                        class: "crop-icon",
                        CropIcon {}
                    }
                }
            }
        };
    }

    // ---- Split-layout: peer video right panel ---------------------------------
    if decision == TileDecision::RenderVideo {
        let peer_video_div_id = Rc::new(format!("peer-video-{}-div", &key));
        let div_id_mobile = (*peer_video_div_id).clone();
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

    // Store the IntersectionObserver together with its backing Closure so that
    // both are kept alive for exactly as long as needed.  When the effect
    // re-runs the old tuple is dropped, which disconnects the observer and
    // frees the closure (instead of leaking via `Closure::forget`).
    type ObserverState = (
        IntersectionObserver,
        Closure<dyn FnMut(js_sys::Array, IntersectionObserver)>,
    );
    let mut observer_signal: Signal<Option<ObserverState>> = use_signal(|| None);

    use_effect(move || {
        // Disconnect any previous observer before creating a new one.
        if let Some((old_observer, _old_closure)) = observer_signal.write().take() {
            old_observer.disconnect();
        }

        if let Some(elem) = gloo_utils::document().get_element_by_id(&id_for_effect) {
            if let Ok(canvas) = elem.dyn_into::<HtmlCanvasElement>() {
                let client_ref = client.clone();
                let id_ref = id_for_effect.clone();
                if let Err(e) = client_ref.set_peer_video_canvas(&id_ref, canvas.clone()) {
                    log::debug!("Canvas not yet ready for peer {id_ref}: {e:?}");
                }

                // Set up IntersectionObserver to track visibility
                let client_for_observer = client.clone();
                let peer_id_for_observer = id_for_effect.clone();
                let callback = Closure::wrap(Box::new(
                    move |entries: js_sys::Array, _observer: IntersectionObserver| {
                        for entry in entries.iter() {
                            let entry: IntersectionObserverEntry = entry.unchecked_into();
                            let is_visible = entry.is_intersecting();
                            client_for_observer
                                .set_peer_visibility(&peer_id_for_observer, is_visible);
                        }
                    },
                )
                    as Box<dyn FnMut(js_sys::Array, IntersectionObserver)>);

                if let Ok(observer) = IntersectionObserver::new(callback.as_ref().unchecked_ref()) {
                    observer.observe(&canvas);
                    // Store both the observer and its closure in the signal so
                    // the closure stays alive without `forget()`.  When the
                    // signal is overwritten or the component unmounts, both are
                    // dropped together.
                    observer_signal.set(Some((observer, callback)));
                }
            }
        }
    });

    // Disconnect the observer when the component unmounts.
    use_drop(move || {
        if let Some((obs, _closure)) = observer_signal.write().take() {
            obs.disconnect();
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

    // Store the IntersectionObserver together with its backing Closure so that
    // both are kept alive for exactly as long as needed.  When the effect
    // re-runs the old tuple is dropped, which disconnects the observer and
    // frees the closure (instead of leaking via `Closure::forget`).
    type ScreenObserverState = (
        IntersectionObserver,
        Closure<dyn FnMut(js_sys::Array, IntersectionObserver)>,
    );
    let mut observer_signal: Signal<Option<ScreenObserverState>> = use_signal(|| None);

    use_effect(move || {
        // Disconnect any previous observer before creating a new one.
        if let Some((old_observer, _old_closure)) = observer_signal.write().take() {
            old_observer.disconnect();
        }

        if let Some(elem) = gloo_utils::document().get_element_by_id(&canvas_id_for_effect) {
            if let Ok(canvas) = elem.dyn_into::<HtmlCanvasElement>() {
                let client_ref = client.clone();
                let peer_id_ref = peer_id_for_effect.clone();
                if let Err(e) = client_ref.set_peer_screen_canvas(&peer_id_ref, canvas.clone()) {
                    log::debug!("Screen canvas not yet ready for peer {peer_id_ref}: {e:?}");
                }

                // Set up IntersectionObserver to track visibility for screen share
                let client_for_observer = client.clone();
                let peer_id_for_observer = peer_id_for_effect.clone();
                let callback = Closure::wrap(Box::new(
                    move |entries: js_sys::Array, _observer: IntersectionObserver| {
                        for entry in entries.iter() {
                            let entry: IntersectionObserverEntry = entry.unchecked_into();
                            let is_visible = entry.is_intersecting();
                            client_for_observer
                                .set_peer_visibility(&peer_id_for_observer, is_visible);
                        }
                    },
                )
                    as Box<dyn FnMut(js_sys::Array, IntersectionObserver)>);

                if let Ok(observer) = IntersectionObserver::new(callback.as_ref().unchecked_ref()) {
                    observer.observe(&canvas);
                    // Store both the observer and its closure in the signal so
                    // the closure stays alive without `forget()`.  When the
                    // signal is overwritten or the component unmounts, both are
                    // dropped together.
                    observer_signal.set(Some((observer, callback)));
                }
            }
        }
    });

    // Disconnect the observer when the component unmounts.
    use_drop(move || {
        if let Some((obs, _closure)) = observer_signal.write().take() {
            obs.disconnect();
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
}

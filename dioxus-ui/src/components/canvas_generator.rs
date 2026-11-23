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
use crate::constants::users_allowed_to_stream;
use crate::context::VideoCallClientCtx;
use dioxus::prelude::*;
use std::rc::Rc;
use videocall_client::VideoCallClient;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{window, HtmlCanvasElement, IntersectionObserver, IntersectionObserverEntry};

/// Class that marks a speaking indicator (the tile's `.glow-overlay` and the
/// `.audio-indicator` mic badge). The look lives in `styles/features/call.css`.
pub(crate) const SPEAKING_CLASS: &str = "speaking";

/// Whether an audio level counts as speaking.
pub(crate) fn is_speaking(audio_level: f32) -> bool {
    audio_level > 0.0
}

/// The inline style for a speaking indicator: one custom property,
/// `--speak-level`, from `0.00` to `1.00`. The border, glow color and
/// transitions are in `styles/features/call.css`, keyed off this property and
/// the [`SPEAKING_CLASS`] class. Anything that is not a positive number
/// (silence, NaN) is written as `0.00`.
pub(crate) fn speak_level_style(audio_level: f32) -> String {
    let level = if is_speaking(audio_level) {
        audio_level.min(1.0)
    } else {
        0.0
    };
    format!("--speak-level: {level:.2}")
}

/// Tile corner buttons. `crop-icon` / `pin-icon` place them (call.css), the
/// rest is the shared small icon button.
const CROP_BUTTON_CLASS: &str = "crop-icon tile-action icon-btn icon-btn-sm";
const PIN_BUTTON_CLASS: &str = "pin-icon tile-action icon-btn icon-btn-sm";

/// Class list for the tile's glow overlay at a given raw audio level.
fn glow_overlay_class(audio_level: f32) -> &'static str {
    if is_speaking(audio_level) {
        "glow-overlay speaking"
    } else {
        "glow-overlay"
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

/// Pure decision function: given the tile mode and whether the peer is
/// screen-sharing, returns which rendering path to take.
///
/// Extracted so that the branching logic can be tested without requiring
/// a `VideoCallClient`, DOM, or any WASM environment.
pub(crate) fn split_layout_decision(
    mode: &TileMode,
    is_screen_share_enabled: bool,
) -> TileDecision {
    match mode {
        TileMode::ScreenOnly => {
            if !is_screen_share_enabled {
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
pub fn generate_for_peer(
    client: &VideoCallClient,
    key: &String,
    full_bleed: bool,
    audio_levels: AudioLevels,
    host_user_id: Option<&str>,
    mode: TileMode,
) -> Element {
    let audio_level = audio_levels.raw;
    let mic_audio_level = audio_levels.mic;
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

    // The mic badge turns green on the held level (kept 1 s after silence),
    // so it stays lit between words. Its glow and the tile border follow the
    // raw level through --speak-level.
    let audio_speaking_class = if is_speaking(mic_audio_level) {
        "audio-indicator speaking"
    } else {
        "audio-indicator"
    };

    let tile_class = glow_overlay_class(audio_level);
    let tile_style = speak_level_style(audio_level);
    let mic_inline_style = tile_style.clone();

    // ---- Split-layout: early return for ScreenOnly / VideoOnly ----------------
    let decision = split_layout_decision(&mode, is_screen_share_enabled_for_peer);

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
                        span { class: "floating-name-text", "{ss_name}" }
                    }
                    button {
                        onclick: move |_| toggle_canvas_crop(&ss_canvas_crop),
                        class: CROP_BUTTON_CLASS,
                        "aria-label": "Crop video",
                        CropIcon {}
                    }
                }
            }
        };
    }

    // ---- Split-layout: peer video right panel ---------------------------------
    if decision == TileDecision::RenderVideo {
        let peer_video_div_id = Rc::new(format!("peer-video-{}-div", key));
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
        let vo_tile_class = tile_class;
        let grid_class = if is_video_enabled_for_peer {
            "canvas-container video-on"
        } else {
            "canvas-container"
        };
        return rsx! {
            div {
                class: "split-peer-tile",
                id: "{peer_video_div_id}",
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
                        span { class: "floating-name-text", "{peer_display_name_vo}" }
                        if is_host {
                            CrownIcon {}
                        }
                    }
                    div {
                        class: "{vo_audio_class}",
                        style: "{vo_mic_style}",
                        MicIcon { muted: !is_audio_enabled_for_peer }
                    }
                    button {
                        onclick: move |_| toggle_canvas_crop(&pv_canvas_crop),
                        class: CROP_BUTTON_CLASS,
                        "aria-label": "Crop video",
                        CropIcon {}
                    }
                    div {
                        style: "{vo_tile_style}",
                        class: "{vo_tile_class}",
                    }
                }
            }
        };
    }

    // Full-bleed single peer (no screen share)
    if full_bleed && !is_screen_share_enabled_for_peer {
        let peer_video_div_id = Rc::new(format!("peer-video-{}-div", key));
        let div_id_mobile = (*peer_video_div_id).clone();
        let div_id_pin = (*peer_video_div_id).clone();
        let canvas_id_crop = key.clone();
        let key_clone = key.clone();
        let peer_display_name_fb = peer_display_name.clone();
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
        return rsx! {
            div {
                class: "grid-item full-bleed",
                id: "{peer_video_div_id}",
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
                            class: "placeholder-content",
                            PeerIcon {}
                            span { class: "placeholder-text", "Camera Off" }
                        }
                    }
                    h4 {
                        class: "floating-name",
                        title: "{title}",
                        dir: "auto",
                        span { class: "floating-name-text", "{peer_display_name_fb}" }
                        if is_host {
                            CrownIcon {}
                        }
                    }
                    div {
                        class: "{audio_speaking_class}",
                        style: "{mic_inline_style}",
                        MicIcon { muted: !is_audio_enabled_for_peer }
                    }
                    button {
                        onclick: move |_| toggle_canvas_crop(&canvas_id_crop),
                        class: CROP_BUTTON_CLASS,
                        "aria-label": "Crop video",
                        CropIcon {}
                    }
                    button {
                        onclick: move |_| toggle_pinned_div(&div_id_pin),
                        class: PIN_BUTTON_CLASS,
                        "aria-label": "Pin video",
                        PushPinIcon {}
                    }
                    // Glow overlay renders ON TOP of video content so the
                    // inset box-shadow is not hidden behind the canvas element.
                    div {
                        style: "{tile_style}",
                        class: "{tile_class}",
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
    let screen_share_div_id = Rc::new(format!("screen-share-{}-div", key));
    let peer_video_div_id = Rc::new(format!("peer-video-{}-div", key));

    let ss_div_mobile = (*screen_share_div_id).clone();
    let ss_div_pin = (*screen_share_div_id).clone();
    let ss_canvas_crop = format!("screen-share-{}", key);
    let ss_name = format!("{}-screen", peer_display_name);

    let pv_div_mobile = (*peer_video_div_id).clone();
    let pv_div_pin = (*peer_video_div_id).clone();
    let pv_canvas_crop = key.clone();
    let key_clone = key.clone();
    let peer_display_name_grid = peer_display_name.clone();
    let title_grid = if is_host {
        format!("Host: {peer_user_id}")
    } else {
        peer_user_id.clone()
    };

    rsx! {
        // Canvas for Screen share.
        if is_screen_share_enabled_for_peer {
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
                    }
                    button {
                        onclick: move |_| toggle_canvas_crop(&ss_canvas_crop),
                        class: CROP_BUTTON_CLASS,
                        "aria-label": "Crop video",
                        CropIcon {}
                    }
                    button {
                        onclick: move |_| toggle_pinned_div(&ss_div_pin),
                        class: PIN_BUTTON_CLASS,
                        "aria-label": "Pin video",
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
            let grid_tile_class = tile_class;
            rsx! {
                div {
                    class: "grid-item",
                    id: "{peer_video_div_id}",
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
                            span { class: "floating-name-text", "{peer_display_name_grid}" }
                            if is_host {
                                CrownIcon {}
                            }
                        }
                        div {
                            class: "{audio_speaking_class}",
                            style: "{grid_mic_style}",
                            MicIcon { muted: !is_audio_enabled_for_peer }
                        }
                        button {
                            onclick: move |_| toggle_canvas_crop(&pv_canvas_crop),
                            class: CROP_BUTTON_CLASS,
                            "aria-label": "Crop video",
                            CropIcon {}
                        }
                        button {
                            onclick: move |_| toggle_pinned_div(&pv_div_pin),
                            class: PIN_BUTTON_CLASS,
                            "aria-label": "Pin video",
                            PushPinIcon {}
                        }
                        // Glow overlay renders ON TOP of video content
                        div {
                            style: "{grid_tile_style}",
                            class: "{grid_tile_class}",
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

    // -- ScreenOnly: peer IS screen-sharing → render screen share -------------
    #[test]
    fn screen_only_sharing_renders_screen() {
        assert_eq!(
            split_layout_decision(&TileMode::ScreenOnly, true),
            TileDecision::RenderScreenShare,
        );
    }

    // -- ScreenOnly: peer is NOT screen-sharing → empty -----------------------
    #[test]
    fn screen_only_not_sharing_returns_empty() {
        assert_eq!(
            split_layout_decision(&TileMode::ScreenOnly, false),
            TileDecision::Empty,
        );
    }

    // -- VideoOnly: always renders the peer video tile ------------------------
    #[test]
    fn video_only_renders_video() {
        assert_eq!(
            split_layout_decision(&TileMode::VideoOnly, false),
            TileDecision::RenderVideo,
        );
    }

    #[test]
    fn video_only_with_screen_share_renders_video() {
        assert_eq!(
            split_layout_decision(&TileMode::VideoOnly, true),
            TileDecision::RenderVideo,
        );
    }

    // -- Full: always falls through to the grid / full-bleed paths ------------
    #[test]
    fn full_mode_falls_through() {
        assert_eq!(
            split_layout_decision(&TileMode::Full, false),
            TileDecision::FallThrough,
        );
    }

    #[test]
    fn full_mode_with_screen_share_falls_through() {
        assert_eq!(
            split_layout_decision(&TileMode::Full, true),
            TileDecision::FallThrough,
        );
    }

    // -- TileMode Default trait -----------------------------------------------
    #[test]
    fn tile_mode_default_is_full() {
        assert_eq!(TileMode::default(), TileMode::Full);
    }

    // -- Speaking indicator contract ------------------------------------------
    // The style attribute carries only --speak-level; the look is in call.css.
    #[test]
    fn speak_level_style_silent_is_zero() {
        assert_eq!(speak_level_style(0.0), "--speak-level: 0.00");
    }

    #[test]
    fn speak_level_style_formats_two_decimals() {
        assert_eq!(speak_level_style(0.5), "--speak-level: 0.50");
        assert_eq!(speak_level_style(0.123), "--speak-level: 0.12");
    }

    #[test]
    fn speak_level_style_clamps_out_of_range() {
        assert_eq!(speak_level_style(1.7), "--speak-level: 1.00");
        assert_eq!(speak_level_style(-0.3), "--speak-level: 0.00");
        assert_eq!(speak_level_style(f32::NAN), "--speak-level: 0.00");
    }

    #[test]
    fn speak_level_style_has_no_literal_look() {
        // The old inline border/box-shadow/transition must not come back.
        let style = speak_level_style(0.8);
        assert!(!style.contains("border"));
        assert!(!style.contains("box-shadow"));
        assert!(!style.contains("transition"));
        assert!(!style.contains("rgba"));
    }

    #[test]
    fn is_speaking_only_for_positive_levels() {
        assert!(!is_speaking(0.0));
        assert!(!is_speaking(-1.0));
        assert!(!is_speaking(f32::NAN));
        assert!(is_speaking(0.01));
        assert!(is_speaking(1.0));
    }

    #[test]
    fn glow_overlay_class_adds_speaking_only_when_speaking() {
        assert_eq!(glow_overlay_class(0.0), "glow-overlay");
        assert_eq!(glow_overlay_class(0.4), "glow-overlay speaking");
        assert!(glow_overlay_class(0.4)
            .split_whitespace()
            .any(|c| c == SPEAKING_CLASS));
    }
}

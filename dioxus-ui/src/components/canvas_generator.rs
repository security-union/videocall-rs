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
use crate::context::{PinnedPanel, PinnedPanelCtx, VideoCallClientCtx};
use dioxus::prelude::*;
use std::rc::Rc;
use videocall_client::VideoCallClient;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{window, HtmlCanvasElement, IntersectionObserver, IntersectionObserverEntry};

/// Compute the inline CSS for the speaking glow on the tile shell.
/// Controls **only box-shadow** glow intensity, which scales with audio level.
/// Border appearance (rotating mint-gradient) is handled by the CSS class
/// `speaking-tile`, toggled by the caller when `audio_level > 0`.
/// All shadow layers use spread 0 so glow blooms from blur only.
pub(crate) fn speak_style(audio_level: f32, is_suppressed: bool) -> String {
    if is_suppressed {
        return "box-shadow: none; transition: none;".to_string();
    }
    if audio_level <= 0.0 {
        return "box-shadow: none; transition: box-shadow 0.6s ease-out;".to_string();
    }

    let i = audio_level.clamp(0.0, 1.0);

    // Shadow colour is fixed to a dominant green in the CSS gradient (#70B652)
    // so the glow hue always coordinates with the animated border.
    format!(
        "box-shadow: \
            0 0 {tight_blur:.0}px 0 rgba(112, 182, 82, {tight_a:.2}), \
            0 0 {wide_blur:.0}px 0 rgba(112, 182, 82, {wide_a:.2}), \
            inset 0 0 {inset_blur:.0}px 0 rgba(112, 182, 82, {inset_a:.2}), \
            inset 0 0 {deep_blur:.0}px 0 rgba(112, 182, 82, {deep_a:.2}); \
         transition: box-shadow 0.18s ease-in-out;",
        // Tight outer glow hugging the border
        tight_blur = 8.0 + i * 16.0, // 8–24 px
        tight_a = 0.30 + i * 0.40,   // 0.30–0.70
        // Wider ambient bloom
        wide_blur = 22.0 + i * 30.0, // 22–52 px
        wide_a = 0.12 + i * 0.22,    // 0.12–0.34
        // Inset edge glow — interior border luminance
        inset_blur = 10.0 + i * 14.0, // 10–24 px
        inset_a = 0.12 + i * 0.28,    // 0.12–0.40
        // Deep inset wash — glass-card interior depth
        deep_blur = 28.0 + i * 22.0, // 28–50 px
        deep_a = 0.04 + i * 0.10,    // 0.04–0.14
    )
}

/// Compute the inline CSS for the mic icon glow.
/// Always returns explicit values — no reliance on CSS class for glow reset.
///
/// Two separate signals control different visual properties:
/// - `mic_audio_level` (held 1s after silence) controls the icon COLOR (green)
/// - `glow_audio_level` (raw, same as border) controls the drop-shadow GLOW
///
/// This way the icon stays green briefly after speech stops (via the held signal)
/// while the drop-shadow glow tracks the border glow exactly.
fn mic_style(mic_audio_level: f32, glow_audio_level: f32, is_suppressed: bool) -> String {
    if is_suppressed {
        // Forced suppression: immediate off with no transition
        return "color: inherit; filter: none; transition: none;".to_string();
    }
    if mic_audio_level <= 0.0 && glow_audio_level <= 0.0 {
        // Fully silent: fade out both color and filter
        return "color: inherit; filter: none; transition: color 5.0s ease-out, filter 1.5s ease-out;".to_string();
    }
    // Unreachable in practice: the mic hold timer guarantees mic_audio_level
    // stays positive at least as long as glow_audio_level. Handle defensively
    // by showing only the glow without the green icon color.
    if mic_audio_level <= 0.0 && glow_audio_level > 0.0 {
        let clamped = glow_audio_level.clamp(0.0, 1.0);
        let glow_i = clamped.sqrt();
        return format!(
            "color: inherit; \
             filter: drop-shadow(0 0 {:.0}px rgba(112, 182, 82, {:.2})) \
                     drop-shadow(0 0 {:.0}px rgba(112, 182, 82, {:.2})); \
             transition: color 5.0s ease-out, filter 0.15s ease-in;",
            8.0 + glow_i * 16.0,
            0.7 + glow_i * 0.3,
            3.0 + glow_i * 8.0,
            0.8 + glow_i * 0.2,
        );
    }
    if mic_audio_level > 0.0 && glow_audio_level <= 0.0 {
        // Held color (still green) but raw glow has faded — no drop-shadow
        return "color: #70B652; filter: none; transition: color 0.05s ease-in, filter 1.5s ease-out;".to_string();
    }
    // Both positive: green color + scaled drop-shadow glow
    let clamped = glow_audio_level.clamp(0.0, 1.0);
    let glow_i = clamped.sqrt();
    format!(
        "color: #70B652; \
         filter: drop-shadow(0 0 {:.0}px rgba(112, 182, 82, {:.2})) \
                 drop-shadow(0 0 {:.0}px rgba(112, 182, 82, {:.2})); \
         transition: color 0.05s ease-in, filter 0.15s ease-in;",
        8.0 + glow_i * 16.0, // primary drop-shadow blur: 8–24px
        0.7 + glow_i * 0.3,  // primary drop-shadow alpha: 0.7–1.0
        3.0 + glow_i * 8.0,  // secondary (tight) glow blur: 3–11px
        0.8 + glow_i * 0.2,  // secondary glow alpha: 0.8–1.0
    )
}

/// Returns `true` when speaking indicators should be suppressed for `current`.
///
/// Suppression fires only when a panel is pinned AND `current` is not that
/// pinned panel (variant-aware: `PeerVideo("x") != ScreenShare("x")`).
fn is_speaking_suppressed(pinned: Option<&PinnedPanel>, current: &PinnedPanel) -> bool {
    pinned.is_some_and(|p| p != current)
}

/// Render a single peer tile. If `full_bleed` is true and the peer is not screen sharing,
/// the video tile will occupy the full grid area. The `audio_level` parameter (0.0–1.0) drives
/// a glow whose intensity scales with voice volume.
/// If `host_user_id` matches the peer's authenticated user_id, a crown icon is displayed next to the name.
/// When a panel is pinned fullscreen, speaking indicators are suppressed on all non-pinned tiles.
pub fn generate_for_peer(
    client: &VideoCallClient,
    key: &String,
    full_bleed: bool,
    audio_level: f32,
    mic_audio_level: f32,
    host_user_id: Option<&str>,
    pinned_panel: PinnedPanelCtx,
    is_speaking: bool,
) -> Element {
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

    // When a panel is pinned fullscreen, suppress speaking indicators on all
    // non-pinned tiles so the glow doesn't bleed through behind the pinned panel.
    // Suppression is computed per panel type: pinned exists AND current ≠ pinned.
    let pinned = pinned_panel();
    let suppress_peer =
        is_speaking_suppressed(pinned.as_ref(), &PinnedPanel::PeerVideo(key.clone()));
    // Screen share tiles have no speaking UI today; computed for correctness.
    let _suppress_screen =
        is_speaking_suppressed(pinned.as_ref(), &PinnedPanel::ScreenShare(key.clone()));
    let audio_level = if suppress_peer { 0.0 } else { audio_level };
    let mic_audio_level = if suppress_peer { 0.0 } else { mic_audio_level };

    let is_speaking = if suppress_peer { false } else { is_speaking };

    let audio_speaking_class = if is_speaking {
        "audio-indicator speaking"
    } else {
        "audio-indicator"
    };

    // Compute inline styles: border glow uses raw audio_level,
    // mic icon uses mic_audio_level (held for 1s after silence in Rust)
    let tile_style = speak_style(audio_level, suppress_peer);
    let mic_inline_style = mic_style(mic_audio_level, audio_level, suppress_peer);

    // Pre-compute pinned state for this peer's panels so the CSS class is
    // derived from the signal, surviving re-renders.
    let is_pv_pinned = pinned.as_ref() == Some(&PinnedPanel::PeerVideo(key.clone()));
    let is_ss_pinned = pinned.as_ref() == Some(&PinnedPanel::ScreenShare(key.clone()));

    // Full-bleed single peer (no screen share)
    if full_bleed && !is_screen_share_enabled_for_peer {
        let peer_video_div_id = Rc::new(format!("peer-video-{}-div", &key));
        let pin_fb_mobile = PinnedPanel::PeerVideo(key.clone());
        let pin_fb_btn = PinnedPanel::PeerVideo(key.clone());
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
        let fb_grid_class = match (is_pv_pinned, is_speaking) {
            (true, true) => "grid-item full-bleed grid-item-pinned speaking-tile",
            (true, false) => "grid-item full-bleed grid-item-pinned",
            (false, true) => "grid-item full-bleed speaking-tile",
            (false, false) => "grid-item full-bleed",
        };
        return rsx! {
            div {
                class: "{fb_grid_class}",
                id: "{peer_video_div_id}",
                style: "{tile_style}",
                div {
                    class: "{full_bleed_class}",
                    onclick: move |_| {
                        if is_mobile_viewport() {
                            toggle_pinned(pinned_panel, &pin_fb_mobile);
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
                        class: "{audio_speaking_class}",
                        style: "{mic_inline_style}",
                        MicIcon { muted: !is_audio_enabled_for_peer }
                    }
                    button {
                        onclick: move |_| toggle_canvas_crop(&canvas_id_crop),
                        class: "crop-icon",
                        CropIcon {}
                    }
                    button {
                        onclick: move |_| toggle_pinned(pinned_panel, &pin_fb_btn),
                        class: "pin-icon",
                        PushPinIcon {}
                    }
                }
            }
        };
    }

    // Regular grid tile, optionally with screen share tile
    let screen_share_css = match (client.is_awaiting_peer_screen_frame(key), is_ss_pinned) {
        (true, _) => "grid-item hidden",
        (false, true) => "grid-item grid-item-pinned",
        (false, false) => "grid-item",
    };
    let screen_share_div_id = Rc::new(format!("screen-share-{}-div", &key));
    let peer_video_div_id = Rc::new(format!("peer-video-{}-div", &key));

    let pin_ss_mobile = PinnedPanel::ScreenShare(key.clone());
    let pin_ss_btn = PinnedPanel::ScreenShare(key.clone());
    let ss_canvas_crop = format!("screen-share-{}", key);
    let ss_name = format!("{}-screen", peer_display_name);

    let pin_pv_mobile = PinnedPanel::PeerVideo(key.clone());
    let pin_pv_btn = PinnedPanel::PeerVideo(key.clone());
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
                            toggle_pinned(pinned_panel, &pin_ss_mobile);
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
                        onclick: move |_| toggle_pinned(pinned_panel, &pin_ss_btn),
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
            let pv_grid_class = match (is_pv_pinned, is_speaking) {
                (true, true) => "grid-item grid-item-pinned speaking-tile",
                (true, false) => "grid-item grid-item-pinned",
                (false, true) => "grid-item speaking-tile",
                (false, false) => "grid-item",
            };
            rsx! {
                div {
                    class: "{pv_grid_class}",
                    id: "{peer_video_div_id}",
                    style: "{grid_tile_style}",
                    // One canvas for the User Video
                    div {
                        class: "{grid_class}",
                        onclick: move |_| {
                            if is_mobile_viewport() {
                                toggle_pinned(pinned_panel, &pin_pv_mobile);
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
                            class: "{audio_speaking_class}",
                            style: "{grid_mic_style}",
                            MicIcon { muted: !is_audio_enabled_for_peer }
                        }
                        button {
                            onclick: move |_| toggle_canvas_crop(&pv_canvas_crop),
                            class: "crop-icon",
                            CropIcon {}
                        }
                        button {
                            onclick: move |_| toggle_pinned(pinned_panel, &pin_pv_btn),
                            class: "pin-icon",
                            PushPinIcon {}
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

/// Toggle the fullscreen pin for a panel.  Only one panel can be pinned at a
/// time; pinning a new panel automatically unpins the previous one.
///
/// The `grid-item-pinned` CSS class is derived from the signal during render,
/// so we only need to update the signal here — no imperative DOM manipulation.
fn toggle_pinned(mut pinned_panel: PinnedPanelCtx, panel: &PinnedPanel) {
    let current = pinned_panel.peek().clone();
    if current.as_ref() == Some(panel) {
        pinned_panel.set(None);
    } else {
        pinned_panel.set(Some(panel.clone()));
    }
}

/// Reset the pinned-panel signal to `None`.
///
/// The `grid-item-pinned` CSS class is derived from the signal during render,
/// so we only need to clear the signal here.
pub(crate) fn clear_pinned(mut pinned_panel: PinnedPanelCtx) {
    pinned_panel.set(None);
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

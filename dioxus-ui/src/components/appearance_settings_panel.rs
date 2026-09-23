/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use crate::components::attendants::action_bar_announce_text;
use crate::components::canvas_generator::{glow_tail_seconds, speak_style};
use crate::components::color_picker::HsvColorPicker;
use crate::components::peer_tile::glow_deadman_ms;
use crate::context::{
    apply_theme_to_dom, default_glow_palette, load_glow_palette_from_storage,
    save_glow_palette_to_storage, AppearanceSettings, AppearanceSettingsCtx, CustomThemeCtx,
    GlowColor, LocalAudioLevelCtx, LocalSpeakingCtx, Theme, ThemePreferenceCtx, MAX_PALETTE_COLORS,
};
use crate::theme::color as theme_color;
use crate::theme_file::{
    clear_custom_theme, custom_theme_display_name, persist_custom_theme_json, ThemeFileError,
    MAX_THEME_JSON_BYTES,
};
use crate::util::color_math::parse_hex;
use dioxus::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use videocall_client::adaptive_quality_constants::HEARTBEAT_KEEPALIVE_INTERVAL_MS;
use videocall_diagnostics::{recv_loop_action, subscribe, DiagEvent, MetricValue, RecvLoopAction};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

/// Restore keyboard focus to a meaningful in-panel control after a color
/// picker action (add, close, cancel, delete). Tries the add button first;
/// when it is unmounted (palette at MAX_PALETTE_COLORS) falls back to the
/// selected swatch, then the swatch container.
fn focus_color_panel_fallback() {
    let doc = match web_sys::window().and_then(|w| w.document()) {
        Some(d) => d,
        None => return,
    };
    // 1. Add button (present when < MAX_PALETTE_COLORS)
    if let Some(el) = doc.get_element_by_id("add-custom-color-btn") {
        if let Ok(html) = el.dyn_into::<web_sys::HtmlElement>() {
            let _ = html.focus();
            return;
        }
    }
    // 2. Currently selected swatch (aria-pressed="true")
    if let Ok(Some(el)) = doc.query_selector(".color-swatch[aria-pressed=\"true\"]") {
        if let Ok(html) = el.dyn_into::<web_sys::HtmlElement>() {
            let _ = html.focus();
            return;
        }
    }
    // 3. Swatch container as final fallback
    if let Some(el) = doc.get_element_by_id("color-swatches-container") {
        if let Ok(html) = el.dyn_into::<web_sys::HtmlElement>() {
            let _ = html.focus();
        }
    }
}

fn focus_swatch_after_delete_deferred(removed_idx: usize) {
    // Use a browser timeout so the list re-renders before we focus the next target.
    let Some(window) = web_sys::window() else {
        return;
    };
    let callback = Closure::wrap(Box::new(move || {
        let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };

        let Ok(nodes) = doc.query_selector_all(SWATCH_SELECT_BUTTONS) else {
            focus_color_panel_fallback();
            return;
        };

        let Some(target_idx) = delete_neighbor_index(nodes.length() as usize, removed_idx) else {
            focus_color_panel_fallback();
            return;
        };
        if let Some(node) = nodes.item(target_idx as u32) {
            if let Ok(html) = node.dyn_into::<web_sys::HtmlElement>() {
                let _ = html.focus();
                return;
            }
        }

        focus_color_panel_fallback();
    }) as Box<dyn FnMut()>);
    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
        callback.as_ref().unchecked_ref(),
        50,
    );
    callback.forget();
}

fn focus_color_panel_fallback_deferred() {
    // Defer focus until after modal unmount / DOM updates to avoid races.
    let Some(window) = web_sys::window() else {
        return;
    };
    let callback = Closure::wrap(Box::new(move || {
        focus_color_panel_fallback();
    }) as Box<dyn FnMut()>);
    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
        callback.as_ref().unchecked_ref(),
        50,
    );
    callback.forget();
}

const SWATCH_SELECT_BUTTONS: &str = "#color-swatches-container .color-swatch:not(.add-color-btn)";

/// The swatch that slides into a deleted slot, else the one before it. Focus
/// and selection both follow this, so they land on the same swatch.
fn delete_neighbor_index(len_after: usize, removed_idx: usize) -> Option<usize> {
    len_after.checked_sub(1).map(|last| removed_idx.min(last))
}

#[derive(Debug, PartialEq)]
struct PaletteDeletion {
    palette: Vec<GlowColor>,
    removed_idx: usize,
    selection: GlowColor,
}

fn delete_from_palette(
    palette: &[GlowColor],
    color: GlowColor,
    selected: GlowColor,
) -> Option<PaletteDeletion> {
    let removed_idx = palette.iter().position(|c| *c == color)?;
    let mut remaining = palette.to_vec();
    remaining.remove(removed_idx);
    let selection = if color == selected {
        delete_neighbor_index(remaining.len(), removed_idx).map_or(selected, |i| remaining[i])
    } else {
        selected
    };
    Some(PaletteDeletion {
        palette: remaining,
        removed_idx,
        selection,
    })
}

fn palette_with(palette: &[GlowColor], color: GlowColor) -> Vec<GlowColor> {
    let mut next = palette.to_vec();
    if !next.contains(&color) && next.len() < MAX_PALETTE_COLORS {
        next.push(color);
    }
    next
}

const DELETE_REPEAT_GUARD_MS: f64 = 400.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum DeleteGesture {
    Key,
    Badge,
}

/// A second tap this soon lands on the swatch that slid under the finger, so
/// it is the same gesture, not a new choice. Keys have their own repeat guard.
fn admits_delete(gesture: DeleteGesture, last_delete_ms: Option<f64>, now_ms: f64) -> bool {
    gesture == DeleteGesture::Key
        || last_delete_ms.is_none_or(|last| now_ms - last >= DELETE_REPEAT_GUARD_MS)
}

thread_local! {
    static PERFORMANCE: Option<web_sys::Performance> =
        web_sys::window().and_then(|window| window.performance());
}

fn monotonic_now_ms() -> f64 {
    PERFORMANCE.with(|performance| {
        performance
            .as_ref()
            .map_or_else(js_sys::Date::now, |performance| performance.now())
    })
}

fn addition_announcement(color: GlowColor, grew: bool) -> String {
    let outcome = if grew { "added" } else { "selected" };
    format!("{} highlight {outcome}.", color.label())
}

fn deletion_announcement(
    deleted: GlowColor,
    selected: GlowColor,
    deletion: &PaletteDeletion,
) -> String {
    if deletion.palette.is_empty() {
        return "All highlight colors deleted.".to_string();
    }
    let mut message = format!("{} highlight deleted.", deleted.label());
    if deletion.selection != selected {
        message.push_str(&format!(" {} selected.", deletion.selection.label()));
    }
    message
}

/// `(select, delete)` accessible names for one swatch.
fn swatch_labels(color: GlowColor) -> (String, String) {
    let label = color.label();
    match color {
        GlowColor::Custom { .. } => (
            format!("Select custom highlight {label}"),
            format!("Delete custom highlight {label}"),
        ),
        _ => (
            format!("Select {label} highlight"),
            format!("Delete {label} highlight"),
        ),
    }
}

const PALETTE_RESET_ANNOUNCEMENT: &str = "Speaker highlight reset to defaults.";

#[derive(Clone, Copy)]
struct PaletteControl {
    palette: Signal<Vec<GlowColor>>,
    appearance: Signal<AppearanceSettings>,
    announcement: Signal<String>,
    announcement_nonce: Signal<u32>,
    last_delete_ms: Signal<Option<f64>>,
}

impl PaletteControl {
    fn select(mut self, color: GlowColor) {
        if self.appearance.peek().glow_color != color {
            let current = *self.appearance.peek();
            self.appearance.set(AppearanceSettings {
                glow_color: color,
                ..current
            });
        }
    }

    fn announce(mut self, message: String) {
        let nonce = *self.announcement_nonce.peek();
        self.announcement_nonce.set(nonce.wrapping_add(1));
        self.announcement.set(message);
    }

    fn store(mut self, palette: Vec<GlowColor>) {
        save_glow_palette_to_storage(&palette);
        self.palette.set(palette);
    }

    fn add(self, color: GlowColor) {
        let next = palette_with(&self.palette.peek(), color);
        let grew = next.len() > self.palette.peek().len();
        if grew {
            self.store(next);
        }
        self.select(color);
        self.announce(addition_announcement(color, grew));
    }

    fn delete(mut self, color: GlowColor, gesture: DeleteGesture) {
        let now_ms = monotonic_now_ms();
        if !admits_delete(gesture, *self.last_delete_ms.peek(), now_ms) {
            return;
        }
        let selected = self.appearance.peek().glow_color;
        let Some(deletion) = delete_from_palette(&self.palette.peek(), color, selected) else {
            return;
        };
        self.last_delete_ms.set(Some(now_ms));
        self.announce(deletion_announcement(color, selected, &deletion));
        self.select(deletion.selection);
        let removed_idx = deletion.removed_idx;
        self.store(deletion.palette);
        focus_swatch_after_delete_deferred(removed_idx);
    }

    fn reset(mut self) {
        let defaults = AppearanceSettings::default();
        let current = *self.appearance.peek();
        self.appearance.set(AppearanceSettings {
            glow_enabled: defaults.glow_enabled,
            glow_color: defaults.glow_color,
            glow_brightness: defaults.glow_brightness,
            inner_glow_strength: defaults.inner_glow_strength,
            glow_decay: defaults.glow_decay,
            glow_velocity: defaults.glow_velocity,
            ..current
        });
        self.store(default_glow_palette());
        self.announce(PALETTE_RESET_ANNOUNCEMENT.to_string());
    }
}

/// Cycle keyboard focus within the color-picker modal on Tab / Shift+Tab.
///
/// Without this, Tab from the last focusable element in the dialog moves
/// focus to the Brightness slider that lives immediately after the modal in
/// DOM order — the scrim blocks mouse clicks but does NOT block keyboard
/// focus, so the user ends up driving a control they can't see. Returns
/// `true` when focus wrapped (caller should `prevent_default`).
fn trap_tab_in_color_modal(shift: bool) -> bool {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return false;
    };
    let modal = match document.query_selector(".custom-color-modal") {
        Ok(Some(el)) => el,
        _ => return false,
    };
    let nodes = match modal
        .query_selector_all("button:not([disabled]), input:not([disabled]), [tabindex=\"0\"]")
    {
        Ok(n) => n,
        Err(_) => return false,
    };
    let count = nodes.length();
    if count == 0 {
        return false;
    }
    let first: web_sys::HtmlElement = match nodes.item(0).and_then(|n| n.dyn_into().ok()) {
        Some(el) => el,
        None => return false,
    };
    let last: web_sys::HtmlElement = match nodes.item(count - 1).and_then(|n| n.dyn_into().ok()) {
        Some(el) => el,
        None => return false,
    };
    let active = document.active_element();
    // Compare via Node::is_same_node — each `.item(i)` returns a fresh JsValue
    // wrapper, but they all reference the same underlying DOM node as the
    // active element, so identity by DOM node is the correct check.
    let first_node: &web_sys::Node = first.as_ref();
    let last_node: &web_sys::Node = last.as_ref();
    let is_first = active
        .as_ref()
        .map(|el| {
            let n: &web_sys::Node = el.as_ref();
            n.is_same_node(Some(first_node))
        })
        .unwrap_or(false);
    let is_last = active
        .as_ref()
        .map(|el| {
            let n: &web_sys::Node = el.as_ref();
            n.is_same_node(Some(last_node))
        })
        .unwrap_or(false);
    // Also wrap when focus has escaped the modal entirely (e.g. the dialog
    // container itself was focused via onmounted and the user Shift+Tabs).
    let modal_node: &web_sys::Node = modal.as_ref();
    let active_in_modal = active
        .as_ref()
        .map(|el| {
            let n: &web_sys::Node = el.as_ref();
            modal_node.contains(Some(n))
        })
        .unwrap_or(false);
    if shift && (is_first || !active_in_modal) {
        let _ = last.focus();
        return true;
    }
    if !shift && (is_last || !active_in_modal) {
        let _ = first.focus();
        return true;
    }
    false
}

fn is_keyboard_activation_key(key: &Key) -> bool {
    *key == Key::Enter || matches!(key, Key::Character(s) if s == " ")
}

/// One constant per slider: the bubble and its `aria-describedby` description
/// render the same text and cannot drift (issue 1871).
const BRIGHTNESS_HELP_TEXT: &str =
    "Brightness sets how intense the glow's color is. 0% is a faint hint; 100% is the most vivid.";
const GLOW_HELP_TEXT: &str = "Glow sets how far the light reaches past the tile edge. 0% is a border only; 100% is the widest spread.";
const VELOCITY_HELP_TEXT: &str = "Velocity sets how fast the glow reacts to your voice. 0% is a slow, smooth rise; 100% snaps to every change.";
const DECAY_HELP_TEXT: &str = "Decay sets how long the glow lingers after speech. 0% is an instant cutoff; 100% is the longest tail.";

/// Next `(open, suppressed)` slider keys after the `slug` trigger is activated.
/// One shared pair drives all four, so opening one closes the rest. Toggling
/// OFF latches suppression: the trigger keeps focus and `:focus-within` reveals.
fn next_help_state(
    slug: &'static str,
    open: Option<&'static str>,
) -> (Option<&'static str>, Option<&'static str>) {
    if open == Some(slug) {
        (None, Some(slug))
    } else {
        (Some(slug), None)
    }
}

/// Class string for the `slug` trigger. Branch order is load-bearing —
/// suppression wins, pinned by `help_class_escape_suppression_wins_over_open`.
fn help_class(slug: &str, open: Option<&str>, suppressed: Option<&str>) -> &'static str {
    if suppressed == Some(slug) {
        "settings-info-icon speaker-highlight-help-icon speaker-highlight-help-icon--suppressed"
    } else if open == Some(slug) {
        "settings-info-icon speaker-highlight-help-icon speaker-highlight-help-icon--open"
    } else {
        "settings-info-icon speaker-highlight-help-icon"
    }
}

/// The `(?)` trigger and its tooltip for one slider. A SIBLING of the label:
/// nesting it would fold the help text into the slider's accessible name.
#[component]
fn SpeakerHighlightHelp(
    slug: &'static str,
    label: &'static str,
    text: &'static str,
    /// Open downward: the topmost row's upward bubble is clipped by
    /// `.settings-panel`'s scroll box when that row reaches the viewport top.
    open_below: bool,
    open: Signal<Option<&'static str>>,
    suppressed: Signal<Option<&'static str>>,
) -> Element {
    let mut open = open;
    let mut suppressed = suppressed;
    let base_class = help_class(slug, open(), suppressed());
    let class = if open_below {
        format!("{base_class} speaker-highlight-help-icon--below")
    } else {
        base_class.to_string()
    };

    rsx! {
        span {
            class: "{class}",
            role: "button",
            tabindex: 0,
            "aria-label": "About the {label} setting",
            "aria-describedby": "speaker-highlight-{slug}-tip",
            "data-testid": "speaker-highlight-{slug}-help",
            onclick: move |evt: Event<MouseData>| {
                evt.stop_propagation();
                let (next_open, next_suppressed) = next_help_state(slug, open());
                open.set(next_open);
                suppressed.set(next_suppressed);
            },
            onkeydown: move |evt: Event<KeyboardData>| {
                let key = evt.key();
                if is_keyboard_activation_key(&key) {
                    evt.prevent_default();
                    evt.stop_propagation();
                    let (next_open, next_suppressed) = next_help_state(slug, open());
                    open.set(next_open);
                    suppressed.set(next_suppressed);
                } else if key == Key::Escape && suppressed() != Some(slug) {
                    // Dismiss the tooltip only, without blurring. A second Escape
                    // finds this slug suppressed and bubbles, closing the modal.
                    evt.stop_propagation();
                    open.set(None);
                    suppressed.set(Some(slug));
                }
            },
            onfocusout: move |_| {
                if *open.peek() == Some(slug) {
                    open.set(None);
                }
                if *suppressed.peek() == Some(slug) {
                    suppressed.set(None);
                }
            },
            "(?)"
            // `role="button"` is children-presentational, so this child's
            // `role="tooltip"` is inert and `aria-describedby` above does all
            // the work. Not a bug to fix.
            span {
                id: "speaker-highlight-{slug}-tip",
                class: "speaker-highlight-help-tip",
                role: "tooltip",
                "data-testid": "speaker-highlight-{slug}-help-text",
                {text}
            }
        }
    }
}

#[component]
pub fn AppearanceSettingsPanel() -> Element {
    let mut theme_ctx = use_context::<ThemePreferenceCtx>();
    let mut appearance_ctx = use_context::<AppearanceSettingsCtx>();
    // Fallback signals for when contexts are not provided (e.g. in tests or
    // isolated component previews). Hooks must be called unconditionally, so we
    // always create them — but any writes the panel makes through these fallback
    // signals stay local to this component instance and do NOT propagate to
    // attendants.rs or any other reader. Production always provides the real context.
    let appearance = (appearance_ctx.0)();

    let brightness_slider_style = slider_fill_style(appearance.glow_brightness);
    let inner_slider_style = slider_fill_style(appearance.inner_glow_strength);
    let velocity_slider_style = slider_fill_style(appearance.glow_velocity);
    let decay_slider_style = slider_fill_style(appearance.glow_decay);

    let palette = use_signal(load_glow_palette_from_storage);
    let announcement = use_signal(String::new);
    let announcement_nonce = use_signal(|| 0u32);
    let palette_control = PaletteControl {
        palette,
        appearance: appearance_ctx.0,
        announcement,
        announcement_nonce,
        last_delete_ms: use_signal(|| None),
    };
    let mut show_picker = use_signal(|| false);
    let mut color_input = use_signal(String::new);
    let mut input_error = use_signal(|| false);

    // Custom theme (single-slot) state
    let fallback_custom_theme = use_signal(|| None::<String>);
    let mut custom_theme_ctx =
        try_use_context::<CustomThemeCtx>().unwrap_or(CustomThemeCtx(fallback_custom_theme));
    let mut import_error: Signal<Option<String>> = use_signal(|| None);

    // One shared pair for all four `(?)` triggers: opening one closes the rest.
    let help_open = use_signal(|| None::<&'static str>);
    let help_suppressed = use_signal(|| None::<&'static str>);

    rsx! {
        div { class: if appearance.glow_enabled { "appearance-settings-panel" } else { "appearance-settings-panel glow-disabled" },

            div { class: "appearance-content-column",

                // ── Section 1: Theme ─────────────────────────────────────────────
                section { class: "appearance-section",
                    div { class: "appearance-section-header",
                        div { class: "settings-panel-title",
                            svg {
                                class: "settings-panel-title-icon",
                                xmlns: "http://www.w3.org/2000/svg",
                                width: "18",
                                height: "18",
                                view_box: "0 0 24 24",
                                fill: "none",
                                stroke: "currentColor",
                                stroke_width: "2",
                                stroke_linecap: "round",
                                stroke_linejoin: "round",
                                "aria-hidden": "true",

                                circle { cx: "12", cy: "12", r: "5" }
                                line {
                                    x1: "12",
                                    y1: "1",
                                    x2: "12",
                                    y2: "3",
                                }
                                line {
                                    x1: "12",
                                    y1: "21",
                                    x2: "12",
                                    y2: "23",
                                }
                                line {
                                    x1: "4.22",
                                    y1: "4.22",
                                    x2: "5.64",
                                    y2: "5.64",
                                }
                                line {
                                    x1: "18.36",
                                    y1: "18.36",
                                    x2: "19.78",
                                    y2: "19.78",
                                }
                                line {
                                    x1: "1",
                                    y1: "12",
                                    x2: "3",
                                    y2: "12",
                                }
                                line {
                                    x1: "21",
                                    y1: "12",
                                    x2: "23",
                                    y2: "12",
                                }
                                line {
                                    x1: "4.22",
                                    y1: "19.78",
                                    x2: "5.64",
                                    y2: "18.36",
                                }
                                line {
                                    x1: "18.36",
                                    y1: "5.64",
                                    x2: "19.78",
                                    y2: "4.22",
                                }
                            }

                            h3 { class: "appearance-section-title", "Theme" }
                        }
                    }
                    p { class: "appearance-section-helper",
                        "Choose how the application looks on your device."
                    }
                    div { class: "theme-icon-toggle",
                        for variant in [Theme::Dark, Theme::System, Theme::Light] {
                            {
                                let is_active = theme_ctx.0() == variant;
                                rsx! {
                                    button {
                                        r#type: "button",
                                        class: if is_active { "theme-icon-button theme-icon-button--active" } else { "theme-icon-button" },
                                        title: variant.label(),
                                        aria_pressed: if is_active { "true" } else { "false" },
                                        onclick: move |_| theme_ctx.0.set(variant),
                                        if variant == Theme::Dark {
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
                                                "aria-hidden": "true",
                                                path { d: "M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" }
                                            }
                                        } else if variant == Theme::System {
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
                                                "aria-hidden": "true",
                                                rect {
                                                    x: "2",
                                                    y: "3",
                                                    width: "20",
                                                    height: "14",
                                                    rx: "2",
                                                }
                                                line {
                                                    x1: "8",
                                                    y1: "21",
                                                    x2: "16",
                                                    y2: "21",
                                                }
                                                line {
                                                    x1: "12",
                                                    y1: "17",
                                                    x2: "12",
                                                    y2: "21",
                                                }
                                            }
                                        } else {
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
                                                "aria-hidden": "true",
                                                circle { cx: "12", cy: "12", r: "5" }
                                                line {
                                                    x1: "12",
                                                    y1: "1",
                                                    x2: "12",
                                                    y2: "3",
                                                }
                                                line {
                                                    x1: "12",
                                                    y1: "21",
                                                    x2: "12",
                                                    y2: "23",
                                                }
                                                line {
                                                    x1: "4.22",
                                                    y1: "4.22",
                                                    x2: "5.64",
                                                    y2: "5.64",
                                                }
                                                line {
                                                    x1: "18.36",
                                                    y1: "18.36",
                                                    x2: "19.78",
                                                    y2: "19.78",
                                                }
                                                line {
                                                    x1: "1",
                                                    y1: "12",
                                                    x2: "3",
                                                    y2: "12",
                                                }
                                                line {
                                                    x1: "21",
                                                    y1: "12",
                                                    x2: "23",
                                                    y2: "12",
                                                }
                                                line {
                                                    x1: "4.22",
                                                    y1: "19.78",
                                                    x2: "5.64",
                                                    y2: "18.36",
                                                }
                                                line {
                                                    x1: "18.36",
                                                    y1: "5.64",
                                                    x2: "19.78",
                                                    y2: "4.22",
                                                }
                                            }
                                        }
                                        span { class: "theme-icon-button-label", "{variant.label()}" }
                                    }
                                }
                            }
                        }
                }

                p { class: "appearance-section-helper", "Imported themes follow the mode above." }

                // ── Theme Source sub-row ─────────────────────────────────────
                div { class: "theme-source-row",
                    span { class: "appearance-control-label", "Source" }
                    div { class: "theme-source-controls",
                        if let Some(name) = (custom_theme_ctx.0)() {
                            span {
                                class: "theme-source-active",
                                "data-testid": "theme-source-active",
                                "\u{2713} {name}"
                            }
                            button {
                                r#type: "button",
                                class: "theme-reset-btn",
                                "data-testid": "theme-reset-btn",
                                "aria-label": "Switch back to the built-in default theme",
                                onclick: move |_| {
                                    clear_custom_theme();
                                    custom_theme_ctx.0.set(None);
                                    import_error.set(None);
                                    apply_theme_to_dom(theme_ctx.0());
                                },
                                "Reset to default"
                            }
                        } else {
                            span {
                                class: "theme-source-active",
                                "data-testid": "theme-source-active",
                                "\u{2713} Default"
                            }
                            label {
                                class: "theme-import-btn",
                                "Import\u{2026}"
                                input {
                                    r#type: "file",
                                    accept: ".json,application/json",
                                    "aria-label": "Import theme file (.json)",
                                    "data-testid": "theme-import-input",
                                    class: "visually-hidden",
                                    onchange: move |evt: Event<FormData>| {
                                        let theme_mode = theme_ctx.0();
                                        let mut custom_sig = custom_theme_ctx.0;
                                        let mut err_sig = import_error;
                                        let file_data = evt.files();
                                        let Some(file) = file_data.into_iter().next() else { return };
                                        spawn(async move {
                                            let contents = match file.read_string().await {
                                                Ok(s) => s,
                                                Err(_) => {
                                                    err_sig.set(Some("Could not read the file.".to_string()));
                                                    return;
                                                }
                                            };
                                            if contents.len() > MAX_THEME_JSON_BYTES {
                                                err_sig.set(Some("File is too large. Theme files must be under 64 KB.".to_string()));
                                                return;
                                            }
                                            match persist_custom_theme_json(&contents) {
                                                Ok(()) => {
                                                    let name = custom_theme_display_name().unwrap_or_else(|| "Custom Theme".to_string());
                                                    custom_sig.set(Some(name));
                                                    err_sig.set(None);
                                                    apply_theme_to_dom(theme_mode);
                                                }
                                                Err(ThemeFileError::UnsupportedVersion(v)) => {
                                                    err_sig.set(Some(format!("This theme uses version {v}, which isn't supported. Version 1 is required.")));
                                                }
                                                Err(ThemeFileError::Json(_)) => {
                                                    err_sig.set(Some("This file isn't valid JSON or has the wrong shape.".to_string()));
                                                }
                                                Err(ThemeFileError::InvalidValue) => {
                                                    err_sig.set(Some("This theme contains an unsupported color value.".to_string()));
                                                }
                                                Err(ThemeFileError::TooLarge) => {
                                                    err_sig.set(Some("File is too large. Theme files must be under 64 KB.".to_string()));
                                                }
                                                Err(ThemeFileError::StorageFull) => {
                                                    err_sig.set(Some("Storage is full \u{2014} couldn't save the theme.".to_string()));
                                                }
                                            }
                                        });
                                    },
                                }
                            }
                        }
                    }
                }

                if let Some(msg) = import_error() {
                    div {
                        class: "input-error-message",
                        role: "alert",
                        "data-testid": "theme-import-error",
                        "{msg}"
                    }
                }

                hr { class: "appearance-section-divider" }

                // ── Section 2: Speaker Highlight ─────────────────────────────────
                section { class: "appearance-section",
                    div { class: "appearance-section-header",
                        div { class: "settings-panel-title",
                            svg {
                                class: "settings-panel-title-icon",
                                xmlns: "http://www.w3.org/2000/svg",
                                width: "18",
                                height: "18",
                                view_box: "0 0 24 24",
                                fill: "none",
                                stroke: "currentColor",
                                stroke_width: "2",
                                stroke_linecap: "round",
                                stroke_linejoin: "round",
                                "aria-hidden": "true",

                                path { d: "M12 3l1.9 5.1L19 10l-5.1 1.9L12 17l-1.9-5.1L5 10l5.1-1.9L12 3z" }
                                path { d: "M5 3v4" }
                                path { d: "M3 5h4" }
                                path { d: "M19 17v4" }
                                path { d: "M17 19h4" }
                            }

                            h3 { class: "appearance-section-title", "Speaker Highlight" }
                        }

                        label { class: "glow-switch",
                            input {
                                r#type: "checkbox",
                                "aria-label": "Toggle speaker highlight",
                                checked: appearance.glow_enabled,
                                onchange: move |evt: Event<FormData>| {
                                    let enabled = evt.checked();
                                    appearance_ctx
                                        .0
                                        .set(AppearanceSettings {
                                            glow_enabled: enabled,
                                            ..appearance_ctx.0()
                                        });
                                },
                            }
                            span { class: "glow-switch-track" }
                        }
                    }

                    p { class: "appearance-section-helper", "Visual glow around the active speaker." }

                    div { class: "speaker-highlight-layout",
                        div { class: "speaker-highlight-controls",
                            div {
                                class: "appearance-control-row glow-palette-section",
                                span { class: "appearance-control-label", "Color" }
                                div { class: "appearance-control-content",
                                    div { id: "color-swatches-container", class: "color-swatches", tabindex: "-1", role: "group", "aria-label": "Speaker highlight colors",
                                        for color in palette() {
                                            {
                                                let is_selected = appearance.glow_color == color;
                                                let (select_label, delete_label) = swatch_labels(color);
                                                rsx! {
                                                    div { key: "{color.to_hex()}", class: "color-swatch-item",
                                                        button {
                                                            r#type: "button",
                                                            class: if is_selected { "color-swatch selected" } else { "color-swatch" },
                                                            style: format!("--glow-color: {}", color.to_hex()),
                                                            title: color.label(),
                                                            "aria-label": select_label,
                                                            "aria-pressed": if is_selected { "true" } else { "false" },
                                                            "aria-keyshortcuts": "Delete Backspace",
                                                            onclick: move |evt: Event<MouseData>| {
                                                                evt.stop_propagation();
                                                                palette_control.select(color);
                                                            },
                                                            onkeydown: move |evt: KeyboardEvent| {
                                                                if matches!(evt.key(), Key::Delete | Key::Backspace) {
                                                                    evt.prevent_default();
                                                                    // Focus lands on the next swatch, so a held key
                                                                    // would otherwise walk the whole row.
                                                                    if !evt.is_auto_repeating() {
                                                                        palette_control.delete(color, DeleteGesture::Key);
                                                                    }
                                                                }
                                                            },
                                                        }
                                                        button {
                                                            r#type: "button",
                                                            class: "color-swatch-delete-btn",
                                                            "aria-label": delete_label,
                                                            onclick: move |evt: Event<MouseData>| {
                                                                evt.stop_propagation();
                                                                palette_control.delete(color, DeleteGesture::Badge);
                                                            },
                                                            svg {
                                                                xmlns: "http://www.w3.org/2000/svg",
                                                                width: "10",
                                                                height: "10",
                                                                view_box: "0 0 24 24",
                                                                fill: "none",
                                                                stroke: "currentColor",
                                                                stroke_width: "3",
                                                                stroke_linecap: "round",
                                                                "aria-hidden": "true",
                                                                "focusable": "false",
                                                                line {
                                                                    x1: "6",
                                                                    y1: "6",
                                                                    x2: "18",
                                                                    y2: "18",
                                                                }
                                                                line {
                                                                    x1: "6",
                                                                    y1: "18",
                                                                    x2: "18",
                                                                    y2: "6",
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        if palette.read().len() < MAX_PALETTE_COLORS {
                                            button {
                                                id: "add-custom-color-btn",
                                                class: "color-swatch add-color-btn",
                                                r#type: "button",
                                                "aria-label": "Add custom color",
                                                title: "Add custom color",
                                                onclick: move |evt: Event<MouseData>| {
                                                    evt.stop_propagation();
                                                    // Keep the popover open/closed state local to this panel.
                                                    color_input.set(String::new());
                                                    input_error.set(false);
                                                    show_picker.set(!show_picker());
                                                },
                                                svg {
                                                    xmlns: "http://www.w3.org/2000/svg",
                                                    width: "14",
                                                    height: "14",
                                                    view_box: "0 0 24 24",
                                                    fill: "none",
                                                    stroke: "currentColor",
                                                    stroke_width: "2.5",
                                                    stroke_linecap: "round",
                                                    stroke_linejoin: "round",
                                                    "aria-hidden": "true",
                                                    line {
                                                        x1: "12",
                                                        y1: "5",
                                                        x2: "12",
                                                        y2: "19",
                                                    }
                                                    line {
                                                        x1: "5",
                                                        y1: "12",
                                                        x2: "19",
                                                        y2: "12",
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    span {
                                        class: "visually-hidden",
                                        role: "status",
                                        "aria-live": "polite",
                                        "aria-atomic": "true",
                                        "data-testid": "speaker-highlight-palette-status",
                                        {action_bar_announce_text(&announcement.read(), announcement_nonce())}
                                    }
                                    // Custom color modal dialog (centered overlay with backdrop)
                                    if show_picker() {
                                        div {
                                            class: "custom-color-modal-overlay",
                                            role: "presentation",
                                            onmousedown: move |_| {
                                                show_picker.set(false);
                                                color_input.set(String::new());
                                                input_error.set(false);
                                                focus_color_panel_fallback_deferred();
                                            },
                                            onkeydown: move |evt: KeyboardEvent| {
                                                if evt.key() == Key::Escape {
                                                    show_picker.set(false);
                                                    color_input.set(String::new());
                                                    input_error.set(false);
                                                    focus_color_panel_fallback_deferred();
                                                }
                                            },
                                            div {
                                                class: "custom-color-popover custom-color-modal",
                                                role: "dialog",
                                                "aria-modal": "true",
                                                "aria-labelledby": "custom-color-modal-title",
                                                // Make the dialog itself focusable so we can move
                                                // keyboard focus into it on open. Without this the
                                                // keydown handler below is unreachable while focus
                                                // is still on the "+" button behind the scrim
                                                // (it's a DOM sibling, not an ancestor, so Escape
                                                // never bubbles here). Mirrors the about/search
                                                // modal accessibility pattern.
                                                tabindex: "-1",
                                                onmounted: move |element| {
                                                    let element = element.data();
                                                    spawn(async move {
                                                        let _ = element.set_focus(true).await;
                                                    });
                                                },
                                                onmousedown: move |evt: Event<MouseData>| evt.stop_propagation(),
                                                onclick: move |evt: Event<MouseData>| evt.stop_propagation(),
                                                onkeydown: move |evt: KeyboardEvent| {
                                                    match evt.key() {
                                                        Key::Escape => {
                                                            show_picker.set(false);
                                                            color_input.set(String::new());
                                                            input_error.set(false);
                                                            focus_color_panel_fallback_deferred();
                                                        }
                                                        Key::Tab
                                                            if trap_tab_in_color_modal(
                                                                evt.modifiers().shift(),
                                                            ) =>
                                                        {
                                                            evt.prevent_default();
                                                        }
                                                        _ => {}
                                                    }
                                                },
                                                {
                                                    // Seed the picker's HSV state from whichever color was
                                                    // selected when the modal opened. Once mounted the
                                                    // picker owns the marker positions and writes back into
                                                    // `color_input` directly.
                                                    let initial_rgb = appearance.glow_color.to_rgb();
                                                    rsx! {
                                                        div { class: "custom-color-modal-header",
                                                            div { class: "custom-color-modal-heading",
                                                                h3 {
                                                                    id: "custom-color-modal-title",
                                                                    class: "custom-color-modal-title",
                                                                    "Choose Custom Color"
                                                                }
                                                                p { class: "custom-color-modal-subtitle",
                                                                    "Select a color for the glow highlight."
                                                                }
                                                            }
                                                            button {
                                                                class: "custom-color-modal-close",
                                                                r#type: "button",
                                                                "aria-label": "Close",
                                                                onclick: move |evt: Event<MouseData>| {
                                                                    evt.stop_propagation();
                                                                    show_picker.set(false);
                                                                    color_input.set(String::new());
                                                                    input_error.set(false);
                                                                    focus_color_panel_fallback_deferred();
                                                                },
                                                                svg {
                                                                    view_box: "0 0 24 24",
                                                                    width: "16",
                                                                    height: "16",
                                                                    "aria-hidden": "true",
                                                                    path {
                                                                        d: "M6 6L18 18M18 6L6 18",
                                                                        stroke: "currentColor",
                                                                        stroke_width: "2",
                                                                        stroke_linecap: "round",
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        div { class: "custom-color-modal-body",
                                                            HsvColorPicker {
                                                                initial_rgb,
                                                                hex_input: color_input,
                                                                input_error,
                                                            }
                                                            // Reserved 18px error slot — keep the height
                                                            // even when no error to avoid layout shift.
                                                            div {
                                                                id: "color-picker-hex-error",
                                                                class: "input-error-slot",
                                                                if input_error() {
                                                                    p {
                                                                        class: "input-error-message",
                                                                        "Invalid format - use #RRGGBB (e.g. #FF5500)" // @token-exempt: example hex in format hint
                                                                    }
                                                                }
                                                            }
                                                            div { class: "custom-color-modal-actions",
                                                                button {
                                                                    class: "custom-color-cancel-btn",
                                                                    r#type: "button",
                                                                    onclick: move |evt: Event<MouseData>| {
                                                                        evt.stop_propagation();
                                                                        show_picker.set(false);
                                                                        color_input.set(String::new());
                                                                        input_error.set(false);
                                                                        focus_color_panel_fallback_deferred();
                                                                    },
                                                                    "Cancel"
                                                                }
                                                                button {
                                                                    class: "custom-color-add-btn",
                                                                    r#type: "button",
                                                                    // Gate the Add button on the SAME lenient validator the
                                                                    // picker uses for its error state (`parse_hex`, which trims
                                                                    // whitespace and accepts a missing `#`). Using the strict
                                                                    // `GlowColor::from_hex` here — while the picker only reports
                                                                    // errors via `parse_hex` — creates a silent dead state
                                                                    // (no error message, Add greyed out) for inputs like
                                                                    // `ABCDEF` or `#FF0000 `.
                                                                    disabled: parse_hex(&color_input()).is_none(),
                                                                    onclick: move |evt: Event<MouseData>| {
                                                                        evt.stop_propagation();
                                                                        if let Some((r, g, b)) = parse_hex(&color_input()) {
                                                                            // Single source of truth: preset detection with
                                                                            // Custom fallback lives in `GlowColor::from_rgb`.
                                                                            palette_control.add(GlowColor::from_rgb(r, g, b));
                                                                            show_picker.set(false);
                                                                            color_input.set(String::new());
                                                                            input_error.set(false);
                                                                            focus_color_panel_fallback_deferred();
                                                                        } else {
                                                                            input_error.set(true);
                                                                        }
                                                                    },
                                                                    "Add"
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                } // appearance-control-content
                            } // appearance-control-row (Color)

                            div { class: "appearance-slider-row",
                                div { class: "appearance-slider-label-group",
                                    label { class: "appearance-slider-label", "Brightness" }
                                    SpeakerHighlightHelp {
                                        slug: "brightness",
                                        label: "Brightness",
                                        text: BRIGHTNESS_HELP_TEXT,
                                        open_below: true,
                                        open: help_open,
                                        suppressed: help_suppressed,
                                    }
                                }
                                input {
                                    r#type: "range",
                                    class: "appearance-slider",
                                    "data-testid": "speaker-highlight-brightness-slider",
                                    "aria-label": "Brightness",
                                    style: "{brightness_slider_style}",
                                    min: "0",
                                    max: "100",
                                    value: "{(appearance.glow_brightness * 100.0) as i32}",
                                    oninput: move |evt: Event<FormData>| {
                                        if let Ok(value) = evt.value().parse::<f32>() {
                                            appearance_ctx
                                                .0
                                                .set(AppearanceSettings {
                                                    glow_brightness: (value / 100.0).clamp(0.0, 1.0),
                                                    ..appearance_ctx.0()
                                                });
                                        }
                                    },
                                }
                                span { class: "appearance-slider-value",
                                    "{(appearance.glow_brightness * 100.0) as i32}%"
                                }
                            }

                            div { class: "appearance-slider-row",
                                div { class: "appearance-slider-label-group",
                                    label { class: "appearance-slider-label", "Glow" }
                                    SpeakerHighlightHelp {
                                        slug: "glow",
                                        label: "Glow",
                                        text: GLOW_HELP_TEXT,
                                        open_below: false,
                                        open: help_open,
                                        suppressed: help_suppressed,
                                    }
                                }
                                input {
                                    r#type: "range",
                                    class: "appearance-slider",
                                    "data-testid": "speaker-highlight-glow-slider",
                                    "aria-label": "Glow",
                                    style: "{inner_slider_style}",
                                    min: "0",
                                    max: "100",
                                    value: "{(appearance.inner_glow_strength * 100.0) as i32}",
                                    oninput: move |evt: Event<FormData>| {
                                        if let Ok(value) = evt.value().parse::<f32>() {
                                            appearance_ctx
                                                .0
                                                .set(AppearanceSettings {
                                                    inner_glow_strength: (value / 100.0).clamp(0.0, 1.0),
                                                    ..appearance_ctx.0()
                                                });
                                        }
                                    },
                                }
                                span { class: "appearance-slider-value",
                                    "{(appearance.inner_glow_strength * 100.0) as i32}%"
                                }
                            }

                            div { class: "appearance-slider-row",
                                div { class: "appearance-slider-label-group",
                                    label { class: "appearance-slider-label", "Velocity" }
                                    SpeakerHighlightHelp {
                                        slug: "velocity",
                                        label: "Velocity",
                                        text: VELOCITY_HELP_TEXT,
                                        open_below: false,
                                        open: help_open,
                                        suppressed: help_suppressed,
                                    }
                                }
                                input {
                                    r#type: "range",
                                    class: "appearance-slider",
                                    "data-testid": "speaker-highlight-velocity-slider",
                                    "aria-label": "Velocity",
                                    style: "{velocity_slider_style}",
                                    min: "0",
                                    max: "100",
                                    value: "{(appearance.glow_velocity * 100.0) as i32}",
                                    oninput: move |evt: Event<FormData>| {
                                        if let Ok(value) = evt.value().parse::<f32>() {
                                            appearance_ctx
                                                .0
                                                .set(AppearanceSettings {
                                                    glow_velocity: (value / 100.0).clamp(0.0, 1.0),
                                                    ..appearance_ctx.0()
                                                });
                                        }
                                    },
                                }
                                span { class: "appearance-slider-value",
                                    "{(appearance.glow_velocity * 100.0) as i32}%"
                                }
                            }

                            div { class: "appearance-slider-row",
                                div { class: "appearance-slider-label-group",
                                    label { class: "appearance-slider-label", "Decay" }
                                    SpeakerHighlightHelp {
                                        slug: "decay",
                                        label: "Decay",
                                        text: DECAY_HELP_TEXT,
                                        open_below: false,
                                        open: help_open,
                                        suppressed: help_suppressed,
                                    }
                                }
                                input {
                                    r#type: "range",
                                    class: "appearance-slider",
                                    "data-testid": "speaker-highlight-decay-slider",
                                    "aria-label": "Decay",
                                    style: "{decay_slider_style}",
                                    min: "0",
                                    max: "100",
                                    value: "{(appearance.glow_decay * 100.0) as i32}",
                                    oninput: move |evt: Event<FormData>| {
                                        if let Ok(value) = evt.value().parse::<f32>() {
                                            appearance_ctx
                                                .0
                                                .set(AppearanceSettings {
                                                    glow_decay: (value / 100.0).clamp(0.0, 1.0),
                                                    ..appearance_ctx.0()
                                                });
                                        }
                                    },
                                }
                                span { class: "appearance-slider-value",
                                    "{(appearance.glow_decay * 100.0) as i32}%"
                                }
                            }

                            div { class: "appearance-slider-row",
                                span { class: "appearance-slider-label", "" }
                                button {
                                    r#type: "button",
                                    class: "theme-reset-btn",
                                    "data-testid": "speaker-highlight-reset-btn",
                                    "aria-label": "Reset speaker highlight settings and colors to defaults",
                                    onclick: move |_| palette_control.reset(),
                                    "Reset highlight"
                                }
                                span { class: "appearance-slider-value", "" }
                            }
                        } // speaker-highlight-controls

                        div { class: "speaker-highlight-preview",
                            SpeakerHighlightPreview { settings: appearance }
                        }
                    }
            }
                }
            }
        }
    }
}

/// Dedicated child component for the speaker-highlight preview tile.
///
/// Owns the audio-driven frame so that per-tick re-renders are scoped to the
/// preview subtree and do not re-render the parent panel.
#[component]
fn SpeakerHighlightPreview(settings: AppearanceSettings) -> Element {
    let mic_level = try_use_context::<LocalAudioLevelCtx>().map(|ctx| ctx.0);
    let mic_speaking = try_use_context::<LocalSpeakingCtx>().map(|ctx| ctx.0);
    let reduced_motion = use_hook(prefers_reduced_motion);
    let decay = use_hook(|| Rc::new(Cell::new(settings.glow_decay)));
    decay.set(settings.glow_decay);
    let glow_enabled = use_hook(|| Rc::new(Cell::new(settings.glow_enabled)));
    glow_enabled.set(settings.glow_enabled);
    let remote = use_hook(|| Rc::new(RefCell::new(RemoteSpeakers::default())));
    let mut frame = use_signal(|| PreviewFrame::SIMULATED_LIT);
    let mut caption = use_signal(|| PreviewSource::Simulated);

    let bus_remote = remote.clone();
    use_future(move || {
        let remote = bus_remote.clone();
        async move {
            let mut rx = subscribe();
            loop {
                let evt = match rx.recv().await {
                    Ok(evt) => evt,
                    Err(e) => match recv_loop_action(&e) {
                        RecvLoopAction::Continue => continue,
                        RecvLoopAction::Break => break,
                    },
                };
                if is_speech_event(&evt) {
                    remote.borrow_mut().observe(&evt, monotonic_now_ms());
                }
            }
        }
    });

    let tick_remote = remote.clone();
    let tick_decay = decay.clone();
    let tick_glow_enabled = glow_enabled.clone();
    use_future(move || {
        let remote = tick_remote.clone();
        let decay = tick_decay.clone();
        let glow_enabled = tick_glow_enabled.clone();
        async move {
            let mut clock = PreviewClock::new(monotonic_now_ms(), reduced_motion);
            let mut captioner = PreviewCaption::default();
            loop {
                gloo_timers::future::TimeoutFuture::new(PREVIEW_TICK_MS).await;
                let now_ms = monotonic_now_ms();
                let mic = mic_speaking.zip(mic_level).and_then(|(speaking, level)| {
                    live_mic_level(*speaking.try_peek().ok()?, *level.try_peek().ok()?)
                });
                let remote_level = remote.borrow_mut().level(now_ms);
                let next = clock.frame(mic, remote_level, decay.get(), now_ms);
                let next_caption = captioner.source(next.source, now_ms);
                if *caption.peek() != next_caption {
                    caption.set(next_caption);
                }
                if glow_enabled.get() && *frame.peek() != next {
                    frame.set(next);
                }
            }
        }
    });

    let current = frame();
    let preview_style = preview_tile_style(&current, &settings, reduced_motion);

    rsx! {
        div {
            class: preview_tile_class(&current),
            style: "{preview_style}",
            "data-preview-source": current.source.as_str(),
            svg {
                xmlns: "http://www.w3.org/2000/svg",
                view_box: "0 0 120 120",
                width: "100%",
                height: "100%",
                style: "pointer-events: none; display: block;",
                // Head
                circle {
                    cx: "60",
                    cy: "44",
                    r: "20",
                    fill: "{theme_color::PREVIEW_AVATAR_BG}",
                }
                // Shoulders / torso
                path {
                    d: "M20 120 C20 86, 38 70, 60 70 C82 70, 100 86, 100 120 Z",
                    fill: "{theme_color::PREVIEW_AVATAR_BG}",
                }
            }
        }
        p {
            class: "speaker-highlight-preview-caption",
            "data-testid": "speaker-highlight-preview-caption",
            {preview_caption(caption())}
        }
    }
}

/// Detect whether the user has requested reduced motion.
fn prefers_reduced_motion() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }

    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|window| window.match_media("(prefers-reduced-motion: reduce)").ok())
            .flatten()
            .map(|media_query| media_query.matches())
            .unwrap_or(false)
    }
}

/// Duration of the "speaking burst" phase in the preview animation (ms).
const PREVIEW_SPEAKING_MS: u32 = 900;
/// Minimum silent phase so the cycle doesn't spin too fast at 0% decay.
const PREVIEW_SILENT_MIN_MS: u32 = 400;
const PREVIEW_TICK_MS: u32 = 100;
const PREVIEW_SIMULATED_LEVEL: f32 = 0.55;
const PREVIEW_LEVEL_STEPS: f32 = 20.0;
const PREVIEW_REMOTE_TTL_MS: f64 = glow_deadman_ms(HEARTBEAT_KEEPALIVE_INTERVAL_MS) as f64;
const CAPTION_SIMULATED_AFTER_MS: f64 = 2000.0;

/// Compute the silent phase duration (ms) for the preview animation cycle.
///
/// Longer decay → longer visible tail → more silent time needed to perceive it.
/// The silent phase is hold + fade + a small minimum baseline.
fn preview_silent_duration_ms(decay: f32) -> u32 {
    let (fade_out, hold) = glow_tail_seconds(decay);
    let tail_ms = ((hold + fade_out) * 1000.0) as u32;
    PREVIEW_SILENT_MIN_MS + tail_ms
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum PreviewSource {
    Mic,
    Remote,
    Simulated,
}

impl PreviewSource {
    fn as_str(self) -> &'static str {
        match self {
            PreviewSource::Mic => "mic",
            PreviewSource::Remote => "remote",
            PreviewSource::Simulated => "simulated",
        }
    }
}

fn preview_caption(source: PreviewSource) -> &'static str {
    match source {
        PreviewSource::Mic => "Preview: your microphone",
        PreviewSource::Remote => "Preview: someone speaking",
        PreviewSource::Simulated => "Preview: simulated speaker",
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct PreviewFrame {
    source: PreviewSource,
    level: f32,
    speaking: bool,
}

impl PreviewFrame {
    const SIMULATED_LIT: Self = Self {
        source: PreviewSource::Simulated,
        level: PREVIEW_SIMULATED_LEVEL,
        speaking: true,
    };
}

fn preview_tile_class(frame: &PreviewFrame) -> &'static str {
    match (frame.source, frame.speaking) {
        (PreviewSource::Simulated, true) => {
            "preview-tile preview-tile-pulsing preview-tile--speaking"
        }
        (PreviewSource::Simulated, false) => {
            "preview-tile preview-tile-pulsing preview-tile--silent"
        }
        (_, true) => "preview-tile preview-tile--speaking",
        (_, false) => "preview-tile preview-tile--silent",
    }
}

/// The self-tile glow's own gate (`speak_style`): a level the encoder left
/// behind after its VAD went quiet must not keep the preview lit.
fn live_mic_level(speaking: bool, level: f32) -> Option<f32> {
    (speaking && level > 0.0).then_some(level)
}

/// Snap a live level to coarse steps so speech jitter does not rewrite the
/// tile's style every tick. The floor keeps a faint speaker lit.
fn quantize_level(level: f32) -> f32 {
    ((level * PREVIEW_LEVEL_STEPS).round() / PREVIEW_LEVEL_STEPS)
        .clamp(1.0 / PREVIEW_LEVEL_STEPS, 1.0)
}

fn is_speech_event(evt: &DiagEvent) -> bool {
    matches!(evt.subsystem, "peer_speaking" | "peer_status")
}

#[derive(Default)]
struct RemoteSpeakers {
    heard: HashMap<String, (f32, f64)>,
}

impl RemoteSpeakers {
    /// Only the decoder VAD's `peer_speaking` refreshes a speaker: a keepalive
    /// heartbeat re-sends a cached `is_speaking`, so a latched `true` would
    /// keep the TTL armed indefinitely. Either subsystem can silence one.
    fn observe(&mut self, evt: &DiagEvent, now_ms: f64) {
        if !is_speech_event(evt) {
            return;
        }
        let mut peer = None;
        let mut level = 0.0_f32;
        let mut speaking = None;
        let mut audio_enabled = None;
        for metric in &evt.metrics {
            match (metric.name, &metric.value) {
                ("to_peer", MetricValue::Text(p)) => peer = Some(p.as_ref()),
                ("audio_level", MetricValue::F64(v)) => level = *v as f32,
                ("speaking" | "is_speaking", MetricValue::U64(v)) => speaking = Some(*v != 0),
                ("audio_enabled", MetricValue::U64(v)) => audio_enabled = Some(*v != 0),
                _ => {}
            }
        }
        let Some(peer) = peer else {
            return;
        };
        if audio_enabled == Some(false) || speaking == Some(false) {
            self.heard.remove(peer);
        } else if evt.subsystem == "peer_speaking" && speaking == Some(true) && level > 0.0 {
            match self.heard.get_mut(peer) {
                Some(heard) => *heard = (level, now_ms),
                None => {
                    self.heard.insert(peer.to_string(), (level, now_ms));
                }
            }
        }
    }

    fn level(&mut self, now_ms: f64) -> Option<f32> {
        self.heard
            .retain(|_, (_, heard_at)| now_ms - *heard_at < PREVIEW_REMOTE_TTL_MS);
        self.heard
            .values()
            .map(|(level, _)| *level)
            .reduce(f32::max)
    }
}

#[derive(Clone, Copy, Debug)]
struct PreviewClock {
    last_real: Option<(PreviewSource, f64)>,
    sim_speaking: bool,
    sim_phase_ends_ms: f64,
    reduced_motion: bool,
}

impl PreviewClock {
    fn new(now_ms: f64, reduced_motion: bool) -> Self {
        Self {
            last_real: None,
            sim_speaking: true,
            sim_phase_ends_ms: now_ms + f64::from(PREVIEW_SPEAKING_MS),
            reduced_motion,
        }
    }

    /// Mic outranks remote, and either pre-empts the simulation at once. The
    /// simulation waits out the same silent phase it uses itself, so the real
    /// decay tail plays and a pause between words does not flash a fake burst.
    /// Reduced motion stops only the simulated cycle: real sound still drives.
    fn frame(
        &mut self,
        mic_level: Option<f32>,
        remote_level: Option<f32>,
        decay: f32,
        now_ms: f64,
    ) -> PreviewFrame {
        let real = mic_level
            .map(|level| (PreviewSource::Mic, level))
            .or(remote_level.map(|level| (PreviewSource::Remote, level)));
        if let Some((source, level)) = real {
            self.last_real = Some((source, now_ms));
            return PreviewFrame {
                source,
                level: quantize_level(level),
                speaking: true,
            };
        }
        if let Some((source, heard_at)) = self.last_real {
            if now_ms - heard_at < f64::from(preview_silent_duration_ms(decay)) {
                return PreviewFrame {
                    source,
                    level: 0.0,
                    speaking: false,
                };
            }
            *self = Self::new(now_ms, self.reduced_motion);
        }
        if self.reduced_motion {
            return PreviewFrame::SIMULATED_LIT;
        }
        if now_ms >= self.sim_phase_ends_ms {
            self.sim_speaking = !self.sim_speaking;
            let phase_ms = if self.sim_speaking {
                PREVIEW_SPEAKING_MS
            } else {
                preview_silent_duration_ms(decay)
            };
            self.sim_phase_ends_ms = now_ms + f64::from(phase_ms);
        }
        PreviewFrame {
            source: PreviewSource::Simulated,
            level: if self.sim_speaking {
                PREVIEW_SIMULATED_LEVEL
            } else {
                0.0
            },
            speaking: self.sim_speaking,
        }
    }
}

struct PreviewCaption {
    shown: PreviewSource,
    simulated_since: Option<f64>,
}

impl Default for PreviewCaption {
    fn default() -> Self {
        Self {
            shown: PreviewSource::Simulated,
            simulated_since: None,
        }
    }
}

impl PreviewCaption {
    /// A real source shows at once; "simulated" waits so the caption does not
    /// flicker in the pauses between words.
    fn source(&mut self, frame_source: PreviewSource, now_ms: f64) -> PreviewSource {
        if frame_source != PreviewSource::Simulated {
            self.shown = frame_source;
            self.simulated_since = None;
        } else if self.shown != PreviewSource::Simulated {
            let since = *self.simulated_since.get_or_insert(now_ms);
            if now_ms - since >= CAPTION_SIMULATED_AFTER_MS {
                self.shown = PreviewSource::Simulated;
                self.simulated_since = None;
            }
        }
        self.shown
    }
}

/// The tile glow for this frame. A real source keeps the tiles' transition,
/// whose decay hold hides the gaps between words; only the simulated frame
/// drops it under reduced motion.
fn preview_tile_style(
    frame: &PreviewFrame,
    settings: &AppearanceSettings,
    reduced_motion: bool,
) -> String {
    let style = speak_style(frame.level, frame.speaking, settings);
    if reduced_motion && frame.source == PreviewSource::Simulated {
        format!("{style} transition: none;")
    } else {
        style
    }
}

/// Emit the inline CSS custom property used by `.appearance-slider` to draw
/// the filled portion of the track.
///
/// The slider track is rendered as a layered background in CSS: a luminous
/// active gradient (`--appearance-slider-fill-soft` → `-fill-bright` →
/// `-fill-spill`) layered on top of the dim base `--appearance-slider-track`,
/// with the bright peak anchored at `--fill` (a percentage). The fill is
/// intentionally NOT derived from the swatch color — the track stays
/// neutral so the floating light particle thumb remains the focal element.
fn slider_fill_style(value_0_1: f32) -> String {
    let pct = (value_0_1.clamp(0.0, 1.0) * 100.0).round() as i32;
    format!("--fill: {pct}%;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::PRESET_GLOW_COLORS;

    #[test]
    fn help_class_resting_state_carries_no_modifier() {
        // The CSS reveal + suppression rules are all keyed off this base class.
        let class = help_class("decay", None, None);
        assert!(class.contains("speaker-highlight-help-icon"));
        assert!(
            !class.contains("--open"),
            "unexpected open modifier: {class}"
        );
        assert!(
            !class.contains("--suppressed"),
            "unexpected suppressed modifier: {class}"
        );
    }

    #[test]
    fn help_class_latched_open_adds_open_modifier() {
        let class = help_class("decay", Some("decay"), None);
        assert!(
            class.contains("speaker-highlight-help-icon--open"),
            "tap/click latch must force the tooltip visible: {class}"
        );
        assert!(!class.contains("--suppressed"));
    }

    #[test]
    fn help_class_ignores_another_sliders_keys() {
        let class = help_class("brightness", Some("glow"), Some("decay"));
        assert!(!class.contains("--open"), "{class}");
        assert!(!class.contains("--suppressed"), "{class}");
    }

    #[test]
    fn help_toggling_on_opens_and_clears_suppression() {
        // An explicit open must beat a prior Escape-dismissal.
        assert_eq!(next_help_state("decay", None), (Some("decay"), None));
    }

    #[test]
    fn opening_one_slider_help_closes_the_others() {
        let (open, suppressed) = next_help_state("glow", Some("brightness"));
        assert_eq!((open, suppressed), (Some("glow"), None));
        assert!(
            !help_class("brightness", open, suppressed).contains("--open"),
            "the previously open trigger must lose its latch"
        );
        assert!(help_class("glow", open, suppressed).contains("--open"));
    }

    #[test]
    fn help_toggling_off_latches_suppression() {
        // Return `(None, None)` here and the bubble survives its own dismissal.
        assert_eq!(
            next_help_state("decay", Some("decay")),
            (None, Some("decay"))
        );
    }

    #[test]
    fn help_toggle_off_renders_the_suppressed_modifier() {
        // End-to-end through the production class builder: the state the
        // off-toggle produces must be the state CSS keys its hide rule off.
        let (open, suppressed) = next_help_state("decay", Some("decay"));
        let class = help_class("decay", open, suppressed);
        assert!(
            class.contains("speaker-highlight-help-icon--suppressed"),
            "a second tap must render the suppressed modifier, since only that \
             rule out-specifies `:focus-within`: {class}"
        );
    }

    #[test]
    fn help_toggle_round_trip_returns_to_a_revealing_state() {
        let (open_1, suppressed_1) = next_help_state("velocity", None);
        assert_eq!((open_1, suppressed_1), (Some("velocity"), None));
        let (open_2, suppressed_2) = next_help_state("velocity", open_1);
        assert_eq!((open_2, suppressed_2), (None, Some("velocity")));
        let (open_3, suppressed_3) = next_help_state("velocity", open_2);
        assert_eq!(
            (open_3, suppressed_3),
            (Some("velocity"), None),
            "re-tapping a suppressed trigger must reopen it"
        );
        assert!(help_class("velocity", open_3, suppressed_3)
            .contains("speaker-highlight-help-icon--open"));
    }

    #[test]
    fn help_class_escape_suppression_wins_over_open() {
        let class = help_class("decay", Some("decay"), Some("decay"));
        assert!(
            class.contains("speaker-highlight-help-icon--suppressed"),
            "suppressed state must be reflected in the class: {class}"
        );
        assert!(
            !class.contains("--open"),
            "an Escape-suppressed trigger must not also be latched open: {class}"
        );
    }

    #[test]
    fn every_slider_help_text_explains_both_endpoints() {
        for text in [
            BRIGHTNESS_HELP_TEXT,
            GLOW_HELP_TEXT,
            VELOCITY_HELP_TEXT,
            DECAY_HELP_TEXT,
        ] {
            assert!(text.contains("0%"), "{text}");
            assert!(text.contains("100%"), "{text}");
        }
    }

    #[test]
    fn preview_silent_duration_zero_decay_is_short() {
        let ms = preview_silent_duration_ms(0.0);
        // 0% decay → instant off, so silent phase is just the minimum baseline.
        assert_eq!(ms, PREVIEW_SILENT_MIN_MS);
    }

    #[test]
    fn preview_silent_duration_full_decay_is_longer() {
        let ms_zero = preview_silent_duration_ms(0.0);
        let ms_full = preview_silent_duration_ms(1.0);
        // 100% decay yields a noticeably longer silent phase than 0%.
        assert!(
            ms_full > ms_zero + 3000,
            "full decay silent ({ms_full}ms) should be >3s longer than zero ({ms_zero}ms)"
        );
    }

    #[test]
    fn preview_silent_duration_mid_decay_between_extremes() {
        let ms_zero = preview_silent_duration_ms(0.0);
        let ms_mid = preview_silent_duration_ms(0.5);
        let ms_full = preview_silent_duration_ms(1.0);
        assert!(ms_mid > ms_zero);
        assert!(ms_mid < ms_full);
    }

    const T0: f64 = 1_000_000.0;

    fn silent(source: PreviewSource) -> PreviewFrame {
        PreviewFrame {
            source,
            level: 0.0,
            speaking: false,
        }
    }

    fn lit(source: PreviewSource, level: f32) -> PreviewFrame {
        PreviewFrame {
            source,
            level,
            speaking: true,
        }
    }

    fn channels(color: GlowColor) -> String {
        let (r, g, b) = color.to_rgb();
        format!("({r}, {g}, {b}, ")
    }

    #[test]
    fn preview_style_paints_the_chosen_color_while_lit() {
        for color in [GlowColor::Cyan, GlowColor::from_rgb(1, 2, 3)] {
            let settings = AppearanceSettings {
                glow_color: color,
                ..AppearanceSettings::default()
            };
            let style = preview_tile_style(&PreviewFrame::SIMULATED_LIT, &settings, false);
            assert!(style.contains("box-shadow: 0 0 "), "{style}");
            assert!(
                style.contains(&format!("border-color: rgba{}", channels(color))),
                "{style}"
            );
        }
    }

    #[test]
    fn preview_style_is_the_tile_glow_for_every_frame() {
        let settings_cases = [
            AppearanceSettings::default(),
            AppearanceSettings {
                glow_color: GlowColor::Plum,
                glow_brightness: 1.0,
                inner_glow_strength: 1.0,
                glow_decay: 1.0,
                glow_velocity: 0.0,
                ..AppearanceSettings::default()
            },
            AppearanceSettings {
                inner_glow_strength: 0.0,
                ..AppearanceSettings::default()
            },
            AppearanceSettings {
                glow_enabled: false,
                ..AppearanceSettings::default()
            },
        ];
        let frames = [
            PreviewFrame::SIMULATED_LIT,
            lit(PreviewSource::Mic, 0.2),
            lit(PreviewSource::Remote, 1.0),
            silent(PreviewSource::Mic),
        ];
        for settings in settings_cases {
            for frame in frames {
                assert_eq!(
                    preview_tile_style(&frame, &settings, false),
                    speak_style(frame.level, frame.speaking, &settings),
                    "{frame:?} {settings:?}"
                );
            }
        }
    }

    #[test]
    fn preview_silent_style_holds_then_fades_with_decay() {
        let at = |decay: f32| {
            preview_tile_style(
                &silent(PreviewSource::Simulated),
                &AppearanceSettings {
                    glow_decay: decay,
                    ..AppearanceSettings::default()
                },
                false,
            )
        };
        assert!(
            at(0.5).contains("box-shadow 1.50s ease-out 1.00s"),
            "{}",
            at(0.5)
        );
        assert!(
            at(0.0).contains("box-shadow 0.00s ease-out 0.00s"),
            "{}",
            at(0.0)
        );
        assert!(
            at(1.0).contains("box-shadow 1.50s ease-out 5.00s"),
            "{}",
            at(1.0)
        );
    }

    #[test]
    fn preview_lit_style_fade_in_follows_velocity() {
        let at = |velocity: f32| {
            preview_tile_style(
                &PreviewFrame::SIMULATED_LIT,
                &AppearanceSettings {
                    glow_velocity: velocity,
                    ..AppearanceSettings::default()
                },
                false,
            )
        };
        assert!(at(0.0).contains("box-shadow 0.45s ease-in"), "{}", at(0.0));
        assert!(at(0.5).contains("box-shadow 0.15s ease-in"), "{}", at(0.5));
        assert!(at(1.0).contains("box-shadow 0.03s ease-in"), "{}", at(1.0));
    }

    #[test]
    fn preview_velocity_leaves_the_decay_tail_alone() {
        for velocity in [0.0, 1.0] {
            let style = preview_tile_style(
                &silent(PreviewSource::Remote),
                &AppearanceSettings {
                    glow_velocity: velocity,
                    glow_decay: 0.5,
                    ..AppearanceSettings::default()
                },
                false,
            );
            assert!(style.contains("box-shadow 1.50s ease-out 1.00s"), "{style}");
        }
    }

    #[test]
    fn preview_reduced_motion_overrides_the_inline_transition() {
        let settings = AppearanceSettings::default();
        let reduced = preview_tile_style(&PreviewFrame::SIMULATED_LIT, &settings, true);
        assert!(reduced.ends_with(" transition: none;"), "{reduced}");
        let full = preview_tile_style(&PreviewFrame::SIMULATED_LIT, &settings, false);
        assert!(!full.contains("transition: none"), "{full}");
    }

    #[test]
    fn preview_glow_disabled_renders_no_glow_even_while_lit() {
        let style = preview_tile_style(
            &lit(PreviewSource::Mic, 1.0),
            &AppearanceSettings {
                glow_enabled: false,
                ..AppearanceSettings::default()
            },
            false,
        );
        assert!(style.contains("box-shadow: none;"), "{style}");
        assert!(
            !style.contains(&channels(AppearanceSettings::default().glow_color)),
            "{style}"
        );
    }

    #[test]
    fn preview_simulation_alternates_speaking_and_decay_sized_silence() {
        let decay = 0.5;
        let mut clock = PreviewClock::new(T0, false);
        assert_eq!(
            clock.frame(None, None, decay, T0),
            PreviewFrame::SIMULATED_LIT
        );
        let speak_end = T0 + f64::from(PREVIEW_SPEAKING_MS);
        assert!(clock.frame(None, None, decay, speak_end - 1.0).speaking);
        assert_eq!(
            clock.frame(None, None, decay, speak_end),
            silent(PreviewSource::Simulated)
        );
        let silent_end = speak_end + f64::from(preview_silent_duration_ms(decay));
        assert!(!clock.frame(None, None, decay, silent_end - 1.0).speaking);
        assert_eq!(
            clock.frame(None, None, decay, silent_end),
            PreviewFrame::SIMULATED_LIT
        );
    }

    #[test]
    fn preview_real_sound_preempts_the_simulation_immediately() {
        let mut clock = PreviewClock::new(T0, false);
        let silent_phase = T0 + f64::from(PREVIEW_SPEAKING_MS);
        assert!(!clock.frame(None, None, 0.5, silent_phase).speaking);
        assert_eq!(
            clock.frame(Some(0.7), None, 0.5, silent_phase + 1.0),
            lit(PreviewSource::Mic, 0.7)
        );
        let mut clock = PreviewClock::new(T0, false);
        assert_eq!(
            clock.frame(None, Some(0.4), 0.5, T0 + 1.0),
            lit(PreviewSource::Remote, 0.4)
        );
    }

    #[test]
    fn preview_mic_outranks_remote() {
        assert_eq!(
            PreviewClock::new(T0, false).frame(Some(0.3), Some(0.9), 0.5, T0),
            lit(PreviewSource::Mic, 0.3)
        );
    }

    #[test]
    fn preview_mic_lights_only_while_its_vad_reports_speech() {
        assert_eq!(live_mic_level(true, 0.4), Some(0.4));
        assert_eq!(live_mic_level(false, 0.01), None);
        assert_eq!(live_mic_level(true, 0.0), None);
        assert_eq!(live_mic_level(false, 0.0), None);
    }

    #[test]
    fn preview_simulation_resumes_only_after_the_decay_tail() {
        for decay in [0.0, 0.5, 1.0] {
            let (fade_out, hold) = glow_tail_seconds(decay);
            let resume_at = T0 + f64::from(preview_silent_duration_ms(decay));
            assert!(resume_at - T0 >= f64::from(hold + fade_out) * 1000.0);
            let mut clock = PreviewClock::new(T0, false);
            assert_eq!(
                clock.frame(Some(0.6), None, decay, T0),
                lit(PreviewSource::Mic, 0.6)
            );
            assert_eq!(
                clock.frame(None, None, decay, resume_at - 1.0),
                silent(PreviewSource::Mic),
                "decay {decay}"
            );
            assert_eq!(
                clock.frame(None, None, decay, resume_at),
                PreviewFrame::SIMULATED_LIT,
                "decay {decay}"
            );
        }
    }

    #[test]
    fn preview_sound_returning_during_the_tail_relights_without_simulating() {
        let mut clock = PreviewClock::new(T0, false);
        let _ = clock.frame(None, Some(0.5), 1.0, T0);
        assert_eq!(
            clock.frame(None, None, 1.0, T0 + 2000.0),
            silent(PreviewSource::Remote)
        );
        assert_eq!(
            clock.frame(None, Some(0.8), 1.0, T0 + 2100.0),
            lit(PreviewSource::Remote, 0.8)
        );
        assert_eq!(
            clock.frame(None, None, 1.0, T0 + 2200.0),
            silent(PreviewSource::Remote)
        );
    }

    fn speaking_event(peer: &str, speaking: u64, level: f64) -> DiagEvent {
        DiagEvent {
            subsystem: "peer_speaking",
            stream_id: Some(format!("speaking->{peer}")),
            ts_ms: 0,
            metrics: vec![
                videocall_diagnostics::metric!("to_peer", peer.to_string()),
                videocall_diagnostics::metric!("speaking", speaking),
                videocall_diagnostics::metric!("audio_level", level),
            ],
        }
    }

    fn status_event(peer: &str, audio_enabled: u64, is_speaking: u64) -> DiagEvent {
        DiagEvent {
            subsystem: "peer_status",
            stream_id: None,
            ts_ms: 0,
            metrics: vec![
                videocall_diagnostics::metric!("to_peer", peer.to_string()),
                videocall_diagnostics::metric!("audio_enabled", audio_enabled),
                videocall_diagnostics::metric!("is_speaking", is_speaking),
                videocall_diagnostics::metric!("audio_level", 0.5f64),
            ],
        }
    }

    #[test]
    fn remote_speaker_lights_until_the_ttl_runs_out() {
        let mut remote = RemoteSpeakers::default();
        remote.observe(&speaking_event("bob", 1, 0.6), T0);
        assert_eq!(remote.level(T0 + PREVIEW_REMOTE_TTL_MS - 1.0), Some(0.6));
        assert_eq!(remote.level(T0 + PREVIEW_REMOTE_TTL_MS), None);
    }

    #[test]
    fn remote_ttl_is_refreshed_by_decoded_speech() {
        let mut remote = RemoteSpeakers::default();
        remote.observe(&speaking_event("bob", 1, 0.6), T0);
        remote.observe(&speaking_event("bob", 1, 0.3), T0 + 5000.0);
        assert_eq!(remote.level(T0 + PREVIEW_REMOTE_TTL_MS + 1000.0), Some(0.3));
    }

    #[test]
    fn remote_terminal_zero_silences_the_speaker() {
        let mut remote = RemoteSpeakers::default();
        remote.observe(&speaking_event("bob", 1, 0.6), T0);
        remote.observe(&speaking_event("bob", 0, 0.0), T0 + 10.0);
        assert_eq!(remote.level(T0 + 20.0), None);
    }

    #[test]
    fn remote_heartbeat_neither_lights_nor_refreshes_a_speaker() {
        let mut remote = RemoteSpeakers::default();
        remote.observe(&status_event("bob", 1, 1), T0);
        assert_eq!(remote.level(T0), None);
        remote.observe(&speaking_event("bob", 1, 0.6), T0);
        remote.observe(
            &status_event("bob", 1, 1),
            T0 + PREVIEW_REMOTE_TTL_MS - 10.0,
        );
        assert_eq!(remote.level(T0 + PREVIEW_REMOTE_TTL_MS), None);
    }

    #[test]
    fn remote_mute_or_quiet_heartbeat_silences_the_speaker() {
        let mut remote = RemoteSpeakers::default();
        remote.observe(&speaking_event("bob", 1, 0.6), T0);
        remote.observe(&status_event("bob", 0, 1), T0 + 10.0);
        assert_eq!(remote.level(T0 + 20.0), None);
        remote.observe(&speaking_event("bob", 1, 0.6), T0 + 30.0);
        remote.observe(&status_event("bob", 1, 0), T0 + 40.0);
        assert_eq!(remote.level(T0 + 50.0), None);
    }

    #[test]
    fn remote_level_is_the_loudest_live_speaker() {
        let mut remote = RemoteSpeakers::default();
        remote.observe(&speaking_event("bob", 1, 0.3), T0);
        remote.observe(&speaking_event("carol", 1, 0.8), T0);
        assert_eq!(remote.level(T0 + 1.0), Some(0.8));
        remote.observe(&speaking_event("carol", 0, 0.0), T0 + 2.0);
        assert_eq!(remote.level(T0 + 3.0), Some(0.3));
    }

    const CORAL: GlowColor = GlowColor::Custom {
        r: 0xff,
        g: 0x57,
        b: 0x33,
    };

    #[test]
    fn every_preset_swatch_is_deletable() {
        let palette = default_glow_palette();
        for preset in PRESET_GLOW_COLORS {
            let deletion = delete_from_palette(&palette, preset, GlowColor::MintGreen)
                .expect("every palette entry can be deleted");
            assert!(!deletion.palette.contains(&preset));
            assert_eq!(deletion.palette.len(), palette.len() - 1);
        }
    }

    #[test]
    fn deleting_the_selected_swatch_selects_the_one_that_slides_in() {
        let palette = [GlowColor::White, GlowColor::Cyan, GlowColor::Magenta];
        assert_eq!(
            delete_from_palette(&palette, GlowColor::Cyan, GlowColor::Cyan),
            Some(PaletteDeletion {
                palette: vec![GlowColor::White, GlowColor::Magenta],
                removed_idx: 1,
                selection: GlowColor::Magenta,
            })
        );
    }

    #[test]
    fn deleting_the_selected_last_swatch_selects_the_previous_one() {
        let palette = [GlowColor::White, CORAL];
        let deletion = delete_from_palette(&palette, CORAL, CORAL).unwrap();
        assert_eq!(deletion.selection, GlowColor::White);
        assert_eq!(deletion.removed_idx, 1);
    }

    #[test]
    fn deleting_an_unselected_swatch_keeps_the_selection() {
        let palette = default_glow_palette();
        let deletion = delete_from_palette(&palette, GlowColor::White, GlowColor::Plum).unwrap();
        assert_eq!(deletion.selection, GlowColor::Plum);
    }

    #[test]
    fn deleting_the_only_swatch_keeps_the_glow_color() {
        let deletion =
            delete_from_palette(&[GlowColor::Cyan], GlowColor::Cyan, GlowColor::Cyan).unwrap();
        assert!(deletion.palette.is_empty());
        assert_eq!(deletion.selection, GlowColor::Cyan);
        assert_eq!(delete_neighbor_index(0, deletion.removed_idx), None);
    }

    #[test]
    fn delete_focus_follows_the_selection_rule() {
        assert_eq!(delete_neighbor_index(3, 1), Some(1));
        assert_eq!(delete_neighbor_index(2, 2), Some(1));
        assert_eq!(delete_neighbor_index(1, 0), Some(0));
        assert_eq!(delete_neighbor_index(0, 0), None);
    }

    #[test]
    fn deleting_a_color_absent_from_the_palette_is_a_no_op() {
        assert_eq!(
            delete_from_palette(&[GlowColor::White], CORAL, GlowColor::White),
            None
        );
    }

    #[test]
    fn adding_a_color_already_in_the_palette_does_not_duplicate_it() {
        let palette = default_glow_palette();
        assert_eq!(palette_with(&palette, GlowColor::Cyan), palette);
    }

    #[test]
    fn adding_a_deleted_presets_hex_restores_the_preset() {
        let palette =
            delete_from_palette(&default_glow_palette(), GlowColor::Cyan, GlowColor::Cyan)
                .unwrap()
                .palette;
        let restored = palette_with(&palette, GlowColor::from_rgb(12, 175, 255));
        assert_eq!(restored.last(), Some(&GlowColor::Cyan));
        assert_eq!(restored.len(), PRESET_GLOW_COLORS.len());
    }

    #[test]
    fn adding_past_the_cap_is_refused() {
        let full: Vec<GlowColor> = (0..MAX_PALETTE_COLORS)
            .map(|i| GlowColor::Custom {
                r: i as u8,
                g: 1,
                b: 2,
            })
            .collect();
        assert_eq!(palette_with(&full, CORAL), full);
        assert_eq!(palette_with(&full[1..], CORAL).len(), MAX_PALETTE_COLORS);
    }

    #[test]
    fn deletion_announcement_names_a_moved_selection() {
        let palette = [GlowColor::White, GlowColor::Cyan];
        let moved = delete_from_palette(&palette, GlowColor::Cyan, GlowColor::Cyan).unwrap();
        assert_eq!(
            deletion_announcement(GlowColor::Cyan, GlowColor::Cyan, &moved),
            "Cyan highlight deleted. White selected."
        );
        let kept = delete_from_palette(&palette, GlowColor::White, GlowColor::Cyan).unwrap();
        assert_eq!(
            deletion_announcement(GlowColor::White, GlowColor::Cyan, &kept),
            "White highlight deleted."
        );
        let emptied = delete_from_palette(&[CORAL], CORAL, CORAL).unwrap();
        assert_eq!(
            deletion_announcement(CORAL, CORAL, &emptied),
            "All highlight colors deleted."
        );
    }

    #[test]
    fn swatch_labels_name_presets_and_customs() {
        assert_eq!(
            swatch_labels(GlowColor::MintGreen),
            (
                "Select Mint Green highlight".to_string(),
                "Delete Mint Green highlight".to_string()
            )
        );
        // @token-exempt: hex literals are accessible-name test inputs, not rendered colors
        assert_eq!(
            swatch_labels(CORAL),
            (
                "Select custom highlight #FF5733".to_string(),
                "Delete custom highlight #FF5733".to_string()
            )
        );
    }

    #[test]
    fn a_second_badge_delete_inside_the_guard_is_refused() {
        let badge = DeleteGesture::Badge;
        assert!(admits_delete(badge, None, T0));
        assert!(!admits_delete(
            badge,
            Some(T0),
            T0 + DELETE_REPEAT_GUARD_MS - 1.0
        ));
        assert!(admits_delete(badge, Some(T0), T0 + DELETE_REPEAT_GUARD_MS));
    }

    #[test]
    fn a_deliberate_second_key_delete_is_never_refused() {
        assert!(admits_delete(DeleteGesture::Key, Some(T0), T0 + 100.0));
        assert!(admits_delete(DeleteGesture::Key, Some(T0), T0));
    }

    #[test]
    fn addition_announcement_says_whether_the_palette_grew() {
        assert_eq!(
            addition_announcement(GlowColor::Cyan, true),
            "Cyan highlight added."
        );
        assert_eq!(
            addition_announcement(GlowColor::Cyan, false),
            "Cyan highlight selected."
        );
    }

    #[test]
    fn preview_reduced_motion_holds_a_static_lit_frame_but_follows_real_sound() {
        let decay = 0.5;
        let mut clock = PreviewClock::new(T0, true);
        let resume = f64::from(preview_silent_duration_ms(decay));
        for at in [
            T0,
            T0 + f64::from(PREVIEW_SPEAKING_MS),
            T0 + resume + 5000.0,
        ] {
            assert_eq!(
                clock.frame(None, None, decay, at),
                PreviewFrame::SIMULATED_LIT
            );
        }
        let spoke_at = T0 + 20_000.0;
        assert_eq!(
            clock.frame(Some(0.6), None, decay, spoke_at),
            lit(PreviewSource::Mic, 0.6)
        );
        assert_eq!(
            clock.frame(None, Some(0.4), decay, spoke_at + 100.0),
            lit(PreviewSource::Remote, 0.4)
        );
        assert_eq!(
            clock.frame(None, None, decay, spoke_at + 200.0),
            silent(PreviewSource::Remote)
        );
        assert_eq!(
            clock.frame(None, None, decay, spoke_at + 100.0 + resume),
            PreviewFrame::SIMULATED_LIT
        );
    }

    #[test]
    fn preview_reduced_motion_renders_a_real_source_exactly_as_the_tiles() {
        let settings = AppearanceSettings::default();
        for frame in [
            lit(PreviewSource::Mic, 0.6),
            silent(PreviewSource::Mic),
            lit(PreviewSource::Remote, 0.4),
            silent(PreviewSource::Remote),
        ] {
            assert_eq!(
                preview_tile_style(&frame, &settings, true),
                speak_style(frame.level, frame.speaking, &settings),
                "{frame:?}: the decay hold must survive reduced motion"
            );
        }
        let simulated = preview_tile_style(&PreviewFrame::SIMULATED_LIT, &settings, true);
        assert!(simulated.ends_with(" transition: none;"), "{simulated}");
    }

    #[test]
    fn preview_quantizes_live_levels_so_jitter_does_not_restyle_the_tile() {
        let mut clock = PreviewClock::new(T0, false);
        let first = clock.frame(Some(0.51), None, 0.5, T0);
        let jittered = clock.frame(Some(0.52), None, 0.5, T0 + 100.0);
        assert_eq!(first, jittered);
        assert_eq!(first.level, 0.5);
        let settings = AppearanceSettings::default();
        assert_eq!(
            preview_tile_style(&first, &settings, false),
            preview_tile_style(&jittered, &settings, false)
        );
        assert_eq!(quantize_level(0.01), 0.05, "a faint speaker stays lit");
        assert_eq!(quantize_level(1.0), 1.0);
        assert_eq!(clock.frame(Some(0.58), None, 0.5, T0 + 200.0).level, 0.6);
    }

    #[test]
    fn preview_pulses_only_while_simulating() {
        assert!(preview_tile_class(&PreviewFrame::SIMULATED_LIT).contains("preview-tile-pulsing"));
        assert!(
            preview_tile_class(&silent(PreviewSource::Simulated)).contains("preview-tile-pulsing")
        );
        for frame in [
            lit(PreviewSource::Mic, 0.5),
            silent(PreviewSource::Mic),
            lit(PreviewSource::Remote, 0.5),
            silent(PreviewSource::Remote),
        ] {
            let class = preview_tile_class(&frame);
            assert!(
                !class.contains("preview-tile-pulsing"),
                "{frame:?}: {class}"
            );
            assert!(class.contains(if frame.speaking {
                "preview-tile--speaking"
            } else {
                "preview-tile--silent"
            }));
        }
    }

    #[test]
    fn caption_names_a_real_source_at_once_and_simulation_after_two_seconds() {
        let mut caption = PreviewCaption::default();
        assert_eq!(
            caption.source(PreviewSource::Simulated, T0),
            PreviewSource::Simulated
        );
        assert_eq!(caption.source(PreviewSource::Mic, T0), PreviewSource::Mic);
        let quiet = T0 + 500.0;
        assert_eq!(
            caption.source(PreviewSource::Simulated, quiet),
            PreviewSource::Mic
        );
        assert_eq!(
            caption.source(
                PreviewSource::Simulated,
                quiet + CAPTION_SIMULATED_AFTER_MS - 1.0
            ),
            PreviewSource::Mic
        );
        assert_eq!(
            caption.source(PreviewSource::Simulated, quiet + CAPTION_SIMULATED_AFTER_MS),
            PreviewSource::Simulated
        );
        assert_eq!(
            caption.source(PreviewSource::Remote, quiet + 3000.0),
            PreviewSource::Remote
        );
        assert_eq!(
            caption.source(PreviewSource::Simulated, quiet + 3100.0),
            PreviewSource::Remote
        );
        assert_eq!(
            caption.source(PreviewSource::Mic, quiet + 3200.0),
            PreviewSource::Mic
        );
        assert_eq!(
            [
                PreviewSource::Mic,
                PreviewSource::Remote,
                PreviewSource::Simulated
            ]
            .map(preview_caption),
            [
                "Preview: your microphone",
                "Preview: someone speaking",
                "Preview: simulated speaker"
            ]
        );
    }

    #[test]
    fn only_speech_subsystems_reach_the_remote_state() {
        assert!(is_speech_event(&speaking_event("bob", 1, 0.6)));
        assert!(is_speech_event(&status_event("bob", 1, 1)));
        let mut other = speaking_event("bob", 1, 0.6);
        other.subsystem = "peer_video";
        assert!(!is_speech_event(&other));
        let mut remote = RemoteSpeakers::default();
        remote.observe(&other, T0);
        assert_eq!(remote.level(T0), None);
    }
}

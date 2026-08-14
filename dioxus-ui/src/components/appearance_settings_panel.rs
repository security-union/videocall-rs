/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use crate::components::canvas_generator::{calculate_glow_params, glow_transition_seconds};
use crate::components::color_picker::HsvColorPicker;
use crate::context::{
    apply_theme_to_dom, load_custom_colors_from_storage, save_custom_colors_to_storage,
    AppearanceSettings, AppearanceSettingsCtx, CustomThemeCtx, GlowColor, Theme,
    ThemePreferenceCtx, MAX_CUSTOM_COLORS,
};
use crate::theme::color as theme_color;
use crate::theme_file::{
    clear_custom_theme, custom_theme_display_name, persist_custom_theme_json, ThemeFileError,
    MAX_THEME_JSON_BYTES,
};
use crate::util::color_math::parse_hex;
use dioxus::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

/// Restore keyboard focus to a meaningful in-panel control after a color
/// picker action (add, close, cancel, delete). Tries the add button first;
/// when it is unmounted (custom_colors at MAX) falls back to the selected
/// swatch, then the swatch container.
fn focus_color_panel_fallback() {
    let doc = match web_sys::window().and_then(|w| w.document()) {
        Some(d) => d,
        None => return,
    };
    // 1. Add button (present when < MAX_CUSTOM_COLORS)
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

fn focus_custom_swatch_after_delete_deferred(removed_idx: usize) {
    // Use a browser timeout so the list re-renders before we focus the next target.
    let Some(window) = web_sys::window() else {
        return;
    };
    let callback = Closure::wrap(Box::new(move || {
        let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };

        let Ok(nodes) = doc.query_selector_all("[aria-label^=\"Select custom highlight \"]") else {
            focus_color_panel_fallback();
            return;
        };

        let len = nodes.length() as usize;
        if len == 0 {
            focus_color_panel_fallback();
            return;
        }

        // Focus the swatch that moved into the deleted slot, or the previous
        // one when the deleted swatch was the last item.
        let target_idx = removed_idx.min(len.saturating_sub(1));
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

/// The Decay explanation. Rendered as a real `role="tooltip"` element and
/// wired to the `(?)` trigger through `aria-describedby`, which is what makes
/// it reach screen readers: CSS `content` on a pseudo-element is not reliably
/// exposed to assistive technology and cannot be referenced by an `id`
/// (issue 1871). Kept as one constant so the visible bubble and the accessible
/// description cannot drift apart.
const DECAY_HELP_TEXT: &str = "Decay controls how long the glow lingers after speech. 0% is instant on/off; 100% is the longest lingering tail.";

/// Next `(is_open, is_suppressed)` for the Decay `(?)` trigger when it is
/// activated by click/tap or by Enter/Space.
///
/// Toggling OFF must **latch suppression**, not merely clear `--open`. The
/// trigger still holds focus immediately after the activation, and
/// `:focus-within` is one of the CSS reveal conditions — clearing `--open`
/// alone therefore leaves the bubble on screen and the second tap looks
/// broken. On touch there is no hover and no Escape key, so re-tapping the
/// trigger is the *only* dismissal available, and WCAG 2.1 SC 1.4.13
/// "Dismissible" requires a dismissal that does not move pointer hover or
/// keyboard focus (tapping elsewhere moves focus, so it does not count).
///
/// Turning ON clears suppression so an explicit open wins over a prior
/// Escape-dismissal. `onfocusout` clears both, so leaving the trigger re-arms
/// the affordance for the next visit.
///
/// Because each branch sets one flag and clears the other, the only two
/// outputs are `(false, true)` and `(true, false)` — never `(true, true)`.
/// `decay_help_class` relies on that.
fn next_decay_help_state(is_open: bool) -> (bool, bool) {
    if is_open {
        (false, true)
    } else {
        (true, false)
    }
}

/// Class string for the Decay `(?)` help trigger, given whether its tooltip is
/// click/tap-latched open and/or Escape-suppressed.
///
/// `--open` forces the tooltip visible (touch devices have no hover, so a tap
/// latches it); `--suppressed` forces it hidden even while the trigger keeps
/// keyboard focus. Suppression is what makes *both* dismissals observable —
/// Escape, and a second tap/activation — because `:focus-within` would
/// otherwise keep the bubble on screen while the trigger stays focused.
/// Mirrors `announce_help_class` in `preferences_settings_panel.rs`, the
/// shipped instance of this pattern.
///
/// **The branch order is load-bearing.** Production never reaches
/// `(true, true)` — both writers clear one flag as they set the other (see
/// `next_decay_help_state`, and the Escape branch on the trigger) — so testing
/// suppression first is defensive against that input, not required by it. But
/// the resulting precedence is a pinned contract
/// (`decay_help_class_escape_suppression_wins_over_open`): should the pair ever
/// arise, Escape must win, because emitting `--open` would keep the bubble on
/// screen and make the dismissal look like a no-op. Swapping the two branches
/// compiles and fails that test.
fn decay_help_class(is_open: bool, is_suppressed: bool) -> &'static str {
    if is_suppressed {
        "settings-info-icon speaker-highlight-help-icon speaker-highlight-help-icon--suppressed"
    } else if is_open {
        "settings-info-icon speaker-highlight-help-icon speaker-highlight-help-icon--open"
    } else {
        "settings-info-icon speaker-highlight-help-icon"
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
    let decay_slider_style = slider_fill_style(appearance.glow_decay);

    let mut custom_colors = use_signal(load_custom_colors_from_storage);
    let mut show_picker = use_signal(|| false);
    let mut color_input = use_signal(String::new);
    let mut input_error = use_signal(|| false);

    // Custom theme (single-slot) state
    let fallback_custom_theme = use_signal(|| None::<String>);
    let mut custom_theme_ctx =
        try_use_context::<CustomThemeCtx>().unwrap_or(CustomThemeCtx(fallback_custom_theme));
    let mut import_error: Signal<Option<String>> = use_signal(|| None);

    // Decay `(?)` help affordance (issue 1871). Touch has no hover, so a
    // tap/click latches the tooltip open; hover and keyboard focus reveal it
    // through CSS alone. Both dismissals — Escape, and a second
    // tap/Enter/Space — suppress a still-focused tooltip without blurring the
    // trigger and without closing the settings modal, which is what WCAG 2.1
    // SC 1.4.13 "Dismissible" requires (a dismissal that moves focus does not
    // count, and on touch the re-tap is the only one available).
    let mut decay_help_open = use_signal(|| false);
    let mut decay_help_suppressed = use_signal(|| false);

    let preset_colors = [
        GlowColor::White,
        GlowColor::Cyan,
        GlowColor::Magenta,
        GlowColor::Plum,
        GlowColor::MintGreen,
    ];

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
                                        // Preset swatches
                                        for color in preset_colors {
                                            {
                                                let is_selected = appearance.glow_color == color;
                                                rsx! {
                                                    div {
                                                        class: if is_selected { "color-swatch selected" } else { "color-swatch" },
                                                        role: "button",
                                                        tabindex: "0",
                                                        "aria-label": format!("Select {} highlight", color.label()),
                                                        "aria-pressed": if is_selected { "true" } else { "false" },
                                                        style: format!("--glow-color: {}", color.to_hex()),
                                                        onclick: move |evt: Event<MouseData>| {
                                                            evt.stop_propagation();
                                                            appearance_ctx
                                                                .0
                                                                .set(AppearanceSettings {
                                                                    glow_color: color,
                                                                    ..appearance_ctx.0()
                                                                });
                                                        },
                                                        onkeydown: move |evt: KeyboardEvent| {
                                                            let key = evt.key();
                                                            if is_keyboard_activation_key(&key) {
                                                                evt.prevent_default();
                                                                evt.stop_propagation();
                                                                appearance_ctx
                                                                    .0
                                                                    .set(AppearanceSettings {
                                                                        glow_color: color,
                                                                        ..appearance_ctx.0()
                                                                    });
                                                            }
                                                        },
                                                        title: color.label(),
                                                    }
                                                }
                                            }
                                        }
                                        // Custom swatches (with delete button)
                                        for (idx, color) in custom_colors().iter().enumerate() {
                                            {
                                                let color = *color;
                                                let is_selected = appearance.glow_color == color;
                                                rsx! {
                                                    div {
                                                        class: if is_selected {
                                                            "color-swatch selected"
                                                        } else {
                                                            "color-swatch"
                                                        },
                                                        style: format!("--glow-color: {}", color.to_hex()),
                                                        title: color.to_hex(),
                                                        role: "button",
                                                        tabindex: "0",
                                                        "aria-label": format!("Select custom highlight {} (delete with button)", color.to_hex()),
                                                        "aria-pressed": if is_selected { "true" } else { "false" },
                                                        onclick: move |evt: Event<MouseData>| {
                                                            evt.stop_propagation();
                                                            appearance_ctx
                                                                .0
                                                                .set(AppearanceSettings {
                                                                    glow_color: color,
                                                                    ..appearance_ctx.0()
                                                                });
                                                        },
                                                        onkeydown: move |evt: KeyboardEvent| {
                                                            let key = evt.key();
                                                            if is_keyboard_activation_key(&key) {
                                                                evt.prevent_default();
                                                                evt.stop_propagation();
                                                                appearance_ctx
                                                                    .0
                                                                    .set(AppearanceSettings {
                                                                        glow_color: color,
                                                                        ..appearance_ctx.0()
                                                                    });
                                                            }
                                                        },
                                                        button {
                                                            class: "color-swatch-delete-btn",
                                                            "aria-label": format!("Delete custom highlight {}", color.to_hex()),
                                                            onkeydown: move |evt: KeyboardEvent| {
                                                                let key = evt.key();
                                                                if is_keyboard_activation_key(&key) {
                                                                    evt.stop_propagation();
                                                                }
                                                            },
                                                            onclick: move |evt: Event<MouseData>| {
                                                                evt.stop_propagation();
                                                                let mut colors = custom_colors();
                                                                colors.remove(idx);
                                                                save_custom_colors_to_storage(&colors);
                                                                custom_colors.set(colors);
                                                                // If deleted color was selected, switch to default
                                                                if appearance.glow_color == color {
                                                                    appearance_ctx
                                                                        .0
                                                                        .set(AppearanceSettings {
                                                                            glow_color: GlowColor::MintGreen,
                                                                            ..appearance_ctx.0()
                                                                        });
                                                                }
                                                                show_picker.set(false);
                                                                focus_custom_swatch_after_delete_deferred(idx);
                                                            },
                                                            svg {
                                                                xmlns: "http://www.w3.org/2000/svg",
                                                                width: "12",
                                                                height: "12",
                                                                view_box: "0 0 24 24",
                                                                fill: "none",
                                                                stroke: "currentColor",
                                                                stroke_width: "3",
                                                                stroke_linecap: "round",
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
                                        if custom_colors().len() < MAX_CUSTOM_COLORS {
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
                                                                            let new_color = GlowColor::from_rgb(r, g, b);
                                                                            let colors = custom_colors();
                                                                            if !colors.contains(&new_color) {
                                                                                let mut colors = colors;
                                                                                colors.push(new_color);
                                                                                save_custom_colors_to_storage(&colors);
                                                                                custom_colors.set(colors);
                                                                            }
                                                                            appearance_ctx.0.set(AppearanceSettings {
                                                                                glow_color: new_color,
                                                                                ..appearance_ctx.0()
                                                                            });
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
                                label { class: "appearance-slider-label", "Brightness" }
                                input {
                                    r#type: "range",
                                    class: "appearance-slider",
                                    "data-testid": "speaker-highlight-brightness-slider",
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
                                label { class: "appearance-slider-label", "Glow" }
                                input {
                                    r#type: "range",
                                    class: "appearance-slider",
                                    "data-testid": "speaker-highlight-glow-slider",
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
                                // The `(?)` trigger is a sibling of the label, not a
                                // child of it: were the label ever wired to the slider
                                // with `for`/`id`, nested help text would be folded into
                                // the slider's accessible name.
                                div { class: "appearance-slider-label-group",
                                    label { class: "appearance-slider-label", "Decay" }
                                    span {
                                        class: decay_help_class(decay_help_open(), decay_help_suppressed()),
                                        role: "button",
                                        tabindex: 0,
                                        "aria-label": "About the Decay setting",
                                        "aria-describedby": "speaker-highlight-decay-tip",
                                        "data-testid": "speaker-highlight-decay-help",
                                        onclick: move |evt: Event<MouseData>| {
                                            evt.stop_propagation();
                                            let (open, suppressed) = next_decay_help_state(
                                                decay_help_open(),
                                            );
                                            decay_help_open.set(open);
                                            decay_help_suppressed.set(suppressed);
                                        },
                                        onkeydown: move |evt: Event<KeyboardData>| {
                                            let key = evt.key();
                                            if is_keyboard_activation_key(&key) {
                                                evt.prevent_default();
                                                evt.stop_propagation();
                                                let (open, suppressed) = next_decay_help_state(
                                                    decay_help_open(),
                                                );
                                                decay_help_open.set(open);
                                                decay_help_suppressed.set(suppressed);
                                            } else if key == Key::Escape && !decay_help_suppressed() {
                                                // First Escape while the tooltip shows: dismiss ONLY
                                                // the tooltip. Stop propagation so the modal's own
                                                // Escape handler (device_settings_modal.rs) does not
                                                // close the modal, and do NOT blur — focus stays on
                                                // the trigger. A second Escape finds the tooltip
                                                // already suppressed, falls through and bubbles, so
                                                // the modal closes as usual.
                                                evt.stop_propagation();
                                                decay_help_open.set(false);
                                                decay_help_suppressed.set(true);
                                            }
                                        },
                                        onfocusout: move |_| {
                                            decay_help_open.set(false);
                                            decay_help_suppressed.set(false);
                                        },
                                        "(?)"
                                        // `role="button"` is children-presentational, so this
                                        // child's `role="tooltip"` is INERT: the AX tree reports
                                        // it as `{role: "none", ignored: true}`. Do not "fix"
                                        // that — `aria-describedby` above does 100% of the work
                                        // (an `aria-describedby` target contributes its text
                                        // even when unrendered and even when its own role is
                                        // dropped). The role is kept only as authoring intent.
                                        // Children-presentational is also what guarantees this
                                        // full sentence-pair explanation can never leak into the
                                        // trigger's accessible name; the explicit `aria-label`
                                        // above is the second guard.
                                        span {
                                            id: "speaker-highlight-decay-tip",
                                            class: "speaker-highlight-help-tip",
                                            role: "tooltip",
                                            "data-testid": "speaker-highlight-decay-help-text",
                                            {DECAY_HELP_TEXT}
                                        }
                                    }
                                }
                                // DELIBERATE: this slider carries no
                                // `aria-describedby`. The `(?)` trigger above
                                // already exposes the full explanation and is
                                // the immediately preceding stop in both DOM
                                // and tab order, so pointing the slider at the
                                // same `#speaker-highlight-decay-tip` would
                                // replay the whole explanation at two
                                // consecutive tab stops. The choice is "once
                                // vs. twice", not "described vs. undescribed".
                                //
                                // This is NOT the sibling-not-child concern in
                                // the comment above: `aria-describedby` never
                                // contributes to the accessible NAME, so it
                                // could not pollute this slider's name the way
                                // nesting the trigger inside the `label` would.
                                // Different problem — do not conflate them, and
                                // do not "fix" this omission as an oversight.
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
                                    "aria-label": "Reset speaker highlight settings to defaults",
                                    onclick: move |_| {
                                        let defaults = AppearanceSettings::default();
                                        appearance_ctx.0.set(AppearanceSettings {
                                            glow_enabled: defaults.glow_enabled,
                                            glow_color: defaults.glow_color,
                                            glow_brightness: defaults.glow_brightness,
                                            inner_glow_strength: defaults.inner_glow_strength,
                                            glow_decay: defaults.glow_decay,
                                            ..appearance_ctx.0()
                                        });
                                    },
                                    "Reset highlight"
                                }
                                span { class: "appearance-slider-value", "" }
                            }
                        } // speaker-highlight-controls

                        div { class: "speaker-highlight-preview",
                            SpeakerHighlightPreview { settings: appearance }
                            p { class: "speaker-highlight-preview-caption", "Active speaker preview" }
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
/// Owns the speaking/silent animation timer so that timer-driven re-renders
/// are scoped to the preview subtree and do not re-render the parent panel.
#[component]
fn SpeakerHighlightPreview(settings: AppearanceSettings) -> Element {
    let mut preview_speaking = use_signal(|| true);
    let mut decay_signal = use_signal(|| settings.glow_decay);
    // Keep the signal in sync with the current slider value.
    if (*decay_signal.peek() - settings.glow_decay).abs() > f32::EPSILON {
        decay_signal.set(settings.glow_decay);
    }
    use_future(move || async move {
        if prefers_reduced_motion() {
            preview_speaking.set(true);
            return;
        }
        loop {
            // Speaking burst (fixed short duration).
            preview_speaking.set(true);
            gloo_timers::future::TimeoutFuture::new(PREVIEW_SPEAKING_MS).await;
            // Silent phase — long enough to perceive the decay tail.
            preview_speaking.set(false);
            let silent_ms = preview_silent_duration_ms(*decay_signal.peek());
            gloo_timers::future::TimeoutFuture::new(silent_ms).await;
        }
    });

    let preview_style = preview_glow_style(&settings);
    let preview_tile_class = if preview_speaking() {
        "preview-tile preview-tile-pulsing preview-tile--speaking"
    } else {
        "preview-tile preview-tile-pulsing preview-tile--silent"
    };

    rsx! {
        div {
            class: "{preview_tile_class}",
            style: "{preview_style}",
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
    }
}

/// Detect whether the document is currently rendering the light theme.
///
/// Used so the appearance preview can dampen its glow further on light
/// surfaces, where the same alpha reads much brighter than on dark.
fn is_light_theme() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }

    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.document_element())
            .and_then(|e| e.get_attribute("data-theme"))
            .map(|t| t == "light")
            .unwrap_or(false)
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
const PREVIEW_SPEAKING_MS: u32 = 600;
/// Minimum silent phase so the cycle doesn't spin too fast at 0% decay.
const PREVIEW_SILENT_MIN_MS: u32 = 400;

/// Compute the silent phase duration (ms) for the preview animation cycle.
///
/// Longer decay → longer visible tail → more silent time needed to perceive it.
/// The silent phase is hold + fade + a small minimum baseline.
fn preview_silent_duration_ms(decay: f32) -> u32 {
    let (_fade_in, fade_out, hold) = glow_transition_seconds(decay);
    let tail_ms = ((hold + fade_out) * 1000.0) as u32;
    PREVIEW_SILENT_MIN_MS + tail_ms
}

/// Compute the inline CSS custom properties for the appearance preview tile.
///
/// The preview tile's actual glow rendering lives in CSS; this helper only
/// publishes the live color channels, glow geometry, and decay-derived timing
/// values so the stylesheet can render the same decay tail as the production
/// speaker glow.
///
/// The preview is intentionally a *quiet* supporting element next to the
/// dominant controls, so the computed glow is scaled down from the
/// production tile parameters (blur ~60%, spread ~70%, alpha ~60%; alpha
/// further dampened on light theme so it doesn't flood the modal).
fn preview_glow_style(settings: &AppearanceSettings) -> String {
    let p = calculate_glow_params(0.55, settings.glow_brightness, settings.inner_glow_strength);
    let (fade_in_seconds, fade_out_duration, hold_delay) =
        glow_transition_seconds(settings.glow_decay);
    let (r, g, b) = settings.glow_color.to_rgb();
    let blur_scale = 0.60_f32;
    let spread_scale = 0.70_f32;
    let alpha_scale = if is_light_theme() { 0.42_f32 } else { 0.60_f32 };

    format!(
        "--preview-glow-r: {r}; --preview-glow-g: {g}; --preview-glow-b: {b}; \
         --preview-glow-outer-blur: {:.0}; --preview-glow-outer-spread: {:.0}; \
         --preview-glow-outer-alpha: {:.2}; --preview-glow-inner-blur: {:.0}; \
         --preview-glow-inner-spread: {:.0}; --preview-glow-inner-alpha: {:.2}; \
         --preview-glow-border-alpha: {:.2}; --preview-glow-fade-in: {fade_in_seconds:.2}s; \
         --preview-glow-fade-out: {fade_out_duration:.2}s; --preview-glow-hold-delay: {hold_delay:.2}s;",
        p.outer_blur * blur_scale,
        p.outer_spread * spread_scale,
        p.outer_alpha * alpha_scale,
        p.inner_blur * blur_scale,
        p.inner_spread * spread_scale,
        p.inner_alpha * alpha_scale,
        p.border_alpha,
    )
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

    #[test]
    fn decay_help_class_resting_state_carries_no_modifier() {
        let class = decay_help_class(false, false);
        // The CSS reveal + suppression rules are all keyed off this base class.
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
    fn decay_help_class_latched_open_adds_open_modifier() {
        let class = decay_help_class(true, false);
        assert!(
            class.contains("speaker-highlight-help-icon--open"),
            "tap/click latch must force the tooltip visible: {class}"
        );
        assert!(!class.contains("--suppressed"));
    }

    #[test]
    fn decay_help_toggling_on_opens_and_clears_suppression() {
        // An explicit open must beat a prior Escape-dismissal.
        assert_eq!(next_decay_help_state(false), (true, false));
    }

    #[test]
    fn decay_help_toggling_off_latches_suppression() {
        // The regression this pins: clearing `--open` alone is NOT a dismissal.
        // The trigger still holds focus right after the tap, and `:focus-within`
        // is a CSS reveal condition, so the bubble would stay on screen. Touch
        // has no hover and no Escape key, so this re-tap is the only dismissal
        // that does not move focus — which is precisely what WCAG 2.1 SC 1.4.13
        // "Dismissible" demands. Restore the old `suppressed = false;
        // open = !open` and this fails on the second element.
        assert_eq!(next_decay_help_state(true), (false, true));
    }

    #[test]
    fn decay_help_toggle_off_renders_the_suppressed_modifier() {
        // End-to-end through the production class builder: the state the
        // off-toggle produces must be the state CSS keys its hide rule off.
        let (is_open, is_suppressed) = next_decay_help_state(true);
        let class = decay_help_class(is_open, is_suppressed);
        assert!(
            class.contains("speaker-highlight-help-icon--suppressed"),
            "a second tap must render the suppressed modifier, since only that \
             rule out-specifies `:focus-within`: {class}"
        );
    }

    #[test]
    fn decay_help_toggle_round_trip_returns_to_a_revealing_state() {
        // Tap-open → tap-closed → tap-open again. The middle state latches
        // suppression, so the third tap must clear it or the affordance would
        // stay dead for the rest of the visit.
        let (open_1, suppressed_1) = next_decay_help_state(false);
        assert!(open_1 && !suppressed_1);
        let (open_2, suppressed_2) = next_decay_help_state(open_1);
        assert!(!open_2 && suppressed_2);
        let (open_3, suppressed_3) = next_decay_help_state(open_2);
        assert!(
            open_3 && !suppressed_3,
            "re-tapping a suppressed trigger must reopen it"
        );
        assert!(
            decay_help_class(open_3, suppressed_3).contains("speaker-highlight-help-icon--open")
        );
    }

    #[test]
    fn decay_help_class_escape_suppression_wins_over_open() {
        // Escape must beat the open latch. The trigger keeps focus after
        // Escape, so leaving `--open` on would keep the bubble on screen via
        // both the latch and `:focus-within`, making Escape look like a no-op
        // (WCAG 2.1 SC 1.4.13 "Dismissible").
        let class = decay_help_class(true, true);
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
    fn decay_help_text_is_the_single_source_for_the_explanation() {
        // The visible `role="tooltip"` element and the accessible description
        // both render this one constant, so they cannot drift. Guard the two
        // endpoints the copy exists to explain.
        assert!(DECAY_HELP_TEXT.contains("0%"), "{DECAY_HELP_TEXT}");
        assert!(DECAY_HELP_TEXT.contains("100%"), "{DECAY_HELP_TEXT}");
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

    #[test]
    fn preview_glow_style_off_state_includes_transition_delay_when_decay_positive() {
        let settings = AppearanceSettings {
            glow_enabled: true,
            glow_decay: 0.5,
            inner_glow_strength: 0.5,
            ..AppearanceSettings::default()
        };
        let style = preview_glow_style(&settings);
        // 50% decay → 1.0s hold delay; the off-style must contain a non-zero delay.
        assert!(
            style.contains("--preview-glow-hold-delay: 1.00s;"),
            "preview vars should include hold delay: {style}"
        );
    }

    #[test]
    fn preview_glow_style_off_state_no_delay_at_zero_decay() {
        let settings = AppearanceSettings {
            glow_enabled: true,
            glow_decay: 0.0,
            inner_glow_strength: 0.5,
            ..AppearanceSettings::default()
        };
        let style = preview_glow_style(&settings);
        // 0% decay → all zeros: no transition, no delay.
        assert!(
            style.contains("--preview-glow-fade-out: 0.00s;"),
            "zero-decay preview vars should have 0s fade-out: {style}"
        );
    }

    #[test]
    fn preview_glow_style_exports_preview_vars() {
        let settings = AppearanceSettings {
            glow_enabled: true,
            glow_decay: 0.5,
            // Use a deterministic strength value so this assertion only checks
            // that the helper exports the CSS variables consumed by the preview.
            inner_glow_strength: 0.0,
            glow_brightness: 0.5,
            ..AppearanceSettings::default()
        };
        let style = preview_glow_style(&settings);
        // The helper should publish the CSS custom properties consumed by the
        // preview tile, including the selected color and decay timing.
        assert!(
            style.contains("--preview-glow-r: "),
            "preview vars should include the channel tokens: {style}"
        );
        assert!(style.contains("--preview-glow-fade-out: 1.50s;"));
        assert!(style.contains("--preview-glow-hold-delay: 1.00s;"));
        assert!(style.contains("--preview-glow-border-alpha:"));
    }
}

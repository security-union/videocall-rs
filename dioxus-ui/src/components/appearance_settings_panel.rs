/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use crate::components::canvas_generator::{calculate_glow_params, DEFAULT_TILE_BORDER_COLOR};
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
use wasm_bindgen::JsCast;

fn focus_add_btn() {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("add-custom-color-btn"))
    {
        if let Ok(html) = el.dyn_into::<web_sys::HtmlElement>() {
            let _ = html.focus();
        }
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
    let preview_style = preview_glow_style(&appearance);
    let brightness_slider_style = slider_fill_style(appearance.glow_brightness);
    let inner_slider_style = slider_fill_style(appearance.inner_glow_strength);

    let mut custom_colors = use_signal(load_custom_colors_from_storage);
    let mut show_picker = use_signal(|| false);
    let mut color_input = use_signal(String::new);
    let mut input_error = use_signal(|| false);

    // Custom theme (single-slot) state
    let fallback_custom_theme = use_signal(|| None::<String>);
    let mut custom_theme_ctx =
        try_use_context::<CustomThemeCtx>().unwrap_or(CustomThemeCtx(fallback_custom_theme));
    let mut import_error: Signal<Option<String>> = use_signal(|| None);

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
                                    div { class: "color-swatches",
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
                                                        button {
                                                            class: "color-swatch-delete-btn",
                                                            onclick: move |evt: Event<MouseData>| {
                                                                evt.stop_propagation(); // Restore keyboard focus to the add-custom-color
                                                                let mut colors = custom_colors();
                                                                colors.remove(idx);
                                                                save_custom_colors_to_storage(&colors);
                                                                custom_colors.set(colors);
                                                                // If deleted color was selected, switch to default
                                                                if appearance.glow_color == color {
                                                                    appearance_ctx
                                                                        .0
                                                                        .set(AppearanceSettings {
                                                                            glow_color: GlowColor::MintGreen, // Restore keyboard focus to the add-custom-color
                                                                            ..appearance_ctx.0()
                                                                        });
                                                                }
                                                                show_picker.set(false);
                                                                focus_add_btn();
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
                                                focus_add_btn();
                                            },
                                            onkeydown: move |evt: KeyboardEvent| {
                                                if evt.key() == Key::Escape {
                                                    show_picker.set(false);
                                                    color_input.set(String::new());
                                                    input_error.set(false);
                                                    focus_add_btn();
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
                                                            focus_add_btn();
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
                                                                    focus_add_btn();
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
                                                                        focus_add_btn();
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
                                                                            focus_add_btn();
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
                                label { class: "appearance-slider-label", "Inner Glow" }
                                input {
                                    r#type: "range",
                                    class: "appearance-slider",
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
                        } // speaker-highlight-controls

                        div { class: "speaker-highlight-preview",
                            div {
                                class: "preview-tile preview-tile-pulsing",
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
                            p { class: "speaker-highlight-preview-caption", "Active speaker preview" }
                        }
                    }
            }
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
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
        .and_then(|e| e.get_attribute("data-theme"))
        .map(|t| t == "light")
        .unwrap_or(false)
}

/// Compute a static glow style for the appearance preview tile.
///
/// Calls [`calculate_glow_params`] with a fixed intensity of 0.55 so the
/// preview is always visible regardless of microphone state. The CSS
/// `preview-tile-pulsing` animation provides visual dynamism.
///
/// The preview is intentionally a *quiet* supporting element next to the
/// dominant controls, so the computed glow is scaled down from the
/// production tile parameters (blur ~60%, spread ~70%, alpha ~60%; alpha
/// further dampened on light theme so it doesn't flood the modal).
fn preview_glow_style(settings: &AppearanceSettings) -> String {
    if !settings.glow_enabled {
        return format!(
            "box-shadow: none; border-color: {};",
            DEFAULT_TILE_BORDER_COLOR
        );
    }

    let (r, g, b) = settings.glow_color.to_rgb();
    let p = calculate_glow_params(0.55, settings.glow_brightness, settings.inner_glow_strength);
    let blur_scale = 0.60_f32;
    let spread_scale = 0.70_f32;
    let alpha_scale = if is_light_theme() { 0.42_f32 } else { 0.60_f32 };
    format!(
        "box-shadow: 0 0 {:.0}px {:.0}px rgba({r}, {g}, {b}, {:.2}), \
         inset 0 0 {:.0}px {:.0}px rgba({r}, {g}, {b}, {:.2}); \
         border-color: rgba({r}, {g}, {b}, {:.2});",
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

/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use crate::components::canvas_generator::{calculate_glow_params, DEFAULT_TILE_BORDER_COLOR};
use crate::components::color_picker::HsvColorPicker;
use crate::components::density::{DensityMode, DENSITY_MODES};
use crate::context::{
    load_custom_colors_from_storage, save_custom_colors_to_storage, save_density_mode,
    save_dock_autohide, save_dock_position, AppearanceSettings, AppearanceSettingsCtx, AutohideCtx,
    DensityModeCtx, DockPosition, DockPositionCtx, GlowColor, Theme, ThemePreferenceCtx,
    MAX_CUSTOM_COLORS,
};
use crate::theme::color as theme_color;
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

#[component]
pub fn AppearanceSettingsPanel() -> Element {
    let mut theme_ctx = use_context::<ThemePreferenceCtx>();
    let mut appearance_ctx = use_context::<AppearanceSettingsCtx>();
    // Fallback signals for when contexts are not provided (e.g. in tests).
    // Hooks must be called unconditionally, so we always create them.
    let fallback_dock = use_signal(|| DockPosition::Bottom);
    let fallback_autohide = use_signal(|| true);
    let fallback_density = use_signal(|| DensityMode::Auto);
    let mut dock_position_ctx =
        try_use_context::<DockPositionCtx>().unwrap_or(DockPositionCtx(fallback_dock));
    let mut autohide_ctx =
        try_use_context::<AutohideCtx>().unwrap_or(AutohideCtx(fallback_autohide));
    let mut density_ctx =
        try_use_context::<DensityModeCtx>().unwrap_or(DensityModeCtx(fallback_density));
    let appearance = (appearance_ctx.0)();
    let preview_style = preview_glow_style(&appearance);
    let brightness_slider_style =
        slider_fill_style(appearance.glow_brightness, appearance.glow_color);
    let inner_slider_style =
        slider_fill_style(appearance.inner_glow_strength, appearance.glow_color);

    let mut custom_colors = use_signal(load_custom_colors_from_storage);
    let mut show_picker = use_signal(|| false);
    let mut color_input = use_signal(String::new);
    let mut input_error = use_signal(|| false);

    let preset_colors = [
        GlowColor::White,
        GlowColor::Cyan,
        GlowColor::Magenta,
        GlowColor::Plum,
        GlowColor::MintGreen,
    ];

    rsx! {
        div {
            class: if appearance.glow_enabled { "appearance-settings-panel" } else { "appearance-settings-panel glow-disabled" },

            div { class: "appearance-content-column",

            // ── Section 1: Theme ─────────────────────────────────────────────
            section { class: "appearance-section",
                div { class: "appearance-section-header",
                    h3 { class: "appearance-section-title", "Theme" }
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
                                                    rect { x: "2", y: "3", width: "20", height: "14", rx: "2" }
                                                    line { x1: "8", y1: "21", x2: "16", y2: "21" }
                                                    line { x1: "12", y1: "17", x2: "12", y2: "21" }
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
                                                    line { x1: "12", y1: "1", x2: "12", y2: "3" }
                                                    line { x1: "12", y1: "21", x2: "12", y2: "23" }
                                                    line { x1: "4.22", y1: "4.22", x2: "5.64", y2: "5.64" }
                                                    line { x1: "18.36", y1: "18.36", x2: "19.78", y2: "19.78" }
                                                    line { x1: "1", y1: "12", x2: "3", y2: "12" }
                                                    line { x1: "21", y1: "12", x2: "23", y2: "12" }
                                                    line { x1: "4.22", y1: "19.78", x2: "5.64", y2: "18.36" }
                                                    line { x1: "18.36", y1: "5.64", x2: "19.78", y2: "4.22" }
                                                }
                                            }
                                            span { class: "theme-icon-button-label", "{variant.label()}" }
                                        }
                                    }
                                }
                            }
                        }
                    }

            hr { class: "appearance-section-divider" }

            // ── Section 2: Speaker Highlight ─────────────────────────────────
            section { class: "appearance-section",
                div { class: "appearance-section-header",
                    h3 { class: "appearance-section-title", "Speaker Highlight" }
                    label {
                        class: "glow-switch",
                        input {
                            r#type: "checkbox",
                            "aria-label": "Toggle speaker highlight",
                            checked: appearance.glow_enabled,
                            onchange: move |evt: Event<FormData>| {
                                let enabled = evt.checked();
                                appearance_ctx.0.set(AppearanceSettings {
                                    glow_enabled: enabled,
                                    ..appearance_ctx.0()
                                });
                            },
                        }
                        span { class: "glow-switch-track" }
                    }
                }
                p { class: "appearance-section-helper",
                    "Visual glow around the active speaker."
                }

                    div { class: "appearance-control-group glow-palette-section",
                        span { class: "appearance-control-label", "Highlight Color" }
                        div { class: "color-swatches",
                        // Preset swatches
                        for color in preset_colors {
                            {
                                let is_selected = appearance.glow_color == color;
                                rsx! {
                                    div {
                                        class: if is_selected {
                                            "color-swatch selected"
                                        } else {
                                            "color-swatch"
                                        },
                                        role: "button",
                                        tabindex: "0",
                                        "aria-label": format!("Select {} highlight", color.label()),
                                        "aria-pressed": if is_selected { "true" } else { "false" },
                                        style: format!("--glow-color: {}", color.to_hex()),
                                        onclick: move |evt: Event<MouseData>| {
                                            evt.stop_propagation();
                                            appearance_ctx.0.set(AppearanceSettings {
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
                                            appearance_ctx.0.set(AppearanceSettings {
                                                glow_color: color,
                                                ..appearance_ctx.0()
                                            });
                                        },
                                        button {
                                            class: "color-swatch-delete-btn",
                                            onclick: move |evt: Event<MouseData>| {
                                                evt.stop_propagation();
                                                let mut colors = custom_colors();
                                                colors.remove(idx);
                                                save_custom_colors_to_storage(&colors);
                                                custom_colors.set(colors);
                                                // If deleted color was selected, switch to default
                                                if appearance.glow_color == color {
                                                    appearance_ctx.0.set(AppearanceSettings {
                                                        glow_color: GlowColor::MintGreen,
                                                        ..appearance_ctx.0()
                                                    });
                                                }
                                                show_picker.set(false);
                                                // Restore keyboard focus to the add-custom-color
                                                // button so users navigating with the keyboard do
                                                // not lose context to <body> after deleting a swatch.
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
                                                line { x1: "6", y1: "6", x2: "18", y2: "18" }
                                                line { x1: "6", y1: "18", x2: "18", y2: "6" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // '+' button to add custom color
                        if custom_colors().len() < MAX_CUSTOM_COLORS {
                            div {
                                class: "color-swatch add-color-btn",
                                id: "add-custom-color-btn",
                                role: "button",
                                tabindex: "0",
                                "aria-label": "Add custom color",
                                title: "Add custom color",
                                onclick: move |evt: Event<MouseData>| {
                                    evt.stop_propagation();
                                    let open = !show_picker();
                                    show_picker.set(open);
                                    if open {
                                        // Seed the hex input with the currently selected
                                        // glow color so the picker opens on a sensible spot
                                        // instead of jumping to red.
                                        color_input.set(appearance.glow_color.to_hex());
                                        input_error.set(false);
                                    }
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
                                    line { x1: "12", y1: "5", x2: "12", y2: "19" }
                                    line { x1: "5", y1: "12", x2: "19", y2: "12" }
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
                                // Backdrop click closes the modal
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
                                onmousedown: move |evt: Event<MouseData>| evt.stop_propagation(),
                                onclick: move |evt: Event<MouseData>| evt.stop_propagation(),
                                onkeydown: move |evt: KeyboardEvent| {
                                    if evt.key() == Key::Escape {
                                        show_picker.set(false);
                                        color_input.set(String::new());
                                        input_error.set(false);
                                        focus_add_btn();
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
                                                    disabled: GlowColor::from_hex(&color_input()).is_none(),
                                                    onclick: move |evt: Event<MouseData>| {
                                                        evt.stop_propagation();
                                                        if let Some(new_color) = GlowColor::from_hex(&color_input()) {
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
                    }

                    div { class: "appearance-slider-row",
                        label { class: "appearance-slider-label", "Brightness" }
                        input {
                            r#type: "range",
                            class: "appearance-slider",
                            min: "0",
                            max: "100",
                            style: "{brightness_slider_style}",
                            value: "{(appearance.glow_brightness * 100.0) as i32}",
                            oninput: move |evt: Event<FormData>| {
                                if let Ok(value) = evt.value().parse::<f32>() {
                                    appearance_ctx.0.set(AppearanceSettings {
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
                            min: "0",
                            max: "100",
                            style: "{inner_slider_style}",
                            value: "{(appearance.inner_glow_strength * 100.0) as i32}",
                            oninput: move |evt: Event<FormData>| {
                                if let Ok(value) = evt.value().parse::<f32>() {
                                    appearance_ctx.0.set(AppearanceSettings {
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

                    div { class: "appearance-preview-block",
                        span { class: "appearance-control-label", "Preview" }
                        div {
                            class: "preview-tile preview-tile-pulsing",
                            style: "{preview_style}",
                            svg {
                                xmlns: "http://www.w3.org/2000/svg",
                                view_box: "0 0 56 56",
                                width: "56",
                                height: "56",
                                style: "pointer-events: none; flex-shrink: 0;",
                                // Background circle
                                circle { cx: "28", cy: "28", r: "28", fill: "{theme_color::PREVIEW_AVATAR_RING}" }
                                // Head
                                circle { cx: "28", cy: "21", r: "9", fill: "{theme_color::PREVIEW_AVATAR_BG}" }
                                // Body / shoulders
                                ellipse { cx: "28", cy: "42", rx: "15", ry: "10", fill: "{theme_color::PREVIEW_AVATAR_BG}" }
                            }
                        }
                    }
            }

            hr { class: "appearance-section-divider" }

            // ── Section 3: Dock Settings ─────────────────────────────────────
            section { class: "appearance-section",
                div { class: "appearance-section-header",
                    h3 { class: "appearance-section-title", "Dock Settings" }
                }

                // Position selector — reuses transport-segmented styling
                div { class: "device-setting-group",
                    span { class: "transport-segmented-label", "Position" }
                    div {
                        class: "transport-segmented",
                        role: "radiogroup",
                        "aria-label": "Action bar position",
                        for (pos, label) in [(DockPosition::Bottom, "Bottom"), (DockPosition::Left, "Left"), (DockPosition::Right, "Right")] {
                            button {
                                r#type: "button",
                                role: "radio",
                                "aria-checked": if dock_position_ctx.0() == pos { "true" } else { "false" },
                                class: if dock_position_ctx.0() == pos { "transport-segmented-option selected" } else { "transport-segmented-option" },
                                onclick: move |_| {
                                    dock_position_ctx.0.set(pos);
                                    save_dock_position(pos);
                                },
                                "{label}"
                            }
                        }
                    }
                }

                // Autohide toggle
                div { class: "appearance-section-header dock-autohide-row",
                    label { class: "appearance-section-title appearance-section-title--sm", "Auto-hide" }
                    label {
                        class: "glow-switch",
                        "aria-label": "Toggle action bar auto-hide",
                        input {
                            r#type: "checkbox",
                            checked: autohide_ctx.0(),
                            onchange: move |evt: Event<FormData>| {
                                let checked = evt.checked();
                                autohide_ctx.0.set(checked);
                                save_dock_autohide(checked);
                            },
                        }
                        span { class: "glow-switch-track" }
                    }
                }
            }

            hr { class: "appearance-section-divider" }

            // ── Section 4: Tiling ────────────────────────────────────────────
            section { class: "appearance-section",
                div { class: "appearance-section-header",
                    h3 { class: "appearance-section-title", "Tiling" }
                }

                div { class: "device-setting-group",
                    span { class: "transport-segmented-label", "Density" }
                    div {
                        class: "transport-segmented",
                        role: "radiogroup",
                        "aria-label": "Tile density mode",
                        for mode in DENSITY_MODES {
                            button {
                                r#type: "button",
                                role: "radio",
                                "aria-checked": if density_ctx.0() == mode { "true" } else { "false" },
                                class: if density_ctx.0() == mode { "transport-segmented-option selected" } else { "transport-segmented-option" },
                                onclick: move |_| {
                                    density_ctx.0.set(mode);
                                    save_density_mode(mode);
                                },
                                "{mode.label()}"
                            }
                        }
                    }
                }
            }
            }
        }
    }
}

fn slider_fill_style(value: f32, color: GlowColor) -> String {
    let (red, green, blue) = color.to_rgb();
    let fill_pct = (value.clamp(0.0, 1.0) * 100.0) as i32;
    format!(
        "--thumb-glow: rgb({red},{green},{blue}); --thumb-halo: rgba({red},{green},{blue},0.38); --fill: {fill_pct}%;" // @token-exempt: dynamic per-glow-color rgba composed at runtime
    )
}

/// Compute a static glow style for the appearance preview tile.
///
/// Calls [`calculate_glow_params`] with a fixed intensity of 0.65 so the
/// preview is always visible regardless of microphone state. The CSS
/// `preview-tile-pulsing` animation provides visual dynamism.
fn preview_glow_style(settings: &AppearanceSettings) -> String {
    if !settings.glow_enabled {
        return format!(
            "box-shadow: none; border-color: {};",
            DEFAULT_TILE_BORDER_COLOR
        );
    }

    let (r, g, b) = settings.glow_color.to_rgb();
    let p = calculate_glow_params(0.65, settings.glow_brightness, settings.inner_glow_strength);
    format!(
        "box-shadow: 0 0 {:.0}px {:.0}px rgba({r}, {g}, {b}, {:.2}), \
         inset 0 0 {:.0}px {:.0}px rgba({r}, {g}, {b}, {:.2}); \
         border-color: rgba({r}, {g}, {b}, {:.2});",
        p.outer_blur,
        p.outer_spread,
        p.outer_alpha,
        p.inner_blur,
        p.inner_spread,
        p.inner_alpha,
        p.border_alpha,
    )
}

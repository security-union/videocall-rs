/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use crate::context::{
    load_custom_colors_from_storage, save_custom_colors_to_storage, AppearanceSettings,
    AppearanceSettingsCtx, GlowColor, MAX_CUSTOM_COLORS,
};
use dioxus::prelude::*;

#[component]
pub fn AppearanceSettingsPanel() -> Element {
    let mut appearance_ctx = use_context::<AppearanceSettingsCtx>();
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
            div { class: "appearance-title",
                h3 { "Appearance" }
                p { "Customize how speaking glows appear on your screen" }
            }

            div { class: "appearance-controls",
                div { class: "appearance-section glow-toggle-section",
                    div { class: "slider-header",
                        label { "Glow" }
                        label {
                            class: "glow-switch",
                            "aria-label": "Toggle glow effect",
                            input {
                                r#type: "checkbox",
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
                }

                div { class: "appearance-divider" }

                div { class: "appearance-section glow-palette-section",
                    h4 { "Glow Color" }
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
                                        "aria-label": format!("Select {} glow", color.label()),
                                        "aria-pressed": if is_selected { "true" } else { "false" },
                                        onclick: move |_| {
                                            appearance_ctx.0.set(AppearanceSettings {
                                                glow_color: color,
                                                ..appearance_ctx.0()
                                            });
                                        },
                                        style: format!("background-color: {}; cursor: pointer;", color.to_hex()),
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
                                        style: format!("background-color: {}; cursor: pointer;", color.to_hex()),
                                        title: color.to_hex(),
                                        role: "button",
                                        tabindex: "0",
                                        "aria-label": format!("Select custom glow {} (delete with button)", color.to_hex()),
                                        "aria-pressed": if is_selected { "true" } else { "false" },
                                        onclick: move |_| {
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
                                role: "button",
                                tabindex: "0",
                                "aria-label": "Add custom color",
                                title: "Add custom color",
                                onclick: move |_| {
                                    let open = !show_picker();
                                    show_picker.set(open);
                                    if open {
                                        color_input.set(String::new());
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
                    // Inline custom color popover
                    if show_picker() {
                        // Click-outside overlay (behind the popover)
                        div {
                            style: "position: fixed; inset: 0; z-index: 99;",
                            onmousedown: move |_| {
                                show_picker.set(false);
                            },
                        }
                        div {
                            class: "custom-color-popover",
                            style: "position: relative; z-index: 100;",
                            {
                                let preview_color = GlowColor::from_hex(&color_input());
                                rsx! {
                                    div { class: "custom-color-popover-row",
                                        if let Some(c) = preview_color {
                                            div {
                                                class: "custom-color-preview",
                                                style: format!("background-color: {};", c.to_hex()),
                                            }
                                        }
                                        input {
                                            class: if input_error() {
                                                "custom-color-input error"
                                            } else {
                                                "custom-color-input"
                                            },
                                            r#type: "text",
                                            placeholder: "#RRGGBB",
                                            maxlength: "7",
                                            spellcheck: "false",
                                            autocomplete: "off",
                                            value: "{color_input}",
                                            oninput: move |evt: Event<FormData>| {
                                                color_input.set(evt.value());
                                                input_error.set(false);
                                            },
                                        }
                                        button {
                                            class: "custom-color-add-btn",
                                            onclick: move |_| {
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

                div { class: "appearance-divider" }

                div { class: "appearance-section brightness-section",
                    div { class: "slider-header",
                        label { "Brightness" }
                        span { class: "slider-value",
                            "{(appearance.glow_brightness * 100.0) as i32}%"
                        }
                    }
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
                }

                div { class: "appearance-divider" }

                div { class: "appearance-section inner-glow-section",
                    div { class: "slider-header",
                        label { "Inner Glow Strength" }
                        span { class: "slider-value",
                            "{(appearance.inner_glow_strength * 100.0) as i32}%"
                        }
                    }
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
                }
            }

            div { class: "appearance-preview-area",
                div { class: "preview-label", "Preview" }
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
                        circle { cx: "28", cy: "28", r: "28", fill: "rgba(0,0,0,0.62)" }
                        // Head
                        circle { cx: "28", cy: "21", r: "9", fill: "#3a3a3a" }
                        // Body / shoulders
                        ellipse { cx: "28", cy: "42", rx: "15", ry: "10", fill: "#3a3a3a" }
                    }
                }
            }
        }
    }
}

fn slider_fill_style(_value: f32, color: GlowColor) -> String {
    let (red, green, blue) = color.to_rgb();
    format!(
        "--thumb-glow: rgba({red},{green},{blue},0.92); --thumb-halo: rgba({red},{green},{blue},0.38);"
    )
}

/// Compute a static glow style for the appearance preview tile.
///
/// Uses the same formula as `speak_style` but with fixed outer/inner intensity
/// constants so the preview is always visible regardless of microphone state.
/// The CSS `preview-tile-pulsing` animation provides visual dynamism.
fn preview_glow_style(settings: &AppearanceSettings) -> String {
    if !settings.glow_enabled {
        return "box-shadow: none; border-color: rgba(255, 255, 255, 0.08);".to_string();
    }

    const INTENSITY: f32 = 0.65;

    let (r, g, b) = settings.glow_color.to_rgb();
    let brightness = settings.glow_brightness.clamp(0.0, 1.0);
    let inner_strength = settings.inner_glow_strength.clamp(0.0, 1.0);
    let brightness_curve = brightness * brightness;
    let inner_curve = inner_strength * inner_strength;

    let outer_blur = 14.0 + INTENSITY * (14.0 + brightness_curve * 10.0);
    let outer_spread = 1.0 + INTENSITY * (2.0 + brightness_curve * 4.0);
    let outer_alpha = (0.18 + INTENSITY * 0.32) * brightness_curve;
    let inner_blur = 10.0 + INTENSITY * (10.0 + inner_curve * 12.0);
    let inner_alpha = (0.10 + INTENSITY * 0.22) * brightness_curve * (0.25 + inner_curve * 0.75);
    let border_alpha = (0.50 + INTENSITY * 0.42).clamp(0.45, 0.92);

    format!(
        "box-shadow: 0 0 {outer_blur:.0}px {outer_spread:.0}px rgba({r}, {g}, {b}, {outer_alpha:.2}), \
         inset 0 0 {inner_blur:.0}px 0 rgba({r}, {g}, {b}, {inner_alpha:.2}); \
         border-color: rgba({r}, {g}, {b}, {border_alpha:.2});",
    )
}

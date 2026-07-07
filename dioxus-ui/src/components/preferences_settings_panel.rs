/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use crate::components::density::{DensityMode, DENSITY_MODES};
use crate::context::{
    save_decode_budget_override, save_density_mode, save_dock_autohide, save_dock_position,
    AppearanceSettings, AppearanceSettingsCtx, AutohideCtx, DecodeBudgetCtx, DecodeBudgetOverride,
    DensityModeCtx, DockPosition, DockPositionCtx,
};
use dioxus::prelude::*;

#[component]
pub fn PreferencesSettingsPanel() -> Element {
    // Fallback signals for when contexts are not provided (e.g. in tests or
    // isolated component previews). Hooks must be called unconditionally, so we
    // always create them — but any writes the panel makes through these fallback
    // signals stay local to this component instance and do NOT propagate to
    // attendants.rs or any other reader. Production always provides the real context.
    let fallback_dock = use_signal(|| DockPosition::Bottom);
    let fallback_autohide = use_signal(|| false);
    let fallback_density = use_signal(|| DensityMode::Auto);
    let fallback_decode_budget = use_signal(DecodeBudgetOverride::default);
    let mut dock_position_ctx =
        try_use_context::<DockPositionCtx>().unwrap_or(DockPositionCtx(fallback_dock));
    let mut autohide_ctx =
        try_use_context::<AutohideCtx>().unwrap_or(AutohideCtx(fallback_autohide));
    let mut density_ctx =
        try_use_context::<DensityModeCtx>().unwrap_or(DensityModeCtx(fallback_density));
    let mut decode_budget_ctx =
        try_use_context::<DecodeBudgetCtx>().unwrap_or(DecodeBudgetCtx(fallback_decode_budget));
    let mut appearance_ctx = use_context::<AppearanceSettingsCtx>();
    let appearance = (appearance_ctx.0)();

    rsx! {
        div { class: "appearance-settings-panel",
            div { class: "appearance-content-column",

                // ── Section 1: Action Bar ────────────────────────────────────
                section { class: "appearance-section",
                    div { class: "appearance-section-header",
                        h3 { class: "appearance-section-title", "Action Bar" }
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

                // ── Section 2: Tiling ────────────────────────────────────────
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

                    // Video-tiles (decode-budget) override — sits directly under
                    // Density and reuses the same segmented control vocabulary.
                    div { class: "device-setting-group",
                        span { class: "transport-segmented-label", "Video tiles" }
                        div {
                            id: "decode-budget-override",
                            class: "transport-segmented",
                            role: "radiogroup",
                            "aria-label": "Number of video tiles to decode",
                            for option in DECODE_BUDGET_OPTIONS {
                                {
                                    let is_selected = decode_budget_ctx.0() == option;
                                    rsx! {
                                        button {
                                            r#type: "button",
                                            role: "radio",
                                            "data-testid": decode_budget_testid(option),
                                            "aria-checked": if is_selected { "true" } else { "false" },
                                            "aria-label": decode_budget_aria_label(option),
                                            class: if is_selected { "transport-segmented-option selected" } else { "transport-segmented-option" },
                                            onclick: move |_| {
                                                decode_budget_ctx.0.set(option);
                                                save_decode_budget_override(option);
                                            },
                                            "{decode_budget_label(option)}"
                                        }
                                    }
                                }
                            }
                        }
                        p { class: "appearance-section-helper",
                            "Auto reduces video tiles on slower devices to keep playback smooth — off-budget participants stay audible and appear as avatars. A fixed number always shows that many video tiles."
                        }
                    }
                }

                hr { class: "appearance-section-divider" }

                // ── Section 3: Notifications ─────────────────────────────────
                section { class: "appearance-section",
                    div { class: "appearance-section-header",
                        h3 { class: "appearance-section-title", "Notifications" }
                    }

                    // Entry/exit notifications toggle
                    div { class: "appearance-section-header dock-autohide-row",
                        div { class: "appearance-section-heading-stack",
                            label {
                                class: "appearance-section-title appearance-section-title--sm",
                                r#for: "join-leave-notifications-toggle",
                                "Entry/exit notifications"
                            }
                            p { class: "appearance-section-helper",
                                "Show a message when participants join or leave."
                            }
                        }
                        label {
                            class: "glow-switch",
                            "aria-label": "Toggle entry and exit notifications",
                            input {
                                id: "join-leave-notifications-toggle",
                                r#type: "checkbox",
                                checked: appearance.show_join_leave_notifications,
                                onchange: move |evt: Event<FormData>| {
                                    let enabled = evt.checked();
                                    appearance_ctx.0.set(AppearanceSettings {
                                        show_join_leave_notifications: enabled,
                                        ..appearance_ctx.0()
                                    });
                                },
                            }
                            span { class: "glow-switch-track" }
                        }
                    }

                    // Entry/exit sounds toggle
                    div { class: "appearance-section-header dock-autohide-row",
                        div { class: "appearance-section-heading-stack",
                            label {
                                class: "appearance-section-title appearance-section-title--sm",
                                r#for: "join-leave-sounds-toggle",
                                "Entry/exit sounds"
                            }
                            p { class: "appearance-section-helper",
                                "Play a sound when participants join or leave."
                            }
                        }
                        label {
                            class: "glow-switch",
                            "aria-label": "Toggle entry and exit sounds",
                            input {
                                id: "join-leave-sounds-toggle",
                                r#type: "checkbox",
                                checked: appearance.play_join_leave_sounds,
                                onchange: move |evt: Event<FormData>| {
                                    let enabled = evt.checked();
                                    appearance_ctx.0.set(AppearanceSettings {
                                        play_join_leave_sounds: enabled,
                                        ..appearance_ctx.0()
                                    });
                                },
                            }
                            span { class: "glow-switch-track" }
                        }
                    }
                }
            }
        }
    }
}

/// Manual decode-budget choices offered in the "Video tiles" control.
///
/// `Auto` (the default) hands the tile count to the adaptive control loop in
/// `attendants.rs`. The fixed values are a short, sensible progression bounded
/// by the layout caps: every value is `<= CANVAS_LIMIT` (30) and the control
/// loop further clamps each choice to the natural tile count for the current
/// viewport, so picking a number larger than the grid can show is harmless. The
/// chosen counts (4 / 6 / 9 / 16) mirror the tile-count vocabulary the density
/// modes already describe (Standard ~4/~9, Auto ~6/~12, Dense ~16).
const DECODE_BUDGET_OPTIONS: [DecodeBudgetOverride; 5] = [
    DecodeBudgetOverride::Auto,
    DecodeBudgetOverride::Fixed(4),
    DecodeBudgetOverride::Fixed(6),
    DecodeBudgetOverride::Fixed(9),
    DecodeBudgetOverride::Fixed(16),
];

/// Short button label for a decode-budget option.
fn decode_budget_label(option: DecodeBudgetOverride) -> String {
    match option {
        DecodeBudgetOverride::Auto => "Auto".to_string(),
        DecodeBudgetOverride::All => "All".to_string(),
        DecodeBudgetOverride::Fixed(n) => n.to_string(),
    }
}

/// Descriptive `aria-label` for a decode-budget option so screen readers
/// announce the bare numbers meaningfully.
fn decode_budget_aria_label(option: DecodeBudgetOverride) -> String {
    match option {
        DecodeBudgetOverride::Auto => "Automatic video tile count".to_string(),
        DecodeBudgetOverride::All => "Show all video tiles".to_string(),
        DecodeBudgetOverride::Fixed(n) => format!("Show {n} video tiles"),
    }
}

/// Stable `data-testid` for a decode-budget option (consumed by 1a.6 E2E).
fn decode_budget_testid(option: DecodeBudgetOverride) -> String {
    match option {
        DecodeBudgetOverride::Auto => "decode-budget-auto".to_string(),
        DecodeBudgetOverride::All => "decode-budget-all".to_string(),
        DecodeBudgetOverride::Fixed(n) => format!("decode-budget-{n}"),
    }
}

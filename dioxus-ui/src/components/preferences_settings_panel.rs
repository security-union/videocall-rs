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

/// Which announcement-channel help tooltip is currently latched open by a
/// tap/click, or Escape-dismissed. Touch devices have no hover, so tapping the
/// (?) icon toggles the open latch; keyboard and pointer users still get the
/// tooltip purely via CSS (`:hover` / `:focus-within`), mirroring the shipped
/// `field-label__info` pattern in `home.rs`. Escape adds a per-icon
/// *suppression* that hides a still-focused tooltip without blurring or closing
/// the modal — see the Escape handling in the render below.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AnnounceHelp {
    Message,
    Sound,
}

/// Class string for an announcement help (?) icon given whether its tooltip is
/// click-latched open and/or Escape-suppressed. `--open` forces the tooltip
/// visible (the touch tap-latch); `--suppressed` forces it hidden even while the
/// icon keeps focus. The two are mutually exclusive by construction (opening
/// clears suppression and vice-versa), so suppression is checked first only
/// defensively.
fn announce_help_class(is_open: bool, is_suppressed: bool) -> &'static str {
    if is_suppressed {
        "field-label__info announce-help announce-help--right announce-help--suppressed"
    } else if is_open {
        "field-label__info announce-help announce-help--right field-label__info--open"
    } else {
        "field-label__info announce-help announce-help--right"
    }
}

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

    // Latches the tapped/clicked announcement-channel tooltip open (touch has no
    // hover). Hover and keyboard focus still reveal the tooltip via CSS alone.
    let mut open_help = use_signal(|| Option::<AnnounceHelp>::None);
    // Escape-dismissal: while the icon keeps focus, `:focus-within` would keep
    // its tooltip on screen, so a dismissed icon is tracked here and hidden via
    // CSS — without blurring (focus stays on the trigger) or closing the modal.
    let mut suppressed_help = use_signal(|| Option::<AnnounceHelp>::None);

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
                //
                // A 2×2 announcement matrix: rows are the participant events
                // (joins / leaves), columns are the delivery channels (an
                // on-screen Message and a Sound). Each of the four cells is one
                // `.glow-switch`, preserving the original input ids so stored
                // preferences and existing selectors keep working. The per-cell
                // meaning is carried by the grid axes: each switch is named via
                // `aria-labelledby="<row-id> <col-id>"` (e.g. "Participant joins
                // Message"), and the two column headers carry a reused
                // `field-label__info` (?) help icon whose tooltip explains the
                // channel — so the four helper sentences collapse to two column
                // tooltips.
                section { class: "appearance-section",
                    div { class: "appearance-section-header",
                        h3 { class: "appearance-section-title", "Notifications" }
                    }

                    div {
                        class: "announce-matrix",
                        role: "group",
                        "aria-label": "Participant announcements",
                        "data-testid": "announce-matrix",

                        // ── Header row: empty corner + two channel headers ──
                        span { class: "announce-matrix__corner" }

                        div { class: "announce-matrix__col-head",
                            span {
                                id: "announce-col-message",
                                class: "announce-matrix__col-label",
                                "Message"
                            }
                            span {
                                class: announce_help_class(
                                    open_help() == Some(AnnounceHelp::Message),
                                    suppressed_help() == Some(AnnounceHelp::Message),
                                ),
                                role: "button",
                                tabindex: 0,
                                "aria-label": "About message announcements",
                                "aria-describedby": "announce-tip-message",
                                "data-testid": "announce-help-message",
                                onclick: move |e| {
                                    e.stop_propagation();
                                    // Explicit open wins over a prior Escape-dismissal.
                                    suppressed_help.set(None);
                                    open_help.set(if open_help() == Some(AnnounceHelp::Message) { None } else { Some(AnnounceHelp::Message) });
                                },
                                onkeydown: move |e| {
                                    let key = e.key();
                                    if key == Key::Enter || key == Key::Character(" ".to_string()) {
                                        e.prevent_default();
                                        e.stop_propagation();
                                        suppressed_help.set(None);
                                        open_help.set(if open_help() == Some(AnnounceHelp::Message) { None } else { Some(AnnounceHelp::Message) });
                                    } else if key == Key::Escape && suppressed_help() != Some(AnnounceHelp::Message) {
                                        // First Escape while the tooltip shows: dismiss ONLY the
                                        // tooltip. Stop propagation so the modal's own Escape
                                        // handler does NOT close it, and do NOT blur — focus stays
                                        // on the trigger (WCAG 2.1 SC 1.4.13). A second Escape
                                        // (already suppressed) falls through and bubbles, letting
                                        // the modal close as usual.
                                        e.stop_propagation();
                                        open_help.set(None);
                                        suppressed_help.set(Some(AnnounceHelp::Message));
                                    }
                                },
                                onfocusout: move |_| {
                                    open_help.set(None);
                                    suppressed_help.set(None);
                                },
                                svg {
                                    class: "field-label__info-icon",
                                    xmlns: "http://www.w3.org/2000/svg",
                                    width: 14,
                                    height: 14,
                                    view_box: "0 0 16 16",
                                    fill: "none",
                                    stroke: "currentColor",
                                    stroke_width: 1.6,
                                    stroke_linecap: "round",
                                    stroke_linejoin: "round",
                                    circle { cx: 8, cy: 8, r: 6.75 }
                                    line { x1: 8, y1: 7.25, x2: 8, y2: 11.25 }
                                    circle { cx: 8, cy: 5, r: 0.55, fill: "currentColor", stroke: "none" }
                                }
                                span {
                                    id: "announce-tip-message",
                                    class: "field-label__tooltip",
                                    role: "tooltip",
                                    "Show an on-screen message when someone joins or leaves."
                                }
                            }
                        }

                        div { class: "announce-matrix__col-head",
                            span {
                                id: "announce-col-sound",
                                class: "announce-matrix__col-label",
                                "Sound"
                            }
                            span {
                                class: announce_help_class(
                                    open_help() == Some(AnnounceHelp::Sound),
                                    suppressed_help() == Some(AnnounceHelp::Sound),
                                ),
                                role: "button",
                                tabindex: 0,
                                "aria-label": "About sound announcements",
                                "aria-describedby": "announce-tip-sound",
                                "data-testid": "announce-help-sound",
                                onclick: move |e| {
                                    e.stop_propagation();
                                    // Explicit open wins over a prior Escape-dismissal.
                                    suppressed_help.set(None);
                                    open_help.set(if open_help() == Some(AnnounceHelp::Sound) { None } else { Some(AnnounceHelp::Sound) });
                                },
                                onkeydown: move |e| {
                                    let key = e.key();
                                    if key == Key::Enter || key == Key::Character(" ".to_string()) {
                                        e.prevent_default();
                                        e.stop_propagation();
                                        suppressed_help.set(None);
                                        open_help.set(if open_help() == Some(AnnounceHelp::Sound) { None } else { Some(AnnounceHelp::Sound) });
                                    } else if key == Key::Escape && suppressed_help() != Some(AnnounceHelp::Sound) {
                                        // First Escape while the tooltip shows: dismiss ONLY the
                                        // tooltip. Stop propagation so the modal's own Escape
                                        // handler does NOT close it, and do NOT blur — focus stays
                                        // on the trigger (WCAG 2.1 SC 1.4.13). A second Escape
                                        // (already suppressed) falls through and bubbles, letting
                                        // the modal close as usual.
                                        e.stop_propagation();
                                        open_help.set(None);
                                        suppressed_help.set(Some(AnnounceHelp::Sound));
                                    }
                                },
                                onfocusout: move |_| {
                                    open_help.set(None);
                                    suppressed_help.set(None);
                                },
                                svg {
                                    class: "field-label__info-icon",
                                    xmlns: "http://www.w3.org/2000/svg",
                                    width: 14,
                                    height: 14,
                                    view_box: "0 0 16 16",
                                    fill: "none",
                                    stroke: "currentColor",
                                    stroke_width: 1.6,
                                    stroke_linecap: "round",
                                    stroke_linejoin: "round",
                                    circle { cx: 8, cy: 8, r: 6.75 }
                                    line { x1: 8, y1: 7.25, x2: 8, y2: 11.25 }
                                    circle { cx: 8, cy: 5, r: 0.55, fill: "currentColor", stroke: "none" }
                                }
                                span {
                                    id: "announce-tip-sound",
                                    class: "field-label__tooltip",
                                    role: "tooltip",
                                    "Play a chime when someone joins or leaves."
                                }
                            }
                        }

                        // ── Row 1: Participant joins ──
                        span {
                            id: "announce-row-join",
                            class: "announce-matrix__row-label",
                            "Participant joins"
                        }
                        label { class: "glow-switch",
                            input {
                                id: "entry-notifications-toggle",
                                r#type: "checkbox",
                                "aria-labelledby": "announce-row-join announce-col-message",
                                "data-testid": "announce-join-message",
                                checked: appearance.show_entry_notifications,
                                onchange: move |evt: Event<FormData>| {
                                    let enabled = evt.checked();
                                    appearance_ctx.0.set(AppearanceSettings {
                                        show_entry_notifications: enabled,
                                        ..appearance_ctx.0()
                                    });
                                },
                            }
                            span { class: "glow-switch-track" }
                        }
                        label { class: "glow-switch",
                            input {
                                id: "entry-sound-toggle",
                                r#type: "checkbox",
                                "aria-labelledby": "announce-row-join announce-col-sound",
                                "data-testid": "announce-join-sound",
                                checked: appearance.play_entry_sound,
                                onchange: move |evt: Event<FormData>| {
                                    let enabled = evt.checked();
                                    appearance_ctx.0.set(AppearanceSettings {
                                        play_entry_sound: enabled,
                                        ..appearance_ctx.0()
                                    });
                                },
                            }
                            span { class: "glow-switch-track" }
                        }

                        // ── Row 2: Participant leaves ──
                        span {
                            id: "announce-row-leave",
                            class: "announce-matrix__row-label",
                            "Participant leaves"
                        }
                        label { class: "glow-switch",
                            input {
                                id: "exit-notifications-toggle",
                                r#type: "checkbox",
                                "aria-labelledby": "announce-row-leave announce-col-message",
                                "data-testid": "announce-leave-message",
                                checked: appearance.show_exit_notifications,
                                onchange: move |evt: Event<FormData>| {
                                    let enabled = evt.checked();
                                    appearance_ctx.0.set(AppearanceSettings {
                                        show_exit_notifications: enabled,
                                        ..appearance_ctx.0()
                                    });
                                },
                            }
                            span { class: "glow-switch-track" }
                        }
                        label { class: "glow-switch",
                            input {
                                id: "exit-sound-toggle",
                                r#type: "checkbox",
                                "aria-labelledby": "announce-row-leave announce-col-sound",
                                "data-testid": "announce-leave-sound",
                                checked: appearance.play_exit_sound,
                                onchange: move |evt: Event<FormData>| {
                                    let enabled = evt.checked();
                                    appearance_ctx.0.set(AppearanceSettings {
                                        play_exit_sound: enabled,
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

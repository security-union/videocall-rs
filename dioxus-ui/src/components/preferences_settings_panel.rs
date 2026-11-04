/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use crate::components::action_bar_layout::{
    load_shrunk_buttons, remove_shrunk_buttons, save_shrunk_buttons, ShrunkButtons, DEFAULT_SLOTS,
    DOCK_AUTOHIDE_HINT, DOCK_AUTOSHRINK_HINT,
};
use crate::components::announcement_overrides::{
    override_change_announcement, AnnouncementOverride, AnnouncementOverrides,
    AnnouncementOverridesCtx, OverrideField,
};
use crate::components::attendants::action_bar_announce_text;
use crate::components::density::{DensityMode, DENSITY_MODES};
use crate::components::recent_actions::{RecentActionsLimitControl, SettingsTarget};
use crate::context::{
    load_dock_autoshrink, save_decode_budget_override, save_density_mode, save_dock_autohide,
    save_dock_autoshrink, save_dock_position, save_self_view_placement, save_self_view_visible,
    AppearanceSettings, AppearanceSettingsCtx, AutohideCtx, AutoshrinkCtx, DecodeBudgetCtx,
    DecodeBudgetOverride, DensityModeCtx, DockPosition, DockPositionCtx, SelfViewPlacement,
    SelfViewPlacementCtx, SelfViewVisibleCtx, ShrinkChoicesCtx, ShrunkButtonsCtx,
};
use dioxus::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

const ANNOUNCE_HOST_LOCK_ID: &str = "announce-host-lock";

fn host_locked(ctx: Option<AnnouncementOverridesCtx>, field: OverrideField) -> bool {
    ctx.is_some_and(|ctx| !ctx.0.peek().get(field).is_participant())
}

/// One join/leave switch. While the host overrides it, it shows the forced
/// value and never writes the participant's own preference.
#[allow(clippy::too_many_arguments)]
fn announce_switch(
    input_id: &'static str,
    labelledby: &'static str,
    testid: &'static str,
    field: OverrideField,
    lock: AnnouncementOverride,
    own: bool,
    overrides_ctx: Option<AnnouncementOverridesCtx>,
    mut appearance: Signal<AppearanceSettings>,
    write: fn(&mut AppearanceSettings, bool),
) -> Element {
    let locked = !lock.is_participant();
    rsx! {
        label { class: if locked { "glow-switch glow-switch--locked" } else { "glow-switch" },
            input {
                id: input_id,
                r#type: "checkbox",
                "aria-labelledby": labelledby,
                "aria-disabled": locked.then_some("true"),
                "aria-describedby": locked.then_some(ANNOUNCE_HOST_LOCK_ID),
                "data-testid": testid,
                checked: lock.resolve(own),
                onclick: move |evt: MouseEvent| {
                    if host_locked(overrides_ctx, field) {
                        evt.prevent_default();
                    }
                },
                onchange: move |evt: Event<FormData>| {
                    if host_locked(overrides_ctx, field) {
                        return;
                    }
                    let mut next = *appearance.peek();
                    write(&mut next, evt.checked());
                    appearance.set(next);
                },
            }
            span { class: "glow-switch-track" }
        }
    }
}

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
    let fallback_self_placement = use_signal(SelfViewPlacement::default);
    let fallback_self_visible = use_signal(|| true);
    let mut dock_position_ctx =
        try_use_context::<DockPositionCtx>().unwrap_or(DockPositionCtx(fallback_dock));
    let mut self_placement_ctx = try_use_context::<SelfViewPlacementCtx>()
        .unwrap_or(SelfViewPlacementCtx(fallback_self_placement));
    let mut self_visible_ctx = try_use_context::<SelfViewVisibleCtx>()
        .unwrap_or(SelfViewVisibleCtx(fallback_self_visible));
    let mut autohide_ctx =
        try_use_context::<AutohideCtx>().unwrap_or(AutohideCtx(fallback_autohide));
    let fallback_autoshrink = use_signal(load_dock_autoshrink);
    let fallback_shrunk = use_signal(load_shrunk_buttons);
    let fallback_choices = use_signal(|| DEFAULT_SLOTS.to_vec());
    let mut autoshrink_ctx =
        try_use_context::<AutoshrinkCtx>().unwrap_or(AutoshrinkCtx(fallback_autoshrink));
    let mut shrunk_ctx =
        try_use_context::<ShrunkButtonsCtx>().unwrap_or(ShrunkButtonsCtx(fallback_shrunk));
    let choices_ctx =
        try_use_context::<ShrinkChoicesCtx>().unwrap_or(ShrinkChoicesCtx(fallback_choices));
    let kept = shrunk_ctx.0();
    let mut kept_reset_nonce = use_signal(|| 0u32);
    let mut density_ctx =
        try_use_context::<DensityModeCtx>().unwrap_or(DensityModeCtx(fallback_density));
    let mut decode_budget_ctx =
        try_use_context::<DecodeBudgetCtx>().unwrap_or(DecodeBudgetCtx(fallback_decode_budget));
    let mut appearance_ctx = use_context::<AppearanceSettingsCtx>();
    let appearance = (appearance_ctx.0)();
    let overrides_ctx = try_use_context::<AnnouncementOverridesCtx>();
    let overrides = overrides_ctx.map(|ctx| (ctx.0)()).unwrap_or_default();
    let any_locked = OverrideField::ALL
        .into_iter()
        .any(|field| !overrides.get(field).is_participant());
    let mut lock_announcement = use_signal(|| (String::new(), 0u32));
    let seen_overrides = use_hook(|| Rc::new(Cell::new(None::<AnnouncementOverrides>)));
    use_effect(move || {
        let Some(ctx) = overrides_ctx else {
            return;
        };
        let next = (ctx.0)();
        let Some(prev) = seen_overrides.replace(Some(next)) else {
            return;
        };
        if let Some(message) = override_change_announcement(&prev, &next) {
            let nonce = lock_announcement.peek().1.wrapping_add(1);
            lock_announcement.set((message, nonce));
        }
    });
    let lock_status = {
        let (message, nonce) = lock_announcement();
        action_bar_announce_text(&message, nonce)
    };

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
                        "data-recent-action": SettingsTarget::ActionBarPosition.slug(),
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
                        "data-recent-action": SettingsTarget::AutoHide.slug(),
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
                    p { class: "appearance-section-helper", {DOCK_AUTOHIDE_HINT} }

                    div { class: "appearance-section-header dock-autohide-row",
                        "data-recent-action": SettingsTarget::AutoShrink.slug(),
                        label { class: "appearance-section-title appearance-section-title--sm", "Auto-shrink" }
                        label {
                            class: "glow-switch",
                            input {
                                r#type: "checkbox",
                                "aria-label": "Auto-shrink action bar",
                                "data-testid": "action-bar-autoshrink-toggle",
                                checked: autoshrink_ctx.0(),
                                onchange: move |evt: Event<FormData>| {
                                    let checked = evt.checked();
                                    autoshrink_ctx.0.set(checked);
                                    save_dock_autoshrink(checked);
                                },
                            }
                            span { class: "glow-switch-track" }
                        }
                    }
                    p { class: "appearance-section-helper", {DOCK_AUTOSHRINK_HINT} }

                    if autoshrink_ctx.0() {
                        fieldset { class: "meetings-filter-group action-bar-kept",
                            legend { class: "transport-segmented-label", "Buttons to keep" }
                            div { class: "action-bar-kept-list",
                                for slot in choices_ctx.0().into_iter() {
                                    label {
                                        key: "{slot.slug()}",
                                        class: "meetings-filter-option",
                                        input {
                                            r#type: "checkbox",
                                            "data-testid": "action-bar-kept-{slot.slug()}",
                                            checked: kept.keeps(slot),
                                            onchange: move |evt: Event<FormData>| {
                                                let next = shrunk_ctx.0.peek().with_slot(slot, evt.checked());
                                                save_shrunk_buttons(&next);
                                                shrunk_ctx.0.set(next);
                                            },
                                        }
                                        span { "{slot.display_name()}" }
                                    }
                                }
                                label { class: "meetings-filter-option",
                                    input {
                                        r#type: "checkbox",
                                        "data-testid": "action-bar-kept-hangup",
                                        checked: kept.hangup,
                                        onchange: move |evt: Event<FormData>| {
                                            let next = ShrunkButtons {
                                                hangup: evt.checked(),
                                                ..shrunk_ctx.0.peek().clone()
                                            };
                                            save_shrunk_buttons(&next);
                                            shrunk_ctx.0.set(next);
                                        },
                                    }
                                    span { "Hang up" }
                                }
                            }
                        }
                        p { class: "appearance-section-helper",
                            "These stay on the bar when it shrinks. With none selected, it shrinks to a small handle."
                        }
                        button {
                            r#type: "button",
                            class: "theme-reset-btn action-bar-kept-reset",
                            "aria-label": "Reset to default kept buttons",
                            "data-testid": "action-bar-kept-reset",
                            onclick: move |_| {
                                remove_shrunk_buttons();
                                shrunk_ctx.0.set(ShrunkButtons::default());
                                kept_reset_nonce += 1;
                            },
                            "Reset to default"
                        }
                        p {
                            class: "visually-hidden",
                            role: "status",
                            "aria-live": "polite",
                            "aria-atomic": "true",
                            "data-testid": "action-bar-kept-status",
                            {
                                if kept_reset_nonce() == 0 {
                                    String::new()
                                } else {
                                    action_bar_announce_text("Kept buttons reset to default.", kept_reset_nonce())
                                }
                            }
                        }
                    }

                    RecentActionsLimitControl {}
                }

                hr { class: "appearance-section-divider" }

                // ── Section 2: Tiling ────────────────────────────────────────
                section { class: "appearance-section",
                    div { class: "appearance-section-header",
                        h3 { class: "appearance-section-title", "Tiling" }
                    }

                    div { class: "device-setting-group",
                        "data-recent-action": SettingsTarget::Density.slug(),
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

                    // Self view (issue 66) — directly under Density.
                    div { class: "device-setting-group",
                        "data-recent-action": SettingsTarget::SelfView.slug(),
                        span { class: "transport-segmented-label", "Self view" }
                        div {
                            class: "transport-segmented",
                            role: "radiogroup",
                            "aria-label": "Self view placement",
                            for (placement, label) in [(SelfViewPlacement::Corner, "Corner"), (SelfViewPlacement::Grid, "Grid")] {
                                button {
                                    r#type: "button",
                                    role: "radio",
                                    "data-testid": "self-view-placement-{placement.as_str()}",
                                    "aria-checked": if self_placement_ctx.0() == placement { "true" } else { "false" },
                                    class: if self_placement_ctx.0() == placement { "transport-segmented-option selected" } else { "transport-segmented-option" },
                                    onclick: move |_| {
                                        self_placement_ctx.0.set(placement);
                                        save_self_view_placement(placement);
                                    },
                                    "{label}"
                                }
                            }
                        }
                    }

                    div { class: "appearance-section-header dock-autohide-row",
                        "data-recent-action": SettingsTarget::SelfView.slug(),
                        label { class: "appearance-section-title appearance-section-title--sm", "Show self view" }
                        label {
                            class: "glow-switch",
                            input {
                                r#type: "checkbox",
                                "aria-label": "Show self view",
                                "data-testid": "self-view-visible-checkbox",
                                checked: self_visible_ctx.0(),
                                onchange: move |evt: Event<FormData>| {
                                    let checked = evt.checked();
                                    self_visible_ctx.0.set(checked);
                                    save_self_view_visible(checked);
                                },
                            }
                            span { class: "glow-switch-track" }
                        }
                    }
                    p { class: "appearance-section-helper",
                        "Only affects your view — others still see you."
                    }

                    // Video-tiles (decode-budget) override — sits directly under
                    // Density and reuses the same segmented control vocabulary.
                    div { class: "device-setting-group",
                        "data-recent-action": SettingsTarget::VideoTiles.slug(),
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
                        {
                            let (target, label) = show_all_toggle_action(decode_budget_ctx.0());
                            rsx! {
                                button {
                                    r#type: "button",
                                    class: "decode-budget-show-all-btn",
                                    "data-testid": "decode-budget-show-all-persistent",
                                    onclick: move |_| {
                                        decode_budget_ctx.0.set(target);
                                        save_decode_budget_override(target);
                                    },
                                    "{label}"
                                }
                            }
                        }
                    }
                }

                hr { class: "appearance-section-divider" }

                // ── Section 3: Notifications ─────────────────────────────────
                //
                // An announcement matrix: rows are the events (a participant
                // joins, a participant leaves, and — since issue 2329 — a hand
                // going up or down), columns are the delivery channels (an
                // on-screen Message and a Sound). Each cell is one
                // `.glow-switch`, preserving the original input ids so stored
                // preferences and existing selectors keep working. The per-cell
                // meaning is carried by the grid axes: each switch is named via
                // `aria-labelledby="<row-id> <col-id>"` (e.g. "Participant joins
                // Message"), and the two column headers carry a reused
                // `field-label__info` (?) help icon whose tooltip explains the
                // channel — so the per-cell helper sentences collapse to two
                // column tooltips.
                //
                // The grid is NOT fully populated: the hand row has a Sound cell
                // and no Message cell, because the raised-hand banner is already
                // a permanent on-screen message surface for that state and is not
                // optional. Its Message cell renders a decorative placeholder so
                // auto-placement keeps the Sound switch under its own column.
                section { class: "appearance-section",
                    "data-recent-action": SettingsTarget::Notifications.slug(),
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
                                    // Issue 2329 added the hand row, so this
                                    // column no longer describes only joins and
                                    // leaves. The copy names all three events
                                    // rather than going vague ("for these
                                    // events"), because it is the only place the
                                    // Sound column explains what it covers.
                                    "Play a chime when someone joins, leaves, or raises their hand."
                                }
                            }
                        }

                        // ── Row 1: Participant joins ──
                        span {
                            id: "announce-row-join",
                            class: "announce-matrix__row-label",
                            "Participant joins"
                        }
                        {announce_switch(
                            "entry-notifications-toggle",
                            "announce-row-join announce-col-message",
                            "announce-join-message",
                            OverrideField::EntryMessage,
                            overrides.entry_message,
                            appearance.show_entry_notifications,
                            overrides_ctx,
                            appearance_ctx.0,
                            |s, on| s.show_entry_notifications = on,
                        )}
                        {announce_switch(
                            "entry-sound-toggle",
                            "announce-row-join announce-col-sound",
                            "announce-join-sound",
                            OverrideField::EntrySound,
                            overrides.entry_sound,
                            appearance.play_entry_sound,
                            overrides_ctx,
                            appearance_ctx.0,
                            |s, on| s.play_entry_sound = on,
                        )}

                        // ── Row 2: Participant leaves ──
                        span {
                            id: "announce-row-leave",
                            class: "announce-matrix__row-label",
                            "Participant leaves"
                        }
                        {announce_switch(
                            "exit-notifications-toggle",
                            "announce-row-leave announce-col-message",
                            "announce-leave-message",
                            OverrideField::ExitMessage,
                            overrides.exit_message,
                            appearance.show_exit_notifications,
                            overrides_ctx,
                            appearance_ctx.0,
                            |s, on| s.show_exit_notifications = on,
                        )}
                        {announce_switch(
                            "exit-sound-toggle",
                            "announce-row-leave announce-col-sound",
                            "announce-leave-sound",
                            OverrideField::ExitSound,
                            overrides.exit_sound,
                            appearance.play_exit_sound,
                            overrides_ctx,
                            appearance_ctx.0,
                            |s, on| s.play_exit_sound = on,
                        )}

                        // ── Row 3: Hand raised or lowered (issue 2329) ──
                        //
                        // Named for the STATE CHANGE rather than for the actor
                        // ("Participant raises hand") because the one switch
                        // governs both directions, and a user deciding whether to
                        // keep it on needs to know it also chimes on the way
                        // down.
                        span {
                            id: "announce-row-hand",
                            class: "announce-matrix__row-label",
                            "Hand raised or lowered"
                        }
                        // This row has NO Message cell. The raised-hand banner is
                        // already a persistent, room-wide message surface that
                        // stays on screen for as long as any hand is up (issue
                        // 2135), so a transient toast would duplicate a channel
                        // that never goes away — and it is not optional, so a
                        // switch here would promise control it cannot deliver.
                        //
                        // A placeholder cell is required rather than omitted:
                        // `.announce-matrix` is a 3-column grid with implicit
                        // auto-placement, so leaving the cell out would slide the
                        // Sound switch left into the Message column and misalign
                        // the whole row. Decorative and `aria-hidden` — there is
                        // no control here to announce.
                        span {
                            class: "announce-matrix__cell-empty",
                            "aria-hidden": "true",
                            "—"
                        }
                        label { class: "glow-switch",
                            input {
                                id: "hand-raise-sound-toggle",
                                r#type: "checkbox",
                                "aria-labelledby": "announce-row-hand announce-col-sound",
                                "data-testid": "announce-hand-sound",
                                checked: appearance.play_hand_raise_sound,
                                onchange: move |evt: Event<FormData>| {
                                    let enabled = evt.checked();
                                    appearance_ctx.0.set(AppearanceSettings {
                                        play_hand_raise_sound: enabled,
                                        ..appearance_ctx.0()
                                    });
                                },
                            }
                            span { class: "glow-switch-track" }
                        }
                    }
                    if any_locked {
                        p {
                            id: ANNOUNCE_HOST_LOCK_ID,
                            class: "announce-host-lock",
                            "data-testid": "announce-host-lock",
                            svg {
                                class: "announce-host-lock__icon",
                                "aria-hidden": "true",
                                xmlns: "http://www.w3.org/2000/svg",
                                width: 12,
                                height: 12,
                                view_box: "0 0 16 16",
                                fill: "none",
                                stroke: "currentColor",
                                stroke_width: 1.6,
                                stroke_linecap: "round",
                                stroke_linejoin: "round",
                                rect { x: 3, y: 7, width: 10, height: 7, rx: 1.5 }
                                path { d: "M5.5 7V5a2.5 2.5 0 0 1 5 0v2" }
                            }
                            span {
                                "Set by the host for this meeting. Your own choice returns when the host changes it back. While you record, join and leave messages still show."
                            }
                        }
                    }
                    p {
                        class: "visually-hidden",
                        role: "status",
                        "aria-live": "polite",
                        "aria-atomic": "true",
                        "data-testid": "announce-host-lock-status",
                        "{lock_status}"
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

fn show_all_toggle_action(current: DecodeBudgetOverride) -> (DecodeBudgetOverride, &'static str) {
    match current {
        DecodeBudgetOverride::Auto => (DecodeBudgetOverride::All, "Show all videos"),
        DecodeBudgetOverride::All | DecodeBudgetOverride::Fixed(_) => {
            (DecodeBudgetOverride::Auto, "Back to automatic")
        }
    }
}

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

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use crate::components::action_bar_layout::ActionBarSlot;
    use crate::components::announcement_overrides::AnnouncementOverride::{Off, On};
    use gloo_timers::future::TimeoutFuture;
    use std::cell::RefCell;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::wasm_bindgen_test;

    type Handles = (Signal<AnnouncementOverrides>, Signal<AppearanceSettings>);

    thread_local! {
        static HANDLES: RefCell<Option<Handles>> = const { RefCell::new(None) };
        static NO_MEETING_APPEARANCE: RefCell<Option<Signal<AppearanceSettings>>> =
            const { RefCell::new(None) };
    }

    #[allow(non_snake_case)]
    fn LockedHarness() -> Element {
        let appearance = use_signal(|| AppearanceSettings {
            play_entry_sound: false,
            ..Default::default()
        });
        let overrides = use_signal(|| AnnouncementOverrides {
            entry_message: Off,
            ..Default::default()
        });
        use_context_provider(|| AppearanceSettingsCtx(appearance));
        use_context_provider(|| AnnouncementOverridesCtx(overrides));
        use_hook(move || HANDLES.with(|h| *h.borrow_mut() = Some((overrides, appearance))));
        rsx! { PreferencesSettingsPanel {} }
    }

    #[allow(non_snake_case)]
    fn NoMeetingHarness() -> Element {
        let appearance = use_signal(|| AppearanceSettings {
            show_entry_notifications: false,
            ..Default::default()
        });
        use_context_provider(|| AppearanceSettingsCtx(appearance));
        use_hook(move || NO_MEETING_APPEARANCE.with(|a| *a.borrow_mut() = Some(appearance)));
        rsx! { PreferencesSettingsPanel {} }
    }

    async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..300 {
            if done() {
                return;
            }
            TimeoutFuture::new(10).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn mount(root_fn: fn() -> Element) -> web_sys::Element {
        let root = gloo_utils::document().create_element("div").unwrap();
        gloo_utils::document()
            .body()
            .unwrap()
            .append_child(&root)
            .unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(root_fn),
            dioxus::web::Config::new().rootelement(root.clone()),
        );
        wait_until("the panel", || {
            root.query_selector("#entry-notifications-toggle")
                .ok()
                .flatten()
                .is_some()
        })
        .await;
        root
    }

    fn switch(root: &web_sys::Element, id: &str) -> web_sys::HtmlInputElement {
        root.query_selector(&format!("#{id}"))
            .unwrap()
            .unwrap_or_else(|| panic!("no #{id}"))
            .unchecked_into()
    }

    fn locked(root: &web_sys::Element, id: &str) -> bool {
        switch(root, id).get_attribute("aria-disabled").as_deref() == Some("true")
    }

    fn status(root: &web_sys::Element) -> String {
        root.query_selector("[data-testid='announce-host-lock-status']")
            .unwrap()
            .expect("the always-mounted status region")
            .text_content()
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    fn handles() -> Handles {
        HANDLES.with(|h| h.borrow().expect("harness signals"))
    }

    #[wasm_bindgen_test]
    async fn a_host_lock_forces_the_switch_and_release_restores_the_own_choice() {
        let root = mount(LockedHarness).await;
        let (mut overrides, appearance) = handles();

        let join_message = switch(&root, "entry-notifications-toggle");
        assert!(
            !join_message.checked(),
            "forced off over the own choice (on)"
        );
        assert!(locked(&root, "entry-notifications-toggle"));
        assert_eq!(
            join_message.get_attribute("aria-describedby").as_deref(),
            Some("announce-host-lock")
        );
        assert!(!join_message.has_attribute("disabled"));
        assert!(join_message
            .closest(".glow-switch--locked")
            .unwrap()
            .is_some());
        let note = root.query_selector("#announce-host-lock").unwrap().unwrap();
        let note_text = note.text_content().unwrap_or_default();
        assert!(note_text.contains("Set by the host for this meeting."));
        assert!(note_text.ends_with("While you record, join and leave messages still show."));
        assert!(!locked(&root, "entry-sound-toggle"));
        assert!(!locked(&root, "hand-raise-sound-toggle"));
        assert_eq!(status(&root), "");

        join_message.click();
        TimeoutFuture::new(30).await;
        assert!(
            !join_message.checked(),
            "a click cannot flip a locked switch"
        );
        join_message.set_checked(false);
        let init = web_sys::EventInit::new();
        init.set_bubbles(true);
        join_message
            .dispatch_event(&web_sys::Event::new_with_event_init_dict("change", &init).unwrap())
            .unwrap();
        TimeoutFuture::new(30).await;
        assert!(appearance.peek().show_entry_notifications);
        assert!(!appearance.peek().play_entry_sound);

        overrides.with_mut(|o| o.entry_sound = On);
        wait_until("the join-sound lock", || {
            locked(&root, "entry-sound-toggle")
        })
        .await;
        assert!(switch(&root, "entry-sound-toggle").checked());
        assert_eq!(
            status(&root),
            "The host turned join sounds on for everyone."
        );

        overrides.set(AnnouncementOverrides::default());
        wait_until("the release", || {
            !locked(&root, "entry-notifications-toggle")
        })
        .await;
        assert!(switch(&root, "entry-notifications-toggle").checked());
        assert!(!switch(&root, "entry-sound-toggle").checked());
        assert!(!locked(&root, "entry-sound-toggle"));
        assert!(root
            .query_selector("#announce-host-lock")
            .unwrap()
            .is_none());
        assert_eq!(
            status(&root),
            "Join messages are your choice again. Join sounds are your choice again."
        );
        assert!(appearance.peek().show_entry_notifications);
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn outside_a_meeting_nothing_is_locked() {
        let root = mount(NoMeetingHarness).await;
        assert!(root
            .query_selector(".glow-switch--locked")
            .unwrap()
            .is_none());
        assert!(root
            .query_selector("#announce-host-lock")
            .unwrap()
            .is_none());
        let join_message = switch(&root, "entry-notifications-toggle");
        assert!(!join_message.checked());
        assert!(!locked(&root, "entry-notifications-toggle"));
        join_message.click();
        let appearance = NO_MEETING_APPEARANCE.with(|a| a.borrow().expect("appearance signal"));
        wait_until("the own choice to save", || {
            appearance.peek().show_entry_notifications
        })
        .await;
        root.remove();
    }

    fn clear_shrink_storage() {
        let storage = gloo_utils::window().local_storage().unwrap().unwrap();
        for key in [
            "vc_dock_autoshrink",
            "vc_dock_autohide",
            "vc_action_bar_shrunk",
        ] {
            storage.remove_item(key).unwrap();
        }
    }

    fn kept_box(root: &web_sys::Element, slug: &str) -> Option<web_sys::HtmlInputElement> {
        root.query_selector(&format!("[data-testid='action-bar-kept-{slug}']"))
            .unwrap()
            .map(JsCast::unchecked_into)
    }

    fn by_testid(root: &web_sys::Element, testid: &str) -> web_sys::HtmlElement {
        root.query_selector(&format!("[data-testid='{testid}']"))
            .unwrap()
            .unwrap_or_else(|| panic!("no {testid}"))
            .unchecked_into()
    }

    #[wasm_bindgen_test]
    async fn the_auto_shrink_switch_shows_and_hides_the_kept_list() {
        clear_shrink_storage();
        let root = mount(NoMeetingHarness).await;
        assert!(
            kept_box(&root, "hangup").is_none(),
            "off by default, so no list"
        );

        by_testid(&root, "action-bar-autoshrink-toggle").click();
        wait_until("the kept list", || kept_box(&root, "hangup").is_some()).await;
        assert!(
            crate::context::load_dock_autoshrink(),
            "the choice is saved"
        );
        for slot in DEFAULT_SLOTS {
            let row = kept_box(&root, slot.slug())
                .unwrap_or_else(|| panic!("outside a meeting the list offers {slot:?}"));
            assert_eq!(
                row.checked(),
                ShrunkButtons::default().keeps(*slot),
                "{slot:?}"
            );
        }
        assert!(kept_box(&root, "hangup").unwrap().checked());

        by_testid(&root, "action-bar-autoshrink-toggle").click();
        wait_until("the list to go", || kept_box(&root, "hangup").is_none()).await;
        assert!(!crate::context::load_dock_autoshrink());
        root.remove();
        clear_shrink_storage();
    }

    #[wasm_bindgen_test]
    async fn unselecting_a_kept_button_persists_and_reset_restores_the_default_set() {
        clear_shrink_storage();
        crate::context::save_dock_autoshrink(true);
        let root = mount(NoMeetingHarness).await;
        wait_until("the kept list", || kept_box(&root, "mic").is_some()).await;

        kept_box(&root, "mic").unwrap().click();
        kept_box(&root, "hangup").unwrap().click();
        kept_box(&root, "participants").unwrap().click();
        let expected = ShrunkButtons {
            slots: vec![
                ActionBarSlot::Camera,
                ActionBarSlot::ScreenShare,
                ActionBarSlot::PeerList,
            ],
            hangup: false,
        };
        wait_until("the choices to save", || load_shrunk_buttons() == expected).await;
        // Lets the unselect render, so the reset is a real unchecked-to-checked diff.
        TimeoutFuture::new(100).await;
        assert!(!kept_box(&root, "mic").unwrap().checked());
        assert_eq!(
            by_testid(&root, "action-bar-kept-status").text_content(),
            Some(String::new()),
            "nothing to announce before a reset"
        );

        by_testid(&root, "action-bar-kept-reset").click();
        wait_until("the reset", || kept_box(&root, "mic").unwrap().checked()).await;
        assert!(kept_box(&root, "hangup").unwrap().checked());
        assert!(!kept_box(&root, "participants").unwrap().checked());
        assert_eq!(load_shrunk_buttons(), ShrunkButtons::default());
        assert_eq!(
            gloo_utils::window()
                .local_storage()
                .unwrap()
                .unwrap()
                .get_item("vc_action_bar_shrunk")
                .unwrap(),
            None
        );
        assert!(
            crate::context::load_dock_autoshrink(),
            "reset leaves shrinking on"
        );
        wait_until("the reset announcement", || {
            by_testid(&root, "action-bar-kept-status")
                .text_content()
                .unwrap_or_default()
                .starts_with("Kept buttons reset to default.")
        })
        .await;
        root.remove();
        clear_shrink_storage();
    }

    #[wasm_bindgen_test]
    fn auto_shrink_inherits_auto_hide_once() {
        clear_shrink_storage();
        crate::context::save_dock_autohide(true);
        assert!(crate::context::load_dock_autoshrink());
        crate::context::save_dock_autohide(false);
        assert!(
            crate::context::load_dock_autoshrink(),
            "turning auto-hide off later does not touch shrinking"
        );

        clear_shrink_storage();
        assert!(!crate::context::load_dock_autoshrink());
        crate::context::save_dock_autohide(true);
        assert!(
            !crate::context::load_dock_autoshrink(),
            "turning auto-hide on later does not turn shrinking on"
        );
        clear_shrink_storage();
    }

    #[allow(non_snake_case)]
    fn LockedInSettingsPanel() -> Element {
        rsx! {
            div { class: "settings-panel", LockedHarness {} }
        }
    }

    fn add_sheet(css: &str) -> web_sys::Element {
        let style = gloo_utils::document().create_element("style").unwrap();
        style.set_text_content(Some(css));
        gloo_utils::document()
            .head()
            .unwrap()
            .append_child(&style)
            .unwrap();
        style
    }

    fn color(el: &web_sys::Element) -> String {
        gloo_utils::window()
            .get_computed_style(el)
            .unwrap()
            .unwrap()
            .get_property_value("color")
            .unwrap()
    }

    #[wasm_bindgen_test]
    async fn the_host_lock_note_paints_in_its_own_token_in_both_themes() {
        let sheets = [
            add_sheet(include_str!("../../static/global.css")),
            add_sheet(include_str!("../../static/style.css")),
        ];
        let html = gloo_utils::document().document_element().unwrap();
        let theme = html.get_attribute("data-theme");
        let probe = gloo_utils::document().create_element("span").unwrap();
        probe
            .set_attribute("style", "color: var(--announce-lock-note-text)")
            .unwrap();
        gloo_utils::document()
            .body()
            .unwrap()
            .append_child(&probe)
            .unwrap();
        let root = mount(LockedInSettingsPanel).await;
        let text = root
            .query_selector("#announce-host-lock > span")
            .unwrap()
            .unwrap();
        let icon = root
            .query_selector("#announce-host-lock > svg")
            .unwrap()
            .unwrap();

        let mut seen = Vec::new();
        for mode in ["light", "dark"] {
            html.set_attribute("data-theme", mode).unwrap();
            let token = color(&probe);
            assert_eq!(color(&text), token, "{mode} text");
            assert_eq!(color(&icon), token, "{mode} icon");
            seen.push(token);
        }
        assert_ne!(seen[0], seen[1], "each theme has its own value");

        match theme {
            Some(theme) => html.set_attribute("data-theme", &theme).unwrap(),
            None => html.remove_attribute("data-theme").unwrap(),
        }
        probe.remove();
        root.remove();
        sheets.iter().for_each(web_sys::Element::remove);
    }

    #[wasm_bindgen_test]
    fn the_lock_glyph_paints_canvas_text_in_forced_colors() {
        let sheet = add_sheet(include_str!("../../static/style.css"));
        let found = js_sys::eval(
            r#"(() => {
                for (const s of document.styleSheets) {
                    for (const r of s.cssRules) {
                        if (!(r.conditionText || '').includes('forced-colors: active')) continue;
                        for (const i of r.cssRules) {
                            if (i.selectorText === '.glow-switch--locked::before') {
                                return i.style.backgroundColor + '|' + i.style.forcedColorAdjust;
                            }
                        }
                    }
                }
                return '';
            })()"#,
        )
        .unwrap()
        .as_string()
        .unwrap_or_default();
        sheet.remove();
        assert_eq!(found.to_ascii_lowercase(), "canvastext|none");
    }
}

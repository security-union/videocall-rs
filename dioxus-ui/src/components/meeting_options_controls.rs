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
 */

//! Shared owner-only meeting-options toggles.
//!
//! This is the SINGLE source of truth for the four mutable meeting options
//! (Waiting Room, Admitted-can-admit, End-on-host-leave, Allow-guests). It is
//! rendered in three places so we never maintain duplicate copies of this UI:
//!   - the pre-join / startup card ([`crate::components::pre_join_settings_card`]),
//!   - the dedicated meeting-settings page ([`crate::pages::meeting_settings`]),
//!   - the in-call "Meeting options" panel (issue: in-meeting edit options).
//!
//! Each toggle optimistically flips its bound signal, PATCHes the meeting via
//! `update_meeting`, and rolls back on error. The host authorization for the
//! PATCH is enforced server-side, so this UI is only *gated* on ownership by
//! its callers (they pass owner-only) — it does not itself decide authority.
//!
//! On-the-fly semantics (waiting-room admit-all on disable, routing new joiners
//! to the waiting room on enable) are handled entirely by the server; toggling
//! here takes effect live for everyone via the existing
//! `on_meeting_settings_updated` push, with no client-side coordination.

use crate::components::toggle_switch::ToggleSwitch;
use dioxus::prelude::*;
use std::rc::Rc;

/// An info `(i)` glyph with a hover `title`.
fn info_icon(title: &str) -> Element {
    rsx! {
        span {
            class: "settings-info-icon",
            title: "{title}",
            svg {
                xmlns: "http://www.w3.org/2000/svg",
                width: "15",
                height: "15",
                view_box: "0 0 24 24",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "2",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                circle { cx: "12", cy: "12", r: "10" }
                line { x1: "12", y1: "16", x2: "12", y2: "12" }
                line { x1: "12", y1: "8", x2: "12.01", y2: "8" }
            }
        }
    }
}

/// Disabling the waiting room also clears admitted-can-admit, so on failure both
/// roll back: the waiting-room value and the prior admitted-can-admit value.
/// Enabling touches only the waiting-room value.
///
/// Returns `(waiting_room_restore, admitted_can_admit_restore)`; the second
/// element is `Some(prev)` only when the disable cleared it.
fn waiting_room_rollback(new_val: bool, prev_aca: bool) -> (bool, Option<bool>) {
    let waiting_room_restore = !new_val;
    let admitted_can_admit_restore = if new_val { None } else { Some(prev_aca) };
    (waiting_room_restore, admitted_can_admit_restore)
}

/// The four owner-editable meeting-option rows, wired to caller-owned signals.
#[component]
pub fn MeetingOptionsControls(
    meeting_id: String,
    waiting_room_toggle: Signal<bool>,
    admitted_can_admit_toggle: Signal<bool>,
    end_on_host_leave_toggle: Signal<bool>,
    allow_guests_toggle: Signal<bool>,
    saving: Signal<bool>,
    toggle_error: Signal<Option<String>>,
) -> Element {
    let update_setting = use_hook(|| {
        Rc::new(
            move |meeting_id: String,
                  waiting_room: Option<bool>,
                  admitted_can_admit: Option<bool>,
                  end_on_host_leave_opt: Option<bool>,
                  allow_guests_opt: Option<bool>,
                  mut rollback_signal: Signal<bool>,
                  old_val: bool,
                  secondary_rollback: Option<(Signal<bool>, bool)>,
                  mut saving: Signal<bool>,
                  mut toggle_error: Signal<Option<String>>| {
                saving.set(true);
                toggle_error.set(None);
                wasm_bindgen_futures::spawn_local(async move {
                    match crate::meeting_api::update_meeting(
                        &meeting_id,
                        waiting_room,
                        admitted_can_admit,
                        end_on_host_leave_opt,
                        allow_guests_opt,
                    )
                    .await
                    {
                        Ok(updated) => {
                            waiting_room_toggle.set(updated.waiting_room_enabled);
                            admitted_can_admit_toggle.set(updated.admitted_can_admit);
                            end_on_host_leave_toggle.set(updated.end_on_host_leave);
                            allow_guests_toggle.set(updated.allow_guests);
                            saving.set(false);
                        }
                        Err(e) => {
                            log::error!("Failed to update meeting setting: {e}");
                            rollback_signal.set(old_val);
                            // Restore any signal cleared as a side effect
                            // (waiting-room disable also clears admitted-can-admit).
                            if let Some((mut secondary_signal, secondary_old)) = secondary_rollback
                            {
                                secondary_signal.set(secondary_old);
                            }
                            saving.set(false);
                            toggle_error.set(Some(format!("Failed to update setting: {e}")));
                        }
                    }
                });
            },
        )
    });

    let aca_opacity = if waiting_room_toggle() { "1" } else { "0.5" };
    let mid = meeting_id.clone();

    rsx! {
        // ── Waiting Room ──────────────────────────────────────────────────
        div { class: "settings-option-row",
            span { class: "settings-option-label", "Waiting Room" }
            div { class: "settings-option-controls",
                {info_icon("Participants must be admitted by the host before joining")}
                ToggleSwitch {
                    enabled: waiting_room_toggle(),
                    disabled: saving(),
                    on_toggle: {
                        let meeting_id = mid.clone();
                        let update_setting = update_setting.clone();
                        move |new_val: bool| {
                            if saving() {
                                return;
                            }
                            // Capture prior admitted-can-admit before the optimistic
                            // clear, so a failed PATCH can restore it.
                            let prev_aca = admitted_can_admit_toggle();
                            let (old_val, aca_restore) =
                                waiting_room_rollback(new_val, prev_aca);
                            waiting_room_toggle.set(new_val);
                            // Disabling the waiting room also disables admitted-can-admit.
                            if !new_val {
                                admitted_can_admit_toggle.set(false);
                            }
                            let secondary_rollback =
                                aca_restore.map(|prev| (admitted_can_admit_toggle, prev));
                            let aca = if new_val { None } else { Some(false) };
                            update_setting(
                                meeting_id.clone(),
                                Some(new_val),
                                aca,
                                None,
                                None,
                                waiting_room_toggle,
                                old_val,
                                secondary_rollback,
                                saving,
                                toggle_error,
                            );
                        }
                    },
                }
            }
        }

        // ── Admitted can admit (only meaningful with waiting room ON) ──────
        div {
            class: "settings-option-row",
            style: "opacity: {aca_opacity};",
            span { class: "settings-option-label", "Admitted can admit" }
            div { class: "settings-option-controls",
                {info_icon("Allow admitted participants to also admit others from the waiting room")}
                ToggleSwitch {
                    enabled: admitted_can_admit_toggle(),
                    disabled: saving() || !waiting_room_toggle(),
                    on_toggle: {
                        let meeting_id = mid.clone();
                        let update_setting = update_setting.clone();
                        move |new_val: bool| {
                            if saving() || !waiting_room_toggle() {
                                return;
                            }
                            let old_val = !new_val;
                            admitted_can_admit_toggle.set(new_val);
                            update_setting(
                                meeting_id.clone(),
                                None,
                                Some(new_val),
                                None,
                                None,
                                admitted_can_admit_toggle,
                                old_val,
                                None,
                                saving,
                                toggle_error,
                            );
                        }
                    },
                }
            }
        }

        // ── End meeting when host leaves ──────────────────────────────────
        div { class: "settings-option-row", style: "opacity: 1;",
            span { class: "settings-option-label", "End meeting when host leaves" }
            div { class: "settings-option-controls",
                {info_icon("Automatically end the meeting for all participants when the host disconnects")}
                ToggleSwitch {
                    enabled: end_on_host_leave_toggle(),
                    disabled: saving(),
                    on_toggle: {
                        let meeting_id = mid.clone();
                        let update_setting = update_setting.clone();
                        move |new_val: bool| {
                            if saving() {
                                return;
                            }
                            let old_val = !new_val;
                            end_on_host_leave_toggle.set(new_val);
                            update_setting(
                                meeting_id.clone(),
                                None,
                                None,
                                Some(new_val),
                                None,
                                end_on_host_leave_toggle,
                                old_val,
                                None,
                                saving,
                                toggle_error,
                            );
                        }
                    },
                }
            }
        }

        // ── Allow guests ──────────────────────────────────────────────────
        div { class: "settings-option-row", style: "opacity: 1;",
            span { class: "settings-option-label", "Allow guests" }
            div { class: "settings-option-controls",
                {info_icon("Allow guests to join the meeting without an account")}
                ToggleSwitch {
                    enabled: allow_guests_toggle(),
                    disabled: saving(),
                    on_toggle: {
                        let meeting_id = mid.clone();
                        let update_setting = update_setting.clone();
                        move |new_val: bool| {
                            if saving() {
                                return;
                            }
                            let old_val = !new_val;
                            allow_guests_toggle.set(new_val);
                            update_setting(
                                meeting_id.clone(),
                                None,
                                None,
                                None,
                                Some(new_val),
                                allow_guests_toggle,
                                old_val,
                                None,
                                saving,
                                toggle_error,
                            );
                        }
                    },
                }
            }
        }

        if let Some(err) = toggle_error() {
            p { class: "toggle-error", "{err}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Disabling the waiting room clears admitted-can-admit, so a failed PATCH
    /// must restore both to ON. Fails if the second element is `None`.
    #[test]
    fn disabling_waiting_room_with_aca_on_rolls_back_both() {
        // Host flips Waiting Room OFF while admitted-can-admit was ON.
        let (wr_restore, aca_restore) = waiting_room_rollback(false, true);
        assert!(wr_restore, "waiting room must roll back to ON");
        assert_eq!(
            aca_restore,
            Some(true),
            "admitted-can-admit must roll back to its prior ON value, not stay cleared",
        );
    }

    /// Disabling with admitted-can-admit already OFF restores it to OFF, never
    /// spuriously turns it on.
    #[test]
    fn disabling_waiting_room_with_aca_off_restores_off() {
        let (wr_restore, aca_restore) = waiting_room_rollback(false, false);
        assert!(wr_restore);
        assert_eq!(aca_restore, Some(false));
    }

    /// Enabling never touches admitted-can-admit, so there is no secondary
    /// rollback target.
    #[test]
    fn enabling_waiting_room_has_no_secondary_rollback() {
        let (wr_restore, aca_restore) = waiting_room_rollback(true, true);
        assert!(!wr_restore, "waiting room must roll back to OFF");
        assert_eq!(
            aca_restore, None,
            "enabling must not schedule an admitted-can-admit rollback",
        );
    }
}

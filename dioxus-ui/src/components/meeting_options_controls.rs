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
//! This is the SINGLE source of truth for the six mutable meeting options
//! (Waiting Room, Admitted-can-admit, End-on-host-leave, Allow-guests, Recording, Chat). It is
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

use crate::components::co_hosts::{can_edit_meeting_options, CoHostsSection, MeetingOwnership};
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

/// The six owner-editable meeting-option rows, wired to caller-owned signals.
#[component]
pub fn MeetingOptionsControls(
    meeting_id: String,
    waiting_room_toggle: Signal<bool>,
    admitted_can_admit_toggle: Signal<bool>,
    end_on_host_leave_toggle: Signal<bool>,
    allow_guests_toggle: Signal<bool>,
    recording_allowed_for_all_toggle: Signal<bool>,
    chat_allowed_for_all_toggle: Signal<bool>,
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
                  recording_allowed_for_all_opt: Option<bool>,
                  chat_allowed_for_all_opt: Option<bool>,
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
                        recording_allowed_for_all_opt,
                        chat_allowed_for_all_opt,
                    )
                    .await
                    {
                        Ok(updated) => {
                            waiting_room_toggle.set(updated.waiting_room_enabled);
                            admitted_can_admit_toggle.set(updated.admitted_can_admit);
                            end_on_host_leave_toggle.set(updated.end_on_host_leave);
                            allow_guests_toggle.set(updated.allow_guests);
                            recording_allowed_for_all_toggle.set(updated.recording_allowed_for_all);
                            chat_allowed_for_all_toggle.set(updated.chat_allowed_for_all);
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
                                None,
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
                                None,
                                None,
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

        // ── Allow recording for all participants ──────────────────────────
        // When enabled every admitted participant sees the record button on
        // their action bar.  When disabled (the default) only the host sees
        // the record button; recording itself is entirely client-side and is
        // not enforced server-side, so this toggle controls visibility, not
        // capability.
        div { class: "settings-option-row", style: "opacity: 1;",
            span { class: "settings-option-label", "Allow recording for all" }
            div { class: "settings-option-controls",
                {info_icon("When on, every admitted participant sees the record button. When off, only the host sees the record button.")}
                ToggleSwitch {
                    enabled: recording_allowed_for_all_toggle(),
                    disabled: saving(),
                    on_toggle: {
                        let meeting_id = mid.clone();
                        let update_setting = update_setting.clone();
                        move |new_val: bool| {
                            if saving() {
                                return;
                            }
                            let old_val = !new_val;
                            recording_allowed_for_all_toggle.set(new_val);
                            update_setting(
                                meeting_id.clone(),
                                None,
                                None,
                                None,
                                None,
                                Some(new_val),
                                None,
                                recording_allowed_for_all_toggle,
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

        // ── Allow chat for all participants ───────────────────────────────
        // When enabled (the default) every admitted participant can SEND chat
        // messages.  When disabled, only the host/co-hosts can send; everyone
        // can still READ the chat.  A host turns this off for an all-hands-style
        // meeting and can flip it back on live (e.g. to open the floor for
        // end-of-meeting questions).  This gates the send affordance in the UI;
        // like recording it is a visibility gate, not server-enforced.
        div { class: "settings-option-row", style: "opacity: 1;",
            span { class: "settings-option-label", "Allow chat for all" }
            div { class: "settings-option-controls",
                {info_icon("When on, everyone can send chat messages. When off, only hosts can send — everyone can still read the chat.")}
                ToggleSwitch {
                    enabled: chat_allowed_for_all_toggle(),
                    disabled: saving(),
                    on_toggle: {
                        let meeting_id = mid.clone();
                        let update_setting = update_setting.clone();
                        move |new_val: bool| {
                            if saving() {
                                return;
                            }
                            let old_val = !new_val;
                            chat_allowed_for_all_toggle.set(new_val);
                            update_setting(
                                meeting_id.clone(),
                                None,
                                None,
                                None,
                                None,
                                None,
                                Some(new_val),
                                chat_allowed_for_all_toggle,
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

const MEETING_OPTIONS_OPENER: &str = "[data-testid='open-meeting-options']";
const DIALOG_ID: &str = "meeting-options-dialog";
const FOCUSABLE: &str = "button:not([disabled]), [href], input:not([disabled]), \
     select:not([disabled]), textarea:not([disabled]), summary, \
     [tabindex]:not([tabindex='-1'])";

/// Keeps Tab inside the modal: wraps from the last control to the first and,
/// with Shift, from the first (or the title) to the last. `true` when it moved
/// focus, so the caller cancels the browser's own move.
fn wrap_tab_in(container_id: &str, backwards: bool) -> bool {
    use wasm_bindgen::JsCast;
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return false;
    };
    let Some(container) = doc.get_element_by_id(container_id) else {
        return false;
    };
    let Ok(nodes) = container.query_selector_all(FOCUSABLE) else {
        return false;
    };
    let items: Vec<web_sys::HtmlElement> = (0..nodes.length())
        .filter_map(|i| nodes.item(i))
        .filter_map(|node| node.dyn_into::<web_sys::HtmlElement>().ok())
        .collect();
    let (Some(first), Some(last)) = (items.first(), items.last()) else {
        return false;
    };
    let active = doc.active_element();
    let is_active = |el: &web_sys::HtmlElement| {
        active
            .as_ref()
            .is_some_and(|a| a.is_same_node(Some(el.as_ref())))
    };
    let target = if backwards {
        (is_active(first) || !items.iter().any(is_active)).then_some(last)
    } else {
        let inside = active
            .as_ref()
            .is_some_and(|a| container.contains(Some(a.as_ref())));
        (is_active(last) || !inside).then_some(first)
    };
    match target {
        Some(el) => el.focus().is_ok(),
        None => false,
    }
}

fn focused_element() -> Option<web_sys::HtmlElement> {
    use wasm_bindgen::JsCast;
    let doc = web_sys::window()?.document()?;
    let active = doc.active_element()?;
    if doc
        .body()
        .is_some_and(|body| active.is_same_node(Some(&body)))
    {
        return None;
    }
    active.dyn_into::<web_sys::HtmlElement>().ok()
}

/// Back to the element that opened the dialog, else the action-bar opener.
fn restore_focus(opener: Option<web_sys::HtmlElement>) {
    use wasm_bindgen::JsCast;
    let target = opener.filter(|el| el.is_connected()).or_else(|| {
        web_sys::window()?
            .document()?
            .query_selector(MEETING_OPTIONS_OPENER)
            .ok()??
            .dyn_into::<web_sys::HtmlElement>()
            .ok()
    });
    if let Some(el) = target {
        let _ = el.focus();
    }
}

/// The in-call Meeting Options dialog. Renders nothing unless `open` and the
/// local user may edit meeting options — the OWNER, or anyone CURRENTLY
/// holding the host role (issue 2702 round 10: a co-host may start/restart
/// the meeting and change its OPTIONS). Co-host management stays
/// owner-only — see `MeetingOptionsDialog`'s own `ownership.is_owner()` gate
/// around `CoHostsSection`, never this function's `is_host` fallback.
#[component]
pub fn MeetingOptionsPanel(
    ownership: MeetingOwnership,
    /// Whether this user CURRENTLY holds the host role (e.g. the live host
    /// set, falling back to a join-time snapshot). See
    /// [`can_edit_meeting_options`].
    #[props(default)]
    is_host: bool,
    open: Signal<bool>,
    meeting_id: String,
    #[props(default)] owner_user_id: Option<String>,
    #[props(default = true)] meeting_active: bool,
    waiting_room_toggle: Signal<bool>,
    admitted_can_admit_toggle: Signal<bool>,
    end_on_host_leave_toggle: Signal<bool>,
    allow_guests_toggle: Signal<bool>,
    recording_allowed_for_all_toggle: Signal<bool>,
    chat_allowed_for_all_toggle: Signal<bool>,
    saving: Signal<bool>,
    toggle_error: Signal<Option<String>>,
    #[props(default)] co_host_refresh: Option<Signal<u64>>,
) -> Element {
    if !(open() && can_edit_meeting_options(ownership, is_host)) {
        return rsx! {};
    }
    rsx! {
        MeetingOptionsDialog {
            ownership,
            open,
            meeting_id,
            owner_user_id,
            meeting_active,
            waiting_room_toggle,
            admitted_can_admit_toggle,
            end_on_host_leave_toggle,
            allow_guests_toggle,
            recording_allowed_for_all_toggle,
            chat_allowed_for_all_toggle,
            saving,
            toggle_error,
            co_host_refresh,
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[component]
fn MeetingOptionsDialog(
    ownership: MeetingOwnership,
    open: Signal<bool>,
    meeting_id: String,
    owner_user_id: Option<String>,
    meeting_active: bool,
    waiting_room_toggle: Signal<bool>,
    admitted_can_admit_toggle: Signal<bool>,
    end_on_host_leave_toggle: Signal<bool>,
    allow_guests_toggle: Signal<bool>,
    recording_allowed_for_all_toggle: Signal<bool>,
    chat_allowed_for_all_toggle: Signal<bool>,
    saving: Signal<bool>,
    toggle_error: Signal<Option<String>>,
    co_host_refresh: Option<Signal<u64>>,
) -> Element {
    let mut open = open;
    let opener = use_hook(|| Rc::new(std::cell::RefCell::new(focused_element())));
    use_drop(move || restore_focus(opener.borrow_mut().take()));
    rsx! {
        div {
            class: "glass-backdrop",
            onclick: move |_| open.set(false),
            onkeydown: move |e: Event<KeyboardData>| {
                if e.key() == Key::Escape {
                    open.set(false);
                } else if e.key() == Key::Tab && wrap_tab_in(DIALOG_ID, e.modifiers().shift()) {
                    e.prevent_default();
                }
            },
            div {
                id: DIALOG_ID,
                class: "card-apple",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "meeting-options-title",
                "data-testid": "meeting-options-panel",
                style: "width: 380px; max-width: 92vw; max-height: 90vh; overflow-y: auto;",
                onclick: move |e| e.stop_propagation(),

                div {
                    style: "display:flex; align-items:center; justify-content:space-between; margin-bottom:var(--space-2);",
                    h3 {
                        id: "meeting-options-title",
                        style: "margin:0;",
                        tabindex: "-1",
                        onmounted: move |e| {
                            spawn(async move {
                                let _ = e.data().set_focus(true).await;
                            });
                        },
                        "Meeting Options"
                    }
                    button {
                        r#type: "button",
                        class: "btn-apple btn-secondary btn-sm",
                        "aria-label": "Close meeting options",
                        onclick: move |_| open.set(false),
                        "Done"
                    }
                }
                p {
                    style: "color: var(--text-secondary); margin-top:0; margin-bottom:var(--space-3); font-size:0.85rem;",
                    "Changes apply to everyone immediately."
                }

                MeetingOptionsControls {
                    meeting_id: meeting_id.clone(),
                    waiting_room_toggle,
                    admitted_can_admit_toggle,
                    end_on_host_leave_toggle,
                    allow_guests_toggle,
                    recording_allowed_for_all_toggle,
                    chat_allowed_for_all_toggle,
                    saving,
                    toggle_error,
                }
                // `MeetingOptionsPanel`'s outer gate admits the owner OR any
                // current host (see `can_edit_meeting_options`), so co-host
                // management — owner-only — needs its own explicit check
                // here rather than relying on the outer gate. Mounting
                // `CoHostsSection` only for the owner also keeps its
                // `list_co_hosts` GET from firing for a non-owner host.
                if ownership.is_owner() {
                    CoHostsSection {
                        meeting_id,
                        owner_user_id,
                        meeting_active,
                        refresh: co_host_refresh,
                    }
                }
            }
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

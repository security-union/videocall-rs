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
//! This is the SINGLE source of truth for the mutable meeting options
//! (Waiting Room, Admitted-can-admit, End-on-host-leave, Allow-guests, Recording, Chat,
//! and the join/leave announcement overrides). It is
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

use crate::components::announcement_overrides::{
    option_label, override_save_error, AnnouncementOverride, AnnouncementOverrides, OverrideEditor,
    OverrideField, OverridePatch, OverrideSaver,
};
use crate::components::attendants::{
    settle_slot_focus, try_set, try_set_if_changed, OVERFLOW_MENU_TRIGGER_ID,
};
use crate::components::co_hosts::{can_edit_meeting_options, CoHostsSection, MeetingOwnership};
use crate::components::toggle_switch::ToggleSwitch;
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use std::cell::Cell;
use std::rc::Rc;

thread_local! {
    static OVERRIDE_GROUP_IDS: Cell<u64> = const { Cell::new(0) };
}

const OVERRIDE_PICK_SETTLE_MS: u32 = if cfg!(test) { 150 } else { 400 };

const OVERRIDE_EVENTS: [(&str, [OverrideField; 2]); 2] = [
    (
        "Participant joins",
        [OverrideField::EntryMessage, OverrideField::EntrySound],
    ),
    (
        "Participant leaves",
        [OverrideField::ExitMessage, OverrideField::ExitSound],
    ),
];

/// Saves `patch`, then whatever the host chose and settled on while it was in
/// flight, one PATCH at a time, also after the editor's owner unmounts.
fn save_overrides(meeting_id: String, mut patch: OverridePatch, editor: OverrideEditor) {
    let mut saver = editor.saver;
    let handoff = editor.handoff();
    wasm_bindgen_futures::spawn_local(async move {
        loop {
            let read = editor.start_read();
            let result =
                crate::meeting_api::update_announcement_overrides(&meeting_id, patch).await;
            if let Err(e) = &result {
                log::error!("Failed to update announcement overrides: {e}");
            }
            let next = if let Ok(mut chain) = saver.try_write() {
                match result {
                    Ok(info) => {
                        editor.land_read(read, AnnouncementOverrides::from_meeting_info(&info));
                        chain.succeeded()
                    }
                    Err(_) => {
                        let reverted = chain.failed();
                        let now = editor.server.try_peek().map(|o| *o).unwrap_or_default();
                        try_set(editor.error, Some(override_save_error(&reverted, &now)));
                        None
                    }
                }
            } else if let (Some(mut left), Ok(_)) = (handoff.take(), &result) {
                left.succeeded()
            } else {
                None
            };
            match next {
                Some(more) => patch = more,
                None => break,
            }
        }
        if saver.try_peek().is_err() {
            editor.count_orphaned_save(&meeting_id);
        }
    });
}

/// How many overrides the table shows as set for everyone.
fn overrides_set(saver: &OverrideSaver, server: &AnnouncementOverrides) -> usize {
    OverrideField::ALL
        .into_iter()
        .filter(|&field| saver.shown(field, server) != AnnouncementOverride::Participant)
        .count()
}

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

/// The owner-editable meeting-option rows, wired to caller-owned signals.
#[component]
pub fn MeetingOptionsControls(
    meeting_id: String,
    waiting_room_toggle: Signal<bool>,
    admitted_can_admit_toggle: Signal<bool>,
    end_on_host_leave_toggle: Signal<bool>,
    allow_guests_toggle: Signal<bool>,
    recording_allowed_for_all_toggle: Signal<bool>,
    chat_allowed_for_all_toggle: Signal<bool>,
    announcement_overrides: OverrideEditor,
    saving: Signal<bool>,
    toggle_error: Signal<Option<String>>,
    #[props(default)] after_allow_guests: Option<Element>,
    #[props(default)] collapsible: bool,
) -> Element {
    let editor = announcement_overrides;
    use_drop({
        let meeting_id = meeting_id.clone();
        move || {
            try_set_if_changed(editor.error, None);
            if let Some(Some(patch)) = editor.edit_saver(OverrideSaver::flush) {
                save_overrides(meeting_id, patch, editor);
            }
        }
    });
    let open_at_mount = use_hook(|| overrides_set(&editor.saver.peek(), &editor.server.peek()) > 0);
    let group = use_hook(|| {
        let id = OVERRIDE_GROUP_IDS.with(|ids| {
            let next = ids.get().wrapping_add(1);
            ids.set(next);
            next
        });
        format!("announce-overrides-{id}")
    });
    let on_override_change = {
        let meeting_id = meeting_id.clone();
        move |field: OverrideField, value: AnnouncementOverride| {
            let Ok(server) = editor.server.try_peek().map(|o| *o) else {
                return;
            };
            let Some(Some(pick)) = editor.edit_saver(|s| s.choose(field, value, &server)) else {
                return;
            };
            try_set_if_changed(editor.error, None);
            let meeting_id = meeting_id.clone();
            spawn(async move {
                TimeoutFuture::new(OVERRIDE_PICK_SETTLE_MS).await;
                if let Some(Some(patch)) = editor.edit_saver(|s| s.settled(pick)) {
                    save_overrides(meeting_id, patch, editor);
                }
            });
        }
    };
    let saver_now = (editor.saver)();
    let server_overrides = (editor.server)();
    let set_count = overrides_set(&saver_now, &server_overrides);
    let group_body = rsx! {
        p { id: "{group}-hint", class: "meeting-options-group__hint",
            "Their choice keeps each participant's own setting. Anyone recording still sees join and leave messages."
        }
        table {
            class: "override-table",
            "data-testid": "meeting-options-announcements-table",
            colgroup {
                col {}
                col { class: "override-table__col--choice" }
                col { class: "override-table__col--value" }
                col { class: "override-table__col--value" }
            }
            thead {
                tr {
                    td { rowspan: 2 }
                    th { scope: "col", rowspan: 2, "Their choice" }
                    th {
                        class: "override-table__colgroup",
                        scope: "colgroup",
                        colspan: 2,
                        span { "For everyone" }
                    }
                }
                tr {
                    th { scope: "col", "On" }
                    th { scope: "col", "Off" }
                }
            }
            for (event, fields) in OVERRIDE_EVENTS {
                tbody {
                    tr { class: "override-table__event",
                        th { scope: "rowgroup", colspan: 4, "{event}" }
                    }
                    for field in fields {
                        tr {
                            class: "override-table__row",
                            "data-testid": field.testid(),
                            th { scope: "row", "{field.channel()}" }
                            for value in AnnouncementOverride::ALL {
                                td {
                                    label {
                                        class: if value == AnnouncementOverride::Participant { "override-radio override-radio--choice" } else { "override-radio" },
                                        input {
                                            r#type: "radio",
                                            name: "{group}-{field.testid()}",
                                            value: value.as_str(),
                                            checked: saver_now.shown(field, &server_overrides) == value,
                                            "aria-label": "{field.label()}: {option_label(value)}",
                                            "data-testid": "{field.testid()}-{value.as_str()}",
                                            onchange: {
                                                let on_change = on_override_change.clone();
                                                move |_| on_change(field, value)
                                            },
                                        }
                                        span {
                                            class: "override-radio__mark",
                                            "aria-hidden": "true",
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

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
        {after_allow_guests}

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

        div {
            class: if collapsible { "meeting-options-group meeting-options-group--collapsible" } else { "meeting-options-group" },
            role: "group",
            "aria-labelledby": "{group}-title",
            "aria-describedby": "{group}-hint",
            if collapsible {
                details { class: "meeting-options-details", open: open_at_mount,
                    summary {
                        class: "meeting-options-summary",
                        "data-testid": "meeting-options-announcements-summary",
                        span { id: "{group}-title", class: "meeting-options-group__title",
                            "Join & leave notifications"
                        }
                        if set_count > 0 {
                            span {
                                class: "meeting-options-count",
                                "data-testid": "meeting-options-announcements-count",
                                "{set_count} set"
                            }
                        }
                        svg {
                            class: "meeting-options-chevron",
                            "aria-hidden": "true",
                            xmlns: "http://www.w3.org/2000/svg",
                            width: "16",
                            height: "16",
                            view_box: "0 0 24 24",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            polyline { points: "6 9 12 15 18 9" }
                        }
                    }
                    div { class: "meeting-options-body", {group_body} }
                }
            } else {
                h4 { id: "{group}-title", class: "meeting-options-group__title",
                    "Join & leave notifications"
                }
                {group_body}
            }
            p { class: "meeting-options-group__error", role: "alert",
                if let Some(err) = (editor.error)() {
                    "{err}"
                }
            }
        }
    }
}

const MEETING_OPTIONS_OPENER: &str = "[data-testid='open-meeting-options']";
const DIALOG_ID: &str = "meeting-options-dialog";
const FOCUSABLE: &str = "button:not([disabled]), [href], \
     input:not([disabled]):not([type='radio']), input[type='radio']:checked, \
     select:not([disabled]), textarea:not([disabled]), summary, \
     [tabindex]:not([tabindex='-1'])";

fn radio_group(el: &web_sys::Element) -> Option<String> {
    (el.get_attribute("type").as_deref() == Some("radio"))
        .then(|| el.get_attribute("name"))
        .flatten()
}

/// Keeps Tab inside the modal: wraps from the last control to the first and,
/// with Shift, from the first (or the title) to the last. A radio group counts
/// as its checked radio. `true` when it moved focus, so the caller cancels the
/// browser's own move.
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
        active.as_ref().is_some_and(|a| {
            a.is_same_node(Some(el.as_ref()))
                || radio_group(a).is_some_and(|group| radio_group(el) == Some(group))
        })
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
    settle_slot_focus(OVERFLOW_MENU_TRIGGER_ID);
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
    announcement_overrides: OverrideEditor,
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
            announcement_overrides,
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
    announcement_overrides: OverrideEditor,
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
                    announcement_overrides,
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

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use crate::components::announcement_overrides::use_override_editor;
    use crate::components::announcement_overrides::AnnouncementOverride::{Off, On, Participant};
    use std::cell::RefCell;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::wasm_bindgen_test;

    const MEETING: &str = "m-3168";
    const STYLE_CSS: &str = include_str!("../../static/style.css");
    const GLOBAL_CSS: &str = include_str!("../../static/global.css");
    const SEEDED: AnnouncementOverrides = AnnouncementOverrides {
        entry_message: Off,
        entry_sound: Participant,
        exit_message: Participant,
        exit_sound: On,
    };

    thread_local! {
        static EDITOR: RefCell<Option<OverrideEditor>> = const { RefCell::new(None) };
        static SAVER_RENDERS: Cell<u32> = const { Cell::new(0) };
    }

    #[component]
    fn SaverReader(saver: Signal<OverrideSaver>) -> Element {
        let _ = saver();
        SAVER_RENDERS.with(|n| n.set(n.get() + 1));
        rsx! {}
    }

    #[component]
    fn Harness(collapsible: bool, slot: bool, seed: AnnouncementOverrides) -> Element {
        let mut owner_shown = use_signal(|| true);
        let mut controls_shown = use_signal(|| true);
        rsx! {
            button { id: "toggle-owner", onclick: move |_| owner_shown.toggle(), "owner" }
            button { id: "toggle-controls", onclick: move |_| controls_shown.toggle(), "controls" }
            if owner_shown() {
                OverridesOwner { controls_shown, collapsible, slot, seed }
            }
        }
    }

    #[component]
    fn OverridesOwner(
        controls_shown: Signal<bool>,
        collapsible: bool,
        slot: bool,
        seed: AnnouncementOverrides,
    ) -> Element {
        let editor = use_override_editor(use_signal(|| seed));
        use_hook(move || EDITOR.with(|e| *e.borrow_mut() = Some(editor)));
        let toggle = use_signal(|| false);
        let error = use_signal(|| None::<String>);
        rsx! {
            SaverReader { saver: editor.saver }
            if controls_shown() {
                MeetingOptionsControls {
                    meeting_id: MEETING.to_string(),
                    waiting_room_toggle: toggle,
                    admitted_can_admit_toggle: toggle,
                    end_on_host_leave_toggle: toggle,
                    allow_guests_toggle: toggle,
                    recording_allowed_for_all_toggle: toggle,
                    chat_allowed_for_all_toggle: toggle,
                    announcement_overrides: editor,
                    saving: toggle,
                    toggle_error: error,
                    after_allow_guests: slot.then(|| rsx! { div { id: "guest-slot" } }),
                    collapsible,
                }
            }
        }
    }

    #[component]
    fn DialogHarness() -> Element {
        let editor = use_override_editor(use_signal(|| SEEDED));
        let open = use_signal(|| true);
        let toggle = use_signal(|| false);
        let error = use_signal(|| None::<String>);
        rsx! {
            MeetingOptionsPanel {
                ownership: MeetingOwnership::of(None, None),
                is_host: true,
                open,
                meeting_id: MEETING.to_string(),
                waiting_room_toggle: toggle,
                admitted_can_admit_toggle: toggle,
                end_on_host_leave_toggle: toggle,
                allow_guests_toggle: toggle,
                recording_allowed_for_all_toggle: toggle,
                chat_allowed_for_all_toggle: toggle,
                announcement_overrides: editor,
                saving: toggle,
                toggle_error: error,
            }
        }
    }

    #[allow(non_snake_case)]
    fn HiddenOpenerHarness() -> Element {
        let editor = use_override_editor(use_signal(|| SEEDED));
        let mut open = use_signal(|| true);
        let toggle = use_signal(|| false);
        let error = use_signal(|| None::<String>);
        rsx! {
            button { "data-testid": "open-meeting-options", style: "display:none" }
            button { id: OVERFLOW_MENU_TRIGGER_ID }
            button { id: "close-dialog", onclick: move |_| open.set(false) }
            MeetingOptionsPanel {
                ownership: MeetingOwnership::of(None, None),
                is_host: true,
                open,
                meeting_id: MEETING.to_string(),
                waiting_room_toggle: toggle,
                admitted_can_admit_toggle: toggle,
                end_on_host_leave_toggle: toggle,
                allow_guests_toggle: toggle,
                recording_allowed_for_all_toggle: toggle,
                chat_allowed_for_all_toggle: toggle,
                announcement_overrides: editor,
                saving: toggle,
                toggle_error: error,
            }
        }
    }

    #[wasm_bindgen_test]
    async fn closing_with_the_opener_hidden_returns_focus_to_the_overflow_trigger() {
        install_mocks();
        let root = new_root();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(HiddenOpenerHarness),
            dioxus::web::Config::new().rootelement(root.clone()),
        );
        wait_until("the dialog title to take focus", || {
            gloo_utils::document()
                .active_element()
                .is_some_and(|el| el.id() == "meeting-options-title")
        })
        .await;
        root.query_selector("#close-dialog")
            .unwrap()
            .unwrap()
            .unchecked_into::<web_sys::HtmlElement>()
            .click();
        wait_until("focus on the overflow trigger", || {
            gloo_utils::document()
                .active_element()
                .is_some_and(|el| el.id() == OVERFLOW_MENU_TRIGGER_ID)
        })
        .await;
        root.remove();
        remove_mocks();
    }

    fn install_mocks() {
        js_sys::eval(
            r#"
            window.__APP_CONFIG = Object.freeze({
                apiBaseUrl: 'http://test:8080', wsUrl: 'ws://test:8080',
                webTransportHost: 'https://test:4433', oauthEnabled: 'false',
                e2eeEnabled: 'false', webTransportEnabled: 'false', firefoxEnabled: 'false',
                usersAllowedToStream: '', serverElectionPeriodMs: 2000, vadThreshold: 0.02
            });
            window.__patches = [];
            window.__sent = 0;
            window.__patchMode = 'ok';
            window.__held = [];
            window.__errors = [];
            if (!window.__errorsHooked) {
                window.__errorsHooked = true;
                window.addEventListener('error', function (e) { window.__errors.push(String(e.message)); });
                window.addEventListener('unhandledrejection', function (e) { window.__errors.push(String(e.reason)); });
            }
            window.__server = {
                entry_message_override: 'off', entry_sound_override: 'participant',
                exit_message_override: 'participant', exit_sound_override: 'on'
            };
            window.__original_fetch = window.__original_fetch || window.fetch;
            window.fetch = function (input, init) {
                var url = typeof input === 'string' ? input : input.url;
                var method = typeof input === 'string' ? ((init && init.method) || 'GET') : input.method;
                if (method !== 'PATCH' || !url.endsWith('/api/v1/meetings/m-3168')) {
                    return window.__original_fetch(input, init);
                }
                window.__sent += 1;
                return input.text().then(function (text) {
                    window.__patches.push(text);
                    var reply = function (status, body) {
                        var resp = new Response(JSON.stringify(body),
                            { status: status, headers: { 'Content-Type': 'application/json' } });
                        Object.defineProperty(resp, 'url', { value: url });
                        return resp;
                    };
                    var respond = function () {
                        if (window.__patchMode === 'fail') {
                            return reply(500, { success: false, result: null });
                        }
                        var body = JSON.parse(text);
                        Object.keys(body).forEach(function (k) { window.__server[k] = body[k]; });
                        var info = Object.assign({ meeting_id: 'm-3168', state: 'active', host: 'h',
                            has_password: false, waiting_room_enabled: false, admitted_can_admit: false,
                            end_on_host_leave: false, participant_count: 1, waiting_count: 0,
                            started_at: 0 }, window.__server);
                        return reply(200, { success: true, result: info });
                    };
                    if (window.__patchMode === 'hold') {
                        return new Promise(function (resolve) {
                            window.__held.push(function () { resolve(respond()); });
                        });
                    }
                    return respond();
                });
            };
            window.__release = function (mode) {
                window.__patchMode = mode;
                var next = window.__held.shift();
                if (next) { next(); }
            };
            "#,
        )
        .unwrap();
        crate::constants::reset_config_cache_for_test();
    }

    fn remove_mocks() {
        js_sys::eval(
            r#"
            if (window.__original_fetch) { window.fetch = window.__original_fetch; delete window.__original_fetch; }
            delete window.__APP_CONFIG;
            "#,
        )
        .unwrap();
        crate::constants::reset_config_cache_for_test();
    }

    fn patches() -> Vec<String> {
        let value = js_sys::Reflect::get(&gloo_utils::window(), &"__patches".into()).unwrap();
        js_sys::Array::from(&value)
            .iter()
            .filter_map(|v| v.as_string())
            .collect()
    }

    fn release(mode: &str) {
        js_sys::eval(&format!("window.__release('{mode}')")).unwrap();
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

    fn patch_calls() -> u32 {
        js_sys::Reflect::get(&gloo_utils::window(), &"__sent".into())
            .unwrap()
            .as_f64()
            .unwrap() as u32
    }

    async fn before_any_timer(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..100 {
            if done() {
                return;
            }
            let tick = js_sys::Promise::resolve(&wasm_bindgen::JsValue::UNDEFINED);
            wasm_bindgen_futures::JsFuture::from(tick).await.unwrap();
        }
        panic!("{what} did not happen before a timer could run");
    }

    fn new_root() -> web_sys::Element {
        let root = gloo_utils::document().create_element("div").unwrap();
        gloo_utils::document()
            .body()
            .unwrap()
            .append_child(&root)
            .unwrap();
        root
    }

    async fn mount_with(
        collapsible: bool,
        slot: bool,
        seed: AnnouncementOverrides,
    ) -> web_sys::Element {
        let root = new_root();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new_with_props(
                Harness,
                HarnessProps {
                    collapsible,
                    slot,
                    seed,
                },
            ),
            dioxus::web::Config::new().rootelement(root.clone()),
        );
        wait_until("the controls", || has_table(&root)).await;
        root
    }

    async fn mount_dialog() -> web_sys::Element {
        let root = new_root();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(DialogHarness),
            dioxus::web::Config::new().rootelement(root.clone()),
        );
        wait_until("the dialog title to take focus", || {
            gloo_utils::document()
                .active_element()
                .is_some_and(|el| el.id() == "meeting-options-title")
        })
        .await;
        root
    }

    async fn mount() -> web_sys::Element {
        mount_with(false, false, SEEDED).await
    }

    fn editor() -> OverrideEditor {
        EDITOR.with(|e| e.borrow().expect("the owner's editor"))
    }

    fn set_server_overrides(value: AnnouncementOverrides) {
        try_set(editor().server, value);
    }

    fn click(el: &web_sys::Element) {
        el.unchecked_ref::<web_sys::HtmlElement>().click();
    }

    fn is_rendered(el: &web_sys::Element) -> bool {
        js_sys::Reflect::get(el, &"checkVisibility".into())
            .unwrap()
            .unchecked_into::<js_sys::Function>()
            .call0(el)
            .unwrap()
            .is_truthy()
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

    fn computed(el: &web_sys::Element, prop: &str) -> String {
        gloo_utils::window()
            .get_computed_style(el)
            .unwrap()
            .unwrap()
            .get_property_value(prop)
            .unwrap()
    }

    fn error_line(root: &web_sys::Element) -> web_sys::Element {
        element(
            root,
            "[role='group'] > p.meeting-options-group__error[role='alert']",
        )
        .expect("the group's always-mounted error line")
    }

    fn has_table(root: &web_sys::Element) -> bool {
        root.query_selector("table.override-table")
            .ok()
            .flatten()
            .is_some()
    }

    async fn toggle(root: &web_sys::Element, button: &str, shown: bool) {
        root.query_selector(&format!("#{button}"))
            .unwrap()
            .unwrap()
            .unchecked_into::<web_sys::HtmlElement>()
            .click();
        wait_until(button, || has_table(root) == shown).await;
    }

    fn element(root: &web_sys::Element, selector: &str) -> Option<web_sys::Element> {
        root.query_selector(selector).unwrap()
    }

    fn all(root: &web_sys::Element, selector: &str) -> Vec<web_sys::Element> {
        let nodes = root.query_selector_all(selector).unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i))
            .map(|node| node.unchecked_into())
            .collect()
    }

    fn text(el: &web_sys::Element) -> String {
        el.text_content().unwrap_or_default()
    }

    fn row(root: &web_sys::Element, field: OverrideField) -> web_sys::Element {
        element(
            root,
            &format!("tr.override-table__row[data-testid='{}']", field.testid()),
        )
        .unwrap_or_else(|| panic!("no row for {field:?}"))
    }

    fn radio(
        root: &web_sys::Element,
        field: OverrideField,
        value: &str,
    ) -> web_sys::HtmlInputElement {
        element(
            root,
            &format!(
                "input[type='radio'][data-testid='{}-{value}']",
                field.testid()
            ),
        )
        .unwrap_or_else(|| panic!("no {value} radio for {field:?}"))
        .unchecked_into()
    }

    fn shown(root: &web_sys::Element, field: OverrideField) -> String {
        let checked: Vec<String> = all(&row(root, field), "input[type='radio']")
            .into_iter()
            .map(|el| el.unchecked_into::<web_sys::HtmlInputElement>())
            .filter(|radio| radio.checked())
            .map(|radio| radio.value())
            .collect();
        assert_eq!(checked.len(), 1, "{field:?} checks one radio: {checked:?}");
        checked[0].clone()
    }

    fn choose(root: &web_sys::Element, field: OverrideField, value: &str) {
        radio(root, field, value).click();
    }

    fn is_focused(el: &web_sys::Element) -> bool {
        gloo_utils::document()
            .active_element()
            .is_some_and(|active| active.is_same_node(Some(el)))
    }

    fn tab(target: &web_sys::Element, shift: bool) -> bool {
        js_sys::Function::new_with_args(
            "el, shift",
            "const e = new KeyboardEvent('keydown', \
             { key: 'Tab', shiftKey: shift, bubbles: true, cancelable: true }); \
             el.dispatchEvent(e); return e.defaultPrevented;",
        )
        .call2(&wasm_bindgen::JsValue::NULL, target, &shift.into())
        .unwrap()
        .as_bool()
        .unwrap()
    }

    fn error_text(root: &web_sys::Element) -> String {
        error_line(root).text_content().unwrap_or_default()
    }

    fn server_entry_message() -> AnnouncementOverride {
        editor().server.peek().entry_message
    }

    fn count(root: &web_sys::Element) -> Option<String> {
        element(root, "[data-testid='meeting-options-announcements-count']")
            .map(|badge| badge.text_content().unwrap_or_default())
    }

    #[wasm_bindgen_test]
    async fn a_refresh_sent_before_a_save_cannot_roll_its_response_back() {
        install_mocks();
        let root = mount().await;
        let refresh = editor().start_read();

        choose(&root, OverrideField::EntryMessage, "on");
        wait_until("the save to land", || server_entry_message() == On).await;
        editor().land_read(refresh, SEEDED);
        TimeoutFuture::new(30).await;
        assert_eq!(server_entry_message(), On);
        assert_eq!(shown(&root, OverrideField::EntryMessage), "on");
        root.remove();
        remove_mocks();
    }

    #[wasm_bindgen_test]
    async fn a_save_response_cannot_overwrite_a_refresh_sent_after_it() {
        install_mocks();
        let root = mount().await;
        js_sys::eval("window.__patchMode = 'hold'").unwrap();

        choose(&root, OverrideField::EntryMessage, "on");
        wait_until("the PATCH", || patches().len() == 1).await;
        let refresh = editor().start_read();
        let newer = AnnouncementOverrides {
            entry_message: On,
            exit_sound: Off,
            ..SEEDED
        };
        editor().land_read(refresh, newer);
        release("ok");
        wait_until("the save to finish", || {
            *editor().saver.peek() == OverrideSaver::default()
        })
        .await;
        assert_eq!(
            *editor().server.peek(),
            newer,
            "the response is older than the refresh"
        );
        assert_eq!(shown(&root, OverrideField::ExitSound), "off");
        root.remove();
        remove_mocks();
    }

    #[wasm_bindgen_test]
    async fn the_collapsed_summary_counts_the_overrides_set() {
        install_mocks();
        let root = mount_with(true, false, AnnouncementOverrides::default()).await;
        assert_eq!(count(&root), None, "nothing set, no count");

        set_server_overrides(SEEDED);
        wait_until("the count", || count(&root).is_some()).await;
        assert_eq!(count(&root).as_deref(), Some("2 set"));
        let badge = element(&root, "[data-testid='meeting-options-announcements-count']").unwrap();
        assert!(summary(&root).contains(Some(&badge)));
        assert!(badge.closest("[aria-hidden='true']").unwrap().is_none());

        js_sys::eval("window.__patchMode = 'hold'").unwrap();
        choose(&root, OverrideField::ExitMessage, "off");
        wait_until("the unsaved choice to count", || {
            count(&root).as_deref() == Some("3 set")
        })
        .await;
        release("ok");
        wait_until("the save to finish", || {
            *editor().saver.peek() == OverrideSaver::default()
        })
        .await;
        set_server_overrides(AnnouncementOverrides::default());
        wait_until("the count to clear", || count(&root).is_none()).await;

        let expanded = mount_with(false, false, SEEDED).await;
        assert_eq!(count(&expanded), None, "only the collapsed copy counts");
        root.remove();
        expanded.remove();
        remove_mocks();
    }

    #[wasm_bindgen_test]
    async fn the_table_has_a_row_of_three_radios_per_field_showing_the_server_values() {
        let root = mount().await;
        let group = element(&root, "[role='group']").unwrap();
        let title_id = group.get_attribute("aria-labelledby").unwrap();
        let base = title_id.strip_suffix("-title").unwrap();
        let table = element(
            &group,
            "table.override-table[data-testid='meeting-options-announcements-table']",
        )
        .expect("the overrides table");
        let heads: Vec<(String, Option<String>)> = all(&table, "thead th")
            .iter()
            .map(|th| (text(th), th.get_attribute("scope")))
            .collect();
        assert_eq!(
            heads,
            [
                ("Their choice", "col"),
                ("For everyone", "colgroup"),
                ("On", "col"),
                ("Off", "col"),
            ]
            .map(|(t, s)| (t.to_string(), Some(s.to_string())))
        );
        assert_eq!(all(&table, "tbody").len(), 2);
        assert_eq!(all(&table, "tr.override-table__row").len(), 4);
        assert_eq!(all(&table, "input[type='radio']").len(), 12);

        let expected = [
            (
                OverrideField::EntryMessage,
                "Participant joins",
                "Message",
                "off",
            ),
            (
                OverrideField::EntrySound,
                "Participant joins",
                "Sound",
                "participant",
            ),
            (
                OverrideField::ExitMessage,
                "Participant leaves",
                "Message",
                "participant",
            ),
            (
                OverrideField::ExitSound,
                "Participant leaves",
                "Sound",
                "on",
            ),
        ];
        for (field, event, channel, value) in expected {
            let row = row(&root, field);
            let event_head = element(
                &row.parent_element().unwrap(),
                "tr.override-table__event > th[scope='rowgroup']",
            )
            .unwrap();
            assert_eq!(text(&event_head), event);
            assert_eq!(text(&element(&row, "th[scope='row']").unwrap()), channel);
            let name = format!("{base}-{}", field.testid());
            let cells: Vec<_> = all(&row, "td > label.override-radio > input[type='radio']")
                .into_iter()
                .map(|el| {
                    let input = el.unchecked_into::<web_sys::HtmlInputElement>();
                    let label = input.parent_element().unwrap();
                    let mark = input.next_element_sibling().unwrap();
                    assert!(mark.class_list().contains("override-radio__mark"));
                    assert_eq!(mark.get_attribute("aria-hidden").as_deref(), Some("true"));
                    assert!(!input.disabled(), "{field:?} is never disabled");
                    (
                        input.value(),
                        input.name(),
                        input.get_attribute("aria-label").unwrap_or_default(),
                        input.get_attribute("data-testid").unwrap_or_default(),
                        label.class_list().contains("override-radio--choice"),
                    )
                })
                .collect();
            let label = field.label();
            let testid = field.testid();
            assert_eq!(
                cells,
                [
                    ("participant", "Their choice", true),
                    ("on", "On for everyone", false),
                    ("off", "Off for everyone", false),
                ]
                .map(|(v, column, choice)| (
                    v.to_string(),
                    name.clone(),
                    format!("{label}: {column}"),
                    format!("{testid}-{v}"),
                    choice,
                ))
            );
            assert_eq!(shown(&root, field), value, "{label}");
        }

        let title = gloo_utils::document().get_element_by_id(&title_id).unwrap();
        assert_eq!(
            title.text_content().as_deref(),
            Some("Join & leave notifications")
        );
        assert!(group.contains(Some(&title)));
        let hint_id = group.get_attribute("aria-describedby").unwrap();
        let hint = gloo_utils::document().get_element_by_id(&hint_id).unwrap();
        assert!(group.contains(Some(&hint)));
        assert_eq!(
            text(&hint),
            "Their choice keeps each participant's own setting. \
             Anyone recording still sees join and leave messages."
        );
        assert_eq!(error_text(&root), "");
        assert!(
            element(&root, ".toggle-error").is_none(),
            "the toggles' error line mounts only with an error"
        );
        assert!(element(&root, "details").is_none());
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn two_mounted_copies_keep_separate_radio_groups() {
        let first = mount().await;
        let second = mount().await;
        assert_ne!(
            element(&first, "[role='group']").and_then(|g| g.get_attribute("aria-labelledby")),
            element(&second, "[role='group']").and_then(|g| g.get_attribute("aria-labelledby")),
            "each mounted copy has its own heading id"
        );
        let theirs = radio(&first, OverrideField::EntryMessage, "on");
        let ours = radio(&second, OverrideField::EntryMessage, "on");
        assert_ne!(theirs.name(), ours.name());

        ours.set_checked(true);
        assert_eq!(shown(&second, OverrideField::EntryMessage), "on");
        assert_eq!(
            shown(&first, OverrideField::EntryMessage),
            "off",
            "checking a radio in one copy leaves the other copy's row alone"
        );
        first.remove();
        second.remove();
    }

    fn summary(root: &web_sys::Element) -> web_sys::Element {
        element(
            root,
            "summary[data-testid='meeting-options-announcements-summary']",
        )
        .unwrap()
    }

    #[wasm_bindgen_test]
    async fn the_pre_join_copy_starts_collapsed_when_every_override_is_the_participants_choice() {
        let root = mount_with(true, false, AnnouncementOverrides::default()).await;
        let group = element(&root, "[role='group']").unwrap();
        let details = element(&root, "[role='group'] > details")
            .expect("the collapsible group is a disclosure");
        assert!(!details.has_attribute("open"), "collapsed by default");
        let summary = summary(&root);
        let title_id = group.get_attribute("aria-labelledby").unwrap();
        let title = gloo_utils::document().get_element_by_id(&title_id).unwrap();
        assert!(summary.contains(Some(&title)));
        assert!(details.contains(Some(&radio(&root, OverrideField::ExitSound, "off"))));
        let error = error_line(&root);
        assert!(!details.contains(Some(&error)));
        assert!(group
            .last_element_child()
            .is_some_and(|last| last.is_same_node(Some(&error))));
        assert!(error.matches(":empty").unwrap());

        set_server_overrides(SEEDED);
        wait_until("the pushed override", || {
            shown(&root, OverrideField::EntryMessage) == "off"
        })
        .await;
        assert!(
            !details.has_attribute("open"),
            "an override set after mount leaves the disclosure to the host"
        );
        click(&summary);
        wait_until("the disclosure to open", || details.has_attribute("open")).await;
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn the_pre_join_copy_starts_open_when_any_override_is_set() {
        for field in OverrideField::ALL {
            let mut seed = AnnouncementOverrides::default();
            seed.set(field, On);
            let root = mount_with(true, false, seed).await;
            let details = element(&root, "[role='group'] > details").unwrap();
            assert!(details.has_attribute("open"), "{field:?} is set");
            root.remove();
        }

        let root = mount_with(true, false, SEEDED).await;
        let details = element(&root, "[role='group'] > details").unwrap();
        assert!(details.has_attribute("open"));
        click(&summary(&root));
        wait_until("the disclosure to close", || !details.has_attribute("open")).await;
        set_server_overrides(AnnouncementOverrides {
            exit_message: On,
            ..SEEDED
        });
        wait_until("the pushed override", || {
            shown(&root, OverrideField::ExitMessage) == "on"
        })
        .await;
        assert!(!details.has_attribute("open"), "the host's collapse sticks");
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_save_failing_behind_a_collapsed_disclosure_is_still_shown() {
        install_mocks();
        let root = mount_with(true, false, SEEDED).await;
        let details = element(&root, "[role='group'] > details").unwrap();
        js_sys::eval("window.__patchMode = 'hold'").unwrap();

        choose(&root, OverrideField::EntrySound, "off");
        wait_until("the PATCH", || patches().len() == 1).await;
        let entry_sound = row(&root, OverrideField::EntrySound);
        assert!(is_rendered(&entry_sound));
        click(&summary(&root));
        wait_until("the disclosure to close", || !details.has_attribute("open")).await;
        assert!(!is_rendered(&entry_sound), "the collapsed rows are hidden");

        release("fail");
        wait_until("the error line", || !error_text(&root).is_empty()).await;
        assert_eq!(
            error_text(&root),
            "Couldn't save Join sound. It's back to \"Their choice\"."
        );
        let error = error_line(&root);
        assert!(error.closest("details").unwrap().is_none());
        assert!(
            is_rendered(&error),
            "the error shows with the rows collapsed"
        );
        assert!(!details.has_attribute("open"));
        root.remove();
        remove_mocks();
    }

    #[wasm_bindgen_test]
    async fn a_closed_copy_takes_its_save_error_with_it() {
        install_mocks();
        let root = mount().await;
        js_sys::eval("window.__patchMode = 'fail'").unwrap();

        choose(&root, OverrideField::EntrySound, "off");
        wait_until("the error line", || !error_text(&root).is_empty()).await;
        toggle(&root, "toggle-controls", false).await;
        toggle(&root, "toggle-controls", true).await;
        assert_eq!(error_text(&root), "", "the next copy starts clean");
        root.remove();
        remove_mocks();
    }

    #[wasm_bindgen_test]
    async fn the_save_error_paints_on_the_opaque_error_chip_in_both_themes() {
        install_mocks();
        let sheets = [add_sheet(GLOBAL_CSS), add_sheet(STYLE_CSS)];
        let html = gloo_utils::document().document_element().unwrap();
        let theme = html.get_attribute("data-theme");
        let probe = gloo_utils::document().create_element("span").unwrap();
        probe
            .set_attribute(
                "style",
                "color: var(--error-chip-text); background: var(--error-chip-bg)",
            )
            .unwrap();
        gloo_utils::document()
            .body()
            .unwrap()
            .append_child(&probe)
            .unwrap();
        let root = mount().await;
        js_sys::eval("window.__patchMode = 'fail'").unwrap();
        choose(&root, OverrideField::EntrySound, "off");
        wait_until("the error line", || !error_text(&root).is_empty()).await;
        let line = error_line(&root);
        let pair =
            |el: &web_sys::Element| (computed(el, "color"), computed(el, "background-color"));

        let mut seen = Vec::new();
        for mode in ["light", "dark"] {
            html.set_attribute("data-theme", mode).unwrap();
            assert_eq!(pair(&line), pair(&probe), "{mode}");
            seen.push(pair(&probe));
        }
        assert_ne!(seen[0], seen[1], "each theme has its own pair");

        match theme {
            Some(theme) => html.set_attribute("data-theme", &theme).unwrap(),
            None => html.remove_attribute("data-theme").unwrap(),
        }
        probe.remove();
        root.remove();
        sheets.iter().for_each(web_sys::Element::remove);
        remove_mocks();
    }

    #[wasm_bindgen_test]
    async fn the_group_ends_flush_with_its_table_while_no_error_shows() {
        let sheets = [add_sheet(GLOBAL_CSS), add_sheet(STYLE_CSS)];
        for collapsible in [false, true] {
            let root = mount_with(collapsible, false, SEEDED).await;
            let group = element(&root, "[role='group']").unwrap();
            let table = element(&group, "table.override-table").unwrap();
            assert_eq!(
                computed(&table, "table-layout"),
                "fixed",
                "style.css did not load, so this test guards nothing"
            );
            let above = if collapsible {
                element(&root, "details").unwrap()
            } else {
                table
            };
            let gap = group.get_bounding_client_rect().bottom()
                - above.get_bounding_client_rect().bottom();
            assert!(gap.abs() < 0.5, "collapsible {collapsible}: {gap}px below");
            root.remove();
        }
        sheets.iter().for_each(web_sys::Element::remove);
    }

    #[wasm_bindgen_test]
    async fn the_set_count_keeps_to_one_line_in_a_narrow_summary() {
        let sheets = [add_sheet(GLOBAL_CSS), add_sheet(STYLE_CSS)];
        let root = mount_with(true, false, SEEDED).await;
        let badge = element(&root, "[data-testid='meeting-options-announcements-count']").unwrap();
        let one_line = badge.get_bounding_client_rect().height();
        root.set_attribute("style", "width: 200px").unwrap();
        assert!(summary(&root).get_bounding_client_rect().width() < 250.0);
        assert!(
            (badge.get_bounding_client_rect().height() - one_line).abs() < 0.5,
            "\"2 set\" wrapped"
        );
        assert!(
            badge.scroll_width() <= badge.client_width(),
            "the count overflows"
        );
        root.remove();
        sheets.iter().for_each(web_sys::Element::remove);
    }

    #[wasm_bindgen_test]
    async fn the_table_fits_a_phone_and_the_dialog_and_each_whole_cell_is_its_radio() {
        let sheets = [add_sheet(GLOBAL_CSS), add_sheet(STYLE_CSS)];
        let root = mount().await;
        let table = element(&root, "table.override-table").unwrap();
        assert_eq!(
            computed(&table, "table-layout"),
            "fixed",
            "style.css loaded"
        );
        let line_boxes = js_sys::Function::new_with_args(
            "el",
            "const range = document.createRange(); range.selectNodeContents(el); \
             return range.getClientRects().length;",
        );
        let one_line = |width: u32| {
            for head in all(&table, "th[scope='row']") {
                let lines = line_boxes
                    .call1(&wasm_bindgen::JsValue::NULL, &head)
                    .unwrap()
                    .as_f64();
                assert_eq!(lines, Some(1.0), "{width}px: {:?} wraps", text(&head));
            }
        };
        for width in [200, 228, 252, 338] {
            root.set_attribute(
                "style",
                &format!(
                    "position: fixed; bottom: 0; left: 0; z-index: 2147483647; width: {width}px"
                ),
            )
            .unwrap();
            assert_eq!(table.get_bounding_client_rect().width(), f64::from(width));
            assert!(
                root.scroll_width() <= root.client_width(),
                "{width}px: the table overflows sideways"
            );
            for head in all(&table, "th") {
                assert!(
                    head.scroll_width() <= head.client_width(),
                    "{width}px: {:?} overflows its cell",
                    text(&head)
                );
            }
            one_line(width);
            for input in all(&table, "input[type='radio']") {
                let cell = input.closest("td").unwrap().unwrap();
                let (a, b) = (
                    input.get_bounding_client_rect(),
                    cell.get_bounding_client_rect(),
                );
                assert!(b.height() >= 44.0, "{width}px: a {}px cell", b.height());
                for (x, y) in [
                    (a.left(), b.left()),
                    (a.right(), b.right()),
                    (a.top(), b.top()),
                    (a.bottom(), b.bottom()),
                ] {
                    assert!((x - y).abs() <= 1.0, "{width}px: the radio misses its cell");
                }
                let (x, y) = (b.left() + b.width() / 2.0, b.top() + b.height() / 2.0);
                let hit = gloo_utils::document().element_from_point(x as f32, y as f32);
                assert!(
                    hit.as_ref()
                        .is_some_and(|hit| hit.is_same_node(Some(&input))),
                    "{width}px: a tap at ({x}, {y}) of a {}x{} viewport lands on {:?}",
                    gloo_utils::window()
                        .inner_width()
                        .unwrap()
                        .as_f64()
                        .unwrap(),
                    gloo_utils::window()
                        .inner_height()
                        .unwrap()
                        .as_f64()
                        .unwrap(),
                    hit.map(|h| h.outer_html())
                );
            }
        }
        root.set_attribute("style", "width: 160px").unwrap();
        one_line(160);
        root.remove();
        sheets.iter().for_each(web_sys::Element::remove);
    }

    #[wasm_bindgen_test]
    async fn the_tick_marks_paint_from_theme_tokens_in_both_themes() {
        let sheets = [add_sheet(GLOBAL_CSS), add_sheet(STYLE_CSS)];
        let html = gloo_utils::document().document_element().unwrap();
        let theme = html.get_attribute("data-theme");
        let root = mount().await;
        let token = |name: &str| {
            let probe = gloo_utils::document().create_element("span").unwrap();
            probe
                .set_attribute("style", &format!("color: var({name})"))
                .unwrap();
            root.append_child(&probe).unwrap();
            let color = computed(&probe, "color");
            probe.remove();
            color
        };
        let mark = |value: &str| {
            radio(&root, OverrideField::EntrySound, value)
                .next_element_sibling()
                .unwrap()
        };
        let tick = |el: &web_sys::Element| {
            gloo_utils::window()
                .get_computed_style_with_pseudo_elt(el, "::after")
                .unwrap()
                .unwrap()
                .get_property_value("border-right-color")
                .unwrap()
        };
        for (mode, ring) in [("dark", "--text-tertiary"), ("light", "--text-secondary")] {
            html.set_attribute("data-theme", mode).unwrap();
            assert_eq!(shown(&root, OverrideField::EntrySound), "participant");
            let choice = mark("participant");
            wait_until(&format!("the {mode} ring and choice fill"), || {
                computed(&mark("on"), "border-top-color") == token(ring)
                    && computed(&choice, "background-color") == token("--text-secondary")
            })
            .await;
            assert_eq!(tick(&choice), token("--surface-elevated"), "{mode}");
            let divider = element(&row(&root, OverrideField::EntrySound), "th").unwrap();
            assert_eq!(
                computed(&divider, "border-top-color"),
                token("--border"),
                "{mode} row divider"
            );
            radio(&root, OverrideField::EntrySound, "on").set_checked(true);
            wait_until(&format!("the {mode} accent fill"), || {
                computed(&mark("on"), "background-color") == token("--accent")
            })
            .await;
            assert_eq!(tick(&mark("on")), token("--on-dark-text"), "{mode}");
            radio(&root, OverrideField::EntrySound, "participant").set_checked(true);
        }
        match theme {
            Some(theme) => html.set_attribute("data-theme", &theme).unwrap(),
            None => html.remove_attribute("data-theme").unwrap(),
        }
        root.remove();
        sheets.iter().for_each(web_sys::Element::remove);
    }

    #[wasm_bindgen_test]
    fn a_cell_s_hover_fill_needs_a_primary_pointer_that_can_hover() {
        let sheet = add_sheet(STYLE_CSS);
        let conditions = js_sys::Function::new_with_args(
            "el",
            "const out = []; \
             const walk = (rules, cond) => { for (const r of rules) { \
               if ((r.selectorText || '').includes('override-radio:hover')) out.push(cond); \
               if (r.cssRules) walk(r.cssRules, r.conditionText || cond); } }; \
             walk(el.sheet.cssRules, 'none'); return out.join(' | ');",
        )
        .call1(&wasm_bindgen::JsValue::NULL, &sheet)
        .unwrap()
        .as_string()
        .unwrap_or_default();
        sheet.remove();
        assert_eq!(conditions, "(hover: hover)");
    }

    #[wasm_bindgen_test]
    async fn the_slot_follows_the_allow_guests_row() {
        let root = mount_with(false, true, SEEDED).await;
        let slot = element(&root, "#guest-slot").unwrap();
        let row = slot.previous_element_sibling().unwrap();
        assert_eq!(
            element(&row, ".settings-option-label")
                .and_then(|l| l.text_content())
                .as_deref(),
            Some("Allow guests")
        );
        let group = element(&root, "[role='group']").unwrap();
        assert!(!group.contains(Some(&slot)));
        assert_eq!(
            slot.compare_document_position(&group) & web_sys::Node::DOCUMENT_POSITION_FOLLOWING,
            web_sys::Node::DOCUMENT_POSITION_FOLLOWING
        );
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_reopened_copy_continues_the_save_in_flight() {
        install_mocks();
        let root = mount().await;
        js_sys::eval("window.__patchMode = 'hold'").unwrap();

        choose(&root, OverrideField::EntryMessage, "on");
        wait_until("the first PATCH", || patches().len() == 1).await;
        toggle(&root, "toggle-controls", false).await;
        toggle(&root, "toggle-controls", true).await;
        assert_eq!(
            shown(&root, OverrideField::EntryMessage),
            "on",
            "the reopened copy shows the unsaved choice"
        );

        choose(&root, OverrideField::EntryMessage, "participant");
        TimeoutFuture::new(OVERRIDE_PICK_SETTLE_MS + 150).await;
        assert_eq!(patches().len(), 1, "the reopened copy waits for the save");

        release("hold");
        wait_until("the follow-up PATCH", || patches().len() == 2).await;
        release("ok");
        wait_until("the last save to land", || {
            server_entry_message() == Participant
        })
        .await;
        TimeoutFuture::new(50).await;
        assert_eq!(
            patches(),
            [
                r#"{"entry_message_override":"on"}"#,
                r#"{"entry_message_override":"participant"}"#,
            ]
        );
        assert_eq!(shown(&root, OverrideField::EntryMessage), "participant");
        root.remove();
        remove_mocks();
    }

    #[wasm_bindgen_test]
    async fn a_choice_queued_behind_a_save_follows_it_when_the_owner_unmounts() {
        let on = r#"{"entry_message_override":"on"}"#;
        let participant = r#"{"entry_message_override":"participant"}"#;
        for (settled, reply, sent) in [
            (true, "ok", vec![on, participant]),
            (false, "ok", vec![on, participant]),
            (false, "fail", vec![on]),
        ] {
            install_mocks();
            let root = mount().await;
            js_sys::eval("window.__patchMode = 'hold'").unwrap();

            choose(&root, OverrideField::EntryMessage, "on");
            wait_until("the first PATCH", || patches().len() == 1).await;
            choose(&root, OverrideField::EntryMessage, "participant");
            if settled {
                TimeoutFuture::new(OVERRIDE_PICK_SETTLE_MS + 150).await;
            }
            toggle(&root, "toggle-owner", false).await;
            TimeoutFuture::new(OVERRIDE_PICK_SETTLE_MS + 150).await;
            assert_eq!(patches().len(), 1, "settled {settled}: one PATCH at a time");

            release(reply);
            wait_until("the queued PATCH", || patches().len() == sent.len()).await;
            TimeoutFuture::new(50).await;
            assert_eq!(patches(), sent, "settled {settled}, {reply}");
            assert_eq!(page_errors(), "");
            root.remove();
            remove_mocks();
        }
    }

    #[wasm_bindgen_test]
    async fn a_closed_copy_s_wait_cannot_settle_a_choice_made_after_it_reopens() {
        install_mocks();
        let root = mount().await;
        let window = f64::from(OVERRIDE_PICK_SETTLE_MS);
        let controls = element(&root, "#toggle-controls").unwrap();
        choose(&root, OverrideField::EntrySound, "on");
        click(&controls);
        before_any_timer("the close", || !has_table(&root)).await;
        click(&controls);
        before_any_timer("the reopen", || has_table(&root)).await;
        TimeoutFuture::new(OVERRIDE_PICK_SETTLE_MS / 2).await;

        let start = js_sys::Date::now();
        choose(&root, OverrideField::ExitMessage, "on");
        wait_until("the reopened copy's PATCH", || patches().len() == 2).await;
        let waited = js_sys::Date::now() - start;
        assert!(waited >= window - 1.0, "sent after {waited}ms");
        wait_until("the save to land", saved).await;
        assert_eq!(
            patches(),
            [
                r#"{"entry_sound_override":"on"}"#,
                r#"{"exit_message_override":"on"}"#,
            ]
        );
        root.remove();
        remove_mocks();
    }

    fn page_errors() -> String {
        js_sys::eval("window.__errors.join('; ')")
            .unwrap()
            .as_string()
            .unwrap_or_default()
    }

    fn fire_change(input: &web_sys::HtmlInputElement) {
        js_sys::Function::new_with_args(
            "el",
            "el.dispatchEvent(new Event('change', { bubbles: true }));",
        )
        .call1(&wasm_bindgen::JsValue::NULL, input)
        .unwrap();
    }

    #[wasm_bindgen_test]
    async fn a_sweep_of_choices_saves_once_with_the_last_one() {
        install_mocks();
        let root = mount().await;
        for value in ["on", "off", "on", "off"] {
            choose(&root, OverrideField::EntrySound, value);
            assert_eq!(shown(&root, OverrideField::EntrySound), value);
            assert_eq!(
                editor()
                    .saver
                    .peek()
                    .shown(OverrideField::EntrySound, &SEEDED)
                    .as_str(),
                value,
                "the choice is recorded at once"
            );
            TimeoutFuture::new(OVERRIDE_PICK_SETTLE_MS / 2).await;
        }
        assert_eq!(
            patches(),
            Vec::<String>::new(),
            "each choice restarts the wait"
        );
        wait_until("the save to land", saved).await;
        assert_eq!(patches(), [r#"{"entry_sound_override":"off"}"#]);
        assert_eq!(shown(&root, OverrideField::EntrySound), "off");
        root.remove();
        remove_mocks();
    }

    fn saved() -> bool {
        !patches().is_empty() && *editor().saver.peek() == OverrideSaver::default()
    }

    #[wasm_bindgen_test]
    async fn choosing_what_the_table_already_shows_saves_nothing() {
        install_mocks();
        let root = mount().await;
        let current = radio(&root, OverrideField::EntrySound, "participant");
        assert!(current.checked());
        fire_change(&current);
        choose(&root, OverrideField::EntrySound, "on");
        choose(&root, OverrideField::EntrySound, "participant");
        TimeoutFuture::new(OVERRIDE_PICK_SETTLE_MS + 150).await;
        assert_eq!(patches(), Vec::<String>::new());
        assert_eq!(shown(&root, OverrideField::EntrySound), "participant");
        assert_eq!(*editor().saver.peek(), OverrideSaver::default());
        root.remove();
        remove_mocks();
    }

    #[wasm_bindgen_test]
    async fn a_single_choice_saves_once_after_the_wait() {
        install_mocks();
        let root = mount().await;
        let start = js_sys::Date::now();
        choose(&root, OverrideField::ExitMessage, "on");
        wait_until("the PATCH", || !patches().is_empty()).await;
        let waited = js_sys::Date::now() - start;
        assert!(
            waited >= f64::from(OVERRIDE_PICK_SETTLE_MS) - 1.0,
            "sent after {waited}ms"
        );
        wait_until("the save to land", saved).await;
        assert_eq!(patches(), [r#"{"exit_message_override":"on"}"#]);
        root.remove();
        remove_mocks();
    }

    async fn choose_then_unmount(root: &web_sys::Element, button: &str) {
        choose(root, OverrideField::EntrySound, "on");
        choose(root, OverrideField::EntrySound, "off");
        click(&element(root, &format!("#{button}")).unwrap());
        before_any_timer("the unmount's PATCH", || {
            !has_table(root) && patch_calls() == 1
        })
        .await;
    }

    #[wasm_bindgen_test]
    async fn a_choice_still_waiting_when_its_owner_unmounts_is_sent_once() {
        install_mocks();
        let root = mount().await;
        choose_then_unmount(&root, "toggle-owner").await;
        TimeoutFuture::new(OVERRIDE_PICK_SETTLE_MS + 150).await;
        assert_eq!(patches(), [r#"{"entry_sound_override":"off"}"#]);
        assert_eq!(page_errors(), "");
        root.remove();
        remove_mocks();
    }

    #[wasm_bindgen_test]
    async fn a_choice_still_waiting_when_its_copy_closes_is_saved_once() {
        install_mocks();
        let root = mount().await;
        choose_then_unmount(&root, "toggle-controls").await;
        wait_until("the save to land", saved).await;
        TimeoutFuture::new(OVERRIDE_PICK_SETTLE_MS + 150).await;
        assert_eq!(patches(), [r#"{"entry_sound_override":"off"}"#]);
        assert_eq!(editor().server.peek().entry_sound, Off);
        root.remove();
        remove_mocks();
    }

    #[wasm_bindgen_test]
    async fn only_a_change_to_the_unsaved_choices_re_renders_their_readers() {
        install_mocks();
        let root = mount().await;
        js_sys::eval("window.__patchMode = 'hold'").unwrap();
        let renders = || SAVER_RENDERS.with(Cell::get);
        let start = renders();

        fire_change(&radio(&root, OverrideField::EntrySound, "participant"));
        TimeoutFuture::new(50).await;
        assert_eq!(renders(), start, "a choice the table already shows");

        choose(&root, OverrideField::EntrySound, "on");
        TimeoutFuture::new(OVERRIDE_PICK_SETTLE_MS / 2).await;
        choose(&root, OverrideField::EntrySound, "off");
        wait_until("the PATCH", || patches().len() == 1).await;
        TimeoutFuture::new(50).await;
        assert_eq!(
            renders(),
            start + 3,
            "two choices and one settle; the superseded wait writes nothing"
        );
        root.remove();
        remove_mocks();
    }

    #[wasm_bindgen_test]
    async fn rapid_changes_end_on_the_last_choice_everywhere() {
        install_mocks();
        let root = mount().await;
        js_sys::eval("window.__patchMode = 'hold'").unwrap();

        choose(&root, OverrideField::EntryMessage, "on");
        wait_until("the first PATCH", || patches().len() == 1).await;
        choose(&root, OverrideField::EntryMessage, "participant");
        choose(&root, OverrideField::EntryMessage, "off");
        choose(&root, OverrideField::EntryMessage, "participant");
        TimeoutFuture::new(OVERRIDE_PICK_SETTLE_MS + 150).await;
        assert_eq!(patches().len(), 1, "one PATCH in flight at a time");
        assert_eq!(shown(&root, OverrideField::EntryMessage), "participant");

        release("hold");
        wait_until("the follow-up PATCH", || patches().len() == 2).await;
        assert_eq!(
            shown(&root, OverrideField::EntryMessage),
            "participant",
            "the superseded response must not overwrite the newer choice"
        );
        release("ok");
        wait_until("the last save to land", || {
            server_entry_message() == Participant
        })
        .await;
        TimeoutFuture::new(50).await;

        assert_eq!(
            patches(),
            [
                r#"{"entry_message_override":"on"}"#,
                r#"{"entry_message_override":"participant"}"#,
            ]
        );
        assert_eq!(shown(&root, OverrideField::EntryMessage), "participant");
        assert_eq!(error_text(&root), "");
        root.remove();
        remove_mocks();
    }

    #[wasm_bindgen_test]
    async fn a_failed_save_restores_the_server_value_and_says_so() {
        install_mocks();
        let root = mount().await;
        js_sys::eval("window.__patchMode = 'fail'").unwrap();
        let off = radio(&root, OverrideField::EntrySound, "off");
        off.focus().unwrap();

        choose(&root, OverrideField::EntrySound, "off");
        wait_until("the error line", || !error_text(&root).is_empty()).await;

        assert_eq!(
            error_text(&root),
            "Couldn't save Join sound. It's back to \"Their choice\"."
        );
        assert_eq!(shown(&root, OverrideField::EntrySound), "participant");
        assert!(!off.checked());
        assert!(is_focused(&off), "focus stays on the radio");
        assert_eq!(server_entry_message(), Off, "other fields are untouched");
        assert!(
            element(&root, ".toggle-error").is_none(),
            "the toggles' error line stays out of it"
        );

        js_sys::eval("window.__patchMode = 'ok'").unwrap();
        choose(&root, OverrideField::EntrySound, "on");
        wait_until("the retry to land", || patches().len() == 2).await;
        wait_until("the error to clear", || error_text(&root).is_empty()).await;
        assert_eq!(patches()[1], r#"{"entry_sound_override":"on"}"#);
        root.remove();
        remove_mocks();
    }

    #[wasm_bindgen_test]
    async fn each_radio_saves_its_own_field_and_value() {
        install_mocks();
        let root = mount().await;
        let choices = [
            (
                OverrideField::EntryMessage,
                Participant,
                "entry_message_override",
            ),
            (OverrideField::EntrySound, On, "entry_sound_override"),
            (OverrideField::ExitMessage, Off, "exit_message_override"),
            (OverrideField::ExitSound, Participant, "exit_sound_override"),
        ];
        for (i, (field, value, key)) in choices.into_iter().enumerate() {
            choose(&root, field, value.as_str());
            wait_until("the PATCH", || patches().len() == i + 1).await;
            assert_eq!(patches()[i], format!(r#"{{"{key}":"{}"}}"#, value.as_str()));
            wait_until("the save to land", || {
                editor().server.peek().get(field) == value
            })
            .await;
            assert_eq!(shown(&root, field), value.as_str());
            assert!(radio(&root, field, value.as_str()).checked());
        }
        assert_eq!(error_text(&root), "");
        root.remove();
        remove_mocks();
    }

    #[wasm_bindgen_test]
    async fn tab_wraps_inside_the_dialog_through_each_row_s_checked_radio() {
        let root = mount_dialog().await;
        let done = element(&root, "button[aria-label='Close meeting options']").unwrap();
        let last_on = radio(&root, OverrideField::ExitSound, "on");
        let last_off = radio(&root, OverrideField::ExitSound, "off");
        assert!(last_on.checked() && !last_off.checked());
        let last_in_dialog = all(
            &root,
            "#meeting-options-dialog button, #meeting-options-dialog input",
        )
        .pop()
        .unwrap();
        assert!(last_in_dialog.is_same_node(Some(&last_off)));

        done.unchecked_ref::<web_sys::HtmlElement>()
            .focus()
            .unwrap();
        assert!(tab(&done, true), "Shift+Tab on Done wraps");
        assert!(
            is_focused(&last_on),
            "to the last row's checked radio, not its last one"
        );
        assert!(tab(&last_on, false), "Tab on the last row wraps");
        assert!(is_focused(&done));

        last_off.focus().unwrap();
        assert!(
            tab(&last_off, false),
            "an unchecked radio stands for its row"
        );
        assert!(is_focused(&done));

        let middle = radio(&root, OverrideField::EntryMessage, "on");
        assert!(!middle.checked());
        middle.focus().unwrap();
        assert!(
            !tab(&middle, true),
            "Shift+Tab inside the table is the browser's"
        );
        assert!(!tab(&middle, false));
        assert!(is_focused(&middle));
        root.remove();
    }
}

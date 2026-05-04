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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use dioxus::prelude::*;
use std::rc::Rc;

#[component]
pub fn PreJoinSettingsCard(
    is_owner: bool,
    meeting_id: String,
    waiting_room_toggle: Signal<bool>,
    admitted_can_admit_toggle: Signal<bool>,
    end_on_host_leave_toggle: Signal<bool>,
    allow_guests_toggle: Signal<bool>,
    saving: Signal<bool>,
    toggle_error: Signal<Option<String>>,
    connection_error: Signal<Option<String>>,
    on_join: EventHandler<()>,
) -> Element {
    // Helper to update a meeting setting with rollback on error.
    // All toggle signals are captured by value (Signal is Copy) so the
    // async block can update whichever fields the server returns.
    let update_setting = use_hook(|| {
        Rc::new(
            move |meeting_id: String,
                  waiting_room: Option<bool>,
                  admitted_can_admit: Option<bool>,
                  end_on_host_leave_opt: Option<bool>,
                  allow_guests_opt: Option<bool>,
                  mut rollback_signal: Signal<bool>,
                  old_val: bool,
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
                            saving.set(false);
                            toggle_error.set(Some(format!("Failed to update setting: {e}")));
                        }
                    }
                });
            },
        )
    });

    let aca_opacity = if waiting_room_toggle() { "1" } else { "0.5" };

    rsx! {
        div { class: "settings-card",
            if is_owner {
                h3 { class: "settings-card-title", "Meeting Options" }
            } else {
                div { class: "join-meeting-header",
                    h2 { class: "join-meeting-title",
                        span { class: "join-meeting-title-text", "Join Meeting" }
                        span { class: "join-meeting-id", "{meeting_id}" }
                    }
                    p { class: "join-meeting-subtitle",
                        "Click the button to participate in the meeting."
                    }
                }
            }

            if let Some(err) = connection_error() {
                p { class: "toggle-error", "{err}" }
            }

            if is_owner {
                {
                    let meeting_id_for_toggle = meeting_id.clone();
                    rsx! {
                        div { class: "settings-option-row",
                            span { class: "settings-option-label", "Waiting Room" }
                            div { class: "settings-option-controls",
                                span {
                                    class: "settings-info-icon",
                                    title: "Participants must be admitted by the host before joining",
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
                                crate::components::toggle_switch::ToggleSwitch {
                                    enabled: waiting_room_toggle(),
                                    disabled: saving(),
                                    on_toggle: {
                                        let meeting_id = meeting_id_for_toggle.clone();
                                        let update_setting = update_setting.clone();
                                        move |new_val: bool| {
                                            if saving() {
                                                return;
                                            }
                                            let old_val = !new_val;
                                            waiting_room_toggle.set(new_val);
                                            // When disabling waiting room, also disable admitted_can_admit
                                            if !new_val {
                                                admitted_can_admit_toggle.set(false);
                                            }
                                            let aca = if new_val { None } else { Some(false) };
                                            update_setting(
                                                meeting_id.clone(),
                                                Some(new_val),
                                                aca,
                                                None,
                                                None,
                                                waiting_room_toggle,
                                                old_val,
                                                saving,
                                                toggle_error,
                                            );
                                        }
                                    },
                                }
                            }
                        }
                        div {
                            class: "settings-option-row",
                            style: "opacity: {aca_opacity};",
                            span { class: "settings-option-label", "Admitted can admit" }
                            div { class: "settings-option-controls",
                                span {
                                    class: "settings-info-icon",
                                    title: "Allow admitted participants to also admit others from the waiting room",
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
                                crate::components::toggle_switch::ToggleSwitch {
                                    enabled: admitted_can_admit_toggle(),
                                    disabled: saving() || !waiting_room_toggle(),
                                    on_toggle: {
                                        let meeting_id = meeting_id_for_toggle.clone();
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
                                                saving,
                                                toggle_error,
                                            );
                                        }
                                    },
                                }
                            }
                        }
                        div { class: "settings-option-row", style: "opacity: 1;",
                            span { class: "settings-option-label", "End meeting when host leaves" }
                            div { class: "settings-option-controls",
                                span {
                                    class: "settings-info-icon",
                                    title: "Automatically end the meeting for all participants when the host disconnects",
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
                                crate::components::toggle_switch::ToggleSwitch {
                                    enabled: end_on_host_leave_toggle(),
                                    disabled: saving(),
                                    on_toggle: {
                                        let meeting_id = meeting_id_for_toggle.clone();
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
                                                saving,
                                                toggle_error,
                                            );
                                        }
                                    },
                                }
                            }
                        }
                        div { class: "settings-option-row", style: "opacity: 1;",
                            span { class: "settings-option-label", "Allow guests" }
                            div { class: "settings-option-controls",
                                span {
                                    class: "settings-info-icon",
                                    title: "Allow guests to join the meeting without an account",
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
                                crate::components::toggle_switch::ToggleSwitch {
                                    enabled: allow_guests_toggle(),
                                    disabled: saving(),
                                    on_toggle: {
                                        let meeting_id = meeting_id_for_toggle.clone();
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
                        p { style: "text-align: center; color: rgba(255,255,255,0.6); font-size: 0.8rem; margin-top: 0.5rem; margin-bottom: 0.25rem;",
                            if waiting_room_toggle() {
                                "Participants will wait for your approval before joining"
                            } else {
                                "Participants will join the meeting directly"
                            }
                        }
                    }
                }
            }

            div { class: "settings-action-row",
                button {
                    class: "btn-apple btn-primary settings-action-btn",
                    onclick: move |_| {
                        on_join.call(());
                    },
                    if is_owner { "Start Meeting" } else { "Join Meeting" }
                }
            }
        }
    }
}

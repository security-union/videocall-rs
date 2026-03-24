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

use crate::context::validate_display_name;
use dioxus::prelude::*;

#[component]
pub fn UpdateDisplayNameModal(
    visible: bool,
    current_display_name: String,
    meeting_id: String,
    on_close: EventHandler<()>,
    on_success: EventHandler<String>,
) -> Element {
    let pending_name = use_signal(|| current_display_name.clone());
    let error_message = use_signal(|| None::<String>);
    let is_updating = use_signal(|| false);
    let success_message = use_signal(|| None::<String>);

    rsx! {
        if visible {
            div {
                class: "glass-backdrop",
                onkeydown: {
                    let current_display_name = current_display_name.clone();
                    let mut pending_name = pending_name;
                    let mut error_message = error_message;
                    let on_close = on_close;

                    move |e: Event<KeyboardData>| {
                        let key = e.key().to_string();

                        if key == "Escape" {
                            error_message.set(None);
                            pending_name.set(current_display_name.clone());
                            on_close.call(());
                        }
                    }
                },

                div {
                    class: "card-apple",
                    style: "width: 380px;",

                    h3 { style: "margin-top:0;", "Edit your display name" }

                    p {
                        style: "color:#AEAEB2; margin-top:0.25rem;",
                        "Your new name will be visible to other participants in real-time."
                    }

                    form {
                        onsubmit: {
                            let meeting_id = meeting_id.clone();
                            let mut error_message = error_message;
                            let mut is_updating = is_updating;
                            let mut success_message = success_message;
                            let on_success = on_success;

                            move |e: Event<FormData>| {
                                e.prevent_default();

                                if is_updating() {
                                    return;
                                }

                                let new_name = pending_name().trim().to_string();

                                if new_name.is_empty() {
                                    error_message.set(Some("Display name cannot be empty.".to_string()));
                                    return;
                                }

                                match validate_display_name(&new_name) {
                                    Ok(valid_name) => {
                                        let meeting_id = meeting_id.clone();
                                        let valid_name_clone = valid_name.clone();

                                        is_updating.set(true);
                                        error_message.set(None);
                                        success_message.set(None);

                                        wasm_bindgen_futures::spawn_local(async move {
                                            log::info!("RENAME: API CALL INITIATED for: {}", valid_name_clone);

                                            match crate::meeting_api::update_display_name(&meeting_id, &valid_name_clone).await {
                                                Ok(_) => {
                                                    log::info!("RENAME: API CALL SUCCESS");

                                                    is_updating.set(false);
                                                    success_message.set(Some("Display name updated!".to_string()));
                                                    on_success.call(valid_name_clone);

                                                    gloo_timers::callback::Timeout::new(2000, move || {
                                                        success_message.set(None);
                                                    }).forget();
                                                }
                                                Err(e) => {
                                                    log::error!("RENAME: API CALL FAILED: {}", e);

                                                    is_updating.set(false);
                                                    error_message.set(Some(format!(
                                                        "Failed to update display name: {}",
                                                        e
                                                    )));
                                                }
                                            }
                                        });
                                    }
                                    Err(msg) => {
                                        error_message.set(Some(msg));
                                    }
                                }
                            }
                        },

                        input {
                            class: "input-apple",
                            value: "{pending_name}",
                            oninput: {
                                let mut pending_name = pending_name;
                                let mut error_message = error_message;
                                let mut success_message = success_message;

                                move |e: Event<FormData>| {
                                    pending_name.set(e.value());
                                    error_message.set(None);
                                    success_message.set(None);
                                }
                            },
                            placeholder: "Enter new display name",
                            autofocus: true,
                            disabled: is_updating(),
                        }

                        if let Some(err) = error_message() {
                            p { style: "color:#FF453A; margin-top:6px; font-size:12px;", "{err}" }
                        }

                        if let Some(msg) = success_message() {
                            p { style: "color:#34C759; margin-top:6px; font-size:12px;", "{msg}" }
                        }

                        div {
                            style: "display:flex; gap:8px; justify-content:flex-end; margin-top:12px;",

                            button {
                                r#type: "button",
                                class: "btn-apple btn-secondary btn-sm",
                                onclick: {
                                    let current_display_name = current_display_name.clone();
                                    let mut pending_name = pending_name;
                                    let mut error_message = error_message;
                                    let mut success_message = success_message;
                                    let on_close = on_close;

                                    move |_| {
                                        error_message.set(None);
                                        success_message.set(None);
                                        pending_name.set(current_display_name.clone());
                                        on_close.call(());
                                    }
                                },
                                disabled: is_updating(),
                                "Cancel"
                            }

                            button {
                                r#type: "submit",
                                class: "btn-apple btn-primary btn-sm",
                                disabled: is_updating() || pending_name().trim().is_empty(),
                                if is_updating() { "Updating..." } else { "Save" }
                            }
                        }
                    }
                }
            }
        }
    }
}

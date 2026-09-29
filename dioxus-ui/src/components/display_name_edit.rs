// SPDX-License-Identifier: MIT OR Apache-2.0

//! Display-name rename, shared by the peer-list modal and the self tile's
//! inline editor (issue 2794).

use crate::context::{save_display_name_to_storage, validate_display_name, DISPLAY_NAME_MAX_LEN};
use dioxus::prelude::*;
use wasm_bindgen::JsCast;

const SELF_TILE_NAME_ERROR_ID: &str = "self-tile-name-error";
const RENAME_SUCCESS_ANNOUNCEMENT: &str = "Display name updated";

/// Trim, reject empty, then apply the shared display-name rules.
pub fn validate_rename(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Display name cannot be empty.".to_string());
    }
    validate_display_name(trimmed)
}

/// Rename via the meeting API and persist the name locally on success.
pub async fn persist_display_name(
    meeting_id: &str,
    valid_name: &str,
    session_id: Option<u64>,
) -> Result<(), String> {
    log::info!("RENAME: API CALL INITIATED for: {valid_name} (session_id: {session_id:?})");
    match crate::meeting_api::update_display_name(meeting_id, valid_name, session_id).await {
        Ok(_) => {
            log::info!("RENAME: API CALL SUCCESS");
            save_display_name_to_storage(valid_name);
            Ok(())
        }
        Err(e) => {
            log::error!("RENAME: API CALL FAILED: {e}");
            Err(format!("Failed to update display name: {e}"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineRenameAction {
    Commit(String),
    Cancel,
    Invalid(String),
}

/// What the inline editor does with `draft`: an empty or unchanged name
/// cancels without an API call.
pub fn inline_rename_action(current: &str, draft: &str) -> InlineRenameAction {
    if draft.trim().is_empty() {
        return InlineRenameAction::Cancel;
    }
    match validate_rename(draft) {
        Ok(name) if name == current => InlineRenameAction::Cancel,
        Ok(name) => InlineRenameAction::Commit(name),
        Err(msg) => InlineRenameAction::Invalid(msg),
    }
}

fn mounted_html(evt: &MountedEvent) -> Option<web_sys::HtmlElement> {
    evt.data()
        .downcast::<web_sys::Element>()
        .and_then(|e| e.clone().dyn_into::<web_sys::HtmlElement>().ok())
}

fn is_focused(el: &web_sys::HtmlElement) -> bool {
    gloo_utils::document()
        .active_element()
        .is_some_and(|a| a == **el)
}

/// Safari sends the IME-confirming Enter with `isComposing` false but keyCode 229.
fn is_ime_keydown(evt: &KeyboardEvent) -> bool {
    evt.is_composing()
        || evt
            .downcast::<web_sys::KeyboardEvent>()
            .is_some_and(|k| k.key_code() == 229)
}

fn focus_and_verify(el: &web_sys::HtmlElement) {
    let _ = el.focus();
    if !is_focused(el) {
        log::warn!("RENAME: focus did not land on the self-tile name control");
    }
}

/// The self tile's name chip: a button that swaps to an inline editor.
#[component]
pub fn SelfTileName(
    display_name: String,
    meeting_id: String,
    session_id: Option<u64>,
    on_renamed: EventHandler<String>,
) -> Element {
    let mut editing = use_signal(|| false);
    let mut draft = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let mut focus_button_on_mount = use_signal(|| false);
    let mut input_el = use_signal(|| None::<web_sys::HtmlElement>);
    let mut announcement = use_signal(String::new);

    let mut finish = move |return_focus: bool| {
        focus_button_on_mount.set(return_focus);
        error.set(None);
        editing.set(false);
    };

    let commit = {
        let current = display_name.clone();
        let meeting_id = meeting_id.clone();
        move |from_blur: bool| {
            if !*editing.peek() || *busy.peek() {
                return;
            }
            match inline_rename_action(&current, &draft.peek()) {
                InlineRenameAction::Cancel => finish(!from_blur),
                InlineRenameAction::Invalid(msg) => error.set(Some(msg)),
                InlineRenameAction::Commit(name) => {
                    busy.set(true);
                    error.set(None);
                    let meeting_id = meeting_id.clone();
                    let persisted_name = name.clone();
                    let (tx, rx) = futures::channel::oneshot::channel();
                    // Detached so the rename and storage save finish even if
                    // the tile unmounts; the UI half below is scope-owned.
                    wasm_bindgen_futures::spawn_local(async move {
                        let result =
                            persist_display_name(&meeting_id, &persisted_name, session_id).await;
                        let _ = tx.send(result);
                    });
                    spawn(async move {
                        let Ok(result) = rx.await else {
                            return;
                        };
                        busy.set(false);
                        match result {
                            Ok(()) => {
                                let had_focus = input_el.peek().as_ref().is_some_and(is_focused);
                                focus_button_on_mount.set(had_focus);
                                editing.set(false);
                                announcement.set(RENAME_SUCCESS_ANNOUNCEMENT.to_string());
                                on_renamed.call(name);
                            }
                            Err(msg) => error.set(Some(msg)),
                        }
                    });
                }
            }
        }
    };
    let mut commit_on_enter = commit.clone();
    let mut commit_on_blur = commit;

    let has_error = error().is_some();
    rsx! {
        h4 { class: "floating-name self-tile-name", dir: "auto",
            if editing() {
                input {
                    r#type: "text",
                    class: "self-tile-name-input",
                    dir: "auto",
                    "data-testid": "self-tile-name-input",
                    "aria-label": "Display name",
                    "aria-invalid": if has_error { "true" } else { "false" },
                    "aria-describedby": has_error.then_some(SELF_TILE_NAME_ERROR_ID),
                    "aria-busy": if busy() { "true" } else { "false" },
                    maxlength: DISPLAY_NAME_MAX_LEN as i64,
                    autocomplete: "off",
                    spellcheck: "false",
                    value: "{draft}",
                    readonly: busy(),
                    onmounted: move |evt: MountedEvent| {
                        let el = mounted_html(&evt);
                        if let Some(el) = &el {
                            focus_and_verify(el);
                            if let Some(input) = el.dyn_ref::<web_sys::HtmlInputElement>() {
                                input.select();
                            }
                        }
                        input_el.set(el);
                    },
                    onclick: move |evt: MouseEvent| evt.stop_propagation(),
                    oninput: move |evt: FormEvent| {
                        if !*busy.peek() {
                            draft.set(evt.value());
                            error.set(None);
                        }
                    },
                    onkeydown: move |evt: KeyboardEvent| {
                        // Keep keys away from the meeting container's shortcuts.
                        evt.stop_propagation();
                        if is_ime_keydown(&evt) {
                            return;
                        }
                        match evt.key() {
                            Key::Enter => {
                                evt.prevent_default();
                                commit_on_enter(false);
                            }
                            Key::Escape => {
                                evt.prevent_default();
                                if !*busy.peek() {
                                    finish(true);
                                }
                            }
                            _ => {}
                        }
                    },
                    onblur: move |_| commit_on_blur(true),
                }
            } else {
                button {
                    r#type: "button",
                    class: "self-tile-name-button",
                    "data-testid": "self-tile-name-button",
                    "aria-label": "Edit display name, {display_name}",
                    title: "Edit display name",
                    onmounted: move |evt: MountedEvent| {
                        if *focus_button_on_mount.peek() {
                            focus_button_on_mount.set(false);
                            if let Some(el) = mounted_html(&evt) {
                                focus_and_verify(&el);
                            }
                        }
                    },
                    onclick: {
                        let current = display_name.clone();
                        move |evt: MouseEvent| {
                            // Tile-overlay control, not a grid click — issue 1790.
                            evt.stop_propagation();
                            draft.set(current.clone());
                            error.set(None);
                            announcement.set(String::new());
                            editing.set(true);
                        }
                    },
                    span { class: "floating-name-text", "{display_name}" }
                    svg {
                        class: "self-tile-name-edit-icon",
                        "aria-hidden": "true",
                        view_box: "0 0 24 24",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "2",
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        path { d: "M12 20h9" }
                        path { d: "M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z" }
                    }
                }
            }
            span { class: "self-indicator", "You" }
        }
        if let Some(msg) = error() {
            div {
                id: SELF_TILE_NAME_ERROR_ID,
                class: "self-tile-name-error",
                "data-testid": "self-tile-name-error",
                role: "alert",
                "{msg}"
            }
        }
        div {
            class: "visually-hidden",
            "data-testid": "self-tile-name-status",
            role: "status",
            "aria-live": "polite",
            "{announcement}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_name_cancels() {
        assert_eq!(
            inline_rename_action("Alice", "Alice"),
            InlineRenameAction::Cancel
        );
        assert_eq!(
            inline_rename_action("Alice", "  Alice  "),
            InlineRenameAction::Cancel
        );
    }

    #[test]
    fn empty_or_blank_draft_cancels() {
        assert_eq!(
            inline_rename_action("Alice", ""),
            InlineRenameAction::Cancel
        );
        assert_eq!(
            inline_rename_action("Alice", "   "),
            InlineRenameAction::Cancel
        );
    }

    #[test]
    fn changed_valid_name_commits_normalized() {
        assert_eq!(
            inline_rename_action("Alice", "  Bob   Smith "),
            InlineRenameAction::Commit("Bob Smith".to_string())
        );
    }

    #[test]
    fn invalid_name_reports_validation_error() {
        assert_eq!(
            inline_rename_action("Alice", "Bob<script>"),
            InlineRenameAction::Invalid(validate_display_name("Bob<script>").unwrap_err())
        );
        let too_long = "a".repeat(DISPLAY_NAME_MAX_LEN + 1);
        assert!(matches!(
            inline_rename_action("Alice", &too_long),
            InlineRenameAction::Invalid(_)
        ));
    }

    #[test]
    fn validate_rename_rejects_blank_with_modal_message() {
        assert_eq!(
            validate_rename("  "),
            Err("Display name cannot be empty.".to_string())
        );
        assert_eq!(validate_rename(" Bob "), Ok("Bob".to_string()));
    }
}

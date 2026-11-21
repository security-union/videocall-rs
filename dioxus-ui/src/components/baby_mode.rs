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

//! Baby mode: hides the meeting controls and blocks taps until an adult
//! unlocks it.
//!
//! While baby mode is on, [`BabyModeOverlay`] renders a transparent full-screen
//! shield that swallows every pointer/touch/click event, so video keeps playing
//! but nothing underneath can be tapped. The only visible element is a small,
//! dim lock icon. Press-and-hold on it (pointer, or Space/Enter when focused)
//! for [`HOLD_TO_UNLOCK_MS`] opens a plain dialog that asks for a freshly
//! generated random code. Typing the code turns baby mode off.
//!
//! Baby mode never touches mic/camera state; it only changes what is rendered.

use std::rc::Rc;

use dioxus::prelude::*;
use dioxus_core::Task;
use futures::StreamExt;
use gloo_timers::future::TimeoutFuture;
use wasm_bindgen::{closure::Closure, JsCast};

/// How long the lock icon must be held before the unlock dialog opens.
pub const HOLD_TO_UNLOCK_MS: u32 = 3_000;

/// The unlock dialog closes itself after this long without input. The
/// countdown starts when the code input gets focus.
pub const UNLOCK_DIALOG_IDLE_MS: u32 = 15_000;

/// Number of characters in an unlock code.
pub const UNLOCK_CODE_LEN: usize = 4;

/// Characters used for unlock codes. Leaves out the easily confused
/// `0`/`O` and `1`/`I`/`L`.
pub const UNLOCK_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

const STORAGE_KEY: &str = "vc_baby_mode";

const LOCK_ID: &str = "baby-mode-lock";
const CODE_INPUT_ID: &str = "baby-mode-code-input";

const ENTRY_HINT_MS: u32 = 4_000;
const TAP_HINT: &str = "Hold to unlock";
const TAP_HINT_MS: u32 = 2_000;

// =============================================================================
// Pure helpers (unit tested)
// =============================================================================

/// Build an unlock code from a source of uniform random numbers in `[0, 1)`.
pub fn generate_unlock_code_with(mut random: impl FnMut() -> f64) -> String {
    let len = UNLOCK_CODE_ALPHABET.len();
    (0..UNLOCK_CODE_LEN)
        .map(|_| {
            // Clamp guards against a source that returns exactly 1.0.
            let index = ((random() * len as f64) as usize).min(len - 1);
            UNLOCK_CODE_ALPHABET[index] as char
        })
        .collect()
}

/// Build an unlock code using the browser's `Math.random()`.
pub fn generate_unlock_code() -> String {
    generate_unlock_code_with(js_sys::Math::random)
}

/// Whether the typed text matches the code, ignoring case and surrounding
/// whitespace.
pub fn unlock_code_matches(input: &str, code: &str) -> bool {
    input.trim().eq_ignore_ascii_case(code.trim())
}

/// Hold duration in whole seconds, for user-facing text.
fn hold_seconds() -> u32 {
    HOLD_TO_UNLOCK_MS.div_ceil(1_000)
}

/// Hint shown when baby mode turns on or is restored after a reload.
pub fn entry_hint() -> String {
    format!(
        "Controls locked. Hold the lock for {} seconds to unlock.",
        hold_seconds()
    )
}

/// Accessible name of the lock button.
pub fn lock_aria_label() -> String {
    format!(
        "Controls locked. Press and hold for {} seconds to unlock.",
        hold_seconds()
    )
}

/// "K7MP" -> "K 7 M P", so screen readers spell the code out.
pub fn spell_out(code: &str) -> String {
    code.chars().map(String::from).collect::<Vec<_>>().join(" ")
}

// =============================================================================
// Persistence (this tab only)
// =============================================================================

fn session_storage() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|w| w.session_storage().ok().flatten())
}

/// Read the persisted baby mode flag for this tab. Any storage failure reads
/// as "off".
pub fn load_baby_mode() -> bool {
    session_storage()
        .and_then(|s| s.get_item(STORAGE_KEY).ok().flatten())
        .is_some_and(|v| v == "1")
}

/// Persist the baby mode flag for this tab. Storage failures are logged and
/// ignored.
pub fn save_baby_mode(enabled: bool) {
    let Some(storage) = session_storage() else {
        return;
    };
    let result = if enabled {
        storage.set_item(STORAGE_KEY, "1")
    } else {
        storage.remove_item(STORAGE_KEY)
    };
    if let Err(e) = result {
        log::warn!("Could not persist baby mode: {e:?}");
    }
}

// =============================================================================
// DOM helpers
// =============================================================================

/// Focus an element by id. Called synchronously inside pointer/touch handlers
/// so iOS treats it as user-initiated and shows the on-screen keyboard.
fn focus_by_id(id: &str) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = el.focus();
    }
}

/// A `document` event listener that is removed when dropped.
struct DocumentListener {
    document: web_sys::Document,
    event: &'static str,
    closure: Closure<dyn FnMut()>,
}

impl DocumentListener {
    fn new(event: &'static str, callback: impl FnMut() + 'static) -> Option<Self> {
        let document = web_sys::window()?.document()?;
        let closure = Closure::<dyn FnMut()>::new(callback);
        document
            .add_event_listener_with_callback(event, closure.as_ref().unchecked_ref())
            .ok()?;
        Some(Self {
            document,
            event,
            closure,
        })
    }
}

impl Drop for DocumentListener {
    fn drop(&mut self) {
        let _ = self
            .document
            .remove_event_listener_with_callback(self.event, self.closure.as_ref().unchecked_ref());
    }
}

// =============================================================================
// Overlay component
// =============================================================================

/// Shield, lock icon, hint and unlock dialog. Render only while baby mode is
/// on; every timer is a task owned by this component, so unmounting it
/// cancels them.
#[component]
pub fn BabyModeOverlay(on_unlock: EventHandler<()>) -> Element {
    let mut holding = use_signal(|| false);
    let mut hold_task = use_signal(|| None::<Task>);
    let mut dialog_open = use_signal(|| false);
    let mut code = use_signal(String::new);
    let mut input = use_signal(String::new);
    let mut wrong_code = use_signal(|| false);
    let mut idle_task = use_signal(|| None::<Task>);
    let mut hint_text = use_signal(entry_hint);
    // Starts visible: the entry hint shows when baby mode turns on and when
    // it is restored after a reload.
    let mut hint_visible = use_signal(|| true);
    let mut hint_task = use_signal(|| None::<Task>);

    let mut show_hint = move |text: &str, ms: u32| {
        if let Some(task) = hint_task.take() {
            task.cancel();
        }
        hint_text.set(text.to_string());
        hint_visible.set(true);
        let task = spawn(async move {
            TimeoutFuture::new(ms).await;
            hint_task.set(None);
            hint_visible.set(false);
        });
        hint_task.set(Some(task));
    };

    // Hide the entry hint after a while. Only `hint_task` (never read during
    // render) is written here, so this doesn't trigger a re-render.
    use_hook(move || {
        let task = spawn(async move {
            TimeoutFuture::new(ENTRY_HINT_MS).await;
            hint_task.set(None);
            hint_visible.set(false);
        });
        hint_task.set(Some(task));
    });

    let mut close_dialog = move || {
        if let Some(task) = idle_task.take() {
            task.cancel();
        }
        dialog_open.set(false);
        input.set(String::new());
        wrong_code.set(false);
        focus_by_id(LOCK_ID);
    };

    // Restart the auto-close countdown for the unlock dialog.
    let mut restart_idle_timer = move || {
        if let Some(task) = idle_task.take() {
            task.cancel();
        }
        let task = spawn(async move {
            TimeoutFuture::new(UNLOCK_DIALOG_IDLE_MS).await;
            idle_task.set(None);
            close_dialog();
        });
        idle_task.set(Some(task));
    };

    let mut open_dialog = move || {
        if let Some(task) = hint_task.take() {
            task.cancel();
        }
        hint_visible.set(false);
        code.set(generate_unlock_code());
        input.set(String::new());
        wrong_code.set(false);
        dialog_open.set(true);
        // Also restarted on input focus and typing; starting here means the
        // dialog can't stay open forever if focus never lands on the input.
        restart_idle_timer();
    };

    let mut begin_hold = move || {
        if dialog_open() {
            return;
        }
        if let Some(task) = hold_task.take() {
            task.cancel();
        }
        holding.set(true);
        let task = spawn(async move {
            TimeoutFuture::new(HOLD_TO_UNLOCK_MS).await;
            hold_task.set(None);
            holding.set(false);
            open_dialog();
        });
        hold_task.set(Some(task));
    };

    // Stop an in-progress hold. `tap_hint` shows "Hold to unlock" when the
    // press ended early (a tap or a short key press).
    let mut end_hold = move |tap_hint: bool| {
        if let Some(task) = hold_task.take() {
            task.cancel();
            holding.set(false);
            if tap_hint {
                show_hint(TAP_HINT, TAP_HINT_MS);
            }
        }
    };

    // Switching tabs/apps mid-hold cancels it. The JS listener only forwards
    // into a task so task cancellation runs inside the Dioxus runtime.
    let _visibility_listener = use_hook(move || {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        spawn(async move {
            while rx.next().await.is_some() {
                end_hold(false);
            }
        });
        Rc::new(DocumentListener::new("visibilitychange", move || {
            let _ = tx.unbounded_send(());
        }))
    });

    let start_pointer_hold = move |evt: PointerEvent| {
        // Only primary-button presses count: left mouse, touch contact and
        // pen tip. Other buttons (right/middle mouse, pen eraser) are ignored.
        if evt
            .trigger_button()
            .is_some_and(|b| b != dioxus::html::input_data::MouseButton::Primary)
        {
            return;
        }
        // Stops text selection, focus changes and long-press menus.
        evt.prevent_default();
        evt.stop_propagation();
        if let Some(raw) = evt.data().downcast::<web_sys::PointerEvent>() {
            // Touch input gets implicit pointer capture, which would stop
            // `pointerleave` from firing when the finger slides off. Release
            // it so "moving off cancels" works on touch screens too.
            let pointer_id = raw.pointer_id();
            if let Some(el) = raw
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            {
                if el.has_pointer_capture(pointer_id) {
                    let _ = el.release_pointer_capture(pointer_id);
                }
            }
        }
        begin_hold();
    };

    let mut submit = move || {
        if unlock_code_matches(&input(), &code()) {
            if let Some(task) = idle_task.take() {
                task.cancel();
            }
            on_unlock.call(());
        } else {
            code.set(generate_unlock_code());
            input.set(String::new());
            wrong_code.set(true);
            restart_idle_timer();
        }
    };

    let lock_class = if holding() {
        "baby-mode-lock holding"
    } else {
        "baby-mode-lock"
    };
    let hint_class = if hint_visible() {
        "baby-mode-hint visible"
    } else {
        "baby-mode-hint"
    };
    let ring_style = format!("animation-duration: {HOLD_TO_UNLOCK_MS}ms;");

    rsx! {
        // Transparent shield: sits above the whole meeting and eats input.
        div {
            id: "baby-mode-shield",
            class: "baby-mode-shield",
            "data-testid": "baby-mode-shield",
            "aria-hidden": "true",
            onclick: move |e| {
                e.prevent_default();
                e.stop_propagation();
            },
            ondoubleclick: move |e| {
                e.prevent_default();
                e.stop_propagation();
            },
            oncontextmenu: move |e| e.prevent_default(),
            onpointerdown: move |e| {
                e.prevent_default();
                e.stop_propagation();
            },
            ontouchstart: move |e| e.stop_propagation(),
        }

        if dialog_open() {
            // Dims the meeting behind the dialog. A tap here focuses the
            // input (iOS keyboard) instead of closing anything.
            div {
                class: "baby-mode-scrim",
                "aria-hidden": "true",
                onpointerdown: move |e| e.prevent_default(),
                onpointerup: move |_| focus_by_id(CODE_INPUT_ID),
                ontouchend: move |_| focus_by_id(CODE_INPUT_ID),
            }
        }

        button {
            id: LOCK_ID,
            class: lock_class,
            "data-testid": "baby-mode-lock",
            r#type: "button",
            "aria-label": lock_aria_label(),
            onpointerdown: start_pointer_hold,
            onpointerup: move |_| {
                end_hold(true);
                // The dialog opened while the finger was still down; focusing
                // here, inside the gesture, brings up the iOS keyboard.
                if dialog_open() {
                    focus_by_id(CODE_INPUT_ID);
                }
            },
            ontouchend: move |_| {
                if dialog_open() {
                    focus_by_id(CODE_INPUT_ID);
                }
            },
            onpointerleave: move |_| end_hold(false),
            onpointercancel: move |_| end_hold(false),
            onblur: move |_| end_hold(false),
            onkeydown: move |e: KeyboardEvent| {
                if matches!(e.key(), Key::Enter) || e.key() == Key::Character(" ".into()) {
                    // Also stops the browser from synthesizing a click.
                    e.prevent_default();
                    if !e.is_auto_repeating() {
                        begin_hold();
                    }
                }
            },
            onkeyup: move |e: KeyboardEvent| {
                if matches!(e.key(), Key::Enter) || e.key() == Key::Character(" ".into()) {
                    e.prevent_default();
                    end_hold(true);
                }
            },
            oncontextmenu: move |e| e.prevent_default(),
            // A plain tap does nothing (the hint comes from pointerup).
            onclick: move |e| {
                e.prevent_default();
                e.stop_propagation();
            },
            svg {
                class: "baby-mode-lock-progress",
                view_box: "0 0 36 36",
                "aria-hidden": "true",
                circle {
                    cx: "18",
                    cy: "18",
                    r: "16",
                    fill: "none",
                    stroke_width: "2",
                    style: "{ring_style}",
                }
            }
            svg {
                class: "baby-mode-lock-icon",
                xmlns: "http://www.w3.org/2000/svg",
                view_box: "0 0 24 24",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "2",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                "aria-hidden": "true",
                rect { x: "3", y: "11", width: "18", height: "11", rx: "2", ry: "2" }
                path { d: "M7 11V7a5 5 0 0 1 10 0v4" }
            }
        }

        p {
            class: hint_class,
            "data-testid": "baby-mode-hint",
            role: "status",
            "aria-live": "polite",
            if hint_visible() {
                "{hint_text}"
            }
        }

        if dialog_open() {
            div {
                id: "baby-mode-unlock-dialog",
                class: "baby-mode-dialog",
                "data-testid": "baby-mode-unlock-dialog",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "baby-mode-unlock-title",
                "aria-describedby": "baby-mode-unlock-desc",
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        e.stop_propagation();
                        close_dialog();
                    }
                },
                form {
                    onsubmit: move |e: FormEvent| {
                        e.prevent_default();
                        submit();
                    },
                    h2 { id: "baby-mode-unlock-title", class: "baby-mode-dialog-title",
                        "Unlock controls"
                    }
                    p { id: "baby-mode-unlock-desc", class: "baby-mode-dialog-body",
                        "Type this code to bring the controls back."
                    }
                    div {
                        id: "baby-mode-code",
                        class: "baby-mode-code",
                        "data-testid": "baby-mode-code",
                        "aria-label": spell_out(&code()),
                        "{code}"
                    }
                    input {
                        id: CODE_INPUT_ID,
                        class: "baby-mode-code-input",
                        "data-testid": "baby-mode-code-input",
                        r#type: "text",
                        "inputmode": "text",
                        autocomplete: "off",
                        "autocorrect": "off",
                        autocapitalize: "characters",
                        spellcheck: "false",
                        "enterkeyhint": "done",
                        maxlength: "16",
                        placeholder: "Tap here to type the code",
                        "aria-label": "Unlock code",
                        value: "{input}",
                        // The idle countdown starts once someone is actually
                        // at the input (on iOS that is after a tap).
                        onfocus: move |_| restart_idle_timer(),
                        oninput: move |e: FormEvent| {
                            input.set(e.value());
                            if wrong_code() {
                                wrong_code.set(false);
                            }
                            restart_idle_timer();
                        },
                        onkeydown: move |e: KeyboardEvent| {
                            // A Space/Enter held on the lock keeps auto-repeating
                            // after focus moves here; don't type or submit it.
                            let repeat_key = matches!(e.key(), Key::Enter)
                                || e.key() == Key::Character(" ".into());
                            if repeat_key && e.is_auto_repeating() {
                                e.prevent_default();
                            }
                        },
                        onmounted: move |evt| {
                            use dioxus::web::WebEventExt;
                            if let Some(el) = evt.try_as_web_event() {
                                if let Ok(html) = el.dyn_into::<web_sys::HtmlElement>() {
                                    let _ = html.focus();
                                }
                            }
                        },
                    }
                    p {
                        class: "baby-mode-feedback",
                        "data-testid": "baby-mode-feedback",
                        role: "status",
                        "aria-live": "polite",
                        if wrong_code() {
                            "That didn't match. Here's a new code."
                        }
                    }
                    div { class: "baby-mode-dialog-actions",
                        button {
                            r#type: "button",
                            class: "btn-apple btn-secondary",
                            "data-testid": "baby-mode-unlock-cancel",
                            onclick: move |_| close_dialog(),
                            "Cancel"
                        }
                        button {
                            r#type: "submit",
                            class: "btn-apple btn-primary",
                            "data-testid": "baby-mode-unlock-submit",
                            "Unlock"
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alphabet_has_no_ambiguous_characters() {
        for c in *b"0O1IL" {
            assert!(
                !UNLOCK_CODE_ALPHABET.contains(&c),
                "alphabet must not contain {}",
                c as char
            );
        }
    }

    #[test]
    fn code_has_expected_length_and_alphabet() {
        let mut seed = 0.0_f64;
        for _ in 0..200 {
            let code = generate_unlock_code_with(|| {
                // Cheap deterministic sweep over [0, 1).
                seed = (seed + 0.137) % 1.0;
                seed
            });
            assert_eq!(code.len(), UNLOCK_CODE_LEN);
            assert!(code.bytes().all(|b| UNLOCK_CODE_ALPHABET.contains(&b)));
        }
    }

    #[test]
    fn code_generation_clamps_edges() {
        let low = generate_unlock_code_with(|| 0.0);
        let high = generate_unlock_code_with(|| 1.0);
        let first = UNLOCK_CODE_ALPHABET[0] as char;
        let last = *UNLOCK_CODE_ALPHABET.last().unwrap() as char;
        assert_eq!(low, first.to_string().repeat(UNLOCK_CODE_LEN));
        assert_eq!(high, last.to_string().repeat(UNLOCK_CODE_LEN));
    }

    #[test]
    fn match_is_case_insensitive_and_trims() {
        assert!(unlock_code_matches("K7MP", "K7MP"));
        assert!(unlock_code_matches("k7mp", "K7MP"));
        assert!(unlock_code_matches("  k7Mp \n", "K7MP"));
    }

    #[test]
    fn match_rejects_wrong_or_partial_input() {
        assert!(!unlock_code_matches("", "K7MP"));
        assert!(!unlock_code_matches("K7M", "K7MP"));
        assert!(!unlock_code_matches("K7MPX", "K7MP"));
        assert!(!unlock_code_matches("K 7MP", "K7MP"));
    }

    #[test]
    fn hint_text_tracks_hold_duration() {
        assert_eq!(
            entry_hint(),
            "Controls locked. Hold the lock for 3 seconds to unlock."
        );
        assert!(lock_aria_label().contains("3 seconds"));
    }

    #[test]
    fn spell_out_separates_characters() {
        assert_eq!(spell_out("K7MP"), "K 7 M P");
        assert_eq!(spell_out(""), "");
    }
}

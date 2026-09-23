// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-call meeting footer line and its "Meeting info" dialog (issue 2791).

use crate::components::attendants::focus_trigger_or_grid;
use crate::components::call_timer::CallTimer;
use crate::components::meeting_format::format_datetime_zoned;
use crate::constants::{build_date_local, build_datetime_local, short_sha};
use crate::context::MeetingTimeCtx;
use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

const MEETING_FOOTER_TRIGGER_ID: &str = "meeting-footer-trigger";
const MEETING_INFO_DIALOG_ID: &str = "meeting-info-dialog";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const COPY_RESET_MS: u32 = 1_600;

fn participants_label(n: usize) -> String {
    if n == 1 {
        "1 participant".to_string()
    } else {
        format!("{n} participants")
    }
}

fn trigger_label(meeting_id: &str) -> String {
    format!("Meeting info, meeting ID {meeting_id}, videocall-ui version {APP_VERSION}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyState {
    Idle,
    Copied,
    Failed,
}

impl CopyState {
    fn button_label(self) -> &'static str {
        match self {
            CopyState::Idle => "Copy link",
            CopyState::Copied => "Copied",
            CopyState::Failed => "Copy failed",
        }
    }

    fn announcement(self) -> Option<&'static str> {
        match self {
            CopyState::Idle => None,
            CopyState::Copied => Some("Meeting link copied"),
            CopyState::Failed => Some("Couldn't copy \u{2014} select the link text"),
        }
    }
}

fn clipboard() -> Option<web_sys::Clipboard> {
    let navigator = web_sys::window()?.navigator();
    let value = js_sys::Reflect::get(&navigator, &"clipboard".into()).ok()?;
    (!value.is_undefined() && !value.is_null()).then(|| value.unchecked_into())
}

fn same_node(a: &web_sys::Node, b: &web_sys::Node) -> bool {
    a.is_same_node(Some(b))
}

fn node_contains(parent: &web_sys::Node, child: &web_sys::Node) -> bool {
    parent.contains(Some(child))
}

/// Wraps Tab / Shift+Tab between the first and last enabled button inside the
/// element with id `dialog_id`. Returns `true` when it moved focus, so the
/// caller prevents the default.
pub(crate) fn trap_tab_in_dialog(dialog_id: &str, shift: bool) -> bool {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return false;
    };
    let Some(dialog) = doc.get_element_by_id(dialog_id) else {
        return false;
    };
    let Ok(buttons) = dialog.query_selector_all("button:not([disabled])") else {
        return false;
    };
    let button_at = |i: u32| {
        buttons
            .item(i)
            .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
    };
    let (Some(first), Some(last)) = (
        button_at(0),
        buttons.length().checked_sub(1).and_then(button_at),
    ) else {
        return false;
    };
    let Some(active) = doc.active_element() else {
        return false;
    };
    let inside = !same_node(&active, &dialog) && node_contains(&dialog, &active);
    let target = if shift && (same_node(&active, &first) || !inside) {
        last
    } else if !shift && (same_node(&active, &last) || !inside) {
        first
    } else {
        return false;
    };
    let _ = target.focus();
    true
}

#[component]
pub fn MeetingFooter(
    mut open: Signal<bool>,
    meeting_id: String,
    participant_count: usize,
    is_active: bool,
) -> Element {
    let meeting_time: MeetingTimeCtx = use_context();
    let built: Option<(String, String)> = use_hook(|| {
        let ts = env!("BUILD_TIMESTAMP");
        build_date_local(ts).map(|short| {
            let full = build_datetime_local(ts).unwrap_or_else(|| short.clone());
            (short, full)
        })
    });
    let meeting_start = meeting_time().meeting_start_time;

    rsx! {
        footer { class: "meeting-footer", "data-testid": "meeting-footer",
            button {
                r#type: "button",
                id: MEETING_FOOTER_TRIGGER_ID,
                class: "meeting-footer-trigger",
                "data-testid": "meeting-footer-trigger",
                "aria-haspopup": "dialog",
                "aria-label": "{trigger_label(&meeting_id)}",
                onclick: move |_| open.set(true),
                span { class: "meeting-footer-content", "aria-hidden": "true",
                    span { class: "meeting-footer-group meeting-footer-group--meeting",
                        if is_active {
                            span { class: "meeting-footer-live-dot" }
                            span { class: "meeting-footer-status-label", "Live" }
                            span {
                                class: "meeting-footer-timer",
                                "data-testid": "meeting-footer-timer",
                                CallTimer { start_time_ms: meeting_start }
                            }
                        } else {
                            span {
                                class: "meeting-footer-ended",
                                "data-testid": "meeting-footer-ended",
                                "Ended"
                            }
                        }
                        span { class: "meeting-footer-sep", "\u{00b7}" }
                        span {
                            class: "meeting-footer-room",
                            "data-testid": "meeting-footer-meeting-id",
                            span { class: "meeting-footer-room-label", "Meeting ID " }
                            "{meeting_id}"
                        }
                        span { class: "meeting-footer-sep meeting-footer-sep--count", "\u{00b7}" }
                        span {
                            class: "meeting-footer-count",
                            "data-testid": "meeting-footer-participants",
                            span { class: "meeting-footer-count-compact",
                                svg {
                                    xmlns: "http://www.w3.org/2000/svg",
                                    view_box: "0 0 24 24",
                                    fill: "none",
                                    stroke: "currentColor",
                                    stroke_width: "2",
                                    stroke_linecap: "round",
                                    stroke_linejoin: "round",
                                    path { d: "M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2" }
                                    circle { cx: "12", cy: "7", r: "4" }
                                }
                                "{participant_count}"
                            }
                            span { class: "meeting-footer-count-text",
                                "{participants_label(participant_count)}"
                            }
                        }
                    }
                    span { class: "meeting-footer-group meeting-footer-group--app",
                        span { class: "meeting-footer-app-name", "videocall-ui\u{00a0}" }
                        span {
                            class: "meeting-footer-version",
                            "data-testid": "meeting-footer-version",
                            "v{APP_VERSION}"
                        }
                        if let Some((short, full)) = built {
                            span { class: "meeting-footer-sep", "\u{00b7}" }
                            span {
                                class: "meeting-footer-built",
                                "data-testid": "meeting-footer-built",
                                span { class: "meeting-footer-built-label", "Built\u{00a0}" }
                                span { class: "meeting-footer-built--short", "{short}" }
                                span { class: "meeting-footer-built--full", "{full}" }
                            }
                        }
                        span { class: "meeting-footer-affordance",
                            svg {
                                xmlns: "http://www.w3.org/2000/svg",
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
            }
        }
    }
}

#[component]
pub fn MeetingInfoDialog(
    open: Signal<bool>,
    meeting_id: String,
    meeting_link: String,
    participant_count: usize,
    is_active: bool,
    show_git: bool,
) -> Element {
    if !open() || !is_active {
        return rsx! {};
    }
    rsx! {
        MeetingInfoCard {
            open,
            meeting_id,
            meeting_link,
            participant_count,
            show_git,
        }
    }
}

#[component]
fn MeetingInfoCard(
    mut open: Signal<bool>,
    meeting_id: String,
    meeting_link: String,
    participant_count: usize,
    show_git: bool,
) -> Element {
    let meeting_time: MeetingTimeCtx = use_context();
    let down_on_backdrop = use_hook(|| Rc::new(Cell::new(false)));
    let mut copy_state = use_signal(|| CopyState::Idle);
    let mut copy_seq = use_signal(|| 0u32);
    let copy_reset: Rc<RefCell<Option<Timeout>>> = use_hook(|| Rc::new(RefCell::new(None)));
    let built = use_hook(|| {
        let ts = env!("BUILD_TIMESTAMP");
        build_datetime_local(ts).unwrap_or_else(|| ts.to_string())
    });

    let meeting_start_memo = use_memo(move || meeting_time().meeting_start_time);
    let started = use_memo(move || {
        meeting_start_memo()
            .map(|ms| format_datetime_zoned(ms as i64))
            .unwrap_or_else(|| "\u{2014}".to_string())
    });

    let time = meeting_time();
    let (meeting_start, call_start) = (time.meeting_start_time, time.call_start_time);

    let mut close = move || {
        open.set(false);
        focus_trigger_or_grid(MEETING_FOOTER_TRIGGER_ID);
    };

    let on_copy = {
        let link = meeting_link.clone();
        move |_: MouseEvent| {
            let link = link.clone();
            let reset = copy_reset.clone();
            spawn(async move {
                let copied = match clipboard() {
                    Some(cb) => JsFuture::from(cb.write_text(&link)).await.is_ok(),
                    None => false,
                };
                copy_state.set(if copied {
                    CopyState::Copied
                } else {
                    CopyState::Failed
                });
                copy_seq += 1;
                *reset.borrow_mut() = Some(Timeout::new(COPY_RESET_MS, move || {
                    copy_state.set(CopyState::Idle)
                }));
            });
        }
    };

    let backdrop_down = down_on_backdrop.clone();
    let card_down = down_on_backdrop.clone();
    let backdrop_click = down_on_backdrop;

    rsx! {
        div {
            class: "glass-backdrop meeting-info-backdrop",
            "data-testid": "meeting-info-dialog-backdrop",
            onmousedown: move |_| backdrop_down.set(true),
            onclick: move |_| {
                if backdrop_click.replace(false) {
                    close();
                }
            },
            div {
                id: MEETING_INFO_DIALOG_ID,
                class: "card-apple meeting-info-card",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "meeting-info-dialog-title",
                tabindex: "-1",
                "data-testid": "meeting-info-dialog",
                onmousedown: move |e| {
                    card_down.set(false);
                    e.stop_propagation();
                },
                onclick: move |e| e.stop_propagation(),
                onkeydown: move |e: Event<KeyboardData>| match e.key() {
                    Key::Escape => {
                        e.stop_propagation();
                        e.prevent_default();
                        close();
                    }
                    Key::Tab if trap_tab_in_dialog(MEETING_INFO_DIALOG_ID, e.modifiers().shift()) => {
                        e.prevent_default();
                    }
                    _ => {}
                },
                onmounted: move |element| {
                    let element = element.data();
                    spawn(async move {
                        let _ = element.set_focus(true).await;
                    });
                },

                div { class: "about-modal-header",
                    h3 { id: "meeting-info-dialog-title", class: "about-modal-title", "Meeting info" }
                    button {
                        r#type: "button",
                        class: "btn-apple btn-secondary btn-sm about-modal-close",
                        "aria-label": "Close meeting info",
                        "data-testid": "meeting-info-dialog-close",
                        onclick: move |_| close(),
                        "Close"
                    }
                }

                section {
                    class: "about-modal-section",
                    "aria-labelledby": "meeting-info-section-meeting",
                    h4 {
                        id: "meeting-info-section-meeting",
                        class: "about-modal-section-title",
                        "Meeting"
                    }
                    div { class: "about-modal-table",
                        div { class: "meeting-info-row", "data-testid": "meeting-info-row-meeting-id",
                            span { class: "about-modal-label", "Meeting ID" }
                            span { class: "about-modal-value meeting-info-value--id", "{meeting_id}" }
                        }
                        div { class: "meeting-info-row", "data-testid": "meeting-info-row-link",
                            span { class: "about-modal-label", "Meeting link" }
                            div { class: "meeting-info-link-cell",
                                span { class: "meeting-info-link", "{meeting_link}" }
                                button {
                                    r#type: "button",
                                    class: "btn-apple btn-secondary btn-sm meeting-info-copy",
                                    "data-testid": "meeting-info-copy-link",
                                    onclick: on_copy,
                                    "{copy_state().button_label()}"
                                }
                            }
                        }
                        div { class: "meeting-info-row", "data-testid": "meeting-info-row-started",
                            span { class: "about-modal-label", "Started" }
                            span { class: "about-modal-value", "{started}" }
                        }
                        div { class: "meeting-info-row", "data-testid": "meeting-info-row-duration",
                            span { class: "about-modal-label", "Duration" }
                            span { class: "about-modal-value meeting-info-value--timer",
                                CallTimer { start_time_ms: meeting_start }
                            }
                        }
                        div { class: "meeting-info-row", "data-testid": "meeting-info-row-your-time",
                            span { class: "about-modal-label", "Your time" }
                            span { class: "about-modal-value meeting-info-value--timer",
                                CallTimer { start_time_ms: call_start }
                            }
                        }
                        div { class: "meeting-info-row", "data-testid": "meeting-info-row-participants",
                            span { class: "about-modal-label", "Participants" }
                            span { class: "about-modal-value", "{participant_count}" }
                        }
                    }
                }

                section {
                    class: "about-modal-section",
                    "aria-labelledby": "meeting-info-section-app",
                    h4 {
                        id: "meeting-info-section-app",
                        class: "about-modal-section-title",
                        "App"
                    }
                    div { class: "about-modal-table",
                        div { class: "meeting-info-row", "data-testid": "meeting-info-row-version",
                            span { class: "about-modal-label", "Version" }
                            span { class: "about-modal-value", "videocall-ui v{APP_VERSION}" }
                        }
                        div { class: "meeting-info-row", "data-testid": "meeting-info-row-built",
                            span { class: "about-modal-label", "Built" }
                            span { class: "about-modal-value", "{built}" }
                        }
                        if show_git {
                            div { class: "meeting-info-row", "data-testid": "meeting-info-row-commit",
                                span { class: "about-modal-label", "Commit" }
                                span { class: "about-modal-value about-modal-value--mono",
                                    "{short_sha(env!(\"GIT_SHA\"))}"
                                }
                            }
                            div { class: "meeting-info-row", "data-testid": "meeting-info-row-branch",
                                span { class: "about-modal-label", "Branch" }
                                span { class: "about-modal-value about-modal-value--mono",
                                    "{env!(\"GIT_BRANCH\")}"
                                }
                            }
                        }
                    }
                }

                span { class: "visually-hidden", role: "status", "aria-live": "polite",
                    if let Some(message) = copy_state().announcement() {
                        span { key: "{copy_seq}", "{message}" }
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
    fn participants_label_is_singular_only_for_one() {
        assert_eq!(participants_label(1), "1 participant");
        assert_eq!(participants_label(5), "5 participants");
        assert_eq!(participants_label(0), "0 participants");
    }

    #[test]
    fn trigger_label_names_the_dialog_the_meeting_and_the_version() {
        let label = trigger_label("standup-42");
        assert!(label.starts_with("Meeting info"), "{label}");
        assert!(label.contains("meeting ID standup-42"), "{label}");
        assert!(label.ends_with(env!("CARGO_PKG_VERSION")), "{label}");
    }

    #[test]
    fn copy_state_drives_the_button_label_and_announcement() {
        assert_eq!(CopyState::Idle.button_label(), "Copy link");
        assert_eq!(CopyState::Copied.button_label(), "Copied");
        assert_eq!(CopyState::Failed.button_label(), "Copy failed");
        assert_eq!(CopyState::Idle.announcement(), None);
        assert_eq!(
            CopyState::Copied.announcement(),
            Some("Meeting link copied")
        );
        assert!(CopyState::Failed
            .announcement()
            .is_some_and(|m| m.contains("select the link text")));
    }
}

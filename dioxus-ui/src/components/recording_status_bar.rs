// SPDX-License-Identifier: MIT OR Apache-2.0

//! The meeting-wide `.meeting-status-bar` recording item and the recorder's
//! local recording toasts.

use crate::components::video_control_buttons::{
    js_state_to_record_button_state, RecordButtonState,
};
use dioxus::prelude::*;
use std::collections::HashSet;
use wasm_bindgen::{JsCast, JsValue};

const GENERIC_DESCRIPTION: &str = "This meeting is being recorded";
const PENDING_CLASS: &str = "meeting-status-item visually-hidden";
const SLOT_CLASS: &str = "meeting-status-rec-slot";
const SELF_SLOT_CLASS: &str = "meeting-status-rec-slot meeting-status-rec-slot--self";
const GENERIC_CLASS: &str = "meeting-status-item meeting-status-item--recording";
const SELF_CLASS: &str =
    "meeting-status-item meeting-status-item--recording meeting-status-item--self";
const SELF_WITH_OTHERS_CLASS: &str = "meeting-status-item meeting-status-item--recording \
     meeting-status-item--self meeting-status-item--with-others";
const VISIBLE_BAR_CLASS: &str = "meeting-status-bar";
const PENDING_BAR_CLASS: &str = "visually-hidden";
const STARTING_LABEL: &str = "Starting recording\u{2026}";
const TAIL_CHARS: usize = 10;
const MIN_HEAD_CHARS: usize = 4;
const MAX_EXT_CHARS: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveMode {
    /// Streamed to a file the user picked.
    File,
    /// Delivered as a browser download when the recording stops.
    Download,
}

/// Where the local recording is being written. Local-only UI state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveTarget {
    pub name: String,
    pub mode: SaveMode,
}

/// Maps the `{name, mode}` fields of `window.__vcRecording.saveTarget()`.
pub fn save_target_from_parts(name: Option<String>, mode: Option<&str>) -> Option<SaveTarget> {
    let mode = match mode? {
        "file" => SaveMode::File,
        "download" => SaveMode::Download,
        _ => return None,
    };
    Some(SaveTarget { name: name?, mode })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedNotice {
    pub file_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecorderUpdate {
    pub save_target: Option<SaveTarget>,
    pub saved_notice: Option<SavedNotice>,
}

/// Local UI effects of one recorder JS state string.
pub fn recorder_update(
    js_state: &str,
    current: Option<&SaveTarget>,
    read_js: impl FnOnce() -> Option<SaveTarget>,
) -> RecorderUpdate {
    let save_target = match js_state_to_record_button_state(js_state) {
        RecordButtonState::Activating => None,
        RecordButtonState::Recording => read_js(),
        RecordButtonState::Idle | RecordButtonState::Stopping | RecordButtonState::Saving => {
            current.cloned()
        }
    };
    let saved_notice = (js_state == "saved").then(|| SavedNotice {
        file_name: known_target(save_target.as_ref()).map(|t| t.name.clone()),
    });
    RecorderUpdate {
        save_target,
        saved_notice,
    }
}

fn is_self_recording(state: &RecordButtonState) -> bool {
    matches!(
        state,
        RecordButtonState::Recording | RecordButtonState::Stopping | RecordButtonState::Saving
    )
}

fn recording_item_visible(any_recording_active: bool, state: &RecordButtonState) -> bool {
    any_recording_active || is_self_recording(state)
}

pub fn recording_status_bar_visible(
    any_recording_active: bool,
    state: &RecordButtonState,
    captions_active: bool,
) -> bool {
    recording_item_visible(any_recording_active, state) || captions_active
}

pub fn recording_status_bar_mounted(visible: bool, state: &RecordButtonState) -> bool {
    visible || matches!(state, RecordButtonState::Activating)
}

pub fn starting_toast_label(state: &RecordButtonState) -> Option<&'static str> {
    matches!(state, RecordButtonState::Activating).then_some(STARTING_LABEL)
}

/// The write `local_recording_key` needs on a recorder transition, if any.
pub fn local_key_write(
    state: &RecordButtonState,
    self_session: &str,
    current: Option<&str>,
) -> Option<Option<String>> {
    let next = match state {
        RecordButtonState::Recording if !self_session.is_empty() => Some(self_session),
        RecordButtonState::Idle => None,
        _ => return None,
    };
    (next != current).then(|| next.map(str::to_string))
}

pub fn other_recorders(recording: &HashSet<String>, own_key: Option<&str>) -> usize {
    recording
        .iter()
        .filter(|id| Some(id.as_str()) != own_key)
        .count()
}

pub fn self_status_label(state: &RecordButtonState, others: usize) -> Option<String> {
    let label = match (state, others) {
        (RecordButtonState::Recording, 0) => "You are recording".to_string(),
        (RecordButtonState::Recording, 1) => "You and 1 other are recording".to_string(),
        (RecordButtonState::Recording, n) => format!("You and {n} others are recording"),
        (RecordButtonState::Stopping, 0) => "Stopping recording\u{2026}".to_string(),
        (RecordButtonState::Stopping, _) => "Stopping your recording\u{2026}".to_string(),
        (RecordButtonState::Saving, 0) => "Saving recording\u{2026}".to_string(),
        (RecordButtonState::Saving, _) => "Saving your recording\u{2026}".to_string(),
        (RecordButtonState::Idle | RecordButtonState::Activating, _) => return None,
    };
    Some(label)
}

/// The visible label for a status bar too narrow for the full sentence.
pub fn self_short_label(state: &RecordButtonState, others: usize) -> Option<String> {
    if others == 0 {
        return None;
    }
    Some(match state {
        RecordButtonState::Recording => format!("You +{others} recording"),
        RecordButtonState::Stopping => "Stopping recording\u{2026}".to_string(),
        RecordButtonState::Saving => "Saving recording\u{2026}".to_string(),
        RecordButtonState::Idle | RecordButtonState::Activating => return None,
    })
}

fn known_target(target: Option<&SaveTarget>) -> Option<&SaveTarget> {
    target.filter(|t| !t.name.trim().is_empty())
}

/// Splits a file name for a middle ellipsis: the tail keeps the last
/// `TAIL_CHARS` chars of the stem plus a short extension.
pub fn split_file_name(name: &str) -> (String, String) {
    let chars: Vec<char> = name.chars().collect();
    let len = chars.len();
    let stem_len = match chars.iter().rposition(|&c| c == '.') {
        Some(dot) if len - dot <= MAX_EXT_CHARS => dot,
        _ => len,
    };
    let split = match stem_len.saturating_sub(TAIL_CHARS) {
        s if s < MIN_HEAD_CHARS => 0,
        s => s,
    };
    (
        chars[..split].iter().collect(),
        chars[split..].iter().collect(),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileLabel {
    pub verb: Option<&'static str>,
    pub head: String,
    pub tail: String,
}

pub fn file_label(state: &RecordButtonState, target: Option<&SaveTarget>) -> Option<FileLabel> {
    let target = known_target(target)?;
    let verb = match (state, target.mode) {
        (RecordButtonState::Recording, SaveMode::File) => Some("Saving to"),
        (RecordButtonState::Recording, SaveMode::Download) => Some("Will download as"),
        (RecordButtonState::Stopping | RecordButtonState::Saving, _) => None,
        (RecordButtonState::Idle | RecordButtonState::Activating, _) => return None,
    };
    let (head, tail) = split_file_name(&target.name);
    Some(FileLabel { verb, head, tail })
}

pub fn file_sr_suffix(target: Option<&SaveTarget>) -> Option<String> {
    let target = known_target(target)?;
    Some(match target.mode {
        SaveMode::File => format!(", saving to {}", target.name),
        SaveMode::Download => format!(", will download as {}", target.name),
    })
}

pub fn self_status_title(
    state: &RecordButtonState,
    label: &str,
    target: Option<&SaveTarget>,
) -> String {
    let Some(target) = known_target(target) else {
        return label.to_string();
    };
    let sentence = label.trim_end_matches('\u{2026}');
    match (state, target.mode) {
        (RecordButtonState::Recording, SaveMode::File) => {
            format!("{sentence}. Saving to {}", target.name)
        }
        (RecordButtonState::Recording, SaveMode::Download) => format!(
            "{sentence}. The file will download as {} when you stop.",
            target.name
        ),
        _ => format!("{sentence}: {}", target.name),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusItemView {
    pub class: &'static str,
    pub label: String,
    pub short_label: Option<String>,
    pub title: Option<String>,
    pub aria_label: Option<&'static str>,
    pub aria_atomic: Option<&'static str>,
    pub file: Option<FileLabel>,
    pub sr_suffix: Option<String>,
}

/// Self vs generic is decided by the LOCAL `record_state` only.
pub fn status_item_view(
    state: &RecordButtonState,
    others: usize,
    target: Option<&SaveTarget>,
) -> StatusItemView {
    match self_status_label(state, others) {
        Some(label) => StatusItemView {
            class: if others == 0 {
                SELF_CLASS
            } else {
                SELF_WITH_OTHERS_CLASS
            },
            title: Some(self_status_title(state, &label, target)),
            short_label: self_short_label(state, others),
            label,
            aria_label: None,
            aria_atomic: Some("false"),
            file: file_label(state, target),
            sr_suffix: file_sr_suffix(target),
        },
        None => StatusItemView {
            class: GENERIC_CLASS,
            label: "Recording".to_string(),
            short_label: None,
            title: Some(GENERIC_DESCRIPTION.to_string()),
            aria_label: Some(GENERIC_DESCRIPTION),
            aria_atomic: None,
            file: None,
            sr_suffix: None,
        },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusBarView {
    pub bar_class: &'static str,
    pub slot_class: &'static str,
    /// Label copy that holds the self slot at least as wide as the label.
    pub sizer: Option<String>,
    pub item: Option<StatusItemView>,
}

pub fn status_bar_view(
    state: &RecordButtonState,
    recording: &HashSet<String>,
    own_key: Option<&str>,
    target: Option<&SaveTarget>,
    captions_active: bool,
) -> StatusBarView {
    let any_recording_active = !recording.is_empty();
    let item = if recording_item_visible(any_recording_active, state) {
        Some(status_item_view(
            state,
            other_recorders(recording, own_key),
            target,
        ))
    } else if matches!(state, RecordButtonState::Activating) {
        Some(StatusItemView {
            class: PENDING_CLASS,
            label: String::new(),
            short_label: None,
            title: None,
            aria_label: None,
            aria_atomic: Some("false"),
            file: None,
            sr_suffix: None,
        })
    } else {
        None
    };
    StatusBarView {
        bar_class: if recording_status_bar_visible(any_recording_active, state, captions_active) {
            VISIBLE_BAR_CLASS
        } else {
            PENDING_BAR_CLASS
        },
        slot_class: if is_self_recording(state) {
            SELF_SLOT_CLASS
        } else {
            SLOT_CLASS
        },
        sizer: item
            .as_ref()
            .filter(|_| is_self_recording(state))
            .map(|i| i.label.clone()),
        item,
    }
}

pub fn read_js_save_target() -> Option<SaveTarget> {
    let window = web_sys::window()?;
    let rec = js_sys::Reflect::get(&window, &JsValue::from_str("__vcRecording")).ok()?;
    let read: js_sys::Function = js_sys::Reflect::get(&rec, &JsValue::from_str("saveTarget"))
        .ok()?
        .dyn_into()
        .ok()?;
    let target = read.call0(&rec).ok()?;
    let field = |key: &str| {
        js_sys::Reflect::get(&target, &JsValue::from_str(key))
            .ok()
            .and_then(|v| v.as_string())
    };
    save_target_from_parts(field("name"), field("mode").as_deref())
}

#[component]
fn FileName(head: String, tail: String) -> Element {
    rsx! {
        span { class: "rec-target-head", "{head}" }
        span { class: "rec-target-tail", "{tail}" }
    }
}

#[component]
pub fn RecordingStatusBar(
    record_state: ReadSignal<RecordButtonState>,
    recording_peer_ids: ReadSignal<HashSet<String>>,
    save_target: ReadSignal<Option<SaveTarget>>,
    local_recording_key: ReadSignal<Option<String>>,
    captions_active: bool,
    children: Element,
) -> Element {
    let view = status_bar_view(
        &record_state.read(),
        &recording_peer_ids.read(),
        local_recording_key.read().as_deref(),
        save_target.read().as_ref(),
        captions_active,
    );
    rsx! {
        div { class: view.bar_class,
            if let Some(item) = view.item {
                div { class: view.slot_class,
                    if let Some(sizer) = view.sizer {
                        span { class: "rec-slot-sizer", "aria-hidden": "true", "{sizer}" }
                    }
                    div { class: "meeting-status-rec-area",
                        div {
                            class: item.class,
                            role: "status",
                            aria_live: "polite",
                            "aria-atomic": item.aria_atomic,
                            aria_label: item.aria_label,
                            title: item.title,
                            span { class: "rec-dot" }
                            span { class: "meeting-status-item-label", "{item.label}" }
                            if let Some(short) = item.short_label {
                                span { class: "rec-label-short", "aria-hidden": "true", "{short}" }
                            }
                            if let Some(file) = item.file {
                                span { class: "rec-sep", "aria-hidden": "true", "\u{b7}" }
                                span { class: "rec-target", "aria-hidden": "true",
                                    if let Some(verb) = file.verb {
                                        span { class: "rec-target-verb", "{verb} " }
                                    }
                                    FileName { head: file.head, tail: file.tail }
                                }
                            }
                            if let Some(suffix) = item.sr_suffix {
                                span { class: "visually-hidden", "{suffix}" }
                            }
                        }
                    }
                }
            }
            {children}
        }
    }
}

#[component]
pub fn RecordingStartingToast(record_state: ReadSignal<RecordButtonState>) -> Element {
    let Some(msg) = starting_toast_label(&record_state.read()) else {
        return rsx! {};
    };
    rsx! {
        div {
            class: "peer-toast recording-status-banner",
            role: "status",
            aria_live: "polite",
            aria_label: msg,
            span { class: "rec-dot" }
            span { class: "toast-text",
                span { class: "toast-name", "{msg}" }
            }
        }
    }
}

#[component]
pub fn RecordingSavedToast(notice: ReadSignal<Option<SavedNotice>>) -> Element {
    let Some(name) = notice.read().as_ref().map(|n| n.file_name.clone()) else {
        return rsx! {};
    };
    let named = name.map(|name| (split_file_name(&name), name));
    rsx! {
        div {
            class: "peer-toast recording-saved-banner",
            role: "status",
            aria_live: "polite",
            span { class: "rec-dot" }
            span { class: "toast-text",
                span { class: "toast-name", "Recording saved" }
                if let Some(((head, tail), name)) = named {
                    span { class: "rec-sep", "aria-hidden": "true", "\u{b7}" }
                    span { class: "rec-target", "aria-hidden": "true",
                        FileName { head, tail }
                    }
                    span { class: "visually-hidden", " as {name}" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::video_control_buttons::RecordButtonState::{
        Activating, Idle, Recording, Saving, Stopping,
    };

    const DEFAULT_NAME: &str = "meeting-recording-2026-10-12T14-03-22-123Z.webm";

    fn target(name: &str, mode: SaveMode) -> SaveTarget {
        SaveTarget {
            name: name.to_string(),
            mode,
        }
    }

    fn ids(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn self_label_covers_every_state_and_pluralises() {
        let cases: &[(RecordButtonState, usize, Option<&str>)] = &[
            (Recording, 0, Some("You are recording")),
            (Recording, 1, Some("You and 1 other are recording")),
            (Recording, 2, Some("You and 2 others are recording")),
            (Recording, 5, Some("You and 5 others are recording")),
            (Stopping, 0, Some("Stopping recording\u{2026}")),
            (Stopping, 1, Some("Stopping your recording\u{2026}")),
            (Stopping, 3, Some("Stopping your recording\u{2026}")),
            (Saving, 0, Some("Saving recording\u{2026}")),
            (Saving, 1, Some("Saving your recording\u{2026}")),
            (Saving, 4, Some("Saving your recording\u{2026}")),
            (Idle, 0, None),
            (Idle, 2, None),
            (Activating, 0, None),
            (Activating, 1, None),
        ];
        for (state, n, want) in cases {
            assert_eq!(
                self_status_label(state, *n).as_deref(),
                *want,
                "{state:?} with {n} others"
            );
        }
    }

    #[test]
    fn short_label_only_exists_when_others_record() {
        let cases: &[(RecordButtonState, usize, Option<&str>)] = &[
            (Recording, 1, Some("You +1 recording")),
            (Recording, 12, Some("You +12 recording")),
            (Stopping, 3, Some("Stopping recording\u{2026}")),
            (Saving, 2, Some("Saving recording\u{2026}")),
            (Recording, 0, None),
            (Stopping, 0, None),
            (Saving, 0, None),
            (Idle, 2, None),
            (Activating, 2, None),
        ];
        for (state, n, want) in cases {
            assert_eq!(
                self_short_label(state, *n).as_deref(),
                *want,
                "{state:?} with {n} others"
            );
            assert_eq!(
                status_item_view(state, *n, None).short_label.as_deref(),
                *want,
                "view: {state:?} with {n} others"
            );
        }
    }

    #[test]
    fn local_idle_or_activating_keeps_the_generic_item() {
        let t = target(DEFAULT_NAME, SaveMode::File);
        for state in [Idle, Activating] {
            let view = status_item_view(&state, 2, Some(&t));
            assert_eq!(view.class, GENERIC_CLASS);
            assert_eq!(view.label, "Recording");
            assert_eq!(
                view.title.as_deref(),
                Some("This meeting is being recorded")
            );
            assert_eq!(view.aria_label, Some("This meeting is being recorded"));
            assert_eq!(view.aria_atomic, None);
            assert_eq!(view.file, None);
            assert_eq!(view.sr_suffix, None);
        }
    }

    #[test]
    fn self_item_has_no_aria_label_and_is_not_atomic() {
        for state in [Recording, Stopping, Saving] {
            let view = status_item_view(&state, 0, None);
            assert_eq!(view.aria_label, None);
            assert_eq!(view.aria_atomic, Some("false"));
        }
    }

    #[test]
    fn bar_is_a_hidden_empty_live_region_until_visible() {
        for captions in [false, true] {
            let view = status_bar_view(&Activating, &ids(&[]), None, None, captions);
            let pending = view.item.expect("pending item");
            assert_eq!(pending.class, "meeting-status-item visually-hidden");
            assert_eq!(pending.label, "");
            assert_eq!(pending.title, None);
            assert_eq!(pending.aria_label, None);
            assert_eq!(view.slot_class, "meeting-status-rec-slot");
            let bar = if captions {
                "meeting-status-bar"
            } else {
                "visually-hidden"
            };
            assert_eq!(view.bar_class, bar);
        }
    }

    #[test]
    fn captions_alone_show_the_bar_without_a_recording_item() {
        for state in [Idle, Activating, Recording] {
            let view = status_bar_view(&state, &ids(&[]), None, None, true);
            assert_eq!(view.bar_class, "meeting-status-bar", "{state:?}");
        }
        assert_eq!(
            status_bar_view(&Idle, &ids(&[]), None, None, true).item,
            None
        );
        assert_eq!(
            status_bar_view(&Idle, &ids(&[]), None, None, false).item,
            None
        );
    }

    #[test]
    fn only_the_self_item_gets_the_sized_slot() {
        for state in [Recording, Stopping, Saving] {
            let view = status_bar_view(&state, &ids(&[]), None, None, true);
            assert_eq!(view.slot_class, SELF_SLOT_CLASS, "{state:?}");
            assert_eq!(view.sizer, view.item.map(|i| i.label), "{state:?}");
        }
        for state in [Idle, Activating] {
            let view = status_bar_view(&state, &ids(&["9"]), None, None, true);
            assert_eq!(view.slot_class, SLOT_CLASS, "{state:?}");
            assert_eq!(view.sizer, None, "{state:?}");
        }
    }

    #[test]
    fn local_key_is_written_at_recording_and_cleared_once_at_idle() {
        let key = |s: &str| Some(Some(s.to_string()));
        assert_eq!(local_key_write(&Recording, "7", None), key("7"));
        assert_eq!(local_key_write(&Recording, "7", Some("6")), key("7"));
        assert_eq!(local_key_write(&Recording, "7", Some("7")), None);
        assert_eq!(local_key_write(&Recording, "", None), None);
        assert_eq!(local_key_write(&Idle, "7", Some("7")), Some(None));
        assert_eq!(local_key_write(&Idle, "7", None), None);
        for state in [Activating, Stopping, Saving] {
            assert_eq!(local_key_write(&state, "7", Some("7")), None, "{state:?}");
            assert_eq!(local_key_write(&state, "7", None), None, "{state:?}");
        }
    }

    #[test]
    fn the_key_written_at_recording_excludes_our_own_entry() {
        let mut key: Option<String> = None;
        for state in [Activating, Recording, Stopping] {
            if let Some(next) = local_key_write(&state, "old", key.as_deref()) {
                key = next;
            }
        }
        let view = status_bar_view(&Stopping, &ids(&["old"]), key.as_deref(), None, false);
        assert_eq!(view.item.unwrap().label, "Stopping recording\u{2026}");
    }

    #[test]
    fn the_self_item_is_marked_when_others_record() {
        for state in [Recording, Stopping, Saving] {
            assert_eq!(status_item_view(&state, 0, None).class, SELF_CLASS);
            let shared = status_item_view(&state, 2, None).class;
            assert!(shared.contains("meeting-status-item--self"), "{state:?}");
            assert!(
                shared.contains("meeting-status-item--with-others"),
                "{state:?}"
            );
        }
    }

    #[test]
    fn file_part_verb_follows_mode_while_recording_only() {
        let file = target(DEFAULT_NAME, SaveMode::File);
        let download = target(DEFAULT_NAME, SaveMode::Download);
        let verb =
            |state: RecordButtonState, t: &SaveTarget| file_label(&state, Some(t)).map(|f| f.verb);
        assert_eq!(verb(Recording, &file), Some(Some("Saving to")));
        assert_eq!(verb(Recording, &download), Some(Some("Will download as")));
        for state in [Stopping, Saving] {
            assert_eq!(verb(state.clone(), &file), Some(None), "{state:?}");
            assert_eq!(verb(state.clone(), &download), Some(None), "{state:?}");
        }
        for state in [Idle, Activating] {
            assert_eq!(verb(state.clone(), &file), None, "{state:?}");
        }
    }

    #[test]
    fn blank_or_unknown_name_has_no_file_part() {
        for name in ["", "   ", "\t"] {
            let t = target(name, SaveMode::File);
            for state in [Recording, Stopping, Saving] {
                let view = status_item_view(&state, 0, Some(&t));
                assert_eq!(view.file, None, "{name:?} {state:?}");
                assert_eq!(view.sr_suffix, None, "{name:?} {state:?}");
                assert_eq!(view.title.as_deref(), Some(view.label.as_str()));
            }
            let saved = recorder_update("saved", Some(&t), || unreachable!());
            assert_eq!(saved.saved_notice, Some(SavedNotice { file_name: None }));
        }
        let view = status_item_view(&Recording, 0, None);
        assert_eq!(view.file, None);
        assert_eq!(view.title.as_deref(), Some("You are recording"));
    }

    #[test]
    fn sr_suffix_is_identical_across_self_states() {
        let cases = [
            (SaveMode::File, format!(", saving to {DEFAULT_NAME}")),
            (
                SaveMode::Download,
                format!(", will download as {DEFAULT_NAME}"),
            ),
        ];
        for (mode, want) in cases {
            let t = target(DEFAULT_NAME, mode);
            for state in [Recording, Stopping, Saving] {
                let view = status_item_view(&state, 1, Some(&t));
                assert_eq!(view.sr_suffix.as_deref(), Some(want.as_str()), "{state:?}");
            }
        }
    }

    #[test]
    fn title_is_a_full_sentence_with_the_name() {
        let file = target(DEFAULT_NAME, SaveMode::File);
        let download = target(DEFAULT_NAME, SaveMode::Download);
        let title = |state: RecordButtonState, others: usize, t: &SaveTarget| {
            status_item_view(&state, others, Some(t)).title.unwrap()
        };
        assert_eq!(
            title(Recording, 0, &file),
            format!("You are recording. Saving to {DEFAULT_NAME}")
        );
        assert_eq!(
            title(Recording, 1, &download),
            format!(
                "You and 1 other are recording. The file will download as {DEFAULT_NAME} when you stop."
            )
        );
        assert_eq!(
            title(Saving, 0, &file),
            format!("Saving recording: {DEFAULT_NAME}")
        );
        assert_eq!(
            title(Stopping, 1, &download),
            format!("Stopping your recording: {DEFAULT_NAME}")
        );
    }

    #[test]
    fn split_keeps_extension_and_ten_stem_chars_in_the_tail() {
        let split = split_file_name;
        let pair = |head: &str, tail: &str| (head.to_string(), tail.to_string());
        assert_eq!(
            split(DEFAULT_NAME),
            pair("meeting-recording-2026-10-12T14-", "03-22-123Z.webm")
        );
        assert_eq!(split("short.webm"), pair("", "short.webm"));
        assert_eq!(split("exactly10c.mp4"), pair("", "exactly10c.mp4"));
        assert_eq!(
            split("my-long-recording-name"),
            pair("my-long-reco", "rding-name")
        );
        assert_eq!(split("thirteen-char"), pair("", "thirteen-char"));
        assert_eq!(split("fourteen-chars"), pair("four", "teen-chars"));
        assert_eq!(split("noext"), pair("", "noext"));
        assert_eq!(split(".webm"), pair("", ".webm"));
        assert_eq!(split("clip.extension1"), pair("clip.", "extension1"));
        assert_eq!(
            split("a-long-meeting-name.abcde"),
            pair("a-long-me", "eting-name.abcde")
        );
        assert_eq!(
            split("a-long-meeting-name.abcdef"),
            pair("a-long-meeting-n", "ame.abcdef")
        );
        assert_eq!(split(""), pair("", ""));
    }

    #[test]
    fn split_counts_chars_not_bytes() {
        let name = "r\u{e9}union-\u{e9}quipe-\u{1f3a5}-enregistr\u{e9}e-\u{e0}-mardi.webm";
        let (head, tail) = split_file_name(name);
        assert_eq!(format!("{head}{tail}"), name);
        assert_eq!(tail, "\u{e9}e-\u{e0}-mardi.webm");
        let emoji_only = "\u{1f3a5}".repeat(14);
        let (head, tail) = split_file_name(&emoji_only);
        assert_eq!(head.chars().count(), 4);
        assert_eq!(tail.chars().count(), 10);
        assert_eq!(format!("{head}{tail}"), emoji_only);
    }

    #[test]
    fn starting_toast_shows_only_while_activating() {
        assert_eq!(
            starting_toast_label(&Activating),
            Some("Starting recording\u{2026}")
        );
        for state in [Idle, Recording, Stopping, Saving] {
            assert_eq!(starting_toast_label(&state), None, "{state:?}");
        }
    }

    #[test]
    fn status_bar_visibility_and_mount_truth_tables() {
        for state in [Idle, Activating, Recording, Stopping, Saving] {
            assert!(
                recording_status_bar_visible(true, &state, false),
                "{state:?}"
            );
            assert!(
                recording_status_bar_visible(false, &state, true),
                "{state:?}"
            );
        }
        assert!(!recording_status_bar_visible(false, &Idle, false));
        assert!(!recording_status_bar_visible(false, &Activating, false));
        // Own session never inserted into the set (empty `self_session`).
        for state in [Recording, Stopping, Saving] {
            assert!(
                recording_status_bar_visible(false, &state, false),
                "{state:?}"
            );
        }
        assert!(recording_status_bar_mounted(false, &Activating));
        assert!(!recording_status_bar_mounted(false, &Idle));
        assert!(recording_status_bar_mounted(true, &Idle));
    }

    #[test]
    fn other_recorders_excludes_only_the_own_key() {
        assert_eq!(other_recorders(&ids(&[]), Some("1")), 0);
        assert_eq!(other_recorders(&ids(&["1"]), Some("1")), 0);
        assert_eq!(other_recorders(&ids(&["1", "2"]), Some("1")), 1);
        assert_eq!(other_recorders(&ids(&["1", "2", "3"]), Some("1")), 2);
        assert_eq!(other_recorders(&ids(&["2", "3"]), Some("1")), 2);
        assert_eq!(other_recorders(&ids(&["2", "3"]), None), 2);
    }

    #[test]
    fn save_target_parts_map_known_modes_only() {
        assert_eq!(
            save_target_from_parts(Some("a.webm".into()), Some("file")),
            Some(target("a.webm", SaveMode::File))
        );
        assert_eq!(
            save_target_from_parts(Some("a.webm".into()), Some("download")),
            Some(target("a.webm", SaveMode::Download))
        );
        assert_eq!(
            save_target_from_parts(Some("a.webm".into()), Some("other")),
            None
        );
        assert_eq!(save_target_from_parts(Some("a.webm".into()), None), None);
        assert_eq!(save_target_from_parts(None, Some("file")), None);
    }

    #[test]
    fn recorder_update_clears_on_activating_and_reads_on_recording() {
        let a = target("a.webm", SaveMode::File);
        let b = target("b.webm", SaveMode::Download);
        let update = recorder_update("activating", Some(&a), || unreachable!());
        assert_eq!(update.save_target, None);
        let update = recorder_update("recording", Some(&a), || Some(b.clone()));
        assert_eq!(update.save_target, Some(b.clone()));
        for js_state in ["stopping", "saving", "failed", "idle", "saved"] {
            let update = recorder_update(js_state, Some(&a), || unreachable!());
            assert_eq!(update.save_target, Some(a.clone()), "{js_state}");
        }
        for js_state in [
            "activating",
            "recording",
            "stopping",
            "saving",
            "failed",
            "idle",
        ] {
            let update = recorder_update(js_state, Some(&a), || Some(b.clone()));
            assert_eq!(update.saved_notice, None, "{js_state}");
        }
    }

    #[test]
    fn saved_toast_names_the_file_that_was_saved() {
        let mut files = vec![
            target("second.webm", SaveMode::File),
            target("first.webm", SaveMode::File),
        ];
        let mut current: Option<SaveTarget> = None;
        let mut notices = Vec::new();
        for js_state in [
            "activating",
            "recording",
            "stopping",
            "saving",
            "saved",
            "activating",
            "recording",
        ] {
            let update = recorder_update(js_state, current.as_ref(), || files.pop());
            current = update.save_target;
            notices.extend(update.saved_notice);
        }
        assert_eq!(
            notices,
            vec![SavedNotice {
                file_name: Some("first.webm".to_string())
            }]
        );
        assert_eq!(current, Some(target("second.webm", SaveMode::File)));
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use crate::components::captions::CaptionStarter;
    use crate::components::captions_overlay::CaptionsLiveIndicator;
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen_test::wasm_bindgen_test;

    const NAME: &str = "meeting-recording-2026-10-12T14-03-22-123Z.webm";
    const CAPTIONS: &str = "[data-testid=captions-live-indicator]";

    #[component]
    fn Harness(
        from: RecordButtonState,
        others: Vec<String>,
        notice: Option<SavedNotice>,
        captions: bool,
    ) -> Element {
        let mut record_state = use_signal(|| from.clone());
        let mut recording_peer_ids = use_signal(|| others.iter().cloned().collect::<HashSet<_>>());
        let mut local_recording_key = use_signal(|| None::<String>);
        let save_target = use_signal(|| {
            Some(SaveTarget {
                name: NAME.to_string(),
                mode: SaveMode::File,
            })
        });
        let notice = use_signal(|| notice.clone());
        rsx! {
            RecordingStatusBar {
                record_state,
                recording_peer_ids,
                save_target,
                local_recording_key,
                captions_active: captions,
                if captions {
                    CaptionsLiveIndicator { starter: CaptionStarter::Me }
                }
            }
            div { class: "peer-toasts",
                RecordingStartingToast { record_state }
                RecordingSavedToast { notice }
            }
            button {
                id: "go",
                onclick: move |_| {
                    recording_peer_ids.write().insert("7".to_string());
                    let state = RecordButtonState::Recording;
                    let write = local_key_write(&state, "7", local_recording_key.peek().as_deref());
                    if let Some(key) = write {
                        local_recording_key.set(key);
                    }
                    record_state.set(state);
                },
            }
        }
    }

    async fn mount(props: HarnessProps, ready: &str) -> web_sys::Element {
        let doc = gloo_utils::document();
        let root = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&root).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new_with_props(Harness, props),
            dioxus::web::Config::new().rootelement(root.clone()),
        );
        wait_for(&root, ready).await;
        root
    }

    async fn wait_for(root: &web_sys::Element, sel: &str) {
        for _ in 0..300 {
            if root.query_selector(sel).unwrap().is_some() {
                return;
            }
            TimeoutFuture::new(10).await;
        }
        panic!("{sel} never rendered");
    }

    fn find(root: &web_sys::Element, sel: &str) -> web_sys::Element {
        root.query_selector(sel).unwrap().unwrap()
    }

    fn text(root: &web_sys::Element, sel: &str) -> String {
        find(root, sel).text_content().unwrap_or_default()
    }

    fn go(root: &web_sys::Element) {
        find(root, "#go")
            .unchecked_into::<web_sys::HtmlElement>()
            .click();
    }

    fn props(
        from: RecordButtonState,
        others: &[&str],
        notice: Option<SavedNotice>,
    ) -> HarnessProps {
        HarnessProps {
            from,
            others: others.iter().map(|s| s.to_string()).collect(),
            notice,
            captions: false,
        }
    }

    fn with_captions(from: RecordButtonState) -> HarnessProps {
        HarnessProps {
            captions: true,
            ..props(from, &[], None)
        }
    }

    #[wasm_bindgen_test]
    async fn captions_keep_the_bar_up_while_the_live_region_waits_hidden() {
        let root = mount(with_captions(RecordButtonState::Activating), CAPTIONS).await;
        let before = find(&root, ".meeting-status-bar [role=status]");
        assert!(before.class_list().contains("visually-hidden"));
        assert_eq!(before.text_content().unwrap_or_default(), "");
        assert_eq!(before.get_attribute("title"), None);
        go(&root);
        wait_for(&root, ".meeting-status-item--self").await;
        let after = find(&root, ".meeting-status-bar [role=status]");
        assert!(before.is_same_node(Some(&after)));
        assert!(!after.class_list().contains("visually-hidden"));
        assert_eq!(
            text(&root, ".meeting-status-item-label"),
            "You are recording"
        );
        assert!(root.query_selector(CAPTIONS).unwrap().is_some());
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn captions_alone_show_the_bar_with_no_recording_item() {
        let root = mount(with_captions(RecordButtonState::Idle), CAPTIONS).await;
        assert!(root
            .query_selector(".meeting-status-bar")
            .unwrap()
            .is_some());
        assert!(root.query_selector("[role=status]").unwrap().is_none());
        assert!(root
            .query_selector(".meeting-status-item--recording")
            .unwrap()
            .is_none());
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn the_recorder_sees_one_status_naming_the_file() {
        let root = mount(
            props(RecordButtonState::Activating, &["9"], None),
            ".meeting-status-item--recording",
        )
        .await;
        let before = find(&root, ".meeting-status-bar [role=status]");
        assert_eq!(
            before.get_attribute("aria-label").as_deref(),
            Some("This meeting is being recorded")
        );
        go(&root);
        wait_for(&root, ".meeting-status-item--self").await;
        let statuses = root.query_selector_all("[role=status]").unwrap();
        assert_eq!(statuses.length(), 1, "exactly one recording status");
        let item = find(&root, "[role=status]");
        assert!(before.is_same_node(Some(&item)));
        assert!(item.class_list().contains("meeting-status-item--self"));
        assert_eq!(item.get_attribute("aria-label"), None);
        assert_eq!(
            text(&root, ".meeting-status-item-label"),
            "You and 1 other are recording"
        );
        let short = find(&root, ".meeting-status-item--self .rec-label-short");
        assert_eq!(short.text_content().unwrap_or_default(), "You +1 recording");
        assert_eq!(short.get_attribute("aria-hidden").as_deref(), Some("true"));
        let shown = format!(
            "{}{}",
            text(&root, ".rec-target-head"),
            text(&root, ".rec-target-tail")
        );
        assert_eq!(shown, NAME);
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn the_pending_live_region_is_filled_in_place() {
        let root = mount(
            props(RecordButtonState::Activating, &[], None),
            ".visually-hidden [role=status]",
        )
        .await;
        assert!(root
            .query_selector(".meeting-status-bar")
            .unwrap()
            .is_none());
        let before = find(&root, ".visually-hidden [role=status]");
        assert_eq!(before.text_content().unwrap_or_default(), "");
        assert_eq!(before.get_attribute("title"), None);
        assert_eq!(before.get_attribute("aria-label"), None);
        go(&root);
        wait_for(&root, ".meeting-status-bar .meeting-status-item--self").await;
        let after = find(&root, ".meeting-status-bar [role=status]");
        assert!(before.is_same_node(Some(&after)));
        assert_eq!(
            text(&root, ".meeting-status-item-label"),
            "You are recording"
        );
        assert!(root.query_selector(".rec-label-short").unwrap().is_none());
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn the_saved_toast_names_its_captured_file() {
        let notice = SavedNotice {
            file_name: Some("first.webm".to_string()),
        };
        let root = mount(
            props(RecordButtonState::Idle, &[], Some(notice)),
            ".recording-saved-banner",
        )
        .await;
        let toast = find(&root, ".recording-saved-banner");
        assert_eq!(toast.get_attribute("aria-label"), None);
        assert_eq!(
            text(&root, ".recording-saved-banner .rec-target"),
            "first.webm"
        );
        assert_eq!(
            text(&root, ".recording-saved-banner .visually-hidden"),
            " as first.webm"
        );
        root.remove();
    }
}

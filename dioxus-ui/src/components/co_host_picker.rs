// SPDX-License-Identifier: MIT OR Apache-2.0

//! The co-host add field: an editable combobox suggesting people the owner
//! has hosted with. A pick only fills the field; Add still commits it.

use crate::components::attendants::action_bar_announce_text;
use crate::components::co_hosts::CoHostErrorKind;
use crate::meeting_api::{CoHostEntry, CoHostSuggestion};
use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

const STATUS_DEBOUNCE_MS: u32 = 400;
const REPEAT_SUBMIT_MS: f64 = 500.0;
const LOADING_NOTE: &str = "Loading suggestions…";
const NO_MATCH_NOTE: &str = "No matching people. Enter their email or user ID.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftError {
    Invalid(CoHostErrorKind),
    AmbiguousName,
    UnknownName,
    /// Carries the co-host's `person_label`.
    AlreadyCoHost(String),
    /// The typed text, without whitespace, is a co-host's display name.
    CoHostsName {
        label: String,
        text: String,
    },
    /// The typed text, without whitespace, is someone's display name.
    SomeonesName(String),
    /// The typed text, without whitespace, is part of a known name.
    PartOfAName(String),
}

impl DraftError {
    pub fn message(&self) -> String {
        match self {
            Self::Invalid(kind) => kind.message().to_string(),
            Self::AmbiguousName => {
                "More than one person has that name. Pick one from the list.".to_string()
            }
            Self::UnknownName => {
                "No one by that name. Pick from the list, or enter their email or user ID."
                    .to_string()
            }
            Self::AlreadyCoHost(label) => format!("{label} is already a co-host."),
            Self::CoHostsName { label, text } => format!(
                "A co-host, {label}, already has that name. \
                 Press Add again to add “{text}” as a user ID."
            ),
            Self::SomeonesName(text) => format!(
                "“{text}” is someone's display name. Pick them from the list, \
                 or press Add again to add “{text}” as a user ID."
            ),
            Self::PartOfAName(text) => format!(
                "No one by that name. Pick from the list, \
                 or press Add again to add “{text}” as a user ID."
            ),
        }
    }

    /// A second Add on the same text adds it as a literal id.
    fn confirmable(&self) -> bool {
        matches!(
            self,
            Self::CoHostsName { .. } | Self::SomeonesName(_) | Self::PartOfAName(_)
        )
    }
}

/// `Name (id)`, or the id alone.
pub fn person_label(user_id: &str, name: Option<&str>) -> String {
    match name {
        Some(name) => format!("{name} ({user_id})"),
        None => user_id.to_string(),
    }
}

fn selected_text(s: &CoHostSuggestion) -> String {
    format!(
        "{} selected. Choose Add to confirm.",
        person_label(&s.user_id, suggestion_name(s))
    )
}

#[derive(Clone, Debug, Default, PartialEq)]
enum Fetch {
    #[default]
    Idle,
    Loading,
    Ready(Vec<CoHostSuggestion>),
    Failed {
        retried: bool,
    },
}

impl Fetch {
    fn ready(&self) -> &[CoHostSuggestion] {
        match self {
            Fetch::Ready(list) => list,
            _ => &[],
        }
    }
}

/// The server stores and compares ids lowercased.
fn id_key(user_id: &str) -> String {
    user_id.to_lowercase()
}

fn same_id(a: &str, b: &str) -> bool {
    id_key(a) == id_key(b)
}

/// `None` when the display name is missing, blank, or just the id.
fn shown_name<'a>(display_name: Option<&'a str>, user_id: &str) -> Option<&'a str> {
    display_name
        .map(str::trim)
        .filter(|name| !name.is_empty() && !same_id(name, user_id))
}

pub(crate) fn suggestion_name(s: &CoHostSuggestion) -> Option<&str> {
    shown_name(s.display_name.as_deref(), &s.user_id)
}

fn entry_name(e: &CoHostEntry) -> Option<&str> {
    shown_name(e.display_name.as_deref(), &e.user_id)
}

fn as_suggestion(e: &CoHostEntry) -> CoHostSuggestion {
    CoHostSuggestion {
        user_id: e.user_id.clone(),
        display_name: e.display_name.clone(),
    }
}

/// Known people minus current entries, `recent` first, without repeats.
pub(crate) fn candidates(
    recent: &[CoHostSuggestion],
    fetched: &[CoHostSuggestion],
    entries: &[CoHostEntry],
) -> Vec<CoHostSuggestion> {
    let mut seen: HashSet<String> = entries.iter().map(|e| id_key(&e.user_id)).collect();
    recent
        .iter()
        .chain(fetched)
        .filter(|s| seen.insert(id_key(&s.user_id)))
        .cloned()
        .collect()
}

/// Moves everyone in `before` but not in `after` to the top of `recent`.
pub(crate) fn reinsert_removed(
    recent: &mut Vec<CoHostSuggestion>,
    before: &[CoHostEntry],
    after: &[CoHostEntry],
) -> bool {
    let mut changed = false;
    for gone in before
        .iter()
        .filter(|b| !after.iter().any(|a| same_id(&a.user_id, &b.user_id)))
    {
        recent.retain(|s| !same_id(&s.user_id, &gone.user_id));
        recent.insert(0, as_suggestion(gone));
        changed = true;
    }
    changed
}

/// 0 for a match at a word start, 1 for any other substring match.
fn match_rank(haystack: &str, needle: &str) -> Option<u8> {
    let mut rank = None;
    for (at, _) in haystack.match_indices(needle) {
        if !matches!(haystack[..at].chars().next_back(), Some(c) if c.is_alphanumeric()) {
            return Some(0);
        }
        rank = Some(1);
    }
    rank
}

/// Case-insensitive substring matches on name or id; word-start matches
/// first, otherwise `candidates` order.
pub(crate) fn rank_matches(candidates: &[CoHostSuggestion], query: &str) -> Vec<CoHostSuggestion> {
    let needle = query.trim().to_lowercase();
    let mut ranked: Vec<(u8, &CoHostSuggestion)> = candidates
        .iter()
        .filter_map(|s| {
            let by_name = suggestion_name(s).and_then(|n| match_rank(&n.to_lowercase(), &needle));
            let by_id = match_rank(&s.user_id.to_lowercase(), &needle);
            by_name.into_iter().chain(by_id).min().map(|rank| (rank, s))
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, s)| s.clone()).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Resolved {
    /// Validate `text`, then grant it.
    Submit { text: String, name: Option<String> },
    /// A typed name found one person: fill in their id, grant nothing yet.
    Pick(CoHostSuggestion),
}

/// What Add does with the draft. An id beats a name, and a name never grants
/// on its own. `confirmed`: Add already refused this exact text once.
pub(crate) fn resolve_draft(
    draft: &str,
    picked: Option<&CoHostSuggestion>,
    candidates: &[CoHostSuggestion],
    entries: &[CoHostEntry],
    confirmed: bool,
) -> Result<Resolved, DraftError> {
    let text = draft.trim();
    let submit = |s: &CoHostSuggestion| Resolved::Submit {
        text: s.user_id.clone(),
        name: suggestion_name(s).map(str::to_string),
    };
    if let Some(p) = picked.filter(|p| p.user_id == text) {
        return Ok(submit(p));
    }
    if let Some(known) = candidates.iter().find(|s| same_id(&s.user_id, text)) {
        return Ok(submit(known));
    }
    if let Some(entry) = entries.iter().find(|e| same_id(&e.user_id, text)) {
        return Ok(submit(&as_suggestion(entry)));
    }
    let wanted = text.to_lowercase();
    let spaced = text.contains(char::is_whitespace);
    let literal = Resolved::Submit {
        text: draft.to_string(),
        name: None,
    };
    let co_host = entries
        .iter()
        .find(|e| entry_name(e).is_some_and(|n| n.to_lowercase() == wanted));
    let refusal = if let Some(entry) = co_host {
        let label = person_label(&entry.user_id, entry_name(entry));
        if !spaced {
            DraftError::CoHostsName {
                label,
                text: text.to_string(),
            }
        } else if !entry.suspended {
            return Err(DraftError::AlreadyCoHost(label));
        } else if name_is_shared(&wanted, candidates, entries) {
            return Err(DraftError::AmbiguousName);
        } else {
            return Ok(Resolved::Pick(as_suggestion(entry)));
        }
    } else {
        let mut named = candidates
            .iter()
            .filter(|s| suggestion_name(s).is_some_and(|n| n.to_lowercase() == wanted));
        match (named.next(), named.next()) {
            (Some(_), _) if !spaced => DraftError::SomeonesName(text.to_string()),
            (Some(_), Some(_)) => return Err(DraftError::AmbiguousName),
            (Some(one), None) => return Ok(Resolved::Pick(one.clone())),
            (None, _) if spaced => return Err(DraftError::UnknownName),
            (None, _) if part_of_a_name(text, candidates, entries) => {
                DraftError::PartOfAName(text.to_string())
            }
            (None, _) => return Ok(literal),
        }
    };
    if confirmed {
        Ok(literal)
    } else {
        Err(refusal)
    }
}

/// More than one known person or co-host goes by `wanted`.
fn name_is_shared(wanted: &str, candidates: &[CoHostSuggestion], entries: &[CoHostEntry]) -> bool {
    candidates
        .iter()
        .filter_map(suggestion_name)
        .chain(entries.iter().filter_map(entry_name))
        .filter(|n| n.to_lowercase() == wanted)
        .count()
        > 1
}

fn part_of_a_name(text: &str, candidates: &[CoHostSuggestion], entries: &[CoHostEntry]) -> bool {
    let wanted = text.to_lowercase();
    let mut names = candidates
        .iter()
        .filter_map(suggestion_name)
        .chain(entries.iter().filter_map(entry_name));
    !text.is_empty() && names.any(|n| n.to_lowercase().contains(&wanted))
}

pub(crate) fn match_count_text(count: usize) -> String {
    match count {
        0 => "No matching people".to_string(),
        1 => "1 suggestion".to_string(),
        n => format!("{n} suggestions"),
    }
}

fn option_id(base: &str, index: usize) -> String {
    format!("{base}-opt-{index}")
}

fn announce(mut status: Signal<(u32, String)>, text: String) {
    if let Ok(mut slot) = status.try_write() {
        slot.0 = slot.0.wrapping_add(1);
        slot.1 = text;
    }
}

fn scroll_into_view_nearest(id: &str) {
    let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
    else {
        return;
    };
    let options = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&options, &"block".into(), &"nearest".into());
    if let Ok(scroll) = js_sys::Reflect::get(&el, &"scrollIntoView".into())
        .and_then(|f| f.dyn_into::<js_sys::Function>())
    {
        let _ = scroll.call1(&el, &options);
    }
}

/// A capture-phase `window` listener, removed on drop.
struct CaptureListener {
    event: &'static str,
    callback: Closure<dyn FnMut()>,
}

impl CaptureListener {
    fn new(event: &'static str, handler: impl FnMut() + 'static) -> Self {
        let callback = Closure::<dyn FnMut()>::new(handler);
        if let Some(window) = web_sys::window() {
            let _ = window.add_event_listener_with_callback_and_bool(
                event,
                callback.as_ref().unchecked_ref(),
                true,
            );
        }
        Self { event, callback }
    }
}

impl Drop for CaptureListener {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            let _ = window.remove_event_listener_with_callback_and_bool(
                self.event,
                self.callback.as_ref().unchecked_ref(),
                true,
            );
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Press {
    Up,
    Down,
    /// The field blurred mid-press; the list closes once the press ends.
    Blurred,
}

/// Suggestion state the section's submit also reads.
#[derive(Clone, Copy, PartialEq)]
pub struct CoHostPickerState {
    fetch: Signal<Fetch>,
    recent: Signal<Vec<CoHostSuggestion>>,
    picked: Signal<Option<CoHostSuggestion>>,
    expanded: Signal<bool>,
    active: Signal<Option<usize>>,
    status: Signal<(u32, String)>,
    /// When Add last turned a typed name into a pick, or ignored a press after one.
    submit_picked_at: Signal<Option<f64>>,
    /// The text Add last refused with a confirmable error, and when.
    refused: Signal<Option<(String, f64)>>,
}

pub fn use_co_host_picker() -> CoHostPickerState {
    CoHostPickerState {
        fetch: use_signal(Fetch::default),
        recent: use_signal(Vec::new),
        picked: use_signal(|| None),
        expanded: use_signal(|| false),
        active: use_signal(|| None),
        status: use_signal(|| (0, String::new())),
        submit_picked_at: use_signal(|| None),
        refused: use_signal(|| None),
    }
}

impl CoHostPickerState {
    /// Resolves the draft for Add. `Ok(None)`: nothing to grant yet.
    pub fn submit(
        mut self,
        draft: Signal<String>,
        entries: &[CoHostEntry],
    ) -> Result<Option<(String, Option<String>)>, DraftError> {
        let now = web_sys::window()
            .and_then(|w| w.performance())
            .map_or_else(js_sys::Date::now, |p| p.now());
        if self
            .submit_picked_at
            .peek()
            .is_some_and(|at| (0.0..REPEAT_SUBMIT_MS).contains(&(now - at)))
        {
            self.submit_picked_at.set(Some(now));
            return Ok(None);
        }
        let typed = draft.peek().clone();
        let confirmed = self
            .refused
            .peek()
            .as_ref()
            .filter(|(text, _)| *text == typed)
            .is_some_and(|(_, at)| !(0.0..REPEAT_SUBMIT_MS).contains(&(now - at)));
        let known = {
            let fetch = self.fetch.peek();
            candidates(&self.recent.peek(), fetch.ready(), entries)
        };
        let resolved = resolve_draft(
            &typed,
            self.picked.peek().as_ref(),
            &known,
            entries,
            confirmed,
        );
        self.close();
        match &resolved {
            Err(error) if error.confirmable() => self.refused.set(Some((typed, now))),
            _ => self.forget_refusal(),
        }
        match resolved {
            Ok(Resolved::Submit { text, name }) => {
                if self.picked.peek().is_some() {
                    self.picked.set(None);
                }
                announce(self.status, String::new());
                Ok(Some((text, name)))
            }
            Ok(Resolved::Pick(one)) => {
                self.pick(draft, &one);
                self.submit_picked_at.set(Some(now));
                Ok(None)
            }
            Err(error) => {
                self.expanded.set(true);
                Err(error)
            }
        }
    }

    fn forget_refusal(mut self) {
        if self.refused.peek().is_some() {
            self.refused.set(None);
        }
    }

    fn pick(mut self, mut draft: Signal<String>, chosen: &CoHostSuggestion) {
        draft.set(chosen.user_id.clone());
        self.picked.set(Some(chosen.clone()));
        self.close();
        announce(self.status, selected_text(chosen));
    }

    fn close(mut self) {
        if *self.expanded.peek() {
            self.expanded.set(false);
        }
        if self.active.peek().is_some() {
            self.active.set(None);
        }
    }

    /// The first call fetches; after a failure, one more call retries.
    fn load(self, meeting_id: String) {
        let retried = match *self.fetch.peek() {
            Fetch::Idle => false,
            Fetch::Failed { retried: false } => true,
            _ => return,
        };
        let mut fetch = self.fetch;
        fetch.set(Fetch::Loading);
        wasm_bindgen_futures::spawn_local(async move {
            let next = match crate::meeting_api::list_co_host_suggestions(&meeting_id).await {
                Ok(list) => Fetch::Ready(list),
                Err(e) => {
                    log::warn!("list_co_host_suggestions failed: {e}");
                    Fetch::Failed { retried }
                }
            };
            if let Ok(mut slot) = fetch.try_write() {
                *slot = next;
            }
        });
    }
}

#[component]
pub fn CoHostPicker(
    state: CoHostPickerState,
    meeting_id: String,
    base: String,
    input_id: String,
    error_id: String,
    draft: Signal<String>,
    draft_error: Signal<Option<DraftError>>,
    entries: Signal<Vec<CoHostEntry>>,
) -> Element {
    let CoHostPickerState {
        fetch,
        mut recent,
        mut picked,
        mut expanded,
        mut active,
        status,
        ..
    } = state;
    let mut draft = draft;
    let mut draft_error = draft_error;
    let pending_status: Rc<RefCell<Option<Timeout>>> = use_hook(|| Rc::new(RefCell::new(None)));
    let seen_entries: Rc<RefCell<Option<Vec<CoHostEntry>>>> =
        use_hook(|| Rc::new(RefCell::new(None)));
    let was_open = use_hook(|| Rc::new(Cell::new(false)));
    let press = use_hook(|| Rc::new(Cell::new(Press::Up)));
    use_hook({
        let press = press.clone();
        move || {
            let down = {
                let press = press.clone();
                CaptureListener::new("pointerdown", move || {
                    if press.get() == Press::Up {
                        press.set(Press::Down);
                    }
                })
            };
            let release = move || {
                if press.replace(Press::Up) == Press::Blurred {
                    state.close();
                }
            };
            Rc::new([
                down,
                CaptureListener::new("click", release.clone()),
                CaptureListener::new("auxclick", release.clone()),
                CaptureListener::new("contextmenu", release.clone()),
                CaptureListener::new("pointercancel", release),
            ])
        }
    });

    use_effect(move || {
        let now = entries();
        if let Some(before) = seen_entries.replace(Some(now.clone())) {
            let mut list = recent.peek().clone();
            if reinsert_removed(&mut list, &before, &now) {
                recent.set(list);
            }
        }
    });

    let (known, loading) = {
        let fetch = fetch.read();
        (
            candidates(&recent.read(), fetch.ready(), &entries.read()),
            matches!(*fetch, Fetch::Loading),
        )
    };
    let query = draft();
    let matches = Rc::new(rank_matches(&known, &query));
    let is_expanded = expanded();
    let list_open = is_expanded && !matches.is_empty();
    let active_index = active().filter(|i| list_open && *i < matches.len());
    let invalid = draft_error().is_some();
    let note = if invalid {
        None
    } else if is_expanded && loading {
        Some(LOADING_NOTE.to_string())
    } else if is_expanded && !known.is_empty() && matches.is_empty() {
        Some(NO_MATCH_NOTE.to_string())
    } else {
        picked()
            .filter(|p| p.user_id == query)
            .map(|p| selected_text(&p))
    };
    let list_id = format!("{base}-suggestions");
    if !was_open.replace(list_open) && list_open {
        let target = list_id.clone();
        spawn(async move {
            gloo_timers::future::TimeoutFuture::new(0).await;
            scroll_into_view_nearest(&target);
        });
    }
    let placeholder = if known.is_empty() {
        "Email or user ID"
    } else {
        "Name or email"
    };
    let status_text = {
        let (nonce, text) = &*status.read();
        action_bar_announce_text(text, *nonce)
    };
    let rows: Vec<(CoHostSuggestion, Option<String>)> = if list_open {
        matches
            .iter()
            .map(|s| (s.clone(), suggestion_name(s).map(str::to_string)))
            .collect()
    } else {
        Vec::new()
    };

    let pick = {
        let pending_status = pending_status.clone();
        move |chosen: &CoHostSuggestion| {
            drop(pending_status.take());
            if draft_error.peek().is_some() {
                draft_error.set(None);
            }
            state.pick(draft, chosen);
        }
    };

    let onkeydown = {
        let matches = matches.clone();
        let base = base.clone();
        let mut pick = pick.clone();
        move |e: KeyboardEvent| {
            if e.is_composing() {
                return;
            }
            let count = matches.len();
            let current = active.peek().filter(|i| list_open && *i < count);
            match e.key() {
                key @ (Key::ArrowDown | Key::ArrowUp) => {
                    e.prevent_default();
                    if !*expanded.peek() {
                        expanded.set(true);
                    }
                    if count == 0 {
                        return;
                    }
                    let next = match (current, key == Key::ArrowDown) {
                        (Some(i), true) => (i + 1) % count,
                        (Some(i), false) => (i + count - 1) % count,
                        (None, true) => 0,
                        (None, false) => count - 1,
                    };
                    active.set(Some(next));
                    let target = option_id(&base, next);
                    spawn(async move {
                        gloo_timers::future::TimeoutFuture::new(0).await;
                        scroll_into_view_nearest(&target);
                    });
                }
                Key::Enter if e.is_auto_repeating() => e.prevent_default(),
                Key::Enter => match current {
                    Some(i) => {
                        e.prevent_default();
                        pick(&matches[i]);
                    }
                    None => state.close(),
                },
                Key::Escape if list_open => {
                    e.prevent_default();
                    e.stop_propagation();
                    state.close();
                }
                Key::Escape | Key::Tab => state.close(),
                Key::Home | Key::End | Key::ArrowLeft | Key::ArrowRight if current.is_some() => {
                    active.set(None);
                }
                _ => {}
            }
        }
    };

    rsx! {
        div { class: "co-hosts-combobox",
            input {
                id: "{input_id}",
                class: if invalid {
                    "input-apple input-apple--invalid co-hosts-input"
                } else {
                    "input-apple co-hosts-input"
                },
                "data-testid": "co-host-input",
                r#type: "text",
                role: "combobox",
                autocomplete: "off",
                autocapitalize: "none",
                spellcheck: "false",
                placeholder,
                value: "{query}",
                "aria-autocomplete": "list",
                "aria-expanded": if list_open { "true" } else { "false" },
                "aria-controls": "{list_id}",
                "aria-activedescendant": active_index.map(|i| option_id(&base, i)),
                "aria-invalid": invalid.then_some("true"),
                "aria-describedby": invalid.then(|| error_id.clone()),
                onfocus: move |_| state.load(meeting_id.clone()),
                onblur: move |_| {
                    if press.get() == Press::Down {
                        press.set(Press::Blurred);
                    } else {
                        state.close();
                    }
                },
                onclick: move |_| {
                    if !*expanded.peek() {
                        expanded.set(true);
                    }
                },
                oninput: {
                    let pending_status = pending_status.clone();
                    move |e: FormEvent| {
                        let value = e.value();
                        if picked.peek().as_ref().is_some_and(|p| p.user_id != value) {
                            picked.set(None);
                        }
                        draft.set(value);
                        state.forget_refusal();
                        if draft_error.peek().is_some() {
                            draft_error.set(None);
                        }
                        if !*expanded.peek() {
                            expanded.set(true);
                        }
                        if active.peek().is_some() {
                            active.set(None);
                        }
                        let timer = Timeout::new(STATUS_DEBOUNCE_MS, move || {
                            if !expanded.try_peek().is_ok_and(|open| *open) {
                                return;
                            }
                            let count = {
                                let (Ok(fetch), Ok(recent), Ok(entries), Ok(draft)) = (
                                    fetch.try_peek(),
                                    recent.try_peek(),
                                    entries.try_peek(),
                                    draft.try_peek(),
                                ) else {
                                    return;
                                };
                                let known = candidates(&recent, fetch.ready(), &entries);
                                if known.is_empty() {
                                    return;
                                }
                                rank_matches(&known, &draft).len()
                            };
                            announce(status, match_count_text(count));
                        });
                        *pending_status.borrow_mut() = Some(timer);
                    }
                },
                onkeydown,
            }
            ul {
                id: "{list_id}",
                class: "co-hosts-suggestions",
                role: "listbox",
                "aria-label": "People you've hosted with",
                "data-testid": "co-host-suggestions",
                hidden: !list_open,
                onmousedown: move |e: MouseEvent| e.prevent_default(),
                for (index, (suggestion, name)) in rows.into_iter().enumerate() {
                    li {
                        key: "{suggestion.user_id}",
                        id: option_id(&base, index),
                        class: "co-hosts-option",
                        role: "option",
                        "aria-selected": (active_index == Some(index)).then_some("true"),
                        "data-testid": "co-host-suggestion",
                        "data-user-id": "{suggestion.user_id}",
                        onclick: {
                            let mut pick = pick.clone();
                            let chosen = suggestion.clone();
                            move |_| pick(&chosen)
                        },
                        if let Some(name) = name {
                            span { class: "co-hosts-option-name", "{name}" }
                            span { class: "co-hosts-option-id", "{suggestion.user_id}" }
                        } else {
                            span { class: "co-hosts-option-name", "{suggestion.user_id}" }
                        }
                    }
                }
            }
            if let Some(text) = note {
                p {
                    class: "co-hosts-suggest-note",
                    "aria-hidden": (text == LOADING_NOTE).then_some("true"),
                    "data-testid": "co-host-suggest-note",
                    "{text}"
                }
            }
            span {
                class: "visually-hidden",
                role: "status",
                "aria-live": "polite",
                "aria-atomic": "true",
                "data-testid": "co-host-suggest-status",
                "{status_text}"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(user_id: &str, name: Option<&str>) -> CoHostSuggestion {
        CoHostSuggestion {
            user_id: user_id.to_string(),
            display_name: name.map(str::to_string),
        }
    }

    fn entry(user_id: &str, name: Option<&str>) -> CoHostEntry {
        CoHostEntry {
            user_id: user_id.to_string(),
            persistent: true,
            is_present_host: false,
            display_name: name.map(str::to_string),
            designated: true,
            suspended: false,
        }
    }

    fn ids(list: &[CoHostSuggestion]) -> Vec<&str> {
        list.iter().map(|s| s.user_id.as_str()).collect()
    }

    fn known() -> Vec<CoHostSuggestion> {
        vec![
            person("zoe@example.com", Some("Zoe Park")),
            person("jo@example.com", Some("Joanna Diaz")),
            person("ann.lee@example.com", Some("Ann Lee")),
            person("pat@example.com", None),
        ]
    }

    #[test]
    fn matches_name_or_id_case_insensitively() {
        assert_eq!(
            ids(&rank_matches(&known(), "ZOE P")),
            vec!["zoe@example.com"]
        );
        assert_eq!(
            ids(&rank_matches(&known(), "PAT@")),
            vec!["pat@example.com"]
        );
        assert_eq!(ids(&rank_matches(&known(), "diaz")), vec!["jo@example.com"]);
        assert!(rank_matches(&known(), "nobody").is_empty());
        assert_eq!(
            ids(&rank_matches(&known(), "  ")),
            ids(&known()),
            "an empty query lists everyone in server order"
        );
    }

    #[test]
    fn word_start_matches_come_first_then_server_order() {
        assert_eq!(
            ids(&rank_matches(&known(), "ann")),
            vec!["ann.lee@example.com", "jo@example.com"],
            "Ann Lee starts a word; JoANNa does not"
        );
        assert_eq!(
            ids(&rank_matches(&known(), "example")),
            vec![
                "zoe@example.com",
                "jo@example.com",
                "ann.lee@example.com",
                "pat@example.com"
            ],
            "equal ranks keep server order"
        );
        let ids_only = [
            person("kathleen@example.com", None),
            person("ann.lee@example.com", None),
        ];
        assert_eq!(
            ids(&rank_matches(&ids_only, "lee")),
            vec!["ann.lee@example.com", "kathleen@example.com"],
            "a word after punctuation in the id counts as a word start"
        );
    }

    #[test]
    fn current_entries_are_excluded_and_repeats_dropped() {
        let fetched = known();
        let recent = vec![person("pat@example.com", None)];
        let entries = vec![entry("zoe@example.com", Some("Zoe Park"))];
        assert_eq!(
            ids(&candidates(&recent, &fetched, &entries)),
            vec!["pat@example.com", "jo@example.com", "ann.lee@example.com"]
        );
    }

    #[test]
    fn ids_match_in_any_case_like_the_server() {
        let fetched = vec![person("Ann@Example.com", Some("Ann"))];
        let entries = vec![entry("ann@example.com", None)];
        assert!(
            candidates(&[], &fetched, &entries).is_empty(),
            "a co-host stored lowercase is not suggested in the case they signed in with"
        );

        let mut recent = Vec::new();
        assert!(
            !reinsert_removed(
                &mut recent,
                &entries,
                &[entry("Ann@Example.com", Some("Ann"))]
            ),
            "joining changes the case, not the membership"
        );
        assert!(recent.is_empty());

        assert!(reinsert_removed(&mut recent, &entries, &[]));
        assert_eq!(
            ids(&candidates(&recent, &fetched, &[])),
            vec!["ann@example.com"],
            "a removed co-host is listed once"
        );
        reinsert_removed(&mut recent, &[entry("Ann@Example.com", None)], &[]);
        assert_eq!(
            ids(&recent),
            vec!["Ann@Example.com"],
            "moved, not duplicated"
        );

        let accented = vec![person("Åsa@example.com", Some("Åsa Berg"))];
        assert_eq!(
            resolve_draft("åsa@example.com", None, &accented, &[], false),
            submit("Åsa@example.com", Some("Åsa Berg"))
        );
    }

    #[test]
    fn a_removed_person_is_reinserted_at_the_top() {
        let before = vec![
            entry("alice@example.com", Some("Alice")),
            entry("bob@example.com", None),
        ];
        let after = vec![entry("bob@example.com", None)];
        let mut recent = vec![person("pat@example.com", None)];
        assert!(reinsert_removed(&mut recent, &before, &after));
        assert_eq!(recent[0], person("alice@example.com", Some("Alice")));
        assert_eq!(ids(&recent), vec!["alice@example.com", "pat@example.com"]);

        assert!(
            !reinsert_removed(&mut recent, &after, &after),
            "no removal, no change"
        );
        let mut again = vec![
            person("pat@example.com", None),
            person("alice@example.com", Some("Alice")),
        ];
        reinsert_removed(&mut again, &before, &after);
        assert_eq!(
            ids(&again),
            vec!["alice@example.com", "pat@example.com"],
            "moved, not duplicated"
        );
        let shown = candidates(&recent, &known(), &after);
        assert_eq!(shown[0].user_id, "alice@example.com");
    }

    fn submit(text: &str, name: Option<&str>) -> Result<Resolved, DraftError> {
        Ok(Resolved::Submit {
            text: text.to_string(),
            name: name.map(str::to_string),
        })
    }

    #[test]
    fn an_exact_name_only_picks_or_asks_which_one() {
        let mut people = known();
        assert_eq!(
            resolve_draft("  ann LEE ", None, &people, &[], false),
            Ok(Resolved::Pick(person(
                "ann.lee@example.com",
                Some("Ann Lee")
            )))
        );
        people.push(person("ann2@example.com", Some("Ann Lee")));
        assert_eq!(
            resolve_draft("Ann Lee", None, &people, &[], false),
            Err(DraftError::AmbiguousName)
        );
        assert_eq!(
            resolve_draft(" new@example.com", None, &people, &[], false),
            submit(" new@example.com", None),
            "no name match: the raw text goes to validation"
        );
    }

    #[test]
    fn a_look_alike_name_never_grants_on_add() {
        let people = vec![person("mallory@evil.example", Some("Alice Smith"))];
        for confirmed in [false, true] {
            assert_eq!(
                resolve_draft("Alice Smith", None, &people, &[], confirmed),
                Ok(Resolved::Pick(people[0].clone())),
                "the owner sees Mallory's id before a second Add"
            );
        }
    }

    #[test]
    fn a_typed_id_beats_a_name_spelled_like_it() {
        let people = vec![
            person("mallory@evil.example", Some("jsmith")),
            person("jsmith", Some("John Smith")),
        ];
        assert_eq!(
            resolve_draft("jsmith", None, &people, &[], false),
            submit("jsmith", Some("John Smith"))
        );
        assert_eq!(
            resolve_draft("JSmith", None, &people, &[], false),
            submit("jsmith", Some("John Smith")),
            "ids match in any case"
        );
        let entries = [entry("jsmith", None)];
        assert_eq!(
            resolve_draft("jsmith", None, &people[..1], &entries, false),
            submit("jsmith", None),
            "a current co-host's id can be added again"
        );
    }

    #[test]
    fn an_id_spelled_like_a_known_name_is_refused_until_confirmed() {
        let people = vec![person("mallory@evil.example", Some("jsmith"))];
        let refused = resolve_draft("jsmith", None, &people, &[], false);
        assert_eq!(
            refused,
            Err(DraftError::SomeonesName("jsmith".to_string())),
            "neither a pick of Mallory nor a grant"
        );
        assert_eq!(
            refused.unwrap_err().message(),
            "“jsmith” is someone's display name. Pick them from the list, \
             or press Add again to add “jsmith” as a user ID."
        );
        assert_eq!(
            resolve_draft("jsmith", None, &people, &[], true),
            submit("jsmith", None),
            "a second Add grants exactly what was typed"
        );
    }

    #[test]
    fn a_current_co_hosts_name_is_never_sent_as_an_id() {
        let entries = [
            entry("ann.lee@example.com", Some("Ann Lee")),
            entry("pat@example.com", None),
        ];
        let err = resolve_draft(" ann LEE ", None, &known()[..1], &entries, false);
        assert_eq!(
            err,
            Err(DraftError::AlreadyCoHost(
                "Ann Lee (ann.lee@example.com)".to_string()
            ))
        );
        assert_eq!(
            err.unwrap_err().message(),
            "Ann Lee (ann.lee@example.com) is already a co-host."
        );
        assert_eq!(
            resolve_draft("Ann Lee", None, &[], &entries, true),
            Err(DraftError::AlreadyCoHost(
                "Ann Lee (ann.lee@example.com)".to_string()
            )),
            "a spaced name is never overridden"
        );
        let solo = [entry("ann@example.com", Some("jsmith"))];
        let refused = resolve_draft("jsmith", None, &[], &solo, false);
        assert_eq!(
            refused.clone().unwrap_err().message(),
            "A co-host, jsmith (ann@example.com), already has that name. \
             Press Add again to add “jsmith” as a user ID."
        );
        assert!(matches!(refused, Err(DraftError::CoHostsName { .. })));
        assert_eq!(
            resolve_draft("jsmith", None, &[], &solo, true),
            submit("jsmith", None),
            "a colleague whose plain id is a co-host's name can still be added"
        );
        assert_eq!(
            resolve_draft("Ann", None, &[], &entries, false),
            Err(DraftError::PartOfAName("Ann".to_string())),
            "part of a co-host's name"
        );
        assert_eq!(
            resolve_draft("ANN.lee@example.com", None, &[], &entries, false),
            submit("ann.lee@example.com", Some("Ann Lee")),
            "their id still goes through"
        );
        assert_eq!(
            resolve_draft("pat", None, &[], &entries, false),
            submit("pat", None),
            "an id-only co-host has no name to match"
        );
    }

    fn paused(user_id: &str, name: &str) -> CoHostEntry {
        CoHostEntry {
            suspended: true,
            ..entry(user_id, Some(name))
        }
    }

    #[test]
    fn a_paused_co_host_typed_by_their_unique_full_name_is_picked_to_restore() {
        assert_eq!(
            resolve_draft(
                "ann lee",
                None,
                &[],
                &[paused("ann.lee@example.com", "Ann Lee")],
                false
            ),
            Ok(Resolved::Pick(person(
                "ann.lee@example.com",
                Some("Ann Lee")
            )))
        );
        assert_eq!(
            resolve_draft(
                "Ann Lee",
                None,
                &[person("ann2@example.com", Some("Ann Lee"))],
                &[paused("ann.lee@example.com", "Ann Lee")],
                false
            ),
            Err(DraftError::AmbiguousName),
            "a shared name never restores anyone"
        );
    }

    #[test]
    fn a_kicked_co_host_named_like_a_plain_id_is_never_picked() {
        let kicked = [paused("mallory@evil.example", "jsmith")];
        assert_eq!(
            resolve_draft("jsmith", None, &[], &kicked, false),
            Err(DraftError::CoHostsName {
                label: "jsmith (mallory@evil.example)".to_string(),
                text: "jsmith".to_string(),
            })
        );
        assert_eq!(
            resolve_draft("jsmith", None, &[], &kicked, true),
            submit("jsmith", None),
            "a second Add grants exactly what was typed"
        );
    }

    #[test]
    fn a_partial_or_spaced_name_is_not_sent_as_an_id() {
        let people = known();
        assert_eq!(
            resolve_draft("ann", None, &people, &[], false),
            Err(DraftError::PartOfAName("ann".to_string())),
            "part of Ann Lee and Joanna Diaz"
        );
        assert_eq!(
            DraftError::PartOfAName("ann".to_string()).message(),
            "No one by that name. Pick from the list, or press Add again to add “ann” as a user ID."
        );
        assert_eq!(
            resolve_draft("ann", None, &people, &[], true),
            submit("ann", None)
        );
        for confirmed in [false, true] {
            assert_eq!(
                resolve_draft("Bob Smith", None, &people, &[], confirmed),
                Err(DraftError::UnknownName),
                "spaced text is never sent as an id"
            );
        }
        assert_eq!(
            DraftError::UnknownName.message(),
            "No one by that name. Pick from the list, or enter their email or user ID."
        );
        assert_eq!(
            resolve_draft("bob", None, &people, &[], false),
            submit("bob", None)
        );
        assert_eq!(
            resolve_draft("  ", None, &people, &[], false),
            submit("  ", None)
        );
    }

    #[test]
    fn a_pick_carries_its_name_and_a_stale_pick_is_ignored() {
        let mut people = known();
        people.push(person("ann2@example.com", Some("Ann Lee")));
        let picked = person("ann2@example.com", Some("Ann Lee"));
        assert_eq!(
            resolve_draft("ann2@example.com", Some(&picked), &people, &[], false),
            submit("ann2@example.com", Some("Ann Lee"))
        );
        assert_eq!(
            resolve_draft("zoe@example.com", Some(&picked), &people, &[], false),
            submit("zoe@example.com", Some("Zoe Park"))
        );
    }

    #[test]
    fn a_label_shows_the_id_next_to_the_name() {
        assert_eq!(
            person_label("alice@corp.com", Some("Alice Smith")),
            "Alice Smith (alice@corp.com)"
        );
        assert_eq!(person_label("alice@corp.com", None), "alice@corp.com");
        assert_eq!(
            selected_text(&person("pat@example.com", None)),
            "pat@example.com selected. Choose Add to confirm."
        );
    }

    #[test]
    fn a_missing_blank_or_id_name_shows_the_id_alone() {
        assert_eq!(
            suggestion_name(&person("a@x.com", Some(" Ann "))),
            Some("Ann")
        );
        assert_eq!(suggestion_name(&person("a@x.com", None)), None);
        assert_eq!(suggestion_name(&person("a@x.com", Some("   "))), None);
        assert_eq!(suggestion_name(&person("a@x.com", Some("A@x.com"))), None);
    }

    #[test]
    fn the_status_counts_matches() {
        assert_eq!(match_count_text(0), "No matching people");
        assert_eq!(match_count_text(1), "1 suggestion");
        assert_eq!(match_count_text(3), "3 suggestions");
    }
}

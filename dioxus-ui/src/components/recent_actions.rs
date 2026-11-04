// SPDX-License-Identifier: MIT OR Apache-2.0

//! The action bar's "Recent actions" menu: what this client used last, across meetings.

use crate::components::action_bar_layout::ActionBarSlot;
use crate::components::action_bar_shrink::blur_clipped_focus;
use crate::components::attendants::{
    focus_slot_trigger, live_slot_icon, overflow_slot_icon, settle_slot_focus,
};
use crate::components::avatar::avatar_editing_allowed;
use crate::components::avatar_settings::focus_first_taker;
use crate::components::captions::{captions_button_hint, CaptionsButtonState};
use crate::components::device_settings_modal::SETTINGS_DIALOG_ID;
use crate::components::meeting_footer::focus_was_dropped;
use crate::components::video_control_buttons::{
    slot_item, OverflowShareItems, RecordButtonState, ShareButtonState,
};
use crate::components::window_event_listener::WindowEventListener;
use crate::local_storage::{load_json, save_json};
use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use videocall_client::utils::is_ios;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

const LIST_KEY: &str = "vc_recent_actions";
const LIMIT_KEY: &str = "vc_recent_actions_limit";
const LIMIT_HINT_ID: &str = "recent-actions-limit-hint";
pub const LIMIT_OPTIONS: [usize; 4] = [3, 5, 8, 10];
pub const DEFAULT_LIMIT: usize = 5;
pub const RECENT_ACTIONS_TRIGGER_ID: &str = "recent-actions-trigger";
const POPOVER_ID: &str = "recent-actions-popover";
const MENU_SELECTOR: &str = "#recent-actions-popover, #recent-actions-trigger";
const HEADING_ID: &str = "recent-actions-heading";
const REVEAL_RETRIES: u8 = 10;
const FLASH_CLASS: &str = "recent-target-flash";
const FLASH_MS: u32 = 1600;

macro_rules! settings_targets {
    ($($name:ident: $slug:literal, $label:literal, $section:literal, $title:literal;)+) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum SettingsTarget {
            $($name),+
        }

        impl SettingsTarget {
            pub const ALL: &'static [SettingsTarget] = &[$(SettingsTarget::$name),+];

            pub fn slug(self) -> &'static str {
                match self { $(SettingsTarget::$name => $slug),+ }
            }

            pub fn label(self) -> &'static str {
                match self { $(SettingsTarget::$name => $label),+ }
            }

            /// The `DeviceSettingsModal` `initial_section` that shows this control.
            pub fn section(self) -> &'static str {
                match self { $(SettingsTarget::$name => $section),+ }
            }

            pub fn section_title(self) -> &'static str {
                match self { $(SettingsTarget::$name => $title),+ }
            }
        }
    };
}

settings_targets! {
    Microphone: "setting:microphone", "Microphone", "audio", "Audio";
    Speaker: "setting:speaker", "Speaker", "audio", "Audio";
    Camera: "setting:camera", "Camera", "video", "Video";
    PreferredProtocol: "setting:preferred_protocol", "Preferred protocol", "network", "Network";
    Theme: "setting:theme", "Theme", "appearance", "Appearance";
    SpeakerHighlight: "setting:speaker_highlight", "Speaker highlight", "appearance", "Appearance";
    ActionBarPosition: "setting:action_bar_position", "Action bar position", "preferences", "Preferences";
    AutoHide: "setting:auto_hide", "Auto-hide", "preferences", "Preferences";
    AutoShrink: "setting:auto_shrink", "Auto-shrink", "preferences", "Preferences";
    Density: "setting:density", "Density", "preferences", "Preferences";
    SelfView: "setting:self_view", "Self view", "preferences", "Preferences";
    VideoTiles: "setting:video_tiles", "Video tiles", "preferences", "Preferences";
    Notifications: "setting:notifications", "Notifications", "preferences", "Preferences";
    Avatar: "setting:avatar", "Avatar", "profile", "Profile";
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecentAction {
    Slot(ActionBarSlot),
    Setting(SettingsTarget),
}

impl RecentAction {
    pub fn slug(self) -> String {
        match self {
            RecentAction::Slot(slot) => format!("slot:{}", slot.slug()),
            RecentAction::Setting(target) => target.slug().to_string(),
        }
    }

    pub fn from_slug(slug: &str) -> Option<Self> {
        if let Some(slot) = slug.strip_prefix("slot:") {
            return ActionBarSlot::from_slug(slot)
                .filter(|slot| is_recordable(*slot))
                .map(RecentAction::Slot);
        }
        SettingsTarget::ALL
            .iter()
            .find(|target| target.slug() == slug)
            .map(|target| RecentAction::Setting(*target))
    }
}

fn is_recordable(slot: ActionBarSlot) -> bool {
    !matches!(
        slot,
        ActionBarSlot::Mic | ActionBarSlot::Camera | ActionBarSlot::RecentActions
    )
}

/// The `data-recent-action` value of a control that opens or runs `slot`.
pub fn slot_attr(slot: ActionBarSlot) -> Option<String> {
    is_recordable(slot).then(|| RecentAction::Slot(slot).slug())
}

pub fn push_recent(list: &mut Vec<RecentAction>, action: RecentAction, limit: usize) {
    list.retain(|existing| *existing != action);
    list.insert(0, action);
    list.truncate(limit);
}

pub fn sanitize(raw: &serde_json::Value, limit: usize) -> Vec<RecentAction> {
    let mut list = Vec::new();
    for action in raw
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str())
        .filter_map(RecentAction::from_slug)
    {
        if !list.contains(&action) {
            list.push(action);
        }
    }
    list.truncate(limit);
    list
}

pub fn normalize_limit(raw: u64) -> usize {
    LIMIT_OPTIONS
        .into_iter()
        .find(|option| *option as u64 == raw)
        .unwrap_or(DEFAULT_LIMIT)
}

/// Shared handle to the persisted list. Reading `list` subscribes the reader, so
/// only the menu reads it: a record must not re-render the meeting.
#[derive(Clone, Copy, PartialEq)]
pub struct RecentActions {
    list: Signal<Vec<RecentAction>>,
    limit: Signal<usize>,
}

impl RecentActions {
    fn record(mut self, action: RecentAction) {
        let (Ok(limit), Ok(first)) = (
            self.limit.try_peek().map(|limit| *limit),
            self.list.try_peek().map(|list| list.first().copied()),
        ) else {
            return;
        };
        if first == Some(action) {
            return;
        }
        if let Ok(mut list) = self.list.try_write() {
            let stored = load_json(LIST_KEY, serde_json::Value::Null);
            if !stored.is_null() {
                *list = sanitize(&stored, limit);
            }
            push_recent(&mut list, action, limit);
            save_list(&list);
        }
    }

    pub fn set_limit(mut self, limit: usize) {
        let limit = normalize_limit(limit as u64);
        self.limit.set(limit);
        save_json(LIMIT_KEY, &limit);
        let mut list = self.list.write();
        list.truncate(limit);
        save_list(&list);
    }
}

fn save_list(list: &[RecentAction]) {
    let slugs: Vec<String> = list.iter().map(|action| action.slug()).collect();
    save_json(LIST_KEY, &slugs);
}

pub fn use_recent_actions() -> RecentActions {
    let limit = use_signal(|| normalize_limit(load_json(LIMIT_KEY, DEFAULT_LIMIT as u64)));
    let list =
        use_signal(move || sanitize(&load_json(LIST_KEY, serde_json::Value::Null), *limit.peek()));
    use_context_provider(|| RecentActions { list, limit })
}

fn event_element(event: &web_sys::Event) -> Option<web_sys::Element> {
    event.target()?.dyn_into::<web_sys::Element>().ok()
}

fn control_ignores_activation(aria_disabled: Option<&str>, disabled: bool) -> bool {
    disabled || aria_disabled == Some("true")
}

fn recorded_action(target: &web_sys::Element) -> Option<RecentAction> {
    let control = target.closest("button,input,[role=option]").ok()??;
    if control_ignores_activation(
        control.get_attribute("aria-disabled").as_deref(),
        control.matches(":disabled").unwrap_or(false),
    ) {
        return None;
    }
    let owner = control.closest("[data-recent-action]").ok()??;
    RecentAction::from_slug(&owner.get_attribute("data-recent-action")?)
}

pub(crate) fn install_recent_actions_listeners(
    store: RecentActions,
    mut open: Signal<bool>,
    customize_mode: Signal<bool>,
) -> Vec<WindowEventListener> {
    let record = move |event: &web_sys::Event| {
        if customize_mode.try_peek().map(|on| *on).unwrap_or(true) {
            return;
        }
        if let Some(action) = event_element(event).and_then(|el| recorded_action(&el)) {
            store.record(action);
        }
    };
    vec![
        WindowEventListener::on_document("click", move |event| {
            let outside = event_element(&event)
                .is_none_or(|el| el.closest(MENU_SELECTOR).ok().flatten().is_none());
            if outside && open.try_peek().is_ok_and(|open| *open) {
                if let Ok(mut open) = open.try_write() {
                    *open = false;
                }
            }
            record(&event);
        }),
        WindowEventListener::on_document("change", move |event| record(&event)),
        WindowEventListener::on_document("keydown", move |event| {
            let enter = event
                .dyn_ref::<web_sys::KeyboardEvent>()
                .is_some_and(|key| key.key() == "Enter");
            let on_option = enter
                && event_element(&event)
                    .is_some_and(|el| el.closest("[role=option]").ok().flatten().is_some());
            if on_option {
                record(&event);
            }
        }),
    ]
}

fn setting_available(target: SettingsTarget, ios: bool, avatar_editable: bool) -> bool {
    match target {
        SettingsTarget::Speaker => !ios,
        SettingsTarget::Avatar => avatar_editable,
        _ => true,
    }
}

fn toggled(
    slot: ActionBarSlot,
    hand_raised: bool,
    captions: Option<CaptionsButtonState>,
) -> Option<bool> {
    match slot {
        ActionBarSlot::RaiseHand => Some(hand_raised),
        ActionBarSlot::Captions => captions.map(|state| state.pressed),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuStep {
    First,
    Last,
    Next,
    Prev,
}

pub fn menu_index(current: Option<usize>, len: usize, step: MenuStep) -> Option<usize> {
    let last = len.checked_sub(1)?;
    Some(match (step, current) {
        (MenuStep::First, _) | (MenuStep::Next, None) => 0,
        (MenuStep::Last, _) | (MenuStep::Prev, None) => last,
        (MenuStep::Next, Some(i)) if i >= last => 0,
        (MenuStep::Next, Some(i)) => i + 1,
        (MenuStep::Prev, Some(0)) => last,
        (MenuStep::Prev, Some(i)) => i - 1,
    })
}

/// What an arrow on the trigger asks for: where to focus, and whether to open on the
/// last item. Nothing while another surface owns the arrows.
pub fn trigger_arrow(key: &Key, surface_owns_arrows: bool) -> Option<(MenuStep, bool)> {
    match key {
        _ if surface_owns_arrows => None,
        Key::ArrowDown => Some((MenuStep::First, false)),
        Key::ArrowUp => Some((MenuStep::Last, true)),
        _ => None,
    }
}

pub fn focus_menu_item(step: MenuStep) {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let selector = format!("#{POPOVER_ID} [role^=\"menuitem\"]");
    let Ok(nodes) = doc.query_selector_all(&selector) else {
        return;
    };
    let items: Vec<web_sys::HtmlElement> = (0..nodes.length())
        .filter_map(|i| nodes.item(i))
        .filter_map(|node| node.dyn_into().ok())
        .collect();
    let active = doc.active_element();
    let current = items.iter().position(|item| {
        active
            .as_ref()
            .is_some_and(|el| el.is_same_node(Some(item.as_ref())))
    });
    if let Some(i) = menu_index(current, items.len(), step) {
        let _ = items[i].focus();
    }
}

pub fn focus_settings_dialog() {
    focus_first_taker(&[&format!("#{SETTINGS_DIALOG_ID}")]);
}

pub fn focus_recents_return() {
    focus_slot_trigger(RECENT_ACTIONS_TRIGGER_ID);
    blur_clipped_focus();
}

fn reveal_focus_order(group: &str) -> [String; 2] {
    [
        format!("{group} [aria-checked=\"true\"], {group} .theme-icon-button--active"),
        format!(
            "{group} button:not([disabled]):not([aria-disabled=\"true\"]), \
             {group} input:not([disabled])"
        ),
    ]
}

/// Scrolls `target` into view in the open Settings modal, focuses its current
/// value and marks it. Nothing is opened or changed.
pub fn reveal_settings_target(target: SettingsTarget) {
    reveal_attempt(target, REVEAL_RETRIES);
}

fn reveal_attempt(target: SettingsTarget, retries: u8) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let dialog = format!("#{SETTINGS_DIALOG_ID}");
    let group_selector = format!("{dialog} [data-recent-action=\"{}\"]", target.slug());
    let group = window
        .document()
        .and_then(|doc| doc.query_selector(&group_selector).ok().flatten());
    match group {
        Some(group) => {
            scroll_to_center(&group);
            let [current, any_control] = reveal_focus_order(&group_selector);
            focus_first_taker(&[&current, &any_control, &dialog]);
            let _ = group.class_list().add_1(FLASH_CLASS);
            Timeout::new(FLASH_MS, move || {
                let _ = group.class_list().remove_1(FLASH_CLASS);
            })
            .forget();
        }
        None if retries > 0 => {
            let retry = Closure::once_into_js(move |_: f64| reveal_attempt(target, retries - 1));
            let _ = window.request_animation_frame(retry.unchecked_ref());
        }
        None => focus_settings_dialog(),
    }
}

fn scroll_to_center(element: &web_sys::Element) {
    let options = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&options, &"block".into(), &"center".into());
    if let Ok(scroll) = js_sys::Reflect::get(element, &"scrollIntoView".into())
        .and_then(|f| f.dyn_into::<js_sys::Function>())
    {
        let _ = scroll.call1(element, &options);
    }
}

#[component]
pub fn RecentActionsMenu(
    store: RecentActions,
    available_slots: Vec<ActionBarSlot>,
    record_state: RecordButtonState,
    hand_raised: bool,
    captions: Option<CaptionsButtonState>,
    share_state: ShareButtonState,
    #[props(default)] open_on_last: bool,
    on_close: EventHandler<()>,
    on_slot: EventHandler<ActionBarSlot>,
    on_setting: EventHandler<SettingsTarget>,
    on_share_start: EventHandler<()>,
    on_share_replace: EventHandler<()>,
    on_share_stop: EventHandler<()>,
) -> Element {
    let (ios, avatar_editable) = (is_ios(), avatar_editing_allowed());
    let entries: Vec<RecentAction> = store
        .list
        .read()
        .iter()
        .copied()
        .filter(|action| match action {
            RecentAction::Slot(slot) => available_slots.contains(slot),
            RecentAction::Setting(target) => setting_available(*target, ios, avatar_editable),
        })
        .collect();
    let activated = move || {
        on_close.call(());
        settle_slot_focus(RECENT_ACTIONS_TRIGGER_ID);
    };
    let shown = entries.len();
    use_effect(use_reactive!(|shown| {
        let _ = shown;
        if focus_was_dropped() {
            focus_menu_item(MenuStep::First);
        }
    }));
    rsx! {
        div {
            id: POPOVER_ID,
            class: "action-bar-overflow-popover recent-actions-popover",
            onclick: move |e: MouseEvent| e.stop_propagation(),
            onmousedown: move |e: MouseEvent| e.prevent_default(),
            onkeydown: move |evt: KeyboardEvent| {
                let step = match evt.key() {
                    Key::ArrowDown => MenuStep::Next,
                    Key::ArrowUp => MenuStep::Prev,
                    Key::Home => MenuStep::First,
                    Key::End => MenuStep::Last,
                    Key::Escape | Key::Tab => {
                        evt.prevent_default();
                        evt.stop_propagation();
                        on_close.call(());
                        focus_recents_return();
                        return;
                    }
                    _ => return,
                };
                evt.prevent_default();
                evt.stop_propagation();
                focus_menu_item(step);
            },
            div { id: HEADING_ID, class: "recent-actions-heading", "Recent actions" }
            div {
                role: "menu",
                "aria-labelledby": HEADING_ID,
                onmounted: move |_| {
                    focus_menu_item(if open_on_last { MenuStep::Last } else { MenuStep::First });
                },
                if entries.is_empty() {
                    button {
                        r#type: "button",
                        class: "overflow-item recent-actions-empty",
                        role: "menuitem",
                        "aria-disabled": "true",
                        "data-testid": "recent-actions-empty",
                        span { class: "recent-actions-item-text",
                            span { "No recent actions yet" }
                            span { class: "recent-actions-item-hint",
                                "Features and settings you use will show up here."
                            }
                        }
                    }
                }
                for action in entries {
                    div { key: "{action.slug()}", class: "recent-actions-row", role: "none",
                        match action {
                            RecentAction::Slot(ActionBarSlot::ScreenShare) => rsx! {
                                OverflowShareItems {
                                    state: share_state,
                                    icon: overflow_slot_icon(ActionBarSlot::ScreenShare),
                                    item_role: Some("menuitem"),
                                    focus_on_done: None,
                                    on_done: move |_| activated(),
                                    on_start: on_share_start,
                                    on_replace: on_share_replace,
                                    on_stop: on_share_stop,
                                }
                            },
                            RecentAction::Slot(slot) => {
                                let item = slot_item(slot, &record_state, captions);
                                let checked = toggled(slot, hand_raised, captions);
                                let hint = captions
                                    .filter(|_| slot == ActionBarSlot::Captions)
                                    .map(captions_button_hint);
                                rsx! {
                                    button {
                                        r#type: "button",
                                        class: "overflow-item",
                                        role: if checked.is_some() { "menuitemcheckbox" } else { "menuitem" },
                                        "data-recent-action": action.slug(),
                                        "aria-checked": checked.map(|on| if on { "true" } else { "false" }),
                                        "aria-disabled": item.disabled.then_some("true"),
                                        "aria-busy": item.busy.then_some("true"),
                                        title: hint,
                                        "aria-description": hint,
                                        onclick: move |_| {
                                            if !item.disabled {
                                                activated();
                                                on_slot.call(slot);
                                            }
                                        },
                                        {live_slot_icon(slot, &record_state)}
                                        span { "{item.label}" }
                                    }
                                }
                            }
                            RecentAction::Setting(target) => rsx! {
                                button {
                                    r#type: "button",
                                    class: "overflow-item",
                                    role: "menuitem",
                                    "data-recent-action": target.slug(),
                                    onclick: move |_| {
                                        activated();
                                        on_setting.call(target);
                                    },
                                    {overflow_slot_icon(ActionBarSlot::DeviceSettings)}
                                    span { class: "recent-actions-item-text",
                                        span { "{target.label()}" }
                                        span { class: "recent-actions-item-hint",
                                            "Settings \u{b7} {target.section_title()}"
                                        }
                                    }
                                }
                            },
                        }
                    }
                }
            }
        }
    }
}

/// Preferences > Action Bar control. Renders nothing outside a meeting.
#[component]
pub fn RecentActionsLimitControl() -> Element {
    let Some(store) = try_use_context::<RecentActions>() else {
        return rsx! {};
    };
    let limit = (store.limit)();
    rsx! {
        div { class: "device-setting-group",
            span { class: "transport-segmented-label", "Recent actions to remember" }
            div {
                class: "transport-segmented",
                role: "radiogroup",
                "aria-label": "Number of recent actions to remember",
                "aria-describedby": LIMIT_HINT_ID,
                for option in LIMIT_OPTIONS {
                    button {
                        r#type: "button",
                        role: "radio",
                        "data-testid": "recent-actions-limit-{option}",
                        "aria-checked": if limit == option { "true" } else { "false" },
                        "aria-label": "{option} actions",
                        class: if limit == option { "transport-segmented-option selected" } else { "transport-segmented-option" },
                        onclick: move |_| store.set_limit(option),
                        "{option}"
                    }
                }
            }
            p { id: LIMIT_HINT_ID, class: "appearance-section-helper",
                "Saved on this device. Lowering the number removes the oldest."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::action_bar_layout::ALL_SLOTS;
    use crate::components::captions::CaptionsRole;
    use serde_json::json;

    const PEERS: RecentAction = RecentAction::Slot(ActionBarSlot::PeerList);
    const DENSITY_SLOT: RecentAction = RecentAction::Slot(ActionBarSlot::DensityMode);
    const DENSITY_SETTING: RecentAction = RecentAction::Setting(SettingsTarget::Density);

    #[test]
    fn push_moves_to_the_front_without_duplicating_and_caps_at_the_limit() {
        let mut list = vec![PEERS, DENSITY_SLOT];
        push_recent(&mut list, DENSITY_SLOT, 5);
        assert_eq!(
            list,
            [DENSITY_SLOT, PEERS],
            "under the cap, so no room to hide a duplicate"
        );
        let mut list = vec![PEERS, DENSITY_SLOT, DENSITY_SETTING];
        push_recent(&mut list, DENSITY_SETTING, 3);
        assert_eq!(list, [DENSITY_SETTING, PEERS, DENSITY_SLOT]);
        let theme = RecentAction::Setting(SettingsTarget::Theme);
        push_recent(&mut list, theme, 3);
        assert_eq!(list, [theme, DENSITY_SETTING, PEERS]);
        push_recent(&mut list, PEERS, 2);
        assert_eq!(list, [PEERS, theme]);
    }

    #[test]
    fn sanitize_drops_tampered_entries_dedupes_and_caps() {
        let raw = json!([
            "slot:participants",
            "slot:mic",
            7,
            null,
            "slot:recent_actions",
            "setting:nope",
            "participants",
            "slot:participants",
            "setting:density",
            "slot:density",
            "slot:diagnostics"
        ]);
        assert_eq!(
            sanitize(&raw, 3),
            [PEERS, DENSITY_SETTING, DENSITY_SLOT],
            "one bad entry must not take the rest of the list with it"
        );
        for not_a_list in [
            json!(null),
            json!("slot:participants"),
            json!({"a": 1}),
            json!(5),
        ] {
            assert_eq!(
                sanitize(&not_a_list, 5),
                Vec::<RecentAction>::new(),
                "{not_a_list}"
            );
        }
    }

    #[test]
    fn limit_is_one_of_the_offered_values_or_the_default() {
        for option in LIMIT_OPTIONS {
            assert_eq!(normalize_limit(option as u64), option);
        }
        for tampered in [0, 1, 4, 6, 11, 5000, u64::MAX] {
            assert_eq!(normalize_limit(tampered), DEFAULT_LIMIT, "{tampered}");
        }
        assert_eq!(DEFAULT_LIMIT, 5);
    }

    #[test]
    fn pinned_slots_and_the_menu_itself_are_never_recorded() {
        for slot in [
            ActionBarSlot::Mic,
            ActionBarSlot::Camera,
            ActionBarSlot::RecentActions,
        ] {
            assert_eq!(
                RecentAction::from_slug(&format!("slot:{}", slot.slug())),
                None
            );
            assert_eq!(slot_attr(slot), None);
        }
    }

    #[test]
    fn every_action_round_trips_through_a_unique_prefixed_slug() {
        let mut slugs = Vec::new();
        for slot in ALL_SLOTS.iter().copied().filter(|s| is_recordable(*s)) {
            let action = RecentAction::Slot(slot);
            assert_eq!(RecentAction::from_slug(&action.slug()), Some(action));
            assert_eq!(slot_attr(slot), Some(action.slug()));
            slugs.push(action.slug());
        }
        for target in SettingsTarget::ALL {
            let action = RecentAction::Setting(*target);
            assert_eq!(RecentAction::from_slug(&action.slug()), Some(action));
            slugs.push(action.slug());
        }
        assert_ne!(DENSITY_SLOT.slug(), DENSITY_SETTING.slug());
        let count = slugs.len();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), count);
    }

    #[test]
    fn only_toggle_slots_report_a_checked_state() {
        let captions = CaptionsButtonState {
            role: CaptionsRole::Viewer,
            pressed: true,
            disabled: false,
            pending: false,
        };
        assert_eq!(toggled(ActionBarSlot::RaiseHand, true, None), Some(true));
        assert_eq!(toggled(ActionBarSlot::RaiseHand, false, None), Some(false));
        assert_eq!(
            toggled(ActionBarSlot::Captions, false, Some(captions)),
            Some(true)
        );
        assert_eq!(toggled(ActionBarSlot::PeerList, true, Some(captions)), None);
    }

    #[test]
    fn a_control_that_ignores_activation_is_not_recorded() {
        assert!(!control_ignores_activation(None, false));
        assert!(!control_ignores_activation(Some("false"), false));
        assert!(control_ignores_activation(Some("true"), false));
        assert!(control_ignores_activation(None, true));
        let flat: String = include_str!("recent_actions.rs")
            .split_whitespace()
            .collect();
        assert!(flat.contains(concat!(
            "control.get_attribute(\"aria-disabled\").as_deref(),",
            "control.matches(\":disabled\").unwrap_or(false),"
        )));
    }

    #[test]
    fn a_reveal_lands_on_the_current_value_and_never_on_a_pressed_swatch() {
        let [current, _] = reveal_focus_order("#group");
        assert!(current.contains("#group [aria-checked=\"true\"]"));
        assert!(current.contains("#group .theme-icon-button--active"));
        assert!(!current.contains("aria-pressed"));
        let flat: String = include_str!("recent_actions.rs")
            .split_whitespace()
            .collect();
        assert!(flat.contains(concat!(
            "let[current,any_control]=reveal_focus_order(&group_selector);",
            "focus_first_taker(&[&current,&any_control,&dialog]);"
        )));
    }

    #[test]
    fn the_help_page_names_the_limit() {
        assert!(include_str!("../pages/help.rs").contains("how many recent actions"));
    }

    #[test]
    fn auto_shrink_is_a_target_that_opens_preferences_on_every_device() {
        let target = SettingsTarget::AutoShrink;
        assert_eq!(
            RecentAction::from_slug("setting:auto_shrink"),
            Some(RecentAction::Setting(target))
        );
        assert_eq!(
            (target.label(), target.section()),
            ("Auto-shrink", "preferences")
        );
        assert!(setting_available(target, true, false));
    }

    #[test]
    fn the_recents_return_obeys_the_clipped_focus_rule() {
        let flat: String = include_str!("recent_actions.rs")
            .split_whitespace()
            .collect();
        assert!(flat.contains(concat!(
            "focus_slot_trigger(RECENT_ACTIONS_TRIGGER_ID);",
            "blur_clipped_focus();"
        )));
    }

    #[test]
    fn every_target_has_an_anchor_in_its_panel() {
        let panels: String = [
            include_str!("device_settings_modal.rs"),
            include_str!("preferences_settings_panel.rs"),
            include_str!("appearance_settings_panel.rs"),
        ]
        .concat();
        for target in SettingsTarget::ALL {
            let anchor = format!("\"data-recent-action\": SettingsTarget::{target:?}.slug()");
            assert!(panels.contains(&anchor), "{target:?}");
        }
    }

    #[test]
    fn settings_targets_are_listed_only_where_the_control_exists() {
        for target in SettingsTarget::ALL {
            assert!(setting_available(*target, false, true), "{target:?}");
        }
        assert!(!setting_available(SettingsTarget::Speaker, true, true));
        assert!(setting_available(SettingsTarget::Microphone, true, true));
        assert!(!setting_available(SettingsTarget::Avatar, false, false));
        assert!(setting_available(SettingsTarget::Speaker, false, false));
    }

    #[test]
    fn trigger_arrows_open_the_menu_unless_another_surface_owns_them() {
        assert_eq!(
            trigger_arrow(&Key::ArrowDown, false),
            Some((MenuStep::First, false))
        );
        assert_eq!(
            trigger_arrow(&Key::ArrowUp, false),
            Some((MenuStep::Last, true))
        );
        assert_eq!(trigger_arrow(&Key::ArrowDown, true), None);
        assert_eq!(trigger_arrow(&Key::ArrowUp, true), None);
        assert_eq!(trigger_arrow(&Key::Enter, false), None);
    }

    #[test]
    fn forced_colors_keep_the_checked_state_and_the_arrival_cue() {
        let css: String = include_str!("../../static/global.css")
            .split_whitespace()
            .collect();
        let (_, block) = css
            .split_once("@media(forced-colors:active){.action-bar-overflow-popover.overflow-item")
            .expect("the forced-colors block");
        let block = block.split("}}").next().unwrap();
        assert!(
            block.contains("[aria-checked=\"true\"]:not(:focus-visible)"),
            "{block}"
        );
        assert!(block.contains(".recent-target-flash::after"), "{block}");
        assert!(block.contains("outline:2pxsolidHighlight"), "{block}");
    }

    #[test]
    fn menu_steps_wrap_and_an_empty_menu_has_no_target() {
        use MenuStep::*;
        assert_eq!(menu_index(Some(2), 3, Next), Some(0));
        assert_eq!(menu_index(Some(0), 3, Next), Some(1));
        assert_eq!(menu_index(Some(0), 3, Prev), Some(2));
        assert_eq!(menu_index(Some(2), 3, Prev), Some(1));
        assert_eq!(menu_index(Some(1), 3, First), Some(0));
        assert_eq!(menu_index(Some(1), 3, Last), Some(2));
        assert_eq!(menu_index(None, 3, Next), Some(0));
        assert_eq!(menu_index(None, 3, Prev), Some(2));
        assert_eq!(menu_index(None, 0, First), None);
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use gloo_timers::future::TimeoutFuture;
    use std::cell::RefCell;
    use wasm_bindgen_test::wasm_bindgen_test;

    thread_local! {
        static SLOTS: RefCell<Vec<ActionBarSlot>> = const { RefCell::new(Vec::new()) };
    }

    #[allow(non_snake_case)]
    fn Harness() -> Element {
        let store = use_recent_actions();
        let mut open = use_signal(|| true);
        let mut activated = use_signal(String::new);
        let mut slots = use_signal(|| SLOTS.with(|s| s.borrow().clone()));
        rsx! {
            div { id: "grid-container", tabindex: "-1" }
            button { id: RECENT_ACTIONS_TRIGGER_ID }
            button { id: "outside" }
            button { id: "drop-first-slot", onclick: move |_| { slots.write().remove(0); } }
            span { id: "activated", "{activated}" }
            RecentActionsLimitControl {}
            if open() {
                RecentActionsMenu {
                    store,
                    available_slots: slots(),
                    record_state: RecordButtonState::Recording,
                    hand_raised: false,
                    captions: None,
                    share_state: ShareButtonState::Idle,
                    on_close: move |_| open.set(false),
                    on_slot: move |slot: ActionBarSlot| activated.set(slot.slug().to_string()),
                    on_setting: move |target: SettingsTarget| activated.set(target.slug().to_string()),
                    on_share_start: |_| {},
                    on_share_replace: |_| {},
                    on_share_stop: |_| {},
                }
            }
        }
    }

    async fn mount(list: &[&str], slots: &[ActionBarSlot], limit: u64) -> web_sys::Element {
        save_json(LIST_KEY, &list);
        save_json(LIMIT_KEY, &limit);
        SLOTS.with(|s| *s.borrow_mut() = slots.to_vec());
        let doc = gloo_utils::document();
        let root = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&root).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(Harness),
            dioxus::web::Config::new().rootelement(root.clone()),
        );
        TimeoutFuture::new(50).await;
        root
    }

    fn press(key: &str) {
        let active = gloo_utils::document().active_element().unwrap();
        js_sys::Function::new_with_args(
            "el, key",
            "el.dispatchEvent(new KeyboardEvent('keydown', \
             { key, bubbles: true, cancelable: true }));",
        )
        .call2(&wasm_bindgen::JsValue::NULL, &active, &key.into())
        .unwrap();
    }

    fn items(root: &web_sys::Element) -> Vec<String> {
        let nodes = root
            .query_selector_all("[role^=\"menuitem\"][data-recent-action]")
            .unwrap();
        (0..nodes.length())
            .map(|i| {
                nodes
                    .item(i)
                    .unwrap()
                    .unchecked_into::<web_sys::Element>()
                    .get_attribute("data-recent-action")
                    .unwrap()
            })
            .collect()
    }

    fn focused_action() -> Option<String> {
        gloo_utils::document()
            .active_element()?
            .get_attribute("data-recent-action")
    }

    fn html(root: &web_sys::Element, selector: &str) -> web_sys::HtmlElement {
        root.query_selector(selector)
            .unwrap()
            .unwrap_or_else(|| panic!("{selector}"))
            .unchecked_into()
    }

    #[wasm_bindgen_test]
    async fn the_keyboard_walks_a_flat_list_that_wraps_and_closing_returns_to_the_trigger() {
        let list = ["slot:participants", "setting:theme", "slot:diagnostics"];
        let root = mount(
            &list,
            &[ActionBarSlot::PeerList, ActionBarSlot::Diagnostics],
            5,
        )
        .await;
        assert_eq!(
            focused_action().as_deref(),
            Some("slot:participants"),
            "opens on the first"
        );
        press("ArrowUp");
        assert_eq!(
            focused_action().as_deref(),
            Some("slot:diagnostics"),
            "wraps up"
        );
        press("ArrowDown");
        assert_eq!(
            focused_action().as_deref(),
            Some("slot:participants"),
            "wraps down"
        );
        press("ArrowDown");
        assert_eq!(focused_action().as_deref(), Some("setting:theme"));
        press("Home");
        assert_eq!(focused_action().as_deref(), Some("slot:participants"));
        press("End");
        assert_eq!(focused_action().as_deref(), Some("slot:diagnostics"));
        let prevented = js_sys::Function::new_with_args(
            "el",
            "return !el.dispatchEvent(new MouseEvent('mousedown', { bubbles: true, cancelable: true }));",
        )
        .call1(&wasm_bindgen::JsValue::NULL, &html(&root, &format!("#{HEADING_ID}")))
        .unwrap();
        assert_eq!(
            prevented.as_bool(),
            Some(true),
            "a press inside keeps focus in the menu"
        );
        press("Escape");
        TimeoutFuture::new(50).await;
        assert!(root
            .query_selector(&format!("#{POPOVER_ID}"))
            .unwrap()
            .is_none());
        assert_eq!(
            gloo_utils::document().active_element().unwrap().id(),
            RECENT_ACTIONS_TRIGGER_ID
        );
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn an_empty_or_fully_unavailable_list_shows_one_focused_disabled_item() {
        for list in [&[][..], &["slot:participants"][..]] {
            let root = mount(list, &[ActionBarSlot::Diagnostics], 5).await;
            let empty = html(&root, "[data-testid='recent-actions-empty']");
            assert_eq!(empty.get_attribute("role").as_deref(), Some("menuitem"));
            assert_eq!(
                empty.get_attribute("aria-disabled").as_deref(),
                Some("true")
            );
            assert_eq!(items(&root), Vec::<String>::new());
            assert_eq!(
                gloo_utils::document().active_element().as_ref(),
                Some(empty.unchecked_ref::<web_sys::Element>()),
            );
            root.remove();
        }
    }

    #[wasm_bindgen_test]
    async fn focus_stays_in_the_menu_when_the_focused_entry_goes_away() {
        let slots = [ActionBarSlot::PeerList, ActionBarSlot::Diagnostics];
        let root = mount(&["slot:participants", "slot:diagnostics"], &slots, 5).await;
        assert_eq!(focused_action().as_deref(), Some("slot:participants"));
        html(&root, "#drop-first-slot").click();
        TimeoutFuture::new(50).await;
        assert_eq!(items(&root), ["slot:diagnostics"]);
        assert_eq!(focused_action().as_deref(), Some("slot:diagnostics"));
        html(&root, "#drop-first-slot").click();
        TimeoutFuture::new(50).await;
        let empty = html(&root, "[data-testid='recent-actions-empty']");
        assert_eq!(
            gloo_utils::document().active_element().as_ref(),
            Some(empty.unchecked_ref::<web_sys::Element>()),
        );
        root.remove();
    }

    thread_local! {
        static MODAL_TARGET: std::cell::Cell<SettingsTarget> =
            const { std::cell::Cell::new(SettingsTarget::Microphone) };
    }

    #[allow(non_snake_case)]
    fn ModalHarness() -> Element {
        let theme = use_signal(|| crate::context::Theme::System);
        use_context_provider(|| crate::context::ThemePreferenceCtx(theme));
        let appearance = use_signal(crate::context::AppearanceSettings::default);
        use_context_provider(|| crate::context::AppearanceSettingsCtx(appearance));
        rsx! {
            crate::components::device_settings_modal::DeviceSettingsModal {
                microphones: Vec::new(),
                cameras: Vec::new(),
                speakers: Vec::new(),
                selected_microphone_id: None,
                selected_camera_id: None,
                selected_speaker_id: None,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
                visible: true,
                on_close: move |_| {},
                initial_section: Some(MODAL_TARGET.with(|t| t.get()).section().to_string()),
            }
        }
    }

    #[wasm_bindgen_test]
    async fn every_target_renders_in_the_section_its_entry_opens() {
        for target in SettingsTarget::ALL {
            MODAL_TARGET.with(|t| t.set(*target));
            let doc = gloo_utils::document();
            let root = doc.create_element("div").unwrap();
            doc.body().unwrap().append_child(&root).unwrap();
            dioxus::web::launch::launch_virtual_dom(
                VirtualDom::new(ModalHarness),
                dioxus::web::Config::new().rootelement(root.clone()),
            );
            TimeoutFuture::new(50).await;
            let anchor = format!(
                "#{SETTINGS_DIALOG_ID} [data-recent-action='{}']",
                target.slug()
            );
            assert!(
                root.query_selector(&anchor).unwrap().is_some(),
                "{target:?}"
            );
            root.remove();
        }
    }

    #[wasm_bindgen_test]
    async fn an_unavailable_entry_is_hidden_but_stays_stored() {
        let root = mount(
            &["slot:meeting_timer", "slot:participants"],
            &[ActionBarSlot::PeerList],
            5,
        )
        .await;
        assert_eq!(items(&root), ["slot:participants"]);
        assert_eq!(
            load_json::<Vec<String>>(LIST_KEY, Vec::new()),
            ["slot:meeting_timer", "slot:participants"]
        );
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn lowering_the_limit_removes_the_oldest_from_the_menu_and_from_storage() {
        let list = [
            "slot:participants",
            "slot:diagnostics",
            "setting:theme",
            "setting:camera",
            "setting:density",
        ];
        let root = mount(
            &list,
            &[ActionBarSlot::PeerList, ActionBarSlot::Diagnostics],
            5,
        )
        .await;
        assert_eq!(items(&root).len(), 5);
        let selected = html(&root, "[data-testid='recent-actions-limit-5']");
        assert_eq!(
            selected.get_attribute("aria-checked").as_deref(),
            Some("true")
        );
        html(&root, "[data-testid='recent-actions-limit-3']").click();
        TimeoutFuture::new(50).await;
        assert_eq!(
            items(&root),
            ["slot:participants", "slot:diagnostics", "setting:theme"]
        );
        assert_eq!(load_json::<u64>(LIMIT_KEY, 0), 3);
        assert_eq!(
            load_json::<Vec<String>>(LIST_KEY, Vec::new()),
            ["slot:participants", "slot:diagnostics", "setting:theme"]
        );
        assert_eq!(
            html(&root, "[role='radiogroup']")
                .get_attribute("aria-describedby")
                .as_deref(),
            Some(LIMIT_HINT_ID)
        );
        assert!(root
            .query_selector(&format!("#{LIMIT_HINT_ID}"))
            .unwrap()
            .is_some());
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_recording_entry_reads_stop_and_activating_an_entry_closes_the_menu() {
        let root = mount(&["slot:recording"], &[ActionBarSlot::Recording], 5).await;
        let item = html(&root, "[data-recent-action='slot:recording']");
        assert_eq!(item.text_content().as_deref(), Some("Stop recording"));
        item.click();
        TimeoutFuture::new(50).await;
        assert_eq!(
            html(&root, "#activated").text_content().as_deref(),
            Some("recording")
        );
        assert!(root
            .query_selector(&format!("#{POPOVER_ID}"))
            .unwrap()
            .is_none());
        root.remove();
    }

    #[component]
    fn ListenerHost(open: Signal<bool>, customize: Signal<bool>) -> Element {
        let store = use_recent_actions();
        use_hook(|| std::rc::Rc::new(install_recent_actions_listeners(store, open, customize)));
        rsx! {}
    }

    #[allow(non_snake_case)]
    fn ListenerHarness() -> Element {
        let open = use_signal(|| true);
        let mut customize = use_signal(|| false);
        let mut host = use_signal(|| true);
        rsx! {
            span { id: "open", "{open}" }
            button { id: "customize", onclick: move |_| customize.set(true) }
            button { id: "unmount", onclick: move |_| host.set(false) }
            if host() {
                ListenerHost { open, customize }
            }
            div { "data-recent-action": "slot:participants",
                id: "wrapper",
                button { id: "plain", span { id: "inner" } }
                button { id: "off", "aria-disabled": "true" }
                button { id: "native-off", disabled: true, span { id: "native-off-inner" } }
                span { id: "help", role: "button" }
            }
            div { "data-recent-action": "setting:camera",
                div { id: "option", role: "option" }
                input { id: "field", r#type: "range" }
            }
            div { "data-recent-action": "slot:mic", button { id: "mic" } }
            button { id: "unmarked" }
        }
    }

    fn stored() -> Vec<String> {
        load_json(LIST_KEY, Vec::new())
    }

    #[wasm_bindgen_test]
    async fn the_listener_records_controls_only_dismisses_outside_and_ends_on_unmount() {
        save_json(LIST_KEY, &Vec::<String>::new());
        save_json(LIMIT_KEY, &5u64);
        let doc = gloo_utils::document();
        let root = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&root).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(ListenerHarness),
            dioxus::web::Config::new().rootelement(root.clone()),
        );
        TimeoutFuture::new(50).await;

        for ignored in [
            "#wrapper",
            "#off",
            "#native-off-inner",
            "#help",
            "#mic",
            "#unmarked",
        ] {
            html(&root, ignored).click();
            assert_eq!(stored(), Vec::<String>::new(), "{ignored}");
        }
        TimeoutFuture::new(30).await;
        assert_eq!(
            html(&root, "#open").text_content().as_deref(),
            Some("false"),
            "a click outside the menu and trigger dismisses it"
        );

        html(&root, "#inner").click();
        assert_eq!(
            stored(),
            ["slot:participants"],
            "a child of the button counts"
        );
        html(&root, "#field")
            .dispatch_event(&web_sys::Event::new("change").unwrap())
            .unwrap();
        assert_eq!(stored(), ["setting:camera", "slot:participants"], "change");
        html(&root, "#plain").click();
        assert_eq!(stored(), ["slot:participants", "setting:camera"]);
        js_sys::Function::new_with_args(
            "el",
            "el.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));",
        )
        .call1(&wasm_bindgen::JsValue::NULL, &html(&root, "#option"))
        .unwrap();
        assert_eq!(
            stored(),
            ["setting:camera", "slot:participants"],
            "Enter on an option"
        );

        let swallow = WindowEventListener::with_event("click", true, |event| {
            event.stop_propagation();
        });
        html(&root, "#plain").click();
        assert_eq!(
            stored(),
            ["setting:camera", "slot:participants"],
            "a click swallowed at window capture never reached its control"
        );
        drop(swallow);

        js_sys::Function::new_with_args(
            "el",
            "el.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));",
        )
        .call1(&wasm_bindgen::JsValue::NULL, &html(&root, "#plain"))
        .unwrap();
        assert_eq!(
            stored(),
            ["setting:camera", "slot:participants"],
            "Enter on a button records from the click it causes"
        );

        save_json(LIST_KEY, &["setting:theme"]);
        html(&root, "#plain").click();
        assert_eq!(
            stored(),
            ["slot:participants", "setting:theme"],
            "another tab's entries survive"
        );

        html(&root, "#customize").click();
        TimeoutFuture::new(30).await;
        html(&root, "#field").click();
        assert_eq!(
            stored(),
            ["slot:participants", "setting:theme"],
            "customize mode"
        );

        html(&root, "#unmount").click();
        TimeoutFuture::new(30).await;
        html(&root, "#field").click();
        assert_eq!(
            stored(),
            ["slot:participants", "setting:theme"],
            "after unmount"
        );
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn revealing_a_target_focuses_its_current_value_and_marks_it_then_clears() {
        let doc = gloo_utils::document();
        let dialog = doc.create_element("div").unwrap();
        dialog.set_id(SETTINGS_DIALOG_ID);
        dialog.set_attribute("tabindex", "0").unwrap();
        dialog.set_inner_html(
            "<div data-recent-action='setting:density' id='group'>\
             <button id='first' role='radio' aria-checked='false'></button>\
             <button id='current' role='radio' aria-checked='true'></button></div>\
             <div data-recent-action='setting:theme'>\
             <button id='dark' aria-pressed='false'></button>\
             <button id='light' class='theme-icon-button--active'></button></div>",
        );
        doc.body().unwrap().append_child(&dialog).unwrap();
        reveal_settings_target(SettingsTarget::Density);
        assert_eq!(doc.active_element().unwrap().id(), "current");
        let group = doc.get_element_by_id("group").unwrap();
        assert!(group.class_list().contains(FLASH_CLASS));
        TimeoutFuture::new(FLASH_MS + 50).await;
        assert!(!group.class_list().contains(FLASH_CLASS));

        reveal_settings_target(SettingsTarget::Theme);
        assert_eq!(doc.active_element().unwrap().id(), "light");

        reveal_settings_target(SettingsTarget::Camera);
        let mut landed = false;
        for _ in 0..100 {
            TimeoutFuture::new(20).await;
            landed = doc.active_element().unwrap().id() == SETTINGS_DIALOG_ID;
            if landed {
                break;
            }
        }
        dialog.remove();
        assert!(
            landed,
            "a target that never renders falls back to the dialog"
        );
    }
}

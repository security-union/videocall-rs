/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */
use crate::context::{confirm_transport_change, TransportPreference};
use crate::types::DeviceInfo;
use dioxus::prelude::*;
use videocall_client::utils::is_ios;
use wasm_bindgen::JsCast;
use web_sys::MediaDeviceInfo;

// ── Reusable glass-styled select ──────────────────────────────────

fn focus_element_by_id(id: &str) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
    {
        if let Ok(html) = el.dyn_into::<web_sys::HtmlElement>() {
            let _ = html.focus();
        }
    }
}

fn focus_option_by_position(trigger_id: &str, last: bool) {
    if let Some(parent) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(trigger_id))
        .and_then(|el| el.parent_element())
    {
        if let Ok(nodes) = parent.query_selector_all(".glass-select-option") {
            let index = if last {
                nodes.length().saturating_sub(1)
            } else {
                0
            };
            if let Some(node) = nodes.item(index) {
                if let Ok(html) = node.dyn_into::<web_sys::HtmlElement>() {
                    let _ = html.focus();
                }
            }
        }
    }
}

fn close_dropdown_and_focus(
    mut open_dropdown: Signal<Option<&'static str>>,
    trigger_id: &'static str,
) {
    open_dropdown.set(None);
    focus_element_by_id(trigger_id);
}

fn click_target_is_within_glass_select(event: &MouseEvent) -> bool {
    event
        .data()
        .downcast::<web_sys::MouseEvent>()
        .and_then(|mouse_event| mouse_event.target())
        .and_then(|target: web_sys::EventTarget| target.dyn_into::<web_sys::Element>().ok())
        .and_then(|element: web_sys::Element| element.closest(".glass-select").ok().flatten())
        .is_some()
}

#[derive(Clone, PartialEq)]
struct GlassSelectOption {
    value: String,
    label: String,
}

#[component]
fn SettingsGlassSelect(
    id: &'static str,
    options: Vec<GlassSelectOption>,
    selected_value: String,
    on_change: EventHandler<String>,
    open_dropdown: Signal<Option<&'static str>>,
) -> Element {
    let mut open_dropdown = open_dropdown;
    let is_open = open_dropdown() == Some(id);

    let selected_label = options
        .iter()
        .find(|o| o.value == selected_value)
        .map(|o| o.label.clone())
        .unwrap_or_else(|| selected_value.clone());

    rsx! {
        div { class: if is_open { "glass-select open" } else { "glass-select" },
            button {
                id,
                class: if is_open { "glass-select-trigger open" } else { "glass-select-trigger" },
                style: if is_open { "position: relative; z-index: 1001;" } else { "" },
                r#type: "button",
                "aria-haspopup": "listbox",
                "aria-expanded": if is_open { "true" } else { "false" },
                onclick: move |e: MouseEvent| {
                    e.stop_propagation();
                    if is_open {
                        open_dropdown.set(None);
                    } else {
                        open_dropdown.set(Some(id));
                    }
                },
                onkeydown: move |evt: KeyboardEvent| {
                    match evt.key() {
                        Key::Escape if is_open => {
                            evt.stop_propagation();
                            open_dropdown.set(None);
                        }
                        Key::ArrowDown if is_open => {
                            evt.stop_propagation();
                            evt.prevent_default();
                            focus_option_by_position(id, false);
                        }
                        Key::ArrowUp if is_open => {
                            evt.stop_propagation();
                            evt.prevent_default();
                            focus_option_by_position(id, true);
                        }
                        Key::ArrowDown | Key::ArrowUp if !is_open => {
                            evt.stop_propagation();
                            open_dropdown.set(Some(id));
                        }
                        _ => {}
                    }
                },
                span { class: "glass-select-label", "{selected_label}" }
                svg {
                    class: "glass-select-chevron",
                    view_box: "0 0 12 8",
                    width: "12",
                    height: "8",
                    path {
                        d: "M1 1.5l5 5 5-5",
                        stroke: "currentColor",
                        stroke_width: "1.5",
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        fill: "none",
                    }
                }
            }
            if is_open {
                div {
                    class: "glass-select-menu",
                    role: "listbox",
                    onclick: move |e: MouseEvent| e.stop_propagation(),
                    for opt in options.iter() {
                        div {
                            class: if opt.value == selected_value { "glass-select-option selected" } else { "glass-select-option" },
                            role: "option",
                            "aria-selected": if opt.value == selected_value { "true" } else { "false" },
                            tabindex: "0",
                            onclick: {
                                let value = opt.value.clone();
                                move |e: MouseEvent| {
                                    e.stop_propagation();
                                    on_change.call(value.clone());
                                    close_dropdown_and_focus(open_dropdown, id);
                                }
                            },
                            onkeydown: {
                                let value = opt.value.clone();
                                move |evt: KeyboardEvent| {
                                    match evt.key() {
                                        Key::Enter => {
                                            evt.stop_propagation();
                                            evt.prevent_default();
                                            on_change.call(value.clone());
                                            close_dropdown_and_focus(open_dropdown, id);
                                        }
                                        Key::Escape => {
                                            evt.stop_propagation();
                                            close_dropdown_and_focus(open_dropdown, id);
                                        }
                                        Key::ArrowDown => {
                                            evt.stop_propagation();
                                            evt.prevent_default();
                                            if let Some(next) = web_sys::window()
                                                .and_then(|w| w.document())
                                                .and_then(|d| d.active_element())
                                                .and_then(|el| el.next_element_sibling())
                                            {
                                                if let Ok(html) = next.dyn_into::<web_sys::HtmlElement>() {
                                                    let _ = html.focus();
                                                }
                                            }
                                        }
                                        Key::ArrowUp => {
                                            evt.stop_propagation();
                                            evt.prevent_default();
                                            if let Some(prev) = web_sys::window()
                                                .and_then(|w| w.document())
                                                .and_then(|d| d.active_element())
                                                .and_then(|el| el.previous_element_sibling())
                                            {
                                                if let Ok(html) = prev.dyn_into::<web_sys::HtmlElement>() {
                                                    let _ = html.focus();
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            },
                            "{opt.label}"
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsSection {
    Audio,
    Video,
    Network,
    Appearance,
}

impl SettingsSection {
    fn title(self) -> &'static str {
        match self {
            SettingsSection::Audio => "Audio",
            SettingsSection::Video => "Video",
            SettingsSection::Network => "Network",
            SettingsSection::Appearance => "Appearance",
        }
    }

    fn tab_id(self) -> &'static str {
        match self {
            SettingsSection::Audio => "settings-tab-audio",
            SettingsSection::Video => "settings-tab-video",
            SettingsSection::Network => "settings-tab-network",
            SettingsSection::Appearance => "settings-tab-appearance",
        }
    }

    fn panel_id(self) -> &'static str {
        match self {
            SettingsSection::Audio => "settings-panel-audio",
            SettingsSection::Video => "settings-panel-video",
            SettingsSection::Network => "settings-panel-network",
            SettingsSection::Appearance => "settings-panel-appearance",
        }
    }

    fn test_id(self) -> &'static str {
        match self {
            SettingsSection::Audio => "settings-nav-audio",
            SettingsSection::Video => "settings-nav-video",
            SettingsSection::Network => "settings-nav-network",
            SettingsSection::Appearance => "settings-nav-appearance",
        }
    }

    fn all() -> [SettingsSection; 4] {
        [
            SettingsSection::Audio,
            SettingsSection::Video,
            SettingsSection::Network,
            SettingsSection::Appearance,
        ]
    }

    fn next(self) -> Self {
        match self {
            SettingsSection::Audio => SettingsSection::Video,
            SettingsSection::Video => SettingsSection::Network,
            SettingsSection::Network => SettingsSection::Appearance,
            SettingsSection::Appearance => SettingsSection::Audio,
        }
    }

    fn prev(self) -> Self {
        match self {
            SettingsSection::Audio => SettingsSection::Appearance,
            SettingsSection::Video => SettingsSection::Audio,
            SettingsSection::Network => SettingsSection::Video,
            SettingsSection::Appearance => SettingsSection::Network,
        }
    }
}

#[component]
pub fn DeviceSettingsModal(
    microphones: Vec<MediaDeviceInfo>,
    cameras: Vec<MediaDeviceInfo>,
    speakers: Vec<MediaDeviceInfo>,
    selected_microphone_id: Option<String>,
    selected_camera_id: Option<String>,
    selected_speaker_id: Option<String>,
    on_camera_select: EventHandler<DeviceInfo>,
    on_microphone_select: EventHandler<DeviceInfo>,
    on_speaker_select: EventHandler<DeviceInfo>,
    visible: bool,
    on_close: EventHandler<()>,
    #[props(default)] transport_preference: TransportPreference,
) -> Element {
    let is_ios_safari = is_ios();
    let mut active_section = use_signal(|| SettingsSection::Audio);
    let mut open_dropdown: Signal<Option<&'static str>> = use_signal(|| None);

    if !visible {
        return rsx! {};
    }

    rsx! {
        div {
            class: "device-settings-modal-overlay visible",
            onclick: move |_| on_close.call(()),

            div {
                id: "device-settings-dialog",
                class: "device-settings-modal settings-modal",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "device-settings-title",
                tabindex: "0",
                onclick: move |e: MouseEvent| {
                    e.stop_propagation();
                    if open_dropdown().is_some() && !click_target_is_within_glass_select(&e) {
                        open_dropdown.set(None);
                    }
                },

                onkeydown: move |evt| {
                    match evt.key() {
                        Key::Escape => {
                            evt.stop_propagation();
                            on_close.call(());
                        }
                        Key::ArrowDown | Key::ArrowRight => {
                            evt.stop_propagation();
                            active_section.set(active_section().next());
                        }
                        Key::ArrowUp | Key::ArrowLeft => {
                            evt.stop_propagation();
                            active_section.set(active_section().prev());
                        }
                        _ => {}
                    }
                },

                div { class: "device-settings-header settings-header",
                    h2 {
                        id: "device-settings-title",
                        "Settings"
                    }
                    button {
                        class: "close-button",
                        r#type: "button",
                        "aria-label": "Close settings",
                        onclick: move |_| on_close.call(()),

                        svg {
                            view_box: "0 0 24 24",
                            width: "18",
                            height: "18",

                            path {
                                d: "M6 6L18 18M18 6L6 18",
                                stroke: "currentColor",
                                stroke_width: "2",
                                stroke_linecap: "round"
                            }
                        }
                    }
                }

                div { class: "device-settings-content settings-layout",
                    div {
                        class: "settings-sidebar",
                        role: "tablist",
                        "aria-label": "Settings sections",

                        for section in SettingsSection::all() {
                            SettingsNavButton {
                                section,
                                active: active_section() == section,
                                onclick: move |_| {
                                    open_dropdown.set(None);
                                    active_section.set(section);
                                },
                            }
                        }
                    }

                    div { class: "settings-panel",
                        match active_section() {
                            SettingsSection::Audio => rsx! {
                                div {
                                    id: SettingsSection::Audio.panel_id(),
                                    class: "settings-section",
                                    role: "tabpanel",
                                    "aria-labelledby": SettingsSection::Audio.tab_id(),

                                    h3 { class: "settings-section-title", "Audio" }
                                    p { class: "settings-section-description",
                                        "Choose the microphone and speaker used during the call."
                                    }

                                    div { class: "device-setting-group",
                                        label { r#for: "modal-audio-select", "Microphone" }
                                        SettingsGlassSelect {
                                            id: "modal-audio-select",
                                            options: microphones.iter().map(|d| GlassSelectOption {
                                                value: d.device_id(),
                                                label: d.label(),
                                            }).collect::<Vec<_>>(),
                                            selected_value: selected_microphone_id.clone().unwrap_or_default(),
                                            on_change: {
                                                let microphones = microphones.clone();
                                                move |device_id: String| {
                                                    if let Some(info) = find_device_by_id(&microphones, &device_id) {
                                                        on_microphone_select.call(info);
                                                    }
                                                }
                                            },
                                            open_dropdown,
                                        }
                                    }

                                    if !is_ios_safari {
                                        div { class: "device-setting-group",
                                            label { r#for: "modal-speaker-select", "Speaker" }
                                            SettingsGlassSelect {
                                                id: "modal-speaker-select",
                                                options: speakers.iter().map(|d| GlassSelectOption {
                                                    value: d.device_id(),
                                                    label: d.label(),
                                                }).collect::<Vec<_>>(),
                                                selected_value: selected_speaker_id.clone().unwrap_or_default(),
                                                on_change: {
                                                    let speakers = speakers.clone();
                                                    move |device_id: String| {
                                                        if let Some(info) = find_device_by_id(&speakers, &device_id) {
                                                            on_speaker_select.call(info);
                                                        }
                                                    }
                                                },
                                                open_dropdown,
                                            }
                                        }
                                    } else {
                                        div { class: "device-setting-group",
                                            p {
                                                class: "ios-speaker-note",
                                                "Speaker selection is handled by your device settings on iOS/Safari."
                                            }
                                        }
                                    }
                                }
                            },
                            SettingsSection::Video => rsx! {
                                div {
                                    id: SettingsSection::Video.panel_id(),
                                    class: "settings-section",
                                    role: "tabpanel",
                                    "aria-labelledby": SettingsSection::Video.tab_id(),

                                    h3 { class: "settings-section-title", "Video" }
                                    p { class: "settings-section-description",
                                        "Choose the camera used during the call."
                                    }

                                    div { class: "device-setting-group",
                                        label { r#for: "modal-video-select", "Camera" }
                                        SettingsGlassSelect {
                                            id: "modal-video-select",
                                            options: cameras.iter().map(|d| GlassSelectOption {
                                                value: d.device_id(),
                                                label: d.label(),
                                            }).collect::<Vec<_>>(),
                                            selected_value: selected_camera_id.clone().unwrap_or_default(),
                                            open_dropdown,
                                            on_change: {
                                                let cameras = cameras.clone();
                                                move |device_id: String| {
                                                    if let Some(info) = find_device_by_id(&cameras, &device_id) {
                                                        on_camera_select.call(info);
                                                    }
                                                }
                                            },
                                        }
                                    }
                                }
                            },
                            SettingsSection::Network => rsx! {
                                div {
                                    id: SettingsSection::Network.panel_id(),
                                    class: "settings-section",
                                    role: "tabpanel",
                                    "aria-labelledby": SettingsSection::Network.tab_id(),

                                    h3 { class: "settings-section-title", "Network" }
                                    p { class: "settings-section-description",
                                        "Choose the transport protocol for media connections."
                                    }

                                    div { class: "device-setting-group",
                                        label { r#for: "modal-transport-select", "Protocol" }
                                        SettingsGlassSelect {
                                            id: "modal-transport-select",
                                            options: vec![
                                                GlassSelectOption { value: "auto".to_string(), label: "Auto".to_string() },
                                                GlassSelectOption { value: "webtransport".to_string(), label: "WebTransport".to_string() },
                                                GlassSelectOption { value: "websocket".to_string(), label: "WebSocket".to_string() },
                                            ],
                                            selected_value: match transport_preference {
                                                TransportPreference::Auto => "auto".to_string(),
                                                TransportPreference::WebTransportOnly => "webtransport".to_string(),
                                                TransportPreference::WebSocketOnly => "websocket".to_string(),
                                            },
                                            on_change: move |value: String| {
                                                confirm_transport_change(
                                                    &value,
                                                    transport_preference,
                                                    "modal-transport-select",
                                                );
                                            },
                                            open_dropdown,
                                        }
                                    }

                                    p { class: "transport-preference-note",
                                        "Changing protocol will reload the page."
                                    }
                                }
                            },
                            SettingsSection::Appearance => rsx! {
                                div {
                                    id: SettingsSection::Appearance.panel_id(),
                                    class: "settings-section",
                                    role: "tabpanel",
                                    "aria-labelledby": SettingsSection::Appearance.tab_id(),

                                    crate::components::appearance_settings_panel::AppearanceSettingsPanel {}
                                }
                            },
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn SettingsNavButton(
    section: SettingsSection,
    active: bool,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    let class = if active {
        "settings-nav-button active"
    } else {
        "settings-nav-button"
    };

    rsx! {
        button {
            id: section.tab_id(),
            class,
            r#type: "button",
            role: "tab",
            "aria-selected": if active { "true" } else { "false" },
            "aria-controls": section.panel_id(),
            "data-testid": section.test_id(),
            tabindex: if active { "0" } else { "-1" },
            onclick: move |evt| onclick.call(evt),
            "{section.title()}"
        }
    }
}

fn find_device_by_id(devices: &[MediaDeviceInfo], device_id: &str) -> Option<DeviceInfo> {
    devices
        .iter()
        .find(|device| device.device_id() == device_id)
        .map(DeviceInfo::from_media_device_info)
}

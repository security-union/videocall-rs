/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */
use crate::context::{apply_transport_decision, load_transport_sticky, TransportPreference};
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

// The `Performance` tab was removed in #1131: the Performance controls moved
// into the Diagnostics drawer. The tablist is now five tabs. (The transitional
// "moved to Diagnostics" redirect row was also removed; users find Performance
// in the drawer, and the "performance" deep link routes there via attendants.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SettingsSection {
    Audio,
    Video,
    Network,
    Appearance,
    Preferences,
}

impl SettingsSection {
    fn title(self) -> &'static str {
        match self {
            SettingsSection::Audio => "Audio",
            SettingsSection::Video => "Video",
            SettingsSection::Network => "Network",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Preferences => "Preferences",
        }
    }

    fn tab_id(self) -> &'static str {
        match self {
            SettingsSection::Audio => "settings-tab-audio",
            SettingsSection::Video => "settings-tab-video",
            SettingsSection::Network => "settings-tab-network",
            SettingsSection::Appearance => "settings-tab-appearance",
            SettingsSection::Preferences => "settings-tab-preferences",
        }
    }

    fn panel_id(self) -> &'static str {
        match self {
            SettingsSection::Audio => "settings-panel-audio",
            SettingsSection::Video => "settings-panel-video",
            SettingsSection::Network => "settings-panel-network",
            SettingsSection::Appearance => "settings-panel-appearance",
            SettingsSection::Preferences => "settings-panel-preferences",
        }
    }

    fn test_id(self) -> &'static str {
        match self {
            SettingsSection::Audio => "settings-nav-audio",
            SettingsSection::Video => "settings-nav-video",
            SettingsSection::Network => "settings-nav-network",
            SettingsSection::Appearance => "settings-nav-appearance",
            SettingsSection::Preferences => "settings-nav-preferences",
        }
    }

    fn all() -> [SettingsSection; 5] {
        [
            SettingsSection::Audio,
            SettingsSection::Video,
            SettingsSection::Network,
            SettingsSection::Appearance,
            SettingsSection::Preferences,
        ]
    }

    fn next(self) -> Self {
        match self {
            SettingsSection::Audio => SettingsSection::Video,
            SettingsSection::Video => SettingsSection::Network,
            SettingsSection::Network => SettingsSection::Appearance,
            SettingsSection::Appearance => SettingsSection::Preferences,
            SettingsSection::Preferences => SettingsSection::Audio,
        }
    }

    fn prev(self) -> Self {
        match self {
            SettingsSection::Audio => SettingsSection::Preferences,
            SettingsSection::Video => SettingsSection::Audio,
            SettingsSection::Network => SettingsSection::Video,
            SettingsSection::Appearance => SettingsSection::Network,
            SettingsSection::Preferences => SettingsSection::Appearance,
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
    #[props(default)] initial_section: Option<String>,
) -> Element {
    let is_ios_safari = is_ios();
    // Map the parent's requested section string to the enum. The "performance"
    // section no longer exists here (it moved to the Diagnostics drawer, #1131);
    // the parent (attendants) intercepts "performance" BEFORE opening this modal
    // and routes it to the drawer, so reaching here with "performance" should not
    // happen — fall back to the default Audio tab defensively rather than panic.
    let requested = match initial_section.as_deref() {
        Some("appearance") => SettingsSection::Appearance,
        Some("preferences") => SettingsSection::Preferences,
        Some("network") => SettingsSection::Network,
        Some("video") => SettingsSection::Video,
        _ => SettingsSection::Audio,
    };
    // The parent uses a `key` (generation counter) to recreate this component
    // when the modal first opens, so `use_signal`'s initializer runs fresh.
    let mut active_section = use_signal(move || requested);
    // Defensive: detect parent-driven section switches while the modal stays
    // mounted. Currently unreachable (the fullscreen overlay prevents clicking
    // "Action Bar…" while open), but guards against future callers that may
    // change `initial_section` without a key remount.
    let initial_section_clone = initial_section.clone();
    let mut prev_section_prop = use_signal(move || initial_section_clone);
    if *prev_section_prop.read() != initial_section {
        prev_section_prop.set(initial_section);
        active_section.set(requested);
    }
    let mut open_dropdown: Signal<Option<&'static str>> = use_signal(|| None);
    let mut sticky_transport = use_signal(load_transport_sticky);
    let mut pending_protocol = use_signal(|| transport_preference);

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

                button {
                    class: "settings-modal-close",
                    r#type: "button",
                    "aria-label": "Close settings",
                    onclick: move |_| on_close.call(()),

                    svg { view_box: "0 0 24 24", width: "16", height: "16",

                        path {
                            d: "M6 6L18 18M18 6L6 18",
                            stroke: "currentColor",
                            stroke_width: "2",
                            stroke_linecap: "round",
                        }
                    }
                }

                div { class: "device-settings-content settings-layout",
                    div {
                        class: "settings-sidebar",
                        role: "tablist",
                        "aria-label": "Settings sections",

                        h2 {
                            id: "device-settings-title",
                            class: "settings-sidebar-title",

                            svg {
                                class: "settings-sidebar-title-icon",
                                xmlns: "http://www.w3.org/2000/svg",
                                width: "20",
                                height: "20",
                                view_box: "0 0 24 24",
                                fill: "none",
                                stroke: "currentColor",
                                stroke_width: "2",
                                stroke_linecap: "round",
                                stroke_linejoin: "round",

                                path { d: "M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.38a2 2 0 0 0-.73-2.73l-.15-.09a2 2 0 0 1-1-1.74v-.51a2 2 0 0 1 1-1.72l.15-.1a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" }

                                circle { cx: "12", cy: "12", r: "3" }
                            }

                            span { "Settings" }
                        }

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
                        // The transitional "Performance moved to Diagnostics" row
                        // was removed (#1131 iteration): the Performance controls
                        // live in the Diagnostics drawer now, and the redirect link
                        // is no longer offered. The "performance" deep link is still
                        // intercepted in attendants and routed to the drawer.
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
                                            options: microphones
                                                .iter()
                                                .map(|d| GlassSelectOption {
                                                    value: d.device_id(),
                                                    label: d.label(),
                                                })
                                                .collect::<Vec<_>>(),
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
                                                options: speakers
                                                    .iter()
                                                    .map(|d| GlassSelectOption {
                                                        value: d.device_id(),
                                                        label: d.label(),
                                                    })
                                                    .collect::<Vec<_>>(),
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
                                        div {
                                        id: SettingsSection::Network.panel_id(),
                                            p { class: "ios-speaker-note",
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
                                    p { class: "settings-section-description", "Choose the camera used during the call." }

                                    div { class: "device-setting-group",
                                        label { r#for: "modal-video-select", "Camera" }
                                        SettingsGlassSelect {
                                            id: "modal-video-select",
                                            options: cameras
                                                .iter()
                                                .map(|d| GlassSelectOption {
                                                    value: d.device_id(),
                                                    label: d.label(),
                                                })
                                                .collect::<Vec<_>>(),
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
                                        } // Persist immediately so the choice survives an unexpected tab close.
                                    }
                                }
                            },
                            SettingsSection::Network => rsx! {
                                div {
                                    id: SettingsSection::Network.panel_id(),
                                    class: "settings-section",
                                    role: "tabpanel",
                                    "aria-labelledby": SettingsSection::Network.tab_id(),

                                    h3 { class: "settings-section-title settings-section-title-with-icon",
                                        {render_nav_icon(SettingsSection::Network)}
                                        span { "Network" }
                                    }

                                    p { class: "settings-section-description", "Choose the transport protocol for media connections." }

                                    // Selection is staged in `pending_protocol`; "Apply" commits and reloads.
                                    div { class: "device-setting-group",
                                        span {
                                            id: "transport-segmented-label",
                                            class: "transport-segmented-label",
                                            "Protocol"
                                        }
                                        div {
                                            class: "transport-segmented",
                                            role: "radiogroup",
                                            "aria-labelledby": "transport-segmented-label",
                                            for option in [
                                                (TransportPreference::WebTransport, "WebTransport (default)", "transport-radio-webtransport"),
                                                (TransportPreference::WebSocket, "WebSocket", "transport-radio-websocket"),
                                            ] {
                                                {
                                                    let (value, label, test_id) = option;
                                                    let is_selected = pending_protocol() == value;
                                                    rsx! {
                                                        button {
                                                            key: "{test_id}",
                                                            r#type: "button",
                                                            role: "radio",
                                                            "aria-checked": if is_selected { "true" } else { "false" },
                                                            "data-testid": test_id,
                                                            class: if is_selected { "transport-segmented-option selected" } else { "transport-segmented-option" },
                                                            onclick: move |_| {
                                                                // Picking a DIFFERENT protocol is a fresh, uncommitted
                                                                // choice, so un-check "Remember" in the UI — otherwise a
                                                                // stale pin from the previously selected protocol (e.g.
                                                                // an old WebSocket pin) would keep the toggle stuck ON
                                                                // and the user could never clear it (issue #1291).
                                                                // `use_signal` only runs its init once per mount; after that
                                                                // `sticky_transport` is reconciled here (radio reset) and by
                                                                // the "Remember" checkbox — never by storage. This is an IN-MEMORY reset
                                                                // only: storage is intentionally NOT touched here, because
                                                                // the modal is a staging surface and persistence happens
                                                                // solely on "Apply" via `apply_transport_decision` (which
                                                                // clears any stale pin in its `(true,false)` and
                                                                // `(false,false)` arms). Mutating storage on an
                                                                // uncommitted radio click would wipe a confirmed pin even
                                                                // if the user abandons the modal without applying.
                                                                if pending_protocol() != value {
                                                                    sticky_transport.set(false);
                                                                }
                                                                pending_protocol.set(value);
                                                            },
                                                            "{label}"
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Shown for BOTH protocols (#1291): hiding the toggle for the
                                    // default left a stale WebSocket pin un-clearable when the user
                                    // switched the radio back to WebTransport. Pinning the default
                                    // is itself harmless — `apply_transport_decision` writes
                                    // `vc_transport_preference=webtransport` + `vc_transport_sticky=true`,
                                    // which `load_transport_preference` resolves to WebTransport anyway.
                                    div { class: "device-setting-group sticky-protocol-row",
                                        div { class: "sticky-protocol-row-inner",
                                            div { class: "sticky-protocol-text",
                                                label {
                                                    r#for: "sticky-transport-checkbox",
                                                    class: "sticky-protocol-label",
                                                    "Remember protocol choice"
                                                }
                                                p { class: "sticky-protocol-hint", "Pin this protocol across browser sessions." }
                                            }
                                            label {
                                                class: "glow-switch",
                                                "aria-label": "Remember protocol choice across browser sessions",
                                                input {
                                                    id: "sticky-transport-checkbox",
                                                    r#type: "checkbox",
                                                    checked: sticky_transport(),
                                                    onchange: move |evt: Event<FormData>| {
                                                        // Stage the choice in memory only; persistence happens
                                                        // solely on "Apply" via `apply_transport_decision`,
                                                        // mirroring the radio's in-memory-only reset. Writing
                                                        // storage on an uncommitted toggle would pin (or wipe) a
                                                        // protocol even if the user abandons the modal.
                                                        sticky_transport.set(evt.checked());
                                                    },
                                                }
                                                span { class: "glow-switch-track" }
                                            }
                                        }
                                    }

                                    // Advisory shown only for a NON-default (WebSocket) pin. The
                                    // "switch back to WebTransport to clear" wording is only true
                                    // when the pinned protocol is WebSocket, so it stays suppressed
                                    // for a WebTransport+remember selection (which is itself
                                    // harmless — load resolves it to the default regardless).
                                    if sticky_transport() && pending_protocol() != TransportPreference::default() {
                                        div {
                                            class: "settings-info-panel",
                                            role: "note",
                                            div { class: "settings-info-panel-icon",
                                                svg {
                                                    view_box: "0 0 24 24",
                                                    width: "16",
                                                    height: "16",
                                                    "aria-hidden": "true",
                                                    circle {
                                                        cx: "12",
                                                        cy: "12",
                                                        r: "10",
                                                        fill: "none",
                                                        stroke: "currentColor",
                                                        stroke_width: "1.5",
                                                    }
                                                    path {
                                                        d: "M12 8v5",
                                                        stroke: "currentColor",
                                                        stroke_width: "1.5",
                                                        stroke_linecap: "round",
                                                    }
                                                    circle {
                                                        cx: "12",
                                                        cy: "16",
                                                        r: "0.9",
                                                        fill: "currentColor",
                                                    }
                                                }
                                            }
                                            div { class: "settings-info-panel-body",
                                                p { class: "settings-info-panel-title", "Protocol pinned" }
                                                p { class: "settings-info-panel-text",
                                                    "This protocol will be used on every future page load. Turn off \"Remember protocol choice\" (or switch back to WebTransport) to clear it."
                                                }
                                            }
                                        }
                                    }

                                    // Shown when the staged selection (protocol or sticky flag) diverges from the stored state.
                                    if pending_protocol() != transport_preference || sticky_transport() != load_transport_sticky() {
                                        div { class: "transport-apply-row",
                                            p { class: "transport-preference-note", "Changing protocol will reload the page." }
                                            button {
                                                r#type: "button",
                                                class: "transport-apply-button",
                                                "data-testid": "transport-apply-button",
                                                onclick: move |_| {
                                                    // Single source of truth for the persistence decision,
                                                    // shared with `confirm_transport_change` so the two
                                                    // callers cannot drift. For a non-default + not-sticky
                                                    // choice this also clears any prior sticky pin so a
                                                    // session-scoped choice wins on the next load (#1291).
                                                    apply_transport_decision(pending_protocol(), sticky_transport());
                                                    if let Some(w) = web_sys::window() {
                                                        let _ = w.location().reload();
                                                    }
                                                },
                                                "Apply"
                                            }
                                        }
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
                            SettingsSection::Preferences => rsx! {
                                div {
                                    id: SettingsSection::Preferences.panel_id(),
                                    class: "settings-section",
                                    role: "tabpanel",
                                    "aria-labelledby": SettingsSection::Preferences.tab_id(),

                                    crate::components::preferences_settings_panel::PreferencesSettingsPanel {}
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
            {render_nav_icon(section)}
            span { class: "settings-nav-label", "{section.title()}" }
        }
    }
}

// Monochrome stroke-only icons for the settings sidebar. Rendered with
// `currentColor` so they pick up the nav button's own text color in both
// themes — no per-tab color, no glow, no fill.
fn render_nav_icon(section: SettingsSection) -> Element {
    match section {
        SettingsSection::Audio => rsx! {
            svg {
                class: "settings-nav-icon",
                view_box: "0 0 24 24",
                width: "18",
                height: "18",
                "aria-hidden": "true",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "1.6",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                path { d: "M5 9v6h3l5 4V5L8 9H5z" }
                path { d: "M16 8a5 5 0 0 1 0 8" }
                path { d: "M19 5a9 9 0 0 1 0 14" }
            }
        },
        SettingsSection::Video => rsx! {
            svg {
                class: "settings-nav-icon",
                view_box: "0 0 24 24",
                width: "18",
                height: "18",
                "aria-hidden": "true",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "1.6",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                rect {
                    x: "3",
                    y: "6",
                    width: "13",
                    height: "12",
                    rx: "2",
                }
                path { d: "M16 10l5-3v10l-5-3z" }
            }
        },
        SettingsSection::Network => rsx! {
            svg {
                class: "settings-nav-icon",
                view_box: "0 0 24 24",
                width: "18",
                height: "18",
                "aria-hidden": "true",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "1.6",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                path { d: "M5 18h2v-4H5v4z" }
                path { d: "M11 18h2v-7h-2v7z" }
                path { d: "M17 18h2V7h-2v11z" }
            }
        },
        SettingsSection::Appearance => rsx! {
            svg {
                class: "settings-nav-icon",
                view_box: "0 0 24 24",
                width: "18",
                height: "18",
                "aria-hidden": "true",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "1.6",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                // Sun: small disc on the upper-left with a few rays
                circle { cx: "9", cy: "9", r: "3.2" }
                path { d: "M9 3.4v1.6 M9 13v1.6 M3.4 9h1.6 M13 9h1.6 M5.1 5.1l1.1 1.1 M11.8 11.8l1.1 1.1 M12.9 5.1l-1.1 1.1 M6.2 11.8l-1.1 1.1" }
                // Moon: crescent on the lower-right
                path { d: "M20.5 14.2a6 6 0 1 1-6.7-6.7 4.6 4.6 0 0 0 6.7 6.7z" }
            }
        },
        SettingsSection::Preferences => rsx! {
            svg {
                class: "settings-nav-icon",
                view_box: "0 0 24 24",
                width: "18",
                height: "18",
                "aria-hidden": "true",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "1.6",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                // Three horizontal sliders
                line { x1: "4", y1: "6", x2: "20", y2: "6" }
                circle { cx: "8", cy: "6", r: "2", fill: "currentColor", stroke: "none" }
                line { x1: "4", y1: "12", x2: "20", y2: "12" }
                circle { cx: "16", cy: "12", r: "2", fill: "currentColor", stroke: "none" }
                line { x1: "4", y1: "18", x2: "20", y2: "18" }
                circle { cx: "10", cy: "18", r: "2", fill: "currentColor", stroke: "none" }
            }
        },
    }
}

fn find_device_by_id(devices: &[MediaDeviceInfo], device_id: &str) -> Option<DeviceInfo> {
    devices
        .iter()
        .find(|device| device.device_id() == device_id)
        .map(DeviceInfo::from_media_device_info)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tablist is now FIVE tabs — Performance was removed (#1131) and
    /// Preferences was added. If a Performance variant is reintroduced into
    /// `all()` this length flips, catching an accidental revert.
    #[test]
    fn tablist_has_five_sections_without_performance() {
        let all = SettingsSection::all();
        assert_eq!(all.len(), 5, "tablist must have exactly 5 tabs");
        let titles: Vec<&str> = all.iter().map(|s| s.title()).collect();
        assert_eq!(
            titles,
            ["Audio", "Video", "Network", "Appearance", "Preferences"]
        );
        assert!(
            !titles.contains(&"Performance"),
            "Performance tab must not be in the tablist (it moved to Diagnostics)"
        );
    }

    /// `next()` cycles through all five sections and wraps.
    #[test]
    fn next_cycles_five_sections_and_wraps() {
        assert_eq!(SettingsSection::Audio.next(), SettingsSection::Video);
        assert_eq!(SettingsSection::Video.next(), SettingsSection::Network);
        assert_eq!(SettingsSection::Network.next(), SettingsSection::Appearance);
        assert_eq!(
            SettingsSection::Appearance.next(),
            SettingsSection::Preferences
        );
        assert_eq!(SettingsSection::Preferences.next(), SettingsSection::Audio);
    }

    /// `prev()` is the exact inverse of `next()` over the five-section ring.
    #[test]
    fn prev_is_inverse_of_next() {
        for s in SettingsSection::all() {
            assert_eq!(s.next().prev(), s, "prev(next({s:?})) must round-trip");
            assert_eq!(s.prev().next(), s, "next(prev({s:?})) must round-trip");
        }
    }
}

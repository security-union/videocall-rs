/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */
use crate::components::avatar_settings::focus_with_fallback;
use crate::components::recent_actions::SettingsTarget;
use crate::components::transport_fallback::{fallback_note, transport_fallback_active};
use crate::context::{
    apply_transport_decision, displayed_default_transport, effective_default_transport,
    load_transport_sticky, pinned_protocol_advice, transport_option_label, TransportPreference,
};
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

fn arrow_switches_tabs(event: &KeyboardEvent) -> bool {
    event
        .data()
        .downcast::<web_sys::KeyboardEvent>()
        .and_then(|key_event| key_event.target())
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        .is_some_and(|element| {
            element.id() == SETTINGS_DIALOG_ID
                || element.closest("[role='tablist']").ok().flatten().is_some()
        })
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

/// The `initial_section` that opens the Profile tab.
pub const PROFILE_SECTION: &str = "profile";
pub(crate) const SETTINGS_DIALOG_ID: &str = "device-settings-dialog";

/// Focus the open modal's Profile tab, else the dialog.
pub fn focus_profile_tab() {
    focus_with_fallback(
        SettingsSection::Profile.tab_id(),
        &format!("#{SETTINGS_DIALOG_ID}"),
    );
}

// The `Performance` tab was removed in #1131: the Performance controls moved
// into the Diagnostics drawer. (The transitional "moved to Diagnostics" redirect
// row was also removed; users find Performance in the drawer, and the
// "performance" deep link routes there via attendants.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SettingsSection {
    Audio,
    Video,
    Network,
    Appearance,
    Preferences,
    Profile,
}

impl SettingsSection {
    fn title(self) -> &'static str {
        match self {
            SettingsSection::Audio => "Audio",
            SettingsSection::Video => "Video",
            SettingsSection::Network => "Network",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Preferences => "Preferences",
            SettingsSection::Profile => "Profile",
        }
    }

    fn tab_id(self) -> &'static str {
        match self {
            SettingsSection::Audio => "settings-tab-audio",
            SettingsSection::Video => "settings-tab-video",
            SettingsSection::Network => "settings-tab-network",
            SettingsSection::Appearance => "settings-tab-appearance",
            SettingsSection::Preferences => "settings-tab-preferences",
            SettingsSection::Profile => "settings-tab-profile",
        }
    }

    fn panel_id(self) -> &'static str {
        match self {
            SettingsSection::Audio => "settings-panel-audio",
            SettingsSection::Video => "settings-panel-video",
            SettingsSection::Network => "settings-panel-network",
            SettingsSection::Appearance => "settings-panel-appearance",
            SettingsSection::Preferences => "settings-panel-preferences",
            SettingsSection::Profile => "settings-panel-profile",
        }
    }

    fn test_id(self) -> &'static str {
        match self {
            SettingsSection::Audio => "settings-nav-audio",
            SettingsSection::Video => "settings-nav-video",
            SettingsSection::Network => "settings-nav-network",
            SettingsSection::Appearance => "settings-nav-appearance",
            SettingsSection::Preferences => "settings-nav-preferences",
            SettingsSection::Profile => "settings-nav-profile",
        }
    }

    fn all() -> [SettingsSection; 6] {
        [
            SettingsSection::Audio,
            SettingsSection::Video,
            SettingsSection::Network,
            SettingsSection::Appearance,
            SettingsSection::Preferences,
            SettingsSection::Profile,
        ]
    }

    fn next(self) -> Self {
        match self {
            SettingsSection::Audio => SettingsSection::Video,
            SettingsSection::Video => SettingsSection::Network,
            SettingsSection::Network => SettingsSection::Appearance,
            SettingsSection::Appearance => SettingsSection::Preferences,
            SettingsSection::Preferences => SettingsSection::Profile,
            SettingsSection::Profile => SettingsSection::Audio,
        }
    }

    fn prev(self) -> Self {
        match self {
            SettingsSection::Audio => SettingsSection::Profile,
            SettingsSection::Video => SettingsSection::Audio,
            SettingsSection::Network => SettingsSection::Video,
            SettingsSection::Appearance => SettingsSection::Network,
            SettingsSection::Preferences => SettingsSection::Appearance,
            SettingsSection::Profile => SettingsSection::Preferences,
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
    #[props(default)] active_transport: Option<TransportPreference>,
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
        Some(PROFILE_SECTION) => SettingsSection::Profile,
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
                id: SETTINGS_DIALOG_ID,
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
                        Key::ArrowDown | Key::ArrowRight if arrow_switches_tabs(&evt) => {
                            evt.stop_propagation();
                            active_section.set(active_section().next());
                        }
                        Key::ArrowUp | Key::ArrowLeft if arrow_switches_tabs(&evt) => {
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
                                        "data-recent-action": SettingsTarget::Microphone.slug(),
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
                                            "data-recent-action": SettingsTarget::Speaker.slug(),
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
                                        "data-recent-action": SettingsTarget::Camera.slug(),
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
                            SettingsSection::Network => {
                                let server_wt_enabled =
                                    crate::constants::webtransport_enabled().unwrap_or(false);
                                let default_transport = effective_default_transport();
                                let marked_default = displayed_default_transport(
                                    default_transport,
                                    server_wt_enabled,
                                );
                                let mismatch_note = active_transport
                                    .filter(|_| {
                                        transport_fallback_active(
                                            transport_preference,
                                            server_wt_enabled,
                                            active_transport,
                                        ) == Some(true)
                                    })
                                    .map(fallback_note);
                                let (current_state, current_label) =
                                    current_transport_readout(active_transport);
                                let current_badge = current_transport_badge(active_transport);
                                rsx! {
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

                                    p {
                                        id: "transport-current",
                                        class: "transport-current",
                                        "data-testid": "transport-current",
                                        "data-state": current_state,
                                        "aria-live": "polite",
                                        "aria-atomic": "true",
                                        span { "Currently using: " }
                                        span { class: "transport-current-reading",
                                            span {
                                                class: "transport-current-value",
                                                "data-testid": "transport-current-value",
                                                "{current_label}"
                                            }
                                            if let Some((badge_class, badge_text)) = current_badge {
                                                span {
                                                    class: badge_class,
                                                    "data-testid": "transport-current-badge",
                                                    "aria-hidden": "true",
                                                    "{badge_text}"
                                                }
                                            }
                                        }
                                    }

                                    if let Some(note_text) = mismatch_note {
                                        div {
                                            class: "settings-info-panel",
                                            role: "note",
                                            "data-testid": "transport-in-use-mismatch",
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
                                                p { class: "settings-info-panel-title", "Not using your preferred protocol" }
                                                p { class: "settings-info-panel-text", "{note_text}" }
                                            }
                                        }
                                    }

                                    // Selection is staged in `pending_protocol`; "Apply" commits and reloads.
                                    div { class: "device-setting-group",
                                        "data-recent-action": SettingsTarget::PreferredProtocol.slug(),
                                        span {
                                            id: "transport-segmented-label",
                                            class: "transport-segmented-label",
                                            "Preferred protocol"
                                        }
                                        div {
                                            class: "transport-segmented",
                                            role: "radiogroup",
                                            "aria-labelledby": "transport-segmented-label",
                                            "aria-describedby": "transport-current",
                                            for option in [
                                                (TransportPreference::WebTransport, "transport-radio-webtransport"),
                                                (TransportPreference::WebSocket, "transport-radio-websocket"),
                                            ] {
                                                {
                                                    let (value, test_id) = option;
                                                    let is_selected = pending_protocol() == value;
                                                    let unavailable = value == TransportPreference::WebTransport
                                                        && !server_wt_enabled;
                                                    let describedby = match value {
                                                        TransportPreference::WebTransport => "transport-webtransport-desc",
                                                        TransportPreference::WebSocket => "transport-websocket-desc",
                                                    };
                                                    rsx! {
                                                        button {
                                                            key: "{test_id}",
                                                            r#type: "button",
                                                            role: "radio",
                                                            "aria-checked": if is_selected { "true" } else { "false" },
                                                            "aria-describedby": describedby,
                                                            // aria-disabled, not the native attribute: this radio
                                                            // may carry the group's checked state.
                                                            "aria-disabled": if unavailable { "true" } else { "false" },
                                                            "data-testid": test_id,
                                                            class: match (is_selected, unavailable) {
                                                                (true, true) => "transport-segmented-option selected unavailable",
                                                                (true, false) => "transport-segmented-option selected",
                                                                (false, true) => "transport-segmented-option unavailable",
                                                                (false, false) => "transport-segmented-option",
                                                            },
                                                            onclick: move |_| {
                                                                if unavailable {
                                                                    return;
                                                                }
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
                                                            {transport_option_label(value, marked_default, server_wt_enabled)}
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        span {
                                            id: "transport-webtransport-desc",
                                            class: "visually-hidden",
                                            if server_wt_enabled {
                                                {TRANSPORT_WEBTRANSPORT_DESC}
                                            } else {
                                                "Unavailable on this deployment."
                                            }
                                        }
                                        span {
                                            id: "transport-websocket-desc",
                                            class: "visually-hidden",
                                            {TRANSPORT_WEBSOCKET_DESC}
                                        }
                                    }

                                    // A freshly inserted live region is skipped by some screen readers,
                                    // so this one stays in the DOM and only its content changes.
                                    div {
                                        class: "transport-warning-live",
                                        "aria-live": "polite",
                                        "aria-atomic": "true",
                                        if pending_protocol() == TransportPreference::WebSocket {
                                            div {
                                                class: "settings-info-panel",
                                                role: "note",
                                                "data-testid": "transport-websocket-note",
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
                                                    p { class: "settings-info-panel-title", "WebSocket is the compatibility fallback" }
                                                    p { class: "settings-info-panel-text",
                                                        {TRANSPORT_WEBSOCKET_NOTE}
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Shown for BOTH protocols (#1291), or a stale pin of the other
                                    // protocol becomes un-clearable.
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

                                    if sticky_transport() && pending_protocol() != marked_default {
                                        div {
                                            class: "settings-info-panel",
                                            role: "note",
                                            "data-testid": "transport-pinned-advisory",
                                            div { class: "settings-info-panel-icon",
                                                svg {
                                                    view_box: "0 0 24 24",
                                                    width: "16",
                                                    height: "16",
                                                    "aria-hidden": "true",
                                                    rect {
                                                        x: "6",
                                                        y: "11",
                                                        width: "12",
                                                        height: "9",
                                                        rx: "1.5",
                                                        fill: "none",
                                                        stroke: "currentColor",
                                                        stroke_width: "1.5",
                                                    }
                                                    path {
                                                        d: "M9 11V8a3 3 0 0 1 6 0v3",
                                                        fill: "none",
                                                        stroke: "currentColor",
                                                        stroke_width: "1.5",
                                                        stroke_linecap: "round",
                                                    }
                                                }
                                            }
                                            div { class: "settings-info-panel-body",
                                                p { class: "settings-info-panel-title", "Protocol pinned" }
                                                p { class: "settings-info-panel-text",
                                                    {pinned_protocol_advice(pending_protocol(), marked_default)}
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
                            SettingsSection::Profile => rsx! {
                                div {
                                    id: SettingsSection::Profile.panel_id(),
                                    class: "settings-section",
                                    "data-recent-action": SettingsTarget::Avatar.slug(),
                                    role: "tabpanel",
                                    "aria-labelledby": SettingsSection::Profile.tab_id(),

                                    crate::components::avatar_settings::AvatarSettingsPanel {
                                        variant: crate::components::avatar_settings::AvatarPanelVariant::InMeeting,
                                        can_edit: crate::components::avatar::avatar_editing_allowed(),
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
        SettingsSection::Profile => rsx! {
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
                circle { cx: "12", cy: "8", r: "3.5" }
                path { d: "M5 20c0-3.6 3.1-6 7-6s7 2.4 7 6" }
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

fn current_transport_readout(active: Option<TransportPreference>) -> (&'static str, &'static str) {
    match active {
        Some(TransportPreference::WebTransport) => ("webtransport", "WebTransport"),
        Some(TransportPreference::WebSocket) => ("websocket", "WebSocket"),
        None => ("none", "Not connected"),
    }
}

fn current_transport_badge(
    active: Option<TransportPreference>,
) -> Option<(&'static str, &'static str)> {
    match active? {
        TransportPreference::WebTransport => Some(("transport-badge transport-badge--wt", "WT")),
        TransportPreference::WebSocket => Some(("transport-badge transport-badge--ws", "WS")),
    }
}

pub const TRANSPORT_WEBTRANSPORT_DESC: &str =
    "Switches to WebSocket automatically when a network blocks it.";
pub const TRANSPORT_WEBSOCKET_DESC: &str =
    "Tries WebSocket first; uses WebTransport only if WebSocket can't connect.";
pub const TRANSPORT_WEBSOCKET_NOTE: &str = "WebSocket is tried first, which helps if this network or device has trouble with WebTransport. WebTransport is used only if WebSocket can't connect. Otherwise WebTransport switches to WebSocket on its own when a network blocks it, so most people never need to change this.";

#[cfg(test)]
mod transport_readout_tests {
    use super::*;

    #[test]
    fn current_transport_readout_names_the_elected_protocol_or_not_connected() {
        assert_eq!(
            current_transport_readout(Some(TransportPreference::WebTransport)),
            ("webtransport", "WebTransport")
        );
        assert_eq!(
            current_transport_readout(Some(TransportPreference::WebSocket)),
            ("websocket", "WebSocket")
        );
        assert_eq!(current_transport_readout(None), ("none", "Not connected"));
    }

    #[test]
    fn current_transport_badge_uses_the_tile_badge_classes_and_is_absent_when_not_connected() {
        assert_eq!(
            current_transport_badge(Some(TransportPreference::WebTransport)),
            Some(("transport-badge transport-badge--wt", "WT"))
        );
        assert_eq!(
            current_transport_badge(Some(TransportPreference::WebSocket)),
            Some(("transport-badge transport-badge--ws", "WS"))
        );
        assert_eq!(current_transport_badge(None), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Performance was removed (#1131); Profile is the last tab.
    #[test]
    fn tablist_has_six_sections_with_profile_last_and_without_performance() {
        let all = SettingsSection::all();
        assert_eq!(all.len(), 6, "tablist must have exactly 6 tabs");
        let titles: Vec<&str> = all.iter().map(|s| s.title()).collect();
        assert_eq!(
            titles,
            [
                "Audio",
                "Video",
                "Network",
                "Appearance",
                "Preferences",
                "Profile"
            ]
        );
        assert!(
            !titles.contains(&"Performance"),
            "Performance tab must not be in the tablist (it moved to Diagnostics)"
        );
    }

    #[test]
    fn a_selected_option_keeps_a_focus_ring() {
        let css: String = include_str!("../../static/style.css")
            .split_whitespace()
            .collect();
        for selector in [".transport-segmented-option", ".theme-icon-button"] {
            let ring = format!(
                "{selector}:focus-visible{{outline:2pxsolidvar(--accent-hover);outline-offset:-2px;"
            );
            assert!(css.contains(&ring), "{selector}");
        }
        assert!(css.contains(concat!(
            "@media(forced-colors:active){.transport-segmented-option.selected:not(:focus-visible),",
            ".theme-icon-button.theme-icon-button--active:not(:focus-visible){",
            "outline:2pxsolidHighlight;"
        )));
    }

    /// `next()` cycles through all six sections and wraps.
    #[test]
    fn next_cycles_six_sections_and_wraps() {
        assert_eq!(SettingsSection::Audio.next(), SettingsSection::Video);
        assert_eq!(SettingsSection::Video.next(), SettingsSection::Network);
        assert_eq!(SettingsSection::Network.next(), SettingsSection::Appearance);
        assert_eq!(
            SettingsSection::Appearance.next(),
            SettingsSection::Preferences
        );
        assert_eq!(
            SettingsSection::Preferences.next(),
            SettingsSection::Profile
        );
        assert_eq!(SettingsSection::Profile.next(), SettingsSection::Audio);
        assert_eq!(SettingsSection::Audio.prev(), SettingsSection::Profile);
    }

    #[test]
    fn every_section_has_its_own_ids() {
        let all = SettingsSection::all();
        assert_eq!(SettingsSection::Profile.tab_id(), "settings-tab-profile");
        assert_eq!(
            SettingsSection::Profile.panel_id(),
            "settings-panel-profile"
        );
        assert_eq!(SettingsSection::Profile.test_id(), "settings-nav-profile");
        let mut ids: Vec<&str> = all
            .iter()
            .flat_map(|s| [s.tab_id(), s.panel_id(), s.test_id()])
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), all.len() * 3);
    }

    /// `prev()` is the exact inverse of `next()` over the six-section ring.
    #[test]
    fn prev_is_inverse_of_next() {
        for s in SettingsSection::all() {
            assert_eq!(s.next().prev(), s, "prev(next({s:?})) must round-trip");
            assert_eq!(s.prev().next(), s, "next(prev({s:?})) must round-trip");
        }
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[allow(non_snake_case)]
    fn NetworkHarness() -> Element {
        let theme = use_signal(|| crate::context::Theme::System);
        use_context_provider(|| crate::context::ThemePreferenceCtx(theme));
        let appearance = use_signal(crate::context::AppearanceSettings::default);
        use_context_provider(|| crate::context::AppearanceSettingsCtx(appearance));
        let mut active = use_signal(|| Some(TransportPreference::WebSocket));
        let mut preferred = use_signal(|| TransportPreference::WebTransport);
        rsx! {
            button {
                id: "pref-ws",
                onclick: move |_| preferred.set(TransportPreference::WebSocket),
            }
            button {
                id: "active-wt",
                onclick: move |_| active.set(Some(TransportPreference::WebTransport)),
            }
            button { id: "active-none", onclick: move |_| active.set(None) }
            button {
                id: "active-ws",
                onclick: move |_| active.set(Some(TransportPreference::WebSocket)),
            }
            DeviceSettingsModal {
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
                transport_preference: preferred(),
                active_transport: active(),
                initial_section: Some("network".to_string()),
            }
        }
    }

    fn inject_app_config(webtransport_enabled: bool) {
        let config = js_sys::Object::new();
        let set = |key: &str, value: wasm_bindgen::JsValue| {
            js_sys::Reflect::set(&config, &key.into(), &value).unwrap();
        };
        set("apiBaseUrl", "http://test:8080".into());
        set("wsUrl", "ws://test:8080".into());
        set("webTransportHost", "https://test:4433".into());
        set("oauthEnabled", "false".into());
        set("e2eeEnabled", "false".into());
        set(
            "webTransportEnabled",
            (if webtransport_enabled {
                "true"
            } else {
                "false"
            })
            .into(),
        );
        set("usersAllowedToStream", "".into());
        set("serverElectionPeriodMs", 2000.into());
        js_sys::Reflect::set(&gloo_utils::window(), &"__APP_CONFIG".into(), &config).unwrap();
        crate::constants::reset_config_cache_for_test();
    }

    fn remove_app_config() {
        let _ =
            js_sys::Reflect::delete_property(&gloo_utils::window().into(), &"__APP_CONFIG".into());
        crate::constants::reset_config_cache_for_test();
    }

    #[wasm_bindgen_test]
    async fn arrows_switch_tabs_only_from_the_dialog_or_the_tablist() {
        inject_app_config(true);
        let doc = gloo_utils::document();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(NetworkHarness),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );
        TimeoutFuture::new(30).await;
        let press = |selector: &str, key: &str| {
            let target = mount.query_selector(selector).unwrap().unwrap();
            js_sys::Function::new_with_args(
                "el, key",
                "el.dispatchEvent(new KeyboardEvent('keydown', \
                 { key, bubbles: true, cancelable: true }));",
            )
            .call2(&wasm_bindgen::JsValue::NULL, &target, &key.into())
            .unwrap();
        };
        let selected = || {
            mount
                .query_selector("[role='tab'][aria-selected='true']")
                .unwrap()
                .unwrap()
                .id()
        };
        for key in ["ArrowRight", "ArrowLeft", "ArrowDown", "ArrowUp"] {
            press("[data-testid='transport-radio-websocket']", key);
            press("#sticky-transport-checkbox", key);
            press(".settings-modal-close", key);
            TimeoutFuture::new(30).await;
            assert_eq!(selected(), "settings-tab-network", "{key} on a control");
        }
        press(&format!("#{SETTINGS_DIALOG_ID}"), "ArrowLeft");
        TimeoutFuture::new(30).await;
        assert_eq!(selected(), "settings-tab-video", "on the dialog");
        press("#settings-tab-video", "ArrowLeft");
        TimeoutFuture::new(30).await;
        assert_eq!(selected(), "settings-tab-audio", "on a tab");
        for key in ["ArrowRight", "ArrowLeft"] {
            press("#modal-audio-select", key);
            TimeoutFuture::new(30).await;
            assert_eq!(selected(), "settings-tab-audio", "{key} on a glass select");
        }
        mount
            .query_selector("#settings-tab-appearance")
            .unwrap()
            .unwrap()
            .unchecked_into::<web_sys::HtmlElement>()
            .click();
        TimeoutFuture::new(30).await;
        press(".color-swatch", "ArrowRight");
        TimeoutFuture::new(30).await;
        assert_eq!(selected(), "settings-tab-appearance", "on a swatch");
        mount.remove();
        remove_app_config();
    }

    #[wasm_bindgen_test]
    async fn the_network_panel_notes_a_webtransport_fallback_only_while_one_is_active() {
        inject_app_config(true);
        let doc = gloo_utils::document();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(NetworkHarness),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );
        let click = |id: &str| {
            mount
                .query_selector(&format!("#{id}"))
                .unwrap()
                .unwrap()
                .unchecked_into::<web_sys::HtmlElement>()
                .click();
        };
        let note = || {
            mount
                .query_selector("[data-testid='transport-in-use-mismatch']")
                .unwrap()
                .map(|el| el.text_content().unwrap_or_default())
        };
        TimeoutFuture::new(30).await;
        assert!(
            mount
                .query_selector("[data-testid='transport-radio-webtransport']")
                .unwrap()
                .is_some(),
            "the Network section must be the one rendered"
        );
        let shown = note().expect("preferred WebTransport, using WebSocket");
        assert!(
            shown.contains(fallback_note(TransportPreference::WebSocket)),
            "{shown}"
        );

        click("active-wt");
        TimeoutFuture::new(30).await;
        assert_eq!(note(), None, "the preferred protocol is in use");

        click("active-none");
        TimeoutFuture::new(30).await;
        assert_eq!(note(), None, "no connection, nothing to compare");

        click("active-ws");
        TimeoutFuture::new(30).await;
        assert!(note().is_some(), "the note follows a live transport change");

        click("pref-ws");
        TimeoutFuture::new(30).await;
        assert_eq!(note(), None, "preferred WebSocket, using WebSocket");
        click("active-wt");
        TimeoutFuture::new(30).await;
        let reverse = note().expect("preferred WebSocket, using WebTransport");
        assert!(
            reverse.contains(fallback_note(TransportPreference::WebTransport)),
            "{reverse}"
        );

        inject_app_config(false);
        click("active-none");
        TimeoutFuture::new(30).await;
        click("active-ws");
        TimeoutFuture::new(30).await;
        let wt_label = mount
            .query_selector("[data-testid='transport-radio-webtransport']")
            .unwrap()
            .and_then(|el| el.text_content())
            .unwrap_or_default();
        let disabled_note = note();
        remove_app_config();
        mount.remove();
        assert_eq!(
            wt_label, "WebTransport (unavailable)",
            "the WebTransport-disabled config must be the one rendered"
        );
        assert_eq!(
            disabled_note, None,
            "WebSocket on a deployment without WebTransport is not a fallback"
        );
    }

    #[wasm_bindgen_test]
    async fn the_network_panel_names_the_protocol_in_use_and_follows_it_live() {
        inject_app_config(true);
        let doc = gloo_utils::document();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(NetworkHarness),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );
        let click = |id: &str| {
            mount
                .query_selector(&format!("#{id}"))
                .unwrap()
                .unwrap()
                .unchecked_into::<web_sys::HtmlElement>()
                .click();
        };
        let readout = || {
            let row = mount
                .query_selector("[data-testid='transport-current']")
                .unwrap()?;
            let reading = row
                .query_selector(":scope > .transport-current-reading")
                .unwrap()?;
            let value = reading
                .query_selector(":scope > [data-testid='transport-current-value']")
                .unwrap()?;
            let badge = reading
                .query_selector(":scope > [data-testid='transport-current-badge']")
                .unwrap()
                .map(|badge| {
                    (
                        badge.get_attribute("class").unwrap_or_default(),
                        badge.text_content().unwrap_or_default(),
                        badge.get_attribute("aria-hidden"),
                        value
                            .next_element_sibling()
                            .is_some_and(|next| next.is_same_node(Some(&badge)))
                            && badge.next_element_sibling().is_none(),
                    )
                });
            Some((
                row.get_attribute("data-state").unwrap_or_default(),
                value.text_content().unwrap_or_default(),
                row.text_content().unwrap_or_default(),
                row.last_element_child()
                    .is_some_and(|last| last.is_same_node(Some(&reading))),
                badge,
            ))
        };
        let readout_of = |state: &str, value: &str, text: &str, badge: Option<(&str, &str)>| {
            Some((
                state.to_string(),
                value.to_string(),
                text.to_string(),
                true,
                badge.map(|(modifier, label)| {
                    (
                        format!("transport-badge transport-badge--{modifier}"),
                        label.to_string(),
                        Some("true".to_string()),
                        true,
                    )
                }),
            ))
        };
        TimeoutFuture::new(30).await;
        let row = mount
            .query_selector("[data-testid='transport-current']")
            .unwrap();
        let group = mount
            .query_selector("[role='radiogroup'][aria-labelledby='transport-segmented-label']")
            .unwrap();
        let note = mount
            .query_selector("[data-testid='transport-in-use-mismatch']")
            .unwrap();
        let row_attr = |name: &str| row.as_ref().and_then(|el| el.get_attribute(name));
        let live = row_attr("aria-live");
        let atomic = row_attr("aria-atomic");
        let describedby = group
            .as_ref()
            .and_then(|el| el.get_attribute("aria-describedby"));
        let precedes = |later: &Option<web_sys::Element>| match (&row, later) {
            (Some(row), Some(later)) => {
                row.compare_document_position(later) & web_sys::Node::DOCUMENT_POSITION_FOLLOWING
                    != 0
            }
            _ => false,
        };
        let precedes_note = precedes(&note);
        let precedes_group = precedes(&group);
        let same_row = || {
            let now = mount
                .query_selector("[data-testid='transport-current']")
                .unwrap();
            matches!((&row, now), (Some(first), Some(now)) if first.is_same_node(Some(&now)))
        };
        let initial = readout();

        click("active-wt");
        TimeoutFuture::new(30).await;
        let on_webtransport = (readout(), same_row());

        click("active-none");
        TimeoutFuture::new(30).await;
        let on_none = (readout(), same_row());

        click("active-ws");
        TimeoutFuture::new(30).await;
        let back_on_websocket = (readout(), same_row());

        remove_app_config();
        mount.remove();

        assert!(row.is_some(), "the current-protocol readout must render");
        assert!(group.is_some(), "the protocol radiogroup must render");
        assert!(
            note.is_some(),
            "the fallback note must render in this state"
        );
        assert_eq!(live.as_deref(), Some("polite"));
        assert_eq!(atomic.as_deref(), Some("true"));
        assert_eq!(describedby.as_deref(), Some("transport-current"));
        assert!(precedes_note, "the readout comes before the fallback note");
        assert!(precedes_group, "the readout comes before the radiogroup");
        assert_eq!(
            initial,
            readout_of(
                "websocket",
                "WebSocket",
                "Currently using: WebSocketWS",
                Some(("ws", "WS"))
            )
        );
        assert_eq!(
            on_webtransport,
            (
                readout_of(
                    "webtransport",
                    "WebTransport",
                    "Currently using: WebTransportWT",
                    Some(("wt", "WT"))
                ),
                true
            ),
            "the live region updates in place, never remounts"
        );
        assert_eq!(
            on_none,
            (
                readout_of(
                    "none",
                    "Not connected",
                    "Currently using: Not connected",
                    None
                ),
                true
            ),
            "the readout stays rendered, in place, with no connection"
        );
        assert_eq!(
            back_on_websocket,
            (
                readout_of(
                    "websocket",
                    "WebSocket",
                    "Currently using: WebSocketWS",
                    Some(("ws", "WS"))
                ),
                true
            ),
            "the readout follows a live transport change in place"
        );
    }

    const GLOBAL_CSS: &str = include_str!("../../static/global.css");
    const STYLE_CSS: &str = include_str!("../../static/style.css");

    fn add_sheet(css: &str) -> web_sys::Element {
        let doc = gloo_utils::document();
        let style = doc.create_element("style").unwrap();
        style.set_text_content(Some(css));
        doc.head().unwrap().append_child(&style).unwrap();
        style
    }

    fn mount_harness() -> web_sys::Element {
        let doc = gloo_utils::document();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(NetworkHarness),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );
        mount
    }

    fn click(mount: &web_sys::Element, id: &str) {
        mount
            .query_selector(&format!("#{id}"))
            .unwrap()
            .unwrap()
            .unchecked_into::<web_sys::HtmlElement>()
            .click();
    }

    fn paint(el: &web_sys::Element) -> (String, String, String) {
        let cs = gloo_utils::window()
            .get_computed_style(el)
            .unwrap()
            .unwrap();
        let get = |p: &str| cs.get_property_value(p).unwrap();
        (
            get("background-color"),
            get("color"),
            get("border-top-color"),
        )
    }

    fn font_size(el: &web_sys::Element) -> String {
        gloo_utils::window()
            .get_computed_style(el)
            .unwrap()
            .unwrap()
            .get_property_value("font-size")
            .unwrap()
    }

    #[wasm_bindgen_test]
    async fn the_network_badge_looks_like_the_tile_badge_in_both_themes() {
        const SMALL_TILE: &str = "--tile-h: 140px";
        inject_app_config(true);
        let global = add_sheet(GLOBAL_CSS);
        let style = add_sheet(STYLE_CSS);
        let doc = gloo_utils::document();
        let root = doc.document_element().unwrap();
        let body = doc.body().unwrap();
        let tile_badge = |parent: &web_sys::Element, modifier: &str| {
            let el = doc.create_element("span").unwrap();
            el.set_class_name(&format!("transport-badge transport-badge--{modifier}"));
            parent.append_child(&el).unwrap();
            el
        };
        let tile_ws = tile_badge(&body, "ws");
        let tile_wt = tile_badge(&body, "wt");
        let small_tile = doc.create_element("div").unwrap();
        small_tile.set_attribute("style", SMALL_TILE).unwrap();
        body.append_child(&small_tile).unwrap();
        let shrunk = font_size(&tile_badge(&small_tile, "ws")) != font_size(&tile_ws);
        let mount = mount_harness();
        mount.set_attribute("style", SMALL_TILE).unwrap();
        TimeoutFuture::new(30).await;

        let mut sizes = Vec::new();
        let mut seen = Vec::new();
        for (modifier, button, tile) in
            [("ws", "active-ws", &tile_ws), ("wt", "active-wt", &tile_wt)]
        {
            click(&mount, button);
            TimeoutFuture::new(30).await;
            let badge = mount
                .query_selector(&format!(
                    ".settings-panel [data-testid='transport-current-badge'].transport-badge--{modifier}"
                ))
                .unwrap();
            sizes.push((modifier, badge.as_ref().map(font_size), font_size(tile)));
            for theme in ["dark", "light"] {
                root.set_attribute("data-theme", theme).unwrap();
                seen.push((
                    theme,
                    modifier,
                    badge.as_ref().map(paint),
                    paint(tile),
                    paint(&tile_ws) != paint(&tile_wt),
                ));
            }
        }

        root.remove_attribute("data-theme").unwrap();
        tile_ws.remove();
        tile_wt.remove();
        small_tile.remove();
        style.remove();
        global.remove();
        remove_app_config();
        mount.remove();

        assert!(
            shrunk,
            "`{SMALL_TILE}` no longer shrinks a tile badge, so the size check guards nothing"
        );
        for (modifier, row, tile) in sizes {
            assert_eq!(
                row,
                Some(tile),
                "{modifier}: the Network badge shrinks with an inherited --tile-h"
            );
        }
        for (theme, modifier, row, tile, distinct) in seen {
            assert!(
                distinct,
                "{theme}: the tile badges paint alike, so the sheets did not load"
            );
            assert_eq!(
                row,
                Some(tile),
                "{theme}/{modifier}: the Network badge drifted off the tile badge's palette"
            );
        }
    }

    #[wasm_bindgen_test]
    async fn the_network_badge_wraps_with_the_protocol_name_never_alone() {
        inject_app_config(true);
        let global = add_sheet(GLOBAL_CSS);
        let style = add_sheet(STYLE_CSS);
        let mount = mount_harness();
        TimeoutFuture::new(30).await;

        let mut seen = Vec::new();
        for button in ["active-ws", "active-wt"] {
            click(&mount, button);
            TimeoutFuture::new(30).await;
            let one = |sel: &str| {
                mount
                    .query_selector(&format!("[data-testid='transport-current']{sel}"))
                    .unwrap()
            };
            let (Some(row), Some(label), Some(value), Some(badge)) = (
                one(""),
                one(" > :first-child"),
                one(" [data-testid='transport-current-value']"),
                one(" [data-testid='transport-current-badge']"),
            ) else {
                seen.push((button, None));
                continue;
            };
            let gap: f64 = gloo_utils::window()
                .get_computed_style(&row)
                .unwrap()
                .unwrap()
                .get_property_value("column-gap")
                .unwrap()
                .trim_end_matches("px")
                .parse()
                .unwrap_or(0.0);
            let width = |el: &web_sys::Element| el.get_bounding_client_rect().width();
            // Room for the label and the name, but not the pill after them.
            let orphaning = width(&label) + gap + width(&value) + gap + width(&badge) / 2.0;
            let row_style = row.clone().unchecked_into::<web_sys::HtmlElement>().style();
            row_style
                .set_property("width", &format!("{orphaning}px"))
                .unwrap();
            let (r, l, v, b) = (
                row.get_bounding_client_rect(),
                label.get_bounding_client_rect(),
                value.get_bounding_client_rect(),
                badge.get_bounding_client_rect(),
            );
            row_style.remove_property("width").unwrap();
            seen.push((
                button,
                Some((
                    b.top() >= l.bottom(),
                    b.top() < v.bottom() && v.top() < b.bottom(),
                    (b.left() - v.right() - gap).abs() < 0.5,
                    b.right() <= r.right() + 0.5,
                )),
            ));
        }

        style.remove();
        global.remove();
        remove_app_config();
        mount.remove();

        for (button, lines) in seen {
            let (wrapped, shared, spaced, contained) =
                lines.unwrap_or_else(|| panic!("{button}: the readout did not render"));
            assert!(
                wrapped,
                "{button}: the width forced no wrap, so this guards nothing"
            );
            assert!(shared, "{button}: the pill wrapped onto a line of its own");
            assert!(spaced, "{button}: the pill is not one gap from the name");
            assert!(contained, "{button}: the pill overflows the row");
        }
    }
}

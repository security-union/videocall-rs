// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;
use dioxus::web::WebEventExt;
use videocall_meeting_client::ApiError;
use videocall_meeting_types::device::DeviceListItem;
use videocall_theme::{
    icons, Alert, AlertActions, AlertMessage, AlertTitle, Button, ButtonVariant, Card, Icon,
    IconButton, ListFooter, ListGroup, ListRow, ListRowLabel, ListRowValue, Scrim, Spinner,
    Toolbar,
};
use wasm_bindgen::{JsCast, JsValue};

use crate::constants::meeting_api_client;
use crate::routing::Route;

/// The router drops unknown query parameters, so the return URL waits in `sessionStorage`.
pub fn redirect_to_login() {
    let Some(window) = web_sys::window() else {
        return;
    };
    if let (Ok(href), Ok(Some(storage))) = (window.location().href(), window.session_storage()) {
        if storage.set_item("vc_oauth_return_to", &href).is_err() {
            // Info, not warn: warn goes to Matomo with the page URL.
            log::info!("sessionStorage unavailable; login will return to the app root");
        }
    }
    let _ = window.location().set_href("/login");
}

#[component]
pub fn Notice(
    title: String,
    #[props(default)] alert: bool,
    testid: &'static str,
    children: Element,
) -> Element {
    rsx! {
        Card { class: "settings-state", role: alert.then_some("alert"), "data-testid": testid,
            if alert {
                Icon { glyph: &icons::ALERT, size: "48", stroke_width: "1.5", class: "settings-state-icon" }
            }
            h2 { "{title}" }
            {children}
        }
    }
}

#[component]
pub fn Busy(label: String) -> Element {
    rsx! {
        div { class: "settings-state",
            Spinner { large: true, label: None }
            span { "{label}" }
        }
    }
}

#[component]
pub fn BackHome() -> Element {
    let navigator = use_navigator();
    rsx! {
        Button {
            onclick: move |_| { navigator.push(Route::Home {}); },
            "Back to Home"
        }
    }
}

fn format_date(unix_seconds: i64) -> String {
    let date = js_sys::Date::new(&JsValue::from_f64(unix_seconds as f64 * 1000.0));
    let options = js_sys::Object::new();
    for (key, value) in [("year", "numeric"), ("month", "short"), ("day", "numeric")] {
        let _ = js_sys::Reflect::set(&options, &key.into(), &value.into());
    }
    let locale = web_sys::window()
        .and_then(|window| window.navigator().language())
        .unwrap_or_else(|| "en-US".to_string());
    date.to_locale_date_string(&locale, &options).into()
}

#[derive(Debug, Clone, PartialEq)]
enum ListState {
    Loading,
    Loaded(Vec<DeviceListItem>),
    Failed,
}

#[component]
pub fn DevicesPage() -> Element {
    let navigator = use_navigator();
    let mut state = use_signal(|| ListState::Loading);
    let mut generation = use_signal(|| 0u32);
    let mut pending_revoke = use_signal(|| None::<DeviceListItem>);
    let mut revoking = use_signal(|| false);
    let mut revoke_error = use_signal(|| None::<String>);

    use_effect(move || {
        let _ = generation();
        spawn(async move {
            let result = match meeting_api_client() {
                Ok(client) => client.list_devices().await,
                Err(e) => Err(ApiError::Config(e)),
            };
            match result {
                Ok(devices) => state.set(ListState::Loaded(devices)),
                Err(ApiError::NotAuthenticated) => redirect_to_login(),
                Err(e) => {
                    log::error!("devices: list failed: {e}");
                    state.set(ListState::Failed);
                }
            }
        });
    });

    let mut close_dialog = move || {
        if !revoking() {
            pending_revoke.set(None);
        }
    };

    let on_confirm_revoke = move |_| {
        let Some(device) = pending_revoke() else {
            return;
        };
        if revoking() {
            return;
        }
        revoking.set(true);
        revoke_error.set(None);
        spawn(async move {
            let result = match meeting_api_client() {
                Ok(client) => client.revoke_device(&device.id).await,
                Err(e) => Err(ApiError::Config(e)),
            };
            revoking.set(false);
            pending_revoke.set(None);
            match result {
                // A 404 means it is already gone; the reload shows that.
                Ok(()) | Err(ApiError::NotFound(_)) => {}
                Err(ApiError::NotAuthenticated) => return redirect_to_login(),
                Err(e) => {
                    log::error!("devices: revoke failed: {e}");
                    revoke_error.set(Some(format!("Could not revoke {}.", device.name)));
                }
            }
            generation += 1;
        });
    };

    let body = match state() {
        ListState::Loading => rsx! { Busy { label: "Loading devices..." } },
        ListState::Failed => rsx! {
            Notice { title: "Could not load your devices", alert: true, testid: "devices-error",
                Button {
                    onclick: move |_| {
                        state.set(ListState::Loading);
                        generation += 1;
                    },
                    "Try again"
                }
            }
        },
        ListState::Loaded(devices) if devices.is_empty() => rsx! {
            Notice { title: "No devices are signed in.", testid: "devices-empty",
                p {
                    "Run "
                    code { "videocall-cli auth login" }
                    " on a device to add it."
                }
            }
        },
        ListState::Loaded(devices) => rsx! {
            section { class: "settings-group",
                ListGroup { list: true,
                    for device in devices {
                        DeviceRow {
                            key: "{device.id}",
                            device: device.clone(),
                            on_revoke: move |device| pending_revoke.set(Some(device)),
                        }
                    }
                }
                ListFooter {
                    "A revoked device is signed out on its next request. A call it is already in is not cut off."
                }
            }
        },
    };

    rsx! {
        div { class: "surface-grouped settings-page",
            div { class: "settings-container",
                Toolbar { header: true, class: "settings-toolbar",
                    IconButton {
                        label: "Back to home",
                        title: "Back to home",
                        onclick: move |_| { navigator.push(Route::Home {}); },
                        Icon { glyph: &icons::CHEVRON_LEFT, size: "20" }
                    }
                    h1 { class: "toolbar-title", "Devices" }
                    span { class: "settings-toolbar-spacer", "aria-hidden": "true" }
                }
                if let Some(message) = revoke_error() {
                    p { class: "settings-footer-error", role: "alert", "{message}" }
                }
                {body}
            }

            if let Some(device) = pending_revoke() {
                Scrim { onclick: move |_| close_dialog(),
                    // Focus starts on Cancel, the safe choice.
                    div {
                        onmounted: move |evt: MountedEvent| {
                            focus_marked_button(&evt);
                        },
                        Alert {
                            "aria-labelledby": "device-revoke-title",
                            "aria-describedby": "device-revoke-message",
                            onkeydown: move |evt: KeyboardEvent| {
                                if evt.key() == Key::Escape {
                                    evt.stop_propagation();
                                    close_dialog();
                                }
                            },
                            AlertTitle { id: "device-revoke-title", "Revoke {device.name}?" }
                            AlertMessage { id: "device-revoke-message",
                                "It can no longer sign in as you. To use it again, run "
                                code { "videocall-cli auth login" }
                                " on it."
                            }
                            AlertActions {
                                Button {
                                    "data-autofocus": "true",
                                    disabled: revoking(),
                                    onclick: move |_| close_dialog(),
                                    "Cancel"
                                }
                                Button {
                                    variant: ButtonVariant::DestructiveProminent,
                                    disabled: revoking(),
                                    onclick: on_confirm_revoke,
                                    "Revoke"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn focus_marked_button(evt: &MountedEvent) {
    let Some(element) = evt.try_as_web_event() else {
        return;
    };
    if let Ok(Some(target)) = element.query_selector("[data-autofocus]") {
        if let Ok(target) = target.dyn_into::<web_sys::HtmlElement>() {
            let _ = target.focus();
        }
    }
}

#[component]
fn DeviceRow(device: DeviceListItem, on_revoke: EventHandler<DeviceListItem>) -> Element {
    let created = format_date(device.created_at);
    let last_used = format_date(device.last_used_at);
    let expires = format_date(device.expires_at);
    let label = format!("Revoke {}", device.name);
    rsx! {
        ListRow { list_item: true, "data-testid": "device-row",
            ListRowLabel { class: "stack gap-1",
                span { class: "text-headline", "{device.name}" }
                span { class: "text-mono text-footnote", "data-testid": "fingerprint", "{device.fingerprint}" }
                span { class: "text-footnote text-secondary", "Added {created}" }
                span { class: "text-footnote text-secondary", "Last used {last_used}" }
                span { class: "text-footnote text-secondary", "Expires {expires}" }
            }
            ListRowValue {
                Button {
                    variant: ButtonVariant::Destructive,
                    "aria-label": "{label}",
                    onclick: move |_| on_revoke.call(device.clone()),
                    "Revoke"
                }
            }
        }
    }
}

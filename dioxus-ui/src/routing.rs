// SPDX-License-Identifier: MIT OR Apache-2.0

//! Application route definitions.

use dioxus::prelude::*;
use videocall_theme::Card;

use crate::components::login::Login;
use crate::pages::device_authorize::DeviceAuthorizePage;
use crate::pages::devices::DevicesPage;
use crate::pages::home::Home;
use crate::pages::meeting::MeetingPage;
use crate::pages::meeting_settings::MeetingSettingsPage;

#[derive(Clone, Routable, PartialEq, Debug)]
pub enum Route {
    #[route("/")]
    Home {},
    #[route("/login")]
    Login {},
    #[route("/devices", DevicesPage)]
    Devices {},
    #[route("/devices/authorize?:request", DeviceAuthorizePage)]
    DeviceAuthorize { request: String },
    #[route("/meeting/:id/settings")]
    MeetingSettings { id: String },
    #[route("/meeting/:id", MeetingPage)]
    Meeting { id: String },
    #[route("/meeting/:id/:webtransport_enabled", MeetingPage2)]
    Meeting2 {
        id: String,
        webtransport_enabled: String,
    },
    #[route("/404")]
    NotFound {},
    /// Any other URL. Without this the router renders its own parse error.
    #[route("/:..segments", UnknownRoute)]
    Unknown { segments: Vec<String> },
}

/// Wrapper component for MeetingSettings route.
#[component]
fn MeetingSettings(id: String) -> Element {
    rsx! { MeetingSettingsPage { id } }
}

/// Wrapper component for Meeting2 route that passes only `id` to MeetingPage.
#[component]
fn MeetingPage2(id: String, webtransport_enabled: String) -> Element {
    rsx! { MeetingPage { id: id } }
}

/// Catch-all wrapper: an unknown URL shows the same page as `/404`.
#[component]
fn UnknownRoute(segments: Vec<String>) -> Element {
    let _ = segments;
    rsx! { NotFound {} }
}

/// Simple 404 page component.
#[component]
fn NotFound() -> Element {
    rsx! {
        div { class: "screen-center surface-grouped",
            Card { class: "status-card",
                h1 { class: "text-large-title", "404" }
                p { class: "text-body text-secondary", "Page not found" }
                a { href: "/", class: "btn btn-prominent", "Go Home" }
            }
        }
    }
}

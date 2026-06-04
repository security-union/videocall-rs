// SPDX-License-Identifier: MIT OR Apache-2.0

//! Application route definitions.

use dioxus::prelude::*;

use crate::components::login::Login;
use crate::components::search_modal::SearchModal;
use crate::pages::guest_join::GuestJoinPage;
use crate::pages::home::Home;
use crate::pages::meeting::MeetingPage;
use crate::pages::meeting_settings::MeetingSettingsPage;
use crate::pages::oauth_callback::OAuthCallback;
use crate::theme::color as theme_color;

#[derive(Clone, Routable, PartialEq, Debug)]
#[rustfmt::skip]
pub enum Route {
    #[layout(Wrapper)]
    #[route("/")]
    Home {},
    #[route("/login")]
    Login {},
    #[route("/auth/callback?:..query_params")]
    OAuthCallback { query_params: String },
    #[route("/meeting/:id/settings")]
    MeetingSettings { id: String },
    #[route("/meeting/:id/guest")]
    GuestJoin { id: String },
    #[route("/meeting/:id", MeetingPage)]
    Meeting { id: String },
    #[route("/meeting/:id/:webtransport_enabled", MeetingPage2)]
    Meeting2 {
        id: String,
        webtransport_enabled: String,
    },
    #[route("/404")]
    NotFound {},
}

#[component]
pub fn Wrapper() -> Element {
    rsx! {
        SearchModal {}
        Outlet::<Route> {}
    }
}

/// Wrapper component for MeetingSettings route.
#[component]
fn MeetingSettings(id: String) -> Element {
    rsx! {
        MeetingSettingsPage { id }
    }
}

/// Wrapper component for GuestJoin route.
#[component]
fn GuestJoin(id: String) -> Element {
    rsx! { GuestJoinPage { id } }
}

/// Wrapper component for Meeting2 route that passes only `id` to MeetingPage.
#[component]
fn MeetingPage2(id: String, webtransport_enabled: String) -> Element {
    rsx! {
        MeetingPage { id }
    }
}

/// Simple 404 page component.
#[component]
fn NotFound() -> Element {
    rsx! {
        div { style: "display: flex; align-items: center; justify-content: center; height: 100vh; background: {theme_color::BG}; color: {theme_color::TEXT_PRIMARY};",
            div { style: "text-align: center;",
                h1 { "404" }
                p { "Page not found" }
                a { href: "/", style: "color: #7928CA;", "Go Home" }
            }
        }
    }
}

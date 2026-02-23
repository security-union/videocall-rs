// SPDX-License-Identifier: MIT OR Apache-2.0

//! Application route definitions.

use dioxus::prelude::*;

use crate::components::login::Login;
use crate::pages::home::Home;
use crate::pages::meeting::MeetingPage;

#[derive(Clone, Routable, PartialEq, Debug)]
pub enum Route {
    #[route("/")]
    Home {},
    #[route("/login")]
    Login {},
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

/// Wrapper component for Meeting2 route that passes only `id` to MeetingPage.
#[component]
fn MeetingPage2(id: String, webtransport_enabled: String) -> Element {
    rsx! { MeetingPage { id: id } }
}

/// Simple 404 page component.
#[component]
fn NotFound() -> Element {
    rsx! {
        div { style: "display: flex; align-items: center; justify-content: center; height: 100vh; background: #000; color: #fff;",
            div { style: "text-align: center;",
                h1 { "404" }
                p { "Page not found" }
                a { href: "/", style: "color: #7928CA;", "Go Home" }
            }
        }
    }
}

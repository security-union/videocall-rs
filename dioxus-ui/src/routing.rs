/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 */

//! Application route definitions.
//!
//! Extracted into its own module so that both the binary entry-point
//! (`main.rs`) and integration tests can share the same `Route` enum.

use dioxus::prelude::*;

use crate::components::login::Login;
use crate::pages::home::Home;
use crate::pages::meeting::{MeetingPage, MeetingPageProps};

#[derive(Clone, Routable, PartialEq, Debug)]
#[rustfmt::skip]
pub enum Route {
    #[route("/")]
    Home {},
    #[route("/login")]
    Login {},
    #[route("/meeting/:id")]
    Meeting { id: String },
    #[route("/meeting/:id/:webtransport_enabled")]
    Meeting2 { id: String, webtransport_enabled: String },
    #[route("/:..segments")]
    NotFound { segments: Vec<String> },
}

/// Meeting route component - wraps MeetingPage
#[component]
fn Meeting(id: String) -> Element {
    MeetingPage(MeetingPageProps { id })
}

/// Meeting2 route component - wraps MeetingPage (webtransport param ignored for now)
#[component]
fn Meeting2(id: String, webtransport_enabled: String) -> Element {
    // webtransport_enabled is handled via config, not route param
    let _ = webtransport_enabled;
    MeetingPage(MeetingPageProps { id })
}

/// NotFound page component
#[component]
fn NotFound(segments: Vec<String>) -> Element {
    let path = segments.join("/");
    rsx! {
        div { class: "error-container",
            h1 { "404 - Page Not Found" }
            p { "The page you're looking for doesn't exist." }
            p { "Path: /{path}" }
            a { href: "/", "Go Home" }
        }
    }
}

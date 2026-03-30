// SPDX-License-Identifier: MIT OR Apache-2.0

//! Application route definitions.

use dioxus::prelude::*;

use crate::components::login::Login;
use crate::pages::home::Home;
use crate::pages::meeting::MeetingPage;
use crate::pages::meeting_settings::MeetingSettingsPage;
use crate::pages::oauth_callback::OAuthCallback;

#[derive(Clone, Routable, PartialEq, Debug)]
pub enum Route {
    #[route("/")]
    Home {},
    #[route("/login")]
    Login {},
    /// OAuth callback route.
    ///
    /// The identity provider redirects here with `?code=<code>&state=<state>`
    /// after the user authenticates.  The [`OAuthCallback`] component reads
    /// those parameters, calls `POST /api/v1/oauth/exchange` on the
    /// meeting-api, stores the returned id_token in `sessionStorage`, and
    /// navigates to the post-login destination.
    ///
    /// Set `OAUTH_REDIRECT_URL` in the meeting-api configuration to this
    /// route's absolute URL (e.g. `http://localhost:3001/auth/callback`).
    #[route("/auth/callback?:..query_params")]
    OAuthCallback { query_params: String },
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
}

/// Wrapper component for MeetingSettings route.
#[component]
fn MeetingSettings(id: String) -> Element {
    rsx! {
        MeetingSettingsPage { id }
    }
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
        div { style: "display: flex; align-items: center; justify-content: center; height: 100vh; background: #000; color: #fff;",
            div { style: "text-align: center;",
                h1 { "404" }
                p { "Page not found" }
                a { href: "/", style: "color: #7928CA;", "Go Home" }
            }
        }
    }
}

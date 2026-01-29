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
 *
 * Unless you explicitly state otherwise, any contribution intentionallyn
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

mod auth;
#[allow(non_camel_case_types)]
mod components;
mod constants;
mod context;
mod pages;
mod types;

use crate::constants::app_config;

use components::config_error::ConfigError;
use enum_display::EnumDisplay;
use matomo_logger::{MatomoConfig, MatomoLogger};
use pages::home::Home;
use yew::prelude::*;
use yew_router::prelude::*;

use components::login::Login;
use context::{load_username_from_storage, UsernameCtx};
use pages::meeting::MeetingPage;

/// Videocall UI
///
/// This module contains the main entry point for the Videocall UI.
/// It is responsible for rendering the main application and handling routing.
/// We use yew-router to handle routing.
///

#[derive(Clone, Routable, PartialEq, Debug, EnumDisplay)]
enum Route {
    #[at("/")]
    Home,
    #[at("/login")]
    Login,
    #[at("/meeting/:id")]
    Meeting { id: String },
    #[at("/meeting/:id/:webtransport_enabled")]
    Meeting2 {
        id: String,
        webtransport_enabled: String,
    },
    #[not_found]
    #[at("/404")]
    NotFound,
}

fn switch(routes: Route) -> Html {
    if let Err(e) = app_config() {
        return html! { <ConfigError message={e} /> };
    }
    // Track SPA navigation in Matomo (no-op when Matomo disabled)
    matomo_logger::track_page_view(&routes.to_string(), &routes.to_string());
    match routes {
        Route::Home => html! { <Home /> },
        Route::Login => html! { <Login/> },
        Route::Meeting { id } => html! { <MeetingPage id={id} /> },
        Route::Meeting2 {
            id,
            webtransport_enabled: _,
        } => html! { <MeetingPage id={id} /> },
        Route::NotFound => html! { <h1>{ "404" }</h1> },
    }
}

#[function_component(AppRoot)]
fn app_root() -> Html {
    let username_state = use_state(load_username_from_storage);
    html! {
        <ContextProvider<UsernameCtx> context={username_state.clone()}>
            <BrowserRouter>
                <Switch<Route> render={switch} />
            </BrowserRouter>
        </ContextProvider<UsernameCtx>>
    }
}

fn main() {
    // Load runtime config to determine if Matomo is enabled
    let config = app_config().ok();
    let matomo_base_url = config.as_ref().and_then(|c| c.matomo_base_url.clone());
    let matomo_site_id = config.as_ref().and_then(|c| c.matomo_site_id);

    // Matomo is only enabled if both base_url and site_id are provided
    let matomo_enabled = matomo_base_url.is_some() && matomo_site_id.is_some();

    // Initialize unified console + Matomo logging
    let _ = MatomoLogger::init(MatomoConfig {
        base_url: matomo_base_url,
        site_id: matomo_site_id,
        console_level: if cfg!(feature = "debugAssertions") {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        },
        matomo_level: if matomo_enabled {
            log::LevelFilter::Warn
        } else {
            log::LevelFilter::Off
        },
        inject_snippet: matomo_enabled,
        ..Default::default()
    });

    console_error_panic_hook::set_once();
    yew::Renderer::<AppRoot>::new().render();
}

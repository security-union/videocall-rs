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
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

mod auth;
#[allow(non_camel_case_types)]
mod components;
mod constants;
mod context;
mod hooks;
pub mod meeting_api;
mod pages;
mod routing;
mod types;

use crate::constants::app_config;
use crate::routing::Route;

use components::config_error::ConfigError;
use components::login::Login;
use context::{load_username_from_storage, UsernameCtx};
use dioxus::prelude::*;
use matomo_logger::{MatomoConfig, MatomoLogger};
use pages::home::Home;
use pages::meeting::MeetingPage;

/// Route switch component that handles routing
#[component]
fn RouteSwitch() -> Element {
    // Check config validity
    if let Err(e) = app_config() {
        return rsx! {
            ConfigError { message: e }
        };
    }

    rsx! {
        Router::<Route> {}
    }
}

/// App root component
#[component]
fn App() -> Element {
    // Initialize username state from localStorage
    let username = use_signal(load_username_from_storage);

    // Provide username context to the entire app
    use_context_provider(|| username);

    rsx! {
        RouteSwitch {}
    }
}

fn main() {
    // Initialize unified console + Matomo logging
    let _ = MatomoLogger::init(MatomoConfig {
        base_url: Some("https://matomo.videocall.rs/".into()),
        site_id: Some(1),
        console_level: if cfg!(feature = "debugAssertions") {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        },
        matomo_level: log::LevelFilter::Warn,
        ..Default::default()
    });

    console_error_panic_hook::set_once();
    dioxus::launch(App);
}

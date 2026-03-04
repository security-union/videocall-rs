// SPDX-License-Identifier: MIT OR Apache-2.0

mod auth;
#[allow(non_camel_case_types)]
mod components;
mod constants;
mod context;
pub mod meeting_api;
mod pages;
mod routing;
mod types;

use crate::routing::Route;
use context::{load_username_from_storage, UsernameCtx};
use dioxus::prelude::*;
use matomo_logger::{MatomoConfig, MatomoLogger};

fn main() {
    console_error_panic_hook::set_once();

    let _ = MatomoLogger::init(MatomoConfig {
        base_url: Some("https://matomo.videocall.rs/".into()),
        site_id: Some(1),
        console_level: log::LevelFilter::Info,
        matomo_level: log::LevelFilter::Warn,
        ..Default::default()
    });

    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let username = use_signal(load_username_from_storage);
    use_context_provider(|| UsernameCtx(username));

    rsx! {
        Router::<Route> {}
    }
}

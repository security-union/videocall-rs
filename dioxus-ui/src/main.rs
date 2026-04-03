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
use context::{
    load_display_name_from_storage, load_transport_preference, migrate_legacy_storage,
    DisplayNameCtx, TransportPreferenceCtx,
};
use dioxus::prelude::*;
use matomo_logger::{MatomoConfig, MatomoLogger};

fn main() {
    console_error_panic_hook::set_once();

    // Migrate any legacy localStorage keys before the router renders so that
    // returning users keep their display name without re-entry.
    migrate_legacy_storage();

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
    let display_name = use_signal(load_display_name_from_storage);
    use_context_provider(|| DisplayNameCtx(display_name));

    let transport_pref = use_signal(load_transport_preference);
    use_context_provider(|| TransportPreferenceCtx(transport_pref));

    rsx! {
        Router::<Route> {}
    }
}

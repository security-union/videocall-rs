// SPDX-License-Identifier: MIT OR Apache-2.0

mod auth;
#[allow(non_camel_case_types)]
mod components;
mod constants;
mod context;
mod id_token;
pub mod meeting_api;
mod pages;
mod pkce;
mod provider_config;
mod routing;
mod types;

use crate::routing::Route;
use context::{load_display_name_from_storage, migrate_legacy_storage, DisplayNameCtx};
use dioxus::prelude::*;
use matomo_logger::{MatomoConfig, MatomoLogger};

fn main() {
    console_error_panic_hook::set_once();

    // Set the storage directory for the file-system LocalStorage backend.
    // This is a no-op on WASM (web) — on native it resolves to the platform's
    // local app-data directory so that LocalStorage writes go to disk.
    dioxus_sdk_storage::set_dir!();

    // Migrate any legacy plain-string localStorage keys (written by older
    // releases) to the new CBOR+zlib format used by dioxus-sdk-storage.
    // Must run after set_dir!() and before the component tree mounts.
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

    rsx! {
        Router::<Route> {}
    }
}

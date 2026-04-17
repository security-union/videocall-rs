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

use crate::components::search_modal::{SearchModal, SearchVisibleCtx};
use crate::routing::Route;
use context::{
    load_display_name_from_storage, load_transport_preference, migrate_legacy_storage,
    DisplayNameCtx, TransportPreferenceCtx,
};
use dioxus::prelude::*;

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

    console_log::init_with_level(log::Level::Info).expect("Failed to initialize logger");

    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    use_context_provider(|| SearchVisibleCtx { 
        is_visible: Signal::new(false) 
});

    let display_name = use_signal(load_display_name_from_storage);
    use_context_provider(|| DisplayNameCtx(display_name));

    let transport_pref = use_signal(load_transport_preference);
    use_context_provider(|| TransportPreferenceCtx(transport_pref));

    let search_visible = use_signal(|| false);
    use_context_provider(|| SearchVisibleCtx { is_visible: search_visible });

    use_effect(move || {
        use wasm_bindgen::prelude::*;
        use wasm_bindgen::JsCast;
        let window = web_sys::window().unwrap();
        let mut sv = search_visible;
        let closure = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(
            move |evt: web_sys::KeyboardEvent| {
                if evt.key() == "k" && (evt.meta_key() || evt.ctrl_key()) {
                    evt.prevent_default();
                    sv.set(!sv());
                }
            },
        );
        window
            .add_event_listener_with_callback("keydown", closure.as_ref().unchecked_ref())
            .unwrap();
        closure.forget();
    });
    
    rsx! {
        Router::<Route> {}
    }
}


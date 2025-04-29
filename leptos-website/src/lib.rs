#![allow(non_snake_case)]

use cfg_if::cfg_if;
pub mod app;
pub mod components;
pub mod error_template;
pub mod errors;
pub mod fallback;
pub mod pages;
pub mod icons;

cfg_if! {
    if #[cfg(feature = "hydrate")] {
        use leptos::*;

        use wasm_bindgen::prelude::wasm_bindgen;

        #[wasm_bindgen]
        pub fn hydrate() {
            console_error_panic_hook::set_once();
            leptos::leptos_dom::HydrationCtx::stop_hydrating();
        }
    }
}

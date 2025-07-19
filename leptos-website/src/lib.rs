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

#![allow(non_snake_case)]

use cfg_if::cfg_if;
pub mod app;
pub mod components;
pub mod error_template;
pub mod errors;
pub mod fallback;
pub mod icons;
pub mod pages;

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

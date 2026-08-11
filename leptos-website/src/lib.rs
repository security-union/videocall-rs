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
// Leptos 0.7+ builds statically-typed view trees; this page nests deeply enough
// that computing the SSR `AnyView` layout overflows the default recursion limit
// (128). Raise it so the whole document type resolves.
#![recursion_limit = "512"]

pub mod app;
pub mod components;
pub mod error_template;
pub mod errors;
pub mod icons;
pub mod pages;

// Islands hydration entry point. In Leptos 0.7+ islands mode, only the
// interactive `#[island]` components are hydrated on the client; the rest of the
// server-rendered DOM stays static. `hydrate_islands()` wires up exactly those.
#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_islands();
}

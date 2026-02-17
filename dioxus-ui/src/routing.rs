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
 */

//! Application route definitions.
//!
//! Extracted into its own module so that both the binary entry-point
//! (`main.rs`) and integration tests can share the same `Route` enum.

use dioxus::prelude::*;

#[derive(Clone, Routable, PartialEq, Debug)]
#[rustfmt::skip]
pub enum Route {
    #[route("/")]
    Home {},
    #[route("/login")]
    Login {},
    #[route("/meeting/:id")]
    Meeting { id: String },
    #[route("/meeting/:id/:webtransport_enabled")]
    Meeting2 { id: String, webtransport_enabled: String },
    #[route("/:..segments")]
    NotFound { segments: Vec<String> },
}

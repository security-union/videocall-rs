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

use enum_display::EnumDisplay;
use yew_router::prelude::*;

#[derive(Clone, Routable, PartialEq, Debug, EnumDisplay)]
pub enum Route {
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

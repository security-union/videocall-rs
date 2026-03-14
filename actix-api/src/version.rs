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

//! Build version information exposed via HTTP endpoints.

use actix_web::{HttpResponse, Responder};
use serde::Serialize;

/// Compile-time build metadata for a service binary.
#[derive(Serialize)]
pub struct BuildInfo {
    pub service: &'static str,
    pub version: &'static str,
    pub git_sha: &'static str,
    pub git_branch: &'static str,
    pub build_timestamp: &'static str,
}

/// Construct a [`BuildInfo`] for the given service name using compile-time env vars.
pub fn build_info(service: &'static str) -> BuildInfo {
    BuildInfo {
        service,
        version: env!("CARGO_PKG_VERSION"),
        git_sha: env!("GIT_SHA"),
        git_branch: env!("GIT_BRANCH"),
        build_timestamp: env!("BUILD_TIMESTAMP"),
    }
}

/// Handler that returns version info for the websocket relay service.
pub async fn websocket_version() -> impl Responder {
    HttpResponse::Ok().json(build_info("websocket-relay"))
}

/// Handler that returns version info for the webtransport relay service.
pub async fn webtransport_version() -> impl Responder {
    HttpResponse::Ok().json(build_info("webtransport-relay"))
}

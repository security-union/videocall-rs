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

//! CORS configuration constants shared between production and tests.

use axum::http;
use axum::http::HeaderName;

/// The set of HTTP headers allowed in CORS preflight and actual requests.
///
/// This is the single source of truth — used by both the production CORS layer
/// in `main.rs` and the test mirror in `tests/test_helpers.rs`.
pub const ALLOWED_HEADERS: &[HeaderName] = &[
    http::header::CONTENT_TYPE,
    http::header::AUTHORIZATION,
    http::header::COOKIE,
    http::header::ACCEPT,
];

/// Custom (non-standard) headers allowed in CORS requests.
///
/// These are defined as string slices because `HeaderName::from_static` is not
/// const-callable in array context. Callers should map these via
/// `HeaderName::from_static` when building the CORS layer.
pub const ALLOWED_CUSTOM_HEADERS: &[&str] = &["x-user-id", "x-session-timestamp", "x-chunk-seq"];

/// The set of HTTP methods allowed in CORS requests.
pub const ALLOWED_METHODS: &[http::Method] = &[
    http::Method::GET,
    http::Method::POST,
    http::Method::PUT,
    http::Method::DELETE,
    http::Method::PATCH,
    http::Method::OPTIONS,
];

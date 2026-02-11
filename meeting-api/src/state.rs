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

//! Shared application state passed to every Axum handler via `State`.

use crate::config::{Config, OAuthConfig};
use sqlx::PgPool;

/// Application state shared across all request handlers.
#[derive(Clone)]
pub struct AppState {
    /// PostgreSQL connection pool.
    pub db: PgPool,
    /// JWT signing secret (shared with the Media Server).
    pub jwt_secret: String,
    /// Token time-to-live in seconds.
    pub token_ttl_secs: i64,
    /// OAuth configuration. `None` disables OAuth endpoints.
    pub oauth: Option<OAuthConfig>,
    /// Cookie domain (e.g. ".example.com"), or `None` for default.
    pub cookie_domain: Option<String>,
}

impl AppState {
    pub fn new(db: PgPool, config: &Config) -> Self {
        Self {
            db,
            jwt_secret: config.jwt_secret.clone(),
            token_ttl_secs: config.token_ttl_secs,
            oauth: config.oauth.clone(),
            cookie_domain: config.cookie_domain.clone(),
        }
    }
}

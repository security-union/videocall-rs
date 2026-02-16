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

use std::sync::Arc;

use crate::config::{Config, OAuthConfig};
use crate::oauth::JwksCache;
use sqlx::PgPool;

/// Application state shared across all request handlers.
#[derive(Clone)]
pub struct AppState {
    /// PostgreSQL connection pool.
    pub db: PgPool,
    /// JWT signing secret (shared with the Media Server).
    pub jwt_secret: String,
    /// Room access token time-to-live in seconds.
    pub token_ttl_secs: i64,
    /// Session JWT time-to-live in seconds.
    pub session_ttl_secs: i64,
    /// OAuth configuration. `None` disables OAuth endpoints.
    pub oauth: Option<OAuthConfig>,
    /// JWKS key cache for ID token signature verification. `None` when JWKS is
    /// not configured (falls back to unverified decode).
    pub jwks_cache: Option<Arc<JwksCache>>,
    /// Cookie domain (e.g. ".example.com"), or `None` for default.
    pub cookie_domain: Option<String>,
    /// Whether to set the `Secure` flag on cookies.
    pub cookie_secure: bool,
}

impl AppState {
    pub fn new(db: PgPool, config: &Config) -> Self {
        let jwks_cache = config
            .oauth
            .as_ref()
            .and_then(|o| o.jwks_url.as_ref())
            .map(|url| JwksCache::new(url.clone()));

        Self {
            db,
            jwt_secret: config.jwt_secret.clone(),
            token_ttl_secs: config.token_ttl_secs,
            session_ttl_secs: config.session_ttl_secs,
            oauth: config.oauth.clone(),
            jwks_cache,
            cookie_domain: config.cookie_domain.clone(),
            cookie_secure: config.cookie_secure,
        }
    }
}

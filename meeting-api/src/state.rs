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

use crate::config::{Config, OAuthConfig, SearchConfig};
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
    /// Name of the session cookie (default: "session").
    /// Set to a unique value in PR preview environments to avoid collision
    /// with the production cookie that shares the same parent domain.
    pub cookie_name: String,
    /// Whether to set the `Secure` flag on cookies.
    pub cookie_secure: bool,
    /// NATS client for publishing meeting events. `None` when `NATS_URL` is not configured.
    pub nats: Option<async_nats::Client>,
    /// Internal URLs for fetching version info from peer services.
    pub service_version_urls: Vec<String>,
    /// Shared HTTP client for outbound requests (e.g. version fan-out, SearchV2 push).
    pub http_client: reqwest::Client,
    /// SearchV2 integration config. `None` disables the push path entirely;
    /// every [`crate::search`] call becomes a no-op. See [`SearchConfig`].
    pub search: Option<SearchConfig>,
    /// Opt-in anonymous-auth fallback flag (mirrors [`crate::config::Config::allow_anonymous`]).
    /// Only intended for local development; guards path 3 in the auth extractor.
    pub allow_anonymous: bool,
}

impl AppState {
    pub fn new(db: PgPool, config: &Config, nats: Option<async_nats::Client>) -> Self {
        let jwks_cache = config
            .oauth
            .as_ref()
            .and_then(|o| o.jwks_url.as_ref())
            .map(|url| JwksCache::new(url.clone()));

        if jwks_cache.is_some() {
            tracing::warn!(
                "JWKS token validation is active. Audience ('aud') validation is DISABLED \
                 for per-request Bearer tokens: any JWT signed by the configured provider \
                 is accepted regardless of its intended audience. This is safe when the \
                 provider is used exclusively for this service. If the same provider issues \
                 tokens for other services, consider adding OAUTH_RESOURCE_SERVER_AUDIENCE \
                 support to restrict accepted tokens to this resource server."
            );
        }

        Self {
            db,
            jwt_secret: config.jwt_secret.clone(),
            token_ttl_secs: config.token_ttl_secs,
            session_ttl_secs: config.session_ttl_secs,
            oauth: config.oauth.clone(),
            jwks_cache,
            cookie_domain: config.cookie_domain.clone(),
            cookie_name: config.cookie_name.clone(),
            cookie_secure: config.cookie_secure,
            nats,
            service_version_urls: config.service_version_urls.clone(),
            http_client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()
                .expect("failed to build reqwest client"),
            search: config.search.clone(),
            allow_anonymous: config.allow_anonymous,
        }
    }
}

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

//! Application configuration loaded from environment variables.

use std::env;

/// Configuration for the Meeting Backend API.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address to bind the HTTP server (e.g. "0.0.0.0:8081").
    pub listen_addr: String,
    /// PostgreSQL connection string.
    pub database_url: String,
    /// Shared secret used to sign room access tokens (HMAC-SHA256).
    pub jwt_secret: String,
    /// Token time-to-live in seconds (default: 600 = 10 minutes).
    pub token_ttl_secs: i64,
    /// OAuth configuration. `None` if `OAUTH_CLIENT_ID` is unset or empty.
    pub oauth: Option<OAuthConfig>,
    /// Cookie domain (optional, e.g. ".example.com").
    pub cookie_domain: Option<String>,
}

/// Google OAuth configuration.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
    pub auth_url: String,
    pub token_url: String,
    pub after_login_url: String,
}

impl Config {
    /// Load configuration from environment variables.
    ///
    /// # Required
    /// - `DATABASE_URL`
    /// - `JWT_SECRET`
    ///
    /// # Optional
    /// - `LISTEN_ADDR` (default: `"0.0.0.0:8081"`)
    /// - `TOKEN_TTL_SECS` (default: `"600"`)
    /// - `COOKIE_DOMAIN`
    /// - OAuth: `OAUTH_CLIENT_ID`, `OAUTH_SECRET`, `OAUTH_REDIRECT_URL`,
    ///   `OAUTH_AUTH_URL`, `OAUTH_TOKEN_URL`, `AFTER_LOGIN_URL`
    pub fn from_env() -> Result<Self, String> {
        let database_url = env::var("DATABASE_URL")
            .map_err(|_| "DATABASE_URL environment variable is required")?;
        let jwt_secret =
            env::var("JWT_SECRET").map_err(|_| "JWT_SECRET environment variable is required")?;

        let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".to_string());
        let token_ttl_secs = env::var("TOKEN_TTL_SECS")
            .unwrap_or_else(|_| "600".to_string())
            .parse::<i64>()
            .map_err(|_| "TOKEN_TTL_SECS must be a valid integer")?;
        let cookie_domain = env::var("COOKIE_DOMAIN").ok().filter(|s| !s.is_empty());

        let oauth = env::var("OAUTH_CLIENT_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|client_id| {
                Ok::<_, String>(OAuthConfig {
                    client_id,
                    client_secret: env::var("OAUTH_SECRET")
                        .map_err(|_| "OAUTH_SECRET required when OAUTH_CLIENT_ID is set")?,
                    redirect_url: env::var("OAUTH_REDIRECT_URL")
                        .map_err(|_| "OAUTH_REDIRECT_URL required when OAUTH_CLIENT_ID is set")?,
                    auth_url: env::var("OAUTH_AUTH_URL").unwrap_or_else(|_| {
                        "https://accounts.google.com/o/oauth2/v2/auth".to_string()
                    }),
                    token_url: env::var("OAUTH_TOKEN_URL")
                        .unwrap_or_else(|_| "https://oauth2.googleapis.com/token".to_string()),
                    after_login_url: env::var("AFTER_LOGIN_URL")
                        .unwrap_or_else(|_| "/".to_string()),
                })
            })
            .transpose()?;

        Ok(Self {
            listen_addr,
            database_url,
            jwt_secret,
            token_ttl_secs,
            oauth,
            cookie_domain,
        })
    }
}

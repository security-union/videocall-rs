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
    /// Room access token time-to-live in seconds (default: 60 = 1 minute).
    /// Tokens are "single-burner": short-lived admission tickets that the UI
    /// refreshes automatically on every reconnect.
    pub token_ttl_secs: i64,
    /// Session JWT time-to-live in seconds (default: 315360000 = ~10 years).
    pub session_ttl_secs: i64,
    /// OAuth configuration. `None` if `OAUTH_CLIENT_ID` is unset or empty.
    pub oauth: Option<OAuthConfig>,
    /// Cookie domain (optional, e.g. ".example.com").
    pub cookie_domain: Option<String>,
    /// Name of the session cookie (default: "session").
    /// Override in PR preview environments to avoid collisions with the
    /// production cookie (which also uses `session` on a parent domain).
    pub cookie_name: String,
    /// Whether to set the `Secure` flag on cookies (default: true).
    /// Set `COOKIE_SECURE=false` for local development over HTTP.
    pub cookie_secure: bool,
    /// Explicit CORS allowed origins for production (e.g. "https://app.videocall.rs").
    /// Comma-separated for multiple origins. Empty list mirrors the request origin (development only).
    pub cors_allowed_origin: Vec<String>,
    /// NATS server URL (e.g. "nats://localhost:4222"). `None` if `NATS_URL` is unset.
    /// When not configured, NATS event publishing is silently skipped (graceful degradation).
    pub nats_url: Option<String>,
    /// Internal URLs for fetching version info from peer services.
    /// Used by the aggregated `/api/v1/versions` endpoint.
    pub service_version_urls: Vec<String>,
    /// CLI device-key settings (`DEVICE_*`, `TRUST_X_FORWARDED_FOR`).
    pub device: DeviceAuthConfig,
}

/// CLI device-key settings.
#[derive(Debug, Clone)]
pub struct DeviceAuthConfig {
    /// `DEVICE_TOKEN_AUDIENCE`: this server's public origin, e.g.
    /// `https://api.videocall.rs`. It is the `aud` of every device JWT and
    /// registration JWS, the only accepted `Origin` on the consent form, the
    /// base of the remote `redirect_uri`, and the origin of the closed
    /// post-login return targets. `None` disables device auth.
    pub audience: Option<String>,
    /// `DEVICE_IDLE_TTL_SECS`: an approved device dies after this long unused.
    pub idle_ttl_secs: i64,
    /// `DEVICE_ABS_TTL_SECS`: an approved device dies this long after approval.
    pub abs_ttl_secs: i64,
    /// `DEVICE_MAX_PER_USER`: live approved devices per user.
    pub max_per_user: i64,
    /// `DEVICE_MAX_PENDING`: unexpired login requests, globally.
    pub max_pending: i64,
    /// `DEVICE_MAX_PENDING_PER_IP`: unexpired login requests per requester IP.
    pub max_pending_per_ip: i64,
    /// `TRUST_X_FORWARDED_FOR`: take the requester IP from the right-most
    /// `X-Forwarded-For` entry instead of the TCP peer.
    pub trust_x_forwarded_for: bool,
}

pub const DEFAULT_DEVICE_IDLE_TTL_SECS: i64 = 90 * 24 * 3600;
pub const DEFAULT_DEVICE_ABS_TTL_SECS: i64 = 365 * 24 * 3600;
pub const DEFAULT_DEVICE_MAX_PER_USER: i64 = 10;
pub const DEFAULT_DEVICE_MAX_PENDING: i64 = 10_000;
pub const DEFAULT_DEVICE_MAX_PENDING_PER_IP: i64 = 5;

impl Default for DeviceAuthConfig {
    fn default() -> Self {
        Self {
            audience: None,
            idle_ttl_secs: DEFAULT_DEVICE_IDLE_TTL_SECS,
            abs_ttl_secs: DEFAULT_DEVICE_ABS_TTL_SECS,
            max_per_user: DEFAULT_DEVICE_MAX_PER_USER,
            max_pending: DEFAULT_DEVICE_MAX_PENDING,
            max_pending_per_ip: DEFAULT_DEVICE_MAX_PENDING_PER_IP,
            trust_x_forwarded_for: false,
        }
    }
}

impl DeviceAuthConfig {
    /// Read the device settings from the environment.
    pub fn from_env() -> Result<Self, String> {
        let var = |name: &str| env::var(name).ok().filter(|v| !v.trim().is_empty());
        let positive = |name: &str, default: i64| -> Result<i64, String> {
            match var(name) {
                None => Ok(default),
                Some(v) => match v.trim().parse::<i64>() {
                    Ok(n) if n > 0 => Ok(n),
                    _ => Err(format!("{name} must be a positive integer")),
                },
            }
        };
        let audience = var("DEVICE_TOKEN_AUDIENCE")
            .map(|s| {
                parse_origin(s.trim())
                    .ok_or_else(|| format!("DEVICE_TOKEN_AUDIENCE is not an http(s) origin: {s}"))
            })
            .transpose()?;
        let trust_x_forwarded_for = match var("TRUST_X_FORWARDED_FOR") {
            None => false,
            Some(v) => match v.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => true,
                "false" | "0" | "no" => false,
                _ => return Err("TRUST_X_FORWARDED_FOR must be true or false".into()),
            },
        };
        Ok(Self {
            audience,
            idle_ttl_secs: positive("DEVICE_IDLE_TTL_SECS", DEFAULT_DEVICE_IDLE_TTL_SECS)?,
            abs_ttl_secs: positive("DEVICE_ABS_TTL_SECS", DEFAULT_DEVICE_ABS_TTL_SECS)?,
            max_per_user: positive("DEVICE_MAX_PER_USER", DEFAULT_DEVICE_MAX_PER_USER)?,
            max_pending: positive("DEVICE_MAX_PENDING", DEFAULT_DEVICE_MAX_PENDING)?,
            max_pending_per_ip: positive(
                "DEVICE_MAX_PENDING_PER_IP",
                DEFAULT_DEVICE_MAX_PENDING_PER_IP,
            )?,
            trust_x_forwarded_for,
        })
    }
}

/// OAuth/OIDC configuration — provider-agnostic.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    /// Client secret. `None` for public clients (e.g. native apps using PKCE only).
    pub client_secret: Option<String>,
    pub redirect_url: String,
    /// OIDC issuer URL (e.g. "https://accounts.google.com"). Used for discovery
    /// and JWT `iss` validation.
    pub issuer: Option<String>,
    /// Authorization endpoint URL.
    pub auth_url: String,
    /// Token endpoint URL.
    pub token_url: String,
    /// JWKS endpoint URL for ID token signature verification.
    pub jwks_url: Option<String>,
    /// UserInfo endpoint URL. Used as fallback when ID token lacks email claim.
    pub userinfo_url: Option<String>,
    /// Space-separated OAuth scopes (default: "openid email profile").
    pub scopes: String,
    pub after_login_url: String,
    /// Comma-separated list of allowed redirect URL origins (e.g.
    /// "http://localhost:80,http://localhost:3001"). The origin of
    /// `after_login_url` is implicitly allowed.
    pub allowed_redirect_urls: Vec<String>,
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
    /// - `TOKEN_TTL_SECS` (default: `"60"`)
    /// - `COOKIE_DOMAIN`
    /// - `COOKIE_NAME` (default: `"session"`) — set to a unique value (e.g. `"pr-session"`)
    ///   in PR preview environments to avoid collision with the production cookie
    /// - OAuth: `OAUTH_CLIENT_ID`, `OAUTH_SECRET` (optional), `OAUTH_REDIRECT_URL`,
    ///   `OAUTH_ISSUER`, `OAUTH_AUTH_URL`, `OAUTH_TOKEN_URL`, `OAUTH_JWKS_URL`,
    ///   `OAUTH_USERINFO_URL`, `OAUTH_SCOPES` (default: `"openid email profile"`),
    ///   `AFTER_LOGIN_URL`
    /// - `CORS_ALLOWED_ORIGIN` (production: e.g. `"https://app.videocall.rs"` or comma-separated for multiple origins)
    pub fn from_env() -> Result<Self, String> {
        let database_url = env::var("DATABASE_URL")
            .map_err(|_| "DATABASE_URL environment variable is required")?;
        let jwt_secret =
            env::var("JWT_SECRET").map_err(|_| "JWT_SECRET environment variable is required")?;

        let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".to_string());
        let token_ttl_secs = env::var("TOKEN_TTL_SECS")
            .unwrap_or_else(|_| "60".to_string())
            .parse::<i64>()
            .map_err(|_| "TOKEN_TTL_SECS must be a valid integer")?;
        let session_ttl_secs = env::var("SESSION_TTL_SECS")
            .unwrap_or_else(|_| "315360000".to_string()) // ~10 years
            .parse::<i64>()
            .map_err(|_| "SESSION_TTL_SECS must be a valid integer")?;
        let cookie_domain = env::var("COOKIE_DOMAIN").ok().filter(|s| !s.is_empty());
        let cookie_name = env::var("COOKIE_NAME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "session".to_string());
        let cookie_secure = env::var("COOKIE_SECURE")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);
        let cors_allowed_origin = env::var("CORS_ALLOWED_ORIGIN")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(|o| o.trim().to_string()).collect())
            .unwrap_or_default();
        let nats_url = env::var("NATS_URL").ok().filter(|s| !s.is_empty());

        // Internal URLs for the aggregated /api/v1/versions endpoint.
        // Comma-separated list, e.g. "http://rustlemania-websocket:8080/version,http://rustlemania-webtransport:444/version"
        let service_version_urls = env::var("SERVICE_VERSION_URLS")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(|u| u.trim().to_string()).collect())
            .unwrap_or_default();

        let device = DeviceAuthConfig::from_env()?;

        let oauth = env::var("OAUTH_CLIENT_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|client_id| {
                let client_secret = env::var("OAUTH_SECRET").ok().filter(|s| !s.is_empty());
                let issuer = env::var("OAUTH_ISSUER").ok().filter(|s| !s.is_empty());
                let auth_url = env::var("OAUTH_AUTH_URL").ok().filter(|s| !s.is_empty());
                let token_url = env::var("OAUTH_TOKEN_URL").ok().filter(|s| !s.is_empty());
                let jwks_url = env::var("OAUTH_JWKS_URL").ok().filter(|s| !s.is_empty());
                let userinfo_url = env::var("OAUTH_USERINFO_URL")
                    .ok()
                    .filter(|s| !s.is_empty());
                let scopes = env::var("OAUTH_SCOPES")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "openid email profile".to_string());

                // When no issuer is set, auth_url and token_url must be provided manually.
                let auth_url = match auth_url {
                    Some(u) => u,
                    None if issuer.is_some() => {
                        // Will be filled in by resolve_discovery().
                        String::new()
                    }
                    None => {
                        return Err(
                            "OAUTH_AUTH_URL required when OAUTH_ISSUER is not set".to_string()
                        );
                    }
                };
                let token_url = match token_url {
                    Some(u) => u,
                    None if issuer.is_some() => String::new(),
                    None => {
                        return Err(
                            "OAUTH_TOKEN_URL required when OAUTH_ISSUER is not set".to_string()
                        );
                    }
                };

                Ok::<_, String>(OAuthConfig {
                    client_id,
                    client_secret,
                    redirect_url: env::var("OAUTH_REDIRECT_URL")
                        .map_err(|_| "OAUTH_REDIRECT_URL required when OAUTH_CLIENT_ID is set")?,
                    issuer,
                    auth_url,
                    token_url,
                    jwks_url,
                    userinfo_url,
                    scopes,
                    after_login_url: env::var("AFTER_LOGIN_URL")
                        .unwrap_or_else(|_| "/".to_string()),
                    allowed_redirect_urls: {
                        let urls: Vec<String> = env::var("ALLOWED_REDIRECT_URLS")
                            .ok()
                            .filter(|s| !s.is_empty())
                            .map(|s| s.split(',').map(|u| u.trim().to_string()).collect())
                            .unwrap_or_default();
                        // Validate at startup so malformed entries fail fast.
                        for u in &urls {
                            url::Url::parse(u).map_err(|e| {
                                format!("ALLOWED_REDIRECT_URLS contains invalid URL {u:?}: {e}")
                            })?;
                        }
                        urls
                    },
                })
            })
            .transpose()?;

        Ok(Self {
            listen_addr,
            database_url,
            jwt_secret,
            token_ttl_secs,
            session_ttl_secs,
            oauth,
            cookie_domain,
            cookie_name,
            cookie_secure,
            cors_allowed_origin,
            nats_url,
            service_version_urls,
            device,
        })
    }

    /// Perform OIDC discovery to fill in missing OAuth endpoint URLs.
    ///
    /// Call this after `from_env()`. When `OAUTH_ISSUER` is set, fetches the
    /// provider's `.well-known/openid-configuration` and uses discovered endpoints
    /// as defaults (manual overrides via env vars take precedence).
    pub async fn resolve_discovery(&mut self) -> Result<(), String> {
        let oauth = match &mut self.oauth {
            Some(o) => o,
            None => return Ok(()),
        };

        let issuer = match &oauth.issuer {
            Some(iss) => iss.clone(),
            None => return Ok(()),
        };

        tracing::info!("Running OIDC discovery for issuer: {issuer}");

        let endpoints = crate::oauth::discover_oidc_endpoints(&issuer)
            .await
            .map_err(|e| format!("OIDC discovery failed: {e:?}"))?;

        if oauth.auth_url.is_empty() {
            oauth.auth_url = endpoints.authorization_endpoint;
        }
        if oauth.token_url.is_empty() {
            oauth.token_url = endpoints.token_endpoint;
        }
        if oauth.jwks_url.is_none() {
            oauth.jwks_url = endpoints.jwks_uri;
        }
        if oauth.userinfo_url.is_none() {
            oauth.userinfo_url = endpoints.userinfo_endpoint;
        }

        tracing::info!(
            "OIDC discovery complete: auth_url={}, token_url={}, jwks_url={:?}, userinfo_url={:?}",
            oauth.auth_url,
            oauth.token_url,
            oauth.jwks_url,
            oauth.userinfo_url
        );

        Ok(())
    }
}

/// Parse `value` as an absolute http(s) URL and return its ASCII origin
/// (`scheme://host[:port]`, no trailing slash). `None` if it is not one.
pub fn parse_origin(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

#[cfg(test)]
mod origin_tests {
    use super::parse_origin;

    #[test]
    fn parse_origin_normalizes() {
        assert_eq!(
            parse_origin("https://api.videocall.rs/").as_deref(),
            Some("https://api.videocall.rs")
        );
        assert_eq!(
            parse_origin("http://localhost:8081").as_deref(),
            Some("http://localhost:8081")
        );
        assert_eq!(
            parse_origin("https://api.videocall.rs:443").as_deref(),
            Some("https://api.videocall.rs")
        );
        assert_eq!(parse_origin("ftp://x"), None);
        assert_eq!(parse_origin("not a url"), None);
    }
}

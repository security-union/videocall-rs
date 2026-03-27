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
    /// End-session endpoint for RP-initiated logout (OIDC RP-Initiated Logout
    /// 1.0). Discovered from the provider's OpenID Configuration when
    /// `OAUTH_ISSUER` is set, or overridden via `OAUTH_END_SESSION_URL`.
    ///
    /// When set, `GET /logout` redirects the browser to this URL (with
    /// `client_id` and optionally `post_logout_redirect_uri`) after clearing
    /// the local session cookie, so the provider also terminates the session.
    pub end_session_endpoint: Option<String>,
    /// URL to redirect to after the provider has completed logout
    /// (`post_logout_redirect_uri` sent to `end_session_endpoint`).
    /// Configured via `AFTER_LOGOUT_URL`. When not set, the parameter is
    /// omitted from the end-session redirect.
    pub after_logout_url: Option<String>,
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
    /// - OIDC logout: `OAUTH_END_SESSION_URL` (manual override; auto-discovered
    ///   from `OAUTH_ISSUER` when not set), `AFTER_LOGOUT_URL` (sent as
    ///   `post_logout_redirect_uri` to the provider's end-session endpoint)
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
                let end_session_url = env::var("OAUTH_END_SESSION_URL")
                    .ok()
                    .filter(|s| !s.is_empty());
                let scopes = env::var("OAUTH_SCOPES")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "openid email profile".to_string());

                // post_logout_redirect_uri sent to the provider's end-session endpoint.
                let after_logout_url = env::var("AFTER_LOGOUT_URL").ok().filter(|s| !s.is_empty());

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
                    end_session_endpoint: end_session_url,
                    after_logout_url,
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
        if oauth.end_session_endpoint.is_none() {
            oauth.end_session_endpoint = endpoints.end_session_endpoint;
        }

        tracing::info!(
            "OIDC discovery complete: auth_url={}, token_url={}, jwks_url={:?}, \
             userinfo_url={:?}, end_session_endpoint={:?}",
            oauth.auth_url,
            oauth.token_url,
            oauth.jwks_url,
            oauth.userinfo_url,
            oauth.end_session_endpoint,
        );

        Ok(())
    }
}

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

use std::collections::HashMap;
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
    /// Room access token time-to-live in seconds (default: 600 = 10 minutes).
    pub token_ttl_secs: i64,
    /// Session JWT time-to-live in seconds (default: 315360000 = ~10 years).
    pub session_ttl_secs: i64,
    /// OAuth configuration. `None` if `OAUTH_CLIENT_ID` is unset or empty.
    pub oauth: Option<OAuthConfig>,
    /// Cookie domain (optional, e.g. ".example.com").
    pub cookie_domain: Option<String>,
    /// Whether to set the `Secure` flag on cookies (default: true).
    /// Set `COOKIE_SECURE=false` for local development over HTTP.
    pub cookie_secure: bool,
    /// Explicit CORS allowed origin for production (e.g. "https://app.videocall.rs").
    /// When `None`, the server mirrors the request origin (development only).
    pub cors_allowed_origin: Option<String>,
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
    /// OIDC `prompt` parameter (e.g. "select_account", "login", "consent").
    /// Omitted from the auth URL when `None`.
    pub prompt: Option<String>,
    /// Extra query parameters appended to the authorization URL.
    /// Useful for provider-specific params like Google's `access_type=offline`.
    pub extra_auth_params: HashMap<String, String>,
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
    /// - OAuth: `OAUTH_CLIENT_ID`, `OAUTH_SECRET` (optional), `OAUTH_REDIRECT_URL`,
    ///   `OAUTH_ISSUER`, `OAUTH_AUTH_URL`, `OAUTH_TOKEN_URL`, `OAUTH_JWKS_URL`,
    ///   `OAUTH_USERINFO_URL`, `OAUTH_SCOPES` (default: `"openid email profile"`),
    ///   `AFTER_LOGIN_URL`
    /// - `CORS_ALLOWED_ORIGIN` (production: e.g. `"https://app.videocall.rs"`)
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
        let session_ttl_secs = env::var("SESSION_TTL_SECS")
            .unwrap_or_else(|_| "315360000".to_string()) // ~10 years
            .parse::<i64>()
            .map_err(|_| "SESSION_TTL_SECS must be a valid integer")?;
        let cookie_domain = env::var("COOKIE_DOMAIN").ok().filter(|s| !s.is_empty());
        let cookie_secure = env::var("COOKIE_SECURE")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);
        let cors_allowed_origin = env::var("CORS_ALLOWED_ORIGIN")
            .ok()
            .filter(|s| !s.is_empty());

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

                let prompt = env::var("OAUTH_PROMPT").ok().filter(|s| !s.is_empty());

                let extra_auth_params: HashMap<String, String> = env::var("OAUTH_EXTRA_PARAMS")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(|json| {
                        serde_json::from_str(&json).map_err(|e| {
                            format!(
                                "OAUTH_EXTRA_PARAMS must be a JSON object \
                                 (e.g. {{\"access_type\":\"offline\"}}): {e}"
                            )
                        })
                    })
                    .transpose()?
                    .unwrap_or_default();

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
                    prompt,
                    extra_auth_params,
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
            cookie_secure,
            cors_allowed_origin,
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

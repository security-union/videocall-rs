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
    /// Room access token time-to-live in seconds (default: 86400 = 24 hours).
    /// Must cover the longest expected meeting plus any connection re-election —
    /// see [discussion #562](https://github01.hclpnp.com/labs-projects/videocall/discussions/562).
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
    /// SearchV2 integration config. `None` when the middleware is not configured
    /// (both `SEARCH_API_URL` and `SEARCH_API_TOKEN` must be set); search push
    /// becomes a no-op in that case. See [`crate::search`].
    pub search: Option<SearchConfig>,
    /// Allow unauthenticated requests to resolve to a stable "anonymous" user
    /// identity (path 3 in [`crate::auth::AuthUser`]).  Controlled by
    /// `ALLOW_ANONYMOUS=true`.  Default `false` — production must set this
    /// explicitly off (or leave unset); only flip it for local development
    /// when running without an OAuth provider.
    pub allow_anonymous: bool,
    /// Disable the per-user display-name rename rate limiter.  Controlled by
    /// `DISPLAY_NAME_RATE_LIMIT_DISABLED=true`.  Default `false` — production
    /// must keep the limiter active so that runaway clients can't churn
    /// `display_name` updates and amplify NATS broadcasts to every meeting
    /// participant.  The flag exists only for local development and the E2E
    /// test harness, where the parallel Playwright workers legitimately
    /// exceed the 5-renames-per-60s budget shared per `(user_id)` across
    /// every test in a window.
    pub display_name_rate_limit_disabled: bool,
    /// Dev-only auto-login user. `None` unless `DEV_USER` is set AND OAuth is
    /// disabled. See [`DevUser`] for format and security warnings.
    pub dev_user: Option<DevUser>,
    /// Explicit opt-in for running OAuth without JWKS-backed ID token
    /// verification. Controlled by `OAUTH_ALLOW_UNVERIFIED=true`.
    ///
    /// Default `false`. When `true`, startup logs an unmistakable error banner
    /// and the OAuth callback falls back to
    /// `decode_id_token_claims_unverified(...)`. Production should never rely
    /// on this except as a deliberately reviewed break-glass mode.
    pub oauth_allow_unverified: bool,
}

/// SearchV2 / opensearch-middleware integration configuration.
///
/// When present on [`Config`] / [`crate::state::AppState`], the meeting-api
/// pushes meeting lifecycle documents to the middleware's content-push API and
/// deletes them on hard deletion.  When `None`, every push is a no-op — the
/// SearchV2 integration degrades gracefully.
///
/// Populated from the `SEARCH_API_URL` and `SEARCH_API_TOKEN` env vars; both
/// must be set for the struct to be constructed.
#[derive(Debug, Clone)]
pub struct SearchConfig {
    /// Base URL of the SearchV2 middleware (no trailing slash required),
    /// e.g. `http://localhost:3000/api/search/v2`.
    pub base_url: String,
    /// Bearer token used for content-push authentication (a middleware JWT
    /// with `pushadmin` or `searchadmin` role).  Not shared with end users —
    /// this is a server-to-server admin token.
    pub token: String,
}

/// Dev-only auto-login user. Parsed from the `DEV_USER` env var
/// (format: `email:display_name`). Only active when OAuth is disabled.
///
/// **WARNING**: This must NEVER be set in production. When set, anyone can
/// obtain a valid session by visiting `/api/v1/dev/auto-login`.
#[derive(Debug, Clone)]
pub struct DevUser {
    /// Email address used as the `sub` claim in the session JWT.
    pub email: String,
    /// Display name used as the `name` claim in the session JWT.
    pub name: String,
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
    /// When `true`, the `GET /login/callback` handler skips session-cookie
    /// issuance after a successful token exchange.  Set this only after
    /// pointing `OAUTH_REDIRECT_URL` at the dioxus-ui `/auth/callback` route
    /// (browser PKCE mode) and verifying the UI handles its own token storage.
    ///
    /// **Default: `false`** — existing deployments that route the provider
    /// callback through the backend (`/login/callback`) continue to receive a
    /// session cookie and require no configuration change.  Flip to `true`
    /// only once `OAUTH_REDIRECT_URL` has been updated to the UI route.
    pub browser_pkce: bool,
    /// Audience value that per-request Bearer tokens must carry in their `aud`
    /// claim.  When `Some`, every token validated by the `AuthUser` extractor
    /// must list this value in `aud`; tokens whose `aud` does not match are
    /// rejected with 401 regardless of signature validity.
    ///
    /// Set via `OAUTH_RESOURCE_SERVER_AUDIENCE`.  Recommended for deployments
    /// that share an identity provider with other services (Keycloak, Okta,
    /// Entra) — without this, any JWT signed by the same IdP is accepted,
    /// including tokens issued for unrelated client applications (confused
    /// deputy risk, RFC 8707).
    ///
    /// When `None` (the default), audience validation is skipped on the
    /// per-request path so that both id_tokens (`aud = client_id`) and access
    /// tokens (`aud = resource-server URL`) continue to work.
    pub resource_server_audience: Option<String>,
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
    /// - `TOKEN_TTL_SECS` (default: `"86400"`) — room access token lifetime in seconds.
    ///   MUST be long enough to cover the duration of any meeting a client might join
    ///   plus connection re-election. Setting this too short causes cached tokens in
    ///   WT/WS URLs to expire before a re-election can complete, stranding users with
    ///   "No valid connections". See
    ///   [discussion #562](https://github01.hclpnp.com/labs-projects/videocall/discussions/562).
    /// - `COOKIE_DOMAIN`
    /// - `COOKIE_NAME` (default: `"session"`) — set to a unique value (e.g. `"pr-session"`)
    ///   in PR preview environments to avoid collision with the production cookie
    /// - OAuth: `OAUTH_CLIENT_ID`, `OAUTH_SECRET` (optional), `OAUTH_REDIRECT_URL`,
    ///   `OAUTH_ISSUER`, `OAUTH_AUTH_URL`, `OAUTH_TOKEN_URL`, `OAUTH_JWKS_URL`,
    ///   `OAUTH_USERINFO_URL`, `OAUTH_SCOPES` (default: `"openid email profile"`),
    ///   `AFTER_LOGIN_URL`, `OAUTH_BROWSER_PKCE` (default: `false`),
    ///   `OAUTH_RESOURCE_SERVER_AUDIENCE` (optional; restricts per-request `aud`),
    ///   `OAUTH_ALLOW_UNVERIFIED` (default: `false`; required to explicitly opt
    ///   into insecure unsigned-ID-token acceptance when no JWKS URL is available)
    /// - OIDC logout: `OAUTH_END_SESSION_URL` (manual override; auto-discovered
    ///   from `OAUTH_ISSUER` when not set), `AFTER_LOGOUT_URL` (sent as
    ///   `post_logout_redirect_uri` to the provider's end-session endpoint)
    /// - `CORS_ALLOWED_ORIGIN` (production: e.g. `"https://app.videocall.rs"` or comma-separated for multiple origins)
    /// - `SEARCH_API_URL` + `SEARCH_API_TOKEN` (both required together to enable SearchV2 push;
    ///   either missing → push is silently disabled). See [`SearchConfig`].
    /// - `ALLOW_ANONYMOUS` (default: `false`) — set to `"true"` / `"1"` for local development
    ///   only. When enabled, unauthenticated requests resolve to a stable anonymous user
    ///   identity instead of returning 401.
    /// - `DISPLAY_NAME_RATE_LIMIT_DISABLED` (default: `false`) — set to `"true"` / `"1"`
    ///   for the E2E test harness only. When enabled, the per-user display-name rename
    ///   rate limiter (5 renames per 60-second window) is bypassed entirely. Required
    ///   for the Playwright suite, which runs many tests in parallel under the same
    ///   `dev_user` identity and would otherwise hit
    ///   `RATE_LIMIT_EXCEEDED` cascades on `POST /api/v1/meetings/{id}/join`.
    pub fn from_env() -> Result<Self, String> {
        let database_url = env::var("DATABASE_URL")
            .map_err(|_| "DATABASE_URL environment variable is required")?;
        let jwt_secret =
            env::var("JWT_SECRET").map_err(|_| "JWT_SECRET environment variable is required")?;

        let oauth_allow_unverified = env::var("OAUTH_ALLOW_UNVERIFIED")
            .map(|v| {
                let v = v.trim().to_lowercase();
                v == "true" || v == "1"
            })
            .unwrap_or(false);

        let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".to_string());
        // Default: 24 hours. Must exceed both the longest expected meeting and
        // the client's connection re-election window, or cached tokens in WT/WS
        // URLs expire mid-meeting and re-election fails. See discussion #562.
        let token_ttl_secs = env::var("TOKEN_TTL_SECS")
            .unwrap_or_else(|_| "86400".to_string())
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

        // SearchV2 push integration is enabled only when both env vars are set.
        // Missing either one → no-op pushes (graceful degradation). We log the
        // decision once at startup so operators can see why search isn't firing.
        let search = match (
            env::var("SEARCH_API_URL").ok().filter(|s| !s.is_empty()),
            env::var("SEARCH_API_TOKEN").ok().filter(|s| !s.is_empty()),
        ) {
            (Some(base_url), Some(token)) => {
                tracing::info!("SearchV2 push enabled (base_url={base_url})");
                Some(SearchConfig { base_url, token })
            }
            (Some(_), None) => {
                tracing::info!(
                    "SearchV2 push disabled: SEARCH_API_URL set but SEARCH_API_TOKEN is missing"
                );
                None
            }
            (None, Some(_)) => {
                tracing::info!(
                    "SearchV2 push disabled: SEARCH_API_TOKEN set but SEARCH_API_URL is missing"
                );
                None
            }
            (None, None) => {
                tracing::info!("SearchV2 push disabled (SEARCH_API_URL not set)");
                None
            }
        };

        // Anonymous auth fallback — opt-in via ALLOW_ANONYMOUS.  Accept the
        // common truthy forms ("true"/"1", case-insensitive) and default to
        // false so production deployments never allow anonymous by accident.
        let allow_anonymous = env::var("ALLOW_ANONYMOUS")
            .map(|v| {
                let v = v.trim().to_lowercase();
                v == "true" || v == "1"
            })
            .unwrap_or(false);
        if allow_anonymous {
            tracing::warn!(
                "ALLOW_ANONYMOUS=true — unauthenticated requests will resolve to \
                 anonymous identities. This is intended for local development only; \
                 do not enable in production."
            );
        }

        // Display-name rate-limit bypass — opt-in via
        // DISPLAY_NAME_RATE_LIMIT_DISABLED.  Same truthy-form parsing as
        // ALLOW_ANONYMOUS so the two flags compose cleanly in dev/CI envs.
        // Production MUST leave this off so a misbehaving client cannot churn
        // rename requests and amplify NATS broadcasts to every participant.
        let display_name_rate_limit_disabled = env::var("DISPLAY_NAME_RATE_LIMIT_DISABLED")
            .map(|v| {
                let v = v.trim().to_lowercase();
                v == "true" || v == "1"
            })
            .unwrap_or(false);
        if display_name_rate_limit_disabled {
            tracing::warn!(
                "DISPLAY_NAME_RATE_LIMIT_DISABLED=true — display-name rename rate \
                 limiter is bypassed.  This is intended for the E2E test harness \
                 only; do not enable in production."
            );
        }

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

                // When true, GET /login/callback will not issue a session cookie.
                // Leave false (the default) for any deployment still routing the
                // provider redirect through the backend /login/callback handler.
                let browser_pkce = env::var("OAUTH_BROWSER_PKCE")
                    .map(|v| v.to_lowercase() == "true" || v == "1")
                    .unwrap_or(false);

                // Audience restriction for per-request Bearer token validation.
                // When set, tokens whose `aud` claim does not contain this value
                // are rejected (RFC 8707 / confused deputy mitigation).
                let resource_server_audience = env::var("OAUTH_RESOURCE_SERVER_AUDIENCE")
                    .ok()
                    .filter(|s| !s.is_empty());

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
                    browser_pkce,
                    resource_server_audience,
                })
            })
            .transpose()?;

        // DEV_USER: only active when OAuth is disabled.
        let dev_user = if oauth.is_none() {
            env::var("DEV_USER")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|raw| {
                    let (email, name) = raw.split_once(':').ok_or_else(|| {
                        "DEV_USER must be in format 'email:display_name' (e.g. 'dev@test.local:Dev User')".to_string()
                    })?;
                    let email = email.trim().to_string();
                    let name = name.trim().to_string();
                    if email.is_empty() || name.is_empty() {
                        return Err("DEV_USER email and display_name must not be empty".to_string());
                    }
                    Ok(DevUser { email, name })
                })
                .transpose()?
        } else {
            // Silently ignore DEV_USER when OAuth is enabled — it would be a
            // security risk to honour it alongside real auth.
            if env::var("DEV_USER")
                .ok()
                .filter(|s| !s.is_empty())
                .is_some()
            {
                tracing::warn!(
                    "DEV_USER is set but OAuth is enabled — ignoring DEV_USER for safety"
                );
            }
            None
        };

        if search.is_some() && dev_user.is_some() {
            tracing::warn!(
                "DEV_USER auto-login is active and SearchV2 is enabled — search pushes will use the synthetic dev identity"
            );
        }

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
            search,
            allow_anonymous,
            display_name_rate_limit_disabled,
            dev_user,
            oauth_allow_unverified,
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

    /// Enforce the OAuth/JWKS security contract after discovery has had a
    /// chance to populate any missing provider endpoints.
    pub fn validate_oauth_security(&self) -> Result<(), String> {
        let Some(oauth) = &self.oauth else {
            return Ok(());
        };

        if oauth.jwks_url.is_some() {
            return Ok(());
        }

        if self.oauth_allow_unverified {
            tracing::error!(
                "\n\
                 ================================================================\n\
                 OAUTH_ALLOW_UNVERIFIED=true and no JWKS URL is configured.\n\
                 meeting-api will accept ID tokens without signature verification.\n\
                 This mode is insecure and must only be used as an explicit,\n\
                 temporary break-glass override for development or controlled\n\
                 triage. Configure OAUTH_JWKS_URL or OAUTH_ISSUER to restore\n\
                 signed-token verification.\n\
                 ================================================================"
            );
            return Ok(());
        }

        Err(
            "OAuth is enabled but no JWKS URL is configured. Set OAUTH_ISSUER \
             or OAUTH_JWKS_URL so ID tokens can be verified, or explicitly opt \
             in to insecure startup with OAUTH_ALLOW_UNVERIFIED=true."
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Run `body` with `key` removed from the process environment, then restore
    /// the prior value. Required because [`Config::from_env`] reads process env,
    /// which is shared across the test binary's parallel runners.
    fn with_env_unset<F: FnOnce()>(key: &str, body: F) {
        let prior = std::env::var(key).ok();
        std::env::remove_var(key);
        body();
        if let Some(v) = prior {
            std::env::set_var(key, v);
        }
    }

    fn base_config() -> Config {
        Config {
            listen_addr: "127.0.0.1:8081".to_string(),
            database_url: "postgres://test/test".to_string(),
            jwt_secret: "secret".to_string(),
            token_ttl_secs: 86400,
            session_ttl_secs: 315360000,
            oauth: None,
            cookie_domain: None,
            cookie_name: "session".to_string(),
            cookie_secure: true,
            cors_allowed_origin: Vec::new(),
            nats_url: None,
            service_version_urls: Vec::new(),
            search: None,
            allow_anonymous: false,
            display_name_rate_limit_disabled: false,
            dev_user: None,
            oauth_allow_unverified: false,
        }
    }

    #[test]
    #[serial]
    fn token_ttl_secs_defaults_to_86400_when_env_unset() {
        // Regression test for discussion #562: the previous default of 60s
        // caused production stranding when re-election fired more than 60s
        // after a client joined, expiring the cached token in the WT/WS URL.
        let prior_db = std::env::var("DATABASE_URL").ok();
        let prior_jwt = std::env::var("JWT_SECRET").ok();

        std::env::set_var("DATABASE_URL", "postgres://test/test");
        std::env::set_var("JWT_SECRET", "test-secret");

        with_env_unset("TOKEN_TTL_SECS", || {
            let cfg = Config::from_env().expect("from_env with TOKEN_TTL_SECS unset must succeed");
            assert_eq!(
                cfg.token_ttl_secs, 86400,
                "default must be 24h — see discussion #562"
            );
        });

        // Restore surrounding env so we don't pollute sibling tests.
        match prior_db {
            Some(v) => std::env::set_var("DATABASE_URL", v),
            None => std::env::remove_var("DATABASE_URL"),
        }
        match prior_jwt {
            Some(v) => std::env::set_var("JWT_SECRET", v),
            None => std::env::remove_var("JWT_SECRET"),
        }
    }

    #[test]
    fn validate_oauth_security_allows_non_oauth_mode() {
        let cfg = base_config();
        assert!(cfg.validate_oauth_security().is_ok());
    }

    #[test]
    fn validate_oauth_security_allows_verified_oauth_mode() {
        let mut cfg = base_config();
        cfg.oauth = Some(OAuthConfig {
            client_id: "client".to_string(),
            client_secret: None,
            redirect_url: "https://app.example/callback".to_string(),
            issuer: Some("https://issuer.example".to_string()),
            auth_url: "https://issuer.example/auth".to_string(),
            token_url: "https://issuer.example/token".to_string(),
            jwks_url: Some("https://issuer.example/jwks".to_string()),
            userinfo_url: None,
            scopes: "openid email profile".to_string(),
            after_login_url: "/".to_string(),
            allowed_redirect_urls: Vec::new(),
            end_session_endpoint: None,
            after_logout_url: None,
            browser_pkce: false,
            resource_server_audience: None,
        });

        assert!(cfg.validate_oauth_security().is_ok());
    }

    #[test]
    fn validate_oauth_security_rejects_unverified_oauth_without_opt_in() {
        let mut cfg = base_config();
        cfg.oauth = Some(OAuthConfig {
            client_id: "client".to_string(),
            client_secret: None,
            redirect_url: "https://app.example/callback".to_string(),
            issuer: None,
            auth_url: "https://issuer.example/auth".to_string(),
            token_url: "https://issuer.example/token".to_string(),
            jwks_url: None,
            userinfo_url: None,
            scopes: "openid email profile".to_string(),
            after_login_url: "/".to_string(),
            allowed_redirect_urls: Vec::new(),
            end_session_endpoint: None,
            after_logout_url: None,
            browser_pkce: false,
            resource_server_audience: None,
        });

        let err = cfg.validate_oauth_security().unwrap_err();
        assert!(err.contains("OAuth is enabled but no JWKS URL is configured"));
    }

    #[test]
    fn validate_oauth_security_allows_unverified_oauth_with_opt_in() {
        let mut cfg = base_config();
        cfg.oauth_allow_unverified = true;
        cfg.oauth = Some(OAuthConfig {
            client_id: "client".to_string(),
            client_secret: None,
            redirect_url: "https://app.example/callback".to_string(),
            issuer: None,
            auth_url: "https://issuer.example/auth".to_string(),
            token_url: "https://issuer.example/token".to_string(),
            jwks_url: None,
            userinfo_url: None,
            scopes: "openid email profile".to_string(),
            after_login_url: "/".to_string(),
            allowed_redirect_urls: Vec::new(),
            end_session_endpoint: None,
            after_logout_url: None,
            browser_pkce: false,
            resource_server_audience: None,
        });

        assert!(cfg.validate_oauth_security().is_ok());
    }

    #[test]
    #[serial]
    fn oauth_allow_unverified_defaults_to_false() {
        let _guard = env_lock().lock().unwrap();
        let prior_db = std::env::var("DATABASE_URL").ok();
        let prior_jwt = std::env::var("JWT_SECRET").ok();

        std::env::set_var("DATABASE_URL", "postgres://test/test");
        std::env::set_var("JWT_SECRET", "test-secret");

        with_env_unset("OAUTH_ALLOW_UNVERIFIED", || {
            let cfg = Config::from_env().expect("from_env with OAUTH_ALLOW_UNVERIFIED unset");
            assert!(
                !cfg.oauth_allow_unverified,
                "default must be false so unverified OAuth is never implicit"
            );
        });
        match prior_db {
            Some(v) => std::env::set_var("DATABASE_URL", v),
            None => std::env::remove_var("DATABASE_URL"),
        }
        match prior_jwt {
            Some(v) => std::env::set_var("JWT_SECRET", v),
            None => std::env::remove_var("JWT_SECRET"),
        }
    }

    #[test]
    #[serial]
    fn dev_user_and_search_can_be_enabled_together() {
        let prior_db = std::env::var("DATABASE_URL").ok();
        let prior_jwt = std::env::var("JWT_SECRET").ok();
        let prior_dev_user = std::env::var("DEV_USER").ok();
        let prior_search_url = std::env::var("SEARCH_API_URL").ok();
        let prior_search_token = std::env::var("SEARCH_API_TOKEN").ok();
        let prior_oauth_issuer = std::env::var("OAUTH_ISSUER").ok();

        std::env::set_var("DATABASE_URL", "postgres://test/test");
        std::env::set_var("JWT_SECRET", "test-secret");
        std::env::set_var("DEV_USER", "dev@test.local:Dev User");
        std::env::set_var("SEARCH_API_URL", "https://search.example.test");
        std::env::set_var("SEARCH_API_TOKEN", "test-token");
        std::env::remove_var("OAUTH_ISSUER");

        let cfg = Config::from_env().expect("DEV_USER + SearchV2 should parse successfully");
        assert!(
            cfg.dev_user.is_some(),
            "DEV_USER should be active when OAuth is disabled"
        );
        assert!(cfg.search.is_some(), "SearchV2 config should be enabled");

        match prior_db {
            Some(v) => std::env::set_var("DATABASE_URL", v),
            None => std::env::remove_var("DATABASE_URL"),
        }
        match prior_jwt {
            Some(v) => std::env::set_var("JWT_SECRET", v),
            None => std::env::remove_var("JWT_SECRET"),
        }
        match prior_dev_user {
            Some(v) => std::env::set_var("DEV_USER", v),
            None => std::env::remove_var("DEV_USER"),
        }
        match prior_search_url {
            Some(v) => std::env::set_var("SEARCH_API_URL", v),
            None => std::env::remove_var("SEARCH_API_URL"),
        }
        match prior_search_token {
            Some(v) => std::env::set_var("SEARCH_API_TOKEN", v),
            None => std::env::remove_var("SEARCH_API_TOKEN"),
        }
        match prior_oauth_issuer {
            Some(v) => std::env::set_var("OAUTH_ISSUER", v),
            None => std::env::remove_var("OAUTH_ISSUER"),
        }
    }

    #[test]
    #[serial]
    fn oauth_allow_unverified_rejects_non_canonical_true_values() {
        // Negative-case test: only "true" and "1" should enable the bypass.
        // Values like "yes", "on", "YES", empty string, etc. must NOT enable it.
        let _guard = env_lock().lock().unwrap();
        let prior_db = std::env::var("DATABASE_URL").ok();
        let prior_jwt = std::env::var("JWT_SECRET").ok();
        let prior_val = std::env::var("OAUTH_ALLOW_UNVERIFIED").ok();

        std::env::set_var("DATABASE_URL", "postgres://test/test");
        std::env::set_var("JWT_SECRET", "test-secret");

        for invalid_value in &[
            "yes", "on", "YES", "ON", "Yes", "TRUE", "", " true ", "enabled",
        ] {
            std::env::set_var("OAUTH_ALLOW_UNVERIFIED", invalid_value);
            let cfg = Config::from_env().unwrap_or_else(|e| {
                panic!(
                    "from_env should not fail for OAUTH_ALLOW_UNVERIFIED={:?}: {e}",
                    invalid_value
                )
            });
            assert!(
                !cfg.oauth_allow_unverified,
                "OAUTH_ALLOW_UNVERIFIED={:?} must parse to false (only \"true\" and \"1\" are accepted)",
                invalid_value
            );
        }

        // Sanity: confirm canonical values DO enable it.
        for valid_value in &["true", "1"] {
            std::env::set_var("OAUTH_ALLOW_UNVERIFIED", valid_value);
            let cfg = Config::from_env().unwrap();
            assert!(
                cfg.oauth_allow_unverified,
                "OAUTH_ALLOW_UNVERIFIED={:?} must parse to true",
                valid_value
            );
        }

        match prior_val {
            Some(v) => std::env::set_var("OAUTH_ALLOW_UNVERIFIED", v),
            None => std::env::remove_var("OAUTH_ALLOW_UNVERIFIED"),
        }
        match prior_db {
            Some(v) => std::env::set_var("DATABASE_URL", v),
            None => std::env::remove_var("DATABASE_URL"),
        }
        match prior_jwt {
            Some(v) => std::env::set_var("JWT_SECRET", v),
            None => std::env::remove_var("JWT_SECRET"),
        }
    }
}

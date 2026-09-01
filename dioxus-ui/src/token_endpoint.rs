// SPDX-License-Identifier: MIT OR Apache-2.0

//! Provider token-endpoint resolution.
//!
//! Shared by the OAuth callback code exchange ([`crate::pages::oauth_callback`])
//! and the PKCE refresh path ([`crate::auth::refresh_access_token`]) so both
//! reach the same endpoint on a deployment that configures `oauthIssuer`
//! without `oauthTokenUrl`.

use crate::constants::{oauth_issuer, oauth_token_url};
use crate::context::{read_session_storage, write_session_storage};
use serde::Deserialize;
use std::future::Future;

#[derive(Debug, Deserialize)]
struct OidcDiscovery {
    token_endpoint: String,
}

/// Endpoint resolved by an earlier call, so a second resolution costs no network.
const CACHED_TOKEN_ENDPOINT_KEY: &str = "vc_cached_token_endpoint";

/// Resolve the provider's token endpoint URL.
///
/// Priority: explicit `oauthTokenUrl` → per-session cache → OIDC well-known
/// discovery against `oauthIssuer` → meeting-api `/api/v1/oauth/provider-config`.
pub(crate) async fn resolve_token_endpoint() -> Result<String, String> {
    resolve_token_endpoint_with(
        oauth_token_url(),
        read_session_storage(CACHED_TOKEN_ENDPOINT_KEY),
        oauth_issuer(),
        |discovery_url| async move {
            let url = fetch_token_endpoint_from_discovery(&discovery_url).await?;
            cache_token_endpoint(&url);
            Ok(url)
        },
        fetch_token_endpoint_from_backend,
    )
    .await
}

/// Ordering core of [`resolve_token_endpoint`], with the network steps injected.
pub(crate) async fn resolve_token_endpoint_with<D, DF, B, BF>(
    explicit: Option<String>,
    cached: Option<String>,
    issuer: Option<String>,
    discover: D,
    backend: B,
) -> Result<String, String>
where
    D: FnOnce(String) -> DF,
    DF: Future<Output = Result<String, String>>,
    B: FnOnce() -> BF,
    BF: Future<Output = Result<String, String>>,
{
    if let Some(url) = explicit.filter(|s| !s.is_empty()) {
        return Ok(url);
    }
    if let Some(url) = cached.filter(|s| !s.is_empty()) {
        return Ok(url);
    }
    if let Some(issuer) = issuer.filter(|s| !s.is_empty()) {
        let discovery_url = discovery_url_for(&issuer);
        match discover(discovery_url.clone()).await {
            Ok(url) => return Ok(url),
            Err(e) => {
                log::warn!("OIDC discovery failed ({discovery_url}): {e}; trying backend fallback")
            }
        }
    }
    backend().await
}

/// Build the OIDC well-known URL for an issuer, tolerating a trailing slash.
pub(crate) fn discovery_url_for(issuer: &str) -> String {
    format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    )
}

async fn fetch_token_endpoint_from_discovery(discovery_url: &str) -> Result<String, String> {
    let resp = reqwest::get(discovery_url)
        .await
        .map_err(|e| format!("OIDC discovery request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("OIDC discovery returned HTTP {status}: {body}"));
    }

    let doc: OidcDiscovery = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse OIDC discovery document: {e}"))?;

    if doc.token_endpoint.is_empty() {
        return Err("OIDC discovery document has an empty token_endpoint".to_string());
    }

    Ok(doc.token_endpoint)
}

async fn fetch_token_endpoint_from_backend() -> Result<String, String> {
    // Delegate to the shared provider-config fetch (handles its own cache).
    let cfg = crate::provider_config::fetch_provider_config().await?;

    if !cfg.token_url.is_empty() {
        cache_token_endpoint(&cfg.token_url);
        return Ok(cfg.token_url);
    }

    // token_url empty but issuer present — try OIDC well-known discovery.
    if let Some(issuer) = cfg.issuer.filter(|s| !s.is_empty()) {
        let url = fetch_token_endpoint_from_discovery(&discovery_url_for(&issuer)).await?;
        cache_token_endpoint(&url);
        return Ok(url);
    }

    Err(
        "Cannot resolve token endpoint: set OAUTH_TOKEN_URL or OAUTH_ISSUER in the \
         dioxus-ui environment, or ensure the backend has OAUTH_TOKEN_URL / OAUTH_ISSUER \
         configured."
            .to_string(),
    )
}

fn cache_token_endpoint(url: &str) {
    write_session_storage(CACHED_TOKEN_ENDPOINT_KEY, url);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    const ISSUER: &str = "https://id.conceptcar7.com";
    const DISCOVERED: &str = "https://id.conceptcar7.com/oauth2/v1/token";
    const BACKEND: &str = "https://backend.example/token";

    fn recorder() -> Rc<RefCell<Vec<String>>> {
        Rc::new(RefCell::new(Vec::new()))
    }

    fn discover_ok(
        seen: Rc<RefCell<Vec<String>>>,
    ) -> impl FnOnce(String) -> std::future::Ready<Result<String, String>> {
        move |url| {
            seen.borrow_mut().push(url);
            std::future::ready(Ok(DISCOVERED.to_string()))
        }
    }

    fn discover_err(
        seen: Rc<RefCell<Vec<String>>>,
    ) -> impl FnOnce(String) -> std::future::Ready<Result<String, String>> {
        move |url| {
            seen.borrow_mut().push(url);
            std::future::ready(Err("boom".to_string()))
        }
    }

    fn backend_ok(
        hits: Rc<RefCell<Vec<String>>>,
    ) -> impl FnOnce() -> std::future::Ready<Result<String, String>> {
        move || {
            hits.borrow_mut().push(BACKEND.to_string());
            std::future::ready(Ok(BACKEND.to_string()))
        }
    }

    /// Issue #2291: the PKCE clusters (ascend, labsworkspace) configure
    /// `oauthIssuer` and never `oauthTokenUrl`. Resolution must reach discovery
    /// rather than give up, or the refresh POST is never sent.
    #[test]
    fn issuer_only_resolves_via_discovery() {
        let seen = recorder();
        let backend_hits = recorder();
        let got = futures::executor::block_on(resolve_token_endpoint_with(
            None,
            None,
            Some(ISSUER.to_string()),
            discover_ok(seen.clone()),
            backend_ok(backend_hits.clone()),
        ));
        assert_eq!(
            got,
            Ok(DISCOVERED.to_string()),
            "issuer-only config must resolve the token endpoint (issue #2291)"
        );
        assert_eq!(
            seen.borrow().as_slice(),
            [format!("{ISSUER}/.well-known/openid-configuration")],
            "discovery must be attempted exactly once, at the well-known URL"
        );
        assert!(
            backend_hits.borrow().is_empty(),
            "a successful discovery must not also hit the backend fallback"
        );
    }

    #[test]
    fn empty_string_config_is_treated_as_absent() {
        let seen = recorder();
        let got = futures::executor::block_on(resolve_token_endpoint_with(
            Some(String::new()),
            Some(String::new()),
            Some(ISSUER.to_string()),
            discover_ok(seen.clone()),
            backend_ok(recorder()),
        ));
        assert_eq!(got, Ok(DISCOVERED.to_string()));
        assert_eq!(seen.borrow().len(), 1);
    }

    #[test]
    fn explicit_config_short_circuits_every_network_step() {
        let seen = recorder();
        let backend_hits = recorder();
        let got = futures::executor::block_on(resolve_token_endpoint_with(
            Some("https://explicit.example/token".to_string()),
            Some(DISCOVERED.to_string()),
            Some(ISSUER.to_string()),
            discover_ok(seen.clone()),
            backend_ok(backend_hits.clone()),
        ));
        assert_eq!(got, Ok("https://explicit.example/token".to_string()));
        assert!(seen.borrow().is_empty() && backend_hits.borrow().is_empty());
    }

    #[test]
    fn session_cache_beats_discovery() {
        let seen = recorder();
        let got = futures::executor::block_on(resolve_token_endpoint_with(
            None,
            Some(DISCOVERED.to_string()),
            Some(ISSUER.to_string()),
            discover_ok(seen.clone()),
            backend_ok(recorder()),
        ));
        assert_eq!(got, Ok(DISCOVERED.to_string()));
        assert!(
            seen.borrow().is_empty(),
            "a cached endpoint must not trigger discovery"
        );
    }

    #[test]
    fn discovery_failure_falls_back_to_backend_once() {
        let seen = recorder();
        let backend_hits = recorder();
        let got = futures::executor::block_on(resolve_token_endpoint_with(
            None,
            None,
            Some(ISSUER.to_string()),
            discover_err(seen.clone()),
            backend_ok(backend_hits.clone()),
        ));
        assert_eq!(got, Ok(BACKEND.to_string()));
        assert_eq!(seen.borrow().len(), 1);
        assert_eq!(backend_hits.borrow().len(), 1);
    }

    #[test]
    fn no_source_at_all_still_asks_the_backend() {
        let backend_hits = recorder();
        let got = futures::executor::block_on(resolve_token_endpoint_with(
            None,
            None,
            None,
            discover_ok(recorder()),
            backend_ok(backend_hits.clone()),
        ));
        assert_eq!(got, Ok(BACKEND.to_string()));
        assert_eq!(backend_hits.borrow().len(), 1);
    }

    #[test]
    fn discovery_url_tolerates_a_trailing_slash() {
        assert_eq!(
            discovery_url_for("https://id.conceptcar7.com/"),
            "https://id.conceptcar7.com/.well-known/openid-configuration"
        );
        assert_eq!(
            discovery_url_for(ISSUER),
            "https://id.conceptcar7.com/.well-known/openid-configuration"
        );
    }
}

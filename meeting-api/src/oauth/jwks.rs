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

//! JWKS (JSON Web Key Set) cache with rate-limited refresh.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use jsonwebtoken::{Algorithm, DecodingKey};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::error::AppError;

/// Minimum interval between JWKS refreshes (5 minutes).
const JWKS_REFRESH_INTERVAL_SECS: u64 = 300;

/// A JWK entry from the JWKS endpoint.
#[derive(Debug, Deserialize)]
struct JwkEntry {
    kid: Option<String>,
    kty: String,
    #[serde(default)]
    alg: Option<String>,
    // RSA fields
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    // EC fields
    #[serde(default)]
    crv: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JwksDocument {
    keys: Vec<JwkEntry>,
}

/// Caches JWKS keys fetched from the provider, with rate-limited refresh.
pub struct JwksCache {
    keys: RwLock<HashMap<String, (Algorithm, DecodingKey)>>,
    jwks_url: String,
    last_refresh: RwLock<Instant>,
}

impl JwksCache {
    /// Create a test-only JwksCache with pre-loaded keys (no HTTP fetching).
    #[cfg(test)]
    pub fn with_keys(keys: HashMap<String, (Algorithm, DecodingKey)>) -> Arc<Self> {
        Arc::new(Self {
            keys: RwLock::new(keys),
            jwks_url: String::new(),
            last_refresh: RwLock::new(Instant::now()),
        })
    }

    pub fn new(jwks_url: String) -> Arc<Self> {
        Arc::new(Self {
            keys: RwLock::new(HashMap::new()),
            jwks_url,
            // Set to epoch-ish so the first request triggers a refresh.
            last_refresh: RwLock::new(
                Instant::now() - std::time::Duration::from_secs(JWKS_REFRESH_INTERVAL_SECS + 1),
            ),
        })
    }

    /// Get the decoding key for a given `kid`. Refreshes the cache if the key
    /// is not found (rate-limited to once per 5 minutes).
    pub async fn get_key(&self, kid: &str) -> Result<(Algorithm, DecodingKey), AppError> {
        // Try read lock first.
        {
            let keys = self.keys.read().await;
            if let Some((alg, key)) = keys.get(kid) {
                return Ok((*alg, key.clone()));
            }
        }

        // Key not found — try refreshing (rate-limited).
        self.refresh().await?;

        let keys = self.keys.read().await;
        keys.get(kid)
            .map(|(alg, key)| (*alg, key.clone()))
            .ok_or_else(|| AppError::internal(&format!("JWKS key not found for kid: {kid}")))
    }

    /// Fetch the JWKS document and update the cache. Rate-limited.
    async fn refresh(&self) -> Result<(), AppError> {
        {
            let last = self.last_refresh.read().await;
            if last.elapsed().as_secs() < JWKS_REFRESH_INTERVAL_SECS {
                return Ok(());
            }
        }

        let resp = reqwest::get(&self.jwks_url)
            .await
            .map_err(|e| AppError::internal(&format!("JWKS fetch failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(AppError::internal(&format!(
                "JWKS fetch returned HTTP {status}"
            )));
        }

        let doc: JwksDocument = resp
            .json()
            .await
            .map_err(|e| AppError::internal(&format!("Failed to parse JWKS: {e}")))?;

        let mut new_keys = HashMap::new();
        for jwk in &doc.keys {
            let kid = match &jwk.kid {
                Some(k) => k.clone(),
                None => continue,
            };

            let alg = jwk_algorithm(jwk);

            let decoding_key = match jwk.kty.as_str() {
                "RSA" => {
                    let n = jwk.n.as_deref().unwrap_or_default();
                    let e = jwk.e.as_deref().unwrap_or_default();
                    if n.is_empty() || e.is_empty() {
                        continue;
                    }
                    DecodingKey::from_rsa_components(n, e)
                        .map_err(|e| AppError::internal(&format!("Invalid RSA JWK: {e}")))?
                }
                "EC" => {
                    let x = jwk.x.as_deref().unwrap_or_default();
                    let y = jwk.y.as_deref().unwrap_or_default();
                    if x.is_empty() || y.is_empty() {
                        continue;
                    }
                    DecodingKey::from_ec_components(x, y)
                        .map_err(|e| AppError::internal(&format!("Invalid EC JWK: {e}")))?
                }
                _ => continue,
            };

            new_keys.insert(kid, (alg, decoding_key));
        }

        *self.keys.write().await = new_keys;
        *self.last_refresh.write().await = Instant::now();
        Ok(())
    }
}

/// Determine the JWT algorithm for a JWK entry.
fn jwk_algorithm(jwk: &JwkEntry) -> Algorithm {
    if let Some(alg) = &jwk.alg {
        match alg.as_str() {
            "RS384" => return Algorithm::RS384,
            "RS512" => return Algorithm::RS512,
            "ES256" => return Algorithm::ES256,
            "ES384" => return Algorithm::ES384,
            "RS256" => return Algorithm::RS256,
            _ if jwk.kty == "RSA" => return Algorithm::RS256,
            _ => {}
        }
    }
    // Default based on key type.
    match jwk.kty.as_str() {
        "EC" => match jwk.crv.as_deref() {
            Some("P-384") => Algorithm::ES384,
            _ => Algorithm::ES256,
        },
        _ => Algorithm::RS256,
    }
}

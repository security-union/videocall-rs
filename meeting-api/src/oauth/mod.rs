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

//! Generic OAuth2/OIDC helpers: OIDC discovery, JWKS caching, JWT verification,
//! PKCE generation, token exchange, and ID token claims extraction.

pub mod claims;
pub mod discovery;
pub mod exchange;
pub mod jwks;
pub mod verify;

// Re-export public API so callers can continue using `crate::oauth::*`.
pub use claims::{fetch_userinfo, IdTokenClaims, UserInfoResponse};
pub use discovery::{discover_oidc_endpoints, OidcEndpoints};
pub use exchange::{build_auth_url, exchange_code_for_claims, OAuthTokenResponse};
pub use jwks::JwksCache;
pub use verify::verify_and_decode_id_token;

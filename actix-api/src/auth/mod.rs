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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use actix_web::web;
use anyhow::{anyhow, Result as Anysult};
use oauth2::{CsrfToken, PkceCodeChallenge};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::db::PostgresPool;

pub(crate) struct DecodedJwtPartClaims {
    b64_decoded: Vec<u8>,
}

pub(crate) fn b64_decode<T: AsRef<[u8]>>(input: T) -> Anysult<Vec<u8>> {
    base64::decode_config(input, base64::URL_SAFE_NO_PAD).map_err(|e| e.into())
}

impl DecodedJwtPartClaims {
    pub fn from_jwt_part_claims(encoded_jwt_part_claims: impl AsRef<[u8]>) -> Anysult<Self> {
        Ok(Self {
            b64_decoded: b64_decode(encoded_jwt_part_claims)?,
        })
    }

    pub fn deserialize<'a, T: Deserialize<'a>>(&'a self) -> Anysult<T> {
        Ok(serde_json::from_slice(&self.b64_decoded)?)
    }
}

#[derive(Debug, Deserialize)]
pub struct AuthRequest {
    pub state: String,
    pub code: String,
    pub scope: String,
    pub authuser: String,
    pub prompt: String,
}

pub struct OAuthRequest {
    pub pkce_challenge: String,
    pub pkce_verifier: String,
    pub csrf_state: String,
}

#[derive(Deserialize, Clone)]
pub struct OAuthResponse {
    pub access_token: String,
    pub token_type: String,
    pub scope: Option<String>,
    pub id_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    pub email: String,
    pub name: String,
}

pub fn generate_and_store_oauth_request(
    pool: web::Data<PostgresPool>,
) -> Anysult<(CsrfToken, PkceCodeChallenge)> {
    let mut connection = pool.get()?;
    let csrf_state = CsrfToken::new_random();
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    connection.query(
        "INSERT INTO oauth_requests (pkce_challenge, pkce_verifier, csrf_state)
                VALUES ($1, $2, $3)
            ",
        &[
            &pkce_challenge.as_str(),
            &pkce_verifier.secret().as_str(),
            &csrf_state.secret().clone(),
        ],
    )?;
    Ok((csrf_state, pkce_challenge))
}

pub fn fetch_oauth_request(pool: web::Data<PostgresPool>, state: String) -> Anysult<OAuthRequest> {
    let mut connection = pool.get()?;
    let result = connection.query(
        "SELECT * FROM oauth_requests WHERE csrf_state=$1",
        &[&state],
    )?;
    #[allow(clippy::manual_try_fold)]
    result
        .iter()
        .fold(Err(anyhow!("Unable to find request")), |_acc, row| {
            Ok(OAuthRequest {
                csrf_state: row.get("csrf_state"),
                pkce_challenge: row.get("pkce_challenge"),
                pkce_verifier: row.get("pkce_verifier"),
            })
        })
}

pub fn upsert_user(
    pool: web::Data<PostgresPool>,
    claims: &Claims,
    oauth_response: &OAuthResponse,
) -> Anysult<()> {
    let mut connection = pool.get()?;
    connection.query(
        "INSERT INTO users (email, name, access_token, refresh_token, created_at, last_login) 
         VALUES ($1, $2, $3, $4, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
         ON CONFLICT (email)
         DO UPDATE
         SET access_token = $3, refresh_token = $4, name = $2, last_login = CURRENT_TIMESTAMP",
        &[
            &claims.email,
            &claims.name,
            &oauth_response.access_token,
            &oauth_response.refresh_token,
        ],
    )?;
    Ok(())
}

pub async fn request_token(
    redirect_url: &str,
    client_id: &str,
    client_secret: &str,
    pkce_verifier: &str,
    oauth_token_url: &str,
    authorization_code: &str,
) -> Anysult<(OAuthResponse, Claims)> {
    let client = Client::new();
    let params = [
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect_url),
        ("client_id", client_id),
        ("code", authorization_code),
        ("client_secret", client_secret),
        ("pkce_verifier", pkce_verifier),
    ];
    let response = client.post(oauth_token_url).form(&params).send().await?;

    // Log the response for debugging
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await?;
        error!("OAuth token request failed. Status: {status}, Body: {body}");
        return Err(anyhow!("OAuth token request failed with status {status}"));
    }

    let body_text = response.text().await?;
    info!("OAuth response body: {body_text}");

    let oauth_response: OAuthResponse = serde_json::from_str(&body_text)
        .map_err(|e| anyhow!("Failed to parse OAuth response: {e}. Body was: {body_text}"))?;
    let jwt_token = oauth_response
        .clone()
        .id_token
        .unwrap_or_else(|| String::from(""));
    let claims: Vec<&str> = jwt_token.split('.').collect();
    let claims_chunk = claims
        .get(1)
        .ok_or_else(|| anyhow!("Unable to parse jwt token"))?;
    let decoded_claims = DecodedJwtPartClaims::from_jwt_part_claims(claims_chunk)?;
    Ok((oauth_response, decoded_claims.deserialize()?))
}

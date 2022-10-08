use actix_web::web;
use anyhow::{anyhow, Result as Anysult};
use oauth2::{CsrfToken, PkceCodeChallenge};
use reqwest::Client;
use serde::{Deserialize, Serialize};

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

#[derive(Deserialize)]
pub struct OAuthResponse {
    pub access_token: String,
    pub token_type: String,
    pub scope: String,
    pub id_token: String,
    pub refresh_token: Option<String>,
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
        "INSERT INTO users (email, access_token, refresh_token) VALUES ($1, $2, $3)
                ON CONFLICT (email)
                    DO UPDATE
                        SET access_token = $2, refresh_token = $3",
        &[
            &claims.email,
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
    let oauth_response: OAuthResponse = response.json().await?;
    let claims: Vec<&str> = oauth_response.id_token.split(".").collect();
    let claims_chunk = claims.get(1).ok_or(anyhow!("Unable to parse jwt token"))?;
    let decoded_claims = DecodedJwtPartClaims::from_jwt_part_claims(claims_chunk)?;
    Ok((oauth_response, decoded_claims.deserialize()?))
}

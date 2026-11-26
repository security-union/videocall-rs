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

//! Interop: a token signed by the CLI verifies with meeting-api's device
//! token verifier, and only for the audience it was minted for.
//!
//! The first test runs everywhere: it applies meeting-api's own validation,
//! key decoding and time checks (everything `verify_device_jwt` does except
//! the database lookup). The second inserts an approved device and runs the
//! full `verify_device_jwt` against the Postgres test database when
//! `DATABASE_URL` is set (as in `make check-backend`), and is skipped
//! otherwise.

use chrono::{Duration, Utc};
use jsonwebtoken::{decode, DecodingKey};
use meeting_api::db::devices::{insert_approved_device, NewDevice};
use meeting_api::db::DbPool;
use meeting_api::device_auth::{
    check_token_times, device_validation, generate_device_id, public_key_raw, verify_device_jwt,
    AllowedStates, DeviceAuthError,
};
use videocall_cli::auth::keys::DeviceKey;
use videocall_meeting_types::device::DeviceTokenClaims;

const DEVICE_ID: &str = "aB3dE5fG7hI9jK1lM3nO5p";
const API: &str = "https://api.example.com";
/// Another origin, standing in for any server other than the API.
const AUTH: &str = "https://other.example.com";

/// meeting-api's verification minus the database lookup.
fn verify_offline(key: &DeviceKey, token: &str, aud: &str) -> Result<DeviceTokenClaims, String> {
    let raw = public_key_raw(&key.public_key_b64()).ok_or("public key")?;
    let claims = decode::<DeviceTokenClaims>(
        token,
        &DecodingKey::from_ed_der(&raw),
        &device_validation(aud, true),
    )
    .map_err(|e| e.to_string())?
    .claims;
    check_token_times(claims.iat, claims.exp, Utc::now())?;
    Ok(claims)
}

#[test]
fn cli_token_passes_meeting_api_validation_and_honors_aud() {
    let key = DeviceKey::generate().unwrap();
    let token = key
        .device_token(DEVICE_ID, API, Utc::now().timestamp())
        .unwrap();

    let claims = verify_offline(&key, &token, API).expect("CLI token verifies");
    assert_eq!(claims.sub, DEVICE_ID);
    assert!(verify_offline(&key, &token, AUTH).is_err(), "wrong aud");

    let other = DeviceKey::generate().unwrap();
    assert!(verify_offline(&other, &token, API).is_err(), "wrong key");
}

#[tokio::test]
async fn cli_token_verifies_with_verify_device_jwt() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipped: DATABASE_URL not set");
        return;
    };
    let pool = DbPool::connect(&url).await.unwrap();
    let key = DeviceKey::generate().unwrap();
    let device_id = generate_device_id();
    let now = Utc::now();
    insert_approved_device(
        &pool,
        &NewDevice {
            id: &device_id,
            public_key: &key.public_key_b64(),
            fingerprint: &key.fingerprint(),
            name: "videocall-cli interop test",
            user_id: "interop@example.com",
            user_name: "Interop",
            requester_ip: None,
            now,
            idle_expires_at: now + Duration::days(90),
            abs_expires_at: now + Duration::days(365),
        },
    )
    .await
    .unwrap();

    let token = key.device_token(&device_id, API, now.timestamp()).unwrap();
    let verified = verify_device_jwt(&pool, &token, API, now, AllowedStates::LiveApproved).await;
    let wrong_aud = verify_device_jwt(&pool, &token, AUTH, now, AllowedStates::LiveApproved).await;
    let other_key = DeviceKey::generate()
        .unwrap()
        .device_token(&device_id, API, now.timestamp())
        .unwrap();
    let forged = verify_device_jwt(&pool, &other_key, API, now, AllowedStates::LiveApproved).await;

    sqlx::query("DELETE FROM devices WHERE id = $1")
        .bind(&device_id)
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(verified.expect("CLI token verifies").id, device_id);
    assert!(matches!(wrong_aud, Err(DeviceAuthError::Invalid(_))));
    assert!(matches!(forged, Err(DeviceAuthError::Invalid(_))));
}

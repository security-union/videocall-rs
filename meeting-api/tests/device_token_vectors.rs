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

//! The golden device-token vectors that videocall-cli's signing test
//! reproduces byte for byte verify here with the production verifiers
//! (everything `verify_device_jwt` does except the row lookup and row
//! state). No database. The vector file lives in videocall-meeting-types so
//! neither crate depends on the other.

use chrono::{DateTime, Utc};
use meeting_api::device_auth::{
    device_token_kid, fingerprint_of, verify_device_token, verify_registration, RegistrationError,
};
use serde_json::Value;

fn vectors() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../videocall-meeting-types/tests/vectors/device_tokens.json"
    );
    let text = std::fs::read_to_string(path).expect("read the shared device token vectors");
    serde_json::from_str(&text).expect("parse the shared device token vectors")
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("vector field {key}"))
}

/// A time inside the token's window: 30 s after `iat`.
fn inside_window(v: &Value) -> DateTime<Utc> {
    DateTime::from_timestamp(v["iat"].as_i64().expect("iat") + 30, 0).expect("timestamp")
}

#[test]
fn vector_fingerprint_matches() {
    let v = vectors();
    assert_eq!(
        fingerprint_of(str_of(&v, "public_key")).as_deref(),
        Some(str_of(&v, "fingerprint"))
    );
}

#[test]
fn vector_device_token_verifies() {
    let v = vectors();
    let d = &v["device_token"];
    let token = str_of(d, "token");
    let public_key = str_of(&v, "public_key");
    let aud = str_of(d, "aud");
    let now = inside_window(d);

    let kid = device_token_kid(token).expect("header");
    assert_eq!(kid, str_of(d, "device_id"));

    let claims = verify_device_token(token, &kid, public_key, aud, now).expect("verifies");
    assert_eq!(claims.sub, kid);
    assert_eq!(claims.aud, aud);
    assert_eq!(claims.iat, d["iat"].as_i64().unwrap());
    assert_eq!(claims.exp, d["exp"].as_i64().unwrap());
    assert_eq!(claims.jti, str_of(d, "jti"));

    // Wrong audience, wrong key, and a `now` past `exp` + leeway all fail.
    assert!(
        verify_device_token(token, &kid, public_key, "https://other.example.com", now).is_err()
    );
    // RFC 8032 section 7.1 TEST 2 public key: valid, but not the signer.
    let wrong_key = "PUAXw-hDiVqStwqnTRt-vJyYLM8uxJaMwM1V8Sr0Zgw";
    assert!(verify_device_token(token, &kid, wrong_key, aud, now).is_err());
    let late = DateTime::from_timestamp(d["exp"].as_i64().unwrap() + 61, 0).unwrap();
    assert!(verify_device_token(token, &kid, public_key, aud, late).is_err());
    // `sub` must equal the `kid` the row was selected by.
    assert!(verify_device_token(token, "zZ3dE5fG7hI9jK1lM3nO5p", public_key, aud, now).is_err());
}

#[test]
fn vector_registration_verifies() {
    let v = vectors();
    let r = &v["registration"];
    let token = str_of(r, "token");
    let now = inside_window(r);

    let (claims, raw) = verify_registration(token, str_of(r, "aud"), now).expect("verifies");
    assert_eq!(claims.public_key, str_of(&v, "public_key"));
    assert_eq!(claims.name, str_of(r, "name"));
    assert_eq!(
        Some(raw),
        videocall_meeting_types::device::decode_public_key(str_of(&v, "public_key"))
    );

    assert_eq!(
        verify_registration(token, "https://other.example.com", now).unwrap_err(),
        RegistrationError::Signature
    );
}

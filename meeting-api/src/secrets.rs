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

//! Small cryptographic helpers.
//!
//! - Random values come from the OS CSPRNG, encoded base64url without padding.
//! - One-time values (e.g. the device approval nonce) are stored only as their
//!   SHA-256 digest (lowercase hex).
//! - Secret-derived values are compared in constant time.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Generate `n` random bytes from the OS CSPRNG, base64url-encoded (no padding).
pub fn random_base64url(n: usize) -> String {
    let mut buf = vec![0u8; n];
    OsRng.fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(&buf)
}

/// Generate a random opaque row identifier (16 bytes, base64url).
pub fn new_id() -> String {
    random_base64url(16)
}

/// Lowercase hex SHA-256 digest of `secret`. This is the only form in which
/// secrets are persisted; lookups are by digest.
pub fn sha256_hex(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// RFC 7636 S256 transform: `BASE64URL(SHA256(ASCII(verifier)))`.
pub fn pkce_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Constant-time byte equality. Lengths are not secret.
pub fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_16_bytes_and_unique() {
        let a = new_id();
        let b = new_id();
        assert_eq!(URL_SAFE_NO_PAD.decode(&a).unwrap().len(), 16);
        assert_ne!(a, b);
    }

    #[test]
    fn sha256_hex_is_64_lowercase_hex() {
        let h = sha256_hex("abc");
        assert_eq!(
            h,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn rfc7636_appendix_b_vector() {
        assert_eq!(
            pkce_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn ct_eq_behaves() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
    }
}

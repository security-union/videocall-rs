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

//! Optional dev-only WebTransport certificate preflight.
//!
//! Chromium 145+ requires the WebTransport `serverCertificateHashes` API for
//! self-signed dev certs. That API only accepts:
//!
//! * ECDSA P-256 leaf cert (NIST `secp256r1` / `prime256v1`)
//! * `notAfter - notBefore` <= 14 days (1209600 seconds)
//! * Subject Alternative Name covering the dial target
//! * Currently within the validity window
//!
//! If a developer accidentally generates an RSA cert (e.g. via a stock
//! `openssl req -x509 -newkey rsa:2048 ...` snippet) or a long-lived cert,
//! the WebTransport server still starts cleanly and `with_single_cert` is
//! happy — but every QUIC handshake fails at runtime with
//! `QUIC_TLS_CERTIFICATE_UNKNOWN`. The failure shows up only in the browser
//! and is brutally hard to trace back to the cert.
//!
//! This module catches that mistake at startup. It is gated behind the
//! `WT_DEV_CERT_PREFLIGHT` env var (default off) so production deployments
//! — which use cert-manager-issued RSA leaf certs with multi-week or
//! multi-month validity — are completely unaffected.
//!
//! To enable in dev/E2E:
//! ```bash
//! WT_DEV_CERT_PREFLIGHT=true cargo run --bin webtransport_server
//! ```
//!
//! The Makefile + `docker/docker-compose.e2e.yaml` set this on the
//! `webtransport-api` service so every E2E run gets the check.

use rustls::pki_types::CertificateDer;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::prelude::*;

/// Maximum allowed validity window for a `serverCertificateHashes`-eligible
/// cert. Chromium enforces 14 days, end-exclusive; we mirror that exactly.
const MAX_VALIDITY_SECS: u64 = 14 * 24 * 60 * 60;

/// OID for NIST P-256 / secp256r1 / prime256v1 — the only curve Chromium
/// accepts for `serverCertificateHashes`.
///
/// Encoded form: `1.2.840.10045.3.1.7`.
const OID_SECP256R1: &str = "1.2.840.10045.3.1.7";

/// OID for `id-ecPublicKey` — present on every EC cert; the curve OID lives
/// inside the algorithm parameters.
const OID_EC_PUBLIC_KEY: &str = "1.2.840.10045.2.1";

/// Env var that opts in to the preflight. Any value of "1", "true", "TRUE",
/// "yes", "YES" enables it.
pub const PREFLIGHT_ENV_VAR: &str = "WT_DEV_CERT_PREFLIGHT";

/// Returns true if the preflight is enabled via env var.
pub fn is_enabled() -> bool {
    matches!(
        std::env::var(PREFLIGHT_ENV_VAR).as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

/// Run the preflight against a parsed cert chain. The leaf cert is the
/// first entry of `chain` (rustls / `with_single_cert` expects this
/// ordering and rejects an empty chain elsewhere).
///
/// Returns `Ok(())` when every check passes. Returns a human-readable
/// `Err(String)` describing the *first* failure on mismatch — callers
/// (`webtransport::start`) print it via `print_failure()` and abort.
///
/// `cert_path_for_diagnostics` is only used to make error messages
/// copy-pasteable; the validation itself is path-independent.
pub fn validate_chain(
    chain: &[CertificateDer<'_>],
    cert_path_for_diagnostics: &str,
) -> Result<(), String> {
    let leaf = chain.first().ok_or_else(|| {
        format!("no certificates found in chain loaded from {cert_path_for_diagnostics}")
    })?;

    let (_rest, parsed) = X509Certificate::from_der(leaf.as_ref())
        .map_err(|e| format!("failed to parse leaf certificate as X.509: {e}"))?;

    check_public_key_algorithm(&parsed)?;
    check_validity_window(&parsed)?;
    check_subject_alt_names(&parsed)?;

    Ok(())
}

fn check_public_key_algorithm(cert: &X509Certificate<'_>) -> Result<(), String> {
    let spki = &cert.tbs_certificate.subject_pki;
    let algorithm_oid = spki.algorithm.algorithm.to_id_string();

    if algorithm_oid != OID_EC_PUBLIC_KEY {
        return Err(format!(
            "key algorithm OID is {algorithm_oid} (expected {OID_EC_PUBLIC_KEY} = id-ecPublicKey \
             for ECDSA P-256). Likely RSA or Ed25519 — Chromium's \
             serverCertificateHashes API rejects everything but ECDSA P-256."
        ));
    }

    // Algorithm parameters carry the named-curve OID (e.g. P-256 vs P-384).
    let params = spki.algorithm.parameters.as_ref().ok_or_else(|| {
        "EC public key has no algorithm parameters; cannot determine named curve".to_string()
    })?;

    let curve_oid = params
        .as_oid()
        .map_err(|e| format!("EC algorithm parameters are not a named-curve OID: {e}"))?
        .to_id_string();

    if curve_oid != OID_SECP256R1 {
        return Err(format!(
            "EC named curve OID is {curve_oid} (expected {OID_SECP256R1} = prime256v1 / P-256). \
             Chromium's serverCertificateHashes API only accepts P-256."
        ));
    }

    Ok(())
}

fn check_validity_window(cert: &X509Certificate<'_>) -> Result<(), String> {
    let validity = cert.validity();
    let not_before = validity.not_before.timestamp();
    let not_after = validity.not_after.timestamp();

    if not_after < not_before {
        return Err(format!(
            "cert validity is inverted (notAfter {not_after} < notBefore {not_before})"
        ));
    }

    let span_secs = (not_after - not_before) as u64;
    if span_secs > MAX_VALIDITY_SECS {
        let days = span_secs / 86400;
        return Err(format!(
            "cert validity window is {span_secs}s (~{days} days); Chromium's \
             serverCertificateHashes API requires <= {MAX_VALIDITY_SECS}s (14 days). \
             notBefore={} notAfter={}",
            validity.not_before, validity.not_after,
        ));
    }

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64;

    if now_secs < not_before {
        return Err(format!(
            "cert is not yet valid: notBefore={} is in the future (now={now_secs})",
            validity.not_before,
        ));
    }
    if now_secs > not_after {
        return Err(format!(
            "cert has expired: notAfter={} is in the past (now={now_secs})",
            validity.not_after,
        ));
    }

    Ok(())
}

fn check_subject_alt_names(cert: &X509Certificate<'_>) -> Result<(), String> {
    let mut has_localhost_dns = false;
    let mut has_loopback_ip = false;

    for ext in cert.extensions() {
        if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
            for name in &san.general_names {
                match name {
                    GeneralName::DNSName(dns) if *dns == "localhost" => {
                        has_localhost_dns = true;
                    }
                    GeneralName::IPAddress(bytes) => {
                        // IPv4 127.0.0.1 is the 4 bytes [127, 0, 0, 1]; IPv6
                        // ::1 is 16 bytes ending in 1. The dev cert is
                        // expected to carry the IPv4 form (the QUIC dial
                        // target is `https://127.0.0.1:4433`), so we only
                        // accept that.
                        if *bytes == [127, 0, 0, 1] {
                            has_loopback_ip = true;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if !has_localhost_dns || !has_loopback_ip {
        return Err(format!(
            "Subject Alternative Name is missing required entries: \
             DNS:localhost present={has_localhost_dns}, IP:127.0.0.1 present={has_loopback_ip}. \
             Chromium matches the dial target ({}) against the cert's SANs.",
            "https://127.0.0.1:4433",
        ));
    }

    Ok(())
}

/// Print the canonical multi-line error preamble + remediation hint to
/// stderr. The wording mirrors what a developer needs to see when a stale
/// or wrong cert silently breaks the QUIC handshake — it tells them
/// exactly which command to run.
pub fn print_failure(cert_path: &str, reason: &str) {
    eprintln!("ERROR: WT_DEV_CERT_PREFLIGHT detected a problem with {cert_path}:");
    for line in reason.lines() {
        eprintln!("ERROR:   {line}");
    }
    eprintln!("ERROR:");
    eprintln!("ERROR: Chromium 145+ requires the WebTransport `serverCertificateHashes` API for");
    eprintln!(
        "ERROR: self-signed dev certs, which only accepts ECDSA P-256 with <= 14-day validity"
    );
    eprintln!("ERROR: and SAN entries DNS:localhost + IP:127.0.0.1.");
    eprintln!("ERROR:");
    eprintln!("ERROR: Run this to regenerate the cert + matching hash file:");
    eprintln!("ERROR:   make e2e-cert ARGS=--force");
    eprintln!("ERROR: Then restart the webtransport-api container:");
    eprintln!("ERROR:   docker restart videocall-e2e-webtransport-api-1");
    eprintln!("ERROR:");
    eprintln!(
        "ERROR: To skip this check (e.g. on a production deploy), unset {PREFLIGHT_ENV_VAR}."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::CertificateDer;
    use std::process::Command;

    /// Helper: convert an on-disk PEM cert file to a single
    /// `CertificateDer<'static>`. Panics on any I/O or parse failure —
    /// only used to seed test fixtures.
    fn load_pem_as_der(path: &str) -> CertificateDer<'static> {
        let pem = std::fs::read(path).expect("read PEM");
        let mut cursor = std::io::Cursor::new(pem);
        let mut chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cursor)
            .collect::<Result<_, _>>()
            .expect("parse certs");
        chain.swap_remove(0)
    }

    #[test]
    fn good_cert_passes() {
        // The committed dev cert is regenerated by `make e2e-cert`; if it
        // ever drifts from the contract this test enforces (P-256, <=14d,
        // SAN includes 127.0.0.1 + localhost), the e2e stack is broken
        // and this test is the canary.
        let der = load_pem_as_der("certs/localhost.pem");
        validate_chain(&[der], "certs/localhost.pem").expect("dev cert should pass preflight");
    }

    /// Regenerate a temporary RSA cert with `openssl` and verify the
    /// preflight rejects it with a message that names the algorithm
    /// mismatch. Skipped when `openssl` is not on PATH (e.g. minimal CI).
    #[test]
    fn rsa_cert_is_rejected() {
        let openssl = match which_openssl() {
            Some(p) => p,
            None => {
                eprintln!("skipping rsa_cert_is_rejected: openssl not found on PATH");
                return;
            }
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let cert_path = dir.path().join("rsa.pem");
        let key_path = dir.path().join("rsa.key");

        let status = Command::new(&openssl)
            .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout"])
            .arg(&key_path)
            .arg("-out")
            .arg(&cert_path)
            .args(["-days", "7", "-subj", "/CN=127.0.0.1"])
            .args(["-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1"])
            .status()
            .expect("run openssl");
        assert!(status.success(), "openssl should generate test cert");

        let der = load_pem_as_der(cert_path.to_str().unwrap());
        let err = validate_chain(&[der], cert_path.to_str().unwrap())
            .expect_err("RSA cert must be rejected");
        assert!(
            err.contains("RSA") || err.contains("id-ecPublicKey") || err.contains("algorithm"),
            "error should mention algorithm mismatch, got: {err}"
        );

        // Also exercise the stderr remediation preamble. The printed lines
        // are what a developer sees when the server aborts at startup, so
        // running this test with `-- --nocapture` is a quick way to eyeball
        // the message wording without booting the full stack.
        print_failure(cert_path.to_str().unwrap(), &err);
    }

    fn which_openssl() -> Option<String> {
        let out = Command::new("which").arg("openssl").output().ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

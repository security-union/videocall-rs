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
                // IPv4 127.0.0.1 is the 4 bytes [127, 0, 0, 1]; IPv6 ::1 is
                // 16 bytes ending in 1. The dev cert is expected to carry the
                // IPv4 form (the QUIC dial target is `https://127.0.0.1:4433`),
                // so only that exact byte sequence counts.
                match name {
                    GeneralName::DNSName(dns) if *dns == "localhost" => {
                        has_localhost_dns = true;
                    }
                    GeneralName::IPAddress(bytes) if *bytes == [127, 0, 0, 1] => {
                        has_loopback_ip = true;
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
    use rcgen::{
        date_time_ymd, CertificateParams, DnType, Ia5String, KeyPair, SanType,
        PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384,
    };
    use rustls::pki_types::CertificateDer;
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    // The P-256 happy-path and validity-window tests below mint certs
    // in-process with `rcgen` rather than shelling out to `openssl`.
    // Rationale (GitHub issue #917): in-process minting makes the SAN /
    // algorithm / validity coverage DETERMINISTIC on every CI run with no
    // dependency on a binary being present on PATH. The previous
    // `openssl`-based fixtures silently `return;`-ed to a fake green when
    // `openssl` was absent, which is the exact false-green defect the issue
    // calls out. Using `#[ignore]` instead would make the skip visible but
    // would still leave the negative SAN test un-runnable in a minimal CI
    // env — reintroducing the same gap — so the SAN / validity / P-256 happy
    // path AND the algorithm-rejection branch are all minted in-process. The
    // algorithm-rejection branch is exercised by a P-384 cert
    // (`non_p256_curve_cert_is_rejected`): EC but wrong curve, so it is rejected
    // by the named-curve check with no `openssl` needed. Only the NON-EC
    // (RSA) rejection branch still needs `openssl` (see `rsa_cert_is_rejected`),
    // because rcgen 0.13 has no native RSA key generation; that one test is
    // `#[ignore]`-d so its skip is honest and visible, never a silent pass.

    /// Build a self-signed ECDSA P-256 leaf cert with the requested SAN
    /// set, in-process via `rcgen`. The validity window is `now -
    /// before_offset .. now + after_offset` (both `std::time::Duration`),
    /// so callers express the window relative to the current instant and
    /// the cert is always anchored to "now". Returns the cert as a single
    /// `CertificateDer<'static>` ready for `validate_chain`.
    ///
    /// `sans` is taken verbatim — rcgen adds ONLY the SANs listed here
    /// (it never auto-promotes the Common Name into a SAN), so callers
    /// have exact control over what `check_subject_alt_names` will see.
    /// The validity window MUST be set explicitly: rcgen's default window
    /// is 1975..4096, which would fail the 14-day validity check for
    /// unrelated reasons and mask what the test intends to exercise.
    ///
    /// We derive "now" as `epoch + (now - UNIX_EPOCH)` using rcgen's public
    /// `date_time_ymd` helper as the entry point to its `time::OffsetDateTime`
    /// field type, then add/subtract `std::time::Duration`s. This keeps the
    /// `time` crate out of this module's imports (it is only a transitive
    /// dependency of rcgen, not a declared dev-dependency).
    fn mint_p256_cert(
        sans: Vec<SanType>,
        before_offset: Duration,
        after_offset: Duration,
    ) -> CertificateDer<'static> {
        let key_pair =
            KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("generate P-256 key pair");

        // `epoch` is a `time::OffsetDateTime`; `now` = epoch + seconds since
        // UNIX_EPOCH. Type is inferred from rcgen's helper return type.
        let epoch = date_time_ymd(1970, 1, 1);
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after 1970");
        let now = epoch + since_epoch;

        let mut params = CertificateParams::default();
        params.subject_alt_names = sans;
        params.not_before = now - before_offset;
        params.not_after = now + after_offset;
        // A distinct CN that is NOT a SAN; used to prove rcgen does not
        // promote the CN into the SAN set (see the negative test below).
        params
            .distinguished_name
            .push(DnType::CommonName, "videocall-cert-preflight-test");

        let cert = params.self_signed(&key_pair).expect("self-sign P-256 cert");
        // `Certificate::der()` borrows; clone into an owned 'static DER so
        // the value can outlive the local `cert` binding.
        cert.der().clone()
    }

    /// Build a self-signed ECDSA **P-384** leaf cert with the CORRECT SAN set
    /// and a short (in-window) validity, in-process via `rcgen`. P-384's SPKI
    /// carries `id-ecPublicKey` with the secp384r1 named-curve OID, so it PASSES
    /// the EC-algorithm gate in `check_public_key_algorithm` but FAILS the named-
    /// curve gate (P-384 != P-256). This deterministically exercises the
    /// algorithm-rejection branch of `validate_chain` in a default `cargo test`
    /// run with NO dependency on `openssl` (issue #917 pre-submit follow-up:
    /// Codex flagged that `#[ignore]`-ing the RSA test left the algorithm-reject
    /// branch with no default coverage). The validity window is now-anchored and
    /// well inside the 14-day cap so the SOLE rejection reason is the curve.
    fn mint_p384_cert(sans: Vec<SanType>) -> CertificateDer<'static> {
        let key_pair =
            KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).expect("generate P-384 key pair");

        let epoch = date_time_ymd(1970, 1, 1);
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after 1970");
        let now = epoch + since_epoch;

        let mut params = CertificateParams::default();
        params.subject_alt_names = sans;
        params.not_before = now - Duration::from_secs(3600);
        params.not_after = now + Duration::from_secs(13 * 24 * 60 * 60);
        params
            .distinguished_name
            .push(DnType::CommonName, "videocall-cert-preflight-test-p384");

        let cert = params.self_signed(&key_pair).expect("self-sign P-384 cert");
        cert.der().clone()
    }

    /// The SAN set the WT preflight requires: DNS:localhost + IP:127.0.0.1
    /// (IPv4 form, which serializes to the 4 bytes [127,0,0,1] that
    /// `check_subject_alt_names` matches).
    fn correct_sans() -> Vec<SanType> {
        vec![
            SanType::DnsName(Ia5String::try_from("localhost").expect("ia5 localhost")),
            SanType::IpAddress(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))),
        ]
    }

    /// Helper: convert an on-disk PEM cert file to a single
    /// `CertificateDer<'static>`. Panics on any I/O or parse failure —
    /// only used to seed test fixtures. Retained for the openssl-based
    /// RSA test.
    fn load_pem_as_der(path: &Path) -> CertificateDer<'static> {
        let pem = std::fs::read(path).expect("read PEM");
        let mut cursor = std::io::Cursor::new(pem);
        let mut chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cursor)
            .collect::<Result<_, _>>()
            .expect("parse certs");
        chain.swap_remove(0)
    }

    #[test]
    fn p256_cert_with_correct_san_and_short_validity_passes() {
        // Self-contained: mint a cert that satisfies every rule and verify
        // the validator accepts it. Deterministic on every CI run (no PATH
        // dependency). Locks the contract that the regen script + WT
        // preflight + wasm hash file all agree on.
        let der = mint_p256_cert(
            correct_sans(),
            Duration::from_secs(3600),
            Duration::from_secs(13 * 24 * 60 * 60),
        );
        validate_chain(&[der], "in-memory rcgen P-256 cert")
            .expect("freshly minted P-256 13-day cert with SAN should pass preflight");
    }

    #[test]
    fn p256_cert_over_14_days_is_rejected() {
        // Boundary case for the 14-day cap. The SANs and algorithm are
        // CORRECT, so the validity span is the SOLE rejection reason. We
        // set notBefore one hour in the PAST and notAfter 30 days in the
        // FUTURE so the cert is currently valid: the span check
        // (`span_secs > MAX_VALIDITY_SECS`) runs BEFORE the now-window
        // checks in `check_validity_window`, so the >14-day span branch
        // fires first regardless of the now-window.
        let der = mint_p256_cert(
            correct_sans(),
            Duration::from_secs(3600),
            Duration::from_secs(30 * 24 * 60 * 60),
        );
        let err = validate_chain(&[der], "in-memory rcgen 30-day cert")
            .expect_err("30-day cert must be rejected");
        assert!(
            err.contains("validity") || err.contains("14 days"),
            "error should mention validity, got: {err}"
        );
    }

    #[test]
    fn non_p256_curve_cert_is_rejected() {
        // Algorithm-rejection coverage in a DEFAULT `cargo test` run, with no
        // `openssl` dependency (issue #917 pre-submit follow-up). A P-384 cert
        // is EC (passes the id-ecPublicKey gate) but the WRONG curve, so
        // `check_public_key_algorithm` rejects it on the named-curve branch.
        // The SANs are CORRECT and the validity is in-window, so the curve is
        // the SOLE rejection reason — proven by asserting on the error text.
        // This restores the algorithm-failure regression coverage that the
        // openssl-gated `rsa_cert_is_rejected` (now `#[ignore]`) provided only
        // when openssl was on PATH; the RSA test still covers the NON-EC branch
        // specifically when run with `--ignored`.
        let der = mint_p384_cert(correct_sans());
        let err = validate_chain(&[der], "in-memory rcgen P-384 cert")
            .expect_err("a P-384 (non-P-256) cert must be rejected by the algorithm check");
        assert!(
            err.contains("P-256") || err.contains("named curve") || err.contains("prime256v1"),
            "rejection should come from the named-curve check, got: {err}"
        );
    }

    #[test]
    fn cert_without_required_san_is_rejected() {
        // Negative SAN coverage (issue #917): a P-256 cert with a SHORT
        // (13-day) validity but SANs that OMIT localhost/127.0.0.1 must be
        // rejected by `check_subject_alt_names` — and by nothing earlier.
        // Because the cert is P-256 (passes the algorithm check) with an
        // in-window short validity (passes the validity check), the only
        // possible failure is the SAN gap. We assert on the error text to
        // PROVE the rejection came from the SAN check, not an earlier one.
        let before = Duration::from_secs(3600);
        let after = Duration::from_secs(13 * 24 * 60 * 60);

        // SANs deliberately omit localhost and 127.0.0.1.
        let wrong_sans = vec![
            SanType::DnsName(Ia5String::try_from("example.com").expect("ia5 example.com")),
            SanType::IpAddress(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
        ];
        let der = mint_p256_cert(wrong_sans, before, after);
        let err = validate_chain(&[der], "in-memory rcgen cert without required SAN")
            .expect_err("cert missing DNS:localhost + IP:127.0.0.1 must be rejected");
        assert!(
            err.contains("Subject Alternative Name")
                || err.contains("localhost")
                || err.contains("127.0.0.1"),
            "error should name the SAN gap, got: {err}"
        );

        // Sanity sub-assertion: the SAME validity window but WITH the
        // correct SANs PASSES. This proves the ONLY differentiator is the
        // SAN set — the algorithm and validity window are identical to the
        // failing cert above.
        let ok_der = mint_p256_cert(correct_sans(), before, after);
        validate_chain(&[ok_der], "in-memory rcgen cert with required SAN")
            .expect("same window + correct SANs must pass — SAN set is the only differentiator");
    }

    /// Regenerate a temporary RSA cert with `openssl` and verify the
    /// preflight rejects it with a message that names the algorithm
    /// mismatch.
    ///
    /// `#[ignore]`-d rather than skipped silently: rcgen 0.13 has no native
    /// RSA key generation (its `ring`/`aws-lc-rs` backends only generate EC
    /// and Ed25519 keys), so minting an RSA cert in-process would require
    /// pulling in the heavyweight `rsa` crate as a dev-dependency just for
    /// this one algorithm-rejection case. That is disproportionate, so this
    /// test keeps the `openssl` path and is `#[ignore]`-d so its skip is
    /// VISIBLE in the test runner ("ignored") instead of a fake green. Run
    /// it explicitly with `cargo test -p actix-api -- --ignored` on a host
    /// that has `openssl` on PATH.
    #[test]
    #[ignore = "requires openssl on PATH to mint an RSA cert (rcgen 0.13 has no native RSA keygen)"]
    fn rsa_cert_is_rejected() {
        let openssl = which_openssl().expect("openssl must be on PATH for this --ignored test");

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

        let der = load_pem_as_der(&cert_path);
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

#!/usr/bin/env bash
#
# Regenerate or verify the WebTransport dev cert at
# actix-api/certs/{localhost.pem,localhost.key} + the matching DER-SHA-256
# hash file at actix-api/certs/localhost.cert-sha256.txt.
#
# Why this exists
# ---------------
# The W3C WebTransport spec lets a page bypass the browser's normal
# certificate-validation path by passing `serverCertificateHashes` to the
# WebTransport constructor (see https://w3c.github.io/webtransport/). The
# Chromium implementation imposes constraints we MUST satisfy or every
# QUIC handshake fails silently with `QUIC_TLS_CERTIFICATE_UNKNOWN`:
#
#   1. Cert MUST be ECDSA P-256 (RSA / Ed25519 / other curves rejected).
#   2. Validity period (notAfter - notBefore) MUST be <= 14 days.
#   3. SubjectAltName MUST contain `IP:127.0.0.1` and `DNS:localhost`.
#   4. The hash a page presents is SHA-256 of the *DER-encoded cert*
#      (NOT the SPKI), base64-encoded.
#
# The companion file `localhost.cert-sha256.txt` is the source of truth that
# the Playwright stack and any other tooling reads at test time. Regenerate
# the cert AND the hash file together — they are a matched pair.
#
# Usage
# -----
#   scripts/regen-dev-cert.sh              # regen if missing, stale, or invalid
#   scripts/regen-dev-cert.sh --force      # always regen
#   scripts/regen-dev-cert.sh --verify     # check existing cert; do NOT regen
#                                          # (exit 0 if cert is good, exit 1 + diagnostic if not)
#
# `--verify` is what `make e2e-doctor` calls. It prints the same diagnostic
# the WT server's startup preflight prints, with copy-paste-friendly
# remediation, so a confused developer gets the answer in one place.
#
# The script fails loudly if openssl is missing or any step errors.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
CERTS_DIR="${REPO_ROOT}/actix-api/certs"
CERT_PEM="${CERTS_DIR}/localhost.pem"
CERT_KEY="${CERTS_DIR}/localhost.key"
HASH_FILE="${CERTS_DIR}/localhost.cert-sha256.txt"

# 13 days: one day shy of the 14-day Chromium cap so a same-day regen +
# test pass still fits inside the validity window even on long CI runs.
VALIDITY_DAYS=13
MAX_VALIDITY_SECONDS=$((14 * 24 * 60 * 60))

MODE=regen
case "${1:-}" in
  --force|-f) FORCE=1; MODE=regen ;;
  --verify|-v) FORCE=0; MODE=verify ;;
  --help|-h)
    sed -n '2,40p' "$0" | sed 's/^# //;s/^#//'
    exit 0
    ;;
  "") FORCE=0; MODE=regen ;;
  *)
    echo "ERROR: unknown argument: $1" >&2
    echo "Usage: $0 [--force | --verify | --help]" >&2
    exit 2
    ;;
esac

if ! command -v openssl >/dev/null 2>&1; then
  cat >&2 <<'EOF'
ERROR: openssl not found on PATH.

The WebTransport dev cert is generated and validated with openssl. Install it:
  - Debian/Ubuntu/WSL: sudo apt-get install -y openssl
  - macOS:             brew install openssl
EOF
  exit 1
fi

# ---------------------------------------------------------------------------
# Validators
# ---------------------------------------------------------------------------
#
# Each validator returns 0 on pass, 1 on fail. On fail, it ALSO appends a
# short reason to global $REASONS (one per line) so we can print all problems
# in one diagnostic block instead of just the first.
# ---------------------------------------------------------------------------

REASONS=()

add_reason() {
  REASONS+=("$1")
}

validate_files_present() {
  local ok=0
  if [[ ! -f "${CERT_PEM}" ]]; then
    add_reason "missing: ${CERT_PEM}"
    ok=1
  fi
  if [[ ! -f "${CERT_KEY}" ]]; then
    add_reason "missing: ${CERT_KEY}"
    ok=1
  fi
  if [[ ! -f "${HASH_FILE}" ]]; then
    add_reason "missing: ${HASH_FILE}"
    ok=1
  fi
  return $ok
}

validate_key_type() {
  # Public Key Algorithm line in `openssl x509 -text` output looks like:
  #   Public Key Algorithm: id-ecPublicKey
  # NIST CURVE: P-256 line confirms the named curve.
  local text
  text="$(openssl x509 -in "${CERT_PEM}" -text -noout 2>/dev/null)"
  if [[ "${text}" != *"Public Key Algorithm: id-ecPublicKey"* ]]; then
    local algo
    algo="$(echo "${text}" | grep -E 'Public Key Algorithm:' | head -1 | sed 's/.*: //')"
    add_reason "key algorithm is '${algo:-unknown}', expected ECDSA (id-ecPublicKey)"
    return 1
  fi
  if [[ "${text}" != *"NIST CURVE: P-256"* ]]; then
    local curve
    curve="$(echo "${text}" | grep -E 'NIST CURVE:|ASN1 OID:' | head -1)"
    add_reason "EC curve is '${curve:-unknown}', expected P-256"
    return 1
  fi
  return 0
}

# Parse an openssl date string (`MMM DD HH:MM:SS YYYY GMT`) to a Unix epoch,
# portably across GNU date (Linux/WSL: `date -d`) and BSD date (macOS: `date -j
# -f`). Prints the epoch on success; returns non-zero if neither parser handles
# the string. macOS ships BSD date, which has no `-d`, so the previous
# `date -u -d` path failed there with "could not parse notBefore from cert".
openssl_date_to_epoch() {
  local s="$1" epoch
  # GNU date first (also covers busybox date on some CI images).
  if epoch="$(date -u -d "$s" +%s 2>/dev/null)"; then
    printf '%s' "$epoch"
    return 0
  fi
  # BSD date (macOS). `%e` handles the space-padded day openssl emits for
  # single-digit dates; `%Z` consumes the trailing `GMT`.
  if epoch="$(date -u -j -f "%b %e %T %Y %Z" "$s" +%s 2>/dev/null)"; then
    printf '%s' "$epoch"
    return 0
  fi
  return 1
}

validate_validity_period() {
  local not_before_epoch not_after_epoch
  # `openssl x509 -dates` emits notBefore=... and notAfter=... in default
  # `MMM DD HH:MM:SS YYYY GMT` format.
  local not_before_str not_after_str
  not_before_str="$(openssl x509 -in "${CERT_PEM}" -noout -startdate 2>/dev/null | sed 's/notBefore=//')"
  not_after_str="$(openssl x509 -in "${CERT_PEM}" -noout -enddate 2>/dev/null | sed 's/notAfter=//')"
  if ! not_before_epoch="$(openssl_date_to_epoch "${not_before_str}")"; then
    add_reason "could not parse notBefore from cert"
    return 1
  fi
  if ! not_after_epoch="$(openssl_date_to_epoch "${not_after_str}")"; then
    add_reason "could not parse notAfter from cert"
    return 1
  fi
  local now_epoch
  now_epoch="$(date -u +%s)"
  local span=$((not_after_epoch - not_before_epoch))
  local rc=0
  if (( span > MAX_VALIDITY_SECONDS )); then
    add_reason "validity period is $((span / 86400)) days, Chromium serverCertificateHashes requires <= 14 days"
    rc=1
  fi
  if (( now_epoch < not_before_epoch )); then
    add_reason "cert is not yet valid (notBefore=${not_before_str})"
    rc=1
  fi
  if (( now_epoch >= not_after_epoch )); then
    add_reason "cert has expired (notAfter=${not_after_str})"
    rc=1
  fi
  return $rc
}

validate_san() {
  local san
  san="$(openssl x509 -in "${CERT_PEM}" -noout -ext subjectAltName 2>/dev/null || true)"
  local rc=0
  if [[ "${san}" != *"IP Address:127.0.0.1"* ]]; then
    add_reason "SubjectAltName missing 'IP:127.0.0.1'"
    rc=1
  fi
  if [[ "${san}" != *"DNS:localhost"* ]]; then
    add_reason "SubjectAltName missing 'DNS:localhost'"
    rc=1
  fi
  return $rc
}

validate_hash_file_matches() {
  local expected actual
  expected="$(openssl x509 -in "${CERT_PEM}" -outform DER 2>/dev/null \
              | openssl dgst -sha256 -binary \
              | openssl enc -base64)"
  # Strip comments + blank lines from the hash file (matches the e2e helper's
  # parsing logic at e2e/helpers/auth-context.ts::readDevCertHashes).
  actual="$(grep -v '^[[:space:]]*#' "${HASH_FILE}" 2>/dev/null \
            | grep -v '^[[:space:]]*$' \
            | tr -d '[:space:]')"
  if [[ "${expected}" != "${actual}" ]]; then
    add_reason "hash file '${HASH_FILE}' (${actual:-empty}) does not match SHA-256 of cert (${expected})"
    return 1
  fi
  return 0
}

# Run every applicable validator and accumulate reasons. Returns 0 if all
# pass, 1 if any failed.
run_validators() {
  REASONS=()
  local rc=0
  validate_files_present || rc=1
  # If files are missing we cannot run the rest meaningfully — early-out.
  if [[ $rc -ne 0 ]]; then
    return 1
  fi
  validate_key_type || rc=1
  validate_validity_period || rc=1
  validate_san || rc=1
  validate_hash_file_matches || rc=1
  return $rc
}

print_diagnostic() {
  cat >&2 <<EOF

ERROR: WebTransport dev cert at ${CERT_PEM} is invalid:
EOF
  for r in "${REASONS[@]}"; do
    echo "ERROR:   - ${r}" >&2
  done
  cat >&2 <<EOF
ERROR:
ERROR: Chromium 145+ rejects WebTransport handshakes whose cert does not satisfy
ERROR: the serverCertificateHashes constraints (ECDSA P-256, <= 14 days validity,
ERROR: SAN includes 127.0.0.1 + localhost). Without a valid cert, every WT-only
ERROR: spec fails with QUIC_TLS_CERTIFICATE_UNKNOWN.
ERROR:
ERROR: Run this to regenerate the cert + matching hash file:
ERROR:   make e2e-cert ARGS=--force
ERROR:
ERROR: If the e2e stack is already running, also restart the WT container so it
ERROR: re-loads the new cert:
ERROR:   docker restart videocall-e2e-webtransport-api-1
EOF
}

# ---------------------------------------------------------------------------
# --verify mode: never write anything; report and exit.
# ---------------------------------------------------------------------------

if [[ "${MODE}" == verify ]]; then
  if run_validators; then
    echo "OK: WT dev cert at ${CERT_PEM} passes all preflight checks."
    openssl x509 -in "${CERT_PEM}" -noout -subject -startdate -enddate -ext subjectAltName \
      | sed 's/^/  /'
    exit 0
  fi
  print_diagnostic
  exit 1
fi

# ---------------------------------------------------------------------------
# Regen mode: skip if existing cert passes, regen otherwise.
# ---------------------------------------------------------------------------

needs_regen=1
if [[ ${FORCE} -eq 0 ]] && run_validators; then
  # All checks pass AND we still have >= 1 day of life left? Skip.
  if openssl x509 -in "${CERT_PEM}" -noout -checkend 86400 >/dev/null 2>&1; then
    needs_regen=0
  fi
fi

if [[ ${needs_regen} -eq 0 ]]; then
  echo "Cert at ${CERT_PEM} is valid and has >= 1 day of life left. Skipping regen."
  echo "  (pass --force to regenerate anyway)"
  exit 0
fi

mkdir -p "${CERTS_DIR}"

# Build a minimal openssl config so we can attach the SubjectAltName the
# wasm client requires (Chromium rejects WebTransport certs without SAN).
TMPCONF="$(mktemp)"
trap 'rm -f "${TMPCONF}"' EXIT

cat > "${TMPCONF}" <<'EOF'
[req]
distinguished_name = req_dn
req_extensions     = v3_req
prompt             = no

[req_dn]
CN = 127.0.0.1

[v3_req]
basicConstraints = critical, CA:FALSE
keyUsage         = critical, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName   = @alt_names

[alt_names]
IP.1  = 127.0.0.1
DNS.1 = localhost
EOF

echo "Generating ECDSA P-256 key + self-signed cert (validity ${VALIDITY_DAYS} days)..."

# Generate the EC key in PKCS#8 envelope so it parses through
# `rustls_pemfile::private_key` at actix-api/src/webtransport/mod.rs:132.
openssl genpkey \
  -algorithm EC \
  -pkeyopt ec_paramgen_curve:P-256 \
  -out "${CERT_KEY}"

openssl req \
  -new -x509 \
  -key "${CERT_KEY}" \
  -out "${CERT_PEM}" \
  -days "${VALIDITY_DAYS}" \
  -config "${TMPCONF}" \
  -extensions v3_req \
  -sha256

chmod 600 "${CERT_KEY}"
chmod 644 "${CERT_PEM}"

# Compute SHA-256 of the DER-encoded cert (NOT the SPKI). This is what
# the WebTransport `serverCertificateHashes` option expects.
HASH_B64="$(openssl x509 -in "${CERT_PEM}" -outform DER \
  | openssl dgst -sha256 -binary \
  | openssl enc -base64)"

cat > "${HASH_FILE}" <<EOF
# Auto-generated by scripts/regen-dev-cert.sh — do not edit by hand.
#
# SHA-256 of the DER-encoded cert at actix-api/certs/localhost.pem,
# base64-encoded. This is the value the wasm WebTransport client passes via
# the \`serverCertificateHashes\` constructor option. Regenerated together
# with the cert; the cert's validity is capped at ${VALIDITY_DAYS} days because
# Chromium rejects \`serverCertificateHashes\` entries for any cert with more
# than 14 days of validity.
${HASH_B64}
EOF

# ---------------------------------------------------------------------------
# Post-regen self-check: make sure what we just produced is what we promised.
# A bug in the openssl invocation above (e.g. wrong validity flag, missing
# SAN, RSA fallback on a misconfigured openssl build) would otherwise ship
# a broken cert with no warning. Bail loudly instead.
# ---------------------------------------------------------------------------

if ! run_validators; then
  print_diagnostic
  exit 1
fi

echo
echo "Wrote:"
echo "  ${CERT_PEM}"
echo "  ${CERT_KEY}"
echo "  ${HASH_FILE}    (sha256 = ${HASH_B64})"
echo
openssl x509 -in "${CERT_PEM}" -noout -subject -issuer -startdate -enddate -ext subjectAltName

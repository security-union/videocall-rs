import { defineConfig, devices } from "@playwright/test";

// ---------------------------------------------------------------------------
// WebTransport dev cert hash
// ---------------------------------------------------------------------------
//
// Chromium 145 IGNORES `--ignore-certificate-errors-spki-list` for
// QUIC/HTTP-3 connections (verified empirically — the freeze regression
// spec failed every CI run from 2026-05-13 onward with
// `net::ERR_QUIC_PROTOCOL_ERROR.QUIC_TLS_CERTIFICATE_UNKNOWN` until this
// flag was replaced). The supported path for self-signed dev certs over
// WebTransport is the constructor option:
//
//   new WebTransport(url, {
//     serverCertificateHashes: [{ algorithm: "sha-256", value: <bytes> }],
//   })
//
// Constraints from the W3C WebTransport spec + Chromium impl:
//   - Cert MUST be ECDSA (P-256 recommended).
//   - Cert validity period MUST be <= 14 days.
//   - Hash is SHA-256 of the **DER-encoded cert** (NOT the SPKI).
//
// The cert + hash are produced together by `scripts/regen-dev-cert.sh` and
// the hash is checked into `actix-api/certs/localhost.cert-sha256.txt`. Each
// authenticated context injects the hash into `window.__VC_WT_CERT_HASHES__`
// before the wasm boots — see `e2e/helpers/auth-context.ts::CERT_HASH_INIT_SCRIPT`
// and the wasm side at
// `videocall-transport/src/webtransport.rs::read_wt_cert_hash_options`.
//
// To regenerate manually: `make e2e-cert` (or `scripts/regen-dev-cert.sh`).
//
// `--ignore-certificate-errors` is kept because it still bypasses TCP/TLS
// validation for incidental HTTPS traffic (e.g. health probes); the new
// hash-injection plumbing is only used for the QUIC handshake.

const CHROME_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
  "--disable-dev-shm-usage",
  "--renderer-process-limit=1",
];

process.env.DISPLAY_NAME_RATE_LIMIT_DISABLED ??= "true";

export default defineConfig({
  globalSetup: "./global-setup.ts",
  testDir: "./tests",
  fullyParallel: false,
  workers: 2,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: [["html", { open: "never" }]],
  use: {
    ignoreHTTPSErrors: true,
  },
  projects: [
    {
      name: "dioxus",
      use: {
        baseURL: "http://localhost:3001",
        ...devices["Desktop Chrome"],
        launchOptions: { args: CHROME_ARGS },
      },
    },
  ],
});

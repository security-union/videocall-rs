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

// Project taxonomy:
//   - `dioxus`  — full suite (34 specs / 207 tests today); the historical
//                 default. CI keeps using this.
//   - `bvt1`    — fast smoke superset run on every PR (~tens of seconds).
//                 Includes anything tagged `@bvt0` or `@bvt1`.
//   - `bvt0`    — minimum viable smoke. Just `@bvt0`-tagged tests. Used for the
//                 fastest possible "is the app even alive" check (a single test today).
//
// Adding tests to a project is opt-in via test-name tags, e.g.:
//     test("home page loads with meeting form @bvt0 @bvt1", ...)
// Untagged tests run only under the `dioxus` project. Tagged tests run under
// `dioxus` AND under any matching bvt project — Playwright does NOT dedupe
// across projects when no `--project` flag is given. To avoid running a tagged
// test multiple times, every invocation site MUST pass an explicit
// `--project=...` (or `make e2e-*` target):
//   - `make e2e` / `npm run test`         → `--project=dioxus` (full suite)
//   - `make e2e-bvt0`                     → `--project=bvt0` (1 test)
//   - `make e2e-bvt1`                     → `--project=bvt1` (smoke superset)
//   - GitHub workflow `e2e-hcl.yaml`      → `--project=dioxus` (full suite)
// Adding a new entry point that omits `--project` will cause tagged tests to
// run multiple times — keep this contract intact.

const dioxusUse = {
  baseURL: "http://localhost:3001",
  ...devices["Desktop Chrome"],
  launchOptions: { args: CHROME_ARGS },
};

export default defineConfig({
  globalSetup: "./global-setup.ts",
  testDir: "./tests",
  fullyParallel: false,
  workers: 2,
  retries: process.env.CI ? 1 : 0,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: [["html", { open: "never" }]],
  use: {
    ignoreHTTPSErrors: true,
  },
  projects: [
    {
      name: "dioxus",
      use: dioxusUse,
    },
    {
      name: "bvt0",
      use: dioxusUse,
      grep: /@bvt0\b/,
    },
    {
      name: "bvt1",
      use: dioxusUse,
      // bvt1 is a superset of bvt0: anything in bvt0 also runs here.
      grep: /@bvt[01]\b/,
    },
  ],
});

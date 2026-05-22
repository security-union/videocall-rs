import { defineConfig, devices } from "@playwright/test";

// SPKI sha256 (base64) of `actix-api/certs/localhost.pem` — the cert the
// `webtransport-api` service serves on UDP 4433. Compute with:
//   openssl x509 -in actix-api/certs/localhost.pem -pubkey -noout \
//     | openssl pkey -pubin -outform DER \
//     | openssl dgst -sha256 -binary \
//     | base64
//
// `--ignore-certificate-errors` alone does NOT bypass cert validation for
// HTTP/3 / QUIC connections in modern Chromium — the WebTransport handshake
// to `https://127.0.0.1:4433` (forced by `--origin-to-force-quic-on`) rejects
// the self-signed dev cert unless its SPKI hash is explicitly pinned via
// this flag. We keep `--ignore-certificate-errors` for non-QUIC TLS traffic
// the suite may incidentally touch.
//
// If the cert is regenerated, recompute this value with the command above
// and update the constant in the same change — CI will start timing out the
// WebTransport-only specs otherwise (see
// `e2e/tests/wt-persistent-streams-freeze-regression.spec.ts`).
const WT_CERT_SPKI_SHA256 = "Oahy1QQGBftLedXEonkr6apvCmD3JnqVaCItsq8w/Zk=";

const CHROME_ARGS = [
  "--ignore-certificate-errors",
  `--ignore-certificate-errors-spki-list=${WT_CERT_SPKI_SHA256}`,
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

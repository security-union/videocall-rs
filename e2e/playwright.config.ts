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

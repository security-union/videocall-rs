/**
 * Shared browser-launch and authentication scaffolding for E2E tests.
 *
 * Centralises the three pieces of boilerplate that every guest-flow spec
 * (and many other specs) previously duplicated:
 *
 *  1. `BROWSER_ARGS` — the Chromium flags required for WebTransport / fake
 *     media / HTTPS-bypass in the CI stack.
 *  2. `createAuthenticatedContext` — launches a new BrowserContext and injects
 *     a signed session JWT cookie so the UI treats the page as logged-in.
 *  3. `launchAuthenticatedBrowser` — convenience wrapper that combines
 *     `chromium.launch`, `createAuthenticatedContext`, and `newPage` into a
 *     single call for the common pattern used by most specs.
 *
 * Adding a new meeting-settings boolean or changing the cookie name now only
 * requires updating this file.
 */

import { chromium, BrowserContext } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";
import { generateSessionToken } from "./auth";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

/**
 * Chromium launch flags shared by all E2E tests.
 *
 * - `--ignore-certificate-errors` bypasses TCP/TLS verification for
 *   incidental HTTPS traffic. **It does NOT bypass QUIC/HTTP-3 cert
 *   verification** in modern Chromium; the WebTransport handshake to the
 *   local self-signed dev cert relies on the wasm client passing
 *   `serverCertificateHashes` to the `WebTransport` constructor. The hash
 *   is read from `actix-api/certs/localhost.cert-sha256.txt` (regenerated
 *   by `scripts/regen-dev-cert.sh`) and injected into every page via
 *   `CERT_HASH_INIT_SCRIPT` below.
 * - `--origin-to-force-quic-on` is what tells Chromium to attempt QUIC for
 *   the `https://127.0.0.1:4433` target the wasm client dials.
 * - `--use-fake-*` replace real camera/mic with synthetic streams so tests
 *   run headlessly without physical devices.
 * - `--disable-gpu` avoids GPU-related flakes in CI containers.
 */
export const BROWSER_ARGS: string[] = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
  "--disable-dev-shm-usage",
  "--renderer-process-limit=1",
];

// ---------------------------------------------------------------------------
// WebTransport cert-hash injection
// ---------------------------------------------------------------------------

/**
 * Path to the base64 SHA-256 of the DER-encoded WT dev cert. Produced by
 * `scripts/regen-dev-cert.sh` alongside `actix-api/certs/localhost.pem`.
 * The file may contain `#`-prefixed comment lines and trailing whitespace.
 *
 * **Gitignored** (issue #2159) — it is the fingerprint of a machine-local,
 * 13-day cert, so a fresh clone does not have it until an `e2e-cert`-dependent
 * Make target runs.
 */
const CERT_HASH_FILE = path.resolve(
  __dirname,
  "..",
  "..",
  "actix-api",
  "certs",
  "localhost.cert-sha256.txt",
);

/**
 * Read the dev cert hash file and return the base64 SHA-256 strings, or an
 * empty array if the file is missing. Strips comment and blank lines so
 * the human-friendly preamble in the file does not poison the wasm side.
 *
 * Returns empty rather than throwing because this runs at module import, and
 * every spec — including the WS-only majority that never needs the hash —
 * imports this module. Throwing here would break them all. The actionable error
 * belongs at the point of USE: see {@link assertDevCertHashesPresent}.
 *
 * The `filePath` parameter exists so the unit test can point at a fixture
 * instead of the developer's real cert directory.
 */
export function readDevCertHashes(filePath: string = CERT_HASH_FILE): string[] {
  let raw: string;
  try {
    raw = fs.readFileSync(filePath, "utf8");
  } catch {
    return [];
  }
  return raw
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0 && !line.startsWith("#"));
}

/**
 * Throw when the WT dev cert hash is absent (#2159). Without it the failure is
 * silent rather than loud: {@link CERT_HASH_INIT_SCRIPT} becomes a no-op, wasm
 * falls back to a bare `WebTransport::new(url)`, and the self-signed cert is
 * rejected as `QUIC_TLS_CERTIFICATE_UNKNOWN` — which reads like a broken server.
 *
 * The remedy is TWO steps: a missing hash makes `regen-dev-cert.sh` rotate the
 * cert, and a running WT container has already loaded the old one.
 */
export function assertDevCertHashesPresent(hashes: string[] = CERT_HASHES): void {
  if (hashes.length > 0) return;
  throw new Error(
    `WT dev cert hash missing (${CERT_HASH_FILE}).\n` +
      `  make e2e-cert ARGS=--force\n` +
      `  docker restart videocall-e2e-webtransport-api-1\n` +
      `Both steps: regenerating rotates the cert, and a running WT container has ` +
      `already loaded the old one. The hash is machine-local and gitignored (#2159), ` +
      `so a fresh clone will not have it. Without it the WebTransport handshake ` +
      `fails later as QUIC_TLS_CERTIFICATE_UNKNOWN.`,
  );
}

// Snapshotted at module import. If the dev cert is regenerated mid-run
// (e.g. interactive `make e2e-headed` overlapping `make e2e-cert --force`),
// restart Playwright workers so this constant is re-read.
const CERT_HASHES = readDevCertHashes();

/**
 * Init script attached to every authenticated `BrowserContext`. Sets
 * `window.__VC_WT_CERT_HASHES__` so the wasm WebTransport client at
 * `videocall-transport/src/webtransport.rs::read_wt_cert_hash_options`
 * picks up the dev cert hash before any `new WebTransport(...)` call.
 *
 * Must run BEFORE the wasm boots — `addInitScript` guarantees this for
 * every page in the context (Playwright runs init scripts before any of
 * the page's own scripts on every navigation, including the initial one).
 *
 * If `CERT_HASHES` is empty (file missing), this becomes a no-op and the
 * wasm falls back to bare `WebTransport::new(url)` — which is also the
 * production path. That fallback is silent, which is
 * why `global-setup.ts` asserts the hash is present before the run.
 */
export const CERT_HASH_INIT_SCRIPT = (() => {
  // Embed the hashes literally so the closure has no closure-over-outer-scope
  // dependency at evaluation time inside the page.
  const literal = JSON.stringify(CERT_HASHES);
  return `(() => {
  try {
    Object.defineProperty(window, "__VC_WT_CERT_HASHES__", {
      value: ${literal},
      writable: false,
      configurable: false,
    });
  } catch (_) {
    // Already defined (re-init on same context); leave existing value alone.
  }
})();`;
})();

export const DEFAULT_WEBSOCKET_TRANSPORT_INIT_SCRIPT = `(() => {
  try {
    if (!localStorage.getItem("vc_transport_preference")) {
      localStorage.setItem("vc_transport_preference", "websocket");
      localStorage.setItem("vc_transport_sticky", "true");
    }
  } catch (_) {}
})();`;

// ---------------------------------------------------------------------------
// Core helper
// ---------------------------------------------------------------------------

/**
 * Create a new `BrowserContext` for the given browser and inject a signed
 * session JWT cookie so the Dioxus UI treats the page as authenticated.
 *
 * @param browser  A Playwright `Browser` instance (typically from `chromium.launch`).
 * @param email    The `sub` / `user_id` field in the JWT payload.
 * @param name     The `name` field in the JWT payload (used as display name).
 * @param uiURL    Base URL of the Dioxus UI (e.g. `http://localhost:3001`).
 */
export async function createAuthenticatedContext(
  browser: Awaited<ReturnType<typeof chromium.launch>>,
  email: string,
  name: string,
  uiURL: string,
): Promise<BrowserContext> {
  const context = await browser.newContext({
    baseURL: uiURL,
    ignoreHTTPSErrors: true,
  });

  // Inject the WT dev cert hash BEFORE any page navigates / boots wasm.
  // Harmless for WS-only specs (the wasm only reads the global on the WT
  // construction path).
  await context.addInitScript(CERT_HASH_INIT_SCRIPT);
  await context.addInitScript(DEFAULT_WEBSOCKET_TRANSPORT_INIT_SCRIPT);

  const token = generateSessionToken(email, name);
  const url = new URL(uiURL);

  await context.addCookies([
    {
      name: COOKIE_NAME,
      value: token,
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      secure: false,
      sameSite: "Lax",
    },
  ]);

  return context;
}

// ---------------------------------------------------------------------------
// Convenience wrapper
// ---------------------------------------------------------------------------

/**
 * Launch a Chromium browser, create an authenticated context, and open a
 * first page — all in one call.
 *
 * Useful for the common pattern:
 * ```ts
 * const browser = await chromium.launch({ args: BROWSER_ARGS });
 * const context = await createAuthenticatedContext(browser, email, name, uiURL);
 * const page    = await context.newPage();
 * ```
 *
 * Extra Chromium args (e.g. `--use-file-for-fake-audio-capture=...`) can be
 * appended via the optional `extraArgs` parameter.
 */
export async function launchAuthenticatedBrowser(
  email: string,
  name: string,
  uiURL: string,
  extraArgs: string[] = [],
): Promise<{
  browser: Awaited<ReturnType<typeof chromium.launch>>;
  context: BrowserContext;
  page: Awaited<ReturnType<BrowserContext["newPage"]>>;
}> {
  const browser = await chromium.launch({ args: [...BROWSER_ARGS, ...extraArgs] });
  const context = await createAuthenticatedContext(browser, email, name, uiURL);
  const page = await context.newPage();
  return { browser, context, page };
}

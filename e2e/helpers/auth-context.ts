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
import { generateSessionToken } from "./auth";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

/**
 * Chromium launch flags shared by all E2E tests.
 *
 * - `--ignore-certificate-errors` and `--origin-to-force-quic-on` enable
 *   WebTransport against the local stack.
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
];

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

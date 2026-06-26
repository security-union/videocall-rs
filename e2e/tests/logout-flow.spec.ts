/**
 * E2E: OIDC logout flow via top-level navigation (#1547 / PR #1550).
 *
 * Verifies that clicking Sign Out:
 *  1. Performs a top-level navigation to the backend /logout endpoint
 *     (NOT a fetch/XHR — SameSite=Lax cookies only ride navigations).
 *  2. Synchronously clears client-side auth state (sessionStorage tokens)
 *     BEFORE navigating, so a post-logout redirect back to the SPA sees a
 *     signed-out state instead of auto-re-authenticating.
 *
 * The e2e stack has no real IdP, so this exercises the FALLBACK redirect
 * branch (no end_session_endpoint). The client-side token clear + top-level
 * navigation is the load-bearing behavior this PR introduces.
 */

import { test, expect } from "@playwright/test";
import { chromium, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";

const UI_URL = process.env.DIOXUS_URL || "http://localhost:3001";

/**
 * Route-patch config.local.js, which loads AFTER config.js (and therefore wins),
 * to enable the OAuth UI and point meetingApiBaseUrl at the meeting API. Without
 * this the auth dropdown (`.auth-dropdown-trigger`) never renders.
 *
 * Patching config.js alone is insufficient: config.local.js loads afterwards and
 * clobbers the override. Patch the file that wins.
 */
async function enableOAuthConfig(context: BrowserContext): Promise<void> {
  await context.route("**/config.local.js", async (route) => {
    const overrides = JSON.stringify({
      oauthEnabled: "true",
      oauthFlow: "pkce",
      meetingApiBaseUrl: UI_URL.replace(":3001", ":8081"),
    });

    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: `window.__APP_CONFIG=Object.assign(window.__APP_CONFIG||{},${overrides});`,
    });
  });

  // The E2E stack has no OIDC verifier, so its meeting API cannot validate the
  // deliberately fake PKCE token used to exercise the client logout path. Mock
  // the /session call so auth state settles and the dropdown renders signed-in.
  await context.route("**:8081/session", async (route) => {
    await route.fulfill({
      status: 200,
      headers: {
        "access-control-allow-credentials": "true",
        "access-control-allow-origin": UI_URL,
      },
      body: "",
    });
  });
}

/** Seed the state written by the OAuth callback before the application boots. */
async function seedClientAuthState(context: BrowserContext, idToken?: string): Promise<void> {
  await context.addInitScript((token) => {
    sessionStorage.setItem("vc_access_token", "fake-access-token");
    sessionStorage.setItem("vc_refresh_token", "fake-refresh-token");
    sessionStorage.setItem("vc_profile_user_id", "e2e-logout@example.com");
    sessionStorage.setItem("vc_profile_display_name", "E2E Logout User");
    if (token) {
      sessionStorage.setItem("vc_id_token", token);
    }
  }, idToken);
}

test.describe("Logout flow (#1547)", () => {
  let browser: Awaited<ReturnType<typeof chromium.launch>>;
  let context: BrowserContext;

  test.beforeAll(async () => {
    browser = await chromium.launch({ args: BROWSER_ARGS });
  });

  test.afterAll(async () => {
    await browser.close();
  });

  test.beforeEach(async () => {
    context = await createAuthenticatedContext(
      browser,
      "e2e-logout@example.com",
      "E2E Logout User",
      UI_URL,
    );
    await enableOAuthConfig(context);
  });

  test.afterEach(async () => {
    await context.close();
  });

  test("Sign Out performs a top-level navigation to /logout (not fetch) @bvt1", async () => {
    await seedClientAuthState(context);
    const page = await context.newPage();

    // Abort the /logout navigation so the page stays on the :3001 origin.
    // The synchronous WASM clear_* + set_href fires before the browser leaves,
    // and aborting prevents the cross-origin hop that would lose sessionStorage.
    await page.route("**/logout*", async (route) => {
      await route.abort();
    });

    await page.goto("/");
    const trigger = page.locator(".auth-dropdown-trigger");
    await expect(trigger).toBeVisible({ timeout: 10_000 });

    // Track only NAVIGATION requests to /logout — fetch/XHR must not count.
    let logoutNavDetected = false;
    let logoutFetchDetected = false;
    page.on("request", (req) => {
      if (req.url().includes("/logout")) {
        if (req.isNavigationRequest()) {
          logoutNavDetected = true;
        } else {
          logoutFetchDetected = true;
        }
      }
    });

    // Open the dropdown and click Sign Out
    await trigger.click();
    const signoutBtn = page.locator(".auth-dropdown-signout");
    await expect(signoutBtn).toBeVisible();
    await signoutBtn.click();

    // Give the set_href call a moment to fire
    await page.waitForTimeout(2_000);

    // The load-bearing assertion: /logout was reached via top-level navigation,
    // NOT via fetch(). This is the exact regression class #1547 introduced.
    expect(logoutNavDetected).toBe(true);
    expect(logoutFetchDetected).toBe(false);
  });

  test("After logout, sessionStorage tokens are cleared @bvt1", async () => {
    await seedClientAuthState(context, "fake-id-token");
    const page = await context.newPage();

    let logoutNavDetected = false;

    // Intercept the /logout top-level navigation and fulfill it with a
    // "204 No Content" SAME-ORIGIN response instead of aborting it.
    //
    // Why not abort (the obvious choice)? An aborted top-level navigation leaves
    // Chromium on an inaccessible error document, so any later sessionStorage
    // read throws SecurityError ("Access is denied for this document").
    //
    // Why not page.evaluate inside the handler (the reference spec's bug)? While
    // the navigation is in flight the original document is being torn down, so
    // evaluating against it throws "Target page ... has been closed".
    //
    // A 204 response to a navigation tells the browser "nothing to render" — it
    // does NOT commit a navigation, so the ORIGINAL :3001 document stays live and
    // scriptable, and the context init script does NOT re-run (no new document =
    // no re-seed). That gives us a stable, same-origin page whose sessionStorage
    // reflects exactly the synchronous clear the product ran in `on_logout`
    // (clear_access_token / clear_refresh_token / clear_id_token) BEFORE
    // set_href. If the product STOPPED clearing, the seeded values would survive
    // and the assertions below fail — mutation-sensitive, not vacuous.
    await page.route("**/logout*", async (route) => {
      logoutNavDetected = true;
      await route.fulfill({ status: 204, body: "" });
    });

    await page.goto("/");
    const trigger = page.locator(".auth-dropdown-trigger");
    await expect(trigger).toBeVisible({ timeout: 10_000 });

    // Click Sign Out
    await trigger.click();
    const signoutBtn = page.locator(".auth-dropdown-signout");
    await expect(signoutBtn).toBeVisible();
    await signoutBtn.click();

    // Wait until the /logout navigation has actually been requested (and
    // 204-fulfilled). That guarantees set_href fired, which means the
    // synchronous clear_* calls that precede it have already run.
    await expect.poll(() => logoutNavDetected, { timeout: 10_000 }).toBe(true);

    // Read the post-clear sessionStorage on the still-live original document.
    const tokensAfter = await page.evaluate(() => ({
      idToken: sessionStorage.getItem("vc_id_token"),
      accessToken: sessionStorage.getItem("vc_access_token"),
      refreshToken: sessionStorage.getItem("vc_refresh_token"),
    }));

    expect(tokensAfter.idToken).toBeNull();
    expect(tokensAfter.accessToken).toBeNull();
    expect(tokensAfter.refreshToken).toBeNull();
  });
});

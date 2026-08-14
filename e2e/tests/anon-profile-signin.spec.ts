/**
 * E2E: the Sign In button stays visible for ANONYMOUS profiles (issue #472 / PR #471).
 *
 * ## The bug
 *
 * When the backend runs with `ALLOW_ANONYMOUS=true` it hands the SPA a profile
 * whose `user_id` is an opaque `anon-<uuid>` string. That is NOT a real
 * identity — the user has not signed in. Before PR #471 the home page treated
 * any `Some(profile)` as "signed in", so it rendered the account dropdown and
 * the user had **no way to sign in at all**.
 *
 * ## What the fix looks like at head (`dioxus-ui/src/pages/home.rs`)
 *
 * Anonymous profiles are rejected in TWO places, and this spec pins the
 * user-visible outcome of the pair:
 *
 *  1. The profile-loading effect (~line 225) refuses to publish them into the
 *     `user_profile` signal at all:
 *         `if !profile.user_id.starts_with("anon-") { ... user_profile.set(...) }`
 *  2. The template (~line 357) filters them again before choosing a branch:
 *         `if let Some(profile) = user_profile().filter(|p| !p.user_id.starts_with("anon-"))`
 *
 * (1) is the load-bearing guard — line 247 is the only `user_profile.set(Some(..))`
 * in the file, i.e. the only writer that PUBLISHES a profile (`home.rs:311` also
 * writes, but `set(None)` on logout). An anon profile therefore never reaches
 * the template, and removing (2) alone changes nothing observable. Both halves were checked by
 * mutation against a live e2e stack: removing (2) alone left this spec green,
 * while removing (1) as well flipped the anonymous page to a `button
 * "Anonymous"` account dropdown and failed the first test below. Reverting the
 * fix therefore breaks this spec, which is the point of it.
 *
 * ## Harness: why this needs no docker-compose change
 *
 * Both halves of the precondition are reachable purely client-side:
 *
 *  - `oauthEnabled` is read from `window.__APP_CONFIG` (the runtime config.js),
 *    NOT from a backend env var — so route-patching the config gives us the
 *    OAuth-enabled UI without touching `docker-compose.e2e.yaml`.
 *  - Under the PKCE flow, `auth.rs::get_user_profile()` takes a fast path that
 *    returns the profile cached in `sessionStorage` (`vc_profile_user_id` /
 *    `vc_profile_display_name`) whenever a token is present. Seeding those keys
 *    is exactly what the OAuth callback page does, so we can hand the app an
 *    `anon-` profile without an IdP and without `ALLOW_ANONYMOUS=true`.
 *
 * This mirrors `logout-flow.spec.ts`, which stands up the same signed-in
 * home-page state.
 */

import { test, expect } from "@playwright/test";
import { chromium, BrowserContext, Page, Locator } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";

const UI_URL = process.env.DIOXUS_URL || "http://localhost:3001";

/**
 * The sign-in affordance in the home page's auth corner.
 *
 * `ProviderButton` (`dioxus-ui/src/components/login.rs`) renders one of THREE
 * variants depending on the runtime `oauthProvider` value:
 *
 *   "google" -> `button.gsi-material-button`     "Sign in with Google"
 *   "okta"   -> `button.okta-sign-in-button`     "Sign in with Okta"
 *   absent   -> `button.generic-sign-in-button`  "Sign in"
 *
 * The e2e stack sets `OAUTH_PROVIDER=google` (`docker-compose.e2e.yaml`), so
 * the Google variant is what actually renders — but matching on role +
 * accessible name pins the user-visible affordance ("a sign-in button in the
 * auth corner") across all three, rather than hard-coding one stack's class.
 *
 * Scoped to `.auth-dropdown-container` so it cannot match the SECOND
 * `ProviderButton` that `components/meetings_list.rs` renders inside its
 * `.meetings-auth-prompt`.
 */
function signInButton(page: Page): Locator {
  return page.locator(".auth-dropdown-container").getByRole("button", { name: /^Sign in/i });
}

/**
 * Turn the OAuth UI on for this context by patching `config.local.js`.
 *
 * `index.html` loads `/config.js` first and then evals `/config.local.js` via a
 * synchronous XHR, so `config.local.js` WINS. The e2e stack ships one
 * (`docker/start-dioxus.sh` generates it from env, with `oauthEnabled: "false"`
 * because the compose file sets `ENABLE_OAUTH=false`), so patching `/config.js`
 * alone would be silently clobbered.
 *
 * We fetch the served body and APPEND our overrides rather than replacing it,
 * so every docker-generated key survives — in particular `apiBaseUrl`
 * (`http://localhost:8081`), which `constants.rs::meeting_api_base_url()` falls
 * back to when `meetingApiBaseUrl` is absent.
 *
 * When the shim is ABSENT we must serve the patch ALONE, and the fetched body
 * cannot be trusted to be JS. `index.html:26-31` records that dev servers
 * (`trunk serve` / `dx serve`) answer missing paths with the SPA `index.html`
 * fallback rather than a 404 — so `route.fetch()` can return `200` + HTML.
 * Prepending that HTML would make our body start with `<`, and the loader's
 * first-byte sniff (`index.html:44`: `if (!body || body.charAt(0) === "<")
 * return;`) would discard the whole override silently. So keep the fetched body
 * only when it is JS-shaped; otherwise emit just the patch, which the loader
 * does eval.
 *
 * `oauthFlow: "pkce"` is required: `is_pkce_flow()` gates the sessionStorage
 * profile fast-path that this spec depends on.
 *
 * Values are STRINGS because `RuntimeConfig::oauth_enabled` is a `String` field
 * run through `videocall_types::truthy`.
 */
async function enableOAuthConfig(context: BrowserContext): Promise<void> {
  const overrides = JSON.stringify({ oauthEnabled: "true", oauthFlow: "pkce" });
  const injection = `;window.__APP_CONFIG=Object.assign(window.__APP_CONFIG||{},${overrides});`;

  await context.route("**/config.local.js", async (route) => {
    let original = "";
    try {
      const response = await route.fetch();
      if (response.status() === 200) {
        original = await response.text();
      }
    } catch {
      /* shim absent on this serve — serve just the patch */
    }
    // Drop an SPA-fallback (or otherwise non-JS) body; see the note above.
    const shim = original.trimStart().startsWith("<") ? "" : original;
    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: shim + injection,
    });
  });
}

/**
 * Stub `GET /session` so `auth.rs::check_session()` succeeds.
 *
 * The e2e meeting-api has no OIDC verifier configured (all `OAUTH_*` vars are
 * empty in `docker-compose.e2e.yaml`), so it cannot validate the deliberately
 * fake PKCE access token seeded below — it would answer 401 and the profile
 * effect would bail before ever reading the profile.
 *
 * Returns a hit counter so each test can PROVE the app actually reached the
 * profile-loading path instead of asserting against a page that never got
 * there. Without that proof the anon test could pass vacuously: the sign-in
 * button also renders when the session check fails.
 *
 * The counter is incremented AFTER `fulfill` resolves, so a non-zero count
 * means the 200 was actually handed back to the page — not merely that a
 * request (or a CORS preflight on the same route) was intercepted.
 */
async function stubSessionEndpoint(context: BrowserContext): Promise<{ hits: () => number }> {
  let hits = 0;
  await context.route("**:8081/session", async (route) => {
    await route.fulfill({
      status: 200,
      headers: {
        "access-control-allow-origin": UI_URL,
        "access-control-allow-credentials": "true",
        "access-control-allow-headers": "authorization,content-type",
        "access-control-allow-methods": "GET,OPTIONS",
      },
      body: "",
    });
    hits += 1;
  });
  return { hits: () => hits };
}

/**
 * Seed the exact sessionStorage state the OAuth callback page writes after a
 * successful token exchange (`auth.rs::store_user_profile` +
 * `store_access_token`). `get_user_profile()` serves this cache verbatim on the
 * PKCE fast path, which is how we inject an `anon-` identity with no IdP.
 */
async function seedProfile(
  context: BrowserContext,
  userId: string,
  displayName: string,
): Promise<void> {
  await context.addInitScript(
    ([id, name]) => {
      sessionStorage.setItem("vc_access_token", "fake-access-token");
      sessionStorage.setItem("vc_profile_user_id", id);
      sessionStorage.setItem("vc_profile_display_name", name);
    },
    [userId, displayName],
  );
}

test.describe("Anonymous profile keeps Sign In visible (#472)", () => {
  let browser: Awaited<ReturnType<typeof chromium.launch>>;
  let context: BrowserContext;
  let session: { hits: () => number };

  test.beforeAll(async () => {
    browser = await chromium.launch({ args: BROWSER_ARGS });
  });

  test.afterAll(async () => {
    await browser.close();
  });

  test.beforeEach(async () => {
    context = await createAuthenticatedContext(
      browser,
      "e2e-anon@example.com",
      "E2E Anon User",
      UI_URL,
    );
    await enableOAuthConfig(context);
    session = await stubSessionEndpoint(context);
  });

  test.afterEach(async () => {
    await context.close();
  });

  /**
   * THE REGRESSION. Reverting guard (1) in `home.rs` — the
   * `if !profile.user_id.starts_with("anon-")` around `user_profile.set(...)`
   * in the profile-loading effect — republishes the seeded `anon-` profile into
   * the signal. The template then takes the signed-in branch, the sign-in
   * button disappears and `.auth-dropdown-trigger` appears, failing BOTH
   * assertions below.
   */
  test("Sign In renders when the profile is anonymous @bvt1", async () => {
    await seedProfile(context, "anon-7f3c2b1a-9d4e-4c8f-a1b2-3c4d5e6f7a8b", "Anonymous");
    const page = await context.newPage();
    await page.goto("/");

    // Presence before absence: the hero container is the PARENT that holds the
    // auth UI, so a blank / crashed page cannot make the absence check below
    // pass vacuously.
    await expect(page.locator(".hero-container")).toBeVisible({ timeout: 15_000 });

    // Prove the app actually walked the profile path (see stubSessionEndpoint).
    await expect.poll(() => session.hits(), { timeout: 15_000 }).toBeGreaterThan(0);

    // The signed-OUT branch of home.rs wraps `ProviderButton` in
    // `.auth-dropdown-container`. The container class is shared by BOTH
    // branches, so it proves nothing on its own — the button inside it is the
    // discriminator. See `signInButton` for the variant handling.
    await expect(signInButton(page)).toBeVisible({ timeout: 15_000 });

    // The account dropdown trigger belongs to the signed-in branch only and
    // must not exist. The control test below shows this element appears well
    // within the expect timeout once a NON-anon profile is published, so a
    // count of 0 here is a real absence and not an un-awaited race.
    await expect(page.locator(".auth-dropdown-trigger")).toHaveCount(0);
  });

  /**
   * ANTI-VACUITY CONTROL. Identical harness; the ONLY difference is that
   * `user_id` lacks the `anon-` prefix.
   *
   * This is what makes the test above meaningful. The sign-in button renders
   * whenever `user_profile()` is `None`, which is also what a broken harness
   * produces (config patch clobbered, session stub not matching, profile keys
   * never read). If any of that were broken this control would fail, because
   * only a profile that survives the full config -> check_session ->
   * get_user_profile -> signal chain can render `.auth-dropdown-trigger`.
   */
  test("Control: a non-anonymous profile renders the account dropdown instead @bvt1", async () => {
    await seedProfile(context, "e2e-anon@example.com", "E2E Anon User");
    const page = await context.newPage();
    await page.goto("/");

    await expect(page.locator(".hero-container")).toBeVisible({ timeout: 15_000 });
    await expect.poll(() => session.hits(), { timeout: 15_000 }).toBeGreaterThan(0);

    const trigger = page.locator(".auth-dropdown-trigger");
    await expect(trigger).toBeVisible({ timeout: 15_000 });
    await expect(trigger).toContainText("E2E Anon User");

    // Signed in => the sign-in button must be gone from the auth corner.
    await expect(signInButton(page)).toHaveCount(0);
  });
});

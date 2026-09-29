/**
 * E2E: the home Display Name under cookie-flow OAuth (#1646).
 */

import { test, expect } from "@playwright/test";
import { chromium, BrowserContext, Page, Locator } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";

const UI_URL = process.env.DIOXUS_URL || "http://localhost:3001";
const API_URL = UI_URL.replace(":3001", ":8081");
const NAME_KEY = "vc_display_name";
const OWNER_KEY = "vc_display_name_uid";
const STORED_NAME = "Tony gMail";
const TYPED_NAME = "Typed Name";
const PROFILE = { user_id: "antonio@example.com", name: "Antonio Estrada" };
// First 16 hex chars of SHA-256(PROFILE.user_id).
const PROFILE_OWNER = "50e4c4f5f12ed16c";
const CORS_HEADERS = {
  "access-control-allow-origin": UI_URL,
  "access-control-allow-credentials": "true",
};

type Profile = typeof PROFILE;

function signInButton(page: Page): Locator {
  return page.locator(".auth-dropdown-container").getByRole("button", { name: /^Sign in/i });
}

function usernameError(page: Page): Locator {
  return page.locator('label[for="username"] .field-label__error');
}

function heldResponse(): { hold: Promise<void>; release: () => void } {
  let release = () => {};
  const hold = new Promise<void>((resolve) => {
    release = resolve;
  });
  return { hold, release };
}

function storedName(page: Page): Promise<{ name: string | null; owner: string | null }> {
  return page.evaluate(
    ([nameKey, ownerKey]) => ({
      name: localStorage.getItem(nameKey),
      owner: localStorage.getItem(ownerKey),
    }),
    [NAME_KEY, OWNER_KEY],
  );
}

// An empty oauthFlow selects the cookie flow, whose sign-in navigates to the backend /login.
async function enableCookieFlowOAuth(context: BrowserContext): Promise<void> {
  const overrides = JSON.stringify({
    oauthEnabled: "true",
    oauthFlow: "",
    meetingApiBaseUrl: API_URL,
  });
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
    const shim = original.trimStart().startsWith("<") ? "" : original;
    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: shim + injection,
    });
  });
}

async function stubSession(
  context: BrowserContext,
  signedIn: boolean,
  { profile = PROFILE, hold = Promise.resolve() }: { profile?: Profile; hold?: Promise<void> } = {},
): Promise<{ hits: () => number }> {
  let hits = 0;
  await context.route("**:8081/session", async (route) => {
    await hold;
    await route.fulfill({ status: signedIn ? 200 : 401, headers: CORS_HEADERS, body: "" });
    hits += 1;
  });
  await context.route("**:8081/profile", (route) =>
    route.fulfill({
      status: 200,
      headers: CORS_HEADERS,
      contentType: "application/json",
      body: JSON.stringify({ success: true, result: profile }),
    }),
  );
  return { hits: () => hits };
}

async function seedStorage(
  context: BrowserContext,
  entries: Record<string, string>,
): Promise<void> {
  await context.addInitScript((items) => {
    for (const [key, value] of Object.entries(items)) {
      localStorage.setItem(key, value);
    }
  }, entries);
}

async function openHome(context: BrowserContext): Promise<Page> {
  const page = await context.newPage();
  await page.goto("/");
  await expect(page.locator(".hero-container")).toBeVisible({ timeout: 15_000 });
  return page;
}

async function expectSignedInAs(page: Page, profile: Profile): Promise<void> {
  await expect(page.locator(".auth-dropdown-trigger")).toContainText(profile.name, {
    timeout: 15_000,
  });
}

test.describe("Home Display Name under cookie-flow OAuth (#1646)", () => {
  let browser: Awaited<ReturnType<typeof chromium.launch>>;
  let context: BrowserContext;

  test.beforeAll(async () => {
    browser = await chromium.launch({ args: BROWSER_ARGS });
  });

  test.afterAll(async () => {
    await browser.close();
  });

  test.beforeEach(async () => {
    context = await createAuthenticatedContext(browser, PROFILE.user_id, PROFILE.name, UI_URL);
    await enableCookieFlowOAuth(context);
  });

  test.afterEach(async () => {
    await context.close();
  });

  test("signed in, the stored name wins over the profile name @bvt1", async () => {
    await stubSession(context, true);
    await seedStorage(context, { [NAME_KEY]: STORED_NAME });
    const page = await openHome(context);

    await expectSignedInAs(page, PROFILE);
    await expect(page.locator("#username")).toHaveValue(STORED_NAME);
    expect((await storedName(page)).name).toBe(STORED_NAME);
  });

  test("signed in with nothing stored, the profile name is shown and saved @bvt1", async () => {
    await stubSession(context, true);
    const page = await openHome(context);

    await expectSignedInAs(page, PROFILE);
    await expect(page.locator("#username")).toHaveValue(PROFILE.name);
    expect((await storedName(page)).name).toBe(PROFILE.name);
  });

  test("signed in, an invalid profile name is shown with its error and not saved @bvt1", async () => {
    const profile = { ...PROFILE, name: "Antonio Estrada (Tony)" };
    await stubSession(context, true, { profile });
    const page = await openHome(context);

    await expectSignedInAs(page, profile);
    await expect(page.locator("#username")).toHaveValue(profile.name);
    await expect(usernameError(page)).toHaveText("Not allowed: '(', ')'");
    expect(await storedName(page)).toEqual({ name: null, owner: null });
  });

  test("signed in, a name stored for another user is replaced by the profile name @bvt1", async () => {
    await stubSession(context, true);
    await seedStorage(context, {
      [NAME_KEY]: STORED_NAME,
      [OWNER_KEY]: "someone-else@example.com",
    });
    const page = await openHome(context);

    await expectSignedInAs(page, PROFILE);
    await expect(page.locator("#username")).toHaveValue(PROFILE.name);
    expect(await storedName(page)).toEqual({ name: PROFILE.name, owner: PROFILE_OWNER });
  });

  test("signed in, a name typed before the profile loads is kept @bvt1", async () => {
    const session = heldResponse();
    await stubSession(context, true, { hold: session.hold });
    const page = await openHome(context);

    // /session answers only after the typing, so the profile arrives to a non-empty field.
    await page.locator("#username").fill(TYPED_NAME);
    session.release();

    await expectSignedInAs(page, PROFILE);
    await expect(page.locator("#username")).toHaveValue(TYPED_NAME);
  });

  test("signed in, an empty-name error clears when the profile name fills the field @bvt1", async () => {
    const session = heldResponse();
    await stubSession(context, true, { hold: session.hold });
    const page = await openHome(context);

    await page.getByRole("button", { name: "Generate a New Meeting ID" }).click();
    await expect(usernameError(page)).toHaveText("Name cannot be empty.");
    session.release();

    await expectSignedInAs(page, PROFILE);
    await expect(page.locator("#username")).toHaveValue(PROFILE.name);
    await expect(usernameError(page)).toHaveText("");
  });

  test("signed out, the stored name is not shown @bvt1", async () => {
    const session = await stubSession(context, false);
    await seedStorage(context, { [NAME_KEY]: STORED_NAME });
    const page = await openHome(context);

    await expect.poll(() => session.hits(), { timeout: 15_000 }).toBeGreaterThan(0);
    await expect(signInButton(page)).toBeVisible({ timeout: 15_000 });
    await expect(page.locator("#username")).toHaveValue("");
    await expect(page.locator(".auth-dropdown-trigger")).toHaveCount(0);
  });

  test("signed out, Sign in forgets the stored name before leaving for /login @bvt1", async () => {
    const session = await stubSession(context, false);
    await seedStorage(context, { [NAME_KEY]: STORED_NAME, [OWNER_KEY]: PROFILE_OWNER });
    let loginUrl: string | null = null;
    // A 204 cancels the navigation, so the home document and its localStorage stay readable.
    await context.route(
      (url) => url.port === "8081" && url.pathname === "/login",
      async (route) => {
        loginUrl = route.request().url();
        await route.fulfill({ status: 204, body: "" });
      },
    );
    const page = await openHome(context);

    await expect.poll(() => session.hits(), { timeout: 15_000 }).toBeGreaterThan(0);
    await expect(signInButton(page)).toBeVisible({ timeout: 15_000 });
    expect(await storedName(page)).toEqual({ name: STORED_NAME, owner: PROFILE_OWNER });

    await signInButton(page).click();

    await expect.poll(() => loginUrl, { timeout: 10_000 }).toContain(`${API_URL}/login?returnTo=`);
    expect(await storedName(page)).toEqual({ name: null, owner: null });
  });
});

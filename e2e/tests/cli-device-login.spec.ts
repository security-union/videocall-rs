/**
 * meeting-api's server-rendered pages for `videocall-cli auth login`:
 * /cli/authorize -> /cli/consent -> code -> POST /api/v1/devices, and the
 * /devices list with Revoke. These pages live on the API origin (port 8081),
 * not the Dioxus UI, so every URL here is absolute and the session cookie is
 * injected for the API host.
 *
 * Stack requirement: DEVICE_TOKEN_AUDIENCE must equal API_URL (the e2e stack
 * defaults both to http://localhost:8081), or every page answers 503 and the
 * consent POST fails its Origin check.
 */
import { randomBytes } from "crypto";
import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import {
  API_URL,
  REMOTE_REDIRECT_URI,
  authorizeUrl,
  deviceJwt,
  generateDeviceKey,
  generatePkce,
  registrationJws,
  startLoopbackListener,
  uniqueDeviceName,
  uniqueEmail,
  type DeviceKey,
  type Pkce,
} from "../helpers/device-auth";

interface CliLogin {
  key: DeviceKey;
  pkce: Pkce;
  deviceName: string;
  state: string;
  redirectUri: string;
  /** The consent request id from the consent page URL. */
  requestId: string;
}

/**
 * Open an authorize link and land on the consent page. Remote login unless
 * a loopback `redirectUri` is given.
 */
async function openConsent(page: Page, redirectUri = REMOTE_REDIRECT_URI): Promise<CliLogin> {
  const key = generateDeviceKey();
  const pkce = generatePkce();
  const deviceName = uniqueDeviceName();
  const state = randomBytes(16).toString("base64url");
  await page.goto(
    authorizeUrl({
      publicKey: key.publicKey,
      deviceName,
      codeChallenge: pkce.challenge,
      state,
      redirectUri,
    }),
  );
  await expect(page).toHaveURL(/\/cli\/consent\?request=[A-Za-z0-9_-]{22}$/);
  await expect(page.getByRole("heading", { name: "Sign in videocall-cli?" })).toBeVisible();
  const requestId = new URL(page.url()).searchParams.get("request")!;
  return { key, pkce, deviceName, state, redirectUri, requestId };
}

/** Tick the remote checkbox, approve, and return the one-time code shown. */
async function approveAndReadCode(page: Page): Promise<string> {
  await page.getByLabel("I started this sign-in myself, on a device I own.").check();
  await page.getByRole("button", { name: "Approve" }).click();
  await expect(page.getByRole("heading", { name: "Your sign-in code" })).toBeVisible();
  const code = await page.getByLabel("Code", { exact: true }).inputValue();
  expect(code).toMatch(/^[A-Za-z0-9_-]{43}$/);
  return code;
}

/** Exchange the code at POST /api/v1/devices, as the CLI does. */
async function registerDevice(
  request: APIRequestContext,
  login: CliLogin,
  code: string,
): Promise<string> {
  const response = await request.post(`${API_URL}/api/v1/devices`, {
    data: {
      code,
      code_verifier: login.pkce.verifier,
      redirect_uri: login.redirectUri,
      registration: registrationJws(login.key, login.deviceName),
    },
  });
  expect(response.status(), await response.text()).toBe(201);
  const body = await response.json();
  expect(body.name).toBe(login.deviceName);
  expect(body.fingerprint).toBe(login.key.fingerprint);
  return body.device_id as string;
}

function devicesMe(request: APIRequestContext, key: DeviceKey, deviceId: string) {
  return request.get(`${API_URL}/api/v1/devices/me`, {
    headers: { Authorization: `Bearer ${deviceJwt(key, deviceId)}` },
  });
}

const SESSION_COOKIE = process.env.COOKIE_NAME || "session";

/** Console messages about Content Security Policy, for failure reports. */
function collectCspMessages(page: Page): string[] {
  const messages: string[] = [];
  page.on("console", (message) => {
    if (/content security policy|form-action/i.test(message.text())) {
      messages.push(message.text());
    }
  });
  return messages;
}

/** The browser shows the loopback listener's page, or the test reports why not. */
async function expectListenerReached(page: Page, cspMessages: string[]): Promise<void> {
  try {
    await expect(page.getByRole("heading", { name: "CLI listener reached" })).toBeVisible();
  } catch (error) {
    throw new Error(
      `Browser did not reach the loopback listener (now at ${page.url()}). ` +
        `CSP console messages: ${JSON.stringify(cspMessages)}`,
      { cause: error },
    );
  }
}

function deviceRow(page: Page, deviceName: string) {
  return page.getByRole("row").filter({ hasText: deviceName });
}

test.describe("CLI device login pages", () => {
  test.describe("signed in", () => {
    test.beforeEach(async ({ context }) => {
      // A fresh user per test keeps device lists independent.
      await injectSessionCookie(context, { baseURL: API_URL, email: uniqueEmail() });
    });

    test("remote login: consent shows the device, approve yields a code that registers it", async ({
      page,
      request,
    }) => {
      const login = await openConsent(page);

      await expect(page.locator("dl.facts")).toContainText(login.deviceName);
      await expect(page.locator(".fingerprint")).toHaveText(login.key.fingerprint);

      const code = await approveAndReadCode(page);
      const deviceId = await registerDevice(request, login, code);
      expect(deviceId).toMatch(/^[A-Za-z0-9_-]{22}$/);

      await page.goto(`${API_URL}/devices`);
      await expect(page.getByRole("heading", { name: "Your devices" })).toBeVisible();
      const row = deviceRow(page, login.deviceName);
      await expect(row).toHaveCount(1);
      await expect(row).toContainText(login.key.fingerprint);
    });

    test("remote login: approving without the checkbox changes nothing", async ({ page }) => {
      await openConsent(page);
      await page.getByRole("button", { name: "Approve" }).click();
      await expect(page.getByRole("heading", { name: "Confirmation needed" })).toBeVisible();
      await expect(page.getByRole("heading", { name: "Your sign-in code" })).toHaveCount(0);
    });

    test("revoke: the device leaves /devices and its key gets 401", async ({ page, request }) => {
      const login = await openConsent(page);
      const code = await approveAndReadCode(page);
      const deviceId = await registerDevice(request, login, code);

      // The signed device JWT works before revocation, so the 401 below is
      // caused by the revoke and not by a malformed token.
      const before = await devicesMe(request, login.key, deviceId);
      expect(before.status(), await before.text()).toBe(200);
      expect((await before.json()).device_id).toBe(deviceId);

      await page.goto(`${API_URL}/devices`);
      await page.getByRole("button", { name: `Revoke ${login.deviceName}` }).click();

      await expect(page).toHaveURL(`${API_URL}/devices`);
      await expect(deviceRow(page, login.deviceName)).toHaveCount(0);
      await expect(page.getByText("No devices are signed in.")).toBeVisible();

      const after = await devicesMe(request, login.key, deviceId);
      expect(after.status()).toBe(401);
    });

    test("deny: shows the denied page and creates no device", async ({ page }) => {
      const login = await openConsent(page);
      await page.getByRole("button", { name: "Deny" }).click();

      await expect(page.getByRole("heading", { name: "Request denied" })).toBeVisible();
      await expect(page.getByText("The device was not signed in.")).toBeVisible();

      await page.goto(`${API_URL}/devices`);
      await expect(page.getByRole("heading", { name: "Your devices" })).toBeVisible();
      await expect(deviceRow(page, login.deviceName)).toHaveCount(0);
      await expect(page.getByText("No devices are signed in.")).toBeVisible();
    });

    test("authorize with an invalid redirect_uri shows an error and does not redirect", async ({
      page,
    }) => {
      const key = generateDeviceKey();
      const url = authorizeUrl({
        publicKey: key.publicKey,
        deviceName: uniqueDeviceName(),
        codeChallenge: generatePkce().challenge,
        redirectUri: "https://attacker.example/callback",
      });

      const response = await page.goto(url);
      expect(response?.status()).toBe(400);
      expect(response?.request().redirectedFrom()).toBeNull();
      expect(page.url()).toBe(url);
      await expect(page.getByRole("heading", { name: "Sign-in link not valid" })).toBeVisible();
    });

    test("consent page: Approve is not focused on load and both buttons are keyboard reachable", async ({
      page,
    }) => {
      await openConsent(page);
      const approve = page.getByRole("button", { name: "Approve" });
      const deny = page.getByRole("button", { name: "Deny" });
      await expect(approve).not.toBeFocused();

      const reached = new Set<string>();
      for (let i = 0; i < 20 && reached.size < 2; i++) {
        await page.keyboard.press("Tab");
        if (await deny.evaluate((el) => el === document.activeElement)) reached.add("Deny");
        if (await approve.evaluate((el) => el === document.activeElement)) reached.add("Approve");
      }
      expect([...reached].sort()).toEqual(["Approve", "Deny"]);
    });

    test("consent page fits a 375 px wide viewport without horizontal scroll", async ({ page }) => {
      await page.setViewportSize({ width: 375, height: 812 });
      await openConsent(page);

      const overflow = await page.evaluate(
        () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
      );
      expect(overflow).toBeLessThanOrEqual(0);
      for (const name of ["Deny", "Approve"]) {
        const box = await page.getByRole("button", { name }).boundingBox();
        expect(box, `${name} button has a layout box`).not.toBeNull();
        expect(box!.x).toBeGreaterThanOrEqual(0);
        expect(box!.x + box!.width).toBeLessThanOrEqual(375);
      }
    });

    test("loopback login: approve sends the browser to the CLI listener with code and state", async ({
      page,
      request,
    }) => {
      const listener = await startLoopbackListener();
      try {
        const cspMessages = collectCspMessages(page);
        const consentResponse = page.waitForResponse(
          (response) =>
            new URL(response.url()).pathname === "/cli/consent" &&
            response.request().method() === "GET",
        );
        const login = await openConsent(page, listener.redirectUri);
        // Chromium enforces form-action on the redirect that follows a form
        // POST, so the page must allow exactly this callback and nothing wider.
        expect((await consentResponse).headers()["content-security-policy"]).toContain(
          `form-action 'self' ${listener.redirectUri};`,
        );
        // Loopback logins have no "I started this myself" checkbox.
        await expect(page.getByRole("checkbox")).toHaveCount(0);

        await page.getByRole("button", { name: "Approve" }).click();
        // The 303 after the form POST must pass the per-response CSP
        // `form-action 'self' http://127.0.0.1:{port}/callback`.
        await expectListenerReached(page, cspMessages);

        const hit = await listener.firstHit;
        const callback = new URL(hit.url, listener.redirectUri);
        expect(callback.pathname).toBe("/callback");
        expect(callback.searchParams.get("state")).toBe(login.state);
        expect(callback.searchParams.get("error")).toBeNull();
        const code = callback.searchParams.get("code");
        expect(code).toMatch(/^[A-Za-z0-9_-]{43}$/);
        expect(hit.headers.referer).toBeUndefined();
        expect(hit.headers.origin).toBeUndefined();

        await registerDevice(request, login, code!);
        await page.goto(`${API_URL}/devices`);
        await expect(deviceRow(page, login.deviceName)).toHaveCount(1);
      } finally {
        await listener.close();
      }
    });

    test("loopback login: deny sends the browser to the CLI listener with access_denied", async ({
      page,
    }) => {
      const listener = await startLoopbackListener();
      try {
        const cspMessages = collectCspMessages(page);
        const login = await openConsent(page, listener.redirectUri);
        await page.getByRole("button", { name: "Deny" }).click();
        await expectListenerReached(page, cspMessages);

        const hit = await listener.firstHit;
        const callback = new URL(hit.url, listener.redirectUri);
        expect(callback.pathname).toBe("/callback");
        expect(callback.searchParams.get("error")).toBe("access_denied");
        expect(callback.searchParams.get("state")).toBe(login.state);
        expect(callback.searchParams.get("code")).toBeNull();

        await page.goto(`${API_URL}/devices`);
        await expect(deviceRow(page, login.deviceName)).toHaveCount(0);
      } finally {
        await listener.close();
      }
    });

    test("sign out from the consent page clears the session and links back to the request", async ({
      page,
      context,
    }) => {
      const login = await openConsent(page);
      const consentUrl = `${API_URL}/cli/consent?request=${login.requestId}`;

      await page.getByRole("button", { name: "Not you? Sign out" }).click();
      await expect(page.getByRole("heading", { name: "Signed out" })).toBeVisible();

      const cookies = await context.cookies(API_URL);
      expect(cookies.map((cookie) => cookie.name)).not.toContain(SESSION_COOKIE);

      const href = await page.getByRole("link", { name: "Sign in again" }).getAttribute("href");
      const signIn = new URL(href!, API_URL);
      expect(signIn.origin).toBe(API_URL);
      expect(signIn.pathname).toBe("/login");
      expect(signIn.searchParams.get("returnTo")).toBe(consentUrl);

      // The same browser can no longer see the decision form. page.request
      // shares this context's cookie jar; redirects are not followed so a
      // configured OAuth provider is never contacted.
      const consent = await page.request.get(consentUrl, { maxRedirects: 0 });
      expect(await consent.text()).not.toContain('action="/cli/consent"');
    });
  });

  test("signed out: the consent link does not show the consent form", async ({ request }) => {
    // The `request` fixture carries no session cookie. Redirects are followed
    // by hand: with OAuth configured, /login would hand off to the real
    // provider, which a test must not depend on.
    const key = generateDeviceKey();
    const authorize = await request.get(
      authorizeUrl({
        publicKey: key.publicKey,
        deviceName: uniqueDeviceName(),
        codeChallenge: generatePkce().challenge,
      }),
      { maxRedirects: 0 },
    );
    expect(authorize.status()).toBe(302);
    const consentPath = authorize.headers()["location"];
    expect(consentPath).toMatch(/^\/cli\/consent\?request=[A-Za-z0-9_-]{22}$/);

    const consent = await request.get(`${API_URL}${consentPath}`, { maxRedirects: 0 });
    const body = await consent.text();
    // meeting-api without OAUTH_CLIENT_ID answers 503 "Sign-in not
    // available"; with it, a redirect to /login that returns to this
    // consent request. Either way there is no decision form.
    if (consent.status() === 302) {
      const login = new URL(consent.headers()["location"], API_URL);
      expect(login.pathname).toBe("/login");
      expect(login.searchParams.get("returnTo")).toBe(`${API_URL}${consentPath}`);
    } else {
      expect(consent.status()).toBe(503);
      expect(body).toContain("Sign-in not available");
    }
    expect(body).not.toContain('action="/cli/consent"');
    expect(body).not.toContain(key.fingerprint);
  });
});

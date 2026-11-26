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
  uniqueDeviceName,
  uniqueEmail,
  type DeviceKey,
  type Pkce,
} from "../helpers/device-auth";

interface RemoteLogin {
  key: DeviceKey;
  pkce: Pkce;
  deviceName: string;
}

/** Open a remote-login authorize link and land on the consent page. */
async function openConsent(page: Page): Promise<RemoteLogin> {
  const login: RemoteLogin = {
    key: generateDeviceKey(),
    pkce: generatePkce(),
    deviceName: uniqueDeviceName(),
  };
  await page.goto(
    authorizeUrl({
      publicKey: login.key.publicKey,
      deviceName: login.deviceName,
      codeChallenge: login.pkce.challenge,
    }),
  );
  await expect(page).toHaveURL(/\/cli\/consent\?request=[A-Za-z0-9_-]{22}$/);
  await expect(page.getByRole("heading", { name: "Sign in videocall-cli?" })).toBeVisible();
  return login;
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
  login: RemoteLogin,
  code: string,
): Promise<string> {
  const response = await request.post(`${API_URL}/api/v1/devices`, {
    data: {
      code,
      code_verifier: login.pkce.verifier,
      redirect_uri: REMOTE_REDIRECT_URI,
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

/**
 * `videocall-cli auth login` in the browser: the CLI's link on meeting-api
 * (`/cli/authorize`) hands off to the app's consent page
 * (`/devices/authorize`), which approves through the JSON API. The device
 * list and Revoke live on the app's `/devices`.
 *
 * The session cookie is set on `localhost`, so the app on :3001 sends it to
 * meeting-api on :8081 (cookies ignore the port).
 */
import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import {
  API_URL,
  REMOTE_REDIRECT_URI,
  UI_URL,
  authorizeQuery,
  deviceJwt,
  generateDeviceKey,
  generatePkce,
  generateState,
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
}

function newLogin(redirectUri: string, deviceName = uniqueDeviceName()): CliLogin {
  return {
    key: generateDeviceKey(),
    pkce: generatePkce(),
    deviceName,
    state: generateState(),
    redirectUri,
  };
}

function queryOf(login: CliLogin): string {
  return authorizeQuery({
    publicKey: login.key.publicKey,
    deviceName: login.deviceName,
    codeChallenge: login.pkce.challenge,
    state: login.state,
    redirectUri: login.redirectUri,
  });
}

/**
 * The CLI's raw query carried by an app URL: `/devices/authorize?request=`
 * holds it as base64url (no padding), so it reaches the app byte for byte.
 */
function carriedQuery(appUrl: string): string {
  const url = new URL(appUrl);
  expect(url.origin + url.pathname).toBe(`${UI_URL}/devices/authorize`);
  const request = url.searchParams.get("request");
  expect(request).toMatch(/^[A-Za-z0-9_-]+$/);
  return Buffer.from(request!, "base64url").toString("utf8");
}

/** Open the link the CLI prints; meeting-api 302s it to the app. */
async function openAuthorizeLink(page: Page, query: string) {
  const cliUrl = `${API_URL}/cli/authorize?${query}`;
  const response = await page.goto(cliUrl);
  expect(response?.request().redirectedFrom()?.url()).toBe(cliUrl);
  expect(carriedQuery(response!.url())).toBe(query);
}

/** Open the link and wait until the consent page shows the device. */
async function openConsent(page: Page, login: CliLogin) {
  await openAuthorizeLink(page, queryOf(login));
  await expect(page.getByTestId("device-authorize-name")).toHaveText(login.deviceName);
  await expect(page.getByTestId("fingerprint")).toHaveText(login.key.fingerprint);
}

/** Remote approve: tick the checkbox, approve, return the code shown. */
async function approveRemote(page: Page): Promise<string> {
  await page.getByLabel("I started this sign-in myself, on a device I own.").check();
  await page.getByRole("button", { name: "Approve", exact: true }).click();
  await expect(page.getByTestId("device-authorize-code")).toBeVisible();
  const code = await page.getByLabel("Code", { exact: true }).inputValue();
  expect(code).toMatch(/^[A-Za-z0-9_-]{43}$/);
  return code;
}

/** Exchange the code at POST /api/v1/devices, as the CLI does. */
async function registerDevice(request: APIRequestContext, login: CliLogin, code: string) {
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

function deviceRow(page: Page, deviceName: string) {
  return page.getByTestId("device-row").filter({ hasText: deviceName });
}

/** The browser shows the loopback listener's page. */
async function expectListenerReached(page: Page) {
  await expect(page.getByRole("heading", { name: "CLI listener reached" })).toBeVisible();
}

test.describe("CLI device login", () => {
  test.describe("signed in", () => {
    test.beforeEach(async ({ context }) => {
      // A fresh user per test keeps device lists independent.
      await injectSessionCookie(context, {
        baseURL: UI_URL,
        email: uniqueEmail(),
        name: "E2E Device User",
      });
    });

    // "C++ rig" is the regression case: a `+` in the name must survive the
    // hand-off, a reload and the exchange (it once decoded to a space).
    for (const fixedName of [undefined, "C++ rig"]) {
      test(`remote (${fixedName ?? "plain name"}): approve shows a code once and it registers the device`, async ({
        page,
        request,
      }) => {
        const deviceName = fixedName ?? uniqueDeviceName();
        const login = newLogin(REMOTE_REDIRECT_URI, deviceName);
        await openConsent(page, login);
        await page.reload();
        await expect(page.getByTestId("device-authorize-name")).toHaveText(deviceName);
        await expect(page.getByTestId("device-authorize-account")).toContainText(
          "Signed in as E2E Device User",
        );

        const approve = page.getByRole("button", { name: "Approve", exact: true });
        await expect(approve).toBeDisabled();
        await page.getByLabel("I started this sign-in myself, on a device I own.").check();
        await expect(approve).toBeEnabled();

        const code = await approveRemote(page);
        expect(page.url()).not.toContain(code);
        const stored = await page.evaluate(() =>
          JSON.stringify([{ ...localStorage }, { ...sessionStorage }]),
        );
        expect(stored).not.toContain(code);

        await registerDevice(request, login, code);
        await page.goto(`${UI_URL}/devices`);
        const row = deviceRow(page, login.key.fingerprint);
        await expect(row).toContainText(deviceName);
        await expect(page.getByRole("button", { name: `Revoke ${deviceName}` })).toHaveCount(1);
      });
    }

    test("loopback approve: the browser reaches the CLI listener with code and state", async ({
      page,
      request,
    }) => {
      const listener = await startLoopbackListener();
      try {
        const login = newLogin(listener.redirectUri);
        await openConsent(page, login);
        await expect(page.getByRole("checkbox")).toHaveCount(0);

        await page.getByRole("button", { name: "Approve", exact: true }).click();
        await expectListenerReached(page);

        const hit = await listener.firstHit;
        const callback = new URL(hit.url, listener.redirectUri);
        expect(callback.pathname).toBe("/callback");
        expect(callback.searchParams.get("state")).toBe(login.state);
        const code = callback.searchParams.get("code");
        expect(code).toMatch(/^[A-Za-z0-9_-]{43}$/);
        // The consent page URL carries the state and key; at most the app
        // origin may leak to the listener.
        expect([undefined, `${UI_URL}/`]).toContain(hit.headers.referer);

        await registerDevice(request, login, code!);
        await page.goto(`${UI_URL}/devices`);
        await expect(deviceRow(page, login.deviceName)).toHaveCount(1);
      } finally {
        await listener.close();
      }
    });

    test("loopback deny: the browser reaches the CLI listener with access_denied", async ({
      page,
    }) => {
      const listener = await startLoopbackListener();
      try {
        const login = newLogin(listener.redirectUri);
        await openConsent(page, login);
        await page.getByRole("button", { name: "Deny", exact: true }).click();
        await expectListenerReached(page);

        const callback = new URL((await listener.firstHit).url, listener.redirectUri);
        expect(callback.pathname).toBe("/callback");
        expect(callback.searchParams.get("error")).toBe("access_denied");
        expect(callback.searchParams.get("state")).toBe(login.state);
        expect(callback.searchParams.get("code")).toBeNull();

        await page.goto(`${UI_URL}/devices`);
        await expect(page.getByTestId("devices-empty")).toContainText("No devices are signed in.");
      } finally {
        await listener.close();
      }
    });

    test("revoke: the device leaves /devices and its key gets 401", async ({ page, request }) => {
      const login = newLogin(REMOTE_REDIRECT_URI);
      await openConsent(page, login);
      const deviceId = await registerDevice(request, login, await approveRemote(page));
      // The device JWT works first, so the 401 below comes from the revoke.
      expect((await devicesMe(request, login.key, deviceId)).status()).toBe(200);

      await page.goto(`${UI_URL}/devices`);
      await page.getByRole("button", { name: `Revoke ${login.deviceName}` }).click();
      await page
        .getByRole("alertdialog")
        .getByRole("button", { name: "Revoke", exact: true })
        .click();

      await expect(deviceRow(page, login.deviceName)).toHaveCount(0);
      await expect(page.getByTestId("devices-empty")).toContainText("No devices are signed in.");
      expect((await devicesMe(request, login.key, deviceId)).status()).toBe(401);
    });

    test("invalid link: a bad redirect_uri shows the static error and goes nowhere", async ({
      page,
    }) => {
      const login = newLogin("https://attacker.example/callback");
      const navigations: string[] = [];
      page.on("request", (r) => {
        if (r.isNavigationRequest() && r.frame() === page.mainFrame()) navigations.push(r.url());
      });

      await openAuthorizeLink(page, queryOf(login));
      await expect(page.getByTestId("device-authorize-invalid")).toBeVisible();
      await expect(page.getByRole("button", { name: "Approve", exact: true })).toHaveCount(0);
      expect(new URL(page.url()).pathname).toBe("/devices/authorize");
      // Only the link itself and its hand-off to the app were navigated.
      expect(navigations.map((url) => new URL(url).origin + new URL(url).pathname)).toEqual([
        `${API_URL}/cli/authorize`,
        `${UI_URL}/devices/authorize`,
      ]);
    });

    test("375 px: no horizontal scroll, one-line buttons, Approve not focused", async ({
      page,
    }) => {
      await page.setViewportSize({ width: 375, height: 812 });
      await openConsent(page, newLogin(REMOTE_REDIRECT_URI));

      const approve = page.getByRole("button", { name: "Approve", exact: true });
      await expect(approve).not.toBeFocused();
      const overflow = await page.evaluate(
        () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
      );
      expect(overflow).toBeLessThanOrEqual(0);
      for (const name of ["Deny", "Approve"]) {
        const button = page.getByRole("button", { name, exact: true });
        const lines = await button.evaluate((el) => {
          const range = document.createRange();
          range.selectNodeContents(el);
          return new Set([...range.getClientRects()].map((rect) => Math.round(rect.top))).size;
        });
        expect(lines, `${name} label lines`).toBe(1);
        const box = (await button.boundingBox())!;
        expect(box.x).toBeGreaterThanOrEqual(0);
        expect(box.x + box.width).toBeLessThanOrEqual(375);
      }
    });
  });

  test("signed out: no consent is shown and the app goes to login", async ({ page }) => {
    const login = newLogin(REMOTE_REDIRECT_URI);
    await openAuthorizeLink(page, queryOf(login));

    await expect(page).toHaveURL(`${UI_URL}/login`);
    await expect(page.getByRole("button", { name: "Approve", exact: true })).toHaveCount(0);
    await expect(page.getByText(login.key.fingerprint)).toHaveCount(0);
    // The login hands this value to meeting-api as returnTo.
    const returnTo = await page.evaluate(() => sessionStorage.getItem("vc_oauth_return_to"));
    expect(carriedQuery(returnTo!)).toBe(queryOf(login));
  });
});

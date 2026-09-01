import { test, expect, chromium, Browser, BrowserContext, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import {
  BROWSER_ARGS,
  DEFAULT_WEBSOCKET_TRANSPORT_INIT_SCRIPT,
  createAuthenticatedContext,
} from "../helpers/auth-context";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";

// Issue #2331: ChatSidebar mounts for every in-meeting participant, and its
// mount effect ran check_session(), which swept the guest's own identity.

const COOKIE_NAME = process.env.COOKIE_NAME || "session";
const API_URL = process.env.API_BASE_URL || "http://localhost:8081";

const LEGACY_GUEST_KEY = "vc_guest_session_id";
const LEGACY_MARKER_VALUE = "pre-2331-marker";
const DECOY_VALUE = "guest:00000000-0000-4000-8000-000000000000";
const idKey = (meetingId: string) => `vc_guest_id:${meetingId}`;
const tokenKey = (meetingId: string) => `vc_guest_token:${meetingId}`;
const GUEST_ID_SHAPE = /^guest:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

async function createMeetingViaApi(
  hostEmail: string,
  hostName: string,
  meetingId: string,
  opts: { allowGuests: boolean; waitingRoomEnabled: boolean },
): Promise<void> {
  const token = generateSessionToken(hostEmail, hostName);
  const res = await fetch(`${API_URL}/api/v1/meetings`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Cookie: `${COOKIE_NAME}=${token}`,
    },
    body: JSON.stringify({
      meeting_id: meetingId,
      attendees: [],
      allow_guests: opts.allowGuests,
      waiting_room_enabled: opts.waitingRoomEnabled,
    }),
  });
  if (!res.ok) {
    throw new Error(`POST /api/v1/meetings failed (${res.status}): ${await res.text()}`);
  }
}

async function fetchWaitingUserIds(
  hostEmail: string,
  hostName: string,
  meetingId: string,
): Promise<string[]> {
  const token = generateSessionToken(hostEmail, hostName);
  const res = await fetch(`${API_URL}/api/v1/meetings/${meetingId}/waiting`, {
    method: "GET",
    headers: { Cookie: `${COOKIE_NAME}=${token}` },
  });
  if (!res.ok) {
    throw new Error(`GET /waiting failed (${res.status}): ${await res.text()}`);
  }
  const json = (await res.json()) as {
    result: { waiting: Array<{ user_id: string }> };
  };
  return json.result.waiting.map((p) => p.user_id);
}

async function hostStartsMeeting(
  browser: Browser,
  hostEmail: string,
  hostName: string,
  meetingId: string,
  uiURL: string,
): Promise<Page> {
  const hostContext = await createAuthenticatedContext(browser, hostEmail, hostName, uiURL);
  const hostPage = await hostContext.newPage();

  await hostPage.goto("/");
  await hostPage.waitForTimeout(1500);

  await hostPage.locator("#meeting-id").click();
  await hostPage.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await hostPage.locator("#username").click();
  await hostPage.locator("#username").fill("");
  await hostPage.locator("#username").pressSequentially(hostName, { delay: 50 });
  await hostPage.waitForTimeout(500);
  await hostPage.locator("#username").press("Enter");
  await expect(hostPage).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await hostPage.waitForTimeout(1500);

  const joinButton = hostPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  await joinButton.waitFor({ timeout: 20_000 });
  await hostPage.waitForTimeout(1000);
  await joinButton.click();
  await hostPage.waitForTimeout(3000);
  await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

  return hostPage;
}

async function admitWaitingParticipant(hostPage: Page, displayName: string): Promise<void> {
  const participantRow = hostPage.locator(".waiting-participant").filter({ hasText: displayName });
  const admitButton = participantRow.getByTitle("Admit");

  await expect(participantRow).toBeVisible({ timeout: 20_000 });
  await expect(admitButton).toBeVisible({ timeout: 20_000 });
  await hostPage.waitForTimeout(500);
  await admitButton.dispatchEvent("click");
  await expect(participantRow).not.toBeVisible({ timeout: 10_000 });
  await hostPage.waitForTimeout(1000);
}

// config.local.js is evalled after config.js so it wins; a dev serve answers a
// missing path with index.html, which the loader's first-byte sniff discards.
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
    const shim = original.trimStart().startsWith("<") ? "" : original;
    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: shim + injection,
    });
  });
}

function readStorage(page: Page, key: string): Promise<string | null> {
  return page.evaluate((k) => window.sessionStorage.getItem(k), key);
}

async function requireStorage(page: Page, key: string): Promise<string> {
  const value = await readStorage(page, key);
  if (value === null || value === "") {
    throw new Error(`sessionStorage["${key}"] is absent or empty`);
  }
  return value;
}

async function submitGuestName(page: Page, displayName: string): Promise<void> {
  await page.locator("#guest-name").click();
  await page.locator("#guest-name").fill("");
  await page.locator("#guest-name").pressSequentially(displayName, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#guest-name").press("Enter");
}

async function waitForGrid(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Join Meeting|Start Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await waitForVisibleState(
    [
      { name: "join-button", locator: joinButton },
      { name: "grid", locator: grid },
    ] as const,
    25_000,
  );

  if (result === "join-button") {
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

test.describe("Guest per-meeting identity persistence", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("an admitted guest keeps its per-meeting identity across the chat sidebar mount and rejoins as the same guest", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_identity_${Date.now()}`;
    const hostEmail = "host-guest-identity@videocall.rs";
    const hostName = "HostGuestIdentity";
    const guestName = "GuestIdentityKeeper";

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: true,
        waitingRoomEnabled: true,
      });
      const hostPage = await hostStartsMeeting(hostBrowser, hostEmail, hostName, meetingId, uiURL);

      const guestContext = await guestBrowser.newContext({
        baseURL: uiURL,
        ignoreHTTPSErrors: true,
      });
      await guestContext.addInitScript(DEFAULT_WEBSOCKET_TRANSPORT_INIT_SCRIPT);
      // A second meeting's id, which nothing in this session can rewrite: only
      // the prefix sweep the fix removed can make it disappear.
      const decoyKey = idKey(`${meetingId}_other`);
      await guestContext.addInitScript(
        (seed: Array<[string, string]>) =>
          seed.forEach(([key, value]) => window.sessionStorage.setItem(key, value)),
        [
          [LEGACY_GUEST_KEY, LEGACY_MARKER_VALUE],
          [decoyKey, DECOY_VALUE],
        ] as Array<[string, string]>,
      );
      await enableOAuthConfig(guestContext);

      const guestPage = await guestContext.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      // OAuth off — the e2e default — leaves check_session unreachable.
      const oauthFlag = await guestPage.evaluate(
        () =>
          (window as unknown as { __APP_CONFIG?: Record<string, string> }).__APP_CONFIG
            ?.oauthEnabled,
      );
      expect(oauthFlag).toBe("true");

      await submitGuestName(guestPage, guestName);
      await expect(guestPage.getByText("Waiting to be admitted")).toBeVisible({
        timeout: 20_000,
      });

      // The waiting room mounts WaitingRoom, not AttendantsComponent: pre-sweep.
      await expect
        .poll(async () => (await readStorage(guestPage, idKey(meetingId))) ?? "", {
          timeout: 15_000,
        })
        .toMatch(GUEST_ID_SHAPE);
      const guestId = await requireStorage(guestPage, idKey(meetingId));
      await requireStorage(guestPage, tokenKey(meetingId));
      expect(await readStorage(guestPage, LEGACY_GUEST_KEY)).toBe(LEGACY_MARKER_VALUE);

      await admitWaitingParticipant(hostPage, guestName);
      await waitForGrid(guestPage);

      // Both sides of the fix evict this key, so its absence proves the call ran.
      await expect
        .poll(() => readStorage(guestPage, LEGACY_GUEST_KEY), { timeout: 20_000 })
        .toBe(null);

      expect(await readStorage(guestPage, idKey(meetingId))).toBe(guestId);
      expect(await readStorage(guestPage, tokenKey(meetingId))).toBeTruthy();
      expect(await readStorage(guestPage, decoyKey)).toBe(DECOY_VALUE);

      // join_attendee resets an existing row to 'waiting' — same row, same id.
      await guestPage.reload();
      await guestPage.waitForTimeout(1500);
      await submitGuestName(guestPage, guestName);
      await expect(guestPage.getByText("Waiting to be admitted")).toBeVisible({
        timeout: 20_000,
      });

      expect(await readStorage(guestPage, idKey(meetingId))).toBe(guestId);
      await expect
        .poll(() => fetchWaitingUserIds(hostEmail, hostName, meetingId), { timeout: 20_000 })
        .toEqual([guestId]);
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });
});

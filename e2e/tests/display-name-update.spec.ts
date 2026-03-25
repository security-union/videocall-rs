import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";
const API_URL = process.env.API_BASE_URL || "http://localhost:8081";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
];

async function createAuthenticatedContext(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  email: string,
  name: string,
  uiURL: string,
) {
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

async function navigateToMeeting(page: Page, meetingId: string, username: string) {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
    timeout: 10_000,
  });
  await page.waitForTimeout(1500);
}

async function joinMeetingFromPage(
  page: Page,
): Promise<"in-meeting" | "waiting" | "waiting-for-meeting"> {
  const joinButton = page.getByText(/Start Meeting|Join Meeting/);
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 20_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 20_000 }).then(() => "waiting-for-meeting" as const),
  ]);

  if (result === "waiting") {
    return "waiting";
  }

  if (result === "waiting-for-meeting") {
    return "waiting-for-meeting";
  }

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);

  await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

async function admitGuestIfNeeded(
  hostPage: Page,
  guestPage: Page,
  guestResult: "in-meeting" | "waiting" | "waiting-for-meeting",
): Promise<void> {
  if (guestResult === "in-meeting") {
    return;
  }

  if (guestResult === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);

    const guestJoinButton = guestPage.getByText(/Join Meeting|Start Meeting/);
    const guestGrid = guestPage.locator("#grid-container");

    const postAdmit = await Promise.race([
      guestJoinButton.waitFor({ timeout: 20_000 }).then(() => "join-button" as const),
      guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);

    if (postAdmit === "join-button") {
      await guestPage.waitForTimeout(1000);
      await guestJoinButton.click();
      await guestPage.waitForTimeout(3000);
      await expect(guestGrid).toBeVisible({ timeout: 15_000 });
    }
  }
}

/**
 * Call the meeting API's PUT /display-name endpoint directly.
 * Uses a fresh JWT for the given user so the request is authenticated.
 */
async function updateDisplayNameViaApi(
  email: string,
  name: string,
  meetingId: string,
  newDisplayName: string,
): Promise<void> {
  const token = generateSessionToken(email, name);
  const url = `${API_URL}/api/v1/meetings/${meetingId}/display-name`;
  const res = await fetch(url, {
    method: "PUT",
    headers: {
      "Content-Type": "application/json",
      Cookie: `${COOKIE_NAME}=${token}`,
    },
    body: JSON.stringify({ display_name: newDisplayName }),
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`PUT display-name failed (${res.status}): ${body}`);
  }
}

test.describe("Display name live update", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Core flow: guest changes display name via the API while in a meeting,
   * the host sees the updated name on the peer tile in real-time — no page
   * refresh or rejoin required. The PARTICIPANT_DISPLAY_NAME_CHANGED event
   * propagates through NATS → WebSocket → client → UI.
   *
   * Currently restricted to the Dioxus UI because the Yew UI does not yet
   * wire up the on_display_name_changed callback needed to trigger a
   * re-render when a peer's display name changes.
   */
  test("name change propagates to other participant without reconnect", async ({ baseURL }) => {
    // The Yew UI does not yet handle PARTICIPANT_DISPLAY_NAME_CHANGED for
    // re-rendering peer tiles, so skip this test for the yew project.
    test.skip(
      baseURL === "http://localhost:80" || baseURL === "http://localhost",
      "Yew UI does not yet support live display name updates",
    );

    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_dn_update_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-dn@videocall.rs",
        "HostUser",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-dn@videocall.rs",
        "OriginalGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Collect any "Connection lost" console messages on both pages —
      // these would indicate an unwanted reconnect cycle.
      const hostReconnectLogs: string[] = [];
      const guestReconnectLogs: string[] = [];
      hostPage.on("console", (msg) => {
        const text = msg.text();
        if (text.includes("Connection lost")) {
          hostReconnectLogs.push(text);
        }
      });
      guestPage.on("console", (msg) => {
        const text = msg.text();
        if (text.includes("Connection lost")) {
          guestReconnectLogs.push(text);
        }
      });

      // ---- Host joins the meeting ----
      await navigateToMeeting(hostPage, meetingId, "HostUser");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // ---- Guest joins the meeting ----
      await navigateToMeeting(guestPage, meetingId, "OriginalGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Wait for peer discovery on both sides
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      const hostPeerTile = hostPage.locator("#grid-container .canvas-container");
      await expect(hostPeerTile.first()).toBeVisible({ timeout: 30_000 });

      // ---- Verify initial display name ----
      const originalNameOnHost = hostPage.locator(".floating-name", {
        hasText: "OriginalGuest",
      });
      await expect(originalNameOnHost.first()).toBeVisible({ timeout: 10_000 });

      // Let join toasts clear so they don't interfere with assertions
      await hostPage.waitForTimeout(9000);

      // ---- Guest updates display name via API ----
      await updateDisplayNameViaApi(
        "guest-dn@videocall.rs",
        "OriginalGuest",
        meetingId,
        "RenamedGuest",
      );

      // ---- Host sees the updated name in real-time (no page refresh) ----
      const renamedOnHost = hostPage.locator(".floating-name", {
        hasText: "RenamedGuest",
      });
      await expect(renamedOnHost.first()).toBeVisible({ timeout: 15_000 });

      // The old name should no longer appear on any peer tile
      await expect(originalNameOnHost).toHaveCount(0, { timeout: 5_000 });

      // ---- No reconnect / connection-lost side effects ----
      // 1) No "Connection lost" messages in browser console
      expect(hostReconnectLogs).toHaveLength(0);
      expect(guestReconnectLogs).toHaveLength(0);

      // 2) No visible connection error banner on either page
      await expect(hostPage.getByText("Connection lost")).toHaveCount(0);
      await expect(guestPage.getByText("Connection lost")).toHaveCount(0);
      await expect(hostPage.getByText("reconnecting")).toHaveCount(0);
      await expect(guestPage.getByText("reconnecting")).toHaveCount(0);

      // 3) Both grids are still visible (nobody got kicked out)
      await expect(hostPage.locator("#grid-container")).toBeVisible();
      await expect(guestPage.locator("#grid-container")).toBeVisible();
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Verify that the guest's own display name signal is updated when the
   * server confirms the change (via PARTICIPANT_DISPLAY_NAME_CHANGED).
   * The guest's local tile should reflect the new name too.
   */
  test("guest sees own display name update confirmed", async ({ baseURL }) => {
    test.skip(
      baseURL === "http://localhost:80" || baseURL === "http://localhost",
      "Yew UI does not yet support live display name updates",
    );

    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_dn_self_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-self@videocall.rs",
        "SelfHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-self@videocall.rs",
        "BeforeName",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host joins
      await navigateToMeeting(hostPage, meetingId, "SelfHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // Guest joins
      await navigateToMeeting(guestPage, meetingId, "BeforeName");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Wait for peer discovery
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });
      await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // ---- Guest updates own display name ----
      await updateDisplayNameViaApi(
        "guest-self@videocall.rs",
        "BeforeName",
        meetingId,
        "AfterName",
      );

      // Guest's own tile (self-view) should show the updated name.
      // The server echoes PARTICIPANT_DISPLAY_NAME_CHANGED to ALL
      // participants in the meeting, including the sender.
      const guestSelfName = guestPage.locator(".floating-name", {
        hasText: "AfterName",
      });
      await expect(guestSelfName.first()).toBeVisible({ timeout: 15_000 });

      // Host also sees the new name
      const guestNameOnHost = hostPage.locator(".floating-name", {
        hasText: "AfterName",
      });
      await expect(guestNameOnHost.first()).toBeVisible({ timeout: 15_000 });

      // Both still in meeting — no disruption
      await expect(hostPage.locator("#grid-container")).toBeVisible();
      await expect(guestPage.locator("#grid-container")).toBeVisible();
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Verify that after a display name change is confirmed by the server,
   * navigating back to the home page shows the updated name pre-filled in
   * the username input. This tests the localStorage persistence path:
   *   on_display_name_changed → save_display_name_to_storage → reload → load_display_name_from_storage
   */
  test("updated display name persists after navigating to home", async ({ baseURL }) => {
    test.skip(
      baseURL === "http://localhost:80" || baseURL === "http://localhost",
      "Yew UI does not yet support live display name updates",
    );

    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_dn_persist_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-persist@videocall.rs",
        "PersistHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-persist@videocall.rs",
        "OldName",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host joins
      await navigateToMeeting(hostPage, meetingId, "PersistHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // Guest joins
      await navigateToMeeting(guestPage, meetingId, "OldName");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Wait for peer discovery
      await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // Guest changes display name via API
      await updateDisplayNameViaApi(
        "guest-persist@videocall.rs",
        "OldName",
        meetingId,
        "NewPersisted",
      );

      // Wait for the server confirmation to propagate — guest sees own new name
      const guestSelfName = guestPage.locator(".floating-name", {
        hasText: "NewPersisted",
      });
      await expect(guestSelfName.first()).toBeVisible({ timeout: 15_000 });

      // Navigate guest back to home page
      await guestPage.goto("/");
      await guestPage.waitForTimeout(2000);

      // The username input should be pre-filled with the persisted new name
      const usernameInput = guestPage.locator("#username");
      await expect(usernameInput).toBeVisible({ timeout: 10_000 });
      await expect(usernameInput).toHaveValue("NewPersisted", { timeout: 5_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});

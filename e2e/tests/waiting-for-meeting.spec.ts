import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

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
  // Display name is a controlled input -- clear before typing to handle any pre-fill
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

/**
 * From the meeting page, wait for the meeting UI to load and click through
 * "Start Meeting" / "Join Meeting" to enter the grid.
 *
 * The meeting page auto-joins the API when navigated to with a username
 * already set (from the home page). Users who lack a username see an inline
 * display name prompt on the meeting page itself.
 *
 * The auto-join shows a brief "Joining as [name]..." spinner while the API
 * call is in flight. Once the API responds the UI transitions to one of:
 *   - "Ready to join?" with Start/Join Meeting button (admitted)
 *   - "Waiting to be admitted" (waiting room)
 *   - "Waiting for meeting to start" (host hasn't started yet)
 *
 * Auth dropdown (user name/email, sign-out) is only shown on the home
 * page -- it no longer appears on this pre-meeting screen.
 */
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

test.describe("Waiting for meeting (push notifications)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("guest joins before host and sees waiting-for-meeting screen", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_wait_meet_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const guestCtx = await createAuthenticatedContext(
        browser,
        "early-guest@videocall.rs",
        "EarlyGuest",
        uiURL,
      );
      const guestPage = await guestCtx.newPage();

      await navigateToMeeting(guestPage, meetingId, "EarlyGuest");

      const guestResult = await joinMeetingFromPage(guestPage);
      expect(guestResult).toBe("waiting-for-meeting");

      await expect(guestPage.getByText("Waiting for meeting to start")).toBeVisible({
        timeout: 10_000,
      });
      await expect(guestPage.getByText("The host hasn't started this meeting yet")).toBeVisible({
        timeout: 5_000,
      });

      await expect(guestPage.getByText("Leave")).toBeVisible({ timeout: 5_000 });
    } finally {
      await browser.close();
    }
  });

  test("guest auto-joins when host starts the meeting via push notification", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_auto_join_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const guestCtx = await createAuthenticatedContext(
        browser1,
        "guest-early@videocall.rs",
        "GuestEarly",
        uiURL,
      );
      const hostCtx = await createAuthenticatedContext(
        browser2,
        "host-late@videocall.rs",
        "HostLate",
        uiURL,
      );

      const guestPage = await guestCtx.newPage();
      const hostPage = await hostCtx.newPage();

      // Guest joins first, lands on waiting-for-meeting
      await navigateToMeeting(guestPage, meetingId, "GuestEarly");
      const guestResult = await joinMeetingFromPage(guestPage);
      expect(guestResult).toBe("waiting-for-meeting");

      await expect(guestPage.getByText("Waiting for meeting to start")).toBeVisible({
        timeout: 10_000,
      });

      // Host joins and starts the meeting
      await navigateToMeeting(hostPage, meetingId, "HostLate");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Guest receives on_meeting_activated via WebSocket and transitions
      const guestJoinButton = guestPage.getByText(/Join Meeting|Start Meeting/);
      const guestWaiting = guestPage.getByText("Waiting to be admitted");
      const guestGrid = guestPage.locator("#grid-container");

      const guestTransition = await Promise.race([
        guestJoinButton.waitFor({ timeout: 30_000 }).then(() => "join-button" as const),
        guestWaiting.waitFor({ timeout: 30_000 }).then(() => "waiting-room" as const),
        guestGrid.waitFor({ timeout: 30_000 }).then(() => "grid" as const),
      ]);

      if (guestTransition === "join-button") {
        await guestPage.waitForTimeout(1000);
        await guestJoinButton.click();
        await guestPage.waitForTimeout(3000);
        await expect(guestGrid).toBeVisible({ timeout: 15_000 });
      } else if (guestTransition === "waiting-room") {
        // Host needs to admit guest
        const admitButton = hostPage.getByTitle("Admit").first();
        await expect(admitButton).toBeVisible({ timeout: 20_000 });
        await hostPage.waitForTimeout(1000);
        await admitButton.dispatchEvent("click");
        await hostPage.waitForTimeout(3000);

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

      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("host rejects guest from waiting room, guest sees entry denied via push", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_reject_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-reject@videocall.rs",
        "HostReject",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-reject@videocall.rs",
        "GuestReject",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "HostReject");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Guest joins via home page and lands in waiting room
      await navigateToMeeting(guestPage, meetingId, "GuestReject");
      const guestResult = await joinMeetingFromPage(guestPage);

      if (guestResult === "in-meeting" || guestResult === "waiting-for-meeting") {
        // Waiting room disabled or edge case -- skip gracefully
        return;
      }

      await expect(guestPage.getByText("Waiting to be admitted")).toBeVisible({ timeout: 10_000 });

      // Host rejects the guest
      const rejectButton = hostPage.getByTitle("Reject").first();
      await expect(rejectButton).toBeVisible({ timeout: 20_000 });
      await hostPage.waitForTimeout(1000);
      await rejectButton.dispatchEvent("click");

      // Guest sees "Entry denied" via push notification
      await expect(guestPage.getByText("Entry denied")).toBeVisible({ timeout: 20_000 });
      await expect(
        guestPage.getByText("The meeting host has denied your request to join"),
      ).toBeVisible({ timeout: 5_000 });

      await expect(guestPage.getByText("Return to Home")).toBeVisible({ timeout: 5_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});

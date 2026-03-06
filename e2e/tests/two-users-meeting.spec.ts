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

/**
 * From the meeting page, wait for the meeting UI to load and click through
 * "Start Meeting" / "Join Meeting" to enter the grid.
 *
 * The meeting page auto-joins the API when navigated to with a username
 * already set (from the home page). There is no display name prompt on the
 * meeting page -- users who lack a username are redirected back to "/".
 *
 * The auto-join shows a brief "Joining meeting..." spinner while the API
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
  // We could be on "Ready to join?", "Waiting to be admitted", or "Waiting for meeting"
  // Race between the possible states
  const joinButton = page.getByText(/Start Meeting|Join Meeting/);
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");

  // Wait for any of these to appear
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

  // Click Join/Start Meeting
  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);

  await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

test.describe("Two users in a meeting", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("host starts meeting, guest joins, both see each other", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_two_user_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host@videocall.rs",
        "HostUser",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest@videocall.rs",
        "GuestUser",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- HOST: go to home page, enter meeting ----
      await hostPage.goto("/");
      await hostPage.waitForTimeout(1500);

      await hostPage.locator("#meeting-id").click();
      await hostPage.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
      // Display name is a controlled input -- clear before typing to handle any pre-fill
      await hostPage.locator("#username").click();
      await hostPage.locator("#username").fill("");
      await hostPage.locator("#username").pressSequentially("HostUser", { delay: 50 });
      await hostPage.waitForTimeout(500);
      await hostPage.locator("#username").press("Enter");
      await expect(hostPage).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
        timeout: 10_000,
      });
      await hostPage.waitForTimeout(1500);

      // Host joins the meeting
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // ---- GUEST: go to home page, enter meeting ----
      await guestPage.goto("/");
      await guestPage.waitForTimeout(1500);

      await guestPage.locator("#meeting-id").click();
      await guestPage.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
      // Display name is a controlled input -- clear before typing to handle any pre-fill
      await guestPage.locator("#username").click();
      await guestPage.locator("#username").fill("");
      await guestPage.locator("#username").pressSequentially("GuestUser", { delay: 50 });
      await guestPage.waitForTimeout(500);
      await guestPage.locator("#username").press("Enter");
      await expect(guestPage).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
        timeout: 10_000,
      });
      await guestPage.waitForTimeout(1500);

      const guestResult = await joinMeetingFromPage(guestPage);

      if (guestResult === "waiting") {
        // Host needs to admit guest from the waiting room.
        // Wait for the admit button to appear (pushed via WebSocket/NATS notification)
        const admitButton = hostPage.getByTitle("Admit").first();
        await expect(admitButton).toBeVisible({ timeout: 20_000 });
        await hostPage.waitForTimeout(1000);
        await admitButton.dispatchEvent("click");
        await hostPage.waitForTimeout(3000);

        // After admission, guest may auto-join (grid appears directly) or
        // may see a "Join Meeting" button — handle both scenarios.
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
        // If "grid" won the race, guest already auto-joined — nothing to click.
      }

      // ---- ASSERT: both users are in the meeting ----
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Give time for WebSocket/WebTransport peer discovery
      await hostPage.waitForTimeout(5000);

      // Once a peer connects, the invite overlay ("Your meeting is ready!") disappears.
      // Verify each side sees at least one remote peer's canvas-container.
      const hostPeer = hostPage.locator("#grid-container .canvas-container");
      const guestPeer = guestPage.locator("#grid-container .canvas-container");

      await expect(hostPeer.first()).toBeVisible({ timeout: 30_000 });
      await expect(guestPeer.first()).toBeVisible({ timeout: 30_000 });

      // Pause so you can watch both browsers
      await hostPage.waitForTimeout(5000);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});

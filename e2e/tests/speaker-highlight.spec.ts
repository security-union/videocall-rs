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
  userId: string,
  name: string,
  uiURL: string,
) {
  const context = await browser.newContext({
    baseURL: uiURL,
    ignoreHTTPSErrors: true,
  });
  const token = generateSessionToken(userId, name);
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

test.describe("Speaker highlight glow on video tiles", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("video tile has transparent border and no glow when silent", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_peer_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "00000000-0000-4000-8000-000000000801",
        "GlowHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "00000000-0000-4000-8000-000000000802",
        "GlowGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "GlowHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // Guest joins the meeting
      await navigateToMeeting(guestPage, meetingId, "GlowGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Wait for peer tile to appear on the host side
      const peerTile = hostPage.locator("#grid-container .canvas-container");
      await expect(peerTile.first()).toBeVisible({ timeout: 30_000 });

      // The glow inline style lives on the child .glow-overlay div.
      const glowOverlay = peerTile.first().locator(".glow-overlay");
      await expect(glowOverlay).toBeVisible({ timeout: 10_000 });

      // When silent: border-color is transparent, box-shadow is none.
      const style = await glowOverlay.getAttribute("style");
      expect(style).toBeTruthy();
      expect(style).toContain("border-color: transparent");
      expect(style).toContain("box-shadow: none");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("video tile has transition property in inline style", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_trans_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "00000000-0000-4000-8000-000000000901",
        "TransHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "00000000-0000-4000-8000-000000000902",
        "TransGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "TransHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // Guest joins the meeting
      await navigateToMeeting(guestPage, meetingId, "TransGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Wait for peer tile to appear
      const peerTile = hostPage.locator("#grid-container .canvas-container");
      await expect(peerTile.first()).toBeVisible({ timeout: 30_000 });

      // The glow inline style lives on the child .glow-overlay div.
      const glowOverlay = peerTile.first().locator(".glow-overlay");
      await expect(glowOverlay).toBeVisible({ timeout: 10_000 });

      // The inline style should contain a transition property for the
      // smooth fade-in/fade-out of the glow border.
      const style = await glowOverlay.getAttribute("style");
      expect(style).toBeTruthy();
      expect(style).toContain("transition:");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("host tile has transparent border when silent", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_host_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "00000000-0000-4000-8000-000000001001",
        "SelfHost",
        uiURL,
      );

      const page = await ctx.newPage();

      // Navigate to a meeting and join
      await navigateToMeeting(page, meetingId, "SelfHost");
      const result = await joinMeetingFromPage(page);
      expect(result).toBe("in-meeting");

      // Wait for the host's own video wrapper to appear
      const hostWrapper = page.locator(".host-video-wrapper");
      await expect(hostWrapper.first()).toBeVisible({ timeout: 15_000 });

      // The glow inline style lives on the child .glow-overlay div.
      const glowOverlay = hostWrapper.first().locator(".glow-overlay");
      await expect(glowOverlay).toBeVisible({ timeout: 10_000 });

      // The host's own tile should also have silent-state inline styles.
      const style = await glowOverlay.getAttribute("style");
      expect(style).toBeTruthy();
      expect(style).toContain("border-color: transparent");
    } finally {
      await browser.close();
    }
  });

  test("mic icon does not have speaking class when silent", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_mic_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "00000000-0000-4000-8000-000000001101",
        "MicHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "00000000-0000-4000-8000-000000001102",
        "MicGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "MicHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // Guest joins the meeting
      await navigateToMeeting(guestPage, meetingId, "MicGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Wait for peer tile to appear
      const peerTile = hostPage.locator("#grid-container .canvas-container");
      await expect(peerTile.first()).toBeVisible({ timeout: 30_000 });

      // The audio indicator on the peer tile should NOT have the
      // "speaking" class when the participant is silent.
      const audioIndicator = hostPage.locator(".audio-indicator").first();
      await expect(audioIndicator).toBeVisible({ timeout: 10_000 });
      await expect(audioIndicator).not.toHaveClass(/speaking/);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});

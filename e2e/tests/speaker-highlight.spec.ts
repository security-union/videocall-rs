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
        "host-glow@videocall.rs",
        "GlowHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-glow@videocall.rs",
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

      // When silent: no border, box-shadow is none.
      const style = await glowOverlay.getAttribute("style");
      expect(style).toBeTruthy();
      expect(style).toContain("border: 1.5px solid transparent");
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
        "host-trans@videocall.rs",
        "TransHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-trans@videocall.rs",
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
        "host-self@videocall.rs",
        "SelfHost",
        uiURL,
      );

      const page = await ctx.newPage();

      // Navigate to a meeting and join
      await navigateToMeeting(page, meetingId, "SelfHost");
      const result = await joinMeetingFromPage(page);
      expect(result).toBe("in-meeting");

      // Wait for ANY visible .glow-overlay on the page. With fake devices
      // in E2E, video may be off so the glow-overlay can render outside
      // .host-video-wrapper.
      const glowOverlay = page.locator(".glow-overlay").first();
      await expect(glowOverlay).toBeVisible({ timeout: 15_000 });

      // The host's own tile should also have silent-state inline styles.
      const style = await glowOverlay.getAttribute("style");
      expect(style).toBeTruthy();
      expect(style).toContain("border: 1.5px solid transparent");
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
        "host-mic@videocall.rs",
        "MicHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-mic@videocall.rs",
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

  test("pinning a peer tile adds grid-item-pinned class", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_pin_class_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-pin@videocall.rs",
        "PinHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-pin@videocall.rs",
        "PinGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "PinHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // Guest joins the meeting
      await navigateToMeeting(guestPage, meetingId, "PinGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Wait for peer tile to appear on the host side
      const peerGridItem = hostPage.locator("#grid-container .grid-item").first();
      await expect(peerGridItem).toBeVisible({ timeout: 30_000 });

      // Before pinning: the grid-item should NOT have grid-item-pinned class
      await expect(peerGridItem).not.toHaveClass(/grid-item-pinned/);

      // Click the pin button on the peer tile
      const pinButton = peerGridItem.locator(".pin-icon");
      await expect(pinButton).toBeVisible({ timeout: 10_000 });
      await pinButton.click();
      await hostPage.waitForTimeout(500);

      // After pinning: the grid-item should have grid-item-pinned class
      await expect(peerGridItem).toHaveClass(/grid-item-pinned/, { timeout: 5_000 });

      // Click pin again to unpin
      await pinButton.click();
      await hostPage.waitForTimeout(500);

      // After unpinning: the grid-item should NOT have grid-item-pinned class
      await expect(peerGridItem).not.toHaveClass(/grid-item-pinned/, { timeout: 5_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("non-pinned peer tile glow is suppressed when another tile is pinned", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_pin_glow_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-pinglow@videocall.rs",
        "PinGlowHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-pinglow@videocall.rs",
        "PinGlowGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "PinGlowHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // Guest joins the meeting
      await navigateToMeeting(guestPage, meetingId, "PinGlowGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Wait for peer tile to appear
      const peerGridItem = hostPage.locator("#grid-container .grid-item").first();
      await expect(peerGridItem).toBeVisible({ timeout: 30_000 });

      // Pin the peer tile
      const pinButton = peerGridItem.locator(".pin-icon");
      await expect(pinButton).toBeVisible({ timeout: 10_000 });
      await pinButton.click();
      await hostPage.waitForTimeout(1000);

      // Verify the pinned tile has the class
      await expect(peerGridItem).toHaveClass(/grid-item-pinned/, { timeout: 5_000 });

      // The host's own tile glow overlay should have suppressed
      // indicator styles (transparent border, no box-shadow) while
      // another panel is pinned fullscreen.
      const hostWrapper = hostPage.locator(".host-video-wrapper");
      await expect(hostWrapper.first()).toBeVisible({ timeout: 15_000 });
      const hostGlow = hostWrapper.first().locator(".glow-overlay");
      await expect(hostGlow).toBeVisible({ timeout: 10_000 });

      const hostStyle = await hostGlow.getAttribute("style");
      expect(hostStyle).toBeTruthy();
      // Speaking indicators must be suppressed: no shadow.
      expect(hostStyle).toContain("box-shadow: none");

      // Unpin the peer tile
      await pinButton.click();
      await hostPage.waitForTimeout(500);
      await expect(peerGridItem).not.toHaveClass(/grid-item-pinned/, { timeout: 5_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("non-pinned peer tile mic indicator lacks speaking class when pinned", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_pin_mic_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-pinmic@videocall.rs",
        "PinMicHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-pinmic@videocall.rs",
        "PinMicGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "PinMicHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // Guest joins the meeting
      await navigateToMeeting(guestPage, meetingId, "PinMicGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Wait for peer tile to appear
      const peerGridItem = hostPage.locator("#grid-container .grid-item").first();
      await expect(peerGridItem).toBeVisible({ timeout: 30_000 });

      // Pin the peer tile
      const pinButton = peerGridItem.locator(".pin-icon");
      await expect(pinButton).toBeVisible({ timeout: 10_000 });
      await pinButton.click();
      await hostPage.waitForTimeout(1000);

      // Verify the pinned tile has the class
      await expect(peerGridItem).toHaveClass(/grid-item-pinned/, { timeout: 5_000 });

      // The peer tile's own glow overlay should still be visible (it is the pinned tile)
      const pinnedGlow = peerGridItem.locator(".glow-overlay").first();
      await expect(pinnedGlow).toBeVisible({ timeout: 10_000 });

      // The host tile's audio-indicator should NOT have the "speaking"
      // class — speaking indicators are suppressed on non-pinned tiles
      // when any panel is pinned fullscreen.
      // Check if the host-video-wrapper area has an audio indicator
      // (the host toolbar mic is separate from tile indicators).
      // Non-pinned peer tiles in the grid also must not show speaking.
      // Since all audio is fake-silent, this verifies the DOM state
      // is correctly suppressed (no "speaking" class anywhere).
      const allAudioIndicators = hostPage.locator(".audio-indicator");
      const count = await allAudioIndicators.count();
      for (let i = 0; i < count; i++) {
        await expect(allAudioIndicators.nth(i)).not.toHaveClass(/speaking/);
      }

      // Unpin
      await pinButton.click();
      await hostPage.waitForTimeout(500);
      await expect(peerGridItem).not.toHaveClass(/grid-item-pinned/, { timeout: 5_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("pinned tile stays pinned when an unrelated peer joins then leaves", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_pin_persist_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });
    const browser3 = await chromium.launch({ args: BROWSER_ARGS });

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
        "PersistGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "PersistHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // Guest joins
      await navigateToMeeting(guestPage, meetingId, "PersistGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Wait for guest peer tile on host side
      const peerGridItem = hostPage.locator("#grid-container .grid-item").first();
      await expect(peerGridItem).toBeVisible({ timeout: 30_000 });

      // Pin the guest's tile
      const pinButton = peerGridItem.locator(".pin-icon");
      await expect(pinButton).toBeVisible({ timeout: 10_000 });
      await pinButton.click();
      await hostPage.waitForTimeout(500);
      await expect(peerGridItem).toHaveClass(/grid-item-pinned/, { timeout: 5_000 });

      // A third user joins — this triggers peer_list_version bump & re-render
      const thirdCtx = await createAuthenticatedContext(
        browser3,
        "third-persist@videocall.rs",
        "PersistThird",
        uiURL,
      );
      const thirdPage = await thirdCtx.newPage();
      await navigateToMeeting(thirdPage, meetingId, "PersistThird");
      const thirdResult = await joinMeetingFromPage(thirdPage);
      await admitGuestIfNeeded(hostPage, thirdPage, thirdResult);

      // Wait for the third user's tile to appear on host side (proves re-render happened)
      await expect(hostPage.locator("#grid-container .grid-item").nth(1)).toBeVisible({
        timeout: 30_000,
      });

      // The originally-pinned tile must STILL have grid-item-pinned
      await expect(peerGridItem).toHaveClass(/grid-item-pinned/, { timeout: 5_000 });

      // Third user leaves — another re-render
      await thirdPage.close();
      await hostPage.waitForTimeout(5_000);

      // Pin must STILL be present after the third peer departs
      await expect(peerGridItem).toHaveClass(/grid-item-pinned/, { timeout: 5_000 });

      // Finally unpin manually to confirm toggle still works
      await pinButton.click();
      await hostPage.waitForTimeout(500);
      await expect(peerGridItem).not.toHaveClass(/grid-item-pinned/, { timeout: 5_000 });
    } finally {
      await browser1.close();
      await browser2.close();
      await browser3.close();
    }
  });
});

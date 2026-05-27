import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Screen-share visibility toast E2E (HCL issue 893).
 *
 * Verifies the toast state machine that confirms a publisher's shared
 * content is actually being decoded by at least one other peer:
 *
 *   Idle → click share → "Starting to share content..."
 *     → peer decodes first frame → "Others can now see your shared content"
 *     → autodismiss
 *
 *   no peer decodes within ~10s → error toast
 */

/**
 * Inject a mock `getDisplayMedia` that returns a synthetic MediaStream
 * from a canvas. Mirrors the helper in `screen-share-state.spec.ts`.
 */
const MOCK_GET_DISPLAY_MEDIA_SCRIPT = `
  (() => {
    navigator.mediaDevices.getDisplayMedia = function(_constraints) {
      return new Promise((resolve) => {
        const canvas = document.createElement('canvas');
        canvas.width = 640;
        canvas.height = 480;
        const ctx = canvas.getContext('2d');
        ctx.fillStyle = '#2a2a2a';
        ctx.fillRect(0, 0, 640, 480);
        ctx.fillStyle = '#fff';
        ctx.font = '24px sans-serif';
        ctx.fillText('Mock Screen Share', 160, 240);
        const stream = canvas.captureStream(10);
        setTimeout(() => resolve(stream), 150);
      });
    };
  })();
`;

async function navigateToMeeting(page: Page, meetingId: string, username: string): Promise<void> {
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
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 30_000 }).then(() => "waiting-for-meeting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting" || result === "waiting-for-meeting") {
    return result;
  }

  if (result === "auto-joined") {
    return "in-meeting";
  }

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);

  await expect(grid).toBeVisible({ timeout: 15_000 });
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

    const guestJoinButton = guestPage.getByRole("button", {
      name: /Join Meeting|Start Meeting/,
    });
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

async function clickScreenShareButton(page: Page): Promise<void> {
  const btn = page.locator(
    '.video-controls-container button[title="Screen Share"], ' +
      '.video-controls-container button[title="Share Screen"], ' +
      ".video-controls-container .controls-secondary button:first-child",
  );
  await expect(btn.first()).toBeVisible({ timeout: 10_000 });
  await btn.first().click();
}

test.describe("Screen-share visibility toast", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Happy path: host + peer in the same meeting. Host shares screen.
   * The peer's renderer decodes the first frame and emits a PEER_EVENT
   * back; the host UI transitions to the success toast.
   */
  test("transitions to success when a peer decodes the shared content", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_ss_vis_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "ss-vis-host@videocall.rs",
        "ShareHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "ss-vis-guest@videocall.rs",
        "ShareGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host needs the mock getDisplayMedia before any navigation.
      await hostPage.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);

      await navigateToMeeting(hostPage, meetingId, "ShareHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "ShareGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({
        timeout: 10_000,
      });
      await expect(guestPage.locator("#grid-container")).toBeVisible({
        timeout: 10_000,
      });

      // Wait for host to see guest's tile (signals the peer connection
      // is established in both directions, so the screen-share decode
      // path on the guest is ready to fire its ack back).
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // Reveal dock controls.
      await hostPage.mouse.move(400, 400);
      await hostPage.waitForTimeout(500);

      await clickScreenShareButton(hostPage);

      // The "Starting" toast must appear first.
      await expect(
        hostPage.locator(".peer-toast.toast-loading.screen-share-toast", {
          hasText: "Starting to share content",
        }),
      ).toBeVisible({ timeout: 5_000 });

      // The "SuccessfullyShared" toast must replace it once the guest
      // decodes the first frame and acks via PEER_EVENT.
      await expect(
        hostPage.locator(".peer-toast.toast-success.screen-share-toast", {
          hasText: "Others can now see your shared content",
        }),
      ).toBeVisible({ timeout: 15_000 });

      // The success toast must auto-dismiss after a few seconds.
      await expect(hostPage.locator(".peer-toast.toast-success.screen-share-toast")).toHaveCount(
        0,
        { timeout: 10_000 },
      );
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Error path: host shares but no peer ever decodes the share (we
   * intercept and drop every PEER_EVENT packet leaving the guest's
   * outgoing send queue). After ~10s the host UI must show the error
   * toast.
   *
   * Implementation note: rather than mocking the relay, we just join
   * a meeting alone — there is no peer to ack. The host should see
   * the error toast after the 10-second window expires.
   */
  test("transitions to error after timeout when no peer acks", async ({ baseURL }) => {
    test.setTimeout(60_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_ss_vis_err_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser,
        "ss-vis-solo@videocall.rs",
        "SoloHost",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      await hostPage.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);

      await navigateToMeeting(hostPage, meetingId, "SoloHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await expect(hostPage.locator("#grid-container")).toBeVisible({
        timeout: 10_000,
      });

      await hostPage.mouse.move(400, 400);
      await hostPage.waitForTimeout(500);

      await clickScreenShareButton(hostPage);

      await expect(
        hostPage.locator(".peer-toast.toast-loading.screen-share-toast", {
          hasText: "Starting to share content",
        }),
      ).toBeVisible({ timeout: 5_000 });

      // After the 10-second visibility window with no peer ack, the
      // toast must transition to the error variant.
      await expect(hostPage.locator(".peer-toast.toast-error.screen-share-toast")).toBeVisible({
        timeout: 15_000,
      });
    } finally {
      await browser.close();
    }
  });
});

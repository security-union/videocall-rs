import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

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

    const guestJoinButton = guestPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
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
 * Enable the local camera by clicking the "Start Video" control button.
 * Mirrors `enableMic` in host-mute.spec.ts.
 */
async function enableCamera(page: Page): Promise<void> {
  const startCamBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Start Video" }),
  });
  await expect(startCamBtn).toBeVisible({ timeout: 10_000 });
  await startCamBtn.click();
  await page.waitForTimeout(500);
}

/**
 * Open the per-tile host actions menu on a remote peer and click the inner
 * "Disable video" item.
 *
 * The per-tile menu is rendered inside the canvas grid (see
 * `canvas_generator.rs`). It only renders when the host viewer sees the peer
 * with `video_enabled=true`. The toggle (`title="Host actions"`, class
 * `.tile-mute-btn`) is hidden via `visibility: hidden` until the parent
 * `.grid-item` is hovered, so the test must hover before interacting.
 *
 * The flow is two-step:
 *   1. Click the toggle to open the tile context menu.
 *   2. Click the inner "Disable video" item, which invokes `on_disable_video`
 *      and broadcasts the host-disable-video event via NATS.
 *
 * We scope to a `.grid-item` that contains a `.tile-mute-btn` to avoid
 * matching the host's own tile.
 */
async function hostDisableVideoForPeerViaTile(page: Page): Promise<void> {
  const guestTile = page.locator(".grid-item:has(.tile-mute-btn)").first();
  await expect(guestTile).toBeVisible({ timeout: 15_000 });

  await guestTile.hover();

  const hostActionsToggle = guestTile.getByTitle("Host actions");
  await expect(hostActionsToggle).toBeVisible({ timeout: 15_000 });
  await hostActionsToggle.click();

  // Inner menu item — match by text.
  const disableVideoItem = guestTile.locator(".tile-context-menu-item", {
    hasText: "Disable video",
  });
  await expect(disableVideoItem).toBeVisible({ timeout: 5_000 });
  await disableVideoItem.click();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Host disable-video controls", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Test 1: Host disables video for a single participant via the per-tile menu.
   *
   * The tile menu's "Disable video" item is gated on:
   *   - the viewer is the host (is_owner = true), AND
   *   - the target peer is not the viewer themselves, AND
   *   - the peer's video_enabled is reported as true by diagnostics.
   * The guest must have their camera on before the host can see the menu item.
   */
  test("host disables video for a single participant", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostdisablevideo_single_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-disablevideo@videocall.rs",
        "DisableVidHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-disablevideo@videocall.rs",
        "DisableVidGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "DisableVidHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "DisableVidGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for the peer connection (host sees guest's tile).
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // Guest enables their camera so the host's per-tile diagnostics show
      // video_enabled=true and the "Disable video" menu item is rendered.
      await enableCamera(guestPage);

      // ---- Host opens the tile menu and clicks "Disable video" ----
      await hostDisableVideoForPeerViaTile(hostPage);

      // ---- Guest receives the host-disable-video event and sees the toast ----
      const guestVideoOffToast = guestPage.locator(".peer-toast .toast-name", {
        hasText: "Host turned off your camera",
      });
      await expect(guestVideoOffToast.first()).toBeVisible({ timeout: 15_000 });

      // ---- "Disable video" item disappears from host's view (peer's video off) ----
      // Once the guest's camera is off, on_disable_video becomes None and the
      // tile context menu no longer renders the "Disable video" item.
      // The "Host actions" toggle itself may still render for the mute item, so
      // we assert specifically on the inner menu item disappearing.
      // Re-open the menu in case it closed:
      const guestTile = hostPage.locator(".grid-item:has(.tile-mute-btn)").first();
      if (await guestTile.isVisible().catch(() => false)) {
        await guestTile.hover();
        const toggle = guestTile.getByTitle("Host actions");
        if (await toggle.isVisible().catch(() => false)) {
          await toggle.click();
        }
      }
      await expect(
        hostPage.locator(".tile-context-menu-item", { hasText: "Disable video" }),
      ).toHaveCount(0, { timeout: 10_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Test 2: "Disable video for all" broadcasts to every guest but NOT to the host.
   *
   * The host's VideoCallClient is constructed with on_host_disable_video: None
   * when is_owner=true, so the host never receives the disable-video callback
   * even though the NATS broadcast reaches their transport layer.
   */
  test("host disable-video-all disables all guests but not the host", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostdisablevideo_all_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-dvall@videocall.rs",
        "DVAllHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-dvall@videocall.rs",
        "DVAllGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      await navigateToMeeting(hostPage, meetingId, "DVAllHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "DVAllGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Enable camera on both sides so we can verify the host's stays on.
      await enableCamera(guestPage);
      await enableCamera(hostPage);

      // Confirm host camera is currently on — "Stop camera" tooltip = active.
      const hostActiveCamBtn = hostPage.locator("button.video-control-button", {
        has: hostPage.locator("span.tooltip", { hasText: "Stop camera" }),
      });
      await expect(hostActiveCamBtn).toBeVisible({ timeout: 5_000 });

      // ---- Host opens peer list then triggers "Disable video for all" ----
      await hostPage.locator(".video-controls-container").hover();
      await hostPage.mouse.move(400, 400);
      await hostPage.waitForTimeout(300);

      const openPeersBtn = hostPage.locator("button.video-control-button", {
        has: hostPage.locator("span.tooltip", { hasText: "Open Peers" }),
      });
      await expect(openPeersBtn).toBeVisible({ timeout: 10_000 });
      await openPeersBtn.click();
      await hostPage.waitForTimeout(1000);

      const hostActionsBtn = hostPage.locator('button[aria-label="Host actions"]');
      await expect(hostActionsBtn).toBeVisible({ timeout: 10_000 });
      await hostActionsBtn.click();

      const disableAllItem = hostPage.locator("button.context-menu-item", {
        hasText: "Disable video for all",
      });
      await expect(disableAllItem).toBeVisible({ timeout: 5_000 });
      await disableAllItem.click();

      // ---- Guest receives the NATS broadcast and sees the toast ----
      const guestVideoOffToast = guestPage.locator(".peer-toast .toast-name", {
        hasText: "Host turned off your camera",
      });
      await expect(guestVideoOffToast.first()).toBeVisible({ timeout: 15_000 });

      // ---- Host does NOT see the toast (on_host_disable_video is None for owner) ----
      await expect(
        hostPage.locator(".peer-toast .toast-name", {
          hasText: "Host turned off your camera",
        }),
      ).toHaveCount(0);

      // ---- Host's own camera remains on after disable-video-all ----
      await expect(hostActiveCamBtn).toBeVisible();
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Test 3: A participant can self-enable their camera after being disabled by the host.
   *
   * After the host disables video, the guest's on_host_disable_video callback
   * sets video_enabled=false. The guest can re-enable by clicking the "Start
   * camera" button (the same toggle used for self-disable).
   */
  test("participant can self-enable camera after being disabled by host", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_selfenablevideo_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-sev@videocall.rs",
        "SEVHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-sev@videocall.rs",
        "SEVGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      await navigateToMeeting(hostPage, meetingId, "SEVHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "SEVGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // Guest enables their camera.
      await enableCamera(guestPage);

      // Confirm guest camera is on ("Stop camera" tooltip visible).
      const guestStopCamBtn = guestPage.locator("button.video-control-button", {
        has: guestPage.locator("span.tooltip", { hasText: "Stop camera" }),
      });
      await expect(guestStopCamBtn).toBeVisible({ timeout: 5_000 });

      // Host disables video for the guest via the per-tile menu.
      await hostDisableVideoForPeerViaTile(hostPage);

      // Guest sees the toast.
      const guestVideoOffToast = guestPage.locator(".peer-toast .toast-name", {
        hasText: "Host turned off your camera",
      });
      await expect(guestVideoOffToast.first()).toBeVisible({ timeout: 15_000 });

      // Guest's camera button now shows "Start Video" (camera is off).
      const guestStartCamBtn = guestPage.locator("button.video-control-button", {
        has: guestPage.locator("span.tooltip", { hasText: "Start Video" }),
      });
      await expect(guestStartCamBtn).toBeVisible({ timeout: 10_000 });

      // Guest self-enables the camera.
      await guestStartCamBtn.click();

      // Camera is on again — "Stop camera" button reappears.
      await expect(
        guestPage.locator("button.video-control-button", {
          has: guestPage.locator("span.tooltip", { hasText: "Stop camera" }),
        }),
      ).toBeVisible({ timeout: 10_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});

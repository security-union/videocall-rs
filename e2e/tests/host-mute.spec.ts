import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

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

/**
 * Admit a guest from the waiting room if needed.
 * Returns once the guest is fully in the meeting (grid visible).
 */
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
 * Enable the local microphone by clicking the "Unmute" control button.
 * The MicButton uses accessible name from its tooltip span, so
 * getByRole("button", { name: "Unmute" }) reliably targets it.
 */
async function enableMic(page: Page): Promise<void> {
  const unmuteBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Unmute" }),
  });
  await expect(unmuteBtn).toBeVisible({ timeout: 10_000 });
  await unmuteBtn.click();
  await page.waitForTimeout(500);
}

/**
 * Mute a remote peer via the canvas tile's three-dot menu.
 *
 * The host-only mute control is rendered inside each remote peer's grid tile
 * (see `canvas_generator.rs`). It only renders when the host viewer sees the
 * peer with `audio_enabled=true`. The button is hidden via `visibility:
 * hidden` until the parent `.grid-item` is hovered (or the tile is in the
 * `.speaking-tile` state), so the test must hover before interacting.
 *
 * The flow is two-step:
 *   1. Click the menu-toggle button (`title="Mute participant"`, an
 *      `.tile-mute-btn`) to open the tile context menu.
 *   2. Click the inner "Mute" item (`.tile-context-menu-item`) to actually
 *      invoke `on_mute` and broadcast the host-mute via NATS.
 *
 * We scope to a `.grid-item` that contains a `.tile-mute-btn` to avoid
 * matching the host's own tile, which never renders the mute button.
 */
async function hostMutePeerViaTile(page: Page): Promise<void> {
  const guestTile = page.locator(".grid-item:has(.tile-mute-btn)").first();
  await expect(guestTile).toBeVisible({ timeout: 15_000 });

  // Hover to reveal `.tile-mute-btn` (CSS sets visibility:hidden until
  // `.grid-item:hover` or `.grid-item.speaking-tile`).
  await guestTile.hover();

  const muteToggle = guestTile.getByTitle("Mute participant");
  await expect(muteToggle).toBeVisible({ timeout: 15_000 });
  await muteToggle.click();

  // The inner "Mute" item only appears once the menu is open. It has no
  // title attribute — match by text inside `.tile-context-menu-item`.
  const muteMenuItem = guestTile.locator(".tile-context-menu-item", {
    hasText: "Mute",
  });
  await expect(muteMenuItem).toBeVisible({ timeout: 5_000 });
  await muteMenuItem.click();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Host mute controls", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Test 1: Host mutes a single participant via the per-tile three-dot menu.
   *
   * The tile mute menu (rendered in `canvas_generator.rs`) appears only when:
   *   - the viewer is the host (is_owner = true), AND
   *   - the target peer is not the viewer themselves, AND
   *   - the peer's audio_enabled is reported as true by diagnostics.
   * Therefore the guest must have their mic on before the host can see the
   * three-dot menu on the guest's tile.
   */
  test("host mutes a single participant", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostmute_single_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-mute@videocall.rs",
        "MuteHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-mute@videocall.rs",
        "MuteGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "MuteHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "MuteGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for the peer connection to establish (host sees guest's tile).
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // Guest enables their microphone so the host's per-tile diagnostics
      // reflect audio_enabled=true and the tile mute menu is rendered.
      await enableMic(guestPage);

      // ---- Host opens the tile menu and clicks the inner "Mute" item ----
      // The tile menu-toggle (`title="Mute participant"`) only opens the
      // context menu — it does not call on_mute directly. The inner "Mute"
      // menu item is what actually triggers the host-mute broadcast.
      await hostMutePeerViaTile(hostPage);

      // ---- Guest receives the host-mute NATS event and sees the toast ----
      const guestMuteToast = guestPage.locator(".peer-toast .toast-name", {
        hasText: "Host muted your microphone",
      });
      await expect(guestMuteToast.first()).toBeVisible({ timeout: 15_000 });

      // ---- Mute menu-toggle disappears from host's view (peer is now muted) ----
      // Once the guest is muted, on_mute becomes None and the entire
      // `.tile-mute-menu-wrapper` (and its `title="Mute participant"` button)
      // is no longer rendered for that peer.
      await expect(hostPage.getByTitle("Mute participant")).toHaveCount(0, {
        timeout: 10_000,
      });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Test 2: "Mute all" broadcasts to every guest but NOT to the host.
   *
   * The host's VideoCallClient is constructed with on_host_mute: None when
   * is_owner=true, so the host never receives the mute callback even though
   * the NATS broadcast reaches their transport layer.
   */
  test("host mute-all mutes all guests but not the host", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostmute_all_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-muteall@videocall.rs",
        "MuteAllHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-muteall@videocall.rs",
        "MuteAllGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "MuteAllHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "MuteAllGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Enable mic on both sides so we can verify the host's stays active.
      await enableMic(guestPage);
      await enableMic(hostPage);

      // Confirm host mic is currently on — "Mute" tooltip means it's active.
      const hostActiveMicBtn = hostPage.locator("button.video-control-button", {
        has: hostPage.locator("span.tooltip", { hasText: "Mute" }),
      });
      await expect(hostActiveMicBtn).toBeVisible({ timeout: 5_000 });

      // ---- Host opens peer list then clicks "Mute all" via context menu ----
      const openPeersBtn = hostPage.locator("button.video-control-button", {
        has: hostPage.locator("span.tooltip", { hasText: "Open Peers" }),
      });
      await expect(openPeersBtn).toBeVisible({ timeout: 10_000 });
      await openPeersBtn.click();
      await hostPage.waitForTimeout(1000);

      const hostActionsBtn = hostPage.locator('button[aria-label="Host actions"]');
      await expect(hostActionsBtn).toBeVisible({ timeout: 10_000 });
      await hostActionsBtn.click();

      const muteAllItem = hostPage.locator("button.context-menu-item", { hasText: "Mute all" });
      await expect(muteAllItem).toBeVisible({ timeout: 5_000 });
      await muteAllItem.click();

      // ---- Guest receives the NATS broadcast and sees the toast ----
      const guestMuteToast = guestPage.locator(".peer-toast .toast-name", {
        hasText: "Host muted your microphone",
      });
      await expect(guestMuteToast.first()).toBeVisible({ timeout: 15_000 });

      // ---- Host does NOT see the mute toast (on_host_mute is None for owner) ----
      // Assert immediately after the guest's toast confirmed propagation, so
      // the NATS event has had time to arrive at the host transport layer too.
      await expect(
        hostPage.locator(".peer-toast .toast-name", {
          hasText: "Host muted your microphone",
        }),
      ).toHaveCount(0);

      // ---- Host's own mic remains active after mute-all ----
      await expect(hostActiveMicBtn).toBeVisible();
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Test 3: A participant can self-unmute after being muted by the host.
   *
   * After the host mutes the guest, the guest's on_host_mute callback sets
   * mic_enabled=false. The guest can re-enable their mic by clicking the
   * "Unmute" button (same toggle they use for self-mute).
   */
  test("participant can self-unmute after being muted by host", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_selfunmute_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-selfunmute@videocall.rs",
        "SelfUnmuteHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-selfunmute@videocall.rs",
        "SelfUnmuteGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "SelfUnmuteHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "SelfUnmuteGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for peer connection
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // ---- Guest enables their mic ----
      await enableMic(guestPage);

      // Confirm guest mic is active ("Mute" tooltip visible = mic on).
      const guestMuteBtn = guestPage.locator("button.video-control-button", {
        has: guestPage.locator("span.tooltip", { hasText: "Mute" }),
      });
      await expect(guestMuteBtn).toBeVisible({ timeout: 5_000 });

      // ---- Host mutes the guest via the per-tile three-dot menu ----
      await hostMutePeerViaTile(hostPage);

      // ---- Guest sees the mute toast ----
      const guestMuteToast = guestPage.locator(".peer-toast .toast-name", {
        hasText: "Host muted your microphone",
      });
      await expect(guestMuteToast.first()).toBeVisible({ timeout: 15_000 });

      // ---- Guest's mic button now shows "Unmute" (mic is off) ----
      const guestUnmuteBtn = guestPage.locator("button.video-control-button", {
        has: guestPage.locator("span.tooltip", { hasText: "Unmute" }),
      });
      await expect(guestUnmuteBtn).toBeVisible({ timeout: 10_000 });

      // ---- Guest self-unmutes ----
      await guestUnmuteBtn.click();

      // ---- Guest's mic is active again — "Mute" button reappears ----
      await expect(
        guestPage.locator("button.video-control-button", {
          has: guestPage.locator("span.tooltip", { hasText: "Mute" }),
        }),
      ).toBeVisible({ timeout: 10_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});

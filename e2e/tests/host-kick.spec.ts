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
 * Open the per-tile host actions menu on a remote peer and click the inner
 * "Kick" item.
 *
 * The per-tile menu is rendered inside the canvas grid (see
 * `canvas_generator.rs`). Unlike Mute / Disable-video — which are gated on
 * the peer's audio/video state — the "Kick" item is rendered whenever the
 * viewer is the host and the peer is not themselves (see `peer_tile.rs`:
 * `is_current_user_host && !is_self_peer`). The toggle
 * (`title="Host actions"`, class `.tile-mute-btn`) is hidden via
 * `visibility: hidden` until the parent `.grid-item` is hovered, so the test
 * must hover before interacting.
 *
 * The flow is two-step:
 *   1. Click the toggle to open the tile context menu.
 *   2. Click the inner "Kick" item (a `.tile-context-menu-item` with the
 *      `--danger` modifier), which invokes the host kick action — the server
 *      sets the participant's DB status to `'kicked'` and publishes a
 *      `PARTICIPANT_KICKED` NATS event.
 *
 * We scope to a `.grid-item` that contains a `.tile-mute-btn` to avoid
 * matching the host's own tile (which never renders the host-actions button).
 */
async function hostKickPeerViaTile(page: Page): Promise<void> {
  const guestTile = page.locator(".grid-item:has(.tile-mute-btn)").first();
  await expect(guestTile).toBeVisible({ timeout: 15_000 });

  // Hover to reveal `.tile-mute-btn` (CSS sets visibility:hidden until
  // `.grid-item:hover`).
  await guestTile.hover();

  const hostActionsToggle = guestTile.getByTitle("Host actions");
  await expect(hostActionsToggle).toBeVisible({ timeout: 15_000 });
  await hostActionsToggle.click();

  // The "Kick" item carries the danger modifier class. Match by text inside
  // `.tile-context-menu-item` (which also matches the danger variant).
  const kickItem = guestTile.locator(".tile-context-menu-item", {
    hasText: "Kick",
  });
  await expect(kickItem).toBeVisible({ timeout: 5_000 });
  await kickItem.click();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Host kick controls", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Test 1: Host kicks a participant via the per-tile three-dot menu.
   *
   * Unlike Mute / Disable-video, the "Kick" item is NOT gated on the peer's
   * audio/video state — it is rendered as soon as the viewer is the host and
   * the tile is not their own. So we do not need to enable mic/camera on the
   * guest before exercising the menu.
   *
   * When the host clicks Kick: the server sets the participant's DB status to
   * `'kicked'` and publishes a `PARTICIPANT_KICKED` NATS event. The kicked
   * participant's UI receives the event and shows the `MeetingEndedOverlay`
   * with the kicked-by-host message.
   */
  test("host kicks a participant", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostkick_single_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-kick@videocall.rs",
        "KickHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-kick@videocall.rs",
        "KickGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "KickHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "KickGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for the peer connection to establish (host sees guest's tile).
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // ---- Host opens the tile menu and clicks "Kick" ----
      await hostKickPeerViaTile(hostPage);

      // ---- Guest receives PARTICIPANT_KICKED: client disconnects immediately
      // and the MeetingEndedOverlay is shown. ----
      // The overlay's root element carries the `meeting-ended-overlay` class
      // (see `meeting_ended_overlay.rs`).
      const guestKickedOverlay = guestPage.locator(".meeting-ended-overlay");
      await expect(guestKickedOverlay).toBeVisible({ timeout: 20_000 });

      // The overlay's message paragraph carries the configured text.
      await expect(
        guestPage.locator(".meeting-ended-message", {
          hasText: "You have been removed from the meeting by the host.",
        }),
      ).toBeVisible({ timeout: 5_000 });

      // The meeting controls (hang-up button etc.) must not be active —
      // immediate disconnect sets is_active=false which hides the in-call UI.
      await expect(guestPage.locator(".video-controls-container")).toHaveCount(0, {
        timeout: 5_000,
      });

      // ---- Host does NOT see the kicked overlay (host is never kicked by
      // their own kick action — server rejects user_id == caller). ----
      await expect(hostPage.locator(".meeting-ended-overlay")).toHaveCount(0);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Test 2: A kicked participant can rejoin the meeting by navigating back to
   * the meeting URL — they go through the normal join flow.
   *
   * Implementation note: the overlay's "Return to Home" button navigates the
   * browser to `/`, so we click that and then re-navigate to the meeting via
   * `navigateToMeeting` (the same flow a real user would take).
   */
  test("kicked participant can rejoin", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostkick_rejoin_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-kickrejoin@videocall.rs",
        "KickRejoinHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-kickrejoin@videocall.rs",
        "KickRejoinGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "KickRejoinHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "KickRejoinGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for the peer connection (host sees guest's tile).
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // ---- Host kicks the guest ----
      await hostKickPeerViaTile(hostPage);

      // ---- Guest sees the kicked overlay ----
      const guestKickedOverlay = guestPage.locator(".meeting-ended-overlay");
      await expect(guestKickedOverlay).toBeVisible({ timeout: 20_000 });
      await expect(
        guestPage.locator(".meeting-ended-message", {
          hasText: "You have been removed from the meeting by the host.",
        }),
      ).toBeVisible({ timeout: 5_000 });

      // ---- Guest clicks "Return to Home" on the overlay ----
      // The button navigates the browser to `/`, which is the same flow a
      // real user would take to leave and try again.
      const returnHomeBtn = guestPage.locator("button.meeting-ended-home-btn");
      await expect(returnHomeBtn).toBeVisible({ timeout: 5_000 });
      await returnHomeBtn.click();

      // Wait for the home page to load.
      await expect(guestPage).toHaveURL(new RegExp(`^${uiURL}/?$`), { timeout: 10_000 });
      await guestPage.waitForTimeout(1500);

      // ---- Guest re-navigates to the meeting via the normal join flow ----
      await navigateToMeeting(guestPage, meetingId, "KickRejoinGuest");
      const rejoinResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, rejoinResult);

      // ---- Guest is back in the meeting (grid container visible again) ----
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 20_000 });

      // ---- The kicked overlay must not be present after a successful rejoin ----
      await expect(guestPage.locator(".meeting-ended-overlay")).toHaveCount(0);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Test 3: The host's own tile never shows the kick (or any host-actions)
   * button — `is_self_peer` is true for the host's own tile, which suppresses
   * the entire `.tile-mute-menu-wrapper` (and its `.tile-mute-btn`).
   *
   * With only the host in the meeting, there should be zero `.tile-mute-btn`
   * elements anywhere on the page.
   */
  test("host tile does not show kick button for self", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostkick_self_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-kickself@videocall.rs",
        "KickSelfHost",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();

      // ---- Host joins alone ----
      await navigateToMeeting(hostPage, meetingId, "KickSelfHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for the host's own canvas tile to render.
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // Hover the host's tile to ensure that if any host-actions button
      // existed, its hover-gated visibility would not hide it from the
      // assertion. There should still be zero such buttons.
      const hostTile = hostPage.locator(".grid-item").first();
      await hostTile.hover();
      await hostPage.waitForTimeout(500);

      // ---- The host's own tile must NOT render the three-dot host-actions
      // button. With only the host present, there must be zero buttons
      // anywhere. ----
      await expect(hostPage.locator(".tile-mute-btn")).toHaveCount(0);
      await expect(hostPage.getByTitle("Host actions")).toHaveCount(0);
    } finally {
      await browser1.close();
    }
  });
});

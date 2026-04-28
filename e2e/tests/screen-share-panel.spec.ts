import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Screen-share right panel layout E2E tests.
 *
 * When a participant shares their screen the meeting switches to a split
 * layout: the shared screen fills the left 2/3 of #grid-container while
 * peer video tiles are arranged in a 2-column grid on the right 1/3.
 *
 * If the number of peers exceeds the right panel's capacity a
 * `.grid-overflow-badge` element is rendered showing "+N more in meeting".
 *
 * LIMITATION: `getDisplayMedia()` opens a system-level picker that
 * Playwright cannot drive in all environments.  We use Chromium's
 * `--auto-select-desktop-capture-source` flag to auto-accept the picker
 * in CI-compatible headless mode.  If the flag is unavailable the screen
 * share button click will not produce a stream and the split layout will
 * not activate — the tests document this and skip gracefully.
 *
 * Mock peers are used to fill the right panel beyond its capacity so that
 * the overflow badge can be verified.  Mock peers require the Dioxus UI
 * to be built with `mockPeersEnabled: "true"` in config.js.
 */

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
  // Auto-accept getDisplayMedia() system picker for screen sharing.
  "--auto-select-desktop-capture-source=Entire screen",
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
 * Set up a two-user meeting (host + guest) and return both pages
 * along with browser handles for cleanup.
 */
async function setupTwoUserMeeting(
  uiURL: string,
  meetingId: string,
  hostName: string,
  guestName: string,
) {
  const browser1 = await chromium.launch({ args: BROWSER_ARGS });
  const browser2 = await chromium.launch({ args: BROWSER_ARGS });

  const hostCtx = await createAuthenticatedContext(
    browser1,
    `${hostName.toLowerCase()}@videocall.rs`,
    hostName,
    uiURL,
  );
  const guestCtx = await createAuthenticatedContext(
    browser2,
    `${guestName.toLowerCase()}@videocall.rs`,
    guestName,
    uiURL,
  );

  const hostPage = await hostCtx.newPage();
  const guestPage = await guestCtx.newPage();

  await navigateToMeeting(hostPage, meetingId, hostName);
  const hostResult = await joinMeetingFromPage(hostPage);
  expect(hostResult).toBe("in-meeting");

  await navigateToMeeting(guestPage, meetingId, guestName);
  const guestResult = await joinMeetingFromPage(guestPage);
  await admitGuestIfNeeded(hostPage, guestPage, guestResult);

  // Wait for peer tile to appear on the host side
  const peerTile = hostPage.locator("#grid-container .grid-item");
  await expect(peerTile.first()).toBeVisible({ timeout: 30_000 });

  return { hostPage, guestPage, browser1, browser2 };
}

/**
 * Click the screen share button on a page.
 *
 * The ScreenShareButton renders with a tooltip "Share Screen" (inactive)
 * or "Stop Screen Share" (active). We locate the button by its tooltip text.
 *
 * Returns true if the split layout activated (screen share succeeded),
 * false otherwise.
 */
async function startScreenShare(sharerPage: Page, viewerPage: Page): Promise<boolean> {
  // The screen share button's tooltip reads "Share Screen" when inactive.
  const shareButton = sharerPage.locator("button.video-control-button", {
    has: sharerPage.locator(".tooltip", { hasText: "Share Screen" }),
  });

  await expect(shareButton).toBeVisible({ timeout: 10_000 });
  await shareButton.click();

  // Wait for the split layout to appear on the viewer's side.
  // The split layout replaces the normal grid with a flex container
  // that has a .split-screen-tile child in the left panel.
  try {
    await expect(viewerPage.locator(".split-screen-tile")).toBeVisible({
      timeout: 15_000,
    });
    return true;
  } catch {
    return false;
  }
}

/**
 * Add mock peers via the Mock Peers popover.
 *
 * Requires `mockPeersEnabled: "true"` in config.js. Opens the popover,
 * sets the peer count, and closes it.
 */
async function addMockPeers(page: Page, count: number): Promise<void> {
  // Open the mock peers popover by clicking the MockPeersButton.
  // The button is inside the host controls bar.
  const mockButton = page.locator("button.video-control-button", {
    has: page.locator(".tooltip", { hasText: /Mock Peers/i }),
  });

  await expect(mockButton).toBeVisible({ timeout: 10_000 });
  await mockButton.click();
  await page.waitForTimeout(500);

  // The popover has a number input for the count
  const countInput = page.locator(".mock-peers-popover input[type='number']");
  await expect(countInput).toBeVisible({ timeout: 5_000 });

  // Clear and set the desired count
  await countInput.fill(String(count));
  await page.waitForTimeout(500);

  // Close the popover
  const closeButton = page.locator(".mock-peers-popover-close");
  await closeButton.click();
  await page.waitForTimeout(1000);
}

test.describe("Screen share right panel layout", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // ──────────────────────────────────────────────────────────────────────
  // 1. Right panel renders 2-column grid during screen share
  //
  // Verifies that when a peer shares their screen, the meeting switches
  // to a split layout where the right panel uses a 2-column CSS grid
  // with `.split-peer-tile` elements for peer video tiles.
  // ──────────────────────────────────────────────────────────────────────
  test("right panel renders 2-column grid during screen share", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_panel_grid_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSGridHost",
      "SSGridGuest",
    );

    try {
      // Give time for WebSocket/WebTransport peer discovery
      await hostPage.waitForTimeout(3000);

      // Guest starts screen sharing
      const shareActivated = await startScreenShare(guestPage, hostPage);

      if (!shareActivated) {
        // getDisplayMedia() could not be auto-accepted in this environment.
        // Skip the test gracefully with a descriptive message.
        test.skip(
          true,
          "Screen share could not be auto-accepted. " +
            "The --auto-select-desktop-capture-source flag may not be supported " +
            "in this Chromium build or display environment.",
        );
        return;
      }

      // ---- ASSERT: split layout is active ----
      // The #grid-container should now be a flex container with two children:
      //   - Left panel (flex: 2) with .split-screen-tile
      //   - Right panel (flex: 1) with a 2-column grid

      // Verify the screen share tile is visible on the left
      await expect(hostPage.locator(".split-screen-tile")).toBeVisible({ timeout: 10_000 });

      // Verify the right panel has grid-template-columns with 2 columns (1fr 1fr)
      // The right panel is the second child of #grid-container with inline
      // style containing "grid-template-columns: 1fr 1fr"
      const rightPanel = hostPage.locator(
        "#grid-container > div:nth-child(2)",
      );
      await expect(rightPanel).toBeVisible({ timeout: 10_000 });

      const rightPanelStyle = await rightPanel.getAttribute("style");
      expect(rightPanelStyle).toBeTruthy();
      expect(rightPanelStyle).toContain("grid-template-columns: 1fr 1fr");

      // Verify peer tiles (.split-peer-tile) are rendered in the right panel
      const peerTiles = hostPage.locator(".split-peer-tile");
      const tileCount = await peerTiles.count();
      expect(tileCount).toBeGreaterThan(0);

      // Verify each peer tile has the expected structure:
      // a .canvas-container child for the video content
      const firstTile = peerTiles.first();
      await expect(firstTile).toBeVisible({ timeout: 5_000 });
      const canvasContainer = firstTile.locator(".canvas-container");
      await expect(canvasContainer).toBeVisible({ timeout: 5_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 2. Overflow badge shows when peers exceed right panel capacity
  //
  // When the number of peers exceeds the right panel's 2-column grid
  // capacity, a `.grid-overflow-badge` element should appear showing
  // "+N" and "more in meeting".
  //
  // This test uses mock peers to fill the panel beyond capacity.
  // Requires `mockPeersEnabled: "true"` in config.js.
  // ──────────────────────────────────────────────────────────────────────
  test("overflow badge shows when peers exceed right panel capacity", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_panel_overflow_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSOverHost",
      "SSOverGuest",
    );

    try {
      // Give time for WebSocket/WebTransport peer discovery
      await hostPage.waitForTimeout(3000);

      // Check if mock peers feature is available by looking for the button
      const mockButton = hostPage.locator("button.video-control-button", {
        has: hostPage.locator(".tooltip", { hasText: /Mock Peers/i }),
      });

      const mockPeersAvailable = await mockButton.isVisible().catch(() => false);

      if (!mockPeersAvailable) {
        test.skip(
          true,
          "Mock peers feature is not enabled. " +
            "Set mockPeersEnabled: \"true\" in config.js to enable this test.",
        );
        return;
      }

      // Add enough mock peers to exceed the right panel capacity.
      // The right panel uses a 2-column grid. At typical viewport sizes,
      // only 4-8 tiles fit (2-4 rows * 2 cols). Adding 20 mock peers
      // ensures overflow regardless of viewport height.
      await addMockPeers(hostPage, 20);

      // Wait for mock peer tiles to render in the normal grid
      await hostPage.waitForTimeout(2000);

      // Guest starts screen sharing to trigger the split layout
      const shareActivated = await startScreenShare(guestPage, hostPage);

      if (!shareActivated) {
        test.skip(
          true,
          "Screen share could not be auto-accepted. " +
            "The --auto-select-desktop-capture-source flag may not be supported " +
            "in this Chromium build or display environment.",
        );
        return;
      }

      // Wait for the split layout to stabilize with mock peers
      await hostPage.waitForTimeout(3000);

      // ---- ASSERT: overflow badge is visible ----
      const overflowBadge = hostPage.locator(".grid-overflow-badge");
      await expect(overflowBadge).toBeVisible({ timeout: 10_000 });

      // Verify the badge contains "+N" text (where N > 0)
      const badgeText = await overflowBadge.textContent();
      expect(badgeText).toBeTruthy();
      expect(badgeText).toMatch(/\+\d+/);

      // Verify the badge contains "more in meeting"
      const moreSpan = overflowBadge.locator("span");
      await expect(moreSpan).toContainText("more in meeting");

      // Verify some split-peer-tile elements are also visible
      // (the badge coexists with visible tiles)
      const peerTiles = hostPage.locator(".split-peer-tile");
      const visibleTileCount = await peerTiles.count();
      expect(visibleTileCount).toBeGreaterThan(0);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 3. Normal grid has no split-layout artifacts
  //
  // Structural safety check: when nobody is screen sharing, the meeting
  // should use the normal grid layout with NO .split-screen-tile,
  // .split-peer-tile, or .grid-overflow-badge elements present in the
  // right panel context.
  // ──────────────────────────────────────────────────────────────────────
  test("normal meeting grid has no split-layout or overflow artifacts", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_panel_clean_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSCleanHost",
      "SSCleanGuest",
    );

    try {
      // Wait for the grid to stabilize
      await hostPage.waitForTimeout(3000);

      // ---- ASSERT: no split-layout elements present ----
      await expect(hostPage.locator(".split-screen-tile")).toHaveCount(0);
      await expect(hostPage.locator(".split-peer-tile")).toHaveCount(0);

      // The normal grid uses .grid-item tiles, not .split-peer-tile
      const gridItems = hostPage.locator("#grid-container .grid-item");
      await expect(gridItems.first()).toBeVisible({ timeout: 10_000 });

      // The normal grid should use CSS grid (grid-template-columns with
      // repeat(N, 1fr)) rather than the split layout's flex container.
      const containerStyle = await hostPage.locator("#grid-container").getAttribute("style");
      expect(containerStyle).toBeTruthy();
      expect(containerStyle).toContain("grid-template-columns");
      expect(containerStyle).toContain("grid-template-rows");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});

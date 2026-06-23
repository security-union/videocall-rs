import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";

/**
 * Screen-share right panel layout E2E tests.
 *
 * When a participant shares their screen the meeting switches to a split
 * layout: the shared screen fills the left 2/3 of #grid-container while
 * peer video tiles are arranged in a CSS grid panel (.ss-peer-panel)
 * on the right 1/3. All participants are rendered (no cap); the panel
 * scrolls vertically when tiles overflow. Off-budget peers render as
 * avatar-tier tiles (no video decode) per the decode-budget system
 * (issue #987).
 *
 * LIMITATION: `getDisplayMedia()` opens a system-level picker that
 * Playwright cannot drive in all environments.  We use Chromium's
 * `--auto-select-desktop-capture-source` flag to auto-accept the picker
 * in CI-compatible headless mode.  If the flag is unavailable the screen
 * share button click will not produce a stream and the split layout will
 * not activate — the tests document this and skip gracefully.
 *
 * Mock peers are used to verify many-participant rendering and scroll
 * behavior.  Mock peers require the Dioxus UI to be built with
 * `mockPeersEnabled: "true"` in config.js.
 */

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
  "--disable-dev-shm-usage",
  "--renderer-process-limit=1",
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
  // Hydration-robust submit (see helpers/join-meeting.ts): gates submission on
  // the post-hydration submit button and waits for the home form to detach (the
  // route-changed signal that is robust to the History-API URL lagging the
  // rendered route under load) instead of a fixed wait + URL poll. Fixes the
  // bvt join flake where the page rendered the meeting but stayed reported at
  // "/" so toHaveURL(/meeting/) timed out.
  await fillAndSubmitJoinForm(page, meetingId, username);
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

  if (result === "waiting") {
    return "waiting";
  }

  if (result === "waiting-for-meeting") {
    return "waiting-for-meeting";
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

// Synthetic getDisplayMedia mock — injected via addInitScript so screen
// share tests run reliably in headless without a system-level picker.
const MOCK_GET_DISPLAY_MEDIA_SCRIPT = `
  (() => {
    const mediaDevices = navigator.mediaDevices;
    if (!mediaDevices) return;
    const createStream = () => {
      const canvas = document.createElement('canvas');
      canvas.width = 640; canvas.height = 480;
      const ctx = canvas.getContext('2d');
      ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 640, 480);
      ctx.fillStyle = '#fff'; ctx.font = '24px sans-serif';
      ctx.fillText('Mock Screen Share', 160, 240);
      return canvas.captureStream(5);
    };
    Object.defineProperty(mediaDevices, 'getDisplayMedia', {
      configurable: true, value: async () => createStream(),
    });
  })();
`;

/**
 * Set up a two-user meeting (host + guest) and return both pages
 * along with browser handles for cleanup.
 */
async function setupTwoUserMeeting(
  uiURL: string,
  meetingId: string,
  hostName: string,
  guestName: string,
  opts: { mockDisplayMedia?: boolean } = {},
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

  if (opts.mockDisplayMedia) {
    await hostCtx.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);
    await guestCtx.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);
  }

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
  // Wake auto-hidden controls bar, then find the share button by tooltip.
  await sharerPage.mouse.move(400, 400);
  await sharerPage.waitForTimeout(300);
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
  test("right panel renders 2-column grid during screen share @bvt1", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_panel_grid_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSGridHost",
      "SSGridGuest",
      { mockDisplayMedia: true },
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

      // Verify the right panel uses the .ss-peer-panel CSS class with
      // flexbox layout. The right panel is the 3rd child: left +
      // resize-handle + right.
      const rightPanel = hostPage.locator("#grid-container > div:nth-child(3)");
      await expect(rightPanel).toBeVisible({ timeout: 10_000 });

      // Layout is CSS-class-driven (.ss-peer-panel) using CSS grid.
      // Verify the panel has the expected class and computed layout.
      await expect(rightPanel).toHaveClass(/ss-peer-panel/);
      expect(await rightPanel.evaluate((el) => getComputedStyle(el).display)).toBe("grid");

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
  // 2. All participants rendered with scrollable panel (no cap/badge)
  //
  // When many peers are present during screen share, all participants
  // are rendered as .split-peer-tile elements (no artificial cap, no
  // overflow badge). The panel scrolls vertically when tiles overflow.
  //
  // Requires `mockPeersEnabled: "true"` in config.js.
  // ──────────────────────────────────────────────────────────────────────
  test("all participants rendered with scrollable panel during screen share", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_panel_scroll_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSScrollHost",
      "SSScrollGuest",
    );

    try {
      await hostPage.waitForTimeout(3000);

      const mockButton = hostPage.locator("button.video-control-button", {
        has: hostPage.locator(".tooltip", { hasText: /Mock Peers/i }),
      });
      const mockPeersAvailable = await mockButton.isVisible().catch(() => false);
      if (!mockPeersAvailable) {
        test.skip(true, 'Mock peers not enabled. Set mockPeersEnabled: "true" in config.js.');
        return;
      }

      // Add 20 mock peers — enough to overflow the panel at any viewport.
      await addMockPeers(hostPage, 20);
      await hostPage.waitForTimeout(2000);

      const shareActivated = await startScreenShare(guestPage, hostPage);
      if (!shareActivated) {
        test.skip(true, "Screen share could not be auto-accepted.");
        return;
      }
      await hostPage.waitForTimeout(3000);

      // All participants should be rendered (no truncation/badge).
      const peerTiles = hostPage.locator(".split-peer-tile");
      const tileCount = await peerTiles.count();
      // 20 mock + 2 real users (host + guest) = 22, but self-tile may
      // or may not appear; assert at least 20 to confirm no cap.
      expect(tileCount).toBeGreaterThanOrEqual(20);

      // No overflow badge should be present.
      const overflowBadge = hostPage.locator(".grid-overflow-badge");
      await expect(overflowBadge).toHaveCount(0);

      // Panel should be scrollable (scrollHeight > clientHeight).
      const panel = hostPage.locator(".ss-peer-panel");
      await expect(panel).toBeVisible({ timeout: 5_000 });
      const scrollable = await panel.evaluate((el) => el.scrollHeight > el.clientHeight);
      expect(scrollable).toBe(true);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 3. Off-budget camera-on peers render as paused avatars, not blank canvases
  //
  // The decode-budget system limits live video decoding to the first N
  // tiles in the screen-share right panel. Tiles beyond the budget must
  // render with `force_avatar: true`, producing a
  // `.placeholder-content--paused` element instead of a `<canvas>`.
  //
  // IMPORTANT: mock peers are video-OFF (is_video_enabled_for_peer returns
  // false for "mock-N" IDs) and render plain `.placeholder-content`
  // ("Video Disabled"), NOT `.placeholder-content--paused`. The --paused
  // placeholder only renders for camera-ON peers excluded by the budget
  // (force_avatar && is_video_enabled_for_peer). Therefore this test uses
  // 4 real browsers (host + 3 guests, all with fake camera) and forces
  // Fixed(2) so at least 1 real camera-on peer lands in the avatar tier.
  //
  // Host sees 3 remote peers in the SS right panel. With ss_budget = 2,
  // peers are sorted by join time: guests 1-2 (earliest) get decoded,
  // guest 3 (latest) gets force_avatar: true → .placeholder-content--paused.
  // ──────────────────────────────────────────────────────────────────────
  test("off-budget camera-on peers render as paused avatars during screen share", async ({
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_panel_avatar_budget_${Date.now()}`;

    // Fixed(2): with 3 remote camera-on peers, ss_budget = 2 → 1 peer
    // is off-budget and must render as .placeholder-content--paused.
    const FORCED_BUDGET = 2;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });
    const browser3 = await chromium.launch({ args: BROWSER_ARGS });
    const browser4 = await chromium.launch({ args: BROWSER_ARGS });

    const hostCtx = await createAuthenticatedContext(
      browser1,
      "ssbudgethost@videocall.rs",
      "SSBudgetHost",
      uiURL,
    );
    // Inject Fixed(N) budget override on host BEFORE page navigation.
    await hostCtx.addInitScript(`
      localStorage.setItem("vc_decode_budget_override", "${FORCED_BUDGET}");
    `);
    await hostCtx.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);

    const guest1Ctx = await createAuthenticatedContext(
      browser2,
      "ssbudgetguest1@videocall.rs",
      "SSBudgetGuest1",
      uiURL,
    );
    await guest1Ctx.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);
    // Enable camera on all guests so they publish real video. The prejoin
    // camera defaults to OFF; without this the peers join camera-off and
    // is_video_enabled_for_peer returns false → plain "Video Disabled"
    // placeholder instead of the --paused avatar we're testing.
    // (Pattern: simulcast-per-receiver.spec.ts:343, signal-quality-peer-transport.spec.ts:58)
    await guest1Ctx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);

    const guest2Ctx = await createAuthenticatedContext(
      browser3,
      "ssbudgetguest2@videocall.rs",
      "SSBudgetGuest2",
      uiURL,
    );
    await guest2Ctx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);

    const guest3Ctx = await createAuthenticatedContext(
      browser4,
      "ssbudgetguest3@videocall.rs",
      "SSBudgetGuest3",
      uiURL,
    );
    await guest3Ctx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);

    const hostPage = await hostCtx.newPage();
    const guest1Page = await guest1Ctx.newPage();
    const guest2Page = await guest2Ctx.newPage();
    const guest3Page = await guest3Ctx.newPage();

    try {
      // Host joins first.
      await navigateToMeeting(hostPage, meetingId, "SSBudgetHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // Guests join sequentially (join order determines budget ranking).
      await navigateToMeeting(guest1Page, meetingId, "SSBudgetGuest1");
      const g1Result = await joinMeetingFromPage(guest1Page);
      await admitGuestIfNeeded(hostPage, guest1Page, g1Result);

      await navigateToMeeting(guest2Page, meetingId, "SSBudgetGuest2");
      const g2Result = await joinMeetingFromPage(guest2Page);
      await admitGuestIfNeeded(hostPage, guest2Page, g2Result);

      await navigateToMeeting(guest3Page, meetingId, "SSBudgetGuest3");
      const g3Result = await joinMeetingFromPage(guest3Page);
      await admitGuestIfNeeded(hostPage, guest3Page, g3Result);

      // Wait for all 3 remote peer tiles to appear on the host.
      const gridTiles = hostPage.locator("#grid-container .grid-item");
      await expect(gridTiles.nth(2)).toBeVisible({ timeout: 30_000 });

      // Precondition: verify the host sees live peer video (canvases) before
      // entering screen share. If cameras didn't publish, canvasCount = 0
      // and the paused-avatar assertion below would fail for the wrong reason.
      const liveCanvases = hostPage.locator(".grid-item canvas");
      await expect(liveCanvases.first()).toBeVisible({ timeout: 15_000 });
      const preSSCanvasCount = await liveCanvases.count();
      expect(preSSCanvasCount).toBeGreaterThanOrEqual(2);

      // Guest 1 starts screen sharing → host sees SS split layout.
      const shareActivated = await startScreenShare(guest1Page, hostPage);
      if (!shareActivated) {
        test.skip(true, "Screen share could not be auto-accepted.");
        return;
      }

      // Let the split layout settle and the decode budget take effect.
      await hostPage.waitForTimeout(5000);

      // ---- ASSERT: peer tiles are rendered in the SS right panel ----
      const peerTiles = hostPage.locator(".split-peer-tile");
      const tileCount = await peerTiles.count();
      // Host sees 3 remote peers (guests 1-3) in the right panel.
      expect(tileCount).toBeGreaterThanOrEqual(3);

      // ---- ASSERT: at least 1 off-budget camera-on peer is --paused ----
      const pausedTiles = hostPage.locator(".split-peer-tile .placeholder-content--paused");
      const canvasTiles = hostPage.locator(".split-peer-tile canvas");

      const pausedCount = await pausedTiles.count();
      const canvasCount = await canvasTiles.count();

      // With Fixed(2) and 3 real camera-on peers, we MUST have at least 1
      // paused avatar tile. This fails (not skips) if force_avatar regresses.
      expect(pausedCount).toBeGreaterThan(0);
      expect(canvasCount).toBeGreaterThan(0);
      expect(canvasCount).toBeLessThanOrEqual(FORCED_BUDGET);

      // Verify the paused placeholder has the expected accessibility
      // attributes (role="img" + non-empty aria-label).
      const firstPaused = pausedTiles.first();
      await expect(firstPaused).toBeVisible({ timeout: 5_000 });
      await expect(firstPaused).toHaveAttribute("role", "img");
      const ariaLabel = await firstPaused.getAttribute("aria-label");
      expect(ariaLabel).toBeTruthy();
    } finally {
      await browser1.close();
      await browser2.close();
      await browser3.close();
      await browser4.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 4. Normal grid has no split-layout artifacts
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

  // ──────────────────────────────────────────────────────────────────────
  // 4. HCL bugs #3 + #4: side-strip tiles left-justify and hold 3:2 cap
  //    on a wide viewport.
  //
  // Bug #3+#4: the right panel now uses CSS class `.ss-peer-panel`
  // with CSS grid layout. Tiles pack to the left edge via
  // align-content: start, and each tile preserves its 3:2 aspect ratio.
  //
  // The test runs on a deliberately WIDE viewport (1600x900) to verify
  // tiles stay left-justified and maintain proper aspect ratio.
  // ──────────────────────────────────────────────────────────────────────
  test("right panel left-justifies tiles with 3:2 footprint on wide viewport (HCL #3+#4) @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_panel_left_justify_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSLeftJustHost",
      "SSLeftJustGuest",
      { mockDisplayMedia: true },
    );

    try {
      // Wide viewport: pre-fix, this maximizes the centering surplus.
      await hostPage.setViewportSize({ width: 1600, height: 900 });
      await hostPage.waitForTimeout(3000);

      const shareActivated = await startScreenShare(guestPage, hostPage);
      if (!shareActivated) {
        test.skip(true, "Screen share could not be auto-accepted; mockDisplayMedia not effective.");
        return;
      }

      // Let the split layout settle.
      await hostPage.waitForTimeout(2000);

      const rightPanel = hostPage.locator("#grid-container > div:nth-child(3)");
      await expect(rightPanel).toBeVisible({ timeout: 10_000 });

      // ── Layout is CSS-class-driven (.ss-peer-panel) using CSS grid.
      // Verify the panel has the right class and computed grid layout.
      await expect(rightPanel).toHaveClass(/ss-peer-panel/);
      expect(await rightPanel.evaluate((el) => getComputedStyle(el).display)).toBe("grid");

      // ── Bug #3 GEOMETRIC: the first tile's LEFT edge must sit at the
      // panel's left edge plus its padding (~6px), not centered.
      const firstTile = hostPage.locator(".split-peer-tile").first();
      await expect(firstTile).toBeVisible({ timeout: 10_000 });
      const offsets = await firstTile.evaluate((tile) => {
        const panel = tile.closest("#grid-container > div:nth-child(3)") as HTMLElement;
        const tr = tile.getBoundingClientRect();
        const pr = panel.getBoundingClientRect();
        return { tileLeft: tr.left, panelLeft: pr.left, panelWidth: pr.width };
      });
      const leftInset = offsets.tileLeft - offsets.panelLeft;
      // Padding is `6px`; allow 4-20px for sub-pixel + scrollbar.
      expect(leftInset).toBeGreaterThanOrEqual(0);
      expect(leftInset).toBeLessThanOrEqual(20);
      // The pre-fix CENTERED tile would sit at panel midpoint ≫ 20px in.
      // (We don't compare to half-width because we'd need to know the cell
      // surplus; the absolute bound above is sufficient.)

      // ── Bug #4 GEOMETRIC: each tile's bounding-box aspect must be ~3:2.
      const tiles = hostPage.locator(".split-peer-tile");
      const tileCount = await tiles.count();
      expect(tileCount).toBeGreaterThan(0);
      for (let i = 0; i < tileCount; i++) {
        const dims = await tiles.nth(i).evaluate((el) => {
          const r = el.getBoundingClientRect();
          return { w: r.width, h: r.height };
        });
        if (dims.w === 0 || dims.h === 0) continue;
        const aspect = dims.w / dims.h;
        // 3:2 = 1.5; allow 7% tolerance for borders + sub-pixel rounding.
        expect(aspect).toBeGreaterThan(1.5 * 0.93);
        expect(aspect).toBeLessThan(1.5 * 1.07);
      }
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 5. Layout switches back to normal grid when screen sharing stops
  //
  // When screen sharing is active the meeting uses a split layout
  // (.split-screen-tile + .split-peer-tile). When the sharer clicks the
  // "Stop Screen Share" button, the meeting must revert to the normal
  // CSS-grid layout with .grid-item tiles inside #grid-container and no
  // split-layout artifacts remaining.
  // ──────────────────────────────────────────────────────────────────────
  test("layout reverts to normal grid when screen sharing stops", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_panel_switchback_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSSwitchHost",
      "SSSwitchGuest",
      { mockDisplayMedia: true },
    );

    try {
      // Give time for WebSocket/WebTransport peer discovery
      await hostPage.waitForTimeout(3000);

      // Guest starts screen sharing
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

      // ---- ASSERT: split layout is active on host's view ----
      await expect(hostPage.locator(".split-screen-tile")).toBeVisible({ timeout: 10_000 });
      await expect(hostPage.locator(".split-peer-tile").first()).toBeVisible({ timeout: 10_000 });

      // ---- ACT: guest stops screen sharing ----
      // Wake the auto-hidden controls bar on the guest page.
      await guestPage.mouse.move(400, 400);
      await guestPage.waitForTimeout(300);
      await guestPage.locator(".video-controls-container").hover();
      await guestPage.waitForTimeout(500);

      const stopButton = guestPage.locator("button.video-control-button", {
        has: guestPage.locator(".tooltip", { hasText: /Stop.*Shar/ }),
      });
      await expect(stopButton).toBeVisible({ timeout: 10_000 });
      await stopButton.click();

      // ---- ASSERT: split layout disappears, normal grid is restored ----
      // Wait for the split-screen-tile to disappear from the host's view.
      await expect(hostPage.locator(".split-screen-tile")).toHaveCount(0, { timeout: 15_000 });
      await expect(hostPage.locator(".split-peer-tile")).toHaveCount(0, { timeout: 10_000 });

      // Normal grid tiles should be visible again.
      const gridItems = hostPage.locator("#grid-container .grid-item");
      await expect(gridItems.first()).toBeVisible({ timeout: 10_000 });

      // The #grid-container should use normal CSS grid properties.
      const containerStyle = await hostPage.locator("#grid-container").getAttribute("style");
      expect(containerStyle).toBeTruthy();
      expect(containerStyle).toContain("grid-template-columns");
      expect(containerStyle).toContain("grid-template-rows");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 6. All tiles same size (CSS grid uniformity)
  //
  // The CSS grid layout uses `repeat(auto-fill, minmax(160px, 1fr))`
  // which guarantees all grid cells are the same width, and
  // `aspect-ratio: 3/2` on each tile guarantees uniform height.
  // With mock peers we verify that all visible `.split-peer-tile`
  // elements have the same width and height within 1px tolerance.
  // ──────────────────────────────────────────────────────────────────────
  test("all split-peer-tiles have uniform dimensions during screen share", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_panel_uniform_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSUniformHost",
      "SSUniformGuest",
    );

    try {
      await hostPage.waitForTimeout(3000);

      const mockButton = hostPage.locator("button.video-control-button", {
        has: hostPage.locator(".tooltip", { hasText: /Mock Peers/i }),
      });
      const mockPeersAvailable = await mockButton.isVisible().catch(() => false);
      if (!mockPeersAvailable) {
        test.skip(true, 'Mock peers not enabled. Set mockPeersEnabled: "true" in config.js.');
        return;
      }

      // Add 6 mock peers — enough for 2+ rows in a 2-column grid.
      await addMockPeers(hostPage, 6);
      await hostPage.waitForTimeout(2000);

      const shareActivated = await startScreenShare(guestPage, hostPage);
      if (!shareActivated) {
        test.skip(true, "Screen share could not be auto-accepted.");
        return;
      }
      await hostPage.waitForTimeout(3000);

      const tiles = hostPage.locator(".split-peer-tile");
      const tileCount = await tiles.count();
      expect(tileCount).toBeGreaterThanOrEqual(6);

      // Collect bounding box dimensions for all visible tiles.
      const dimensions: { w: number; h: number }[] = [];
      for (let i = 0; i < tileCount; i++) {
        const box = await tiles.nth(i).boundingBox();
        if (box && box.width > 0 && box.height > 0) {
          dimensions.push({ w: box.width, h: box.height });
        }
      }
      expect(dimensions.length).toBeGreaterThanOrEqual(4);

      // All tiles must have the same width and height within 1px.
      const refW = dimensions[0].w;
      const refH = dimensions[0].h;
      for (let i = 1; i < dimensions.length; i++) {
        expect(Math.abs(dimensions[i].w - refW)).toBeLessThanOrEqual(1);
        expect(Math.abs(dimensions[i].h - refH)).toBeLessThanOrEqual(1);
      }
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 7. Gaps between adjacent tiles
  //
  // The `.ss-peer-panel` CSS grid has `gap: 10px`. Adjacent tiles in the
  // same row must have at least 8px of horizontal space between them
  // (allowing for sub-pixel rounding). This ensures tiles never visually
  // touch each other.
  // ──────────────────────────────────────────────────────────────────────
  test("adjacent tiles have gaps between them during screen share", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_panel_gaps_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSGapsHost",
      "SSGapsGuest",
    );

    try {
      await hostPage.waitForTimeout(3000);

      const mockButton = hostPage.locator("button.video-control-button", {
        has: hostPage.locator(".tooltip", { hasText: /Mock Peers/i }),
      });
      const mockPeersAvailable = await mockButton.isVisible().catch(() => false);
      if (!mockPeersAvailable) {
        test.skip(true, 'Mock peers not enabled. Set mockPeersEnabled: "true" in config.js.');
        return;
      }

      // Add 6 mock peers to get multiple columns.
      await addMockPeers(hostPage, 6);
      await hostPage.waitForTimeout(2000);

      const shareActivated = await startScreenShare(guestPage, hostPage);
      if (!shareActivated) {
        test.skip(true, "Screen share could not be auto-accepted.");
        return;
      }
      await hostPage.waitForTimeout(3000);

      // Collect bounding boxes of all visible tiles.
      const tiles = hostPage.locator(".split-peer-tile");
      const tileCount = await tiles.count();
      expect(tileCount).toBeGreaterThanOrEqual(4);

      const boxes: { left: number; right: number; top: number; bottom: number }[] = [];
      for (let i = 0; i < tileCount; i++) {
        const box = await tiles.nth(i).boundingBox();
        if (box && box.width > 0) {
          boxes.push({
            left: box.x,
            right: box.x + box.width,
            top: box.y,
            bottom: box.y + box.height,
          });
        }
      }

      // Find pairs of tiles in the same row (overlapping Y ranges) and
      // verify horizontal gap >= 8px. The CSS gap is 10px; 8px allows
      // for sub-pixel rounding.
      let horizontalPairsChecked = 0;
      for (let i = 0; i < boxes.length; i++) {
        for (let j = i + 1; j < boxes.length; j++) {
          const a = boxes[i];
          const b = boxes[j];
          // Same row: vertical overlap > 50% of tile height.
          const overlapY = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
          const tileH = a.bottom - a.top;
          if (overlapY > tileH * 0.5) {
            // These tiles are in the same row — check horizontal gap.
            const gap = Math.abs(a.left > b.right ? a.left - b.right : b.left - a.right);
            expect(gap).toBeGreaterThanOrEqual(8);
            horizontalPairsChecked++;
          }
        }
      }

      // Find pairs of tiles in the same column (overlapping X ranges) and
      // verify vertical gap >= 8px.
      let verticalPairsChecked = 0;
      for (let i = 0; i < boxes.length; i++) {
        for (let j = i + 1; j < boxes.length; j++) {
          const a = boxes[i];
          const b = boxes[j];
          const overlapX = Math.min(a.right, b.right) - Math.max(a.left, b.left);
          const tileW = a.right - a.left;
          if (overlapX > tileW * 0.5) {
            // Same column — check vertical gap only for adjacent rows.
            const vGap = Math.abs(a.top > b.bottom ? a.top - b.bottom : b.top - a.bottom);
            if (vGap < 50) {
              // Adjacent rows (gap < 50px means they are neighbors).
              expect(vGap).toBeGreaterThanOrEqual(8);
              verticalPairsChecked++;
            }
          }
        }
      }

      // We must have found at least one pair in a multi-column/multi-row
      // grid to make the gap assertion meaningful.
      expect(horizontalPairsChecked + verticalPairsChecked).toBeGreaterThan(0);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 8. Tile width fills the grid cell
  //
  // Each `.split-peer-tile` has `width: 100%` so it fills the full grid
  // cell width. The tile width should be approximately
  // (panel_content_width - gaps) / columns. This test verifies tiles
  // consume the available panel width minus padding and gaps.
  // ──────────────────────────────────────────────────────────────────────
  test("tiles fill the available grid cell width during screen share", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_panel_fill_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSFillHost",
      "SSFillGuest",
      { mockDisplayMedia: true },
    );

    try {
      await hostPage.waitForTimeout(3000);

      const shareActivated = await startScreenShare(guestPage, hostPage);
      if (!shareActivated) {
        test.skip(true, "Screen share could not be auto-accepted.");
        return;
      }
      await hostPage.waitForTimeout(2000);

      const panel = hostPage.locator(".ss-peer-panel");
      await expect(panel).toBeVisible({ timeout: 10_000 });

      const tiles = hostPage.locator(".split-peer-tile");
      const tileCount = await tiles.count();
      expect(tileCount).toBeGreaterThan(0);

      // Get the panel content width and tile width.
      const metrics = await panel.evaluate((el) => {
        const cs = getComputedStyle(el);
        const paddingLeft = parseFloat(cs.paddingLeft) || 0;
        const paddingRight = parseFloat(cs.paddingRight) || 0;
        const gap = parseFloat(cs.columnGap) || parseFloat(cs.gap) || 0;
        const contentWidth = el.clientWidth - paddingLeft - paddingRight;
        // Count resolved grid columns from computed style.
        const cols = cs.gridTemplateColumns.split(/\s+/).filter((s) => s.length > 0).length;
        return { contentWidth, gap, cols };
      });

      const firstTile = tiles.first();
      const tileBox = await firstTile.boundingBox();
      expect(tileBox).not.toBeNull();

      // Expected tile width: (contentWidth - (cols-1)*gap) / cols
      const expectedTileWidth =
        (metrics.contentWidth - (metrics.cols - 1) * metrics.gap) / metrics.cols;

      // Allow 2px tolerance for sub-pixel rounding.
      expect(Math.abs(tileBox!.width - expectedTileWidth)).toBeLessThanOrEqual(2);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 9. Name truncation: long usernames do not overflow into icon area
  //
  // The `.floating-name` label uses `max-width: calc(100% - 90px)` with
  // `white-space: nowrap; text-overflow: ellipsis; overflow: hidden` to
  // prevent long names from overflowing into the mute/signal icon area
  // on the right side of the tile.
  // ──────────────────────────────────────────────────────────────────────
  test("long usernames are truncated and do not overflow tile icons", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_panel_trunc_${Date.now()}`;

    // Use a very long guest name to test truncation.
    const longName = "AVeryLongDisplayNameThatShouldDefinitelyBeTruncatedByCSS";
    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSTruncHost",
      longName,
      { mockDisplayMedia: true },
    );

    try {
      await hostPage.waitForTimeout(3000);

      const shareActivated = await startScreenShare(guestPage, hostPage);
      if (!shareActivated) {
        test.skip(true, "Screen share could not be auto-accepted.");
        return;
      }
      await hostPage.waitForTimeout(2000);

      // Find the .floating-name element within a split-peer-tile.
      const floatingNames = hostPage.locator(".split-peer-tile .floating-name");
      const nameCount = await floatingNames.count();
      expect(nameCount).toBeGreaterThan(0);

      for (let i = 0; i < nameCount; i++) {
        const nameEl = floatingNames.nth(i);
        const isVisible = await nameEl.isVisible();
        if (!isVisible) continue;

        // Verify overflow is hidden (the real truncation contract).
        // Note: text-overflow:ellipsis is declared but has no visual
        // effect on display:inline-flex elements — the geometric check
        // below is the authoritative truncation assertion.
        const styles = await nameEl.evaluate((el) => {
          const cs = getComputedStyle(el);
          return { overflow: cs.overflow };
        });
        expect(styles.overflow).toBe("hidden");

        // Verify the floating-name does not extend beyond the tile's
        // right edge minus icon area (~90px). The name's right edge must
        // not reach the tile's right edge.
        const overflow = await nameEl.evaluate((el) => {
          const tile = el.closest(".split-peer-tile") as HTMLElement;
          if (!tile) return { nameRight: 0, tileRight: 0 };
          const nr = el.getBoundingClientRect();
          const tr = tile.getBoundingClientRect();
          return { nameRight: nr.right, tileRight: tr.right };
        });

        // The name's right edge should leave room for icons (~90px area).
        // Allow some tolerance but the name must not reach the tile edge.
        if (overflow.tileRight > 0) {
          const rightMargin = overflow.tileRight - overflow.nameRight;
          // There should be at least 40px of space for icons (the CSS
          // reserves 90px, but the actual icon width varies).
          expect(rightMargin).toBeGreaterThanOrEqual(40);
        }
      }
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});

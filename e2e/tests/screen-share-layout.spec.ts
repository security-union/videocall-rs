import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Screen-share split-layout E2E tests.
 *
 * The split layout is rendered when any peer is sharing their screen:
 *  - Left panel: the active screen-share tile (TileMode::ScreenOnly)
 *  - Resize handle: `.screen-share-resize-handle`
 *  - Right panel: a CSS-grid of compact peer tiles
 *
 * Key behaviors introduced in the layout update:
 *
 *  1. FIXED TILE HEIGHT: Tiles in the right panel always have a height
 *     computed to fit exactly 4 tiles per column, regardless of how far
 *     the resize handle has been dragged. The grid row size is encoded as
 *     `grid-auto-rows: <N>px` (a fixed pixel value, never `auto` or `%`).
 *
 *  2. SINGLE-COLUMN SWITCH: When the participant panel width is 25% or
 *     less of the total screen (`right_ratio <= 0.25`), the right panel
 *     switches to `grid-template-columns: 1fr` (single column). Above
 *     that threshold it uses `grid-template-columns: 1fr 1fr`.
 *
 *  3. TILE HEIGHT STYLE: Each tile inside the right panel uses
 *     `height: 100%` (filling the grid row) instead of `aspect-ratio:
 *     16/9` — so tile height is determined by the fixed grid row, not by
 *     the column width.
 *
 * AUTOMATION LIMITATION
 * ─────────────────────
 * Activating screen share requires `getDisplayMedia()`, which opens a
 * native OS-level picker that Playwright cannot drive programmatically.
 * The `--use-fake-device-for-media-stream` / `--use-fake-ui-for-media-stream`
 * browser flags fake camera and microphone only — they do not stub the
 * display capture path.
 *
 * Tests that require an active screen-share session (behaviors 1–3 above)
 * are therefore structurally documented below with a `test.skip()` guard.
 * Each skipped test includes the exact CSS assertions that would be run
 * once display-capture automation becomes available (e.g., via a Chrome
 * `--auto-select-desktop-capture-source` flag in a headed environment with
 * a running display server, or via a future Playwright display-capture API).
 *
 * The tests that CAN run today verify the *absence* of split-layout
 * elements in a normal (no-screen-share) meeting, confirming the baseline
 * state against which the screen-share layout changes are measured.
 *
 * TODO: Track automation unblocking. Options to revisit:
 *   - Chrome --auto-select-desktop-capture-source=<window-title> in a headed
 *     environment with a running display server (e.g. Xvfb in CI).
 *   - Playwright display-capture API if/when it ships upstream.
 *   - A mock getDisplayMedia shim injected via page.addInitScript() that
 *     returns a synthetic MediaStream from a canvas element.
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

// HCL follow-up #944: synthetic getDisplayMedia mock — injected via
// `addInitScript` so screen-share tests can run reliably in headless
// Chromium without a system-level picker. Mirrors the pattern used in
// `screen-share-panel.spec.ts`.
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

  const peerTile = hostPage.locator("#grid-container .grid-item");
  await expect(peerTile.first()).toBeVisible({ timeout: 30_000 });

  return { hostPage, guestPage, browser1, browser2 };
}

test.describe("Screen-share split-layout", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // ──────────────────────────────────────────────────────────────────────
  // 1. Baseline: no split-layout elements present in a normal meeting
  //
  // Verifies that when nobody is screen-sharing the grid container uses
  // the normal CSS grid layout, not the flex split layout, and that no
  // split-layout-specific DOM elements exist.
  // ──────────────────────────────────────────────────────────────────────
  test("grid-container uses CSS grid layout when no screen share is active", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_layout_grid_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSLayoutHost",
      "SSLayoutGuest",
    );

    try {
      const container = hostPage.locator("#grid-container");
      await expect(container).toBeVisible({ timeout: 10_000 });

      const style = await container.getAttribute("style");
      expect(style).toBeTruthy();

      // Normal grid layout: must declare display:grid
      expect(style).toContain("display: grid");

      // Split layout resets flex props explicitly when transitioning back;
      // in the normal grid path those props are not set to flex values.
      expect(style).not.toContain("flex-direction: row");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("resize handle and split-screen tiles are absent when no screen share is active", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_layout_absent_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSAbsHost",
      "SSAbsGuest",
    );

    try {
      // No resize handle
      await expect(hostPage.locator(".screen-share-resize-handle")).toHaveCount(0);

      // No split-screen tiles (TileMode::ScreenOnly renders with this class)
      await expect(hostPage.locator(".split-screen-tile")).toHaveCount(0);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 2. Fixed tile height in the participant panel (right side)
  //
  // LIMITATION: Cannot be fully automated. Triggering screen share
  // requires getDisplayMedia(), which opens a native OS-level picker that
  // Playwright cannot drive. The `--use-fake-device-for-media-stream` and
  // `--use-fake-ui-for-media-stream` Chrome flags do not stub the display
  // capture path.
  //
  // WHAT THIS TEST WOULD VERIFY (once screen-share automation is available):
  //   - After a peer starts sharing their screen, the right panel
  //     (`.screen-share-resize-handle` + sibling div) is rendered.
  //   - The right panel's `grid-auto-rows` style is a fixed pixel value
  //     (e.g. "grid-auto-rows: 148px"), not "auto" or a percentage.
  //   - Dragging the `.screen-share-resize-handle` left or right does NOT
  //     change the `grid-auto-rows` value, confirming tile height is
  //     independent of the resize position.
  //
  // IMPLEMENTATION NOTE (from attendants.rs):
  //   const SS_BOTTOM_PAD: f64 = 80.0;
  //   const SS_VERT_PAD:   f64 = 28.0;
  //   let ss_avail_h = vh - SS_BOTTOM_PAD - SS_VERT_PAD;
  //   let ss_tile_h  = ((ss_avail_h - 3.0 * SS_GRID_GAP) / 4.0).max(40.0);
  //   // Right panel: grid-auto-rows: {ss_tile_h:.0}px
  //
  // To assert in a future automated test:
  //   const rightPanel = page.locator("#grid-container > div").nth(2);
  //   const rightStyle = await rightPanel.getAttribute("style");
  //   expect(rightStyle).toMatch(/grid-auto-rows:\s*\d+px/);
  //   // Verify the value stays constant before and after dragging the handle:
  //   const rowsBefore = extractGridAutoRows(rightStyle);
  //   await dragResizeHandle(page, deltaX);
  //   const rightStyleAfter = await rightPanel.getAttribute("style");
  //   expect(extractGridAutoRows(rightStyleAfter)).toBe(rowsBefore);
  // ──────────────────────────────────────────────────────────────────────
  // HCL follow-up #944: this test is no longer skipped — the synthetic
  // `MOCK_GET_DISPLAY_MEDIA_SCRIPT` defined above (and the
  // `setupTwoUserMeeting({ mockDisplayMedia: true })` opt-in) lets us
  // trigger a screen share programmatically without the OS-level picker.
  test("right panel tiles have fixed height independent of resize handle position", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_layout_fixed_height_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSFixHeightHost",
      "SSFixHeightGuest",
      { mockDisplayMedia: true },
    );

    try {
      await expect(hostPage.locator("#grid-container .grid-item")).toHaveCount(1, {
        timeout: 30_000,
      });

      // Guest starts the mocked screen share.
      await guestPage.mouse.move(400, 400);
      await guestPage.waitForTimeout(300);
      const shareButton = guestPage.locator("button.video-control-button", {
        has: guestPage.locator(".tooltip", { hasText: "Share Screen" }),
      });
      await expect(shareButton).toBeVisible({ timeout: 10_000 });
      await shareButton.click();

      // Host sees the split-screen tile appear.
      await expect(hostPage.locator(".split-screen-tile")).toBeVisible({ timeout: 15_000 });

      // The right panel is the 3rd direct child of #grid-container
      // (left split-screen + handle + right grid). It carries the
      // `grid-auto-rows: <N>px` inline style we want to lock in.
      const rightPanel = hostPage.locator("#grid-container > div").nth(2);
      const styleBefore = await rightPanel.getAttribute("style");
      expect(styleBefore).toMatch(/grid-auto-rows:\s*\d+px/);
      const rowsBefore = (styleBefore?.match(/grid-auto-rows:\s*(\d+)px/) ?? [])[1];

      // Drag the resize handle ~100px to the right so screen_share_ratio
      // changes. The split layout reflows but grid-auto-rows must NOT
      // change — tile height is sized to fit 4-per-column on the
      // available viewport, independent of the resize position.
      const handle = hostPage.locator(".screen-share-resize-handle");
      await expect(handle).toBeVisible({ timeout: 5_000 });
      const handleBox = await handle.boundingBox();
      if (!handleBox) {
        throw new Error("resize handle has no bounding box");
      }
      const startX = handleBox.x + handleBox.width / 2;
      const startY = handleBox.y + handleBox.height / 2;
      await hostPage.mouse.move(startX, startY);
      await hostPage.mouse.down();
      await hostPage.mouse.move(startX + 100, startY, { steps: 10 });
      await hostPage.mouse.up();
      await hostPage.waitForTimeout(300);

      const styleAfter = await rightPanel.getAttribute("style");
      const rowsAfter = (styleAfter?.match(/grid-auto-rows:\s*(\d+)px/) ?? [])[1];
      expect(rowsAfter).toBe(rowsBefore);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 3. Single-column switch when screen share occupies ≥ 75% of screen
  //
  // LIMITATION: Same as test 2 — requires an active screen-share session.
  //
  // WHAT THIS TEST WOULD VERIFY (once screen-share automation is available):
  //   - Default split (screen_share_ratio = 0.667 → right_ratio = 0.333):
  //     right panel `grid-template-columns` is "repeat(2, <ss_tile_w>px)"
  //     (two fixed-width tile-sized columns, HCL #3/#4 fix).
  //   - After dragging the resize handle so screen share occupies ≥ 75%
  //     (right_ratio ≤ 0.25):
  //     right panel `grid-template-columns` is "<ss_tile_w>px" (single column).
  //   - Dragging back below the threshold restores the 2-column form.
  //
  // IMPLEMENTATION NOTE (from attendants.rs):
  //   let right_ratio = 1.0 - screen_share_ratio();
  //   let ss_cols = if right_ratio <= 0.25 || ss_panel_width < 180.0 {
  //       1.0   // single column
  //   } else {
  //       2.0   // two columns
  //   };
  //   let ss_tile_w = (ss_tile_h * TILE_AR).round(); // 3:2 footprint
  //   // Right panel: grid-template-columns:
  //   //   ss_cols > 1.0 → format!("repeat(2, {ss_tile_w:.0}px)")
  //   //   else          → format!("{ss_tile_w:.0}px")
  //   // Plus `justify-content: start` so tiles pack to the left edge.
  //
  // The resize handle is constrained to [0.3, 0.85] (screen_share_ratio),
  // so right_ratio spans [0.15, 0.70]. The single-column threshold at 0.25
  // is reachable by dragging screen share to cover ≥ 75% of the container.
  //
  // To assert in a future automated test:
  //   // Default: two fixed-width columns
  //   const rightPanel = page.locator("#grid-container > div").nth(2);
  //   let style = await rightPanel.getAttribute("style");
  //   expect(style).toMatch(/grid-template-columns:\s*repeat\(2,\s*\d+px\)/);
  //   expect(style).toContain("justify-content: start");
  //   // Drag handle far left (screen share ratio → 0.85, right_ratio → 0.15)
  //   await dragResizeHandle(page, -largeOffset);
  //   style = await rightPanel.getAttribute("style");
  //   expect(style).toMatch(/grid-template-columns:\s*\d+px\b/);
  // ──────────────────────────────────────────────────────────────────────
  // HCL follow-up #944: no longer skipped, same rationale as the
  // fixed-height test above.
  test("right panel switches to single column when screen share occupies >= 75%", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_layout_single_col_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSSingleColHost",
      "SSSingleColGuest",
      { mockDisplayMedia: true },
    );

    try {
      await expect(hostPage.locator("#grid-container .grid-item")).toHaveCount(1, {
        timeout: 30_000,
      });

      // Guest triggers screen share.
      await guestPage.mouse.move(400, 400);
      await guestPage.waitForTimeout(300);
      const shareButton = guestPage.locator("button.video-control-button", {
        has: guestPage.locator(".tooltip", { hasText: "Share Screen" }),
      });
      await expect(shareButton).toBeVisible({ timeout: 10_000 });
      await shareButton.click();

      // Split-layout activates on host.
      await expect(hostPage.locator(".split-screen-tile")).toBeVisible({ timeout: 15_000 });

      const rightPanel = hostPage.locator("#grid-container > div").nth(2);

      // Default split (screen_share_ratio = 0.667 → right_ratio = 0.333):
      // grid-template-columns is `repeat(2, <px>px)` (two fixed-width
      // tile-sized columns from the PR #940 HCL #3/#4 fix).
      const styleDefault = await rightPanel.getAttribute("style");
      expect(styleDefault).toMatch(/grid-template-columns:\s*repeat\(2,\s*\d+px\)/);

      // Drag the resize handle toward the right edge so screen_share_ratio
      // approaches 0.85 (right_ratio → 0.15). At right_ratio <= 0.25 the
      // layout collapses to a single column.
      const handle = hostPage.locator(".screen-share-resize-handle");
      const handleBox = await handle.boundingBox();
      const viewport = hostPage.viewportSize();
      if (!handleBox || !viewport) {
        throw new Error("resize handle or viewport unavailable");
      }
      const startX = handleBox.x + handleBox.width / 2;
      const startY = handleBox.y + handleBox.height / 2;
      const targetX = Math.floor(viewport.width * 0.85);
      await hostPage.mouse.move(startX, startY);
      await hostPage.mouse.down();
      await hostPage.mouse.move(targetX, startY, { steps: 20 });
      await hostPage.mouse.up();
      await hostPage.waitForTimeout(500);

      // Single-column form: grid-template-columns is a single `<px>px`
      // entry, NOT `repeat(2, ...)`.
      const styleNarrow = await rightPanel.getAttribute("style");
      expect(styleNarrow).toMatch(/grid-template-columns:\s*\d+px/);
      expect(styleNarrow).not.toMatch(/grid-template-columns:\s*repeat\(2,/);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});

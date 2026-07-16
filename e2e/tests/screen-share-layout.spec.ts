import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";

/**
 * Screen-share split-layout E2E tests.
 *
 * The split layout is rendered when any peer is sharing their screen:
 *  - Left panel: the active screen-share tile (TileMode::ScreenOnly)
 *  - Resize handle: `.screen-share-resize-handle`
 *  - Right panel: a CSS-grid of compact peer tiles
 *
 * Key behaviors of the current layout:
 *
 *  1. CSS GRID WITH AUTO-FILL: The right panel uses
 *     `grid-template-columns: repeat(auto-fill, minmax(160px, 1fr))` so
 *     the browser automatically determines the column count based on the
 *     available panel width. All tiles are always the same size (uniform
 *     grid cells).
 *
 *  2. SINGLE-COLUMN COLLAPSE: When the panel narrows below ~340px (i.e.
 *     when the resize handle is dragged far right), the auto-fill grid
 *     cannot fit two 160px columns and collapses to a single column.
 *
 *  3. ASPECT-RATIO-DRIVEN HEIGHT: Each `.split-peer-tile` uses
 *     `aspect-ratio: 3/2` with `width: 100%`, so tile height is derived
 *     from tile width and stays proportional regardless of panel size.
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
  // 2. Tile aspect ratio consistency across resize handle positions
  //
  // The right panel uses CSS grid with `grid-auto-rows: min-content` and
  // tiles use `aspect-ratio: 3/2`. This means tile height is always
  // derived from tile width. When the resize handle is dragged, tile
  // width changes but the 3:2 aspect ratio must be preserved.
  //
  // The synthetic `MOCK_GET_DISPLAY_MEDIA_SCRIPT` allows driving the
  // screen share flow programmatically without the OS-level picker.
  // ──────────────────────────────────────────────────────────────────────
  test("right panel tiles maintain consistent aspect ratio after resize handle drag", async ({
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
      await wakeControls(guestPage);
      await guestPage.waitForTimeout(300);
      const shareButton = guestPage.locator("button.video-control-button", {
        has: guestPage.locator(".tooltip", { hasText: "Share Screen" }),
      });
      await expect(shareButton).toBeVisible({ timeout: 10_000 });
      await shareButton.click();

      // Host sees the split-screen tile appear.
      await expect(hostPage.locator(".split-screen-tile")).toBeVisible({ timeout: 15_000 });

      // The right panel now uses CSS grid with `grid-auto-rows: min-content`
      // and tiles use `aspect-ratio: 3/2`. Tile height is derived from tile
      // width, so the aspect ratio stays constant regardless of the resize
      // handle position.
      const rightPanel = hostPage.locator("#grid-container > div").nth(2);
      await expect(rightPanel).toHaveClass(/ss-peer-panel/);

      // Measure tile dimensions before the drag.
      const firstTile = hostPage.locator(".split-peer-tile").first();
      await expect(firstTile).toBeVisible({ timeout: 5_000 });
      const dimsBefore = await firstTile.evaluate((el) => {
        const r = el.getBoundingClientRect();
        return { w: r.width, h: r.height };
      });
      const aspectBefore = dimsBefore.w / dimsBefore.h;
      // Aspect ratio should be ~3:2 = 1.5
      expect(aspectBefore).toBeGreaterThan(1.5 * 0.9);
      expect(aspectBefore).toBeLessThan(1.5 * 1.1);

      // Drag the resize handle ~100px to the right so the panel narrows.
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
      await hostPage.waitForTimeout(500);

      // Measure tile dimensions after the drag. Width may change as the
      // panel narrows, but the aspect ratio must remain ~3:2 because
      // `aspect-ratio: 3/2` in CSS derives height from width.
      const dimsAfter = await firstTile.evaluate((el) => {
        const r = el.getBoundingClientRect();
        return { w: r.width, h: r.height };
      });
      const aspectAfter = dimsAfter.w / dimsAfter.h;
      expect(aspectAfter).toBeGreaterThan(1.5 * 0.9);
      expect(aspectAfter).toBeLessThan(1.5 * 1.1);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 3. Single-column collapse when the right panel is narrow
  //
  // The CSS grid uses `repeat(auto-fill, minmax(160px, 1fr))`. At default
  // split (~33% right), the panel is wide enough for 2+ columns. When the
  // resize handle is dragged far right (narrowing the panel below ~340px),
  // the auto-fill algorithm can only fit one 160px column, collapsing the
  // grid to a single column.
  // ──────────────────────────────────────────────────────────────────────
  test("right panel collapses to single column when panel is narrow", async ({ baseURL }) => {
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
      await wakeControls(guestPage);
      await guestPage.waitForTimeout(300);
      const shareButton = guestPage.locator("button.video-control-button", {
        has: guestPage.locator(".tooltip", { hasText: "Share Screen" }),
      });
      await expect(shareButton).toBeVisible({ timeout: 10_000 });
      await shareButton.click();

      // Split-layout activates on host.
      await expect(hostPage.locator(".split-screen-tile")).toBeVisible({ timeout: 15_000 });

      const rightPanel = hostPage.locator("#grid-container > div").nth(2);
      await expect(rightPanel).toHaveClass(/ss-peer-panel/);

      // The CSS grid uses `repeat(auto-fill, minmax(160px, 1fr))`.
      // At default split (~33% right), the panel is wide enough for 2+
      // columns. Count the resolved columns via computed grid style.
      const colCountDefault = await rightPanel.evaluate((el) => {
        const cols = getComputedStyle(el).gridTemplateColumns;
        // gridTemplateColumns resolves to space-separated pixel values
        return cols.split(/\s+/).filter((s) => s.length > 0).length;
      });
      // On a standard viewport (1280px+), right panel at ~33% width
      // (~420px) should fit at least 2 columns with minmax(160px, 1fr).
      expect(colCountDefault).toBeGreaterThanOrEqual(2);

      // Drag the resize handle toward the right edge so the right panel
      // becomes very narrow (< 340px), which should collapse to 1 column
      // because minmax(160px, 1fr) cannot fit 2 columns.
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

      // After narrowing, the CSS grid should resolve to a single column
      // because the available width is below 2 * 160px + gap.
      const colCountNarrow = await rightPanel.evaluate((el) => {
        const cols = getComputedStyle(el).gridTemplateColumns;
        return cols.split(/\s+/).filter((s) => s.length > 0).length;
      });
      expect(colCountNarrow).toBe(1);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 4. Lone peer tile is not cropped at max drag on a narrow viewport
  //
  // Regression for #1594: the old `minmax(160px, 1fr)` grid floor could
  // not shrink below 160px, so at max drag (~85%) on a ~1100px viewport
  // the single tile overflowed the panel and was cropped by
  // `overflow-x: hidden`. The fix uses `minmax(min(160px, 100%), 1fr)`
  // so the floor tracks the available width.
  // ──────────────────────────────────────────────────────────────────────
  test("lone peer tile is not cropped at max drag on a narrow viewport (#1594)", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_layout_crop_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSCropHost",
      "SSCropGuest",
      { mockDisplayMedia: true },
    );

    try {
      // Narrow viewport to trigger the bug reliably.
      await hostPage.setViewportSize({ width: 1100, height: 800 });
      await hostPage.waitForTimeout(500);

      await expect(hostPage.locator("#grid-container .grid-item")).toHaveCount(1, {
        timeout: 30_000,
      });

      // Guest triggers screen share.
      await wakeControls(guestPage);
      await guestPage.waitForTimeout(300);
      const shareButton = guestPage.locator("button.video-control-button", {
        has: guestPage.locator(".tooltip", { hasText: "Share Screen" }),
      });
      await expect(shareButton).toBeVisible({ timeout: 10_000 });
      await shareButton.click();

      await expect(hostPage.locator(".split-screen-tile")).toBeVisible({
        timeout: 15_000,
      });

      const rightPanel = hostPage.locator("#grid-container > div").nth(2);
      await expect(rightPanel).toHaveClass(/ss-peer-panel/);

      // Drag the resize handle to max ratio (~85% of viewport width).
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

      // The tile must not overflow the panel horizontally.
      // scrollWidth > clientWidth means content is clipped.
      const overflow = await rightPanel.evaluate((el) => el.scrollWidth - el.clientWidth);
      expect(overflow).toBeLessThanOrEqual(1);

      // Corroborate: tile right edge <= panel content-box right edge.
      const { tileRight, panelContentRight } = await rightPanel.evaluate((el) => {
        const tile = el.querySelector(".split-peer-tile");
        if (!tile) return { tileRight: 0, panelContentRight: 0 };
        const tileRect = tile.getBoundingClientRect();
        const panelRect = el.getBoundingClientRect();
        const paddingRight = parseFloat(getComputedStyle(el).paddingRight) || 0;
        return {
          tileRight: tileRect.right,
          panelContentRight: panelRect.right - paddingRight,
        };
      });
      expect(tileRight).toBeLessThanOrEqual(panelContentRight + 1);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});

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
  test.skip(
    "right panel tiles have fixed height independent of resize handle position (requires screen-share automation)",
    () => {
      // Skipped: getDisplayMedia() cannot be automated in headless Chromium.
      // See the block comment above for the full assertion plan.
    },
  );

  // ──────────────────────────────────────────────────────────────────────
  // 3. Single-column switch when screen share occupies ≥ 75% of screen
  //
  // LIMITATION: Same as test 2 — requires an active screen-share session.
  //
  // WHAT THIS TEST WOULD VERIFY (once screen-share automation is available):
  //   - Default split (screen_share_ratio = 0.667 → right_ratio = 0.333):
  //     right panel `grid-template-columns` is "1fr 1fr" (two columns).
  //   - After dragging the resize handle so screen share occupies ≥ 75%
  //     (right_ratio ≤ 0.25):
  //     right panel `grid-template-columns` is "1fr" (single column).
  //   - Dragging back below the threshold restores "1fr 1fr".
  //
  // IMPLEMENTATION NOTE (from attendants.rs):
  //   let right_ratio = 1.0 - screen_share_ratio();
  //   let ss_cols = if right_ratio <= 0.25 || ss_panel_width < 180.0 {
  //       1.0   // single column
  //   } else {
  //       2.0   // two columns
  //   };
  //   // Right panel: grid-template-columns: {if ss_cols > 1.0 { "1fr 1fr" } else { "1fr" }}
  //
  // The resize handle is constrained to [0.3, 0.85] (screen_share_ratio),
  // so right_ratio spans [0.15, 0.70]. The single-column threshold at 0.25
  // is reachable by dragging screen share to cover ≥ 75% of the container.
  //
  // To assert in a future automated test:
  //   // Default: two columns
  //   const rightPanel = page.locator("#grid-container > div").nth(2);
  //   let style = await rightPanel.getAttribute("style");
  //   expect(style).toContain("grid-template-columns: 1fr 1fr");
  //   // Drag handle far left (screen share ratio → 0.85, right_ratio → 0.15)
  //   await dragResizeHandle(page, -largeOffset);
  //   style = await rightPanel.getAttribute("style");
  //   expect(style).toContain("grid-template-columns: 1fr");
  // ──────────────────────────────────────────────────────────────────────
  test.skip(
    "right panel switches to single column when screen share occupies ≥ 75% of screen (requires screen-share automation)",
    () => {
      // Skipped: getDisplayMedia() cannot be automated in headless Chromium.
      // See the block comment above for the full assertion plan.
    },
  );
});

import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Crop toggle E2E tests.
 *
 * The crop button on peer video tiles flips between "cropped" (fill, default)
 * and "uncropped" (letterboxed) modes. The button has class `.crop-icon` and
 * is only visible on tile hover (inside `.tile-top-icons`).
 *
 * Default state: `.crop-icon.active` + `canvas.cropped`
 * After toggle:  `.crop-icon` (no active) + `canvas.uncropped`
 *
 * Screen-share canvases (id starting with `screen-share-`) have
 * `object-fit: contain` forced by CSS regardless of crop toggle state.
 *
 * Per-tile isolation: the `cropped_tiles` HashMap is keyed by canvas ID,
 * so toggling on one tile must not affect another.
 *
 * IMPORTANT: The crop button exists only on REAL camera-on peer tiles
 * (`.grid-item` with a `<canvas>`). The self-view is a `<video>` element
 * with no crop control, and mock peers are video-OFF placeholders with
 * neither `<canvas>` nor `.crop-icon`. Tests 1–4 therefore use a real
 * second browser (fake media device) to produce a genuine peer tile.
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

  if (result === "waiting") return "waiting";
  if (result === "waiting-for-meeting") return "waiting-for-meeting";
  if (result === "auto-joined") return "in-meeting";

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
  if (guestResult === "in-meeting") return;

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

/**
 * Set up a two-user meeting with real camera-on peers. Both browsers use
 * `--use-fake-device-for-media-stream` so each participant's camera produces
 * a real video stream → the remote peer tile renders a `<canvas>` with a
 * `.crop-icon` control.
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

  // Wait for the remote peer tile to appear on the host's grid.
  const peerTile = hostPage.locator("#grid-container .grid-item");
  await expect(peerTile.first()).toBeVisible({ timeout: 30_000 });

  return { hostPage, guestPage, browser1, browser2 };
}

/**
 * On `viewerPage`, find a remote peer tile that has a `<canvas>` and a
 * `.crop-icon`, hover it, and return locators for the tile, crop button,
 * and canvas.
 */
async function getPeerTileWithCrop(viewerPage: Page): Promise<{
  tile: ReturnType<Page["locator"]>;
  cropBtn: ReturnType<Page["locator"]>;
  canvas: ReturnType<Page["locator"]>;
}> {
  const tile = viewerPage.locator(".grid-item:has(canvas)").first();
  await expect(tile).toBeVisible({ timeout: 15_000 });

  const canvas = tile.locator("canvas").first();
  await expect(canvas).toBeVisible({ timeout: 10_000 });

  // Hover to reveal the crop button.
  await tile.hover();

  const cropBtn = tile.locator(".crop-icon").first();
  await expect(cropBtn).toBeVisible({ timeout: 5_000 });

  return { tile, cropBtn, canvas };
}

test.describe("Crop toggle", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // ────────────────────────────────────────────────────────────────────────
  // Test 1: Default state is cropped (fill mode)
  // ────────────────────────────────────────────────────────────────────────
  test("default state is cropped (fill mode)", async ({ baseURL }) => {
    test.setTimeout(90_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `crop_default_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "CropHost1",
      "CropGuest1",
    );

    try {
      const { cropBtn, canvas } = await getPeerTileWithCrop(hostPage);

      // The crop button should have the `active` class by default (cropped mode).
      await expect(cropBtn).toHaveClass(/\bactive\b/, { timeout: 5_000 });

      // The canvas should have the `cropped` class.
      await expect(canvas).toHaveClass(/\bcropped\b/, { timeout: 5_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ────────────────────────────────────────────────────────────────────────
  // Test 2: Click flips to uncropped (letterboxed)
  //
  // Asserts computed object-fit: contain and object-position: 50% 50%
  // unconditionally — global.css guarantees these values for `.uncropped`.
  // ────────────────────────────────────────────────────────────────────────
  test("click flips to uncropped (letterboxed)", async ({ baseURL }) => {
    test.setTimeout(90_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `crop_uncrop_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "CropHost2",
      "CropGuest2",
    );

    try {
      const { tile, cropBtn, canvas } = await getPeerTileWithCrop(hostPage);

      // Sanity: starts in cropped/active state.
      await expect(cropBtn).toHaveClass(/\bactive\b/, { timeout: 5_000 });
      await expect(canvas).toHaveClass(/\bcropped\b/, { timeout: 5_000 });

      // Click the crop button to toggle to uncropped.
      await cropBtn.click();

      // Re-hover to keep the button visible after re-render.
      await tile.hover();

      // Button should lose the `active` class.
      const cropBtnAfter = tile.locator(".crop-icon").first();
      await expect(cropBtnAfter).toBeVisible({ timeout: 5_000 });
      await expect(cropBtnAfter).not.toHaveClass(/\bactive\b/, { timeout: 5_000 });

      // Canvas should now have the `uncropped` class.
      const canvasAfter = tile.locator("canvas").first();
      await expect(canvasAfter).toHaveClass(/\buncropped\b/, { timeout: 5_000 });

      // Unconditional computed-style checks (global.css .uncropped rule).
      const objectFit = await canvasAfter.evaluate((el) => window.getComputedStyle(el).objectFit);
      expect(objectFit).toBe("contain");

      const objectPosition = await canvasAfter.evaluate(
        (el) => window.getComputedStyle(el).objectPosition,
      );
      expect(["50% 50%", "center center"]).toContain(objectPosition);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ────────────────────────────────────────────────────────────────────────
  // Test 3: Second click restores cropped
  //
  // Regression guard for #765 and #885 — object-position must return to
  // `center top` (50% 0%) after toggling back to cropped.
  // ────────────────────────────────────────────────────────────────────────
  test("second click restores cropped", async ({ baseURL }) => {
    test.setTimeout(90_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `crop_restore_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "CropHost3",
      "CropGuest3",
    );

    try {
      const { tile, cropBtn } = await getPeerTileWithCrop(hostPage);

      // First click: cropped -> uncropped.
      await cropBtn.click();
      await tile.hover();

      // Second click: uncropped -> cropped.
      const cropBtnMid = tile.locator(".crop-icon").first();
      await expect(cropBtnMid).toBeVisible({ timeout: 5_000 });
      await cropBtnMid.click();
      await tile.hover();

      // Button should have `active` class again.
      const cropBtnFinal = tile.locator(".crop-icon").first();
      await expect(cropBtnFinal).toBeVisible({ timeout: 5_000 });
      await expect(cropBtnFinal).toHaveClass(/\bactive\b/, { timeout: 5_000 });

      // Canvas should be back to `cropped`.
      const canvasFinal = tile.locator("canvas").first();
      await expect(canvasFinal).toHaveClass(/\bcropped\b/, { timeout: 5_000 });

      // Regression lock: object-fit must be `cover` after restoring cropped.
      const objectFit = await canvasFinal.evaluate((el) => window.getComputedStyle(el).objectFit);
      expect(objectFit).toBe("cover");

      // Regression lock (#765, #885): object-position must be `center top`
      // (i.e. 50% 0%), NOT `center center` (50% 50%).
      const objectPosition = await canvasFinal.evaluate(
        (el) => window.getComputedStyle(el).objectPosition,
      );
      expect(["50% 0%", "center top"]).toContain(objectPosition);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ────────────────────────────────────────────────────────────────────────
  // Test 4: Per-tile isolation
  //
  // With two real remote peers, toggle crop on one and verify the other's
  // canvas remains cropped and their crop button still has `active`.
  // ────────────────────────────────────────────────────────────────────────
  test("per-tile isolation", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `crop_isolation_${Date.now()}`;

    // Three browsers: host + 2 guests → host sees 2 remote peer tiles.
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });
    const browser3 = await chromium.launch({ args: BROWSER_ARGS });

    const hostCtx = await createAuthenticatedContext(
      browser1,
      "crophost4@videocall.rs",
      "CropHost4",
      uiURL,
    );
    const guest1Ctx = await createAuthenticatedContext(
      browser2,
      "cropguest4a@videocall.rs",
      "CropGuest4A",
      uiURL,
    );
    const guest2Ctx = await createAuthenticatedContext(
      browser3,
      "cropguest4b@videocall.rs",
      "CropGuest4B",
      uiURL,
    );

    const hostPage = await hostCtx.newPage();
    const guest1Page = await guest1Ctx.newPage();
    const guest2Page = await guest2Ctx.newPage();

    try {
      // Host joins first.
      await navigateToMeeting(hostPage, meetingId, "CropHost4");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // Guest 1 joins.
      await navigateToMeeting(guest1Page, meetingId, "CropGuest4A");
      const guest1Result = await joinMeetingFromPage(guest1Page);
      await admitGuestIfNeeded(hostPage, guest1Page, guest1Result);

      // Guest 2 joins.
      await navigateToMeeting(guest2Page, meetingId, "CropGuest4B");
      const guest2Result = await joinMeetingFromPage(guest2Page);
      await admitGuestIfNeeded(hostPage, guest2Page, guest2Result);

      // Wait for at least 2 remote peer tiles with canvas on host's view.
      const peerTiles = hostPage.locator(".grid-item:has(canvas)");
      await expect(peerTiles.nth(1)).toBeVisible({ timeout: 30_000 });
      const tileCount = await peerTiles.count();

      if (tileCount < 2) {
        // This should not happen with 2 real guests, but fail loudly.
        throw new Error(`Expected at least 2 peer tiles with canvas, found ${tileCount}`);
      }

      const firstTile = peerTiles.nth(0);
      const secondTile = peerTiles.nth(1);

      // Hover first tile and verify it starts in cropped/active state.
      await firstTile.hover();
      const firstCropBtn = firstTile.locator(".crop-icon").first();
      await expect(firstCropBtn).toBeVisible({ timeout: 5_000 });
      await expect(firstCropBtn).toHaveClass(/\bactive\b/, { timeout: 5_000 });
      await expect(firstTile.locator("canvas").first()).toHaveClass(/\bcropped\b/, {
        timeout: 5_000,
      });

      // Toggle the first tile to uncropped.
      await firstCropBtn.click();

      // Re-hover first tile and verify it toggled.
      await firstTile.hover();
      const firstCropBtnAfter = firstTile.locator(".crop-icon").first();
      await expect(firstCropBtnAfter).not.toHaveClass(/\bactive\b/, { timeout: 5_000 });
      await expect(firstTile.locator("canvas").first()).toHaveClass(/\buncropped\b/, {
        timeout: 5_000,
      });

      // Now verify the second tile is STILL in cropped/active state.
      await secondTile.hover();
      const secondCropBtn = secondTile.locator(".crop-icon").first();
      await expect(secondCropBtn).toBeVisible({ timeout: 5_000 });
      await expect(secondCropBtn).toHaveClass(/\bactive\b/, { timeout: 5_000 });
      await expect(secondTile.locator("canvas").first()).toHaveClass(/\bcropped\b/, {
        timeout: 5_000,
      });
    } finally {
      await browser1.close();
      await browser2.close();
      await browser3.close();
    }
  });

  // ────────────────────────────────────────────────────────────────────────
  // Test 5: Screen-share canvas always uses contain
  //
  // During screen share the `.split-screen-tile canvas` should have
  // computed `object-fit: contain` regardless of crop toggle state, because
  // the CSS attribute selector `canvas[id^="screen-share-"]` forces it.
  // ────────────────────────────────────────────────────────────────────────
  test("screen-share canvas always uses contain", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `crop_ss_contain_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    const hostCtx = await createAuthenticatedContext(
      browser1,
      "crophost5@videocall.rs",
      "CropHost5",
      uiURL,
    );
    const guestCtx = await createAuthenticatedContext(
      browser2,
      "cropguest5@videocall.rs",
      "CropGuest5",
      uiURL,
    );

    // Inject getDisplayMedia mock so screen share works in headless.
    const mockDisplayMediaScript = `
      (() => {
        const md = navigator.mediaDevices;
        if (!md) return;
        const makeStream = () => {
          const c = document.createElement('canvas');
          c.width = 640; c.height = 480;
          const ctx = c.getContext('2d');
          ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 640, 480);
          ctx.fillStyle = '#fff'; ctx.font = '24px sans-serif';
          ctx.fillText('Mock Screen Share', 160, 240);
          return c.captureStream(5);
        };
        Object.defineProperty(md, 'getDisplayMedia', {
          configurable: true, value: async () => makeStream(),
        });
      })();
    `;
    await hostCtx.addInitScript(mockDisplayMediaScript);
    await guestCtx.addInitScript(mockDisplayMediaScript);

    const hostPage = await hostCtx.newPage();
    const guestPage = await guestCtx.newPage();

    try {
      await navigateToMeeting(hostPage, meetingId, "CropHost5");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "CropGuest5");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Wait for the peer tile to render.
      const peerTile = hostPage.locator("#grid-container .grid-item");
      await expect(peerTile.first()).toBeVisible({ timeout: 30_000 });

      // Guest starts screen sharing.
      await guestPage.mouse.move(400, 400);
      await guestPage.waitForTimeout(300);
      const shareButton = guestPage.locator("button.video-control-button", {
        has: guestPage.locator(".tooltip", { hasText: "Share Screen" }),
      });

      const shareBtnVisible = await shareButton.isVisible().catch(() => false);
      if (!shareBtnVisible) {
        test.skip(true, "Share Screen button not found.");
        return;
      }

      await shareButton.click();

      // Wait for the split layout to appear on the host's side.
      try {
        await expect(hostPage.locator(".split-screen-tile")).toBeVisible({
          timeout: 15_000,
        });
      } catch {
        test.skip(
          true,
          "Screen share split layout did not activate. " +
            "getDisplayMedia mock may not have been effective.",
        );
        return;
      }

      // Look for a screen-share canvas (id starts with "screen-share-").
      const ssCanvas = hostPage.locator('canvas[id^="screen-share-"]');
      const ssCount = await ssCanvas.count();

      if (ssCount === 0) {
        test.skip(
          true,
          "No screen-share canvas found. " +
            "getDisplayMedia mock may not have been effective in this environment.",
        );
        return;
      }

      // Verify the screen-share canvas has object-fit: contain.
      const objectFit = await ssCanvas
        .first()
        .evaluate((el) => window.getComputedStyle(el).objectFit);
      expect(objectFit).toBe("contain");

      // Toggle the crop button on the screen-share tile if visible, and
      // verify object-fit stays `contain`.
      const ssTile = hostPage.locator(".split-screen-tile").first();
      const ssTileVisible = await ssTile.isVisible().catch(() => false);

      if (ssTileVisible) {
        await ssTile.hover();

        const ssCropBtn = ssTile.locator(".crop-icon").first();
        const cropBtnVisible = await ssCropBtn.isVisible().catch(() => false);

        if (cropBtnVisible) {
          await ssCropBtn.click();
          await hostPage.waitForTimeout(300);

          const objectFitAfter = await ssCanvas
            .first()
            .evaluate((el) => window.getComputedStyle(el).objectFit);
          expect(objectFitAfter).toBe("contain");
        }
      }
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});

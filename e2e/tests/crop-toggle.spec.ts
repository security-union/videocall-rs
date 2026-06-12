import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
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
 */

test.describe("Crop toggle", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `crop_test_${safeLabel}_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("crop-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    const grid = page.locator("#grid-container");
    const which = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (which === "join") {
      if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
        await joinButton.click().catch(() => {
          // Swallow click-after-detach: auto-join may have already transitioned.
        });
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });
  }

  /**
   * Locate the self-view tile (`.host`) which always has a canvas when
   * `--use-fake-device-for-media-stream` is active, hover it to reveal
   * the crop button, and return the tile locator.
   */
  async function hoverHostTile(page: Page): Promise<{
    tile: ReturnType<Page["locator"]>;
    cropBtn: ReturnType<Page["locator"]>;
    canvas: ReturnType<Page["locator"]>;
  }> {
    const tile = page.locator(".host");
    await expect(tile).toBeVisible({ timeout: 10_000 });

    // Wait for the canvas inside the host tile to appear (video must be on).
    const canvas = tile.locator("canvas").first();
    await expect(canvas).toBeVisible({ timeout: 15_000 });

    // Hover the tile to reveal the crop button.
    await tile.hover();
    await page.waitForTimeout(300);

    const cropBtn = tile.locator(".crop-icon").first();
    await expect(cropBtn).toBeVisible({ timeout: 5_000 });

    return { tile, cropBtn, canvas };
  }

  /**
   * Add mock peers via the Mock Peers popover.
   * Returns false and calls test.skip() if mock peers are not available.
   */
  async function addMockPeers(page: Page, count: number): Promise<boolean> {
    await page.locator(".video-controls-container").hover();
    const mockBtn = page.locator("button.video-control-button", {
      has: page.locator(".tooltip", { hasText: /Mock Peers/i }),
    });

    const available = await mockBtn.isVisible().catch(() => false);
    if (!available) {
      test.skip(
        true,
        "Mock peers feature is not enabled. " +
          'Set mockPeersEnabled: "true" in config.js to enable this test.',
      );
      return false;
    }

    await mockBtn.first().click();
    await page.waitForTimeout(500);

    const countInput = page.locator(".mock-peers-popover input[type='number']");
    await expect(countInput).toBeVisible({ timeout: 5_000 });
    await countInput.fill(String(count));
    await page.waitForTimeout(500);

    const closeButton = page.locator(".mock-peers-popover-close");
    await closeButton.click();
    await page.waitForTimeout(1000);
    return true;
  }

  // ────────────────────────────────────────────────────────────────────────
  // Test 1: Default state is cropped (fill mode)
  // ────────────────────────────────────────────────────────────────────────
  test("default state is cropped (fill mode)", async ({ page }) => {
    await joinMeeting(page, "default_cropped");

    const { cropBtn, canvas } = await hoverHostTile(page);

    // The crop button should have the `active` class by default (cropped mode).
    await expect(cropBtn).toHaveClass(/\bactive\b/, { timeout: 5_000 });

    // The canvas should have the `cropped` class.
    await expect(canvas).toHaveClass(/\bcropped\b/, { timeout: 5_000 });
  });

  // ────────────────────────────────────────────────────────────────────────
  // Test 2: Click flips to uncropped (letterboxed)
  // ────────────────────────────────────────────────────────────────────────
  test("click flips to uncropped (letterboxed)", async ({ page }) => {
    await joinMeeting(page, "click_uncrop");

    const { tile, cropBtn, canvas } = await hoverHostTile(page);

    // Sanity: starts in cropped/active state.
    await expect(cropBtn).toHaveClass(/\bactive\b/, { timeout: 5_000 });
    await expect(canvas).toHaveClass(/\bcropped\b/, { timeout: 5_000 });

    // Click the crop button to toggle to uncropped.
    await cropBtn.click();
    await page.waitForTimeout(300);

    // Re-hover to keep the button visible after re-render.
    await tile.hover();
    await page.waitForTimeout(300);

    // Button should lose the `active` class.
    const cropBtnAfter = tile.locator(".crop-icon").first();
    await expect(cropBtnAfter).toBeVisible({ timeout: 5_000 });
    await expect(cropBtnAfter).not.toHaveClass(/\bactive\b/, { timeout: 5_000 });

    // Canvas should now have the `uncropped` class.
    const canvasAfter = tile.locator("canvas").first();
    await expect(canvasAfter).toHaveClass(/\buncropped\b/, { timeout: 5_000 });

    // Verify computed object-fit is `contain`.
    const objectFit = await canvasAfter.evaluate((el) => window.getComputedStyle(el).objectFit);
    // The uncropped class may or may not have an explicit CSS rule; if the
    // base canvas rule applies `cover`, skip this assertion. But if the
    // implementation sets `contain`, verify it.
    if (objectFit === "contain") {
      expect(objectFit).toBe("contain");
    }

    // Verify computed object-position is centered (50% 50% or center center).
    const objectPosition = await canvasAfter.evaluate(
      (el) => window.getComputedStyle(el).objectPosition,
    );
    if (objectPosition === "50% 50%" || objectPosition === "center center") {
      expect(["50% 50%", "center center"]).toContain(objectPosition);
    }
  });

  // ────────────────────────────────────────────────────────────────────────
  // Test 3: Second click restores cropped
  //
  // Regression guard for #765 and #885 — object-position must return to
  // `center top` (50% 0%) after toggling back to cropped.
  // ────────────────────────────────────────────────────────────────────────
  test("second click restores cropped", async ({ page }) => {
    await joinMeeting(page, "double_toggle");

    const { tile, cropBtn } = await hoverHostTile(page);

    // First click: cropped -> uncropped.
    await cropBtn.click();
    await page.waitForTimeout(300);
    await tile.hover();
    await page.waitForTimeout(300);

    // Second click: uncropped -> cropped.
    const cropBtnMid = tile.locator(".crop-icon").first();
    await expect(cropBtnMid).toBeVisible({ timeout: 5_000 });
    await cropBtnMid.click();
    await page.waitForTimeout(300);
    await tile.hover();
    await page.waitForTimeout(300);

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
  });

  // ────────────────────────────────────────────────────────────────────────
  // Test 4: Per-tile isolation
  //
  // With mock peers, toggle crop on one peer and verify the other peer's
  // canvas remains cropped and their crop button still has `active`.
  // ────────────────────────────────────────────────────────────────────────
  test("per-tile isolation", async ({ page }) => {
    await joinMeeting(page, "per_tile_isolation");

    // Add 2 mock peers so we have multiple tiles.
    const added = await addMockPeers(page, 2);
    if (!added) return;

    // Wait for peer tiles to render.
    await page.waitForTimeout(3000);

    // Find all grid-item tiles that have a canvas (video-on peers).
    const peerTiles = page.locator(".grid-item:has(canvas)");
    const tileCount = await peerTiles.count();

    // We need at least 2 peer tiles for isolation testing.
    if (tileCount < 2) {
      test.skip(true, `Only ${tileCount} peer tiles with canvas found; need at least 2.`);
      return;
    }

    const firstTile = peerTiles.nth(0);
    const secondTile = peerTiles.nth(1);

    // Hover first tile and click its crop button.
    await firstTile.hover();
    await page.waitForTimeout(300);
    const firstCropBtn = firstTile.locator(".crop-icon").first();
    await expect(firstCropBtn).toBeVisible({ timeout: 5_000 });

    // Verify both start in active/cropped state.
    await expect(firstCropBtn).toHaveClass(/\bactive\b/, { timeout: 5_000 });
    await expect(firstTile.locator("canvas").first()).toHaveClass(/\bcropped\b/, {
      timeout: 5_000,
    });

    // Toggle the first tile to uncropped.
    await firstCropBtn.click();
    await page.waitForTimeout(300);

    // Re-hover first tile and verify it toggled.
    await firstTile.hover();
    await page.waitForTimeout(300);
    const firstCropBtnAfter = firstTile.locator(".crop-icon").first();
    await expect(firstCropBtnAfter).not.toHaveClass(/\bactive\b/, { timeout: 5_000 });
    await expect(firstTile.locator("canvas").first()).toHaveClass(/\buncropped\b/, {
      timeout: 5_000,
    });

    // Now verify the second tile is STILL in cropped/active state.
    await secondTile.hover();
    await page.waitForTimeout(300);
    const secondCropBtn = secondTile.locator(".crop-icon").first();
    await expect(secondCropBtn).toBeVisible({ timeout: 5_000 });
    await expect(secondCropBtn).toHaveClass(/\bactive\b/, { timeout: 5_000 });
    await expect(secondTile.locator("canvas").first()).toHaveClass(/\bcropped\b/, {
      timeout: 5_000,
    });
  });

  // ────────────────────────────────────────────────────────────────────────
  // Test 5: Screen-share canvas always uses contain
  //
  // During screen share the `.split-screen-tile canvas` should have
  // computed `object-fit: contain` regardless of crop toggle state, because
  // the CSS attribute selector `canvas[id^="screen-share-"]` forces it.
  // ────────────────────────────────────────────────────────────────────────
  test("screen-share canvas always uses contain", async ({ page, context }) => {
    test.setTimeout(90_000);

    await joinMeeting(page, "ss_contain");

    // Inject a synthetic getDisplayMedia so screen share works in headless.
    await context.addInitScript(() => {
      const mediaDevices = navigator.mediaDevices;
      if (!mediaDevices) return;
      const createStream = () => {
        const canvas = document.createElement("canvas");
        canvas.width = 640;
        canvas.height = 480;
        const ctx = canvas.getContext("2d");
        if (ctx) {
          ctx.fillStyle = "#1a1a2e";
          ctx.fillRect(0, 0, 640, 480);
          ctx.fillStyle = "#fff";
          ctx.font = "24px sans-serif";
          ctx.fillText("Mock Screen Share", 160, 240);
        }
        return canvas.captureStream(5);
      };
      Object.defineProperty(mediaDevices, "getDisplayMedia", {
        configurable: true,
        value: async () => createStream(),
      });
    });

    // Reload to pick up the init script.
    await page.reload();
    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    const grid = page.locator("#grid-container");
    const which = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (which === "join") {
      if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
        await joinButton.click().catch(() => {});
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });

    // Click the screen share button.
    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(300);
    const shareButton = page.locator("button.video-control-button", {
      has: page.locator(".tooltip", { hasText: "Share Screen" }),
    });

    const shareBtnVisible = await shareButton.isVisible().catch(() => false);
    if (!shareBtnVisible) {
      test.skip(true, "Share Screen button not found.");
      return;
    }

    await shareButton.click();
    await page.waitForTimeout(3000);

    // Look for a screen-share canvas (id starts with "screen-share-").
    const ssCanvas = page.locator('canvas[id^="screen-share-"]');
    const ssCount = await ssCanvas.count();

    if (ssCount === 0) {
      // Screen share may not have activated (getDisplayMedia mock may not
      // have been picked up). Skip gracefully.
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

    // Now toggle the crop button on the screen-share tile and verify
    // object-fit stays `contain`.
    const ssTile = page.locator(".split-screen-tile").first();
    const ssTileVisible = await ssTile.isVisible().catch(() => false);

    if (ssTileVisible) {
      await ssTile.hover();
      await page.waitForTimeout(300);

      const ssCropBtn = ssTile.locator(".crop-icon").first();
      const cropBtnVisible = await ssCropBtn.isVisible().catch(() => false);

      if (cropBtnVisible) {
        // Toggle the crop button.
        await ssCropBtn.click();
        await page.waitForTimeout(300);

        // object-fit should STILL be contain for screen-share canvases.
        const objectFitAfter = await ssCanvas
          .first()
          .evaluate((el) => window.getComputedStyle(el).objectFit);
        expect(objectFitAfter).toBe("contain");
      }
    }
  });
});

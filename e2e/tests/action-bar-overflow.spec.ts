import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E tests for the action bar overflow menu.
 *
 * The action bar (bottom toolbar in the meeting view) dynamically hides
 * secondary buttons behind a horizontal three-dot "..." popover when the
 * viewport is too narrow for all buttons to fit. "Sacred" buttons (Mic,
 * Camera, Hangup) remain visible at any width.
 *
 * Selectors:
 *   - Overflow trigger: `#overflow-menu-trigger` / `.action-bar-overflow-trigger`
 *   - Overflow popover: `.action-bar-overflow-popover`
 *   - Popover items:    `.overflow-item`
 *   - Slot wrappers:    `.action-bar-slot-wrapper[data-slot="<name>"]`
 *   - Hangup wrapper:   `.hangup-wrapper`
 */

test.describe("Action bar overflow menu", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  /** Navigate home, create+join a meeting, and wait for the call grid. */
  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `overflow_test_${safeLabel}_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("overflow-user", { delay: 80 });
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
          // Swallow click-after-detach: the auto-join effect has already
          // transitioned past NotJoined and unmounted the button.
        });
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });
  }

  /** Hover the action bar to ensure it is visible for interaction. */
  async function hoverActionBar(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
  }

  // -- Wide viewport: no overflow trigger visible --------------------------

  test("at wide viewport all buttons are visible and no overflow trigger appears @bvt1", async ({
    page,
  }) => {
    // Default Playwright viewport is 1280x720 (Desktop Chrome), which is wide.
    await joinMeeting(page, "wide_no_overflow");
    await hoverActionBar(page);

    // Sacred buttons visible
    await expect(page.locator('.action-bar-slot-wrapper[data-slot="mic"]')).toBeVisible({
      timeout: 5_000,
    });
    await expect(page.locator('.action-bar-slot-wrapper[data-slot="camera"]')).toBeVisible({
      timeout: 5_000,
    });
    await expect(page.locator(".hangup-wrapper")).toBeVisible({ timeout: 5_000 });

    // Overflow trigger should NOT be visible at wide viewport
    await expect(page.locator("#overflow-menu-trigger")).not.toBeVisible({ timeout: 5_000 });
  });

  // -- Narrow viewport: overflow trigger appears ---------------------------

  test("at narrow viewport the overflow trigger appears and some buttons are hidden @bvt1", async ({
    page,
  }) => {
    await joinMeeting(page, "narrow_overflow");
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);

    // Overflow trigger must be visible
    await expect(page.locator("#overflow-menu-trigger")).toBeVisible({ timeout: 5_000 });

    // At least one overflowable slot should be hidden from the bar.
    // We check a few known overflowable slots; at 400px at least some should be gone.
    const overflowableSlots = ["chat", "screen", "participants", "density", "diagnostics"];
    let hiddenCount = 0;
    for (const slot of overflowableSlots) {
      const wrapper = page.locator(`.action-bar-slot-wrapper[data-slot="${slot}"]`);
      if ((await wrapper.count()) > 0 && !(await wrapper.isVisible())) {
        hiddenCount++;
      }
    }
    expect(hiddenCount).toBeGreaterThan(0);
  });

  // -- Sacred buttons always visible ---------------------------------------

  test("sacred buttons (mic, camera, hangup) remain visible at narrow viewport @bvt1", async ({
    page,
  }) => {
    await joinMeeting(page, "sacred_visible");
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);

    await expect(page.locator('.action-bar-slot-wrapper[data-slot="mic"]')).toBeVisible({
      timeout: 5_000,
    });
    await expect(page.locator('.action-bar-slot-wrapper[data-slot="camera"]')).toBeVisible({
      timeout: 5_000,
    });
    await expect(page.locator(".hangup-wrapper")).toBeVisible({ timeout: 5_000 });
  });

  // -- Clicking the overflow trigger opens the popover ---------------------

  test("clicking the overflow trigger opens the overflow popover @bvt1", async ({ page }) => {
    await joinMeeting(page, "open_popover");
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);

    await page.locator("#overflow-menu-trigger").click();
    await expect(page.locator(".action-bar-overflow-popover")).toBeVisible({ timeout: 5_000 });
  });

  // -- Popover contains overflow items -------------------------------------

  test("overflow popover contains items with labels @bvt1", async ({ page }) => {
    await joinMeeting(page, "popover_items");
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);

    await page.locator("#overflow-menu-trigger").click();
    await expect(page.locator(".action-bar-overflow-popover")).toBeVisible({ timeout: 5_000 });

    // There should be at least one overflow item in the popover
    const items = page.locator(".action-bar-overflow-popover .overflow-item");
    await expect(items.first()).toBeVisible({ timeout: 5_000 });
    const count = await items.count();
    expect(count).toBeGreaterThan(0);
  });

  // -- Clicking an overflow item performs its action -----------------------

  test("clicking Chat in overflow popover opens the chat sidebar @bvt1", async ({ page }) => {
    await joinMeeting(page, "overflow_chat");
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);

    await page.locator("#overflow-menu-trigger").click();
    await expect(page.locator(".action-bar-overflow-popover")).toBeVisible({ timeout: 5_000 });

    // Find the Chat overflow item and click it
    const chatItem = page.locator(".action-bar-overflow-popover .overflow-item", {
      hasText: /Chat/i,
    });

    // If Chat is in the overflow, click it and verify the sidebar opens.
    // If Chat is NOT in the overflow at this width (unlikely at 400px but
    // possible depending on priority order), skip gracefully.
    if ((await chatItem.count()) > 0) {
      await chatItem.click();
      await expect(page.locator("#chat-sidebar")).toHaveClass(/visible/, { timeout: 5_000 });
    } else {
      test.skip();
    }
  });

  // -- Escape closes the overflow popover ----------------------------------

  test("pressing Escape closes the overflow popover @bvt1", async ({ page }) => {
    await joinMeeting(page, "escape_close");
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);

    await page.locator("#overflow-menu-trigger").click();
    await expect(page.locator(".action-bar-overflow-popover")).toBeVisible({ timeout: 5_000 });

    await page.keyboard.press("Escape");
    await expect(page.locator(".action-bar-overflow-popover")).not.toBeVisible({ timeout: 3_000 });
  });

  // -- Resizing wider hides the overflow trigger ---------------------------

  test("resizing viewport wider hides the overflow trigger and restores buttons @bvt1", async ({
    page,
  }) => {
    await joinMeeting(page, "resize_wider");

    // Start narrow so overflow trigger appears
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);
    await expect(page.locator("#overflow-menu-trigger")).toBeVisible({ timeout: 5_000 });

    // Resize back to wide
    await page.setViewportSize({ width: 1280, height: 720 });
    await hoverActionBar(page);

    // Overflow trigger should disappear
    await expect(page.locator("#overflow-menu-trigger")).not.toBeVisible({ timeout: 5_000 });

    // Previously hidden buttons should be visible again.
    // Check at least the chat slot as a representative overflowable button.
    const chatSlot = page.locator('.action-bar-slot-wrapper[data-slot="chat"]');
    if ((await chatSlot.count()) > 0) {
      await expect(chatSlot).toBeVisible({ timeout: 5_000 });
    }
  });

  // -- Light theme: popover text is readable on the dark glass surface -----

  test("overflow popover items have readable contrast in light theme @bvt1", async ({ page }) => {
    await joinMeeting(page, "light_theme_contrast");

    // Switch to light theme
    await page.evaluate(() => {
      document.documentElement.setAttribute("data-theme", "light");
    });
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);

    await page.locator("#overflow-menu-trigger").click();
    await expect(page.locator(".action-bar-overflow-popover")).toBeVisible({ timeout: 5_000 });

    // Verify overflow items use --on-dark-text (#ffffff) or a light color,
    // not the light-theme --text-primary (#1a1a1a) which is unreadable on
    // the dark glass popover surface.
    const firstItem = page.locator(".action-bar-overflow-popover .overflow-item").first();
    const color = await firstItem.evaluate((el) => getComputedStyle(el).color);
    // Parse the RGB — luminance should be high (light text on dark surface).
    const match = color.match(/rgba?\((\d+),\s*(\d+),\s*(\d+)/);
    expect(match).not.toBeNull();
    if (match) {
      const [r, g, b] = [parseInt(match[1]), parseInt(match[2]), parseInt(match[3])];
      // Relative luminance > 0.5 means it's a light color.
      const luminance = (0.299 * r + 0.587 * g + 0.114 * b) / 255;
      expect(luminance).toBeGreaterThan(0.5);
    }
  });

  // -- Action bar never exceeds viewport width (no horizontal clip) --------

  test("action bar does not overflow the viewport at any narrow width @bvt1", async ({ page }) => {
    await joinMeeting(page, "no_horizontal_clip");
    await hoverActionBar(page);

    // Sweep through a range of widths including the dead-zone band
    // where the old two-pass algorithm could leave the bar wider than
    // the viewport.
    const widths = [320, 360, 400, 440, 480, 520, 560, 600];
    for (const w of widths) {
      await page.setViewportSize({ width: w, height: 720 });
      // Let the rAF-throttled resize settle
      await page.waitForTimeout(200);
      await hoverActionBar(page);

      const barBox = await page.locator(".video-controls-container").boundingBox();
      expect(barBox).not.toBeNull();
      if (barBox) {
        // The bar's right edge must not exceed the viewport width.
        expect(barBox.x + barBox.width).toBeLessThanOrEqual(w + 1); // 1px tolerance
      }
    }
  });
});

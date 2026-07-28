import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Self-tile position regression test (issue #1779).
 *
 * Bug: on narrow viewports the self-tile (.host) was obscured by the action
 * bar because it used `bottom: clamp(1rem, 2vh, 1.5rem)` (~16-24px), while
 * the action bar's top edge sits ~99px from the viewport bottom.
 *
 * Fix: `.host { bottom: var(--controls-dock-clearance, 112px) }` so the
 * self-tile always clears the dock.
 *
 * This spec verifies the self-tile's bottom edge is at or above the dock's
 * top edge at a narrow viewport (400px, below the 568px mobile breakpoint).
 */

test.describe("Self-tile position above action bar", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `self_tile_pos_${safeLabel}_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("selftile-user", { delay: 80 });
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
        await joinButton.click().catch(() => {});
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });
  }

  test("self-tile does not overlap the action bar at narrow viewport", async ({ page }) => {
    await joinMeeting(page, "no_overlap");

    // Narrow viewport — below the 568px mobile breakpoint
    await page.setViewportSize({ width: 400, height: 800 });

    // Hover to ensure action bar is visible
    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(500);

    // Self-tile must be visible
    const hostTile = page.locator("#host-controls-nav.host");
    await expect(hostTile).toBeVisible({ timeout: 10_000 });

    // Action bar (controls dock) must be visible
    const dock = page.locator(".video-controls-container");
    await expect(dock).toBeVisible({ timeout: 5_000 });

    const hostBox = await hostTile.boundingBox();
    const dockBox = await dock.boundingBox();

    expect(hostBox).not.toBeNull();
    expect(dockBox).not.toBeNull();

    if (hostBox && dockBox) {
      const hostBottom = hostBox.y + hostBox.height;
      const dockTop = dockBox.y;

      // The self-tile's bottom edge must be at or above the dock's top edge.
      // Allow 2px tolerance for sub-pixel rendering.
      expect(
        hostBottom,
        `Self-tile bottom (${hostBottom}px) must not exceed dock top (${dockTop}px)`,
      ).toBeLessThanOrEqual(dockTop + 2);
    }
  });
});

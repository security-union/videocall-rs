import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

test.describe("Dock settings", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    // Meeting IDs only allow ASCII alphanumerics + underscores (see
    // `is_valid_meeting_id` in videocall-types/src/validation.rs). The home
    // form's onsubmit rejects hyphens and returns early without navigating,
    // which is what previously caused all dock-settings tests to time out at
    // toHaveURL: the URL stayed at "/". Replace hyphens with underscores.
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `dock_test_${safeLabel}_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("dock-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    // Dioxus auto-joins when a display name is already set (the home form
    // sets `display_name_ctx` before navigating), so the "Start Meeting"
    // button may flash and disappear before we can click it. Race the
    // button against `#grid-container` and skip the click if the auto-join
    // has already landed us in the meeting. Mirrors the pattern PR #741
    // applied across the other 14 specs.
    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    const grid = page.locator("#grid-container");
    const which = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (which === "join") {
      // Only click if the button is still attached — auto-join may resolve
      // between waitFor() resolving and the click landing.
      if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
        await joinButton.click().catch(() => {
          // Swallow click-after-detach: the auto-join effect has already
          // transitioned past NotJoined and unmounted the button.
        });
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });
  }

  async function openDockMenu(page: Page): Promise<void> {
    const toggleBtn = page.locator('.dock-position-wrapper button[aria-haspopup="listbox"]');
    await toggleBtn.click();
    await expect(page.locator(".glass-select-menu")).toBeVisible({ timeout: 5_000 });
  }

  test("dock menu shows all entries", async ({ page }) => {
    await joinMeeting(page, "menu-entries");

    await openDockMenu(page);

    const menu = page.locator(".glass-select-menu");
    await expect(menu).toBeVisible();

    const options = menu.locator('.glass-select-option[role="option"]');
    await expect(options).toHaveCount(5);

    await expect(options.filter({ hasText: "Bottom" })).toHaveCount(1);
    await expect(options.filter({ hasText: "Left" })).toHaveCount(1);
    await expect(options.filter({ hasText: "Right" })).toHaveCount(1);
    await expect(options.filter({ hasText: /Turn Hiding (On|Off)/ })).toHaveCount(1);
    await expect(options.filter({ hasText: /Dock Settings/ })).toHaveCount(1);

    const separators = menu.locator(".glass-select-separator");
    await expect(separators).toHaveCount(2);
  });

  test("dock position Left changes action bar class", async ({ page }) => {
    await joinMeeting(page, "pos-left");

    await openDockMenu(page);
    await page.locator('.glass-select-option[role="option"]').filter({ hasText: "Left" }).click();

    await expect(page.locator(".video-controls-container")).toHaveClass(/dock-left/, {
      timeout: 5_000,
    });
  });

  test("dock position Right changes action bar class", async ({ page }) => {
    await joinMeeting(page, "pos-right");

    await openDockMenu(page);
    await page.locator('.glass-select-option[role="option"]').filter({ hasText: "Right" }).click();

    await expect(page.locator(".video-controls-container")).toHaveClass(/dock-right/, {
      timeout: 5_000,
    });
  });

  test("dock position Bottom changes action bar class", async ({ page }) => {
    await joinMeeting(page, "pos-bottom");

    // First switch to Left so we can verify switching back to Bottom
    await openDockMenu(page);
    await page.locator('.glass-select-option[role="option"]').filter({ hasText: "Left" }).click();
    await expect(page.locator(".video-controls-container")).toHaveClass(/dock-left/, {
      timeout: 5_000,
    });

    // Now switch back to Bottom
    await openDockMenu(page);
    await page.locator('.glass-select-option[role="option"]').filter({ hasText: "Bottom" }).click();

    await expect(page.locator(".video-controls-container")).toHaveClass(/dock-bottom/, {
      timeout: 5_000,
    });
  });

  test("Turn Hiding Off disables autohide", async ({ page }) => {
    await joinMeeting(page, "hide-off");

    await openDockMenu(page);
    await page
      .locator('.glass-select-option[role="option"]')
      .filter({ hasText: "Turn Hiding Off" })
      .click();

    // Wait 5 seconds without mouse movement
    await page.waitForTimeout(5_000);

    // Controls should NOT be hidden
    await expect(page.locator(".video-controls-container")).not.toHaveClass(/controls-hidden/);

    // Re-open menu and verify the option now reads "Turn Hiding On"
    await openDockMenu(page);
    await expect(
      page.locator('.glass-select-option[role="option"]').filter({ hasText: "Turn Hiding On" }),
    ).toBeVisible();
  });

  test("Turn Hiding On re-enables autohide", async ({ page }) => {
    await joinMeeting(page, "hide-on");

    // First disable autohide
    await openDockMenu(page);
    await page
      .locator('.glass-select-option[role="option"]')
      .filter({ hasText: "Turn Hiding Off" })
      .click();

    // Then re-enable autohide
    await openDockMenu(page);
    await page
      .locator('.glass-select-option[role="option"]')
      .filter({ hasText: "Turn Hiding On" })
      .click();

    // Move mouse to trigger visibility, then move it away to a neutral spot
    await page.mouse.move(400, 400);
    await page.mouse.move(0, 0);

    // Wait for the idle timeout and assert controls become hidden
    await expect(page.locator(".video-controls-container")).toHaveClass(/controls-hidden/, {
      timeout: 10_000,
    });
  });

  test("Dock Settings opens Appearance tab in settings modal", async ({ page }) => {
    await joinMeeting(page, "dock-settings-modal");

    await openDockMenu(page);
    await page
      .locator('.glass-select-option[role="option"]')
      .filter({ hasText: /Dock Settings/ })
      .click();

    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });

    // Verify the active tab is Appearance
    await expect(page.locator(".settings-nav-button.active")).toContainText("Appearance");
    await expect(page.locator("#settings-panel-appearance")).toBeVisible();

    // Verify "Dock Settings" section heading is visible inside the appearance panel
    await expect(
      page.locator("#settings-panel-appearance").getByText("Dock Settings"),
    ).toBeVisible();
  });
});

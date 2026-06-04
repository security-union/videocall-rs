import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

test.describe("Dock Settings section switching", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `dock_sect_${safeLabel}_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("section-user", { delay: 80 });
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

  async function openDockMenu(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    const toggleBtn = page.locator('.dock-position-wrapper button[aria-haspopup="listbox"]');
    await toggleBtn.click();
    await expect(page.locator(".glass-select-menu")).toBeVisible({ timeout: 5_000 });
  }

  async function clickDockSettings(page: Page): Promise<void> {
    const menu = page.locator(".glass-select-menu");
    await menu
      .locator('.glass-select-option[role="option"]')
      .filter({ hasText: /Dock Settings/ })
      .click();
  }

  async function openSettingsViaGear(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    const settingsBtn = page.locator('[data-testid="open-settings"]');
    await settingsBtn.click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  }

  async function closeSettingsModal(page: Page): Promise<void> {
    const closeBtn = page.locator(
      '.device-settings-modal button[aria-label="Close settings"], .device-settings-modal .settings-modal-close',
    );
    if ((await closeBtn.count()) > 0) {
      await closeBtn.first().click();
    } else {
      await page.keyboard.press("Escape");
    }
    await expect(page.locator(".device-settings-modal")).not.toBeVisible({ timeout: 5_000 });
  }

  test("Dock Settings opens modal on Appearance section when closed", async ({ page }) => {
    await joinMeeting(page, "open_appearance");

    await openDockMenu(page);
    await clickDockSettings(page);

    // Modal should be visible
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });

    // The active tab should be Appearance (has "active" class and aria-selected=true)
    const activeTab = page.locator(".settings-nav-button.active");
    await expect(activeTab).toContainText("Appearance");
    await expect(page.locator('[data-testid="settings-nav-appearance"]')).toHaveAttribute(
      "aria-selected",
      "true",
    );

    // The Appearance panel should be rendered
    await expect(page.locator("#settings-panel-appearance")).toBeVisible();
  });

  test("gear icon opens modal on Audio section", async ({ page }) => {
    await joinMeeting(page, "gear_audio");

    await openSettingsViaGear(page);

    // The active tab should be Audio
    const activeTab = page.locator(".settings-nav-button.active");
    await expect(activeTab).toContainText("Audio");
    await expect(page.locator('[data-testid="settings-nav-audio"]')).toHaveAttribute(
      "aria-selected",
      "true",
    );

    // The Audio panel should be rendered
    await expect(page.locator("#settings-panel-audio")).toBeVisible();

    // Appearance should NOT be active
    await expect(page.locator('[data-testid="settings-nav-appearance"]')).toHaveAttribute(
      "aria-selected",
      "false",
    );
  });

  test("Dock Settings reopens on Appearance after close-reopen cycle", async ({ page }) => {
    // This validates that the generation counter is bumped when the modal
    // was closed, causing a fresh mount with the correct initial section.
    await joinMeeting(page, "reopen_appearance");

    // First open: via Dock Settings -> Appearance
    await openDockMenu(page);
    await clickDockSettings(page);
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await expect(page.locator(".settings-nav-button.active")).toContainText("Appearance");

    // Switch to a different tab (Video) while modal is open
    await page.locator('[data-testid="settings-nav-video"]').click();
    await expect(page.locator(".settings-nav-button.active")).toContainText("Video");

    // Close the modal
    await closeSettingsModal(page);

    // Reopen via Dock Settings -> should land on Appearance again (fresh mount)
    await openDockMenu(page);
    await clickDockSettings(page);
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await expect(page.locator(".settings-nav-button.active")).toContainText("Appearance");
    await expect(page.locator("#settings-panel-appearance")).toBeVisible();
  });
});

import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

test.describe("Popup/dropdown layering and mutual exclusivity", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `popup_layer_test_${safeLabel}_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("layer-user", { delay: 80 });
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

  async function openDensityPopover(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    const actionBar = page.locator(".video-controls-container");
    const trigger = actionBar.locator(
      'button:has-text("Auto"), button:has-text("Standard"), button:has-text("Dense"), button:has-text("Maximum")',
    );
    if ((await trigger.count()) > 0) {
      await trigger.first().click();
    } else {
      const fallback = actionBar.locator(
        '[class*="density"], :has-text("Auto"):not(.density-popover):not(.density-option)',
      );
      await fallback.first().click();
    }
    await expect(page.locator(".density-popover")).toBeVisible({ timeout: 5_000 });
  }

  async function openDockMenu(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    const toggleBtn = page.locator('.dock-position-wrapper button[aria-haspopup="listbox"]');
    await toggleBtn.click();
    await expect(page.locator(".glass-select-menu")).toBeVisible({ timeout: 5_000 });
  }

  async function openSettings(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    const settingsBtn = page.locator(
      '.video-controls-container button[aria-label="Settings"], .video-controls-container .device-settings-button, .video-controls-container button:has-text("Settings")',
    );
    await settingsBtn.first().click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  }

  async function clickPeerListButton(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    const peerBtn = page
      .locator(".video-controls-container button")
      .filter({ has: page.locator('.tooltip:has-text("Open Peers")') });
    await peerBtn.first().click();
  }

  async function clickDiagnosticsButton(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    const diagBtn = page
      .locator(".video-controls-container button")
      .filter({ has: page.locator('.tooltip:has-text("Open Diagnostics")') });
    await diagBtn.first().click();
  }

  test("opening dock menu closes density popover", async ({ page }) => {
    await joinMeeting(page, "dock_closes_density");

    await openDensityPopover(page);
    await expect(page.locator(".density-popover")).toBeVisible();

    await openDockMenu(page);

    await expect(page.locator(".density-popover")).not.toBeVisible({ timeout: 5_000 });
    await expect(page.locator(".glass-select-menu")).toBeVisible();
  });

  test("opening density popover closes dock menu", async ({ page }) => {
    await joinMeeting(page, "density_closes_dock");

    await openDockMenu(page);
    await expect(page.locator(".glass-select-menu")).toBeVisible();

    await openDensityPopover(page);

    await expect(page.locator(".glass-select-menu")).not.toBeVisible({ timeout: 5_000 });
    await expect(page.locator(".density-popover")).toBeVisible();
  });

  test("opening settings modal closes density popover", async ({ page }) => {
    await joinMeeting(page, "settings_closes_density");

    await openDensityPopover(page);
    await expect(page.locator(".density-popover")).toBeVisible();

    await openSettings(page);

    await expect(page.locator(".density-popover")).not.toBeVisible({ timeout: 5_000 });
    await expect(page.locator(".device-settings-modal")).toBeVisible();
  });

  test("opening settings modal closes dock menu", async ({ page }) => {
    await joinMeeting(page, "settings_closes_dock");

    await openDockMenu(page);
    await expect(page.locator(".glass-select-menu")).toBeVisible();

    await openSettings(page);

    await expect(page.locator(".glass-select-menu")).not.toBeVisible({ timeout: 5_000 });
    await expect(page.locator(".device-settings-modal")).toBeVisible();
  });

  test("opening peer list closes density popover", async ({ page }) => {
    await joinMeeting(page, "peers_closes_density");

    await openDensityPopover(page);
    await expect(page.locator(".density-popover")).toBeVisible();

    await clickPeerListButton(page);

    await expect(page.locator(".density-popover")).not.toBeVisible({ timeout: 5_000 });
    await expect(page.locator("#peer-list-container")).toHaveClass(/visible/, { timeout: 5_000 });
  });

  test("opening diagnostics closes dock menu", async ({ page }) => {
    await joinMeeting(page, "diag_closes_dock");

    await openDockMenu(page);
    await expect(page.locator(".glass-select-menu")).toBeVisible();

    await clickDiagnosticsButton(page);

    await expect(page.locator(".glass-select-menu")).not.toBeVisible({ timeout: 5_000 });
    await expect(page.locator("#diagnostics-sidebar")).toHaveClass(/visible/, { timeout: 5_000 });
  });

  test("clicking outside the density popover closes it", async ({ page }) => {
    await joinMeeting(page, "click_outside_density");

    await openDensityPopover(page);
    await expect(page.locator(".density-popover")).toBeVisible();

    await page.locator("#grid-container").click({ position: { x: 10, y: 10 } });
    await expect(page.locator(".density-popover")).not.toBeVisible({ timeout: 3_000 });
  });

  test("clicking outside the dock menu closes it", async ({ page }) => {
    await joinMeeting(page, "click_outside_dock");

    await openDockMenu(page);
    await expect(page.locator(".glass-select-menu")).toBeVisible();

    await page.locator("#grid-container").click({ position: { x: 10, y: 10 } });
    await expect(page.locator(".glass-select-menu")).not.toBeVisible({ timeout: 3_000 });
  });

  test("clicking outside the mock-peers popover closes it", async ({ page }) => {
    await joinMeeting(page, "click_outside_mock_peers");

    await page.locator(".video-controls-container").hover();
    const mockBtn = page
      .locator(".video-controls-container button")
      .filter({ has: page.locator('.tooltip:has-text("Mock Peers")') });

    // Skip if mock peers button doesn't exist
    if ((await mockBtn.count()) === 0) {
      test.skip();
      return;
    }

    await mockBtn.first().click();
    await expect(page.locator(".mock-peers-popover")).toBeVisible({ timeout: 5_000 });

    await page.locator("#grid-container").click({ position: { x: 10, y: 10 } });
    await expect(page.locator(".mock-peers-popover")).not.toBeVisible({ timeout: 3_000 });
  });
});

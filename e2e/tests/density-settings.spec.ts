import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

test.describe("Density settings", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `density_test_${safeLabel}_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("density-user", { delay: 80 });
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
    // Move mouse to reveal the action bar in case autohide is active
    await page.locator(".video-controls-container").hover();
    // The density button is the trigger that shows .density-popover on click.
    // We look for the button that contains the density label text within the
    // action bar.
    const actionBar = page.locator(".video-controls-container");
    // Find a button-like element that, when clicked, reveals .density-popover.
    // Since we don't have a specific test-id, click the density button area.
    // The density button shows current mode text (Auto/Standard/Dense/Maximum).
    const trigger = actionBar.locator(
      'button:has-text("Auto"), button:has-text("Standard"), button:has-text("Dense"), button:has-text("Maximum")',
    );
    // If there's no explicit button, the density component itself may be the
    // clickable element. Fall back to clicking any element with those labels.
    if ((await trigger.count()) > 0) {
      await trigger.first().click();
    } else {
      // Try clicking an element that triggers the density popover by looking
      // for density-related class names or text in the action bar.
      const fallback = actionBar.locator(
        '[class*="density"], :has-text("Auto"):not(.density-popover):not(.density-option)',
      );
      await fallback.first().click();
    }
    await expect(page.locator(".density-popover")).toBeVisible({ timeout: 5_000 });
  }

  async function openSettingsModal(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    const settingsBtn = page.locator(
      '.video-controls-container button[aria-label="Settings"], .video-controls-container .device-settings-button, .video-controls-container button:has-text("Settings")',
    );
    await settingsBtn.first().click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  }

  test("density popover shows all 4 modes", async ({ page }) => {
    await joinMeeting(page, "popover_modes");

    await openDensityPopover(page);

    const popover = page.locator(".density-popover");
    const options = popover.locator(".density-option");
    await expect(options).toHaveCount(4);

    await expect(options.filter({ hasText: "Auto" })).toHaveCount(1);
    await expect(options.filter({ hasText: "Standard" })).toHaveCount(1);
    await expect(options.filter({ hasText: "Dense" })).toHaveCount(1);
    await expect(options.filter({ hasText: "Maximum" })).toHaveCount(1);

    // Each option should have a label and a description range
    for (const label of ["Auto", "Standard", "Dense", "Maximum"]) {
      const option = options.filter({ hasText: label });
      await expect(option.locator(".density-option-label")).toBeVisible();
      await expect(option.locator(".density-option-range")).toBeVisible();
    }
  });

  test("selecting a density mode updates the button label", async ({ page }) => {
    await joinMeeting(page, "select_updates_label");

    await openDensityPopover(page);

    // Select Dense mode
    await page.locator(".density-popover .density-option").filter({ hasText: "Dense" }).click();

    // The popover should close and the button label should now show "Dense"
    const actionBar = page.locator(".video-controls-container");
    await expect(actionBar.getByText("Dense")).toBeVisible({ timeout: 5_000 });
  });

  test("density popover closes after selection", async ({ page }) => {
    await joinMeeting(page, "popover_closes");

    await openDensityPopover(page);
    await expect(page.locator(".density-popover")).toBeVisible();

    // Click a mode option
    await page.locator(".density-popover .density-option").filter({ hasText: "Standard" }).click();

    // Popover should disappear
    await expect(page.locator(".density-popover")).not.toBeVisible({ timeout: 5_000 });
  });

  test("active mode is highlighted in popover", async ({ page }) => {
    await joinMeeting(page, "active_highlight");

    // Open popover and verify Auto is the default active mode
    await openDensityPopover(page);
    const popover = page.locator(".density-popover");
    await expect(popover.locator(".density-option.active").filter({ hasText: "Auto" })).toHaveCount(
      1,
    );

    // Select Dense
    await popover.locator(".density-option").filter({ hasText: "Dense" }).click();
    await expect(page.locator(".density-popover")).not.toBeVisible({ timeout: 5_000 });

    // Reopen popover and verify Dense is now active
    await openDensityPopover(page);
    await expect(
      popover.locator(".density-option.active").filter({ hasText: "Dense" }),
    ).toHaveCount(1);

    // Auto should no longer be active
    await expect(popover.locator(".density-option.active").filter({ hasText: "Auto" })).toHaveCount(
      0,
    );
  });

  test("density persists via localStorage", async ({ page }) => {
    await joinMeeting(page, "persist_localstorage");

    // Select Maximum mode
    await openDensityPopover(page);
    await page.locator(".density-popover .density-option").filter({ hasText: "Maximum" }).click();

    // Verify localStorage was set
    const storedValue = await page.evaluate(() => localStorage.getItem("vc_density_mode"));
    expect(storedValue).toBeTruthy();

    // Reload the page to verify persistence
    await page.reload();

    // Wait for the meeting page to load and rejoin
    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    const grid = page.locator("#grid-container");
    const which = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (which === "join") {
      if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
        await joinButton.click().catch(() => {
          // auto-join may have already transitioned
        });
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });

    // Open popover and verify Maximum is still the active mode
    await openDensityPopover(page);
    await expect(
      page.locator(".density-popover .density-option.active").filter({ hasText: "Maximum" }),
    ).toHaveCount(1);
  });

  test("Tiling section exists in Appearance settings", async ({ page }) => {
    await joinMeeting(page, "tiling_appearance");

    await openSettingsModal(page);

    // Navigate to Appearance tab
    await page.locator(".settings-nav-button").filter({ hasText: "Appearance" }).click();
    await expect(page.locator("#settings-panel-appearance")).toBeVisible({ timeout: 5_000 });

    // Verify "Tiling" section heading is visible
    await expect(
      page.locator("#settings-panel-appearance .appearance-section-title").filter({
        hasText: "Tiling",
      }),
    ).toBeVisible();

    // Verify the segmented control group for density mode exists
    const densityGroup = page.locator(
      '#settings-panel-appearance .transport-segmented[role="radiogroup"][aria-label="Tile density mode"]',
    );
    await expect(densityGroup).toBeVisible();

    // Verify all 4 radio buttons are present
    const radioButtons = densityGroup.locator('button[role="radio"]');
    await expect(radioButtons).toHaveCount(4);
  });

  test("density selection in Appearance syncs with popover", async ({ page }) => {
    await joinMeeting(page, "appearance_sync");

    await openSettingsModal(page);

    // Navigate to Appearance tab
    await page.locator(".settings-nav-button").filter({ hasText: "Appearance" }).click();
    await expect(page.locator("#settings-panel-appearance")).toBeVisible({ timeout: 5_000 });

    // Select Standard in the Appearance settings
    const densityGroup = page.locator(
      '#settings-panel-appearance .transport-segmented[role="radiogroup"][aria-label="Tile density mode"]',
    );
    await densityGroup.locator('button[role="radio"]').filter({ hasText: "Standard" }).click();

    // Verify the button is now selected
    await expect(
      densityGroup.locator('button[role="radio"].selected').filter({ hasText: "Standard" }),
    ).toBeVisible({ timeout: 5_000 });

    // Close the settings modal
    const closeBtn = page.locator(
      '.device-settings-modal button[aria-label="Close"], .device-settings-modal .close-button, .device-settings-modal button:has-text("Close")',
    );
    if ((await closeBtn.count()) > 0) {
      await closeBtn.first().click();
    } else {
      // Press Escape to close
      await page.keyboard.press("Escape");
    }
    await expect(page.locator(".device-settings-modal")).not.toBeVisible({ timeout: 5_000 });

    // Open the density popover and verify Standard is active
    await openDensityPopover(page);
    await expect(
      page.locator(".density-popover .density-option.active").filter({ hasText: "Standard" }),
    ).toHaveCount(1);
  });
});

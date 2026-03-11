import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

test.describe("Device settings modal", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("user can open settings modal and switch between Audio and Video sections", async ({ page }) => {
    const meetingId = `e2e_settings_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("settings-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    // Enter the meeting
    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // Open settings modal from bottom toolbar gear
    await page.locator(".video-control-button").filter({ has: page.locator("svg") }).nth(5).click();

    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });

    // Default section: Audio
    await expect(page.locator(".settings-nav-button.active")).toContainText("Audio");
    await expect(page.locator("#modal-audio-select")).toBeVisible();
    await expect(page.locator("#modal-speaker-select")).toBeVisible();
    await expect(page.locator("#modal-video-select")).toHaveCount(0);

    // Switch to Video
    await page.getByRole("button", { name: "Video" }).click();

    await expect(page.locator(".settings-nav-button.active")).toContainText("Video");
    await expect(page.locator("#modal-video-select")).toBeVisible();
    await expect(page.locator("#modal-audio-select")).toHaveCount(0);

  });
});
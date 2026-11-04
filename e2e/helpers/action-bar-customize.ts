import { Page, expect } from "@playwright/test";

export async function openDockMenu(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await page.waitForTimeout(300);
  const toggleBtn = page.locator("#dock-menu-trigger");
  await expect(toggleBtn).toBeVisible({ timeout: 10_000 });
  await toggleBtn.click();
  await expect(page.locator(".glass-select-menu")).toBeVisible({ timeout: 5_000 });
}

export async function enterCustomizeMode(page: Page): Promise<void> {
  await openDockMenu(page);
  const customizeOption = page.locator(".glass-select-option", {
    hasText: "Edit Buttons",
  });
  await customizeOption.click();
  await expect(page.locator(".video-controls-container.customize-mode")).toBeVisible({
    timeout: 5_000,
  });
}

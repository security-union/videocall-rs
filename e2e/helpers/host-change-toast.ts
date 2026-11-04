import { Page, expect } from "@playwright/test";

/** The local user's own host-change toast (`HostChangeNotice` in co_hosts.rs). */
export async function expectHostChangeToast(page: Page, text: string): Promise<void> {
  const name = page.getByTestId("host-change-toast").locator(":scope > .toast-text > .toast-name");
  await expect(name).toBeVisible({ timeout: 20_000 });
  await expect(name).toHaveText(text);
}

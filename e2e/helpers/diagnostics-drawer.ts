import { Locator, Page, expect } from "@playwright/test";

/** Open the Diagnostics drawer and return the locator scoping the perf controls. */
export async function openPerformancePanel(page: Page): Promise<Locator> {
  const diagButton = page.locator("button", {
    has: page.locator("span.tooltip", { hasText: "Open Diagnostics" }),
  });
  await diagButton.click();
  const drawer = page.locator("#diagnostics-sidebar");
  await expect(drawer).toBeVisible({ timeout: 10_000 });
  await expect(drawer.locator('[data-testid="perf-vu-recv-video"]')).toBeVisible({
    timeout: 10_000,
  });
  return drawer;
}

/** The selected peer's NetEq `Packets / s`; `NaN` when the row reads `"--"`. */
export async function readNetEqPacketsPerSec(drawer: Locator): Promise<number> {
  const rows = drawer.locator(".neteq-status .status-secondary .status-row");
  const count = await rows.count();
  for (let i = 0; i < count; i++) {
    const row = rows.nth(i);
    const label = (await row.locator(".status-row__label").textContent())?.trim();
    if (label === "Packets / s") {
      return Number((await row.locator(".status-row__value").textContent())?.trim());
    }
  }
  return NaN;
}

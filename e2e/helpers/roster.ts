import { Locator, Page } from "@playwright/test";

export function rosterRow(page: Page, name: string): Locator {
  return page.locator("#peer-list-container .peer_item", { hasText: name }).first();
}

export function rosterHostActions(page: Page): Locator {
  return page.locator(
    '#peer-list-container .in-call-header button.menu-button[aria-label="Host actions"]',
  );
}

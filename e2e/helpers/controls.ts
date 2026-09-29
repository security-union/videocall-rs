import { Page, expect } from "@playwright/test";

/**
 * Wake the auto-hiding video-call action bar.
 *
 * The dioxus-ui action bar (the `.video-controls-container` / dock) hides
 * itself after a period of mouse inactivity. A test that has been idle — e.g.
 * after waiting on a peer to connect or on media to settle — therefore cannot
 * click an action-bar control until the bar is re-revealed. Nudging the mouse
 * over the viewport re-triggers the visibility timer and brings the bar back.
 *
 * `(400, 400)` is an arbitrary point comfortably inside the default Playwright
 * viewport; the exact coordinate is immaterial, only that a `mousemove` fires.
 *
 * This helper performs ONLY the wake gesture. Callers that need the bar to have
 * finished its reveal transition before interacting keep their own settle wait
 * (`waitForTimeout(...)`) inline, since the appropriate settle time varies by
 * call site.
 */
export async function wakeControls(page: Page): Promise<void> {
  await page.mouse.move(400, 400);
}

// Hover, then park the pointer at the viewport centre (a fixed point can fall
// outside a narrow viewport), so a following `isVisible()` probe does not read
// a bar that is mid-hide.
export async function wakeActionBar(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover({ timeout: 10_000 });
  const vp = page.viewportSize() ?? { width: 800, height: 600 };
  await page.mouse.move(Math.floor(vp.width / 2), Math.floor(vp.height / 2));
  await page.waitForTimeout(300);
}

// Asserting the starting `data-raised` too makes a click that went nowhere fail
// here. The slot is not sacred, so a narrow bar moves it into "More actions".
export async function setHandRaised(page: Page, want: boolean): Promise<void> {
  const trigger = page.locator('[data-testid="raise-hand-button"]');
  await expect(trigger).toHaveAttribute("data-raised", want ? "false" : "true");

  await wakeActionBar(page);
  if (await trigger.isVisible().catch(() => false)) {
    await trigger.click({ timeout: 10_000 });
  } else {
    await page.locator("#overflow-menu-trigger").click({ timeout: 10_000 });
    await page.locator(".overflow-item", { hasText: "Raise hand" }).click({ timeout: 10_000 });
  }

  await expect(trigger).toHaveAttribute("data-raised", want ? "true" : "false");
}

// False when the control is absent (MOCK_PEERS_ENABLED off). Call it before a
// screen share: only in grid mode does the dismiss click land on empty grid.
export async function setMockPeers(page: Page, count: number): Promise<boolean> {
  await page.locator(".video-controls-container").hover({ timeout: 10_000 });
  const mockBtn = page
    .locator(".video-controls-container button")
    .filter({ has: page.locator('.tooltip:has-text("Mock Peers")') });

  if ((await mockBtn.count()) === 0) {
    return false;
  }

  await mockBtn.first().click({ timeout: 10_000 });
  await expect(page.locator(".mock-peers-popover")).toBeVisible({ timeout: 5_000 });
  const input = page.locator("#mock-count-input");
  await input.fill(String(count));
  await input.dispatchEvent("input");
  await page.waitForTimeout(300);

  await page.locator("#grid-container").click({ position: { x: 10, y: 10 }, timeout: 10_000 });
  await expect(page.locator(".mock-peers-popover")).not.toBeVisible({ timeout: 3_000 });
  return true;
}

// The dock's `[data-testid="peer-list-button"]` moves into the overflow
// popover at narrow widths, hence the fallback.
export async function openPeerList(page: Page): Promise<void> {
  await wakeControls(page);
  await page.waitForTimeout(300);
  const direct = page.locator('[data-testid="peer-list-button"]').first();
  if ((await direct.count()) > 0 && (await direct.isVisible())) {
    await direct.click({ timeout: 10_000 });
  } else {
    const trigger = page.locator("#overflow-menu-trigger");
    await expect(trigger).toBeVisible({ timeout: 10_000 });
    await trigger.click({ timeout: 10_000 });
    const item = page.locator(".action-bar-overflow-popover button.overflow-item", {
      has: page.locator('span:text-is("Participants")'),
    });
    await expect(item).toBeVisible({ timeout: 10_000 });
    await item.click({ timeout: 10_000 });
  }
  await expect(page.locator("#peer-list-container")).toHaveClass(/\bvisible\b/, {
    timeout: 10_000,
  });
}

// The camera is OFF on join; the tooltip flips to "Stop Video" once capture is
// live, so this locator only matches while it is still off.
export async function enableCamera(page: Page): Promise<void> {
  await wakeControls(page);
  await page.waitForTimeout(300);
  const startCamBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Start Video" }),
  });
  await expect(startCamBtn).toBeVisible({ timeout: 10_000 });
  await startCamBtn.click();
  await page.waitForTimeout(500);
}

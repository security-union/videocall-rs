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

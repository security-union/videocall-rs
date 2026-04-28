import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Connection Quality Indicator E2E tests.
 *
 * The ConnectionQualityIndicator component renders a signal-bars badge on
 * the self-view tile when the RTT exceeds warning thresholds:
 *   - 300ms+ -> "Slow connection" (amber, 2 bars)
 *   - 500ms+ -> "Poor connection" (red, 1 bar)
 *
 * Hysteresis prevents strobe: 3 consecutive bad samples to enter,
 * 5 consecutive good samples to exit, with a 500ms fade-out animation.
 *
 * The component renders nothing (rsx! {}) when the quality level is Good,
 * so the indicator element does not exist in the DOM at all during normal
 * conditions. When active, the indicator renders with:
 *   - class: "connection-quality-indicator visible" (or "exiting")
 *   - role: "status"
 *   - aria-live: "polite"
 *   - aria-label describing the quality level and RTT
 *
 * Since the indicator depends on real RTT diagnostics from the Rust-side
 * connection manager (which requires a live server round-trip), we cannot
 * easily inject high-RTT conditions in E2E. Tests focus on:
 *   1. Verifying the indicator is absent under normal (low-RTT) conditions
 *   2. Verifying the CSS rules exist in the loaded stylesheet
 *   3. Verifying no false positives on good localhost connections
 */

test.describe("Connection quality indicator", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("indicator is not visible when connection quality is good", async ({ page }) => {
    const meetingId = `e2e_cqi_good_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("CQITestUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    // Enter the meeting
    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // Wait for a few seconds to allow diagnostics events to flow.
    // On a localhost connection, RTT should be well below 300ms, so the
    // indicator should never appear.
    await page.waitForTimeout(5000);

    // The component renders nothing when quality is Good, so the element
    // should not exist in the DOM at all.
    await expect(page.locator(".connection-quality-indicator.visible")).toHaveCount(0);
    await expect(page.locator(".connection-quality-indicator.exiting")).toHaveCount(0);
  });

  test("no status role element from connection quality indicator when quality is good", async ({
    page,
  }) => {
    const meetingId = `e2e_cqi_aria_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("CQIAriaUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
    await page.waitForTimeout(5000);

    // When the component renders rsx! {} (Good quality), no status element
    // with connection quality aria-label should exist in the DOM.
    const qualityStatus = page.locator(
      '[role="status"][aria-live="polite"][aria-label*="Connection quality"]',
    );
    await expect(qualityStatus).toHaveCount(0);
  });

  test("connection quality CSS rules are present in the stylesheet", async ({ page }) => {
    const meetingId = `e2e_cqi_css_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("CQICssUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // Verify the CSS rules for the connection quality indicator are loaded.
    // This confirms the stylesheet includes the indicator styles even when
    // the component is not currently rendering.
    const hasBaseRule = await page.evaluate(() => {
      for (const sheet of document.styleSheets) {
        try {
          for (const rule of sheet.cssRules) {
            if (rule instanceof CSSStyleRule) {
              if (rule.selectorText === ".connection-quality-indicator") {
                return true;
              }
            }
          }
        } catch {
          // Cross-origin stylesheets throw SecurityError — skip them.
        }
      }
      return false;
    });

    const hasVisibleRule = await page.evaluate(() => {
      for (const sheet of document.styleSheets) {
        try {
          for (const rule of sheet.cssRules) {
            if (rule instanceof CSSStyleRule) {
              if (rule.selectorText === ".connection-quality-indicator.visible") {
                return true;
              }
            }
          }
        } catch {
          // Cross-origin stylesheets throw SecurityError — skip them.
        }
      }
      return false;
    });

    const hasExitingRule = await page.evaluate(() => {
      for (const sheet of document.styleSheets) {
        try {
          for (const rule of sheet.cssRules) {
            if (rule instanceof CSSStyleRule) {
              if (rule.selectorText === ".connection-quality-indicator.exiting") {
                return true;
              }
            }
          }
        } catch {
          // Cross-origin stylesheets throw SecurityError — skip them.
        }
      }
      return false;
    });

    const hasLabelRule = await page.evaluate(() => {
      for (const sheet of document.styleSheets) {
        try {
          for (const rule of sheet.cssRules) {
            if (rule instanceof CSSStyleRule) {
              if (rule.selectorText === ".connection-quality-label") {
                return true;
              }
            }
          }
        } catch {
          // Cross-origin stylesheets throw SecurityError — skip them.
        }
      }
      return false;
    });

    expect(hasBaseRule).toBe(true);
    expect(hasVisibleRule).toBe(true);
    expect(hasExitingRule).toBe(true);
    expect(hasLabelRule).toBe(true);
  });

  test("indicator remains absent after extended wait on localhost", async ({ page }) => {
    // Regression guard: verify that the indicator does not spuriously
    // appear on a low-latency localhost connection even after an extended
    // observation period. This catches bugs where the hysteresis state
    // might drift or where stale diagnostics data triggers a false warning.
    const meetingId = `e2e_cqi_extended_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("CQIExtUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // Wait long enough for multiple diagnostic cycles (diagnostics fire at
    // ~1 Hz, hysteresis requires 3 consecutive bad samples to trigger).
    // 10 seconds covers well over 3 cycles.
    await page.waitForTimeout(10_000);

    // Snapshot check: no indicator element of any state should exist.
    await expect(page.locator(".connection-quality-indicator")).toHaveCount(0);
  });
});

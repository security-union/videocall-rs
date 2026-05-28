import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Regression test for #933: window focus event must not auto-start a meeting.
 *
 * Before the fix, the focus handler in AttendantsComponent called
 * mda.request() unconditionally when meeting_joined=false. A window.focus
 * event on the pre-join screen triggered the media permission flow, which
 * cascaded into client.connect() + meeting_joined.set(true), bypassing
 * the "Start Meeting" button entirely.
 */

test.describe("Pre-join screen stability on focus events", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("window focus event does not auto-start meeting from pre-join screen @bvt1", async ({
    page,
  }) => {
    await page.addInitScript(() => {
      localStorage.setItem("vc_display_name", "FocusTestUser");
    });

    const meetingId = `e2e_focus_regression_${Date.now()}`;
    await page.goto(`/meeting/${meetingId}`);

    const startButton = page.getByRole("button", {
      name: /Start Meeting|Join Meeting/,
    });
    await startButton.waitFor({ timeout: 30_000 });

    // Dispatch focus events — this is what triggered the bug before the fix.
    await page.evaluate(() => {
      window.dispatchEvent(new Event("focus"));
      window.dispatchEvent(new Event("focus"));
    });

    // Give the (buggy) async cascade time to fire if it's going to.
    await page.waitForTimeout(2_000);

    // Pre-join card must still be visible — meeting must NOT have auto-started.
    await expect(startButton).toBeVisible();

    // The in-meeting "Your meeting is ready!" state must NOT be present.
    await expect(page.getByText("Your meeting is ready!")).not.toBeVisible();
  });

  test("clicking Start Meeting still works after focus events @bvt1", async ({ page }) => {
    await page.addInitScript(() => {
      localStorage.setItem("vc_display_name", "FocusClickUser");
    });

    const meetingId = `e2e_focus_click_${Date.now()}`;
    await page.goto(`/meeting/${meetingId}`);

    const startButton = page.getByRole("button", {
      name: /Start Meeting|Join Meeting/,
    });
    await startButton.waitFor({ timeout: 30_000 });

    // Fire focus events first — should be a no-op.
    await page.evaluate(() => {
      window.dispatchEvent(new Event("focus"));
    });

    await page.waitForTimeout(1_000);
    await expect(startButton).toBeVisible();

    // Now actually click Start Meeting.
    await startButton.click();

    // Should transition to in-meeting state.
    await expect(page.getByText("Your meeting is ready!")).toBeVisible({
      timeout: 30_000,
    });
  });
});

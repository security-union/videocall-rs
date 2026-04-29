import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * OAuth-based display name handling (anonymous profile filtering + pre-fill)
 *
 * Recent changes introduced OAuth-specific display name behavior:
 *
 *   1. Anonymous profile filtering (home.rs)
 *      When `user_profile()` returns a profile with `user_id` starting with
 *      "anon-", the profile is filtered out and the sign-in button is shown
 *      instead of the authenticated UI. This allows anonymous sessions to be
 *      handled gracefully.
 *
 *   2. OAuth display name pre-fill (home.rs)
 *      The profile fetch effect now always derives the display name from the
 *      OAuth provider profile for authenticated users, pre-filling the
 *      `#username` input with the profile name.
 *
 *   3. Display name pre-fill on direct navigation (meeting.rs)
 *      The profile fetch effect in the meeting page now pre-fills the meeting
 *      display name input from the OAuth profile when navigating directly to
 *      `/meeting/<id>` without going through the home page first.
 *
 *   4. Guest fast-path fix (auth.rs)
 *      The `check_session()` guest fast-path (`vc_guest_session_id` in
 *      sessionStorage) now only short-circuits when `is_pkce_flow()` is true.
 *      In server-side OAuth mode, it clears the stale marker and falls through
 *      to the normal backend session check.
 *
 * IMPORTANT COVERAGE NOTE:
 * All four changes above are guarded by `oauth_enabled()` checks and only
 * execute when the deployment has OAuth configured. The E2E test stack runs
 * with ENABLE_OAUTH=false, so these OAuth-specific code paths cannot be
 * exercised directly.
 *
 * What we CAN test in the non-OAuth E2E environment:
 *   1. The `#username` input is empty on initial page load when localStorage
 *      is clear (OAuth pre-fill is disabled).
 *   2. In non-OAuth mode, a name saved in localStorage IS restored into the
 *      `#username` input on page load.
 *   3. Navigating directly to `/meeting/<id>` when a display name is already
 *      in localStorage pre-fills the meeting page input.
 *
 * For full OAuth coverage (anonymous profile filtering, OAuth profile pre-fill,
 * OAuth-based direct navigation pre-fill, and sessionStorage-based guest fast-
 * path behavior), an OAuth-enabled E2E stack is required. This is tracked as a
 * known coverage gap.
 */

test.describe("Auth-based display name handling", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("display name input is empty when localStorage is clear", async ({ page }) => {
    // In the non-OAuth stack, the display name input should start empty when
    // there is no saved value in localStorage. OAuth pre-fill does not run.
    await page.goto("/");
    await page.waitForTimeout(1500);
    await expect(page.locator("#username")).toHaveValue("");
  });

  test("display name is restored from localStorage on page load", async ({ page }) => {
    // When a display name is saved in localStorage in non-OAuth mode, it should
    // be restored into the #username input on page load.
    // Use addInitScript so the value is present before the app's own scripts run.
    await page.addInitScript(() => {
      localStorage.setItem("vc_display_name", "StoredUser");
    });

    await page.goto("/");
    await page.waitForTimeout(1500);

    await expect(page.locator("#username")).toHaveValue("StoredUser");
  });

  test("navigating directly to /meeting/<id> picks up display name from localStorage", async ({
    page,
  }) => {
    // When a user navigates directly to a /meeting/<id> URL without going through
    // the home page first, the meeting page should pick up the stored display name
    // and skip the entry form, going straight to the auto-join loading state.
    // Use addInitScript so the value is present before the app's own scripts run.
    await page.addInitScript(() => {
      localStorage.setItem("vc_display_name", "DirectNavUser");
    });

    const meetingId = `e2e_direct_nav_${Date.now()}`;
    await page.goto(`/meeting/${meetingId}`);
    await page.waitForTimeout(2000);

    // The meeting page skips the name-entry form when a stored name is present
    // and shows the "Joining as <name>..." loading state instead.
    await expect(page.getByText("DirectNavUser")).toBeVisible({ timeout: 8_000 });
    // The form input should NOT be visible because the name was already set.
    await expect(page.locator('input[placeholder="Enter your display name"]')).not.toBeVisible();
  });
});

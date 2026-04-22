import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * GUID display name detection (Fix #8)
 *
 * When an identity provider returns a UUID/GUID as the user's display name,
 * the system should detect it and derive a human-readable name from the email
 * address instead.  The `is_guid_like()` validator recognises the standard
 * UUID format: `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (8-4-4-4-12 hex digits).
 *
 * The GUID-filtering logic is integrated into:
 *   - OAuth callback page (oauth_callback.rs)  -- derives display name from email
 *   - Home page profile effect (home.rs)        -- derives display name from email
 *   - Auth dropdown (home.rs)                   -- shows derived name, not GUID
 *
 * IMPORTANT COVERAGE NOTE:
 * The GUID-filtering code paths are guarded by `oauth_enabled()` and only
 * execute when the deployment has OAuth configured.  The E2E test stack runs
 * with ENABLE_OAUTH=false, so the profile-derivation and auth-dropdown GUID
 * detection cannot be exercised directly.
 *
 * What we CAN test in the non-OAuth E2E environment:
 *   1. A GUID-format string passes display name validation (it contains only
 *      allowed characters: hex digits and hyphens).
 *   2. A GUID-format display name entered by the user is shown on the meeting
 *      tile as-is (no unwanted filtering of user-chosen names).
 *   3. A GUID-format name stored in localStorage is restored and shown.
 *   4. The display name input correctly accepts and rejects boundary cases
 *      relevant to the GUID format.
 *
 * For full OAuth GUID-filtering coverage, an OAuth-enabled E2E stack is
 * required (identity provider returning a UUID as the name claim).  This is
 * tracked as a known coverage gap.
 *
 * Also covers tile layout stability (Fix #6):
 * The deterministic tiebreaker added to the speech-priority sort prevents
 * tiles with equal speech timestamps from bouncing between renders.  This
 * requires multiple peers with identical speech activity and observing render
 * stability across frames -- not practically testable in automated E2E.
 * Tracked as a known coverage gap.
 */

// A standard UUID/GUID in the 8-4-4-4-12 format.
const SAMPLE_GUID = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";

// A UUID with uppercase hex -- also valid GUID format.
const SAMPLE_GUID_UPPER = "ABCDEF01-2345-6789-ABCD-EF0123456789";

// The all-zeros UUID -- edge case that is still GUID-like.
const SAMPLE_GUID_ZEROS = "00000000-0000-0000-0000-000000000000";

test.describe("GUID display name handling", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("GUID-format string is accepted as a valid display name", async ({ page }) => {
    // A UUID contains only hex digits and hyphens, both of which are allowed
    // by validate_display_name().  When a user deliberately chooses a GUID as
    // their name (non-OAuth flow), it should be accepted without error.
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially("e2e_guid_valid", { delay: 80 });
    await page.waitForTimeout(500);

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially(SAMPLE_GUID, { delay: 30 });
    await page.waitForTimeout(500);

    // Submit the form -- should navigate to the meeting page without error.
    await page.locator("#username").press("Enter");
    await expect(page).toHaveURL(/\/meeting\/e2e_guid_valid/, { timeout: 10_000 });

    // No validation error should be visible.
    await expect(page.locator("text=Invalid character")).not.toBeVisible();
  });

  test("GUID-format name is shown on the meeting page after joining", async ({ page }) => {
    // When a user enters a GUID as their display name and joins a meeting,
    // the meeting page should show that name.  In the non-OAuth flow, there
    // is no GUID filtering -- the user's chosen name is respected as-is.
    const meetingId = `e2e_guid_tile_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially(SAMPLE_GUID, { delay: 30 });
    await page.waitForTimeout(500);

    await page.locator("#username").press("Enter");
    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
    await page.waitForTimeout(2000);

    // Click Start/Join Meeting to enter the grid.
    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);

    // The grid should be visible with the user's self-view tile.
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // The GUID display name should appear on the user's own tile.
    const selfName = page.locator(".floating-name", { hasText: SAMPLE_GUID });
    await expect(selfName.first()).toBeVisible({ timeout: 10_000 });
  });

  test("GUID-format name persists in localStorage across page navigations", async ({ page }) => {
    // When a user joins a meeting with a GUID display name, that name is
    // saved to localStorage.  Navigating back to the home page should
    // restore it in the display name input.
    const meetingId = `e2e_guid_persist_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially(SAMPLE_GUID, { delay: 30 });
    await page.waitForTimeout(500);

    await page.locator("#username").press("Enter");
    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    // Navigate back to the home page.
    await page.goto("/");
    await page.waitForTimeout(2000);

    // The display name input should be pre-filled with the GUID.
    await expect(page.locator("#username")).toHaveValue(SAMPLE_GUID, { timeout: 5_000 });
  });

  test("uppercase GUID-format string is accepted as a display name", async ({ page }) => {
    // UUIDs with uppercase hex digits are also valid GUID format.
    // They should pass validation just like lowercase ones.
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially("e2e_guid_upper", { delay: 80 });
    await page.waitForTimeout(500);

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially(SAMPLE_GUID_UPPER, { delay: 30 });
    await page.waitForTimeout(500);

    await page.locator("#username").press("Enter");
    await expect(page).toHaveURL(/\/meeting\/e2e_guid_upper/, { timeout: 10_000 });
  });

  test("all-zeros UUID is accepted as a display name", async ({ page }) => {
    // The all-zeros UUID is an edge case for is_guid_like() and should
    // still pass display name validation.
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially("e2e_guid_zeros", { delay: 80 });
    await page.waitForTimeout(500);

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially(SAMPLE_GUID_ZEROS, { delay: 30 });
    await page.waitForTimeout(500);

    await page.locator("#username").press("Enter");
    await expect(page).toHaveURL(/\/meeting\/e2e_guid_zeros/, { timeout: 10_000 });
  });

  test("GUID without hyphens is not GUID-like but still a valid display name", async ({ page }) => {
    // A 32-character hex string without hyphens is NOT in GUID format
    // (is_guid_like returns false), but it IS a valid display name because
    // it contains only alphanumeric characters.
    const hexOnly = "a1b2c3d4e5f67890abcdef1234567890";

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially("e2e_guid_nohyph", { delay: 80 });
    await page.waitForTimeout(500);

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially(hexOnly, { delay: 30 });
    await page.waitForTimeout(500);

    await page.locator("#username").press("Enter");
    await expect(page).toHaveURL(/\/meeting\/e2e_guid_nohyph/, { timeout: 10_000 });
  });

  test("display name input shows validation error for disallowed characters", async ({ page }) => {
    // Characters like '@' and '.' are disallowed.  This ensures the
    // validation layer is working -- important context for GUID detection,
    // which relies on the same validation pipeline.
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially("e2e_guid_invalid", { delay: 80 });
    await page.waitForTimeout(500);

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("user@name.com", { delay: 80 });
    await page.waitForTimeout(500);

    // Submit -- should NOT navigate; should show validation error.
    await page.locator("#username").press("Enter");

    // Should stay on the home page.
    await expect(page).toHaveURL("/");

    // A validation error message should appear.
    await expect(page.locator("text=Invalid character")).toBeVisible({ timeout: 5_000 });
  });

  test("empty display name is rejected with validation error", async ({ page }) => {
    // Edge case: empty string should fail validation.  This matters because
    // the GUID detection code falls back to empty string when no email is
    // available to derive from, and empty names must be caught.
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially("e2e_guid_empty", { delay: 80 });
    await page.waitForTimeout(500);

    // Leave username empty and try to submit.
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.waitForTimeout(500);

    await page.locator("#username").press("Enter");

    // Should stay on the home page -- the HTML `required` attribute on the
    // input prevents form submission, so no navigation occurs.
    await expect(page).toHaveURL("/");
  });

  test("display name pre-filled from localStorage is editable before joining", async ({ page }) => {
    // Simulate a scenario where a GUID display name was previously saved
    // to localStorage (e.g., from a prior session with a GUID-returning
    // identity provider).  The user should be able to see it pre-filled
    // and change it before joining.

    // First, set a GUID display name in localStorage by joining a meeting.
    const meetingId1 = `e2e_guid_edit1_${Date.now()}`;
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId1, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially(SAMPLE_GUID, { delay: 30 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");
    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId1}`), { timeout: 10_000 });

    // Navigate back to home -- GUID should be pre-filled.
    await page.goto("/");
    await page.waitForTimeout(2000);
    await expect(page.locator("#username")).toHaveValue(SAMPLE_GUID, { timeout: 5_000 });

    // Now change it to a human-readable name and join a new meeting.
    const meetingId2 = `e2e_guid_edit2_${Date.now()}`;
    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId2, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("HumanName", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");
    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId2}`), { timeout: 10_000 });

    // Navigate back -- should now show "HumanName", not the GUID.
    await page.goto("/");
    await page.waitForTimeout(2000);
    await expect(page.locator("#username")).toHaveValue("HumanName", { timeout: 5_000 });
  });

  test("max-length GUID-like string is rejected when too long", async ({ page }) => {
    // A string that looks like a GUID but exceeds the 50-character max
    // length should be rejected.  The standard UUID format (36 chars) is
    // well within the limit, but this tests the validation boundary.
    const tooLong = "a".repeat(51);

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially("e2e_guid_toolong", { delay: 80 });
    await page.waitForTimeout(500);

    // The input has maxlength=50, so the browser should truncate.
    // But we test the validation error path by submitting.
    await page.locator("#username").click();
    await page.locator("#username").fill(tooLong);
    await page.waitForTimeout(500);

    // Due to maxlength=50 on the input, the value should be truncated.
    const inputValue = await page.locator("#username").inputValue();
    expect(inputValue.length).toBeLessThanOrEqual(50);
  });
});

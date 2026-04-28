import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

test.describe("Meetings", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("home page loads with meeting form", async ({ page }) => {
    await page.goto("/");
    await page.waitForTimeout(1500);
    await expect(page.locator("h1")).toContainText("videocall.rs");
    await expect(page.locator("#username")).toBeVisible();
    await expect(page.locator("#meeting-id")).toBeVisible();
    // With an empty meeting-id field, only the Generate button is rendered.
    await expect(page.getByText("Generate a New Meeting ID")).toBeVisible();
    // The Start/Join button is NOT in the DOM until the user types into #meeting-id.
    await expect(page.getByText("Start or Join Meeting")).toHaveCount(0);
    await page.waitForTimeout(1500);
  });

  test("display name input starts empty in a fresh session", async ({ page }) => {
    // In a fresh browser context (no localStorage), the controlled display
    // name input should start with an empty value.
    await page.goto("/");
    await page.waitForTimeout(1500);
    await expect(page.locator("#username")).toHaveValue("");
  });

  test("can join a meeting by filling the form", async ({ page }) => {
    await page.goto("/");
    await page.waitForTimeout(1500);
    // Fill meeting-id first (has oninput handler that triggers re-render),
    // then username last so re-render doesn't clobber it.
    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially("e2e_test_room", { delay: 80 });
    await page.waitForTimeout(1000);
    // Once the meeting-id field has content, the button-swap kicks in:
    // Start/Join is rendered, Generate is removed from the DOM.
    await expect(page.getByText("Start or Join Meeting")).toBeVisible();
    await expect(page.getByText("Generate a New Meeting ID")).toHaveCount(0);
    // The display name is a controlled input (value bound to signal).
    // Clear it first in case any pre-fill occurred, then type our value.
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("e2euser", { delay: 80 });
    await page.waitForTimeout(1000);
    // Submit via Enter on the form to avoid re-render race
    await page.locator("#username").press("Enter");
    await expect(page).toHaveURL(/\/meeting\/e2e_test_room/, { timeout: 10_000 });
    await page.waitForTimeout(2000);
  });

  test("clicking Generate populates the meeting-id field; user clicks Start/Join to enter the meeting", async ({
    page,
  }) => {
    // The Generate button no longer navigates straight into a meeting.
    // It now mints a server-side meeting ID, writes it into the #meeting-id
    // input, and the button-swap exposes the Start/Join button. Navigation
    // happens only when the user submits the form.
    await page.goto("/");
    await page.waitForTimeout(1500);

    // Display name is required because the server records the creator.
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("e2euser", { delay: 80 });
    await page.waitForTimeout(500);

    // Click Generate. Stay on home page, wait for the input to populate.
    await page.getByText("Generate a New Meeting ID").click();
    await expect(page.locator("#meeting-id")).not.toHaveValue("", { timeout: 10_000 });

    // Sanity-check the URL did NOT change to /meeting/<id>.
    await expect(page).toHaveURL(/\/$/);

    // Button-swap should have happened: Generate gone, Start/Join visible.
    await expect(page.getByText("Generate a New Meeting ID")).toHaveCount(0);
    await expect(page.getByText("Start or Join Meeting")).toBeVisible();

    // Click Start/Join to actually enter the meeting.
    await page.getByText("Start or Join Meeting").click();
    await expect(page).toHaveURL(/\/meeting\/[a-f0-9]+/, { timeout: 10_000 });
    await page.waitForTimeout(2000);
  });

  test("display name is saved to localStorage on form submit", async ({ page }) => {
    // The display name should be saved to localStorage when "Start or Join
    // Meeting" is clicked (not on keystroke). Verify by joining, then
    // navigating back to the home page and checking the input is pre-filled.
    const meetingId = `e2e_persist_${Date.now()}`;
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("PersistUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");
    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    // Navigate back to home page
    await page.goto("/");
    await page.waitForTimeout(2000);

    // The display name input should be pre-filled from localStorage
    await expect(page.locator("#username")).toHaveValue("PersistUser", { timeout: 5_000 });
  });

  test("navigating directly to a meeting URL without a display name shows inline prompt", async ({
    page,
  }) => {
    // When no display name is stored, the meeting page shows an inline
    // display name prompt instead of redirecting to the home page.
    await page.goto("/meeting/no_username_test");
    await page.waitForTimeout(2000);
    // Should stay on the meeting page, NOT redirect to "/"
    await expect(page).toHaveURL(/\/meeting\/no_username_test/);
    // The inline prompt should be visible with input and join button
    await expect(page.getByText("Enter your display name")).toBeVisible({ timeout: 5_000 });
    await expect(page.locator("input.input-apple")).toBeVisible({ timeout: 5_000 });
    await expect(page.getByText("Join Meeting")).toBeVisible({ timeout: 5_000 });
  });

  test("display name is saved to localStorage on Generate, then on Start/Join navigation", async ({
    page,
  }) => {
    // Same persistence check, but using the new two-step Generate -> Start/Join
    // flow. The display name is saved to localStorage on Generate click (in
    // the validation success path before the async create_meeting runs); the
    // URL change only happens once the user clicks Start/Join.
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("CreateUser", { delay: 80 });
    await page.waitForTimeout(500);

    // Step 1: Generate populates the field but does not navigate.
    await page.getByText("Generate a New Meeting ID").click();
    await expect(page.locator("#meeting-id")).not.toHaveValue("", { timeout: 10_000 });
    await expect(page).toHaveURL(/\/$/);

    // Step 2: Click Start/Join to enter the meeting.
    await page.getByText("Start or Join Meeting").click();
    await expect(page).toHaveURL(/\/meeting\/[a-f0-9]+/, { timeout: 10_000 });

    // Navigate back to home page and confirm display name was persisted.
    await page.goto("/");
    await page.waitForTimeout(2000);
    await expect(page.locator("#username")).toHaveValue("CreateUser", { timeout: 5_000 });
  });

  test("display name field shows inline validation error when invalid char is typed", async ({
    page,
  }) => {
    // The static hint under #username is gone; an inline error <p> appears
    // only when the user types a disallowed character, and clears as soon as
    // the field becomes valid again.
    await page.goto("/");
    await page.waitForTimeout(1500);

    // Empty state shows no error.
    await expect(page.locator("#username + p")).toHaveCount(0);

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("alice@", { delay: 80 });
    await page.waitForTimeout(500);

    // The error message should mention the offending character and "not allowed".
    const errorLocator = page.locator("#username + p");
    await expect(errorLocator).toBeVisible({ timeout: 5_000 });
    await expect(errorLocator).toContainText("'@'");
    await expect(errorLocator).toContainText("not allowed");

    // Remove the invalid char — error should clear.
    await page.locator("#username").fill("alice");
    await page.waitForTimeout(500);
    await expect(page.locator("#username + p")).toHaveCount(0);
  });

  test("meeting-id field shows inline validation error when invalid char is typed", async ({
    page,
  }) => {
    // Hyphens are NOT allowed in meeting IDs (is_valid_meeting_id only permits
    // alphanumerics + underscore). The inline error appears on invalid keystrokes
    // and clears once the field is valid.
    await page.goto("/");
    await page.waitForTimeout(1500);

    // Empty state: no error, no static hint.
    await expect(page.locator("#meeting-id + p")).toHaveCount(0);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially("my-room", { delay: 80 });
    await page.waitForTimeout(500);

    const errorLocator = page.locator("#meeting-id + p");
    await expect(errorLocator).toBeVisible({ timeout: 5_000 });
    await expect(errorLocator).toContainText("'-'");
    await expect(errorLocator).toContainText("not allowed");

    // Fix the field — error should clear.
    await page.locator("#meeting-id").fill("myroom");
    await page.waitForTimeout(500);
    await expect(page.locator("#meeting-id + p")).toHaveCount(0);
  });

  test("home page does NOT show static validation hints in the empty state", async ({ page }) => {
    // The previous static hints under both inputs were removed; only inline
    // errors should ever appear, and only when the user types an invalid char.
    await page.goto("/");
    await page.waitForTimeout(1500);

    await expect(
      page.getByText("Allowed: letters, numbers, spaces, hyphens, underscores, apostrophes"),
    ).toHaveCount(0);
    await expect(page.getByText("Characters allowed: a-z, A-Z, 0-9, and _")).toHaveCount(0);
  });
});

import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

test.describe("Meeting settings – Options toggles", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    // Meeting settings page only exists in Dioxus UI (port 3001)
    test.skip(!baseURL?.includes("3001"), "Meeting settings page is Dioxus-only");
    await injectSessionCookie(context, { baseURL });
  });

  /**
   * Create a meeting by joining from the home page, then navigate to the
   * settings page for that meeting. Returns once the Options card is visible.
   */
  async function createMeetingAndOpenSettings(
    page: Page,
    meetingId: string,
    username: string,
  ): Promise<void> {
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially(username, { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    // Wait for meeting page to load and auto-join API (creates the meeting)
    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
      timeout: 10_000,
    });
    await expect(page.getByText(/Start Meeting|Join Meeting/)).toBeVisible({
      timeout: 20_000,
    });

    // Navigate to the settings page
    await page.goto(`/meeting/${meetingId}/settings`);
    await page.waitForTimeout(1500);

    await expect(page.getByText("Meeting Settings")).toBeVisible({
      timeout: 10_000,
    });
    await expect(page.getByText("Options")).toBeVisible({ timeout: 5_000 });
  }

  /** Locate the toggle button inside a settings-option-row by its label text. */
  function optionToggle(page: Page, label: string) {
    return page
      .locator(".settings-option-row")
      .filter({ hasText: label })
      .locator('button[role="switch"]');
  }

  test("settings page displays both Waiting Room and Participants can admit others toggles", async ({
    page,
  }) => {
    const meetingId = `e2e_opt_show_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "opt-show-user");

    await expect(page.getByText("Waiting Room")).toBeVisible();
    await expect(page.getByText("Participants can admit others")).toBeVisible();

    const wrToggle = optionToggle(page, "Waiting Room");
    const acaToggle = optionToggle(page, "Participants can admit others");

    await expect(wrToggle).toBeVisible();
    await expect(acaToggle).toBeVisible();

    // Default: waiting room ON, admitted_can_admit OFF
    await expect(wrToggle).toHaveAttribute("aria-checked", "true");
    await expect(acaToggle).toHaveAttribute("aria-checked", "false");
  });

  test("Waiting Room toggle can be toggled off and on", async ({ page }) => {
    const meetingId = `e2e_opt_wr_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "wr-toggle-user");

    const wrToggle = optionToggle(page, "Waiting Room");

    // Starts ON
    await expect(wrToggle).toHaveAttribute("aria-checked", "true");

    // Toggle OFF
    await wrToggle.click();
    await expect(wrToggle).toHaveAttribute("aria-checked", "false", {
      timeout: 5_000,
    });

    // Toggle back ON
    await wrToggle.click();
    await expect(wrToggle).toHaveAttribute("aria-checked", "true", {
      timeout: 5_000,
    });
  });

  test("Participants can admit others toggle is disabled when Waiting Room is off", async ({
    page,
  }) => {
    const meetingId = `e2e_opt_aca_dis_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "aca-dis-user");

    const wrToggle = optionToggle(page, "Waiting Room");
    const acaToggle = optionToggle(page, "Participants can admit others");

    // Turn waiting room OFF first
    await wrToggle.click();
    await expect(wrToggle).toHaveAttribute("aria-checked", "false", {
      timeout: 5_000,
    });

    // Admitted-can-admit should be disabled and off
    await expect(acaToggle).toBeDisabled();
    await expect(acaToggle).toHaveAttribute("aria-checked", "false");
  });

  test("Participants can admit others toggle can be enabled when Waiting Room is on", async ({
    page,
  }) => {
    const meetingId = `e2e_opt_aca_on_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "aca-on-user");

    const wrToggle = optionToggle(page, "Waiting Room");
    const acaToggle = optionToggle(page, "Participants can admit others");

    // Waiting room should be ON by default
    await expect(wrToggle).toHaveAttribute("aria-checked", "true");

    // Admitted-can-admit should be enabled (not disabled) but OFF
    await expect(acaToggle).not.toBeDisabled();
    await expect(acaToggle).toHaveAttribute("aria-checked", "false");

    // Toggle admitted-can-admit ON
    await acaToggle.click();
    await expect(acaToggle).toHaveAttribute("aria-checked", "true", {
      timeout: 5_000,
    });

    // Toggle it back OFF
    await acaToggle.click();
    await expect(acaToggle).toHaveAttribute("aria-checked", "false", {
      timeout: 5_000,
    });
  });

  test("disabling Waiting Room also turns off Participants can admit others", async ({ page }) => {
    const meetingId = `e2e_opt_cascade_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "cascade-user");

    const wrToggle = optionToggle(page, "Waiting Room");
    const acaToggle = optionToggle(page, "Participants can admit others");

    // Enable admitted-can-admit (waiting room is already ON by default)
    await acaToggle.click();
    await expect(acaToggle).toHaveAttribute("aria-checked", "true", {
      timeout: 5_000,
    });

    // Now disable waiting room — admitted-can-admit should also turn off
    await wrToggle.click();
    await expect(wrToggle).toHaveAttribute("aria-checked", "false", {
      timeout: 5_000,
    });
    await expect(acaToggle).toHaveAttribute("aria-checked", "false", {
      timeout: 5_000,
    });
    await expect(acaToggle).toBeDisabled();
  });

  test("toggle state persists after page reload", async ({ page }) => {
    const meetingId = `e2e_opt_persist_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "persist-user");

    const acaToggle = optionToggle(page, "Participants can admit others");

    // Enable admitted-can-admit
    await acaToggle.click();
    await expect(acaToggle).toHaveAttribute("aria-checked", "true", {
      timeout: 5_000,
    });

    // Reload the page
    await page.reload();
    await page.waitForTimeout(2000);
    await expect(page.getByText("Options")).toBeVisible({ timeout: 10_000 });

    // Both toggles should reflect the persisted state
    await expect(optionToggle(page, "Waiting Room")).toHaveAttribute("aria-checked", "true");
    await expect(optionToggle(page, "Participants can admit others")).toHaveAttribute(
      "aria-checked",
      "true",
    );
  });
});

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
    await expect(page.getByText("Create a New Meeting ID")).toBeVisible();
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
    await expect(page.getByText("Start or Join Meeting")).toBeVisible();
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

  test("create new meeting generates ID and navigates", async ({ page }) => {
    await page.goto("/");
    await page.waitForTimeout(1500);
    // Clear the controlled input before typing to handle any pre-fill
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("e2euser", { delay: 80 });
    await page.waitForTimeout(1000);
    await page.getByText("Create a New Meeting ID").click();
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

  test("display name is saved to localStorage on create new meeting", async ({ page }) => {
    // Same persistence check, but using the "Create a New Meeting ID" button.
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("CreateUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.getByText("Create a New Meeting ID").click();
    await expect(page).toHaveURL(/\/meeting\/[a-f0-9]+/, { timeout: 10_000 });

    // Navigate back to home page
    await page.goto("/");
    await page.waitForTimeout(2000);

    // The display name input should be pre-filled from localStorage
    await expect(page.locator("#username")).toHaveValue("CreateUser", { timeout: 5_000 });
  });
});

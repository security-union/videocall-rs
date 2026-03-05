import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

test.describe("Meetings", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context }) => {
    await injectSessionCookie(context);
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

  test("can join a meeting by filling the form", async ({ page }) => {
    await page.goto("/");
    await page.waitForTimeout(1500);
    // Fill meeting-id first (has oninput handler that triggers re-render),
    // then username last so re-render doesn't clobber it.
    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially("e2e_test_room", { delay: 80 });
    await page.waitForTimeout(1000);
    await expect(page.getByText("Start or Join Meeting")).toBeVisible();
    await page.locator("#username").click();
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
    await page.locator("#username").click();
    await page.locator("#username").pressSequentially("e2euser", { delay: 80 });
    await page.waitForTimeout(1000);
    await page.getByText("Create a New Meeting ID").click();
    await expect(page).toHaveURL(/\/meeting\/[a-f0-9]+/, { timeout: 10_000 });
    await page.waitForTimeout(2000);
  });
});
